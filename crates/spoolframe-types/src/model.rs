//! The media model clients browse: libraries, and the movies, shows and
//! music in them.
//!
//! `playback` is filled in by the server for the session's own user; it
//! never comes from Plex.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::codec::AudioCodec;
use crate::date::CalendarDate;
use crate::duration_ms;
use crate::id::{ArtworkId, LibraryId, MediaId, PersonId};
use crate::search::PersonRole;

/// What a library holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum LibraryKind {
    Movies,
    Shows,
    Music,
}

/// A library of one kind of media.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct Library {
    pub id: LibraryId,
    pub title: String,
    pub kind: LibraryKind,
    /// The paired server a library is shared from (experimental
    /// `federation`): only on a library of another server's, and
    /// only to a client that named the feature. `None` for the server's
    /// own libraries, and left out then, as experimental fields are.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<LibraryServer>,
}

/// The server a library of another server's is shared from, so a
/// client groups libraries by server, an offline one last and greyed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum ArtworkKind {
    Poster,
    Background,
    Thumb,
}

/// A reference to artwork. Clients fetch the image through the client
/// protocol with the `id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct Artwork {
    pub id: ArtworkId,
    pub kind: ArtworkKind,
}

/// The session user's progress on an item.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct PlaybackState {
    /// Where playback stopped, or `None` if it never started.
    #[serde(with = "duration_ms::option")]
    pub position: Option<Duration>,
    pub watched: bool,
    /// When the user last viewed it, Unix seconds: their last progress
    /// report on it from any device, or its last mark as watched or
    /// unwatched. Filled where its reply says so (Home's rows, an item's
    /// details); `None` elsewhere, and for an item never viewed.
    ///
    /// This and the two below are read as none when left out: the SDK
    /// keeps an item with its download, and a download kept by an older
    /// SDK lacks the keys.
    #[serde(default)]
    pub last_viewed_at: Option<i64>,
    /// How many times the user played it through: their counted plays
    /// from any device. Filled where its reply says so (an
    /// item's details, Recently played); 0 elsewhere.
    #[serde(default)]
    pub play_count: u32,
    /// When the user last played it, Unix seconds: the end of their
    /// latest counted play. Filled where `play_count` is; `None` for an
    /// item never played.
    #[serde(default)]
    pub last_played_at: Option<i64>,
}

impl PlaybackState {
    /// Where a Play goes on from, as `PlaybackOptions::start` takes it:
    /// `None`, the start, once watched or never begun; else where it
    /// stopped.
    pub fn resume_from(&self) -> Option<Duration> {
        if self.watched {
            return None;
        }
        self.position.filter(|p| !p.is_zero())
    }

    /// How far through an item of `duration` the user is, for a
    /// progress bar and "20 min left".
    pub fn progress(&self, duration: Option<Duration>) -> Progress {
        let position = self.position.unwrap_or_default();
        let duration = duration.filter(|d| !d.is_zero());
        Progress {
            position,
            remaining: duration.map(|d| d.saturating_sub(position)),
            fraction: duration.map(|d| (position.as_secs_f64() / d.as_secs_f64()).clamp(0.0, 1.0)),
            watched: self.watched,
        }
    }
}

