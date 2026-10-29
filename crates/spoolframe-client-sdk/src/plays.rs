//! Play history: what the user played lately, tracks included, and how
//! often.
//!
//! An item's details ([`Session::item`]) carry each item's
//! `PlaybackState::play_count` and `last_played_at`, its own and its
//! children's (an album's tracks); [`Session::recently_played`] lists the
//! user's items by their latest play.

use spoolframe_protocol::playlists::Kinds;
use spoolframe_protocol::{Reply, Request};
use spoolframe_types::{Album, Item};

use crate::{ItemsPage, SdkError, Session};

impl Session {
    /// What the user played lately, from any device of theirs: each item
    /// once, by its latest counted play, newest first; a page from
    /// `offset` of at most `limit` (500), and how many in all. `kinds`
    /// keeps films, episodes or tracks only (music's Recently played
    /// ticks `tracks`); `None`, every kind. Each item's playback state
    /// has its `play_count` and `last_played_at`.
    pub async fn recently_played(
        &self,
        kinds: Option<Kinds>,
        offset: u32,
        limit: u32,
    ) -> Result<ItemsPage, SdkError> {
        let request = Request::RecentlyPlayed {
            kinds,
            offset,
            limit,
        };
        match self.call(request).await? {
            Reply::Items {
                items,
                total,
                unavailable,
            } => Ok(ItemsPage {
                items,
                total,
                unavailable,
            }),
            other => Err(SdkError::Protocol(format!("unexpected reply {other:?}"))),
        }
    }

    /// The albums the user played lately, newest first, each once, at
    /// most `limit`: Home's "Recently played music". Found in the latest
    /// 100 tracks played, and made from their own album fields, so the
    /// row needs no read per album.
    pub async fn recently_played_albums(&self, limit: usize) -> Result<Vec<Album>, SdkError> {
        let tracks = Kinds {
            tracks: true,
            ..Kinds::default()
        };
        let played = self.recently_played(Some(tracks), 0, RECENT_TRACKS).await?;
        Ok(recent_albums(&played.items, limit))
    }
}

/// The tracks [`Session::recently_played_albums`] looks in.
const RECENT_TRACKS: u32 = 100;

/// The albums of `played`, tracks newest first: each album once, where
/// its latest-played track is, at most `limit`. A track that does not
/// name its album is left out: there is nothing to call the card.
fn recent_albums(played: &[Item], limit: usize) -> Vec<Album> {
    let mut albums: Vec<Album> = Vec::new();
    for item in played {
        if albums.len() == limit {
            break;
        }
        let Item::Track(track) = item else { continue };
        let Some(title) = &track.album_title else {
            continue;
        };
        if albums.iter().any(|a| a.id == track.album_id) {
            continue;
        }
        albums.push(Album {
            id: track.album_id.clone(),
            artist_id: track.artist_id.clone(),
            artist_name: track.artist_name.clone(),
            title: title.clone(),
            year: None,
            track_count: track.album_track_count,
            // The server gives a track its album's cover.
            artwork: track.artwork.clone(),
        });
    }
    albums
}

#[cfg(test)]
mod tests {
    use super::*;
    use spoolframe_types::{MediaId, MediaKind, Namespace, PlaybackState, Track};

    fn id(kind: MediaKind, key: &str) -> MediaId {
        MediaId::new(Namespace::Plex, kind, key).unwrap()
    }

    fn track(key: &str, album: &str, album_title: Option<&str>) -> Item {
        Item::Track(Track {
            id: id(MediaKind::Track, key),
            album_id: id(MediaKind::Album, album),
            artist_id: id(MediaKind::Artist, "1"),
            artist_name: Some("The Night Shift".into()),
            album_title: album_title.map(str::to_owned),
            title: format!("Track {key}"),
            track: None,
            disc: None,
            album_track_count: Some(9),
            codec: None,
            bit_depth: None,
            sample_rate: None,
            duration: None,
            artwork: Vec::new(),
            playback: PlaybackState::default(),
        })
    }

    fn titles(albums: &[Album]) -> Vec<&str> {
        albums.iter().map(|a| a.title.as_str()).collect()
    }

    #[test]
    fn each_album_is_listed_once_where_its_latest_track_is() {
        let played = [
            track("1", "a", Some("Low Tide")),
            track("2", "b", Some("Glasshouse")),
            track("3", "a", Some("Low Tide")),
        ];
        assert_eq!(
            titles(&recent_albums(&played, 10)),
            ["Low Tide", "Glasshouse"]
        );
    }

    #[test]
    fn no_more_albums_than_asked_for_are_listed() {
        let played = [
            track("1", "a", Some("Low Tide")),
            track("2", "b", Some("Glasshouse")),
            track("3", "c", Some("North Line")),
        ];
        assert_eq!(
            titles(&recent_albums(&played, 2)),
            ["Low Tide", "Glasshouse"]
        );
    }

    #[test]
    fn a_track_that_names_no_album_is_left_out() {
        let played = [track("1", "a", None), track("2", "b", Some("Glasshouse"))];
        assert_eq!(titles(&recent_albums(&played, 10)), ["Glasshouse"]);
    }

    #[test]
    fn the_album_is_the_one_the_track_is_on() {
        let albums = recent_albums(&[track("1", "a", Some("Low Tide"))], 10);
        assert_eq!(
            (albums[0].id.clone(), albums[0].artist_name.as_deref()),
            (id(MediaKind::Album, "a"), Some("The Night Shift"))
        );
    }
}
