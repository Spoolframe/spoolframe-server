//! Playback plans (ARCHITECTURE §6, §8): how the server would play an item
//! for this client, given what it can decode and which tracks it picked.

use serde::{Deserialize, Serialize};
use spoolframe_types::{AudioCodec, Container, Delivery, SubtitleFormat, VideoCodec};

/// Of a link's measured rate, the share a playback is planned for
/// (P6-06): the rest is room for the link's swings and for the requests,
/// artwork and progress reports that share it. The server plans for it,
/// and the SDK asks for a new plan when a file needs more than it.
pub const LINK_SHARE: f64 = 0.75;

/// The tracks a client wants, as positions in the item's `MediaTracks`
/// lists. Only these count: a track the user did not pick never blocks
/// playback.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    /// `None`: the file's default audio track.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub audio: Option<u32>,
    /// `None`: no subtitles.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub subtitle: Option<u32>,
}

/// How a picked bitmap subtitle (PGS) is shown (P2-07): drawn by the
/// client, read as text by the server, or burned into the video. Text
/// subtitles are not affected; they are never burned in. Pictures are
/// burned in only when the client cannot draw them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BitmapSubtitles {
    /// As the viewer's preference says (`TrackPreferences`'
    /// `subtitles.bitmap`): read as text if the server can, else drawn by
    /// the client if it can, else burned in; or, for pictures, drawn by
    /// the client if it can, else burned in. Apps send this unless the
    /// user overrides it for one playback. The plan says which was used.
    #[default]
    Auto,
    /// Read as text and sent as a text subtitle, even to a client that
    /// could draw the pictures. The plan is `Unsupported` if the server
    /// cannot read this track.
    Text,
    /// Pictures: drawn by the client if it can, else burned into the
    /// video. The server plans it as the `pictures` preference; it no
    /// longer forces a burn-in on a client that can draw the pictures.
    /// Kept so the wire does not change.
    BurnIn,
    /// A mode this build does not know: treated as `Auto`.
    #[serde(other)]
    Unknown,
}

/// The tracks the server chose when the client sent no [`Selection`]
/// (P1-20), and why, so the client can show it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChosenTracks {
    pub selection: Selection,
    pub audio: TrackReason,
    pub subtitle: TrackReason,
}

/// Why a track was chosen, or why none was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackReason {
    /// What the viewer picked for this series or movie.
    Override,
    /// The viewer's preferences.
    Preferences,
    /// No preferred language is in the file: its default track.
    FileDefault,
    /// The preferences want none here (subtitles off, or the audio is
    /// already in a preferred subtitle language).
    NotWanted,
    /// Wanted, but no track fits (e.g. only SDH, and SDH is avoided).
    NoneMatched,
    /// The file has no tracks of this kind.
    NoTracks,
    /// A reason this build does not know.
    #[serde(other)]
    Unknown,
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
    /// The server converts the file for this client (P2): copies the
    /// streams that fit, encodes the ones that do not, and delivers the
    /// result as `delivery` says. A remux is a plan whose streams are all
    /// `Copy`.
    Converted {
        delivery: Delivery,
        /// The container of the output.
        container: Container,
        video: Option<ConvertedVideo>,
        audio: Option<ConvertedAudio>,
        subtitle: Option<ConvertedSubtitle>,
        /// Why the file is not played as it is, one per stream that is
        /// converted, e.g. "subtitle 0: pgs cannot be shown; burned into
        /// the video". A client can offer to avoid them (subtitles off).
        reasons: Vec<String>,
    },
    /// The client cannot play this selection. Each reason names a stream
    /// and why, e.g. "audio 0: dts not supported".
    Unsupported { reasons: Vec<String> },
    /// A plan kind this build does not know.
    #[serde(other)]
    Unknown,
}

/// What happens to the video in a converted plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConvertedVideo {
    /// Into the output as it is.
    Copy { codec: VideoCodec },
    Encode {
        codec: VideoCodec,
        /// Lines in the output.
        height: u32,
        /// The most it may use, bits per second; `None` for the encoder's
        /// own choice.
        max_bitrate: Option<u64>,
        /// HDR mapped to SDR.
        tone_map: bool,
    },
    /// A kind this build does not know.
    #[serde(other)]
    Unknown,
}

/// What happens to the selected audio track in a converted plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConvertedAudio {
    pub index: u32,
    pub action: AudioAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AudioAction {
    Copy {
        codec: AudioCodec,
    },
    Encode {
        codec: AudioCodec,
        channels: u32,
        /// Bits per second.
        bitrate: u64,
    },
    #[serde(other)]
    Unknown,
}

/// What happens to the selected subtitle in a converted plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConvertedSubtitle {
    pub index: u32,
    pub action: SubtitleAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SubtitleAction {
    /// Into the output as it is.
    Copy { format: SubtitleFormat },
    /// Converted to another text format, e.g. WebVTT for HLS.
    Convert {
        from: SubtitleFormat,
        to: SubtitleFormat,
    },
    /// Picture subtitles drawn into the video: the client cannot draw them
    /// in this delivery, and they are not read as text. Forces a video
    /// encode. The server never burns in a text subtitle: one the client
    /// cannot show is left out of the plan, with a reason.
    BurnIn { format: SubtitleFormat },
    /// A bitmap subtitle read as text by the server (P2-07). It is not in
    /// the output: the client fetches it whole with `ReadSubtitle`, which
    /// answers `SubtitleNotReady` until the reading is done.
    Ocr {
        from: SubtitleFormat,
        to: SubtitleFormat,
    },
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
    /// A separate file the client fetches whole (P1-08). A bitmap track
    /// planned as a text format is one the server reads as text (P2-07):
    /// `ReadSubtitle` answers `SubtitleNotReady` until it is done.
    External,
    #[serde(other)]
    Unknown,
}
