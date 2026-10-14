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
    ArtworkId, CalendarDate, CastMember, Episode, Item, Library, LibraryId, LibraryKind, Marker,
    MarkerKind, MediaId, MediaKind, Movie, Namespace, PersonId, PersonRole, PlaybackState,
    SearchKind, Season, Show,
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

/// One library holding one movie, `plex:movie:1`.
#[derive(Debug)]
pub struct FakeCatalog {
    /// The movie's file, once a test writes it.
    media: PathBuf,
    /// What the catalog answers for keys items had before it was rebuilt.
    old_keys: Mutex<HashMap<MediaId, OldKey>>,
    /// Each `item` and `external_ids` call: the method and the id.
    calls: Mutex<Vec<(&'static str, MediaId)>>,
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
    ]
}

pub fn library() -> Library {
    Library {
        id: LibraryId::new(Namespace::Plex, "1").unwrap(),
        title: "Movies".into(),
        kind: LibraryKind::Movies,
    }
}

#[async_trait]
impl Catalog for FakeCatalog {
    async fn libraries(&self) -> Result<Vec<Library>, CatalogError> {
        Ok(vec![library()])
    }

    async fn items(&self, library: &LibraryId, page: Page) -> Result<ItemPage, CatalogError> {
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
        tree()
            .into_iter()
            .find(|(item, _)| item.id() == id)
            .map(|(item, children)| {
                let markers = if id.key() == "15" {
                    markers()
                } else {
                    Vec::new()
                };
                ItemWithChildren {
                    external_ids: external_ids(&item),
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
        tree()
            .iter()
            .find(|(item, _)| item.id() == id)
            .map(|(item, _)| external_ids(item))
            .ok_or_else(|| CatalogError::NotFound(id.to_string()))
    }

    /// The movie, for a part of "alien" or for [`A_TITLE_OVER_A_FRAME`];
    /// nothing else.
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
        let groups = query
            .kinds
            .iter()
            .map(|&kind| match kind {
                SearchKind::Movies => FoundGroup {
                    total: found.len() as u32,
                    items: found.clone(),
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
        if id.key() != POSTER {
            return Err(CatalogError::NotFound(id.to_string()));
        }
        let img = image::RgbImage::from_pixel(400, 200, image::Rgb([10, 120, 200]));
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
