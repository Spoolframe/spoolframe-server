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
            other => SdkError::Server(other.to_string()),
        }
    }
}
