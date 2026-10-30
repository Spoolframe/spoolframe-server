//! "Who's watching": everyone who can open a profile on a server from
//! this device, merged from the sessions kept here, the profiles the
//! server trusts this device for, and the saved sign-ins, with how each
//! opens; which one to open at start; and the profile browsed last. Each
//! app merged these itself, and in its own order.

use serde::{Deserialize, Serialize};

use crate::portable::PortableUserId;
use crate::session::{ProfileSignIn, SignInMethod};
use crate::storage::keys;
use crate::{Client, SdkError, Session};

/// One profile on "Who's watching" ([`Client::whos_watching`]).
#[derive(Debug, Clone)]
pub struct WatchingProfile {
    pub username: String,
    pub display_name: String,
    /// How choosing it opens it.
    pub opens: ProfileOpens,
    /// Its PIN is asked for before it opens.
    pub locked: bool,
    /// It has a PIN, asked for when it opens on a device that asks (a
    /// lock on its card), whether or not it is locked now. False where
    /// nothing here says: a saved sign-in with no session.
    pub pin_required: bool,
    /// How many digits its PIN has, for a PIN pad of that length: as the
    /// server says for a trusted profile, or a session's user; `None`
    /// when it does not say.
    pub pin_digits: Option<u8>,
    /// The server opens it at start, past "Who's watching"
    /// ([`spoolframe_protocol::TrustedProfile::auto_login`]).
    pub auto_login: bool,
    /// The profile browsed last on this server ([`Client::remember_last`]).
    pub last: bool,
}

/// How a profile on "Who's watching" opens.
#[derive(Debug, Clone)]
pub enum ProfileOpens {
    /// Signed in now: open its session ([`Client::activate_profile`]).
    Session(Session),
    /// Its session ended, and the server trusts this device for them:
    /// [`Client::resume_profile`] with `user_id`, no password.
    Trusted { user_id: String },
    /// Signed in as a portable identity: [`Client::sign_in_portable`]
    /// with `identity`, nothing asked.
    Portable { identity: PortableUserId },
    /// Its session ended: they sign in again, the way they last did
    /// (`method`; `None` when none is recorded).
    SignIn { method: Option<SignInMethod> },
}

/// Who is on "Who's watching" for one server, in the order to show them:
/// signed in now, then those trusted or portable to open with nothing
/// asked, then those to sign in again, each in the order they first
/// signed in here.
#[derive(Debug, Clone, Default)]
pub struct WhosWatching {
    pub profiles: Vec<WatchingProfile>,
    /// The server could not be reached: the profiles are only the
    /// sign-ins saved here, none signed in, and none opens until it is
    /// back. Offline mode shows their downloads.
    pub offline: bool,
}

/// What an app does at start, or when a server is chosen
/// ([`WhosWatching::launch_target`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum LaunchTarget {
    /// No one to show: straight to the sign-in form.
    SignIn,
    /// Show "Who's watching".
    Pick,
    /// Open `username`'s profile at once, by its
    /// [`WatchingProfile::opens`]; a locked one asks for its PIN, or is
    /// unlocked with the PIN that opened it offline
    /// ([`Session::unlock_as_offline`]).
    Open { username: String },
}

impl WhosWatching {
    pub fn get(&self, username: &str) -> Option<&WatchingProfile> {
        self.profiles.iter().find(|p| p.username == username)
    }

    /// Where an app goes on this server. With no one, the sign-in form.
    /// With `open_last` (at start, not when the user went back to pick):
    /// the profile the server opens at start, else the one browsed last
    /// if it is signed in and not locked, or locked and unlocked offline
    /// a moment ago ([`Session::unlocked_offline`]). Else "Who's
    /// watching".
    pub fn launch_target(&self, open_last: bool) -> LaunchTarget {
        if self.profiles.is_empty() {
            return LaunchTarget::SignIn;
        }
        if !open_last {
            return LaunchTarget::Pick;
        }
        let open = |p: &WatchingProfile| LaunchTarget::Open {
            username: p.username.clone(),
        };
        if let Some(auto) = self.profiles.iter().find(|p| p.auto_login) {
            return open(auto);
        }
        match self.profiles.iter().find(|p| p.last) {
            Some(
                last @ WatchingProfile {
                    opens: ProfileOpens::Session(session),
                    ..
                },
            ) if !last.locked || session.unlocked_offline() => open(last),
            _ => LaunchTarget::Pick,
        }
    }
}

