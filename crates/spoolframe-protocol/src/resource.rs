//! One metadata frame followed by exactly `length` raw bytes for every
//! binary resource. Playback revisions are independent of encoder epochs.
use crate::{ArtworkRequest, Error, MediaRead, PlaybackHandle, SessionId};
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
    /// Upload a cover for playlist `id` (PX-53, `spoolframe:playlist-art`):
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResourceMetadata {
    Artwork {
        content_type: String,
        etag: String,
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
    PlaylistArt {
        artwork: ArtworkId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
