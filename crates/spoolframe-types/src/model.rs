//! The media model clients browse: libraries, and the movies, shows and
//! music in them (ARCHITECTURE §5).
//!
//! `playback` is filled in by the server for the session's own user; it
//! never comes from Plex (§36).

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::codec::AudioCodec;
use crate::date::CalendarDate;
use crate::duration_ms;
use crate::id::{ArtworkId, LibraryId, MediaId, PersonId};
use crate::search::PersonRole;

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
    /// The paired server a library is shared from (experimental
    /// `federation`, P8-02): only on a library of another server's, and
    /// only to a client that named the feature. `None` for the server's
    /// own libraries.
    // compat(v1): required at the protocol reset
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<LibraryServer>,
}

/// The server a library of another server's is shared from (P8-02), so a
/// client groups libraries by server, an offline one last and greyed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LibraryServer {
    /// Its server id: the origin a `fed:` id names.
    pub server_id: String,
    /// What it calls itself.
    pub name: String,
    /// Whether this server is connected to it now. Offline, what it shared
    /// is still browsed from the cache, and its items do not play.
    pub online: bool,
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
    /// When the user last viewed it, Unix seconds: their last progress
    /// report on it from any device, or its last mark as watched or
    /// unwatched. Sent only to a client that listed the
    /// `spoolframe:last-viewed` feature, and only where its reply says
    /// so (Home's rows, an item's details); `None` elsewhere, and for an
    /// item never viewed.
    // compat(v1): required at the protocol reset
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_viewed_at: Option<i64>,
    /// How many times the user played it through: their counted plays
    /// from any device (PX-52). Sent only to a client that listed the
    /// `spoolframe:play-history` feature, and only where its reply says
    /// so (an item's details, Recently played); left out when 0.
    // compat(v1): required at the protocol reset
    #[serde(default, skip_serializing_if = "is_zero")]
    pub play_count: u32,
    /// When the user last played it, Unix seconds: the end of their
    /// latest counted play. Sent where `play_count` is; `None` for an
    /// item never played.
    // compat(v1): required at the protocol reset
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_played_at: Option<i64>,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Movie {
    pub id: MediaId,
    pub title: String,
    /// The year it came out: the catalog's own, else `release_date`'s.
    pub year: Option<u16>,
    /// The day it came out, `"YYYY-MM-DD"`.
    ///
    /// This and `cast` were added without a PROTOCOL_VERSION bump, as
    /// `Episode::air_date` was: a server from before them leaves the keys
    /// out, and that reads as `None` and no one.
    // compat(v1): required at the protocol reset
    pub release_date: Option<CalendarDate>,
    pub summary: Option<String>,
    /// The age rating as the catalog has it ("R", "TV-MA"), shown as
    /// is; `None` when it has none.
    pub content_rating: Option<String>,
    #[serde(with = "duration_ms::option")]
    pub duration: Option<Duration>,
    /// Who is in it and who directed it (see [`CastMember`]).
    // compat(v1): required at the protocol reset
    #[serde(default)]
    pub cast: Vec<CastMember>,
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
    /// The day its latest episode in the library first aired, for the
    /// end of "2015–2018". Only the show's own item has it; `None` in
    /// listings and search, and where no episode has a date.
    ///
    /// A catalog that does not know whether a show has ended (Plex does
    /// not) still sets it on a running show: a client that shows a run
    /// from it may show one that has not ended.
    ///
    /// This and `cast` were added without a PROTOCOL_VERSION bump, so a
    /// server from before them leaves the keys out, and that reads as
    /// `None` and no one.
    // compat(v1): required at the protocol reset
    pub last_aired: Option<CalendarDate>,
    /// Who is in it and who directed it (see [`CastMember`]).
    // compat(v1): required at the protocol reset
    #[serde(default)]
    pub cast: Vec<CastMember>,
    pub artwork: Vec<Artwork>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Season {
    pub id: MediaId,
    pub show_id: MediaId,
    /// The show's title, so a season shown alone needs no second request.
    #[serde(deserialize_with = "crate::required::option")]
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
    #[serde(deserialize_with = "crate::required::option")]
    pub show_title: Option<String>,
    pub season_id: MediaId,
    pub season: u32,
    pub episode: u32,
    pub title: String,
    pub summary: Option<String>,
    /// The episode's own age rating, as on a movie; a client that has
    /// none here may show the show's.
    pub content_rating: Option<String>,
    /// The year it first aired, for "2019" on its page: the catalog's
    /// own, else `air_date`'s.
    ///
    /// This and `air_date` were added without a PROTOCOL_VERSION bump,
    /// so they are not `required::option`: a server from before them
    /// leaves the keys out, and that reads as `None`.
    // compat(v1): required at the protocol reset
    pub year: Option<u16>,
    /// The day it first aired, `"YYYY-MM-DD"`.
    // compat(v1): required at the protocol reset
    pub air_date: Option<CalendarDate>,
    #[serde(with = "duration_ms::option")]
    pub duration: Option<Duration>,
    /// Who is in it and who directed it (see [`CastMember`]): the
    /// episode's own, which may leave out the show's regulars. Added
    /// without a PROTOCOL_VERSION bump; left out, no one.
    // compat(v1): required at the protocol reset
    #[serde(default)]
    pub cast: Vec<CastMember>,
    pub artwork: Vec<Artwork>,
    pub playback: PlaybackState,
}

/// Someone in a movie's, show's or episode's credits: an actor, with the
/// part they play, or a director.
///
/// An item's `cast` is in the catalog's billing order, actors first, then
/// directors. Only the item's own read (`Item`) has it; it is empty in
/// listings, search and the home rows, where Plex gives names without
/// the ids a person is listed by. A native catalog fills it with images
/// and people (P9-05); until then only Plex does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CastMember {
    /// The id a person search gives them: their items are listed with
    /// `PersonItems`, as a found person's are.
    pub id: PersonId,
    pub name: String,
    pub role: PersonRole,
    /// The part an actor plays ("Ellen Ripley"), as the catalog has it;
    /// `None` for a director, and for an actor where it does not say.
    #[serde(deserialize_with = "crate::required::option")]
    pub character: Option<String>,
    /// Their portrait, fetched through the client protocol as any
    /// artwork is; `None` where the catalog has none.
    #[serde(deserialize_with = "crate::required::option")]
    pub photo: Option<ArtworkId>,
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
    #[serde(deserialize_with = "crate::required::option")]
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
    /// Who it is by, so a track shown alone (a search result, a queue)
    /// says whose it is: the track's own artist where its tags name one
    /// ("Adele with Chris Stapleton"), else its album's.
    pub artist_name: Option<String>,
    /// Its album's title, for the same reason.
    pub album_title: Option<String>,
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
    /// Its album's cover as the poster, and its artist's background.
    pub artwork: Vec<Artwork>,
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
            last_viewed_at: None,
            play_count: 0,
            last_played_at: None,
        }
    }

    fn ripley() -> CastMember {
        CastMember {
            id: PersonId::new(Namespace::Plex, "actor-13").unwrap(),
            name: "Sigourney Weaver".into(),
            role: PersonRole::Actor,
            character: Some("Ellen Ripley".into()),
            photo: Some(ArtworkId::new(Namespace::Plex, "/people/1/a/b.jpg").unwrap()),
        }
    }

    fn scott() -> CastMember {
        CastMember {
            id: PersonId::new(Namespace::Plex, "director-7").unwrap(),
            name: "Ridley Scott".into(),
            role: PersonRole::Director,
            character: None,
            photo: None,
        }
    }

    #[test]
    fn a_cast_member_is_their_person_id_role_and_photo_on_the_wire() {
        assert_eq!(
            serde_json::to_value(ripley()).unwrap(),
            serde_json::json!({
                "id": "plex:actor-13",
                "name": "Sigourney Weaver",
                "role": "actor",
                "character": "Ellen Ripley",
                "photo": "plex:/people/1/a/b.jpg",
            })
        );
        round_trip(ripley());
        round_trip(scott());
    }

    /// A new type, so every field is sent (PX-15): `null` for none.
    #[test]
    fn a_cast_member_s_character_and_photo_are_required() {
        crate::test_support::assert_each_required(&scott(), &["character", "photo"]);
    }

    /// `cast`, `release_date` and `last_aired` came without a revision
    /// bump: a server from before them leaves them out.
    #[test]
    fn items_from_a_server_without_cast_or_dates_still_decode() {
        let movie = Movie {
            id: media(MediaKind::Movie, "10"),
            title: "Alien".into(),
            year: Some(1979),
            release_date: CalendarDate::new(1979, 5, 25),
            summary: None,
            content_rating: None,
            duration: None,
            cast: vec![ripley()],
            artwork: Vec::new(),
            playback: PlaybackState::default(),
        };
        let mut json = serde_json::to_value(&movie).unwrap();
        assert_eq!(json["release_date"], "1979-05-25");
        let object = json.as_object_mut().unwrap();
        object.remove("release_date");
        object.remove("cast");
        let older: Movie = serde_json::from_value(json).unwrap();
        assert_eq!((older.release_date, older.cast), (None, Vec::new()));

        let show = Show {
            id: media(MediaKind::Show, "20"),
            title: "The Expanse".into(),
            year: Some(2015),
            summary: None,
            content_rating: None,
            season_count: None,
            genres: Vec::new(),
            last_aired: CalendarDate::new(2022, 4, 8),
            cast: vec![ripley()],
            artwork: Vec::new(),
        };
        let mut json = serde_json::to_value(&show).unwrap();
        assert_eq!(json["last_aired"], "2022-04-08");
        let object = json.as_object_mut().unwrap();
        object.remove("last_aired");
        object.remove("cast");
        let older: Show = serde_json::from_value(json).unwrap();
        assert_eq!((older.last_aired, older.cast), (None, Vec::new()));

        let episode = Episode {
            id: media(MediaKind::Episode, "22"),
            show_id: media(MediaKind::Show, "20"),
            show_title: None,
            season_id: media(MediaKind::Season, "21"),
            season: 1,
            episode: 1,
            title: "Dulcinea".into(),
            summary: None,
            content_rating: None,
            year: None,
            air_date: None,
            duration: None,
            cast: vec![scott()],
            artwork: Vec::new(),
            playback: PlaybackState::default(),
        };
        let mut json = serde_json::to_value(&episode).unwrap();
        json.as_object_mut().unwrap().remove("cast");
        let older: Episode = serde_json::from_value(json).unwrap();
        assert_eq!(older.cast, Vec::new());
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
            artist_name: None,
            album_title: None,
            title: "Catch".into(),
            track: Some(1),
            disc: Some(1),
            album_track_count: Some(3),
            codec: Some(AudioCodec::Flac),
            bit_depth: Some(24),
            sample_rate: Some(96_000),
            duration: None,
            artwork: Vec::new(),
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
            last_aired: None,
            cast: Vec::new(),
            artwork: Vec::new(),
        });
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(json["kind"], "show");
        assert_eq!(json["id"], "plex:show:20");
        round_trip(item);
    }

    /// The parent's name on a season, an episode and an album was once
    /// `serde(default)`; every field is now required (PX-15).
    #[test]
    fn a_parent_s_name_left_out_is_refused() {
        let season = Season {
            id: media(MediaKind::Season, "21"),
            show_id: media(MediaKind::Show, "20"),
            show_title: None,
            season: 1,
            title: "Season 1".into(),
            artwork: Vec::new(),
        };
        crate::test_support::assert_each_required(&season, &["show_title"]);
        let episode = Episode {
            id: media(MediaKind::Episode, "22"),
            show_id: media(MediaKind::Show, "20"),
            show_title: None,
            season_id: media(MediaKind::Season, "21"),
            season: 1,
            episode: 1,
            title: "Dulcinea".into(),
            summary: None,
            content_rating: None,
            year: None,
            air_date: None,
            duration: None,
            cast: Vec::new(),
            artwork: Vec::new(),
            playback: PlaybackState::default(),
        };
        crate::test_support::assert_each_required(&episode, &["show_title"]);
        let album = Album {
            id: media(MediaKind::Album, "30"),
            artist_id: media(MediaKind::Artist, "31"),
            artist_name: None,
            title: "Moon Safari".into(),
            year: None,
            track_count: None,
            artwork: Vec::new(),
        };
        crate::test_support::assert_each_required(&album, &["artist_name"]);
        // `null` is how none is sent.
        round_trip(album);
    }

    #[test]
    fn an_episode_from_a_server_without_air_dates_still_decodes() {
        let episode = Episode {
            id: media(MediaKind::Episode, "22"),
            show_id: media(MediaKind::Show, "20"),
            show_title: None,
            season_id: media(MediaKind::Season, "21"),
            season: 1,
            episode: 1,
            title: "Dulcinea".into(),
            summary: None,
            content_rating: None,
            year: Some(2015),
            air_date: CalendarDate::new(2015, 11, 23),
            duration: None,
            cast: Vec::new(),
            artwork: Vec::new(),
            playback: PlaybackState::default(),
        };
        let mut json = serde_json::to_value(&episode).unwrap();
        assert_eq!(json["air_date"], "2015-11-23");
        let object = json.as_object_mut().unwrap();
        object.remove("year");
        object.remove("air_date");
        let older: Episode = serde_json::from_value(json).unwrap();
        assert_eq!((older.year, older.air_date), (None, None));
    }

    #[test]
    fn every_model_type_round_trips() {
        round_trip(Library {
            id: LibraryId::new(Namespace::Plex, "1").unwrap(),
            title: "Movies".into(),
            kind: LibraryKind::Movies,
            server: None,
        });
        round_trip(Movie {
            id: media(MediaKind::Movie, "10"),
            title: "Alien".into(),
            year: Some(1979),
            release_date: CalendarDate::new(1979, 5, 25),
            summary: Some("In space no one can hear you scream.".into()),
            content_rating: Some("R".into()),
            duration: Some(Duration::from_secs(7020)),
            cast: vec![ripley(), scott()],
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
            last_aired: CalendarDate::new(2022, 4, 8),
            cast: vec![ripley()],
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
            year: Some(2015),
            air_date: CalendarDate::new(2015, 11, 23),
            duration: Some(Duration::from_secs(2640)),
            cast: vec![scott()],
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
            artist_name: Some("Boards of Canada".into()),
            album_title: Some("Music Has the Right to Children".into()),
            title: "Roygbiv".into(),
            track: Some(10),
            disc: Some(1),
            album_track_count: Some(18),
            codec: Some(AudioCodec::Flac),
            bit_depth: Some(16),
            sample_rate: Some(44_100),
            duration: Some(Duration::from_secs(151)),
            artwork: poster(),
            playback: PlaybackState {
                position: None,
                watched: true,
                last_viewed_at: None,
                play_count: 0,
                last_played_at: None,
            },
        });
    }
}