/// How far through an item the user is ([`PlaybackState::progress`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Progress {
    /// Where playback stopped; zero if it never started.
    pub position: Duration,
    /// What is left to play; `None` with no length known.
    pub remaining: Option<Duration>,
    /// `position` over the length, from 0 to 1; `None` with no length
    /// known.
    pub fraction: Option<f64>,
    pub watched: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct Movie {
    pub id: MediaId,
    pub title: String,
    /// The year it came out: the catalog's own, else `release_date`'s.
    pub year: Option<u16>,
    /// The day it came out, `"YYYY-MM-DD"`.
    ///
    /// This and `cast` are read as `None` and no one when left out, though
    /// every server sends them: the SDK keeps an item with its download,
    /// and a download kept by an older SDK lacks the keys.
    pub release_date: Option<CalendarDate>,
    pub summary: Option<String>,
    /// The age rating as the catalog has it ("R", "TV-MA"), shown as
    /// is; `None` when it has none.
    pub content_rating: Option<String>,
    #[serde(with = "duration_ms::option")]
    pub duration: Option<Duration>,
    /// Who is in it and who directed it (see [`CastMember`]).
    #[serde(default)]
    pub cast: Vec<CastMember>,
    pub artwork: Vec<Artwork>,
    pub playback: PlaybackState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
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
    /// This and `cast` are read as `None` and no one when left out, as
    /// [`Movie::release_date`] is.
    pub last_aired: Option<CalendarDate>,
    /// Who is in it and who directed it (see [`CastMember`]).
    #[serde(default)]
    pub cast: Vec<CastMember>,
    pub artwork: Vec<Artwork>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
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
    /// This and `air_date` are not `required::option`, as
    /// [`Movie::release_date`] is not.
    pub year: Option<u16>,
    /// The day it first aired, `"YYYY-MM-DD"`.
    pub air_date: Option<CalendarDate>,
    #[serde(with = "duration_ms::option")]
    pub duration: Option<Duration>,
    /// Who is in it and who directed it (see [`CastMember`]): the
    /// episode's own, which may leave out the show's regulars. Left out,
    /// no one, as [`Movie::cast`].
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
/// the ids a person is listed by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct Artist {
    pub id: MediaId,
    pub name: String,
    pub summary: Option<String>,
    pub artwork: Vec<Artwork>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum MarkerKind {
    Intro,
    Credits,
    /// A kind this build does not know, from a newer server. Show nothing
    /// for it.
    #[serde(other)]
    Unknown,
}

/// A stretch of a movie or episode a viewer may want to skip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
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

/// The years a show ran, as its card says them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum YearsAired {
    /// "2019": it ran in one year, or only one is known.
    One(u16),
    /// "2019–": an episode aired in the last twelve months.
    Running(u16),
    /// "2019–2023".
    Between(u16, u16),
}

/// "2019", "2019–", "2019–2023".
impl std::fmt::Display for YearsAired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            YearsAired::One(year) => write!(f, "{year}"),
            YearsAired::Running(first) => write!(f, "{first}\u{2013}"),
            YearsAired::Between(first, last) => write!(f, "{first}\u{2013}{last}"),
        }
    }
}

impl Show {
    /// The years it ran, from its first year and the day its latest
    /// episode aired: one year when that was the same year, running when
    /// it aired in the twelve months up to `today`, else both years. With
    /// no first year, the latest episode's year alone; `None` with
    /// neither.
    pub fn year_range(&self, today: CalendarDate) -> Option<YearsAired> {
        let Some(last) = self.last_aired else {
            return self.year.map(YearsAired::One);
        };
        let Some(first) = self.year else {
            return Some(YearsAired::One(last.year()));
        };
        if last.year() <= first {
            return Some(YearsAired::One(first));
        }
        // A year back from today, compared field by field: most years have
        // no 29 February to land on.
        let year_ago = (today.year() - 1, today.month(), today.day());
        if (last.year(), last.month(), last.day()) >= year_ago {
            Some(YearsAired::Running(first))
        } else {
            Some(YearsAired::Between(first, last.year()))
        }
    }

    /// [`Show::year_range`] as text: "2019", "2019–", "2019–2023".
    pub fn years_label(&self, today: CalendarDate) -> Option<String> {
        self.year_range(today).map(|years| years.to_string())
    }
}

/// The line under a card's title, in parts.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum Caption {
    /// A film's year.
    Year(u16),
    /// A show's years.
    Years(YearsAired),
    /// A season's show.
    Show(String),
    /// An episode: its show, and where it is in it ("S2 · E5").
    Episode {
        show: Option<String>,
        season: u32,
        episode: u32,
    },
    /// An album's artist and year.
    Album {
        artist: Option<String>,
        year: Option<u16>,
    },
    /// A track's length.
    Length(Duration),
}

