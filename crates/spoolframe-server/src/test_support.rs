//! Stand-ins the unit tests and the integration tests (`tests/it`) share:
//! a fake catalog with a fixed tree, and a stand-in ffprobe answering with
//! recorded output. Built for `cfg(test)` and for the `test-support`
//! feature, which only the crate's own dev-dependency turns on; a release
//! build never has it.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use spoolframe_catalog::{
    Catalog, CatalogError, Found, FoundGroup, Image, ItemPage, ItemWithChildren, Page, SearchQuery,
};
use spoolframe_protocol::codec::MAX_FRAME;
use spoolframe_types::{
    Album, Artist, ArtworkId, CalendarDate, CastMember, Episode, Item, Library, LibraryId,
    LibraryKind, Marker, MarkerKind, MediaId, MediaKind, Movie, Namespace, PersonId, PersonRole,
    PlaybackState, SearchKind, Season, Show, Track,
};

/// The RPC request frames read as an `RpcRequest`, by every server in the
/// process: a count only a test run alone can read as its own.
pub static RPC_REQUEST_DECODES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// The fake catalog's one image.
pub const POSTER: &str = "/library/metadata/1/thumb/1";

pub fn poster() -> ArtworkId {
    ArtworkId::new(Namespace::Plex, POSTER).unwrap()
}

/// A search for this finds a movie whose title alone is over a frame: a
/// reply that cannot be sent.
pub const A_TITLE_OVER_A_FRAME: &str = "a title over a frame";

/// What finds the show's "heist episodes", episodes 12 and 15: the
/// stand-in for "every heist episode of Brooklyn Nine-Nine" (PX-42).
pub const HEIST: &str = "heist";

/// One library holding one movie, `plex:movie:1`.
#[derive(Debug)]
pub struct FakeCatalog {
    /// The movie's file, once a test writes it.
    media: PathBuf,
    /// What the catalog answers for keys items had before it was rebuilt.
    old_keys: Mutex<HashMap<MediaId, OldKey>>,
    /// Each `item` and `external_ids` call: the method and the id.
    calls: Mutex<Vec<(&'static str, MediaId)>>,
    /// Items taken out of the library: not found any more.
    removed: Mutex<Vec<MediaId>>,
    /// Items given a new key (a rebuild): the old key, and the new one.
    rekeyed: Mutex<Vec<(MediaId, MediaId)>>,
    /// Items still in the library whose file cannot be read.
    unreadable: Mutex<Vec<MediaId>>,
    /// Whether its music is a library of its own (PX-43): the artist,
    /// albums and tracks are items either way.
    music: Mutex<bool>,
    /// Whether its films, episodes and tracks have posters (PX-53).
    posters: Mutex<bool>,
    /// Agents' ids an item has besides its `test://` one (PX-02:
    /// `imdb://tt…`).
    more_ids: Mutex<HashMap<MediaId, Vec<String>>>,
}

/// What a key holds after the catalog gave its item a new one. A key the
/// fake knows nothing of is not found.
#[derive(Debug, Clone)]
pub enum OldKey {
    /// Another item of the same kind, with these external ids.
    AnotherItem(Vec<String>),
    /// An item of another kind: `PlexCatalog` cannot read it as `item`,
    /// and has no `external_ids` for it.
    AnotherKind,
    /// The key is another catalog's.
    Foreign,
    /// The catalog cannot be asked.
    Unreachable,
}

impl FakeCatalog {
    /// The fake tree, with the movie's file at `media` (written or not)
    /// and each episode's next to it.
    pub fn new(media: PathBuf) -> Self {
        Self {
            media,
            old_keys: Mutex::default(),
            calls: Mutex::default(),
            removed: Mutex::default(),
            rekeyed: Mutex::default(),
            unreadable: Mutex::default(),
            music: Mutex::default(),
            posters: Mutex::default(),
            more_ids: Mutex::default(),
        }
    }

    /// Give item `id` these agents' ids too (`imdb://tt0078748`).
    pub fn give_ids(&self, id: &MediaId, ids: &[&str]) {
        self.more_ids
            .lock()
            .insert(id.clone(), ids.iter().map(|s| (*s).to_owned()).collect());
    }

    fn ids_of(&self, item: &Item) -> Vec<String> {
        let mut ids = external_ids(item);
        if let Some(more) = self.more_ids.lock().get(item.id()) {
            ids.extend(more.iter().cloned());
        }
        ids
    }

