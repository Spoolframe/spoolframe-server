//! PMS metadata → Spoolframe's media model.
//!
//! Errors here know the item but not the request; the HTTP layer adds the
//! PMS URL and call when it turns them into `CatalogError::Backend`.

use std::time::Duration;

use spoolframe_types::{
    Album, Artist, Artwork, ArtworkId, ArtworkKind, Episode, Item, Library, LibraryId, LibraryKind,
    Marker, MarkerKind, MediaId, MediaKind, Movie, Namespace, PlaybackState, Season, Show, Track,
};

use serde::Deserialize;

use crate::wire::{self, Directory, Hub, Metadata};

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
    let id = LibraryId::new(Namespace::Plex, dir.key.as_str()).map_err(|e| ConvertError {
        item: None,
        message: format!("library {:?}: {e}", dir.title),
    })?;
    Ok(Some(Library {
        id,
        title: dir.title.clone(),
        kind,
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

/// The `MediaId` of an item with this `ratingKey` and kind.
pub fn media_id(kind: MediaKind, rating_key: &str) -> Result<MediaId, ConvertError> {
    MediaId::new(Namespace::Plex, kind, rating_key).map_err(|e| ConvertError {
        item: Some(rating_key.to_owned()),
        message: e.to_string(),
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
            year: m.year,
            summary: summary(m),
            duration: duration(m),
            artwork: artwork(m, ArtworkKind::Poster)?,
            playback: PlaybackState::default(),
        }),
        MediaKind::Show => Item::Show(Show {
            id,
            title: m.title.clone(),
            year: m.year,
            summary: summary(m),
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
            duration: duration(m),
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
            artwork: artwork(m, ArtworkKind::Poster)?,
        }),
        MediaKind::Track => Item::Track(Track {
            id,
            album_id: parent(m, MediaKind::Album)?,
            artist_id: grandparent(m, MediaKind::Artist)?,
            title: m.title.clone(),
            track: m.index,
            disc: m.parent_index,
            duration: duration(m),
            playback: PlaybackState::default(),
        }),
    })
}

/// Every searchable item in a `/hubs/search` response. Hubs of kinds
/// Spoolframe does not model (actors, genres, photos, playlists) are
/// skipped; an item of a modelled kind that fails to convert is an error.
pub fn search_results(hubs: &[Hub]) -> Result<Vec<Item>, ConvertError> {
    hubs.iter()
        .flat_map(|h| &h.metadata)
        .filter(|m| media_kind(&m.kind).is_some())
        .map(item)
        .collect()
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
                start_ms: marker.start_time_offset,
                end_ms: marker.end_time_offset,
                to_end: marker.is_final,
            })
        })
        .collect();
    markers.sort_by_key(|marker| marker.start_ms);
    markers
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

fn summary(m: &Metadata) -> Option<String> {
    m.summary.clone().filter(|s| !s.trim().is_empty())
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
    [(&m.thumb, primary), (&m.art, ArtworkKind::Background)]
        .into_iter()
        .filter_map(|(path, kind)| path.as_deref().map(|p| (p, kind)))
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
                },
                Library {
                    id: LibraryId::new(Namespace::Plex, "1").unwrap(),
                    title: "TV Shows".into(),
                    kind: LibraryKind::Shows,
                },
                Library {
                    id: LibraryId::new(Namespace::Plex, "12").unwrap(),
                    title: "Music".into(),
                    kind: LibraryKind::Music,
                },
            ]
        );
    }

    #[test]
    fn a_photo_library_is_skipped() {
        let dir = Directory {
            key: "5".into(),
            kind: Some("photo".into()),
            title: "Photos".into(),
            locations: Vec::new(),
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
    fn search_keeps_media_and_skips_other_hubs() {
        let c = fixture("hubs-search");
        let kinds: Vec<_> = search_results(&c.hubs)
            .unwrap()
            .iter()
            .map(|i| i.id().kind())
            .collect();
        assert!(kinds.contains(&MediaKind::Artist), "{kinds:?}");
        assert!(kinds.contains(&MediaKind::Movie), "{kinds:?}");
        assert!(kinds.contains(&MediaKind::Track), "{kinds:?}");
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
    fn a_show_has_no_media_file() {
        assert!(media_files(&fixture("metadata-show").metadata[0]).is_empty());
    }
}