/// The profile browsed last, as kept ([`keys::LAST_PROFILE`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct LastProfile {
    /// The server, a hex endpoint id.
    pub server: String,
    pub username: String,
}

impl Client {
    /// "Who's watching" on server `endpoint_id`: its sessions here, asked
    /// of the server as [`Client::profiles`] asks; the profiles it trusts
    /// this device for whose sessions ended ([`Client::trusted_profiles`];
    /// none from a server that cannot say); and the saved sign-ins
    /// ([`Client::saved_profiles`]), merged by username. The profile
    /// browsed last is marked.
    pub async fn whos_watching(&self, endpoint_id: &str) -> Result<WhosWatching, SdkError> {
        let server = crate::client::parse_id(endpoint_id)?;
        self.require_pinned(server).await?;
        // One load: the sessions asked of the server and the saved
        // sign-ins are both of it, with any that ended left unsigned.
        let mut stored = crate::session::load(self.inner.storage.as_ref(), &server).await?;
        let (sessions, trusted, last) = tokio::join!(
            self.sessions_of(server, &mut stored),
            self.trusted_profiles(endpoint_id),
            self.last_profile()
        );
        let saved = stored.iter().map(crate::SavedProfile::from).collect();
        let last = last?
            .filter(|l| l.server == endpoint_id)
            .map(|l| l.username);
        // With no session asked of the server, the trusted profiles are
        // all that says whether it is reached.
        watching(sessions.transpose(), trusted, saved, last.as_deref())
    }

    /// Open `username`'s profile on server `endpoint_id` as "Who's
    /// watching" lists it, made the foreground profile: its session, or
    /// one resumed on a trusted device, or a portable identity's signed
    /// in. A locked profile opens too, and asks for its PIN
    /// ([`Session::unlock`]) before anything else. [`SdkError::NotFound`]
    /// for no such profile; [`SdkError::SignInRequired`] for one that
    /// signs in again with a password or passkey; transport errors while
    /// the server is not reached.
    pub async fn open_profile(
        &self,
        endpoint_id: &str,
        username: &str,
    ) -> Result<Session, SdkError> {
        let server = crate::client::parse_id(endpoint_id)?;
        self.require_pinned(server).await?;
        // Only `username`'s own: its kept session, asked of the server
        // alone, then what "Who's watching" would fall back to, in the
        // same order.
        let mut stored = crate::session::load(self.inner.storage.as_ref(), &server).await?;
        let mine = stored.iter().position(|s| s.username == username);
        if let Some(at) = mine.filter(|&at| stored[at].token.is_some())
            && let Some(session) = self
                .sessions_of(server, &mut stored[at..=at])
                .await?
                .unwrap_or_default()
                .pop()
        {
            self.activate_profile(&session).await?;
            return Ok(session);
        }
        let saved = mine.map(|at| crate::SavedProfile::from(&stored[at]));
        let trusted = match self.trusted_profiles(endpoint_id).await {
            Ok(trusted) => trusted.into_iter().find(|t| t.username == username),
            // A saved sign-in still opens without the trusted profiles.
            Err(e) if saved.is_none() => return Err(e),
            Err(_) => None,
        };
        let session = match (trusted, saved) {
            (Some(trusted), _) => self.resume_profile(endpoint_id, &trusted.user_id).await?,
            (
                None,
                Some(crate::SavedProfile {
                    portable: Some(identity),
                    method: Some(SignInMethod::Portable),
                    ..
                }),
            ) => self.sign_in_portable(endpoint_id, identity).await?,
            (None, Some(_)) => return Err(SdkError::SignInRequired),
            (None, None) => return Err(SdkError::NotFound(format!("profile {username}"))),
        };
        self.activate_profile(&session).await?;
        Ok(session)
    }