    /// Give each film, episode and track a poster, as a PMS does: a
    /// film's own, an episode's season's, a track's album's, each at
    /// [`poster_of`] and served as a picture of its own colour
    /// ([`poster_color`]). Without, items have none (PX-53).
    pub fn show_posters(&self) {
        *self.posters.lock() = true;
    }

    /// Serve its music as a second library, `plex:library:2`, holding
    /// [`artist`]: what makes Liked and All listed (PX-43).
    pub fn serve_music(&self) {
        *self.music.lock() = true;
    }

    /// Take `id` out of the library: it is not found from now on, under
    /// any key.
    pub fn remove(&self, id: &MediaId) {
        self.removed.lock().push(id.clone());
    }

    /// Give `id` the key `key`, as a rebuilt library does: the old key is
    /// not found, the new one holds the item, and its external ids find
    /// it there.
    pub fn rekey(&self, id: &MediaId, key: &str) -> MediaId {
        let new = MediaId::new(id.namespace(), id.kind(), key).unwrap();
        self.rekeyed.lock().push((id.clone(), new.clone()));
        new
    }

    /// Make `id`'s file unreadable: a drive offline, a file moved. The item
    /// stays in the library.
    pub fn unreadable(&self, id: &MediaId) {
        self.unreadable.lock().push(id.clone());
    }

    /// What the fake's tree has under `id` now, with its children: none
    /// for a removed item or a key given away, the item for its new key.
    fn current(&self, id: &MediaId) -> Option<(Item, Vec<Item>)> {
        if self.removed.lock().contains(id) || self.rekeyed.lock().iter().any(|(old, _)| old == id)
        {
            return None;
        }
        let old = self
            .rekeyed
            .lock()
            .iter()
            .find(|(_, new)| new == id)
            .map(|(old, _)| old.clone());
        match old {
            Some(old) => tree()
                .into_iter()
                .find(|(item, _)| *item.id() == old)
                .map(|(item, children)| (with_id(item, id), children)),
            None => tree().into_iter().find(|(item, _)| item.id() == id),
        }
    }

    pub fn set_old_key(&self, id: &MediaId, holds: OldKey) {
        self.old_keys.lock().insert(id.clone(), holds);
    }

    pub fn forget_old_key(&self, id: &MediaId) {
        self.old_keys.lock().remove(id);
    }

    /// The `item` and `external_ids` calls since the last time, by method.
    pub fn take_calls(&self, id: &MediaId) -> Vec<&'static str> {
        let mut calls = self.calls.lock();
        let about = calls.iter().filter(|(_, i)| i == id).map(|(m, _)| *m);
        let about = about.collect();
        calls.clear();
        about
    }

    fn called(&self, method: &'static str, id: &MediaId) {
        self.calls.lock().push((method, id.clone()));
    }

    fn old_key(&self, id: &MediaId) -> Option<OldKey> {
        self.old_keys.lock().get(id).cloned()
    }

    fn unreachable() -> CatalogError {
        CatalogError::Unreachable {
            url: "http://fake".into(),
            call: "GET /library/metadata".into(),
            source: "connection refused".into(),
        }
    }
}

/// The external ids of an item of the fake's tree.
fn external_ids(item: &Item) -> Vec<String> {
    let first = movie();
    let of = if item.id() == second_copy().id() {
        &first
    } else {
        item
    };
    vec![format!("test://{}", of.id().key())]
}

/// `item` under the key of `id`.
fn with_id(item: Item, id: &MediaId) -> Item {
    let id = id.clone();
    match item {
        Item::Movie(m) => Item::Movie(Movie { id, ..m }),
        Item::Show(s) => Item::Show(Show { id, ..s }),
        Item::Season(s) => Item::Season(Season { id, ..s }),
        Item::Episode(e) => Item::Episode(Episode { id, ..e }),
        Item::Artist(a) => Item::Artist(Artist { id, ..a }),
        Item::Album(a) => Item::Album(Album { id, ..a }),
        Item::Track(t) => Item::Track(Track { id, ..t }),
    }
}

