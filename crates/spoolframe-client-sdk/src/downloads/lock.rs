//! The profile lock offline. The PIN is the server's to check, and a profile whose PIN is asked for on this device opens only
//! through it while the server is reached. Offline, the downloads of
//! such a profile open only with its PIN, checked here against what the
//! last online unlock left, or the device owner's own authentication
//! (Face ID, the passcode), the app saying it passed.
//!
//! What is kept, per server and user: whether the PIN is asked for here,
//! as the server last said, and an Argon2id hash of the PIN. It is in the
//! app's secret storage (the Keychain on Apple), never in the downloads
//! directory. Like the PIN itself it guards a shared device, not a stolen
//! one: a 4 to 8 digit PIN's hash can be tried through.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::now;
use crate::SdkError;
use crate::storage::{Storage, keys};

/// Wrong PINs in a row after which a profile stays locked offline until
/// it is unlocked online: the server's limit.
const MAX_WRONG: u32 = 10;
/// Wrong PINs in a row before each next one waits: 2 s, then twice as
/// long each time, up to 5 minutes.
const FREE_TRIES: u32 = 3;
const MAX_WAIT: Duration = Duration::from_secs(300);

/// One profile's lock on this device.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct ProfileLock {
    /// The server last said the profile's PIN is asked for here.
    pin_required: bool,
    /// The PIN's Argon2id PHC string, made here from the PIN that last
    /// unlocked the profile online, or was set here.
    pin_check: Option<String>,
    /// Wrong PINs in a row offline, and the last one's time (Unix
    /// seconds).
    wrong: u32,
    wrong_at: i64,
}

/// The profile the app shows, and whether it was unlocked since it was
/// chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Active {
    server: String,
    user_id: String,
    unlocked: bool,
}

/// The profiles' locks on this device, and the active profile.
#[derive(Default)]
pub(crate) struct Locks {
    profiles: Mutex<BTreeMap<String, ProfileLock>>,
    active: Mutex<Option<Active>>,
    /// Where they are kept: given once the client has its storage.
    storage: OnceLock<Arc<dyn Storage>>,
    /// Held across each save: the last one writes what is newest.
    saving: tokio::sync::Mutex<()>,
    /// The PIN that unlocked the active profile offline, kept in memory
    /// only, until that profile is opened online: it unlocks it there too,
    /// not asked for again ([`crate::Session::unlock_as_offline`]).
    held_pin: Mutex<Option<HeldPin>>,
}

/// [`Locks::held_pin`].
struct HeldPin {
    server: String,
    user_id: String,
    pin: String,
}

fn key(server: &str, user_id: &str) -> String {
    format!("{server}/{user_id}")
}

impl Locks {
    /// Read the locks kept in `storage`, and keep them there from now on.
    pub async fn load(&self, storage: Arc<dyn Storage>) -> Result<(), SdkError> {
        let kept = storage.secret(keys::OFFLINE_LOCKS).await?;
        if let Some(bytes) = kept {
            match serde_json::from_slice(&bytes) {
                Ok(profiles) => *self.profiles.lock() = profiles,
                Err(e) => {
                    tracing::warn!(error = %e, "the offline profile locks cannot be read; none kept")
                }
            }
        }
        let _ = self.storage.set(storage);
        Ok(())
    }

    async fn save(&self) {
        let Some(storage) = self.storage.get() else {
            return;
        };
        let _saving = self.saving.lock().await;
        let bytes = serde_json::to_vec(&*self.profiles.lock()).expect("locks serialize");
        if let Err(e) = storage.set_secret(keys::OFFLINE_LOCKS, Some(bytes)).await {
            tracing::warn!(error = %e, "the offline profile locks were not saved");
        }
    }