    /// Keep `username` on server `endpoint_id` as the profile browsed
    /// last: [`Client::whos_watching`] marks it, and
    /// [`WhosWatching::launch_target`] opens it at start. Kept in the
    /// app's plain data, not its secrets.
    pub async fn remember_last(&self, endpoint_id: &str, username: &str) -> Result<(), SdkError> {
        let last = LastProfile {
            server: endpoint_id.to_owned(),
            username: username.to_owned(),
        };
        let bytes = serde_json::to_vec(&last).expect("a last profile serializes");
        self.inner
            .storage
            .set_data(keys::LAST_PROFILE, Some(bytes))
            .await?;
        Ok(())
    }

    /// The profile browsed last, on any server: where an app opens at
    /// start, and whose downloads offline mode shows first.
    pub async fn last_profile(&self) -> Result<Option<LastProfile>, SdkError> {
        let kept = self.inner.storage.data(keys::LAST_PROFILE).await?;
        Ok(kept.and_then(|bytes| serde_json::from_slice(&bytes).ok()))
    }

    /// Forget the profile browsed last, if it is `username` on
    /// `endpoint_id`: they signed out, or the server was forgotten.
    pub async fn forget_last(&self, endpoint_id: &str, username: &str) -> Result<(), SdkError> {
        let is_it = self
            .last_profile()
            .await?
            .is_some_and(|l| l.server == endpoint_id && l.username == username);
        if is_it {
            self.inner
                .storage
                .set_data(keys::LAST_PROFILE, None)
                .await?;
        }
        Ok(())
    }

    /// Sign in as portable identity `identity` on each of `servers` (hex
    /// endpoint ids, as a pairing brought them,
    /// [`crate::pairing::Paired::servers`]), one not answering never
    /// stopping the rest: the sessions made, in order, and why each other
    /// one failed.
    pub async fn sign_in_paired(
        &self,
        identity: PortableUserId,
        servers: &[String],
    ) -> PairedSignIn {
        let mut signed = PairedSignIn::default();
        for server in servers {
            match self.sign_in_portable(server, identity).await {
                Ok(session) => signed.sessions.push(session),
                Err(error) => signed.failed.push((server.clone(), error)),
            }
        }
        signed
    }
}

/// What [`Client::sign_in_paired`] did.
#[derive(Debug, Default)]
pub struct PairedSignIn {
    /// Open the first: its server is the one to show.
    pub sessions: Vec<Session>,
    /// Each server not signed in to, a hex endpoint id, and why.
    pub failed: Vec<(String, SdkError)>,
}

impl Session {
    /// Unlock this profile with the PIN that unlocked its downloads
    /// offline a moment ago ([`crate::Downloads::unlock_offline`]), so it
    /// is not asked for again once the server is back: `true` when it
    /// did. `false` with no such PIN held (none, or another profile's),
    /// and the PIN pad asks. The PIN is held in memory only, and once.
    pub async fn unlock_as_offline(&self) -> Result<bool, SdkError> {
        let held = self
            .client
            .inner
            .downloads
            .locks
            .take_pin(&self.server(), &self.user().user_id);
        match held {
            Some(pin) => self.unlock(&pin).await.map(|()| true),
            None => Ok(false),
        }
    }

    /// The PIN that unlocked this profile offline is held: opening it
    /// online needs no PIN typed ([`Session::unlock_as_offline`]).
    pub fn unlocked_offline(&self) -> bool {
        self.client
            .inner
            .downloads
            .locks
            .holds_pin(&self.server(), &self.user().user_id)
    }
}

