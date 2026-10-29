//! Subtitle sync and offset: an external text subtitle (SRT, WebVTT, ASS
//! beside the film) synced to the file, or moved by a manual offset, for
//! everyone who plays the item.
//!
//! Each such track's `SubtitleTrack::timing` (from [`Session::tracks`])
//! says how it is timed. [`Session::sync_subtitle`] starts a sync in the
//! background, [`Session::use_original_subtitle_timing`] sets it aside,
//! and [`Session::set_subtitle_offset`] moves the track (a player's ±
//! control, in 0.1 s steps). What the server serves already has the
//! timing: a player never adds the offset itself. When a track's timing
//! changes, by anyone, a device that asked for the item's tracks or plays
//! it is told ([`crate::Client::subtitle_timing_events`]): a player
//! showing the track reads it again ([`Session::read_subtitle`]), with
//! no restart.

use spoolframe_protocol::{Reply, Request};
use spoolframe_types::{MediaId, SubtitleTiming};

use crate::{SdkError, Session};

/// A subtitle track's timing changed, as a server told.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct SubtitleTimingEvent {
    /// The server's key.
    pub server: String,
    pub id: MediaId,
    /// The track's place in the item's `subtitles`.
    pub track: u32,
    pub timing: SubtitleTiming,
}

impl Session {
    async fn timing_call(&self, request: Request) -> Result<SubtitleTiming, SdkError> {
        match self.call(request).await? {
            Reply::SubtitleTiming(timing) => Ok(timing),
            other => Err(SdkError::Protocol(format!("unexpected reply {other:?}"))),
        }
    }

    /// Sync item `id`'s subtitle `track` to the file, in the background:
    /// its timing now, `Syncing`. The outcome comes as an event, or in
    /// the item's tracks asked again. A track that is not an external
    /// text subtitle is [`SdkError::Server`] (`InvalidInput`).
    pub async fn sync_subtitle(
        &self,
        id: &MediaId,
        track: u32,
    ) -> Result<SubtitleTiming, SdkError> {
        self.timing_call(Request::SyncSubtitle {
            id: id.clone(),
            track,
        })
        .await
    }

    /// Serve item `id`'s subtitle `track` with its original timing again,
    /// for everyone; its offset stays.
    pub async fn use_original_subtitle_timing(
        &self,
        id: &MediaId,
        track: u32,
    ) -> Result<SubtitleTiming, SdkError> {
        self.timing_call(Request::UseOriginalSubtitleTiming {
            id: id.clone(),
            track,
        })
        .await
    }

    /// Move item `id`'s subtitle `track` by `offset_ms` (positive: later),
    /// for everyone; 0 resets it. At most ten minutes either way.
    pub async fn set_subtitle_offset(
        &self,
        id: &MediaId,
        track: u32,
        offset_ms: i64,
    ) -> Result<SubtitleTiming, SdkError> {
        self.timing_call(Request::SetSubtitleOffset {
            id: id.clone(),
            track,
            offset_ms,
        })
        .await
    }
}