impl Item {
    /// What a card says under its title on `today`; `None` for an artist
    /// or where nothing is known.
    pub fn caption_parts(&self, today: CalendarDate) -> Option<Caption> {
        match self {
            Item::Movie(m) => m.year.map(Caption::Year),
            Item::Show(s) => s.year_range(today).map(Caption::Years),
            Item::Season(s) => s.show_title.clone().map(Caption::Show),
            Item::Episode(e) => Some(Caption::Episode {
                show: e.show_title.clone(),
                season: e.season,
                episode: e.episode,
            }),
            Item::Album(a) => {
                (a.artist_name.is_some() || a.year.is_some()).then(|| Caption::Album {
                    artist: a.artist_name.clone(),
                    year: a.year,
                })
            }
            Item::Track(t) => t.duration.map(Caption::Length),
            Item::Artist(_) => None,
        }
    }

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

    /// Its own name: a film's, an episode's or a track's title; an
    /// artist's name.
    pub fn title(&self) -> &str {
        match self {
            Item::Movie(m) => &m.title,
            Item::Show(s) => &s.title,
            Item::Season(s) => &s.title,
            Item::Episode(e) => &e.title,
            Item::Artist(a) => &a.name,
            Item::Album(a) => &a.title,
            Item::Track(t) => &t.title,
        }
    }

    /// The user's progress on a film, an episode or a track; `None` for
    /// the kinds that do not play themselves.
    pub fn playback(&self) -> Option<&PlaybackState> {
        match self {
            Item::Movie(m) => Some(&m.playback),
            Item::Episode(e) => Some(&e.playback),
            Item::Track(t) => Some(&t.playback),
            _ => None,
        }
    }

    /// Its length, for the kinds that play.
    pub fn duration(&self) -> Option<Duration> {
        match self {
            Item::Movie(m) => m.duration,
            Item::Episode(e) => e.duration,
            Item::Track(t) => t.duration,
            _ => None,
        }
    }

    /// The user watched it (or played it to its end).
    pub fn is_watched(&self) -> bool {
        self.playback().is_some_and(|p| p.watched)
    }

    /// Where the user stopped in it, watched or not.
    pub fn resume_position(&self) -> Option<Duration> {
        self.playback().and_then(|p| p.position)
    }

    /// Where a Play goes on from ([`PlaybackState::resume_from`]).
    pub fn resume_from(&self) -> Option<Duration> {
        self.playback().and_then(PlaybackState::resume_from)
    }

