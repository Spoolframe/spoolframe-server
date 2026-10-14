//! What a source's read can fail with. The variants the bridge tells apart
//! are the client SDK's own (`SdkError`), so the moved code reads them as it
//! did; anything else is [`Error::Other`].

use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// Gone for good: `410`, and [`Bridge::expired`](crate::Bridge::expired)
    /// says so.
    #[error("the playback is closed")]
    PlaybackClosed,
    /// As [`Self::PlaybackClosed`].
    #[error("the playback has expired")]
    PlaybackExpired,
    /// The server's admin stopped it (P3-15): `410`, and
    /// [`Bridge::stopped`](crate::Bridge::stopped) says so.
    #[error("the playback was stopped on the server")]
    PlaybackStopped,
    /// The encode was replaced under the same playback (a seek, a track or
    /// quality change): read the segment again from 0.
    #[error("the playback restarted (epoch {epoch})")]
    PlaybackRestarted { epoch: u32 },
    /// The playback was started again in another encode, at `at`: read the
    /// segment again from 0.
    #[error("the playback was started again at {} s", at.as_secs())]
    PlaybackResumed { at: Duration },
    /// Not ready yet: `503`, and the player asks again.
    #[error("the subtitle is still being read ({done_percent}% done)")]
    SubtitleNotReady { done_percent: u8 },
    /// Rewritten while read: `503`, and the player asks again.
    #[error("the subtitle changed while it was read")]
    SubtitleChanged,
    #[error("{0}")]
    InvalidInput(String),
    #[error("server: {0}")]
    Server(String),
    #[error("protocol: {0}")]
    Protocol(String),
    /// Anything else: `502`.
    #[error("{0}")]
    Other(String),
}