    /// Save from where no one waits for it.
    fn save_later(self: &Arc<Self>) {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let locks = self.clone();
            runtime.spawn(async move { locks.save().await });
        }
    }

    /// The app shows `server`'s `user_id` now: another profile than
    /// before starts locked, if its PIN is asked for here.
    pub fn set_active(&self, server: &str, user_id: &str) {
        let mut active = self.active.lock();
        if active
            .as_ref()
            .is_some_and(|a| a.server == server && a.user_id == user_id)
        {
            return;
        }
        *active = Some(Active {
            server: server.to_owned(),
            user_id: user_id.to_owned(),
            unlocked: false,
        });
        // Another profile now: the PIN held was that one's.
        *self.held_pin.lock() = None;
    }

    /// The PIN that unlocked `server`'s `user_id` offline is held.
    pub fn holds_pin(&self, server: &str, user_id: &str) -> bool {
        self.held_pin
            .lock()
            .as_ref()
            .is_some_and(|h| h.server == server && h.user_id == user_id)
    }

    /// The PIN that unlocked `server`'s `user_id` offline, once.
    pub fn take_pin(&self, server: &str, user_id: &str) -> Option<String> {
        let mut held = self.held_pin.lock();
        match held.as_ref() {
            Some(h) if h.server == server && h.user_id == user_id => held.take().map(|h| h.pin),
            _ => None,
        }
    }

    pub fn clear_active(&self) {
        *self.active.lock() = None;
        *self.held_pin.lock() = None;
    }

    /// The active profile, as `(server, user_id)`.
    pub fn active(&self) -> Option<(String, String)> {
        self.active
            .lock()
            .as_ref()
            .map(|a| (a.server.clone(), a.user_id.clone()))
    }

    /// `server`'s `user_id` is locked here: its PIN is asked for, and it
    /// is not the active profile unlocked.
    pub fn is_locked(&self, server: &str, user_id: &str) -> bool {
        let required = self
            .profiles
            .lock()
            .get(&key(server, user_id))
            .is_some_and(|l| l.pin_required);
        required
            && !self
                .active
                .lock()
                .as_ref()
                .is_some_and(|a| a.server == server && a.user_id == user_id && a.unlocked)
    }

    /// Unlock the active profile with `pin`, checked against the hash the
    /// last online unlock left.
    pub async fn unlock_offline(&self, pin: &str) -> Result<(), SdkError> {
        let (server, user_id) = self.locked_active()?;
        let k = key(&server, &user_id);
        let lock = self.profiles.lock().get(&k).cloned();
        let Some(lock) = lock.filter(|l| l.pin_required) else {
            return self.open(&server, &user_id);
        };
        if lock.wrong >= MAX_WRONG {
            return Err(SdkError::SignInRequired);
        }
        let wait = wait_after(lock.wrong);
        let since = Duration::from_secs(now().saturating_sub(lock.wrong_at).max(0) as u64);
        if since < wait {
            return Err(SdkError::RateLimited {
                retry_after: wait - since,
            });
        }
        let Some(check) = lock.pin_check else {
            return Err(SdkError::NoOfflinePin);
        };
        if verify(pin, check).await? {
            if let Some(lock) = self.profiles.lock().get_mut(&k) {
                lock.wrong = 0;
            }
            *self.held_pin.lock() = Some(HeldPin {
                server: server.clone(),
                user_id: user_id.clone(),
                pin: pin.to_owned(),
            });
            self.save().await;
            return self.open(&server, &user_id);
        }
        let wrong = {
            let mut profiles = self.profiles.lock();
            let lock = profiles.entry(k).or_default();
            lock.wrong += 1;
            lock.wrong_at = now();
            lock.wrong
        };
        self.save().await;
        if wrong >= MAX_WRONG {
            Err(SdkError::SignInRequired)
        } else {
            Err(SdkError::InvalidCredentials)
        }
    }

    /// Unlock the active profile: the device's owner proved it is them.
    /// Not once 10 wrong PINs in a row locked it until an online unlock.
    pub fn unlock_by_owner(&self) -> Result<(), SdkError> {
        let (server, user_id) = self.locked_active()?;
        let wrong = self
            .profiles
            .lock()
            .get(&key(&server, &user_id))
            .map_or(0, |l| l.wrong);
        if wrong >= MAX_WRONG {
            return Err(SdkError::SignInRequired);
        }
        self.open(&server, &user_id)
    }

    /// The active profile, which must be there.
    fn locked_active(&self) -> Result<(String, String), SdkError> {
        self.active().ok_or_else(|| {
            SdkError::InvalidInput("no profile is active: Downloads::set_profile first".into())
        })
    }

    fn open(&self, server: &str, user_id: &str) -> Result<(), SdkError> {
        if let Some(active) = self.active.lock().as_mut()
            && active.server == server
            && active.user_id == user_id
        {
            active.unlocked = true;
        }
        Ok(())
    }

    /// The server said whether `server`'s `user_id` asks for their PIN
    /// here. Off drops the hash kept.
    pub fn seen(self: &Arc<Self>, server: &str, user_id: &str, pin_required: bool) {
        let changed = {
            let mut profiles = self.profiles.lock();
            let k = key(server, user_id);
            if pin_required {
                let lock = profiles.entry(k).or_default();
                let changed = !lock.pin_required;
                lock.pin_required = true;
                changed
            } else {
                profiles.remove(&k).is_some()
            }
        };
        if changed {
            self.save_later();
        }
    }

    /// A session of `server`'s `user_id` started locked: its PIN is asked
    /// for here. One that did not says nothing (its connection may have
    /// been unlocked).
    pub fn session_locked(self: &Arc<Self>, server: &str, user_id: &str, locked: bool) {
        if locked {
            self.seen(server, user_id, true);
        }
    }

    /// `pin` unlocked `server`'s `user_id` online: kept to check offline,
    /// with the wrong ones counted here forgotten, and the profile open
    /// here if it is the active one.
    pub async fn unlocked_online(&self, server: &str, user_id: &str, pin: &str) {
        let check = hash(pin).await;
        {
            let mut profiles = self.profiles.lock();
            let lock = profiles.entry(key(server, user_id)).or_default();
            lock.pin_required = true;
            lock.wrong = 0;
            if check.is_some() {
                lock.pin_check = check;
            }
        }
        let _ = self.open(server, user_id);
        self.save().await;
    }

    /// `server`'s `user_id` set their PIN to `pin` here.
    pub async fn pin_set(&self, server: &str, user_id: &str, pin: &str) {
        let Some(check) = hash(pin).await else {
            return;
        };
        self.profiles
            .lock()
            .entry(key(server, user_id))
            .or_default()
            .pin_check = Some(check);
        self.save().await;
    }

    /// `server`'s `user_id`'s PIN changed elsewhere; `required`, whether
    /// they have one now. Without one, as [`Locks::pin_cleared`]. With a
    /// new one, the PIN kept to check offline is the old one: it goes, and
    /// the profile, if active, locks again where its PIN is asked for.
    pub async fn pin_changed(&self, server: &str, user_id: &str, required: bool) {
        if !required {
            return self.pin_cleared(server, user_id).await;
        }
        {
            let mut profiles = self.profiles.lock();
            let Some(lock) = profiles.get_mut(&key(server, user_id)) else {
                return;
            };
            lock.pin_check = None;
            lock.wrong = 0;
        }
        if let Some(active) = self.active.lock().as_mut()
            && active.server == server
            && active.user_id == user_id
        {
            active.unlocked = false;
        }
        self.save().await;
    }

    /// `server`'s `user_id` has no PIN now: nothing is asked for here.
    pub async fn pin_cleared(&self, server: &str, user_id: &str) {
        let removed = self.profiles.lock().remove(&key(server, user_id)).is_some();
        if removed {
            self.save().await;
        }
    }
}

