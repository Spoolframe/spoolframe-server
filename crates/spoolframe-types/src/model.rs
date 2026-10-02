//! The media model clients browse: libraries, and the movies, shows and
//! music in them (ARCHITECTURE §5).
//!
//! `playback` is filled in by the server for the session's own user; it
//! never comes from Plex (§36).

use std::time::Duration;

use serde::{Deserialize, Serialize};

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
    pub artwork: Vec<Artwork>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Season {
    pub id: MediaId,
    pub show_id: MediaId,
    /// The season number; 0 is specials.
    pub season: u32,
    pub title: String,
    pub artwork: Vec<Artwork>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Episode {
    pub id: MediaId,
    pub show_id: MediaId,
    pub season_id: MediaId,
    pub season: u32,
    pub episode: u32,
    pub title: String,
    pub summary: Option<String>,
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
    pub title: String,
    pub year: Option<u16>,
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
    #[serde(with = "duration_ms::option")]
    pub duration: Option<Duration>,
    pub playback: PlaybackState,
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
    fn library_kinds_are_snake_case_on_the_wire() {
        assert_eq!(
            serde_json::to_string(&LibraryKind::Movies).unwrap(),
            r#""movies""#
        );
    }

    #[test]
    fn an_item_carries_its_kind_on_the_wire() {
        let item = Item::Show(Show {
            id: media(MediaKind::Show, "20"),
            title: "The Expanse".into(),
            year: None,
            summary: None,
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
            duration: Some(Duration::from_secs(7020)),
            artwork: poster(),
            playback: halfway(),
        });
        round_trip(Show {
            id: media(MediaKind::Show, "20"),
            title: "The Expanse".into(),
            year: Some(2015),
            summary: None,
            artwork: poster(),
        });
        round_trip(Season {
            id: media(MediaKind::Season, "21"),
            show_id: media(MediaKind::Show, "20"),
            season: 1,
            title: "Season 1".into(),
            artwork: Vec::new(),
        });
        round_trip(Episode {
            id: media(MediaKind::Episode, "22"),
            show_id: media(MediaKind::Show, "20"),
            season_id: media(MediaKind::Season, "21"),
            season: 1,
            episode: 1,
            title: "Dulcinea".into(),
            summary: None,
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
            title: "Music Has the Right to Children".into(),
            year: Some(1998),
            artwork: poster(),
        });
        round_trip(Track {
            id: media(MediaKind::Track, "32"),
            album_id: media(MediaKind::Album, "31"),
            artist_id: media(MediaKind::Artist, "30"),
            title: "Roygbiv".into(),
            track: Some(10),
            disc: Some(1),
            duration: Some(Duration::from_secs(151)),
            playback: PlaybackState {
                position: None,
                watched: true,
            },
        });
    }
}