pub fn movie() -> Item {
    Item::Movie(Movie {
        id: MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap(),
        title: "Alien".into(),
        year: Some(1979),
        release_date: CalendarDate::new(1979, 5, 25),
        summary: None,
        content_rating: None,
        duration: Some(Duration::from_secs(7020)),
        cast: vec![
            CastMember {
                id: PersonId::new(Namespace::Plex, "actor-13434").unwrap(),
                name: "Sigourney Weaver".into(),
                role: PersonRole::Actor,
                character: Some("Ripley".into()),
                photo: None,
            },
            CastMember {
                id: PersonId::new(Namespace::Plex, "director-20469").unwrap(),
                name: "Ridley Scott".into(),
                role: PersonRole::Director,
                character: None,
                photo: None,
            },
        ],
        artwork: Vec::new(),
        playback: PlaybackState::default(),
    })
}

/// The same film in a second library, `plex:movie:2`: its own key, and
/// the first copy's external ids.
pub fn second_copy() -> Item {
    Item::Movie(Movie {
        id: MediaId::new(Namespace::Plex, MediaKind::Movie, "2").unwrap(),
        title: "Alien (4K)".into(),
        year: Some(1979),
        release_date: None,
        summary: None,
        content_rating: None,
        duration: Some(Duration::from_secs(7020)),
        cast: Vec::new(),
        artwork: Vec::new(),
        playback: PlaybackState::default(),
    })
}

fn media(kind: MediaKind, key: &str) -> MediaId {
    MediaId::new(Namespace::Plex, kind, key).unwrap()
}

/// A show, `plex:show:10`: specials (season 0, episode 21), season 1
/// (episodes 12 and 13) and season 2 (episode 15).
pub fn show() -> Item {
    Item::Show(Show {
        id: media(MediaKind::Show, "10"),
        title: "The Expanse".into(),
        year: Some(2015),
        summary: None,
        content_rating: None,
        season_count: None,
        genres: Vec::new(),
        last_aired: CalendarDate::new(2022, 4, 8),
        cast: Vec::new(),
        artwork: Vec::new(),
    })
}

fn season(key: &str, number: u32) -> Item {
    Item::Season(Season {
        id: media(MediaKind::Season, key),
        show_id: media(MediaKind::Show, "10"),
        show_title: Some("The Expanse".into()),
        season: number,
        title: format!("Season {number}"),
        artwork: Vec::new(),
    })
}

/// Episode `key`: number `episode` of season `season` (`season_key`).
pub fn episode(key: &str, season_key: &str, season: u32, episode: u32) -> Item {
    Item::Episode(Episode {
        id: media(MediaKind::Episode, key),
        show_id: media(MediaKind::Show, "10"),
        show_title: Some("The Expanse".into()),
        season_id: media(MediaKind::Season, season_key),
        season,
        episode,
        title: format!("S{season}E{episode}"),
        summary: None,
        content_rating: None,
        year: Some(2015),
        air_date: CalendarDate::new(2015, 11, 23),
        duration: Some(Duration::from_secs(2640)),
        cast: Vec::new(),
        artwork: Vec::new(),
        playback: PlaybackState::default(),
    })
}

/// An artist, `plex:artist:30`, with two albums: `plex:album:31`
/// (2001: tracks 33 and 32, listed in that order, numbered 1 and 2) and
/// `plex:album:34` (1997: track 35).
pub fn artist() -> Item {
    Item::Artist(Artist {
        id: media(MediaKind::Artist, "30"),
        name: "Daft Punk".into(),
        summary: None,
        artwork: Vec::new(),
    })
}

pub fn album(key: &str) -> Item {
    let (title, year) = if key == "31" {
        ("Discovery", 2001)
    } else {
        ("Homework", 1997)
    };
    Item::Album(Album {
        id: media(MediaKind::Album, key),
        artist_id: media(MediaKind::Artist, "30"),
        artist_name: Some("Daft Punk".into()),
        title: title.into(),
        year: Some(year),
        track_count: None,
        artwork: Vec::new(),
    })
}

/// Track `key`, number `number` of album `album`.
pub fn track(key: &str, album: &str, number: u32) -> Item {
    Item::Track(Track {
        id: media(MediaKind::Track, key),
        album_id: media(MediaKind::Album, album),
        artist_id: media(MediaKind::Artist, "30"),
        artist_name: Some("Daft Punk".into()),
        album_title: None,
        title: format!("Track {key}"),
        track: Some(number),
        disc: Some(1),
        album_track_count: None,
        codec: None,
        bit_depth: None,
        sample_rate: None,
        duration: Some(Duration::from_secs(240)),
        artwork: Vec::new(),
        playback: PlaybackState::default(),
    })
}

