//! The media model clients browse: libraries, and the movies, shows and
//! music in them (ARCHITECTURE §5).
//!
//! `playback` is filled in by the server for the session's own user; it
//! never comes from Plex (§36).

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::codec::AudioCodec;
use crate::duration_ms;
use crate::id::{ArtworkId, LibraryId, MediaId};

/// What a library holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibraryKind {
    Movies,
    Shows,
    Music,
}

/// A library of one kind of media.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Library {
    pub id: LibraryId,
    pub title: String,
    pub kind: LibraryKind,
}

/// The role a piece of artwork plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtworkKind {
    Poster,
    Background,
    Thumb,
}

/// A reference to artwork. Clients fetch the image through the client
/// protocol with the `id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artwork {
    pub id: ArtworkId,
    pub kind: ArtworkKind,
}

/// The session user's progress on an item.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybackState {
    /// Where playback stopped, or `None` if it never started.
    #[serde(with = "duration_ms::option")]
    pub position: Option<Duration>,
    pub watched: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Movie {
    pub id: MediaId,
    pub title: String,
    pub year: Option<u16>,
    pub summary: Option<String>,
    /// The age rating as the catalog has it ("R", "TV-MA"), shown as
    /// is; `None` when it has none.
    pub content_rating: Option<String>,
    #[serde(with = "duration_ms::option")]
    pub duration: Option<Duration>,
    pub artwork: Vec<Artwork>,
    pub playback: PlaybackState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Show {
    pub id: MediaId,
    pub title: String,
    pub year: Option<u16>,
    pub summary: Option<String>,
    /// The age rating as the catalog has it ("R", "TV-MA"), shown as
    /// is; `None` when it has none.
    pub content_rating: Option<String>,
    /// How many seasons it has, specials counted as one, as the catalog
    /// counts them; `None` when it does not say.
    pub season_count: Option<u32>,
    /// Its genres, by the catalog's names and in its order. A listing
    /// may hold only the first few: Plex gives two in listings and
    /// search, and all of them on the show itself.
    pub genres: Vec<String>,
    pub artwork: Vec<Artwork>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Season {
    pub id: MediaId,
    pub show_id: MediaId,
    /// The show's title, so a season shown alone needs no second request.
    #[serde(default)]
    pub show_title: Option<String>,
    /// The season number; 0 is specials.
    pub season: u32,
    pub title: String,
    pub artwork: Vec<Artwork>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Episode {
    pub id: MediaId,
    pub show_id: MediaId,
    /// The show's title, so an episode shown alone (Continue watching)
    /// needs no second request.
    #[serde(default)]
    pub show_title: Option<String>,
    pub season_id: MediaId,
    pub season: u32,
    pub episode: u32,
    pub title: String,
    pub summary: Option<String>,
    /// The episode's own age rating, as on a movie; a client that has
    /// none here may show the show's.
    pub content_rating: Option<String>,
    #[serde(with = "duration_ms::option")]
    pub duration: Option<Duration>,
    pub artwork: Vec<Artwork>,
    pub playback: PlaybackState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artist {
    pub id: MediaId,
    pub name: String,
    pub summary: Option<String>,
    pub artwork: Vec<Artwork>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Album {
    pub id: MediaId,
    pub artist_id: MediaId,
    /// The artist's name, for the same reason.
    #[serde(default)]
    pub artist_name: Option<String>,
    pub title: String,
    pub year: Option<u16>,
    /// How many tracks it has, on every disc. `None` where the catalog
    /// does not say: Plex counts them on the album itself, not in
    /// listings or search.
    pub track_count: Option<u32>,
    pub artwork: Vec<Artwork>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Track {
    pub id: MediaId,
    pub album_id: MediaId,
    pub artist_id: MediaId,
    pub title: String,
    /// Position on its disc, when the tags say.
    pub track: Option<u32>,
    pub disc: Option<u32>,
    /// How many tracks its album has (as `Album::track_count`), for
    /// "track 3 of 9". Set on an album's own tracks, the children of
    /// its item; `None` elsewhere.
    pub album_track_count: Option<u32>,
    /// The audio codec, `None` when the catalog does not say.
    pub codec: Option<AudioCodec>,
    /// Bits per sample (16, 24); `None` when not known, and for lossy
    /// codecs, which have none. Known for an album's own tracks and a
    /// track's own item, not in other listings or search.
    pub bit_depth: Option<u32>,
    /// Samples per second, in Hz (44_100, 96_000); known where
    /// `bit_depth` is.
    pub sample_rate: Option<u32>,
    #[serde(with = "duration_ms::option")]
    pub duration: Option<Duration>,
    pub playback: PlaybackState,
}

/// What a marked stretch of an item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkerKind {
    Intro,
    Credits,
    /// A kind this build does not know, from a newer server. Show nothing
    /// for it.
    #[serde(other)]
    Unknown,
}

/// A stretch of a movie or episode a viewer may want to skip (P2-10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    pub kind: MarkerKind,
    /// Where it starts, in milliseconds from the start of the item.
    pub start_ms: u64,
    /// Where it ends. On a marker that runs to the end this may be a
    /// little off the item's duration, either way: go by `to_end`.
    pub end_ms: u64,
    /// Nothing follows it: credits with no scene after them. The catalog
    /// says so; it is never worked out from `end_ms` and the duration.
    pub to_end: bool,
}

/// Any one item a catalog returns. On the wire it is the item's own object
/// with a `kind` field added, for example `{"kind": "movie", "id": …}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Item {
    Movie(Movie),
    Show(Show),
    Season(Season),
    Episode(Episode),
    Artist(Artist),
    Album(Album),
    Track(Track),
}

impl Item {
    pub fn id(&self) -> &MediaId {
        match self {
            Item::Movie(m) => &m.id,
            Item::Show(s) => &s.id,
            Item::Season(s) => &s.id,
            Item::Episode(e) => &e.id,
            Item::Artist(a) => &a.id,
            Item::Album(a) => &a.id,
            Item::Track(t) => &t.id,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Debug;

    use serde::de::DeserializeOwned;

    use super::*;
    use crate::id::{MediaKind, Namespace};

    fn round_trip<T: Serialize + DeserializeOwned + PartialEq + Debug>(value: T) {
        let json = serde_json::to_string(&value).unwrap();
        assert_eq!(serde_json::from_str::<T>(&json).unwrap(), value, "{json}");
    }

    fn media(kind: MediaKind, key: &str) -> MediaId {
        MediaId::new(Namespace::Plex, kind, key).unwrap()
    }

    fn poster() -> Vec<Artwork> {
        vec![Artwork {
            id: ArtworkId::new(Namespace::Plex, "/library/metadata/1/thumb/9").unwrap(),
            kind: ArtworkKind::Poster,
        }]
    }

    fn halfway() -> PlaybackState {
        PlaybackState {
            position: Some(Duration::from_millis(2_220_500)),
            watched: false,
        }
    }

    #[test]
    fn durations_are_whole_milliseconds_on_the_wire() {
        let json = serde_json::to_value(halfway()).unwrap();
        assert_eq!(json["position"], 2_220_500);
    }

    #[test]
    fn an_unstarted_item_has_a_null_position() {
        let json = serde_json::to_value(PlaybackState::default()).unwrap();
        assert!(json["position"].is_null());
    }

    #[test]
    fn a_track_s_format_is_plain_numbers_on_the_wire() {
        let track = Track {
            id: media(MediaKind::Track, "224630"),
            album_id: media(MediaKind::Album, "224629"),
            artist_id: media(MediaKind::Artist, "224303"),
            title: "Catch".into(),
            track: Some(1),
            disc: Some(1),
            album_track_count: Some(3),
            codec: Some(AudioCodec::Flac),
            bit_depth: Some(24),
            sample_rate: Some(96_000),
            duration: None,
            playback: PlaybackState::default(),
        };
        let json = serde_json::to_value(&track).unwrap();
        assert_eq!(
            (
                &json["codec"],
                &json["bit_depth"],
                &json["sample_rate"],
                &json["album_track_count"]
            ),
            (
                &serde_json::json!("flac"),
                &serde_json::json!(24),
                &serde_json::json!(96_000),
                &serde_json::json!(3)
            )
        );
    }

    #[test]
    fn library_kinds_are_snake_case_on_the_wire() {
        assert_eq!(
            serde_json::to_string(&LibraryKind::Movies).unwrap(),
            r#""movies""#
        );
    }

    #[test]
    fn a_marker_is_milliseconds_and_a_snake_case_kind_on_the_wire() {
        let marker = Marker {
            kind: MarkerKind::Credits,
            start_ms: 2_662_962,
            end_ms: 2_709_958,
            to_end: true,
        };
        assert_eq!(
            serde_json::to_value(marker).unwrap(),
            serde_json::json!({
                "kind": "credits",
                "start_ms": 2_662_962,
                "end_ms": 2_709_958,
                "to_end": true,
            })
        );
        round_trip(marker);
    }

    #[test]
    fn a_marker_kind_from_a_newer_server_is_unknown() {
        let marker: Marker = serde_json::from_str(
            r#"{"kind": "recap", "start_ms": 0, "end_ms": 30000, "to_end": false}"#,
        )
        .unwrap();
        assert_eq!(marker.kind, MarkerKind::Unknown);
    }

    #[test]
    fn an_item_carries_its_kind_on_the_wire() {
        let item = Item::Show(Show {
            id: media(MediaKind::Show, "20"),
            title: "The Expanse".into(),
            year: None,
            summary: None,
            content_rating: None,
            season_count: None,
            genres: Vec::new(),
            artwork: Vec::new(),
        });
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(json["kind"], "show");
        assert_eq!(json["id"], "plex:show:20");
        round_trip(item);
    }

    #[test]
    fn every_model_type_round_trips() {
        round_trip(Library {
            id: LibraryId::new(Namespace::Plex, "1").unwrap(),
            title: "Movies".into(),
            kind: LibraryKind::Movies,
        });
        round_trip(Movie {
            id: media(MediaKind::Movie, "10"),
            title: "Alien".into(),
            year: Some(1979),
            summary: Some("In space no one can hear you scream.".into()),
            content_rating: Some("R".into()),
            duration: Some(Duration::from_secs(7020)),
            artwork: poster(),
            playback: halfway(),
        });
        round_trip(Show {
            id: media(MediaKind::Show, "20"),
            title: "The Expanse".into(),
            year: Some(2015),
            summary: None,
            content_rating: Some("TV-14".into()),
            season_count: Some(6),
            genres: vec!["Science Fiction".into(), "Drama".into()],
            artwork: poster(),
        });
        round_trip(Season {
            id: media(MediaKind::Season, "21"),
            show_id: media(MediaKind::Show, "20"),
            show_title: Some("The Expanse".into()),
            season: 1,
            title: "Season 1".into(),
            artwork: Vec::new(),
        });
        round_trip(Episode {
            id: media(MediaKind::Episode, "22"),
            show_id: media(MediaKind::Show, "20"),
            show_title: Some("The Expanse".into()),
            season_id: media(MediaKind::Season, "21"),
            season: 1,
            episode: 1,
            title: "Dulcinea".into(),
            summary: None,
            content_rating: Some("TV-14".into()),
            duration: Some(Duration::from_secs(2640)),
            artwork: poster(),
            playback: PlaybackState::default(),
        });
        round_trip(Artist {
            id: media(MediaKind::Artist, "30"),
            name: "Boards of Canada".into(),
            summary: None,
            artwork: poster(),
        });
        round_trip(Album {
            id: media(MediaKind::Album, "31"),
            artist_id: media(MediaKind::Artist, "30"),
            artist_name: Some("Boards of Canada".into()),
            title: "Music Has the Right to Children".into(),
            year: Some(1998),
            track_count: Some(18),
            artwork: poster(),
        });
        round_trip(Track {
            id: media(MediaKind::Track, "32"),
            album_id: media(MediaKind::Album, "31"),
            artist_id: media(MediaKind::Artist, "30"),
            title: "Roygbiv".into(),
            track: Some(10),
            disc: Some(1),
            album_track_count: Some(18),
            codec: Some(AudioCodec::Flac),
            bit_depth: Some(16),
            sample_rate: Some(44_100),
            duration: Some(Duration::from_secs(151)),
            playback: PlaybackState {
                position: None,
                watched: true,
            },
        });
    }
}
