//! PMS metadata → Spoolframe's media model.
//!
//! Errors here know the item but not the request; the HTTP layer adds the
//! PMS URL and call when it turns them into `CatalogError::Backend`.

use std::time::Duration;

use spoolframe_types::{
    Album, Artist, Artwork, ArtworkId, ArtworkKind, AudioCodec, CalendarDate, CastMember, Episode,
    Item, Library, LibraryId, LibraryKind, Marker, MarkerKind, MediaId, MediaKind, Movie,
    Namespace, PersonRole, PlaybackState, Season, Show, Track,
};

use serde::Deserialize;

use crate::wire::{self, Directory, Metadata};

/// Why a PMS item could not be converted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvertError {
    /// The item's `ratingKey`, when the problem is in one item.
    pub item: Option<String>,
    pub message: String,
}

impl ConvertError {
    fn in_item(m: &Metadata, message: impl Into<String>) -> Self {
        Self {
            item: Some(m.rating_key.clone()),
            message: message.into(),
        }
    }
}

/// A library section, or `None` for kinds Spoolframe does not serve
/// (photos, home videos).
pub fn library(dir: &Directory) -> Result<Option<Library>, ConvertError> {
    let kind = match dir.kind.as_deref() {
        Some("movie") => LibraryKind::Movies,
        Some("show") => LibraryKind::Shows,
        Some("artist") => LibraryKind::Music,
        other => {
            tracing::debug!(section = %dir.key, kind = ?other, "skipping Plex library of unsupported kind");
            return Ok(None);
        }
    };
    let id = local_key(&dir.key)
        .and_then(|key| LibraryId::new(Namespace::Plex, key).map_err(|e| e.to_string()))
        .map_err(|e| ConvertError {
            item: None,
            message: format!("library {:?}: {e}", dir.title),
        })?;
    Ok(Some(Library {
        id,
        title: dir.title.clone(),
        kind,
        server: None,
    }))
}

/// Whether Spoolframe models items of Plex `type` `kind`.
pub fn is_modelled(kind: &str) -> bool {
    media_kind(kind).is_some()
}

/// Plex's name for a media kind, as in the `type` field.
pub fn media_kind(plex: &str) -> Option<MediaKind> {
    Some(match plex {
        "movie" => MediaKind::Movie,
        "show" => MediaKind::Show,
        "season" => MediaKind::Season,
        "episode" => MediaKind::Episode,
        "artist" => MediaKind::Artist,
        "album" => MediaKind::Album,
        "track" => MediaKind::Track,
        _ => return None,
    })
}

/// Plex's `type` for a media kind: [`media_kind`] the other way.
pub fn plex_name(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Movie => "movie",
        MediaKind::Show => "show",
        MediaKind::Season => "season",
        MediaKind::Episode => "episode",
        MediaKind::Artist => "artist",
        MediaKind::Album => "album",
        MediaKind::Track => "track",
    }
}

/// `key` from the PMS (a `ratingKey` or a section's key), if it can be an
/// id's key. Spoolframe's ids read a key with a `:` as naming another
/// Plex server (`plex:movie:cabin:12`), and a Plex key never has one; a
/// PMS that hands one out would have its items and watch state taken for
/// another server's (finding 60).
pub(crate) fn local_key(key: &str) -> Result<&str, String> {
    if key.contains(':') {
        return Err(format!("key {key:?} has a ':', which names another server"));
    }
    Ok(key)
}

/// The `MediaId` of an item with this `ratingKey` and kind.
pub fn media_id(kind: MediaKind, rating_key: &str) -> Result<MediaId, ConvertError> {
    local_key(rating_key)
        .and_then(|key| MediaId::new(Namespace::Plex, kind, key).map_err(|e| e.to_string()))
        .map_err(|message| ConvertError {
            item: Some(rating_key.to_owned()),
            message,
        })
}