/// The markers of episode 15, the only item that has any: an intro, and
/// credits that run to the end, starting after the point P1-14's rule
/// calls the episode watched at (39:00 of its 44:00).
pub fn markers() -> Vec<Marker> {
    vec![
        Marker {
            kind: MarkerKind::Intro,
            start_ms: 30_000,
            end_ms: 90_000,
            to_end: false,
        },
        Marker {
            kind: MarkerKind::Credits,
            start_ms: 2_400_000,
            end_ms: 2_640_001,
            to_end: true,
        },
    ]
}

/// Every item the fake catalog has, with its children.
fn tree() -> Vec<(Item, Vec<Item>)> {
    vec![
        (movie(), Vec::new()),
        (second_copy(), Vec::new()),
        (
            show(),
            vec![season("20", 0), season("11", 1), season("14", 2)],
        ),
        (season("20", 0), vec![episode("21", "20", 0, 1)]),
        (
            season("11", 1),
            vec![episode("12", "11", 1, 1), episode("13", "11", 1, 2)],
        ),
        (season("14", 2), vec![episode("15", "14", 2, 1)]),
        (episode("21", "20", 0, 1), Vec::new()),
        (episode("12", "11", 1, 1), Vec::new()),
        (episode("13", "11", 1, 2), Vec::new()),
        (episode("15", "14", 2, 1), Vec::new()),
        (artist(), vec![album("31"), album("34")]),
        (
            album("31"),
            vec![track("33", "31", 2), track("32", "31", 1)],
        ),
        (album("34"), vec![track("35", "34", 1)]),
        (track("32", "31", 1), Vec::new()),
        (track("33", "31", 2), Vec::new()),
        (track("35", "34", 1), Vec::new()),
    ]
}

pub fn library() -> Library {
    Library {
        id: LibraryId::new(Namespace::Plex, "1").unwrap(),
        title: "Movies".into(),
        kind: LibraryKind::Movies,
    }
}

/// The music library [`FakeCatalog::serve_music`] adds.
pub fn music_library() -> Library {
    Library {
        id: LibraryId::new(Namespace::Plex, "2").unwrap(),
        title: "Music".into(),
        kind: LibraryKind::Music,
    }
}

#[async_trait]
impl Catalog for FakeCatalog {
    async fn libraries(&self) -> Result<Vec<Library>, CatalogError> {
        if *self.music.lock() {
            return Ok(vec![library(), music_library()]);
        }
        Ok(vec![library()])
    }

    async fn items(&self, library: &LibraryId, page: Page) -> Result<ItemPage, CatalogError> {
        if library.key() == "2" && *self.music.lock() {
            let items = if page.offset == 0 && page.limit > 0 {
                vec![artist()]
            } else {
                Vec::new()
            };
            return Ok(ItemPage { items, total: 1 });
        }
        if library.key() != "1" {
            return Err(CatalogError::NotFound(library.to_string()));
        }
        let items = if page.offset == 0 && page.limit > 0 {
            vec![movie()]
        } else {
            Vec::new()
        };
        Ok(ItemPage { items, total: 1 })
    }

    async fn item(&self, id: &MediaId) -> Result<ItemWithChildren, CatalogError> {
        self.called("item", id);
        match self.old_key(id) {
            Some(OldKey::AnotherItem(external_ids)) => {
                let Item::Movie(movie) = movie() else {
                    unreachable!()
                };
                return Ok(ItemWithChildren {
                    item: Item::Movie(Movie {
                        id: id.clone(),
                        title: "Aliens".into(),
                        content_rating: None,
                        ..movie
                    }),
                    children: Vec::new(),
                    external_ids,
                    markers: Vec::new(),
                });
            }
            Some(OldKey::AnotherKind) => {
                return Err(CatalogError::Backend {
                    url: "http://fake".into(),
                    call: "GET /library/metadata".into(),
                    item: Some(id.to_string()),
                    message: format!("the PMS returned another kind, not {id}"),
                });
            }
            Some(OldKey::Foreign) => return Err(CatalogError::ForeignId(id.to_string())),
            Some(OldKey::Unreachable) => return Err(Self::unreachable()),
            None => {}
        }
        let posters = *self.posters.lock();
        self.current(id)
            .map(|(mut item, children)| {
                if posters {
                    with_poster(&mut item);
                }
                let markers = if id.key() == "15" {
                    markers()
                } else {
                    Vec::new()
                };
                ItemWithChildren {
                    external_ids: self.ids_of(&item),
                    item,
                    children,
                    markers,
                }
            })
            .ok_or_else(|| CatalogError::NotFound(id.to_string()))
    }

