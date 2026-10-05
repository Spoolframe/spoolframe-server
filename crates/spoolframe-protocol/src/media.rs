//! Playing media: playback sessions and range reads (ARCHITECTURE §34,
//! P1-08).
//!
//! `StartPlayback` (an RPC) plans the item and returns a
//! [`PlaybackHandle`]. The client then reads the file in ranges, each on its
//! own [`crate::streams::bi::RESOURCE`] stream, wrapped with the playback
//! revision in [`crate::resource::ResourceRequest`]. These media structs
//! describe the request and SDK result; the resource header announces the
//! raw body length. Several reads may be in flight at once. End of file is
//! `offset + sent == size`; a read at or past the end has `sent = 0`.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::messages::{Error, SessionId};

/// The largest range one read returns. A reply shorter than asked is
/// normal: the client asks again from where it stopped.
pub const MAX_READ: u64 = 16 << 20;

/// A playback session on the server: one planned file, bound to the
/// session and endpoint that started it. Opaque to the client.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlaybackHandle(pub String);

impl fmt::Debug for PlaybackHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Not a credential on its own (it is bound to a session and an
        // endpoint), but there is no reason to log it.
        f.write_str("PlaybackHandle(<redacted>)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaRead {
    pub session: SessionId,
    pub handle: PlaybackHandle,
    pub offset: u64,
    /// Capped at [`MAX_READ`].
    pub length: u64,
    /// The encode this read is for (P2-04): byte offsets mean nothing
    /// across a restart. 0 for direct play and a playback's first encode.
    #[serde(default)]
    pub epoch: u32,
    /// An HLS segment to read in, 0 being the init segment (P2-05); `None`
    /// reads the file itself (direct play) or the progressive output.
    #[serde(default)]
    pub segment: Option<u32>,
}

/// What follows on the stream. New fields (P2 adds some) arrive with
/// defaults, so older clients keep working.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaReadReply {
    /// How many bytes follow this frame.
    pub sent: u64,
    /// The size of the whole file, or of what an encode has written so
    /// far (see `complete`).
    pub size: u64,
    /// `size` is final: always for direct play, once the encode has ended
    /// for a converted playback. The end is `complete` and `offset + sent
    /// == size`. A reply from a server before P2-04 means direct play,
    /// hence the default.
    #[serde(default = "complete")]
    pub complete: bool,
    /// The encode the bytes are from.
    #[serde(default)]
    pub epoch: u32,
}

fn complete() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaReadResponse {
    Ok(MediaReadReply),
    Err(Error),
}