/// [`Client::whos_watching`] from what the server answered: merged, or,
/// with the server not reached, the saved sign-ins alone, marked
/// offline. `sessions` is `None` when no session was asked of it; then
/// the trusted profiles are the only call, and their transport error is
/// the server not reached. Any other failure of the sessions is the
/// error. A failure of the trusted profiles alone, a timeout too, leaves
/// them out when the sessions answered: the server is reached and they
/// still open.
fn watching(
    sessions: Option<Result<Vec<Session>, SdkError>>,
    trusted: Result<Vec<spoolframe_protocol::TrustedProfile>, SdkError>,
    saved: Vec<crate::SavedProfile>,
    last: Option<&str>,
) -> Result<WhosWatching, SdkError> {
    let unreached = match &sessions {
        Some(sessions) => sessions.as_ref().is_err_and(SdkError::is_transport),
        None => trusted.as_ref().is_err_and(SdkError::is_transport),
    };
    if unreached {
        return Ok(WhosWatching {
            offline: true,
            ..merge(Vec::new(), Vec::new(), saved, last)
        });
    }
    let trusted = trusted.unwrap_or_else(|e| {
        tracing::debug!(error = %e, "no trusted profiles; listed without them");
        Vec::new()
    });
    Ok(merge(
        sessions.transpose()?.unwrap_or_default(),
        trusted,
        saved,
        last,
    ))
}

/// [`Client::whos_watching`]'s merge, by username: each session; each
/// trusted profile with none; each saved sign-in with neither, portable
/// ones before those to sign in again.
fn merge(
    sessions: Vec<Session>,
    trusted: Vec<spoolframe_protocol::TrustedProfile>,
    saved: Vec<crate::SavedProfile>,
    last: Option<&str>,
) -> WhosWatching {
    let auto: Vec<&str> = trusted
        .iter()
        .filter(|t| t.auto_login)
        .map(|t| t.username.as_str())
        .collect();
    let trusted_digits = |username: &str| {
        trusted
            .iter()
            .find(|t| t.username == username)
            .and_then(|t| t.pin_digits)
    };
    let mut profiles: Vec<WatchingProfile> = sessions
        .into_iter()
        .map(|s| WatchingProfile {
            username: s.user().username.clone(),
            display_name: s.user().display_name.clone(),
            locked: s.user().locked,
            pin_required: s.user().locked || s.user().pin.is_some(),
            // The user's own, else as the trusted row says.
            pin_digits: s
                .user()
                .pin
                .and_then(|p| p.digits)
                .or_else(|| trusted_digits(&s.user().username)),
            auto_login: auto.contains(&s.user().username.as_str()),
            last: false,
            opens: ProfileOpens::Session(s),
        })
        .collect();
    let listed = |profiles: &[WatchingProfile], username: &str| {
        profiles.iter().any(|p| p.username == username)
    };
    for t in &trusted {
        if !listed(&profiles, &t.username) {
            profiles.push(WatchingProfile {
                username: t.username.clone(),
                display_name: t.display_name.clone(),
                opens: ProfileOpens::Trusted {
                    user_id: t.user_id.clone(),
                },
                locked: t.pin_required,
                pin_required: t.pin_required,
                pin_digits: t.pin_digits,
                auto_login: t.auto_login,
                last: false,
            });
        }
    }
    let (portable, others): (Vec<_>, Vec<_>) = saved
        .into_iter()
        .partition(|s| s.portable.is_some() && s.method == Some(SignInMethod::Portable));
    for s in portable.into_iter().chain(others) {
        if listed(&profiles, &s.username) {
            continue;
        }
        let opens = match (s.portable, s.method, s.sign_in) {
            (Some(identity), Some(SignInMethod::Portable), _) => {
                ProfileOpens::Portable { identity }
            }
            // A session kept that the server no longer knew is ended by
            // `profiles`, so one still saved here signs in again.
            (_, method, ProfileSignIn::Automatic | ProfileSignIn::Password) => {
                ProfileOpens::SignIn { method }
            }
        };
        profiles.push(WatchingProfile {
            username: s.username,
            display_name: s.display_name,
            opens,
            locked: false,
            pin_required: false,
            pin_digits: None,
            auto_login: false,
            last: false,
        });
    }
    for p in &mut profiles {
        p.last = last == Some(p.username.as_str());
    }
    WhosWatching {
        profiles,
        offline: false,
    }
}

#[cfg(test)]
#[path = "whos_watching_tests.rs"]
mod tests;