    async fn external_ids(&self, id: &MediaId) -> Result<Vec<String>, CatalogError> {
        self.called("external_ids", id);
        match self.old_key(id) {
            Some(OldKey::AnotherItem(external_ids)) => return Ok(external_ids),
            Some(OldKey::AnotherKind) => return Err(CatalogError::NotFound(id.to_string())),
            Some(OldKey::Foreign) => return Err(CatalogError::ForeignId(id.to_string())),
            Some(OldKey::Unreachable) => return Err(Self::unreachable()),
            None => {}
        }
        self.current(id)
            .map(|(item, _)| self.ids_of(&item))
            .ok_or_else(|| CatalogError::NotFound(id.to_string()))
    }

    /// An item of the tree with one of `ids`, under its key now.
    async fn find_external(
        &self,
        kind: MediaKind,
        ids: &[String],
    ) -> Result<Option<MediaId>, CatalogError> {
        let rekeyed = self.rekeyed.lock().clone();
        for (item, _) in tree() {
            if item.id().kind() != kind || !external_ids(&item).iter().any(|i| ids.contains(i)) {
                continue;
            }
            let now = rekeyed
                .iter()
                .find(|(old, _)| old == item.id())
                .map_or_else(|| item.id().clone(), |(_, new)| new.clone());
            if self.current(&now).is_some() {
                return Ok(Some(now));
            }
        }
        Ok(None)
    }

    /// The movie, for a part of "alien" or for [`A_TITLE_OVER_A_FRAME`];
    /// two episodes for [`HEIST`]; nothing else.
    async fn search(&self, query: &SearchQuery) -> Result<Found, CatalogError> {
        let text = query.text.to_lowercase();
        let found = if text == A_TITLE_OVER_A_FRAME {
            let Item::Movie(movie) = movie() else {
                unreachable!()
            };
            vec![Item::Movie(Movie {
                title: "a".repeat(MAX_FRAME as usize),
                content_rating: None,
                ..movie
            })]
        } else if "alien".contains(&text) && !text.is_empty() {
            vec![movie()]
        } else {
            Vec::new()
        };
        let episodes = if text == HEIST {
            vec![episode("12", "11", 1, 1), episode("15", "14", 2, 1)]
        } else {
            Vec::new()
        };
        let groups = query
            .kinds
            .iter()
            .map(|&kind| match kind {
                SearchKind::Movies => FoundGroup {
                    total: found.len() as u32,
                    items: found.clone(),
                    ..FoundGroup::empty(kind)
                },
                SearchKind::Episodes => FoundGroup {
                    total: episodes.len() as u32,
                    items: episodes.clone(),
                    ..FoundGroup::empty(kind)
                },
                _ => FoundGroup::empty(kind),
            })
            .collect();
        Ok(Found { groups })
    }

    /// The fake knows no people.
    async fn person_items(&self, id: &PersonId) -> Result<Vec<Item>, CatalogError> {
        Err(CatalogError::NotFound(id.to_string()))
    }

    async fn media_path(&self, id: &MediaId) -> Result<PathBuf, CatalogError> {
        if self.removed.lock().contains(id) || self.rekeyed.lock().iter().any(|(old, _)| old == id)
        {
            return Err(CatalogError::NotFound(id.to_string()));
        }
        if self.unreadable.lock().contains(id) {
            return Err(CatalogError::MediaFileUnavailable(Box::new(
                spoolframe_catalog::UnavailableFile {
                    id: id.clone(),
                    reported: PathBuf::from("/media/offline"),
                    mapping: None,
                    local: PathBuf::from("/media/offline"),
                    reason: "no such file".into(),
                },
            )));
        }
        let path = if id == movie().id() {
            self.media.clone()
        } else {
            episode_media(&self.media, id)
        };
        if path.exists() {
            Ok(path)
        } else {
            Err(CatalogError::NotFound(id.to_string()))
        }
    }

    async fn recently_added(&self, _limit: u32) -> Result<Vec<Item>, CatalogError> {
        Ok(vec![movie()])
    }

