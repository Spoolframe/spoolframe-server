//! Downloads for offline viewing (PX-38, feature `spoolframe:downloads`):
//! the messages of [`crate::Request::DownloadOptions`],
//! [`crate::Request::PrepareDownload`], [`crate::Request::DownloadStatus`],
//! [`crate::Request::CancelDownload`] and
//! [`crate::Request::SyncOfflineWatch`], the
//! [`crate::Event::DownloadChanged`] event, and the
//! [`crate::resource::ResourceRequest::Download`] read.
//!
//! A download is a film or an episode written as one file for the
//! device: the source file as it is (with every track, when the device
//! plays it so), a remux of the tracks chosen, or a remux with what the
//! device cannot play converted. Apple devices (iOS, iPadOS, macOS) get
//! MP4, Linux ones Matroska; tvOS and the web have none. Tracks are named
//! by their place in the item's `Tracks`: `audio[i]`, `subtitles[i]`.

use serde::{Deserialize, Serialize};
use spoolframe_types::{Container, MediaId, MediaTracks};

/// The most ids one `DownloadStatus` asks about, and the most entries one
/// `SyncOfflineWatch` carries.
pub const MAX_ASKED: usize = 500;

/// What a download is made from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DownloadSource {
    /// Original quality: the file, converted only where the device needs
    /// it, at its resolution and with its HDR.
    Original,
    /// The copy in an optimized profile (PX-37), by name, as
    /// `DownloadOptions::profiles` lists them.
    Profile { name: String },
    /// A source this build does not know, from a newer client: the server
    /// answers `InvalidInput`.
    #[serde(other)]
    Unknown,
}

/// Which tracks a download carries. The video is always there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DownloadTracks {
    /// Every track of the file: the only choice that may send the file
    /// as it is, byte for byte.
    All,
    /// These, by their place in `Tracks`' `audio` and `subtitles`. Order
    /// and repeats do not count.
    Chosen {
        audio: Vec<u32>,
        subtitles: Vec<u32>,
    },
    /// A choice this build does not know: the server answers
    /// `InvalidInput`.
    #[serde(other)]
    Unknown,
}

impl DownloadTracks {
    /// The same choice, each list in order and without repeats: two
    /// requests that differ only so are one.
    pub fn normalized(&self) -> Self {
        match self {
            Self::Chosen { audio, subtitles } => {
                let sorted = |v: &[u32]| {
                    let mut v = v.to_vec();
                    v.sort_unstable();
                    v.dedup();
                    v
                };
                Self::Chosen {
                    audio: sorted(audio),
                    subtitles: sorted(subtitles),
                }
            }
            other => other.clone(),
        }
    }
}

/// What a download in original quality with every track would be
/// (`DownloadOptions::original`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OriginalDownload {
    /// The file itself, byte for byte: the device plays it as it is.
    AsIs,
    /// A remux, with nothing converted: another container, or subtitles
    /// beside the file to carry.
    Remux,
    /// What the device cannot play is converted first (the server keeps
    /// the conversion for the next download): the video when `video`,
    /// and the audio tracks in `audio`, by place. A choice of tracks
    /// that leaves out every one of them, and keeps the video, is a
    /// remux.
    Convert { video: bool, audio: Vec<u32> },
    /// A kind this build does not know.
    #[serde(other)]
    Unknown,
}

/// A subtitle a download would leave out, and why (`DownloadOptions`):
/// a picture subtitle in an Apple file when the viewer prefers pictures
/// (MP4 carries none), or one the server cannot write to a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeftOutSubtitle {
    /// Its place in `Tracks`' `subtitles`.
    pub subtitle: u32,
    /// Said to the user as it is.
    pub reason: String,
}

/// What the device may download of an item (`DownloadOptions`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadOptionsInfo {
    /// The item's tracks, as `Tracks` gives them: what `Chosen` names by
    /// place.
    pub tracks: MediaTracks,
    /// The choice to show first: one or two audio tracks in the viewer's
    /// languages, and every text subtitle in their languages, with the
    /// subtitle playback would pick. Always `Chosen`.
    pub preferred: DownloadTracks,
    /// The optimized profiles a download may be made in, in the order
    /// the admin set.
    pub profiles: Vec<String>,
    /// What original quality with every track would be. For a choice of
    /// tracks: `convert` when the video is converted or a chosen audio
    /// track is among `audio`, else `remux`.
    pub original: OriginalDownload,
    /// The subtitles this device's download would leave out, of all the
    /// item's, with why: leave them unticked.
    pub left_out: Vec<LeftOutSubtitle>,
}

/// Where a download is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DownloadState {
    /// Waiting its turn: for its conversion, for room on the server, or
    /// for a place to write it.
    Waiting,
    /// What the device cannot play is being converted, or picture
    /// subtitles read as text: `done_percent` of it, and about
    /// `eta_secs` to go, once known.
    Converting {
        done_percent: Option<u8>,
        eta_secs: Option<u64>,
    },
    /// The file is being written: `done_percent` of it, once known.
    Writing { done_percent: Option<u8> },
    /// The file is whole: `size` bytes, with this SHA-256 (64 hex
    /// digits). Read it with `ResourceRequest::Download`.
    Ready { size: u64, sha256: String },
    /// It could not be made: a conversion failed, the file is gone, or
    /// the server removed it a day after its last read. `PrepareDownload`
    /// again tries again, under the same id.
    Failed { reason: String },
    /// A state this build does not know: ask `DownloadStatus` later.
    #[serde(other)]
    Unknown,
}

/// The tracks a download's file carries, by their place in `Tracks`, in
/// the file's order after its video.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileTracks {
    pub audio: Vec<u32>,
    pub subtitles: Vec<u32>,
}

/// What a download's file is: for a player, and for labelling its tracks
/// offline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadFile {
    pub container: Container,
    pub tracks: FileTracks,
}

/// A download, as `PrepareDownload` and `DownloadStatus` answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadInfo {
    /// The download's id: the session user's, for this item, source and
    /// tracks; the same id each time they prepare the same.
    pub id: String,
    pub state: DownloadState,
    /// What the file is; `None` where the server does not know it.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub file: Option<DownloadFile>,
}

/// Where a device got to in an item while it had no server
/// (`SyncOfflineWatch`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfflineWatch {
    /// A film or an episode.
    pub id: MediaId,
    /// Where it stopped; `None` when only `watched` changed.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub position_ms: Option<u64>,
    /// Marked watched (`true`) or unwatched (`false`) on the device;
    /// `None` when only the position changed. With a position, the
    /// server judges watched by its own rules, and `true` makes it
    /// watched whatever they say; `false` alone starts it over.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub watched: Option<bool>,
    /// When, by the device's clock, in Unix seconds.
    pub at: i64,
}

/// What became of one [`OfflineWatch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OfflineWatchOutcome {
    /// Written: it was newer than what the server had.
    Applied,
    /// Not written: the server has a newer state for the item (from
    /// another device, say). Show the server's.
    Older,
    /// Not written: the item has no watch state (not a film or an
    /// episode), or is not on the server any more.
    NotTracked,
    /// An outcome this build does not know.
    #[serde(other)]
    Unknown,
}
