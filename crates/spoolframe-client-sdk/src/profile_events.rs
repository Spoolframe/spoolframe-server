//! Changes to a signed-in user's profile, from [`crate::Client::profile_events`]:
//! what a "Who's watching" screen and the profile shown follow. The
//! server tells some (`ProfileRemoved`, `PinChanged`, with
//! `spoolframe:profile-events` on); the others the SDK learns from a
//! call's answer.

use tokio::sync::broadcast::{Receiver, error::RecvError};

/// A change to a profile on server `server` (its key).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ProfileEvent {
    pub server: String,
    pub change: ProfileChange,
}

impl ProfileEvent {
    /// It is about user `user_id` on server `server`.
    pub fn is_of(&self, server: &str, user_id: &str) -> bool {
        self.server == server && self.change.user() == user_id
    }
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

impl ProfileChange {
    /// The user id it is about.
    pub fn user(&self) -> &str {
        match self {
            ProfileChange::Removed { user }
            | ProfileChange::PinChanged { user, .. }
            | ProfileChange::SessionEnded { user, .. }
            | ProfileChange::PasswordChangeRequired { user } => user,
        }
    }
}

/// The changes to one session's own profile, from
/// [`crate::Session::own_profile_events`]: what the profile shown
/// follows, with no other user's or server's.
pub struct OwnProfileEvents {
    events: Receiver<ProfileEvent>,
    server: String,
    user_id: String,
}

impl OwnProfileEvents {
    /// The next change, as a broadcast receiver gives one:
    /// `Lagged` with how many changes were missed while behind (read the
    /// profile again), `Closed` once the client is gone.
    pub async fn next(&mut self) -> Result<ProfileChange, RecvError> {
        loop {
            let event = self.events.recv().await?;
            if event.is_of(&self.server, &self.user_id) {
                return Ok(event.change);
            }
        }
    }
}

impl crate::Session {
    /// The changes to this session's own profile, as
    /// [`crate::Client::profile_events`] tells them: its PIN, its
    /// removal, its session ended.
    pub fn own_profile_events(&self) -> OwnProfileEvents {
        OwnProfileEvents {
            events: self.client.profile_events(),
            server: self.server(),
            user_id: self.user().user_id.clone(),
        }
    }
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

#[cfg(test)]
mod tests {
    use tokio::sync::broadcast;

    use super::*;

    fn event(server: &str, change: ProfileChange) -> ProfileEvent {
        ProfileEvent {
            server: server.into(),
            change,
        }
    }

    #[tokio::test]
    async fn a_session_hears_only_its_own_profile_s_changes() {
        let (tx, events) = broadcast::channel(8);
        let mut own = OwnProfileEvents {
            events,
            server: "s1".into(),
            user_id: "u1".into(),
        };
        let removed = |user: &str| ProfileChange::Removed { user: user.into() };
        let cleared = ProfileChange::PinChanged {
            user: "u1".into(),
            required: false,
        };
        tx.send(event("s1", removed("u2"))).unwrap();
        tx.send(event("s2", removed("u1"))).unwrap();
        tx.send(event("s1", cleared.clone())).unwrap();
        tx.send(event("s1", removed("u1"))).unwrap();
        drop(tx);
        assert_eq!(
            [own.next().await, own.next().await, own.next().await],
            [Ok(cleared), Ok(removed("u1")), Err(RecvError::Closed)]
        );
    }

    #[tokio::test]
    async fn a_session_behind_says_how_many_it_missed() {
        let (tx, events) = broadcast::channel(1);
        let mut own = OwnProfileEvents {
            events,
            server: "s1".into(),
            user_id: "u1".into(),
        };
        let removed = ProfileChange::Removed { user: "u2".into() };
        tx.send(event("s1", removed.clone())).unwrap();
        tx.send(event("s1", removed)).unwrap();
        assert_eq!(own.next().await, Err(RecvError::Lagged(1)));
    }
}
