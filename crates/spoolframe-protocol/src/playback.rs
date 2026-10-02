//! Playback plans (ARCHITECTURE §6, §8): how the server would play an item
//! for this client, given what it can decode and which tracks it picked.

use serde::{Deserialize, Serialize};
use spoolframe_types::{AudioCodec, Container, SubtitleFormat, VideoCodec};

/// The tracks a client wants, as positions in the item's `MediaTracks`
/// lists. Only these count: a track the user did not pick never blocks
/// playback.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    /// `None`: the file's default audio track.
    #[serde(default)]
    pub audio: Option<u32>,
    /// `None`: no subtitles.
    #[serde(default)]
    pub subtitle: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PlaybackPlan {
    /// The client plays the file as it is.
    Direct {
        container: Container,
        video: Option<VideoCodec>,
        audio: Option<PlannedAudio>,
        subtitle: Option<PlannedSubtitle>,
    },
    /// The streams are fine but the container is not: the server rewraps
    /// them. Phase 2; Phase 1 never plans it.
    Remux { container: Container },
    /// Something must be re-encoded; the targets. Phase 2; Phase 1 never
    /// plans it.
    Transcode {
        video: Option<VideoCodec>,
        audio: Option<AudioCodec>,
    },
    /// The client cannot play this selection. Each reason names a stream
    /// and why, e.g. "audio 0: dts not supported".
    Unsupported { reasons: Vec<String> },
    /// A plan kind this build does not know.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedAudio {
    pub index: u32,
    pub codec: AudioCodec,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedSubtitle {
    pub index: u32,
    pub format: SubtitleFormat,
    pub delivery: SubtitleDelivery,
}

/// Where the client gets a subtitle from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubtitleDelivery {
    /// A stream inside the media file it plays.
    Embedded,
    /// A separate file the client fetches whole (P1-08).
    External,
    #[serde(other)]
    Unknown,
}