/// One item. A `type` Spoolframe does not model is an error: asking for an
/// item and getting something else back is a broken request, not a filter.
pub fn item(m: &Metadata) -> Result<Item, ConvertError> {
    let kind = media_kind(&m.kind)
        .ok_or_else(|| ConvertError::in_item(m, format!("unsupported type {:?}", m.kind)))?;
    let id = media_id(kind, &m.rating_key)?;
    Ok(match kind {
        MediaKind::Movie => Item::Movie(Movie {
            id,
            title: m.title.clone(),
            // As on an episode: where Plex has only the date, the year is
            // the date's.
            year: m.year.or(m.originally_available_at.map(CalendarDate::year)),
            release_date: m.originally_available_at,
            summary: summary(m),
            content_rating: content_rating(m),
            duration: duration(m),
            cast: Vec::new(),
            artwork: artwork(m, ArtworkKind::Poster)?,
            playback: PlaybackState::default(),
        }),
        MediaKind::Show => Item::Show(Show {
            id,
            title: m.title.clone(),
            year: m.year,
            summary: summary(m),
            content_rating: content_rating(m),
            season_count: m.child_count,
            genres: m.genres.iter().map(|g| g.tag.clone()).collect(),
            last_aired: None,
            cast: Vec::new(),
            artwork: artwork(m, ArtworkKind::Poster)?,
        }),
        MediaKind::Season => Item::Season(Season {
            id,
            show_id: parent(m, MediaKind::Show)?,
            show_title: m.parent_title.clone(),
            season: required(m, m.index, "index")?,
            title: m.title.clone(),
            artwork: artwork(m, ArtworkKind::Poster)?,
        }),
        MediaKind::Episode => Item::Episode(Episode {
            id,
            show_id: grandparent(m, MediaKind::Show)?,
            show_title: m.grandparent_title.clone(),
            season_id: parent(m, MediaKind::Season)?,
            season: required(m, m.parent_index, "parentIndex")?,
            episode: required(m, m.index, "index")?,
            title: m.title.clone(),
            summary: summary(m),
            content_rating: content_rating(m),
            // Plex gives an episode a year too; where it has only the
            // date, the year is the date's.
            year: m.year.or(m.originally_available_at.map(CalendarDate::year)),
            air_date: m.originally_available_at,
            duration: duration(m),
            cast: Vec::new(),
            // An episode's thumb is a still from it, not a poster; its
            // poster is its season's, else its show's.
            artwork: with_poster(
                artwork(m, ArtworkKind::Thumb)?,
                m,
                m.parent_thumb.as_deref().or(m.grandparent_thumb.as_deref()),
            )?,
            playback: PlaybackState::default(),
        }),
        MediaKind::Artist => Item::Artist(Artist {
            id,
            name: m.title.clone(),
            summary: summary(m),
            artwork: artwork(m, ArtworkKind::Poster)?,
        }),
        MediaKind::Album => Item::Album(Album {
            id,
            artist_id: parent(m, MediaKind::Artist)?,
            artist_name: m.parent_title.clone(),
            title: m.title.clone(),
            year: m.year,
            track_count: m.leaf_count,
            artwork: artwork(m, ArtworkKind::Poster)?,
        }),
        MediaKind::Track => Item::Track(Track {
            id,
            album_id: parent(m, MediaKind::Album)?,
            artist_id: grandparent(m, MediaKind::Artist)?,
            // A track's own artist (`originalTitle`, from its tags) is set
            // only where it is not the album's; the album artist is the
            // `grandparentTitle`. A compilation's album artist is "Various
            // Artists", so the track's own name is the one to show.
            artist_name: m
                .original_title
                .clone()
                .or_else(|| m.grandparent_title.clone()),
            album_title: m.parent_title.clone(),
            title: m.title.clone(),
            track: m.index,
            disc: m.parent_index,
            album_track_count: None,
            codec: m
                .media
                .first()
                .and_then(|media| media.audio_codec.as_deref())
                .map(audio_codec),
            bit_depth: audio_stream(m).and_then(|s| s.bit_depth),
            sample_rate: audio_stream(m).and_then(|s| s.sampling_rate),
            duration: duration(m),
            // The album's cover is the poster, as on the album; Plex
            // gives a track its album's as its own thumb too. `art` is
            // the artist's background.
            artwork: images(
                m,
                [
                    (
                        m.parent_thumb.as_deref().or(m.thumb.as_deref()),
                        ArtworkKind::Poster,
                    ),
                    (m.art.as_deref(), ArtworkKind::Background),
                ],
            )?,
            playback: PlaybackState::default(),
        }),
    })
}

/// The item's stable ids: Plex's guid, then the other agents'. Local-only
/// guids (`local://…`, from items Plex could not match) are not stable
/// across a rebuild and are left out.
pub fn external_ids(m: &Metadata) -> Vec<String> {
    m.guid
        .iter()
        .chain(m.guids.iter().map(|g| &g.id))
        .filter(|id| !id.starts_with("local://"))
        .cloned()
        .collect()
}

/// The item's intros and credits, in order of start. Plex's other marker
/// types (`commercial`) are left out, and so is a marker that cannot be
/// read or that does not end after it starts: the item is whole without
/// it.
pub fn markers(m: &Metadata) -> Vec<Marker> {
    let mut markers: Vec<_> = m
        .markers
        .iter()
        .filter_map(|marker| {
            let marker = match wire::Marker::deserialize(marker) {
                Ok(marker) => marker,
                Err(e) => {
                    tracing::debug!(
                        rating_key = %m.rating_key,
                        error = %e,
                        "skipping a Plex marker that cannot be read"
                    );
                    return None;
                }
            };
            let kind = match marker.kind.as_str() {
                "intro" => MarkerKind::Intro,
                "credits" => MarkerKind::Credits,
                _ => return None,
            };
            if marker.end_time_offset <= marker.start_time_offset {
                tracing::debug!(
                    rating_key = %m.rating_key,
                    kind = %marker.kind,
                    start_ms = marker.start_time_offset,
                    end_ms = marker.end_time_offset,
                    "skipping a Plex marker that does not end after it starts"
                );
                return None;
            }
            Some(Marker {
                kind,
                start: std::time::Duration::from_millis(marker.start_time_offset),
                end: std::time::Duration::from_millis(marker.end_time_offset),
                to_end: marker.is_final,
            })
        })
        .collect();
    markers.sort_by_key(|marker| marker.start);
    markers
}

/// The host Plex's own agent keeps people's photos on, and the artwork
/// key a photo there is known by: `/metadata-static/<its path>`. A photo
/// on another host (an older agent's, on TMDB) is left out, so an
/// artwork id can never make the PMS fetch a URL of a client's choosing.
const PEOPLE_HOST: &str = "metadata-static.plex.tv";
const PEOPLE_KEY: &str = "/metadata-static/";

