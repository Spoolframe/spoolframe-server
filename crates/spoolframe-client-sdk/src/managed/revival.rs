//! Starting a playback again when the app finds it dead, under the one
//! restart budget, and why one is over for good.

use super::{ManagedPlayback, Restart};
use crate::{Playback, SdkError};

/// Why the app asks for a playback to be started again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartCause {
    /// The server ended it: a call said [`SdkError::PlaybackExpired`].
    Expired,
    /// The player's link to it broke (its reader failed, the connection
    /// dropped): the old handle is stopped, as it may still run.
    TransportLost,
}

/// Why a playback is over for good: nothing the app or the SDK does
/// starts it again. Say so, and stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaybackStopped {
    /// The server's admin stopped it.
    ByAdmin,
    /// The session ended: sign in again.
    SignedOut,
    /// The item is no longer served to this user.
    Gone,
    /// It cannot play here, one reason per stream.
    CannotPlay { reasons: Vec<String> },
    /// Started again, it would play differently.
    Changed,
    /// It was started again too often lately
    /// ([`ManagedPlayback::MAX_RESUMES`]).
    TooManyRestarts,
    /// The app closed it.
    Closed,
}

impl PlaybackStopped {
    /// Whether `error`, from a [`ManagedPlayback`] call, ends it, and why.
    /// `None` for an error that a later try may not meet (a lost
    /// connection, a busy server).
    pub fn of(error: &SdkError) -> Option<Self> {
        Some(match error {
            SdkError::PlaybackStopped => Self::ByAdmin,
            SdkError::SignInRequired => Self::SignedOut,
            SdkError::NotFound(_) => Self::Gone,
            SdkError::CannotPlay { reasons } => Self::CannotPlay {
                reasons: reasons.clone(),
            },
            SdkError::PlaybackChanged => Self::Changed,
            // A managed playback gives it only once its budget is spent.
            SdkError::PlaybackExpired => Self::TooManyRestarts,
            SdkError::PlaybackClosed => Self::Closed,
            _ => return None,
        })
    }
}

impl ManagedPlayback {
    /// Why the playback is over for good, once it is.
    pub fn stopped(&self) -> Option<PlaybackStopped> {
        let state = self.0.state.lock();
        if state.closed {
            return Some(PlaybackStopped::Closed);
        }
        state.ended.as_ref().and_then(PlaybackStopped::of)
    }

    /// Start the playback again where the viewer was, with its tracks
    /// and options, and swap it in for every clone; build a new reader
    /// from the answer. It counts against the same budget as the SDK's
    /// own resumes ([`Self::MAX_RESUMES`]): past it, the error is
    /// [`SdkError::PlaybackExpired`] ([`PlaybackStopped::TooManyRestarts`]).
    pub async fn restart(&self, cause: RestartCause) -> Result<Playback, SdkError> {
        let this = self.clone();
        self.0
            .runtime
            .spawn(async move { this.restart_inner(cause).await })
            .await
            .map_err(|e| SdkError::Protocol(format!("playback control task ended: {e}")))?
    }

    async fn restart_inner(&self, cause: RestartCause) -> Result<Playback, SdkError> {
        let _control = self.0.control.lock().await;
        let old = self.live()?;
        let at = {
            let state = self.0.state.lock();
            state.position.or(state.options.start).unwrap_or_default()
        };
        self.restart_locked(&old.handle, Restart::Control { at })
            .await?;
        if cause == RestartCause::TransportLost {
            let backend = self.0.backend.clone();
            self.0.runtime.spawn(async move {
                let _ = backend.stop(&old.handle).await;
            });
        }
        self.flush_locked().await?;
        Ok(self.snapshot())
    }
}
