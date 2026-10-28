//! Play history: what the user played lately, tracks included, and how
//! often.
//!
//! An item's details ([`Session::item`]) carry each item's
//! `PlaybackState::play_count` and `last_played_at`, its own and its
//! children's (an album's tracks); [`Session::recently_played`] lists the
//! user's items by their latest play.

use spoolframe_protocol::playlists::Kinds;
use spoolframe_protocol::{Reply, Request};

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
}
