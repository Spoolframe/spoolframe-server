//! What can go wrong, as an app needs to tell it apart.

use std::time::Duration;

use spoolframe_protocol::Error as ServerError;

use crate::storage::StorageError;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SdkError {
    #[error("the operation belongs to an earlier server instance")]
    ServerRestarted,
    #[error("the operation expired; its outcome is no longer recoverable")]
    OperationExpired,
    #[error("the operation ID was already used with different arguments")]
    OperationConflict,
    #[error("the playback is closed")]
    PlaybackClosed,
    #[error("the media changed while recovering playback")]
    PlaybackChanged,
    #[error("{0}")]
    Unavailable(String),
    #[error("an older progress report was rejected")]
    StaleProgress,
    #[error("playback revision changed to {revision}")]
    RevisionConflict { revision: u64 },
    /// The app's storage failed. Show it; the SDK never works around it.
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// The server cannot be reached right now.
    #[error("{0} cannot be reached")]
    Unreachable(String),
    /// The server did not answer in time ([`crate::Timeouts`]). Whether a
    /// request that changes something was done is not known: show the
    /// state again before offering to repeat it. A request that only
    /// reads has been tried again, once, on a new connection where the
    /// old one had died. Either way the next call works if the server is
    /// back: a dead connection has been dropped, and the next call dials.
    #[error("no answer in time: {0}")]
    TimedOut(String),
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
    /// The server's admin stopped the playback (P3-15): it is over, and
    /// [`crate::ManagedPlayback`] does not start it again. Say it ended.
    #[error("the playback was stopped on the server")]
    PlaybackStopped,
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
    /// The call carried the user's account password and they have none:
    /// they sign in with a passkey or SSO only
    /// ([`crate::PinProof::Password`]). Nothing was counted as a failed
    /// sign-in. Offer another proof, or send them to the server's web UI
    /// ([`crate::Session::web_url`], when the server has one).
    #[error("this account has no password; use the PIN or the web UI")]
    NoPassword,
    /// The subtitle is still being read as text (P2-07): ask again in a
    /// few seconds.
    #[error("the subtitle is still being read ({done_percent}% done)")]
    SubtitleNotReady { done_percent: u8 },
    /// The item's frame previews are being made (P3-13): show the time
    /// alone, and ask again in a few seconds.
    #[error("the previews are being made ({done_percent}% done)")]
    PreviewsNotReady { done_percent: u8 },
    /// The item has no frame previews, and will not have: show the time
    /// alone, and do not ask again for it.
    #[error("this item has no previews")]
    NoPreviews,
    /// The subtitle was written again or replaced on the server while it
    /// was read, and again while it was read a second time (P2-11). No
    /// part of it is returned: read it again.
    #[error("the subtitle changed while it was read")]
    SubtitleChanged,
    /// The server has no library yet (P3-10): it is to be set up in its
    /// web UI. Show "This server is not set up yet" where the library
    /// would be; signing in works.
    #[error("this server is not set up yet")]
    NotSetUp,
    /// The session signed in too long ago for this: approving a device
    /// (P3-07), adding or removing a passkey (P3-19), no longer asking for
    /// the PIN. Confirm it is the user ([`crate::Session::confirm_password`]
    /// or [`crate::Session::begin_confirm_passkey`]), or sign in again,
    /// then ask again.
    #[error("sign in again first")]
    RecentSignInRequired,
    /// The passkey is the user's last way to sign in, and is kept
    /// ([`crate::Session::remove_passkey`]): add another passkey, or a
    /// password in the web UI, first.
    #[error("that is the last way to sign in to this account")]
    LastSignInMethod,
    /// The device sign-in was refused on the approving device (P3-07).
    #[error("the sign-in was denied")]
    SignInDenied,
    /// The device sign-in is over (it timed out): begin another.
    #[error("the sign-in code has expired")]
    SignInExpired,
    /// Any other error the server answered with.
    #[error("server: {0}")]
    Server(String),
    /// The server's answer made no sense to this build.
    #[error("protocol: {0}")]
    Protocol(String),
}

impl SdkError {
    /// The connection failed under a request: it was not answered in
    /// time, or its stream broke. The connection may be dead.
    pub(crate) fn is_transport(&self) -> bool {
        matches!(self, SdkError::TimedOut(_) | SdkError::Unreachable(_))
    }

    /// The error for a server's answer. `Unauthenticated` is
    /// [`SdkError::SignInRequired`]; the caller forgets the session.
    pub(crate) fn from_server(error: ServerError) -> Self {
        match error {
            ServerError::ServerRestarted => Self::ServerRestarted,
            ServerError::OperationExpired => Self::OperationExpired,
            ServerError::OperationConflict => Self::OperationConflict,
            ServerError::Unavailable { message } => SdkError::Unavailable(message),
            ServerError::StaleProgress => SdkError::StaleProgress,
            ServerError::RevisionConflict { revision } => SdkError::RevisionConflict { revision },
            ServerError::Unauthenticated => SdkError::SignInRequired,
            ServerError::InvalidCredentials => SdkError::InvalidCredentials,
            ServerError::RateLimited { retry_after_ms } => SdkError::RateLimited {
                retry_after: Duration::from_millis(retry_after_ms),
            },
            ServerError::CannotPlay { reasons } => SdkError::CannotPlay { reasons },
            ServerError::PlaybackExpired => SdkError::PlaybackExpired,
            ServerError::PlaybackStopped => SdkError::PlaybackStopped,
            ServerError::NotFound { what } => SdkError::NotFound(what),
            ServerError::Busy => SdkError::Busy,
            ServerError::PlaybackFailed { reason } => SdkError::PlaybackFailed(reason),
            ServerError::PlaybackRestarted { epoch } => SdkError::PlaybackRestarted { epoch },
            ServerError::ProfileLocked => SdkError::ProfileLocked,
            ServerError::InvalidInput { message } => SdkError::InvalidInput(message),
            ServerError::NoPassword => SdkError::NoPassword,
            ServerError::SubtitleNotReady { done_percent } => {
                SdkError::SubtitleNotReady { done_percent }
            }
            ServerError::SubtitleChanged => SdkError::SubtitleChanged,
            ServerError::PreviewsNotReady { done_percent } => {
                SdkError::PreviewsNotReady { done_percent }
            }
            ServerError::NoPreviews => SdkError::NoPreviews,
            ServerError::DeviceSignInDenied => SdkError::SignInDenied,
            ServerError::DeviceSignInExpired => SdkError::SignInExpired,
            ServerError::NotSetUp => SdkError::NotSetUp,
            ServerError::RecentSignInRequired => SdkError::RecentSignInRequired,
            ServerError::LastSignInMethod => SdkError::LastSignInMethod,
            other => SdkError::Server(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_signed_in_too_long_ago_is_told_apart() {
        assert_eq!(
            SdkError::from_server(ServerError::RecentSignInRequired),
            SdkError::RecentSignInRequired
        );
    }

    #[test]
    fn a_playback_stopped_on_the_server_is_told_apart_from_an_expired_one() {
        assert_eq!(
            SdkError::from_server(ServerError::PlaybackStopped),
            SdkError::PlaybackStopped
        );
    }

    #[test]
    fn the_last_way_to_sign_in_is_told_apart() {
        assert_eq!(
            SdkError::from_server(ServerError::LastSignInMethod),
            SdkError::LastSignInMethod
        );
    }
}