    /// How far through it the user is ([`PlaybackState::progress`]);
    /// `None` for the kinds that do not play themselves.
    pub fn progress(&self) -> Option<Progress> {
        Some(self.playback()?.progress(self.duration()))
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

    /// "Today" for a show's years: 14 October 2026.
    fn years(year: Option<u16>, last_aired: Option<(u16, u8, u8)>) -> Option<String> {
        let show = Show {
            id: media(MediaKind::Show, "20"),
            title: "Andor".into(),
            year,
            summary: None,
            content_rating: None,
            season_count: None,
            genres: Vec::new(),
            last_aired: last_aired.and_then(|(y, m, d)| CalendarDate::new(y, m, d)),
            cast: Vec::new(),
            artwork: Vec::new(),
        };
        show.years_label(CalendarDate::new(2026, 10, 14).unwrap())
    }

    #[test]
    fn a_show_s_years_come_in_parts() {
        let show = |last: (u16, u8, u8)| Show {
            id: media(MediaKind::Show, "20"),
            title: "Andor".into(),
            year: Some(2022),
            summary: None,
            content_rating: None,
            season_count: None,
            genres: Vec::new(),
            last_aired: CalendarDate::new(last.0, last.1, last.2),
            cast: Vec::new(),
            artwork: Vec::new(),
        };
        let today = CalendarDate::new(2026, 10, 14).unwrap();
        assert_eq!(
            show((2025, 10, 14)).year_range(today),
            Some(YearsAired::Running(2022))
        );
        assert_eq!(
            show((2025, 10, 13)).year_range(today),
            Some(YearsAired::Between(2022, 2025))
        );
        assert_eq!(
            Item::Show(show((2022, 5, 1))).caption_parts(today),
            Some(Caption::Years(YearsAired::One(2022)))
        );
    }

    #[test]
    fn a_show_with_no_air_date_shows_its_first_year() {
        assert_eq!(years(Some(2022), None), Some("2022".into()));
        assert_eq!(years(None, None), None);
    }

    #[test]
    fn a_show_last_aired_in_its_first_year_shows_that_year() {
        assert_eq!(years(Some(2022), Some((2022, 12, 1))), Some("2022".into()));
    }

    #[test]
    fn a_show_aired_in_the_last_twelve_months_is_an_open_range() {
        assert_eq!(
            years(Some(2022), Some((2025, 10, 14))),
            Some("2022\u{2013}".into())
        );
    }

    #[test]
    fn a_show_last_aired_over_a_year_ago_shows_both_years() {
        assert_eq!(
            years(Some(2022), Some((2025, 10, 13))),
            Some("2022\u{2013}2025".into())
        );
    }

    #[test]
    fn a_show_with_no_first_year_shows_when_it_last_aired() {
        assert_eq!(years(None, Some((2019, 3, 2))), Some("2019".into()));
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

    fn alien(playback: PlaybackState, duration: Option<Duration>) -> Item {
        Item::Movie(Movie {
            id: media(MediaKind::Movie, "10"),
            title: "Alien".into(),
            year: Some(1979),
            release_date: None,
            summary: None,
            content_rating: None,
            duration,
            cast: Vec::new(),
            artwork: Vec::new(),
            playback,
        })
    }

    fn at(secs: u64, watched: bool) -> PlaybackState {
        PlaybackState {
            position: Some(Duration::from_secs(secs)),
            watched,
            ..PlaybackState::default()
        }
    }

    #[test]
    fn a_film_stopped_part_way_resumes_where_it_stopped() {
        assert_eq!(at(600, false).resume_from(), Some(Duration::from_secs(600)));
    }

    #[test]
    fn a_watched_film_plays_from_the_start() {
        assert_eq!(at(600, true).resume_from(), None);
    }

    #[test]
    fn a_film_never_begun_plays_from_the_start() {
        assert_eq!(PlaybackState::default().resume_from(), None);
        assert_eq!(at(0, false).resume_from(), None);
    }

    #[test]
    fn progress_is_the_fraction_watched_and_the_time_left() {
        let progress = at(1_500, false).progress(Some(Duration::from_secs(6_000)));
        assert_eq!(
            progress,
            Progress {
                position: Duration::from_secs(1_500),
                remaining: Some(Duration::from_secs(4_500)),
                fraction: Some(0.25),
                watched: false,
            }
        );
    }

    #[test]
    fn progress_past_the_length_is_whole_with_nothing_left() {
        let progress = at(7_000, true).progress(Some(Duration::from_secs(6_000)));
        assert_eq!(
            (progress.fraction, progress.remaining, progress.watched),
            (Some(1.0), Some(Duration::ZERO), true)
        );
    }

    #[test]
    fn progress_with_no_length_has_no_fraction() {
        for length in [None, Some(Duration::ZERO)] {
            let progress = at(60, false).progress(length);
            assert_eq!((progress.fraction, progress.remaining), (None, None));
        }
    }

    #[test]
    fn an_item_answers_from_its_own_playback() {
        let item = alien(at(600, false), Some(Duration::from_secs(1_200)));
        assert_eq!(item.title(), "Alien");
        assert!(!item.is_watched());
        assert_eq!(item.resume_position(), Some(Duration::from_secs(600)));
        assert_eq!(item.resume_from(), Some(Duration::from_secs(600)));
        assert_eq!(item.progress().and_then(|p| p.fraction), Some(0.5));
    }

    #[test]
    fn a_watched_item_keeps_its_position_but_resumes_from_the_start() {
        let item = alien(at(600, true), None);
        assert!(item.is_watched());
        assert_eq!(item.resume_position(), Some(Duration::from_secs(600)));
        assert_eq!(item.resume_from(), None);
    }

    #[test]
    fn an_artist_has_a_title_but_no_progress() {
        let show = Item::Artist(Artist {
            id: media(MediaKind::Artist, "30"),
            name: "Daft Punk".into(),
            summary: None,
            artwork: Vec::new(),
        });
        assert_eq!(show.title(), "Daft Punk");
        assert_eq!(show.progress(), None);
        assert!(!show.is_watched());
        assert_eq!(show.resume_from(), None);
    }
}