    /// A 400×200 PNG at the movie's poster path; nothing else.
    async fn authorize_artwork(
        &self,
        _id: &spoolframe_types::ArtworkId,
    ) -> Result<(), CatalogError> {
        Ok(())
    }

    async fn artwork(&self, id: &ArtworkId) -> Result<Image, CatalogError> {
        let color = match (*self.posters.lock(), poster_color(id)) {
            (true, Some(color)) => color,
            _ if id.key() == POSTER => [10, 120, 200],
            _ => return Err(CatalogError::NotFound(id.to_string())),
        };
        let img = image::RgbImage::from_pixel(400, 200, image::Rgb(color));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        Ok(Image {
            bytes: out.into_inner(),
            content_type: "image/png".into(),
        })
    }
}

/// The poster of the item keyed `key`, with [`FakeCatalog::show_posters`].
pub fn poster_of(key: &str) -> ArtworkId {
    ArtworkId::new(Namespace::Plex, format!("/library/metadata/{key}/thumb/9")).unwrap()
}

/// The colour of a poster [`poster_of`] names: one of its own per key.
pub fn poster_color(id: &ArtworkId) -> Option<[u8; 3]> {
    let key: u32 = id
        .key()
        .strip_prefix("/library/metadata/")?
        .strip_suffix("/thumb/9")?
        .parse()
        .ok()?;
    let k = (key % 251) as u8;
    Some([k.wrapping_mul(37), k.wrapping_mul(91), k.wrapping_mul(53)])
}

/// `item` with its poster, as [`FakeCatalog::show_posters`] gives one.
fn with_poster(item: &mut Item) {
    let (artwork, key) = match item {
        Item::Movie(m) => (&mut m.artwork, m.id.key().to_owned()),
        Item::Episode(e) => (&mut e.artwork, e.season_id.key().to_owned()),
        Item::Track(t) => (&mut t.artwork, t.album_id.key().to_owned()),
        Item::Album(a) => (&mut a.artwork, a.id.key().to_owned()),
        _ => return,
    };
    artwork.push(spoolframe_types::Artwork {
        id: poster_of(&key),
        kind: spoolframe_types::ArtworkKind::Poster,
    });
}

/// Where an episode's file goes, next to the movie's. The stand-in ffprobe
/// describes it as Brooklyn Nine-Nine S07E01 (P1-20's fixture).
pub fn episode_media(movie: &Path, id: &MediaId) -> PathBuf {
    movie.with_file_name(format!("episode-{}.b99.mkv", id.key()))
}

/// Write episode `key`'s file next to the movie at `movie`; the stand-in
/// ffprobe describes it as B99 S07E01.
pub fn write_episode(movie: &Path, key: &str) {
    let id = MediaId::new(Namespace::Plex, MediaKind::Episode, key).unwrap();
    let path = episode_media(movie, &id);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b"stand-in").unwrap();
}

/// A stand-in ffprobe in `dir/bin` answering with recorded output: the
/// SRT for `.srt` files, B99 S07E01 for an episode's file, and `fixture`
/// (a file in spoolframe-media's fixtures) for anything else.
pub fn fake_ffprobe(dir: &Path, fixture: &str) -> PathBuf {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../spoolframe-media/tests/fixtures");
    let script = dir.join("bin").join("ffprobe");
    std::fs::create_dir_all(script.parent().unwrap()).unwrap();
    write_script(
        &script,
        &format!(
            "#!/bin/sh\nfor path; do :; done\ncase \"$path\" in *.srt) cat '{}' ;; *.b99.mkv) cat '{}' ;; *) cat '{}' ;; esac\n",
            fixtures.join("external-srt.json").display(),
            fixtures.join("b99-s07e01.json").display(),
            fixtures.join(fixture).display(),
        ),
    );
    script
}

/// Writes an executable script. The text goes to a plain file first and
/// `install` makes the script from it, so this process never holds the
/// script open for writing: a child another test forks at that moment
/// would inherit the handle, and running the script would then fail with
/// "Text file busy" (ETXTBSY).
pub fn write_script(path: &Path, text: &str) {
    let source = path.with_extension("txt");
    std::fs::write(&source, text).unwrap();
    let status = std::process::Command::new("install")
        .args(["-m", "755"])
        .arg(&source)
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success(), "install {} failed", path.display());
}