/// `item`'s cast, from `m`, its own response: the cast comes only with
/// an item's own read, never in listings, where Plex names a few people
/// without the ids they are listed by.
pub fn add_cast(item: &mut Item, m: &Metadata) {
    match item {
        Item::Movie(movie) => movie.cast = cast(m),
        Item::Show(show) => show.cast = cast(m),
        Item::Episode(episode) => episode.cast = cast(m),
        Item::Season(_) | Item::Artist(_) | Item::Album(_) | Item::Track(_) => {}
    }
}

/// The item's actors in billing order, then its directors. One Plex sends
/// without an id, or in another shape, is left out: the item is whole
/// without them.
pub fn cast(m: &Metadata) -> Vec<CastMember> {
    let people = |tags: &[serde_json::Value], role| {
        tags.iter()
            .filter_map(move |tag| match wire::Person::deserialize(tag) {
                Ok(person) => Some(CastMember {
                    id: crate::search::person_id(role, person.id),
                    name: person.tag,
                    character: person.role.filter(|r| !r.trim().is_empty()),
                    photo: person.thumb.as_deref().and_then(person_photo),
                    role,
                }),
                Err(e) => {
                    tracing::debug!(
                        rating_key = %m.rating_key,
                        error = %e,
                        "skipping a Plex person that cannot be read"
                    );
                    None
                }
            })
            .collect::<Vec<_>>()
    };
    let mut cast = people(&m.roles, PersonRole::Actor);
    cast.extend(people(&m.directors, PersonRole::Director));
    cast
}

/// The artwork id of a person's photo at `url`, when it is on Plex's
/// people host.
fn person_photo(url: &str) -> Option<ArtworkId> {
    let path = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?
        .strip_prefix(PEOPLE_HOST)?
        .strip_prefix('/')?;
    let key = format!("{PEOPLE_KEY}{path}");
    person_photo_url(&key)?;
    ArtworkId::new(Namespace::Plex, key).ok()
}

/// The URL of the person's photo an artwork key names, or `None` when it
/// names none. Each part of the path is plain (letters, digits, `-`,
/// `_`, `.`), so the URL cannot leave the path or the host.
pub fn person_photo_url(key: &str) -> Option<String> {
    let path = key.strip_prefix(PEOPLE_KEY)?;
    let plain = |part: &str| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    path.split('/')
        .all(plain)
        .then(|| format!("https://{PEOPLE_HOST}/{path}"))
}

/// The media files Plex reports for the item: every part of its first
/// version, in order. Choosing between versions is later work.
pub fn media_files(m: &Metadata) -> Vec<&str> {
    m.media
        .first()
        .map(|media| {
            media
                .parts
                .iter()
                .filter_map(|part| part.file.as_deref())
                .collect()
        })
        .unwrap_or_default()
}

/// Plex's name for an audio codec (`audioCodec`, a stream's `codec`) as
/// the shared vocabulary has it; one it does not know is `Unknown`.
pub fn audio_codec(plex: &str) -> AudioCodec {
    match plex {
        "aac" => AudioCodec::Aac,
        "ac3" => AudioCodec::Ac3,
        "eac3" => AudioCodec::Eac3,
        "dca" | "dts" => AudioCodec::Dts,
        "truehd" => AudioCodec::TrueHd,
        "flac" => AudioCodec::Flac,
        "alac" => AudioCodec::Alac,
        "opus" => AudioCodec::Opus,
        "vorbis" => AudioCodec::Vorbis,
        "mp3" => AudioCodec::Mp3,
        pcm if pcm.starts_with("pcm") => AudioCodec::Pcm,
        _ => AudioCodec::Unknown,
    }
}

/// The first audio stream of the first part of the first version: what a
/// track plays. Plex lists lyrics as streams too.
fn audio_stream(m: &Metadata) -> Option<&wire::Stream> {
    m.media
        .first()?
        .parts
        .first()?
        .streams
        .iter()
        .find(|s| s.stream_type == Some(wire::Stream::AUDIO))
}

fn summary(m: &Metadata) -> Option<String> {
    m.summary.clone().filter(|s| !s.trim().is_empty())
}

fn content_rating(m: &Metadata) -> Option<String> {
    m.content_rating
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_owned)
}

fn duration(m: &Metadata) -> Option<Duration> {
    m.duration.map(Duration::from_millis)
}

fn required(m: &Metadata, value: Option<u32>, field: &str) -> Result<u32, ConvertError> {
    value.ok_or_else(|| ConvertError::in_item(m, format!("{} has no {field}", m.kind)))
}

fn parent(m: &Metadata, kind: MediaKind) -> Result<MediaId, ConvertError> {
    let key = m
        .parent_rating_key
        .as_deref()
        .ok_or_else(|| ConvertError::in_item(m, format!("{} has no parentRatingKey", m.kind)))?;
    media_id(kind, key)
}

fn grandparent(m: &Metadata, kind: MediaKind) -> Result<MediaId, ConvertError> {
    let key = m.grandparent_rating_key.as_deref().ok_or_else(|| {
        ConvertError::in_item(m, format!("{} has no grandparentRatingKey", m.kind))
    })?;
    media_id(kind, key)
}

