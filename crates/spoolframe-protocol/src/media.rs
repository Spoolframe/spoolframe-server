//! Playing media: playback sessions and range reads (ARCHITECTURE §34,
//! P1-08).
//!
//! `StartPlayback` (an RPC) plans the item and returns a
//! [`PlaybackHandle`]. The client then reads the file in ranges, each on its
//! own `streams::bi::MEDIA` stream: one [`MediaRead`] frame out, one
//! [`MediaReadResponse`] frame back and, if it is `ok`, exactly `sent` raw
//! bytes. Several reads may be in flight at once. End of file is
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
}

/// What follows on the stream. New fields (P2 adds some) arrive with
/// defaults, so older clients keep working.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaReadReply {
    /// How many bytes follow this frame.
    pub sent: u64,
    /// The size of the whole file.
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaReadResponse {
    Ok(MediaReadReply),
    Err(Error),
}
