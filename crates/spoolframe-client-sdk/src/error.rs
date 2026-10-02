//! What can go wrong, as an app needs to tell it apart.

use std::time::Duration;

use spoolframe_protocol::Error as ServerError;

use crate::storage::StorageError;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SdkError {
    /// The app's storage failed. Show it; the SDK never works around it.
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// The server cannot be reached right now.
    #[error("{0} cannot be reached")]
    Unreachable(String),
    /// Not saved, not heard on the LAN and no address given.
    #[error("no server {0} is known")]
    UnknownServer(String),
    /// A server must be pinned ([`crate::Client::pin`]) before anything but
    /// `Hello` is sent to it (P1-21).
    #[error("server {0} is not saved yet; pin it first")]
    NotPinned(String),
    /// Saved server `saved` seems to answer with another key, `now` (P1-13):
    /// it was reinstalled, or this is a different server. Show both
    /// fingerprints; [`crate::Client::trust_new_identity`] if the user says so.
    #[error("server {saved} answered with another key, {now}")]
    IdentityChanged { saved: String, now: String },
    /// The endpoint answered as another server than the one dialled.
    #[error("expected server {expected}, but {actual} answered")]
    WrongServer { expected: String, actual: String },
    /// The session is gone (revoked, expired, user disabled) and was
    /// forgotten; the user has to sign in again.
    #[error("sign in again")]
    SignInRequired,
    #[error("the username or password is wrong")]
    InvalidCredentials,
    #[error("too many failed sign-ins; try again in {} s", retry_after.as_secs())]
    RateLimited { retry_after: Duration },
    /// One reason per stream that this device cannot play.
    #[error("cannot play: {}", reasons.join("; "))]
    CannotPlay { reasons: Vec<String> },
    /// Start the playback again and go on from where it was.
    #[error("the playback has expired")]
    PlaybackExpired,
    #[error("{0} not found")]
    NotFound(String),
    /// The server is converting as many files as it may (P2-03); try
    /// again later, or pick tracks that play without converting.
    #[error("the server is busy converting other files")]
    Busy,
    /// The conversion failed, with ffmpeg's reason.
    #[error("the conversion failed: {0}")]
    PlaybackFailed(String),
    /// A seek restarted the conversion: read `epoch` from offset 0.
    #[error("the playback restarted (epoch {epoch})")]
    PlaybackRestarted { epoch: u32 },
    /// This profile is locked on this device (P2-08): ask for its PIN and
    /// [`crate::Session::unlock`]. The profile's [`crate::Session::user`]
    /// from [`crate::Client::profiles`] says `locked` before anything is
    /// shown.
    #[error("this profile is locked; enter its PIN")]
    ProfileLocked,
    /// A value the call carried is not acceptable (a PIN that is not 4 to 8
    /// digits); the message says why.
    #[error("{0}")]
    InvalidInput(String),
    /// The subtitle is still being read as text (P2-07): ask again in a
    /// few seconds.
    #[error("the subtitle is still being read ({done_percent}% done)")]
    SubtitleNotReady { done_percent: u8 },
    /// Any other error the server answered with.
    #[error("server: {0}")]
    Server(String),
    /// The server's answer made no sense to this build.
    #[error("protocol: {0}")]
    Protocol(String),
}

impl SdkError {
    /// The error for a server's answer. `Unauthenticated` is
    /// [`SdkError::SignInRequired`]; the caller forgets the session.
    pub(crate) fn from_server(error: ServerError) -> Self {
        match error {
            ServerError::Unauthenticated => SdkError::SignInRequired,
            ServerError::InvalidCredentials => SdkError::InvalidCredentials,
            ServerError::RateLimited { retry_after_ms } => SdkError::RateLimited {
                retry_after: Duration::from_millis(retry_after_ms),
            },
            ServerError::CannotPlay { reasons } => SdkError::CannotPlay { reasons },
            ServerError::PlaybackExpired => SdkError::PlaybackExpired,
            ServerError::NotFound { what } => SdkError::NotFound(what),
            ServerError::Busy => SdkError::Busy,
            ServerError::PlaybackFailed { reason } => SdkError::PlaybackFailed(reason),
            ServerError::PlaybackRestarted { epoch } => SdkError::PlaybackRestarted { epoch },
            ServerError::ProfileLocked => SdkError::ProfileLocked,
            ServerError::InvalidInput { message } => SdkError::InvalidInput(message),
            ServerError::SubtitleNotReady { done_percent } => {
                SdkError::SubtitleNotReady { done_percent }
            }
            other => SdkError::Server(other.to_string()),
        }
    }
}
