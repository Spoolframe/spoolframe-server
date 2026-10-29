//! Changes to a signed-in user's profile, from [`crate::Client::profile_events`]:
//! what a "Who's watching" screen and the profile shown follow. The
//! server tells some (`ProfileRemoved`, `PinChanged`, with
//! `spoolframe:profile-events` on); the others the SDK learns from a
//! call's answer.

/// A change to a profile on server `server` (its key).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ProfileEvent {
    pub server: String,
    pub change: ProfileChange,
}

/// Each `user` is a user id, as in [`crate::Session::user`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum ProfileChange {
    /// The user was disabled or deleted, or this device was forgotten for
    /// them: their sessions here have ended, and the next call in one is
    /// [`crate::SdkError::SignInRequired`]. Leave the profile.
    Removed { user: String },
    /// The user's PIN was set, changed or removed on another device or by
    /// an admin; `required`, whether they have one now. The SDK has
    /// already locked the profile again here, where its PIN is asked for,
    /// and forgotten the old PIN kept for offline: show the PIN pad if
    /// [`crate::Downloads::profile_locked`] says so.
    PinChanged { user: String, required: bool },
    /// A call in the user's session was
    /// [`crate::SdkError::SignInRequired`]: the session is forgotten
    /// here, and the user stays in [`crate::Client::saved_profiles`].
    SessionEnded {
        user: String,
        reason: SessionEndReason,
    },
    /// A call in the user's session was
    /// [`crate::SdkError::PasswordChangeRequired`]: show "Choose a new
    /// password".
    PasswordChangeRequired { user: String },
}

/// Why a session ended, as far as this device can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum SessionEndReason {
    /// The server answered that it no longer knows the session: signed
    /// out elsewhere, revoked, expired, or its user disabled.
    Ended,
    /// The server signed this device out: no session of any user is left
    /// for it there ([`crate::Client::signed_out`]).
    SignedOut,
}
