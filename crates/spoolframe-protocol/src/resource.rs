//! One metadata frame followed by exactly `length` raw bytes for every
//! binary resource. Playback revisions are independent of encoder epochs.
use crate::{ArtworkFace, ArtworkRequest, Error, MediaRead, PlaybackHandle, SessionId};
use serde::{Deserialize, Serialize};
use spoolframe_types::{ArtworkId, SubtitleFormat};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResourceRequest {
    Artwork {
        request: ArtworkRequest,
        max_bytes: u64,
    },
    Media {
        request: MediaRead,
        revision: u64,
    },
    Subtitle {
        session: SessionId,
        handle: PlaybackHandle,
        revision: u64,
        offset: u64,
        version: Option<u64>,
    },
    SubtitleSegment {
        session: SessionId,
        handle: PlaybackHandle,
        revision: u64,
        segment: u32,
    },
    /// Upload a cover for playlist `id` (`spoolframe:playlist-art`):
    /// the only request whose stream goes on past it, with exactly
    /// `length` bytes of a JPEG, PNG, WebP or HEIC picture. Answered
    /// `ready { length: 0, playlist_art { artwork } }`, with no body.
    /// Refused before the picture is read when the user may not edit the
    /// playlist (`NotAllowed`) or `length` is past
    /// [`crate::playlists::MAX_ART_BYTES`] (`ImageTooLarge`).
    SetPlaylistArt {
        session: SessionId,
        id: String,
        length: u64,
    },
    /// Read download `id`'s file (`spoolframe:downloads`): at most
    /// `length` bytes from `offset`, and no more than [`crate::MAX_READ`].
    /// Any offset may be asked, so a download broken off resumes where it
    /// stopped. Answered `ready { length, download { size } }` with the
    /// bytes; `length` is short only at the file's end. `NotFound` for an
    /// id that is not the session user's; `DownloadNotReady` until its
    /// state is `ready`. Not a playback: it takes no playback slot.
    Download {
        session: SessionId,
        id: String,
        offset: u64,
        length: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResourceMetadata {
    Artwork {
        content_type: String,
        etag: String,
        /// Experimental (`smart-crop`), for a connection that named it:
        /// the faces in this backdrop, empty if it has none. Left out for
        /// a backdrop not scanned yet, and for any other image.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        faces: Option<Vec<ArtworkFace>>,
    },
    Media {
        size: u64,
        complete: bool,
        epoch: u32,
        revision: u64,
    },
    Subtitle {
        format: SubtitleFormat,
        total: u64,
        version: u64,
        revision: u64,
    },
    /// The cover `SetPlaylistArt` stored: the playlist's `artwork` now.
    PlaylistArt { artwork: ArtworkId },
    /// A download's file (`Download`): its whole length.
    Download { size: u64 },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResourceResponse {
    Ready {
        length: u64,
        metadata: ResourceMetadata,
    },
    Error {
        error: Error,
    },
}