/// How long the next PIN waits after `wrong` wrong ones in a row.
fn wait_after(wrong: u32) -> Duration {
    match wrong.checked_sub(FREE_TRIES) {
        None => Duration::ZERO,
        Some(over) => Duration::from_secs(2u64 << over.min(16)).min(MAX_WAIT),
    }
}

/// `pin` as an Argon2id PHC string; `None`, logged, if it cannot be made.
async fn hash(pin: &str) -> Option<String> {
    let pin = zeroize::Zeroizing::new(pin.to_owned());
    let made = tokio::task::spawn_blocking(move || {
        Argon2::default()
            .hash_password(pin.as_bytes())
            .map(|h: PasswordHash| h.to_string())
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())
    .flatten();
    made.inspect_err(|e| tracing::warn!(error = %e, "the PIN cannot be kept for offline"))
        .ok()
}

/// `pin` is the one `check` was made from.
async fn verify(pin: &str, check: String) -> Result<bool, SdkError> {
    let pin = zeroize::Zeroizing::new(pin.to_owned());
    tokio::task::spawn_blocking(move || {
        let parsed = PasswordHash::new(&check)
            .map_err(|e| SdkError::Downloads(format!("the PIN kept here cannot be read: {e}")))?;
        Ok(Argon2::default()
            .verify_password(pin.as_bytes(), &parsed)
            .is_ok())
    })
    .await
    .map_err(|e| SdkError::Downloads(e.to_string()))?
}

#[cfg(test)]
mod tests;
