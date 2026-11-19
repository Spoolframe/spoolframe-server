//! Home's "Recently played" row (feature `spoolframe:recently-played-row`):
//! one row of every kind, each card once at its latest counted play.
//!
//! A client says where it started a track ([`PlaySource`]) as it starts
//! the playback (`StartPlayback.source`) or records a play made offline
//! (`OfflinePlay.source`). The field needs no feature: a server that does
//! not know it ignores it. The server keeps it with the play, and
//! `RecentlyPlayedRow` answers cards ([`RecentCard`]) by it and by the
//! user's [`RecentlyPlayedPreferences`]: for music, a play from a
//! playlist shows the playlist, one from an album the album, and a track
//! queued by hand the track; shows by show, season or episode; films each
//! their own. The preferences only change what the row shows: every play
//! is recorded and counted whatever they say.

use serde::{Deserialize, Serialize};
use spoolframe_types::{Album, Episode, MediaId, Movie, Season, Show, Track};
pub use spoolframe_types::{
    RecentMoviesMode, RecentMusicMode, RecentShowsMode, RecentlyPlayedPreferences,
};

use crate::playlists::PlaylistSummary;

/// The most cards a `RecentlyPlayedRow` answers.
pub const MAX_ROW: u32 = 50;

/// Where a track's play was started.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlaySource {
    /// Playing album `id`, in order or shuffled.
    Album { id: MediaId },
    /// Playing playlist `id` (Liked and All included).
    Playlist { id: String },
    /// Added to the queue by hand ("Play next", "Add to queue").
    Queued,
    /// A source this build does not know: kept as no source.
    #[serde(other)]
    Unknown,
}

/// One card of Home's "Recently played" row. A film, an episode or a
/// track has its watch state and play count filled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
// Few cards a row; boxed, the apps' pattern would change for nothing.
#[allow(clippy::large_enum_variant)]
pub enum RecentCard {
    Movie {
        movie: Movie,
    },
    /// A show, at its latest episode played.
    Show {
        show: Show,
    },
    /// A season, at its latest episode played.
    Season {
        season: Season,
    },
    Episode {
        episode: Episode,
    },
    Album {
        album: Album,
    },
    Playlist {
        playlist: PlaylistSummary,
    },
    Track {
        track: Track,
    },
    /// A card this build does not know: leave it out.
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode, encode};
    use spoolframe_types::{MediaKind, Namespace};

    fn round_trip<T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug>(t: T) {
        assert_eq!(decode::<T>(&encode(&t).unwrap()).unwrap(), t);
    }

    /// The names are promised to the apps.
    #[test]
    fn the_wire_names_are_fixed() {
        let album = MediaId::new(Namespace::Plex, MediaKind::Album, "7").unwrap();
        let cases = [
            (
                PlaySource::Album { id: album },
                serde_json::json!({"kind": "album", "id": "plex:album:7"}),
            ),
            (
                PlaySource::Playlist { id: "p1".into() },
                serde_json::json!({"kind": "playlist", "id": "p1"}),
            ),
            (PlaySource::Queued, serde_json::json!({"kind": "queued"})),
        ];
        for (source, json) in cases {
            assert_eq!(serde_json::to_value(&source).unwrap(), json);
            round_trip(source);
        }
        let preferences = RecentlyPlayedPreferences {
            music: RecentMusicMode::Albums,
            shows: RecentShowsMode::Season,
            movies: RecentMoviesMode::Hidden,
        };
        assert_eq!(
            serde_json::to_value(preferences).unwrap(),
            serde_json::json!({"music": "albums", "shows": "season", "movies": "hidden"})
        );
        round_trip(preferences);
    }

    #[test]
    fn the_defaults_are_by_source_by_episode_and_each_film() {
        assert_eq!(
            RecentlyPlayedPreferences::default(),
            RecentlyPlayedPreferences {
                music: RecentMusicMode::Sources,
                shows: RecentShowsMode::Episode,
                movies: RecentMoviesMode::Movie,
            }
        );
    }

    /// What a newer peer adds reads as unknown, through CBOR too.
    #[test]
    fn what_a_newer_peer_adds_reads_as_unknown() {
        let source = serde_json::json!({"kind": "radio", "station": "x"});
        assert_eq!(
            decode::<PlaySource>(&encode(&source).unwrap()).unwrap(),
            PlaySource::Unknown
        );
        assert_eq!(
            serde_json::from_value::<RecentMusicMode>(serde_json::json!("shuffled")).unwrap(),
            RecentMusicMode::Unknown
        );
        let card = serde_json::json!({"type": "station", "name": "x"});
        assert_eq!(
            decode::<RecentCard>(&encode(&card).unwrap()).unwrap(),
            RecentCard::Unknown
        );
    }
}