/// `thumb` as `primary`, and `art` as the background. The key is the PMS
/// image path; the artwork proxy (P1-05) fetches it.
fn artwork(m: &Metadata, primary: ArtworkKind) -> Result<Vec<Artwork>, ConvertError> {
    images(
        m,
        [
            (m.thumb.as_deref(), primary),
            (m.art.as_deref(), ArtworkKind::Background),
        ],
    )
}

/// Each PMS image path that is there, as artwork of its kind.
fn images<const N: usize>(
    m: &Metadata,
    paths: [(Option<&str>, ArtworkKind); N],
) -> Result<Vec<Artwork>, ConvertError> {
    paths
        .into_iter()
        .filter_map(|(path, kind)| path.map(|p| (p, kind)))
        .map(|(path, kind)| {
            ArtworkId::new(Namespace::Plex, path)
                .map(|id| Artwork { id, kind })
                .map_err(|e| ConvertError::in_item(m, e.to_string()))
        })
        .collect()
}

/// `artwork` with `poster` added as the poster, when there is one.
fn with_poster(
    mut artwork: Vec<Artwork>,
    m: &Metadata,
    poster: Option<&str>,
) -> Result<Vec<Artwork>, ConvertError> {
    if let Some(path) = poster {
        let id = ArtworkId::new(Namespace::Plex, path)
            .map_err(|e| ConvertError::in_item(m, e.to_string()))?;
        artwork.push(Artwork {
            id,
            kind: ArtworkKind::Poster,
        });
    }
    Ok(artwork)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{Envelope, MediaContainer};
    use spoolframe_types::PersonId;

    fn fixture(name: &str) -> MediaContainer {
        let path = format!("{}/tests/fixtures/{name}.json", env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        serde_json::from_str::<Envelope>(&text)
            .unwrap_or_else(|e| panic!("{path}: {e}"))
            .container
    }

    fn only_item(name: &str) -> Item {
        let c = fixture(name);
        assert_eq!(c.metadata.len(), 1, "{name}");
        item(&c.metadata[0]).unwrap()
    }

    fn id(kind: MediaKind, key: &str) -> MediaId {
        MediaId::new(Namespace::Plex, kind, key).unwrap()
    }

    fn art(kind: ArtworkKind, path: &str) -> Artwork {
        Artwork {
            id: ArtworkId::new(Namespace::Plex, path).unwrap(),
            kind,
        }
    }

    #[test]
    fn sections_become_libraries_of_each_kind() {
        let libraries: Vec<_> = fixture("sections")
            .directories
            .iter()
            .filter_map(|d| library(d).unwrap())
            .collect();
        assert_eq!(
            libraries,
            vec![
                Library {
                    id: LibraryId::new(Namespace::Plex, "2").unwrap(),
                    title: "Movies".into(),
                    kind: LibraryKind::Movies,
                    server: None,
                },
                Library {
                    id: LibraryId::new(Namespace::Plex, "1").unwrap(),
                    title: "TV Shows".into(),
                    kind: LibraryKind::Shows,
                    server: None,
                },
                Library {
                    id: LibraryId::new(Namespace::Plex, "12").unwrap(),
                    title: "Music".into(),
                    kind: LibraryKind::Music,
                    server: None,
                },
            ]
        );
    }

    /// Finding 60 (G-11): a key with a `:` names another server in
    /// Spoolframe's ids, so one from the PMS would be taken for another
    /// server's item.
    #[test]
    fn a_rating_key_with_a_colon_is_refused() {
        let mut m = fixture("metadata-movie").metadata.remove(0);
        m.rating_key = "cabin:12".into();
        assert!(item(&m).is_err());
    }

    #[test]
    fn a_parent_s_rating_key_with_a_colon_is_refused() {
        let mut m = fixture("metadata-episode").metadata.remove(0);
        m.grandparent_rating_key = Some("cabin:12".into());
        assert!(item(&m).is_err());
    }

    #[test]
    fn a_section_key_with_a_colon_is_refused() {
        let dir = Directory {
            key: "cabin:2".into(),
            kind: Some("movie".into()),
            title: "Movies".into(),
            locations: Vec::new(),
            filter: None,
        };
        assert!(library(&dir).is_err());
    }

    #[test]
    fn a_photo_library_is_skipped() {
        let dir = Directory {
            key: "5".into(),
            kind: Some("photo".into()),
            title: "Photos".into(),
            locations: Vec::new(),
            filter: None,
        };
        assert_eq!(library(&dir), Ok(None));
    }

    #[test]
    fn a_movie_converts() {
        let Item::Movie(movie) = only_item("metadata-movie") else {
            panic!("not a movie")
        };
        assert_eq!(movie.id, id(MediaKind::Movie, "134883"));
        assert_eq!(movie.title, "6 Below: Miracle on the Mountain");
        assert_eq!(movie.year, Some(2017));
        assert_eq!(movie.duration, Some(Duration::from_millis(5_863_942)));
        assert!(movie.summary.is_some());
        assert_eq!(
            movie.artwork,
            vec![
                art(
                    ArtworkKind::Poster,
                    "/library/metadata/134883/thumb/1786645049"
                ),
                art(
                    ArtworkKind::Background,
                    "/library/metadata/134883/art/1786645049"
                ),
            ]
        );
    }

    #[test]
    fn a_show_converts() {
        let Item::Show(show) = only_item("metadata-show") else {
            panic!("not a show")
        };
        assert_eq!(show.id, id(MediaKind::Show, "208618"));
        assert_eq!(show.title, "12 Monkeys");
        assert_eq!(show.year, Some(2015));
    }

    #[test]
    fn a_show_says_how_many_seasons_and_which_genres_it_has() {
        let Item::Show(show) = only_item("metadata-show") else {
            panic!("not a show")
        };
        assert_eq!(show.season_count, Some(4));
        assert_eq!(
            show.genres,
            [
                "Thriller",
                "Science Fiction",
                "Mystery",
                "Adventure",
                "Drama",
                "Sci-Fi & Fantasy",
                "Action",
                "Suspense"
            ]
        );
    }

    #[test]
    fn a_show_in_a_listing_has_the_genres_plex_lists_there() {
        let shows: Vec<_> = fixture("section-all-show")
            .metadata
            .iter()
            .map(|m| match item(m).unwrap() {
                Item::Show(s) => (s.season_count, s.genres),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            shows,
            [
                (
                    Some(4),
                    vec!["Thriller".to_owned(), "Science Fiction".into()]
                ),
                (Some(9), vec!["Drama".to_owned(), "Crime".into()]),
            ]
        );
    }

    #[test]
    fn a_show_without_counts_or_genres_has_none() {
        let mut m = fixture("metadata-show").metadata.remove(0);
        m.child_count = None;
        m.genres.clear();
        let Item::Show(show) = item(&m).unwrap() else {
            panic!("not a show")
        };
        assert_eq!((show.season_count, show.genres), (None, Vec::new()));
    }

    #[test]
    fn a_season_knows_its_show_and_number() {
        let Item::Season(season) = only_item("metadata-season") else {
            panic!("not a season")
        };
        assert_eq!(season.id, id(MediaKind::Season, "208619"));
        assert_eq!(season.show_id, id(MediaKind::Show, "208618"));
        assert_eq!(season.season, 1);
        assert_eq!(season.title, "Season 1");
    }

    #[test]
    fn an_episode_knows_its_show_season_and_numbers() {
        let Item::Episode(ep) = only_item("metadata-episode") else {
            panic!("not an episode")
        };
        assert_eq!(ep.id, id(MediaKind::Episode, "208620"));
        assert_eq!(ep.show_id, id(MediaKind::Show, "208618"));
        assert_eq!(ep.season_id, id(MediaKind::Season, "208619"));
        assert_eq!((ep.season, ep.episode), (1, 1));
        assert_eq!(ep.title, "Splinter");
        assert_eq!(ep.artwork[0].kind, ArtworkKind::Thumb);
    }

    #[test]
    fn an_episode_carries_the_day_it_aired_and_its_year() {
        let Item::Episode(ep) = only_item("metadata-episode") else {
            panic!("not an episode")
        };
        assert_eq!(ep.air_date, CalendarDate::new(2015, 1, 16));
        assert_eq!(ep.year, Some(2015));
    }

    #[test]
    fn an_episode_with_only_an_air_date_takes_its_year() {
        let mut m = fixture("metadata-episode").metadata.remove(0);
        m.year = None;
        let Item::Episode(ep) = item(&m).unwrap() else {
            panic!("not an episode")
        };
        assert_eq!(ep.year, Some(2015));
    }

    #[test]
    fn an_air_date_in_another_shape_is_left_out_not_an_error() {
        let text = std::fs::read_to_string(format!(
            "{}/tests/fixtures/metadata-episode.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
        .replace("\"2015-01-16\"", "\"16/01/2015\"");
        let mut c = serde_json::from_str::<Envelope>(&text).unwrap().container;
        let Item::Episode(ep) = item(&c.metadata.remove(0)).unwrap() else {
            panic!("not an episode")
        };
        assert_eq!((ep.air_date, ep.year), (None, Some(2015)));
    }

    #[test]
    fn an_episode_carries_its_show_s_title_and_its_season_s_poster() {
        let Item::Episode(ep) = only_item("metadata-episode") else {
            panic!("not an episode")
        };
        assert_eq!(ep.show_title.as_deref(), Some("12 Monkeys"));
        let poster = ep
            .artwork
            .iter()
            .find(|a| a.kind == ArtworkKind::Poster)
            .expect("a poster");
        assert!(
            poster
                .id
                .key()
                .starts_with("/library/metadata/208619/thumb/"),
            "{poster:?}"
        );
    }

    #[test]
    fn a_season_and_an_album_carry_their_parent_s_name() {
        let Item::Season(season) = only_item("metadata-season") else {
            panic!("not a season")
        };
        assert_eq!(season.show_title.as_deref(), Some("12 Monkeys"));
        let Item::Album(album) = only_item("metadata-album") else {
            panic!("not an album")
        };
        assert_eq!(album.artist_name.as_deref(), Some("The Cranberries"));
    }

    #[test]
    fn plex_view_state_is_not_carried_over() {
        // The fixture has viewCount 2; Spoolframe keeps its own per-user
        // state (§36).
        let Item::Episode(ep) = only_item("metadata-episode") else {
            panic!("not an episode")
        };
        assert_eq!(ep.playback, PlaybackState::default());
    }

    #[test]
    fn an_artist_converts() {
        let Item::Artist(artist) = only_item("metadata-artist") else {
            panic!("not an artist")
        };
        assert_eq!(artist.id, id(MediaKind::Artist, "222392"));
        assert_eq!(artist.name, "The Cranberries");
    }

    #[test]
    fn an_album_knows_its_artist() {
        let Item::Album(album) = only_item("metadata-album") else {
            panic!("not an album")
        };
        assert_eq!(album.id, id(MediaKind::Album, "223609"));
        assert_eq!(album.artist_id, id(MediaKind::Artist, "222392"));
        assert_eq!(album.title, "In the End");
        assert_eq!(album.year, Some(2019));
    }

    #[test]
    fn a_track_knows_its_album_artist_and_position() {
        let Item::Track(track) = only_item("metadata-track") else {
            panic!("not a track")
        };
        assert_eq!(track.id, id(MediaKind::Track, "223610"));
        assert_eq!(track.album_id, id(MediaKind::Album, "223609"));
        assert_eq!(track.artist_id, id(MediaKind::Artist, "222392"));
        assert_eq!((track.track, track.disc), (Some(1), Some(1)));
        assert_eq!(track.duration, Some(Duration::from_millis(257_386)));
    }

    /// An album's own tracks say whose they are too (PX-07), so a track
    /// queued or shown away from its album still does. The fixture is
    /// hand-written in the shape the PMS sends.
    #[test]
    fn an_album_s_tracks_carry_their_artist_album_and_cover() {
        let tracks: Vec<_> = fixture("children-album-whose")
            .metadata
            .iter()
            .map(|m| match item(m).unwrap() {
                Item::Track(t) => t,
                other => panic!("{other:?}"),
            })
            .collect();
        let whose: Vec<_> = tracks
            .iter()
            .map(|t| (t.artist_name.as_deref(), t.album_title.as_deref()))
            .collect();
        assert_eq!(
            whose,
            [
                (Some("Mara Quell"), Some("Harbour Lights")),
                // The track's own artist, from its tags, over the album's.
                (Some("Mara Quell with The Lanterns"), Some("Harbour Lights")),
            ]
        );
        for track in &tracks {
            let artwork: Vec<_> = track
                .artwork
                .iter()
                .map(|a| (a.kind, a.id.key().to_owned()))
                .collect();
            assert_eq!(
                artwork,
                [
                    (
                        ArtworkKind::Poster,
                        "/library/metadata/900100/thumb/1700000100".to_owned()
                    ),
                    (
                        ArtworkKind::Background,
                        "/library/metadata/900001/art/1700000001".to_owned()
                    ),
                ],
                "{}",
                track.title
            );
        }
    }

    /// Without a `parentThumb`, the track's own thumb is the cover: Plex
    /// gives a track its album's.
    #[test]
    fn a_track_without_its_album_s_thumb_takes_its_own_as_the_cover() {
        let Item::Track(track) = only_item("metadata-track") else {
            panic!("not a track")
        };
        let poster: Vec<_> = track
            .artwork
            .iter()
            .filter(|a| a.kind == ArtworkKind::Poster)
            .map(|a| a.id.key().to_owned())
            .collect();
        assert_eq!(poster, ["/library/metadata/223609/thumb/1783793666"]);
    }

    #[test]
    fn movies_shows_and_episodes_carry_their_content_rating() {
        let Item::Movie(movie) = only_item("metadata-movie") else {
            panic!("not a movie")
        };
        assert_eq!(movie.content_rating.as_deref(), Some("PG-13"));
        let Item::Show(show) = only_item("metadata-show") else {
            panic!("not a show")
        };
        assert_eq!(show.content_rating.as_deref(), Some("TV-14"));
        let Item::Episode(ep) = only_item("metadata-episode") else {
            panic!("not an episode")
        };
        assert_eq!(ep.content_rating.as_deref(), Some("TV-14"));
    }

    #[test]
    fn listings_carry_the_content_rating_too() {
        for name in ["section-all-show", "children-season"] {
            for m in &fixture(name).metadata {
                let rating = match item(m).unwrap() {
                    Item::Show(s) => s.content_rating,
                    Item::Episode(e) => e.content_rating,
                    other => panic!("{other:?}"),
                };
                assert_eq!(rating.as_deref(), Some("TV-14"), "{name}");
            }
        }
    }

    #[test]
    fn an_unrated_item_has_no_content_rating() {
        let mut m = fixture("metadata-movie").metadata.remove(0);
        for unrated in [None, Some(String::new()), Some("  ".to_owned())] {
            m.content_rating = unrated;
            let Item::Movie(movie) = item(&m).unwrap() else {
                panic!("not a movie")
            };
            assert_eq!(movie.content_rating, None);
        }
    }

    #[test]
    fn a_track_says_its_codec_bit_depth_and_sample_rate() {
        let Item::Track(track) = only_item("metadata-track") else {
            panic!("not a track")
        };
        assert_eq!(
            (track.codec, track.bit_depth, track.sample_rate),
            (Some(AudioCodec::Flac), Some(16), Some(44_100))
        );
    }

    #[test]
    fn a_hi_res_track_s_format_is_read_past_its_lyrics() {
        let c = fixture("metadata-tracks-catch");
        let Item::Track(track) = item(&c.metadata[0]).unwrap() else {
            panic!("not a track")
        };
        assert_eq!(
            (track.codec, track.bit_depth, track.sample_rate),
            (Some(AudioCodec::Flac), Some(24), Some(96_000))
        );
    }

    #[test]
    fn a_track_in_a_listing_has_its_codec_but_no_bit_depth_or_sample_rate() {
        for m in &fixture("children-album-catch").metadata {
            let Item::Track(track) = item(m).unwrap() else {
                panic!("not a track")
            };
            assert_eq!(
                (track.codec, track.bit_depth, track.sample_rate),
                (Some(AudioCodec::Flac), None, None)
            );
            // Only the album's own listing knows how many it has.
            assert_eq!(track.album_track_count, None);
        }
    }

    #[test]
    fn plex_s_audio_codec_names_become_the_shared_ones() {
        for (plex, codec) in [
            ("flac", AudioCodec::Flac),
            ("mp3", AudioCodec::Mp3),
            ("aac", AudioCodec::Aac),
            ("alac", AudioCodec::Alac),
            ("dca", AudioCodec::Dts),
            ("truehd", AudioCodec::TrueHd),
            ("pcm", AudioCodec::Pcm),
            ("wmapro", AudioCodec::Unknown),
        ] {
            assert_eq!(audio_codec(plex), codec, "{plex}");
        }
    }

    #[test]
    fn an_album_says_how_many_tracks_it_has() {
        let Item::Album(album) = only_item("metadata-album-catch") else {
            panic!("not an album")
        };
        assert_eq!(album.track_count, Some(3));
        // Plex leaves the count out of some answers.
        let Item::Album(album) = only_item("metadata-album") else {
            panic!("not an album")
        };
        assert_eq!(album.track_count, None);
    }

    #[test]
    fn an_empty_summary_is_none() {
        let c = fixture("metadata-track");
        assert_eq!(c.metadata[0].summary.as_deref(), Some(""));
        assert_eq!(summary(&c.metadata[0]), None);
    }

    #[test]
    fn children_of_each_parent_convert() {
        for (name, kind) in [
            ("children-show", MediaKind::Season),
            ("children-season", MediaKind::Episode),
            ("children-artist", MediaKind::Album),
            ("children-album", MediaKind::Track),
        ] {
            let c = fixture(name);
            assert!(!c.metadata.is_empty(), "{name}");
            for m in &c.metadata {
                assert_eq!(item(m).unwrap().id().kind(), kind, "{name}");
            }
        }
    }

    #[test]
    fn external_ids_are_plex_s_guid_and_the_agents() {
        assert_eq!(
            external_ids(&fixture("metadata-movie").metadata[0]),
            [
                "plex://movie/5d776c85fb0d55001f589f07",
                "imdb://tt5503688",
                "tmdb://423087",
                "tvdb://4670"
            ]
        );
    }

    #[test]
    fn listings_carry_the_library_total() {
        assert_eq!(fixture("section-all-movie").total_size, Some(770));
    }

    #[test]
    fn an_unknown_item_type_is_an_error_naming_the_item() {
        let mut m = fixture("metadata-movie").metadata.remove(0);
        m.kind = "clip".into();
        let err = item(&m).unwrap_err();
        assert_eq!(err.item.as_deref(), Some("134883"));
        assert!(err.message.contains("clip"), "{}", err.message);
    }

    #[test]
    fn an_episode_without_a_number_is_an_error() {
        let mut m = fixture("metadata-episode").metadata.remove(0);
        m.index = None;
        assert!(item(&m).unwrap_err().message.contains("index"));
    }

    #[test]
    fn a_track_without_an_album_is_an_error() {
        let mut m = fixture("metadata-track").metadata.remove(0);
        m.parent_rating_key = None;
        assert!(item(&m).unwrap_err().message.contains("parentRatingKey"));
    }

    #[test]
    fn the_media_files_are_the_parts_of_the_first_version() {
        let c = fixture("metadata-episode");
        assert_eq!(
            media_files(&c.metadata[0]),
            ["/data/TVShows/12 Monkeys/Season 1/12.Monkeys.S01E01.Splinter.Bluray-1080p.Remux.mkv"]
        );
    }

    #[test]
    fn a_movie_carries_the_day_it_came_out_and_its_year() {
        let Item::Movie(movie) = only_item("metadata-movie-cast") else {
            panic!("not a movie")
        };
        assert_eq!(
            (movie.release_date, movie.year),
            (CalendarDate::new(1979, 5, 25), Some(1979))
        );
    }

    #[test]
    fn a_movie_with_only_a_release_date_takes_its_year() {
        let mut m = fixture("metadata-movie-cast").metadata.remove(0);
        m.year = None;
        let Item::Movie(movie) = item(&m).unwrap() else {
            panic!("not a movie")
        };
        assert_eq!(movie.year, Some(1979));
    }

    /// The dates are in Plex's listings too, so a library's rows and the
    /// home rows have them without each item's own read.
    #[test]
    fn listings_carry_the_release_and_air_dates_and_years() {
        let dates: Vec<_> = fixture("watch-movies-watched")
            .metadata
            .iter()
            .map(|m| match item(m).unwrap() {
                Item::Movie(movie) => (movie.year, movie.release_date),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            dates,
            [
                (Some(1979), CalendarDate::new(1979, 5, 25)),
                (Some(2017), CalendarDate::new(2017, 5, 9)),
            ]
        );
        let dates: Vec<_> = fixture("watch-episodes-watched")
            .metadata
            .iter()
            .map(|m| match item(m).unwrap() {
                Item::Episode(ep) => (ep.year, ep.air_date),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            dates,
            [
                (Some(2015), CalendarDate::new(2015, 1, 16)),
                (Some(2015), CalendarDate::new(2015, 1, 22)),
            ]
        );
    }

    fn person(key: &str) -> PersonId {
        PersonId::new(Namespace::Plex, key).unwrap()
    }

    #[test]
    fn an_item_s_cast_is_its_actors_in_billing_order_then_its_directors() {
        let cast = cast(&fixture("metadata-movie-cast").metadata[0]);
        let who: Vec<_> = cast
            .iter()
            .map(|c| {
                (
                    c.id.clone(),
                    c.name.as_str(),
                    c.role,
                    c.character.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            who,
            [
                (
                    person("actor-21052"),
                    "Tom Skerritt",
                    PersonRole::Actor,
                    Some("Dallas")
                ),
                (
                    person("actor-13434"),
                    "Sigourney Weaver",
                    PersonRole::Actor,
                    Some("Ripley")
                ),
                (
                    person("actor-30061"),
                    "Veronica Cartwright",
                    PersonRole::Actor,
                    Some("Lambert")
                ),
                (
                    person("actor-30062"),
                    "Harry Dean Stanton",
                    PersonRole::Actor,
                    Some("Brett")
                ),
                (
                    person("director-20469"),
                    "Ridley Scott",
                    PersonRole::Director,
                    None
                ),
            ]
        );
    }

    /// The id is the tag's, as a person search gives it, so the person's
    /// items list from a cast member as from a found person.
    #[test]
    fn a_cast_member_has_the_id_a_person_search_gives() {
        let cast = cast(&fixture("metadata-movie-cast").metadata[0]);
        assert_eq!(
            cast[1].id,
            crate::search::person_id(PersonRole::Actor, 13434)
        );
    }

    #[test]
    fn a_cast_member_s_photo_on_plex_s_people_host_is_artwork() {
        let cast = cast(&fixture("metadata-movie-cast").metadata[0]);
        let photos: Vec<_> = cast
            .iter()
            .map(|c| c.photo.as_ref().map(|p| p.key().to_owned()))
            .collect();
        assert_eq!(
            photos,
            [
                Some("/metadata-static/b/people/b4b9a3c1a1f6fc4b9a0f5b8e4f0e2a51.jpg".to_owned()),
                Some("/metadata-static/9/people/9e3c2f9b7b1f2d6a4f1a2b3c4d5e6f70.jpg".to_owned()),
                // No thumb.
                None,
                // An older agent's, on another host: not fetched.
                None,
                Some("/metadata-static/7/people/7a290cd6d0b1d82c84d1bd4a8a1e0e5d.jpg".to_owned()),
            ]
        );
    }

    #[test]
    fn a_photo_key_names_an_image_on_plex_s_people_host_and_nothing_else() {
        assert_eq!(
            person_photo_url("/metadata-static/9/people/9e3c.jpg").as_deref(),
            Some("https://metadata-static.plex.tv/9/people/9e3c.jpg")
        );
        for key in [
            "/library/metadata/1/thumb/2",
            "/metadata-static/",
            "/metadata-static/../library/sections",
            "/metadata-static/9//x.jpg",
            "/metadata-static/9/x.jpg?url=http://nas",
            "/metadata-static/9/x%2e.jpg",
            "/metadata-static/9/x.jpg#",
        ] {
            assert_eq!(person_photo_url(key), None, "{key}");
        }
    }

    /// Plex's listings name only the first few people, without the ids
    /// they are listed by: a listing's items have no cast.
    #[test]
    fn people_named_without_an_id_are_left_out() {
        for name in ["watch-movies-watched", "watch-episodes-watched"] {
            for m in &fixture(name).metadata {
                assert_eq!(cast(m), Vec::new(), "{name}");
            }
        }
    }

    /// A cast comes only with an item's own read; `item` alone, as for a
    /// listing, leaves it out.
    #[test]
    fn an_item_converted_alone_has_no_cast() {
        let Item::Movie(movie) = only_item("metadata-movie-cast") else {
            panic!("not a movie")
        };
        assert_eq!(movie.cast, Vec::new());
    }

    #[test]
    fn a_person_in_another_shape_is_left_out_and_the_item_loads() {
        let mut m = fixture("metadata-movie-cast").metadata.remove(0);
        m.roles[0] = serde_json::json!({ "id": "not a number", "tag": 7 });
        assert_eq!(cast(&m).len(), 4);
        assert!(item(&m).is_ok());
    }

    #[test]
    fn a_show_has_no_media_file() {
        assert!(media_files(&fixture("metadata-show").metadata[0]).is_empty());
    }
}
