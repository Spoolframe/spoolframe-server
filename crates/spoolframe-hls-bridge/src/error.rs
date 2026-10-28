//! What a source's read can fail with. The bridge turns each error into an
//! HTTP status for the player. A [`Source`](crate::Source) maps its own
//! errors onto this set (the `sdk` feature does it for SDK errors).

use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The encode was replaced under the same playback, at `epoch` (a
    /// seek, a track or quality change): read the segment again from 0.
    #[error("the playback restarted (epoch {epoch})")]
    Restarted { epoch: u32 },
    /// The playback was started again in another encode, at `at`: read the
    /// segment again from 0.
    #[error("the playback was started again at {} s", at.as_secs())]
    Resumed { at: Duration },
    /// Not ready yet, `done_percent` of it made (a subtitle being read):
    /// `503`, and the player asks again.
    #[error("not ready yet ({done_percent}% done)")]
    NotReady { done_percent: u8 },
    /// The server's admin stopped the playback: `410`, and
    /// [`Bridge::stopped`](crate::Bridge::stopped) says so.
    #[error("the playback was stopped on the server")]
    Stopped,
    /// Gone for good (closed, idle too long, ended for a newer
    /// connection's): `410`, and [`Bridge::expired`](crate::Bridge::expired)
    /// says so.
    #[error("the playback has expired")]
    Expired,
    /// The playback has no such resource: `404`.
    #[error("not found")]
    NotFound,
    /// Anything else: `502`.
    #[error("{0}")]
    Other(String),
}
