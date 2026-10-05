//! One metadata frame followed by exactly `length` raw bytes for every
//! binary resource. Playback revisions are independent of encoder epochs.
use crate::{ArtworkRequest, Error, MediaRead, PlaybackHandle, SessionId};
use serde::{Deserialize, Serialize};
use spoolframe_types::SubtitleFormat;

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
