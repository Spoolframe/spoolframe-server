//! Password login (ARCHITECTURE §22), the sessions it creates, and the
//! limit on failed attempts.
//!
//! A login ends at a [`LocalUserId`]; the session does not record how it
//! was reached (§24B), so other methods (OIDC, P4) plug in beside this one.

use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use password_hash::rand_core::{OsRng, RngCore};

use spoolframe_types::Role;

use crate::clock::Clock;
use crate::device_sign_in::{Claim, DeviceSignIns, Handover};
use crate::refusals::Refusals;
use crate::store::sessions::{ApiSessionUser, SessionBinding, SessionOwner, SessionUser};
use crate::store::trust::{Resumed, TrustedProfile};
use crate::store::users::{LocalUserId, PasswordLogin, normalize_username};
use crate::store::{Store, StoreError};

/// How long a client (Iroh endpoint) session lasts after its last use.
/// Use renews it, so a TV in use never signs out.
pub const ENDPOINT_SESSION: Duration = Duration::from_secs(90 * 24 * 3600);
/// The longest an admin API (web UI) session lasts, from its sign-in,
/// however much it is used.
pub const API_SESSION: Duration = Duration::from_secs(12 * 3600);
/// How long an admin API session lasts unused (A-18): a use renews it,
/// within [`API_SESSION`], as a client's is renewed. A browser left
/// signed in, or a cookie taken from it, is good for an hour after the
/// last use, not for the rest of the 12.
pub const API_SESSION_IDLE: Duration = Duration::from_secs(3600);

/// Failed attempts allowed per username and per source within
/// [`FAILURE_WINDOW`] before logins are refused without checking.
pub const MAX_FAILURES: usize = 5;
pub const FAILURE_WINDOW: Duration = Duration::from_secs(15 * 60);

/// How long after its user proved who they are (signed in, or confirmed it
/// since) a session may make sensitive changes (P3-03).
pub const RECENT_SIGN_IN: Duration = Duration::from_secs(10 * 60);

/// The shortest password accepted when one is set.
pub const MIN_PASSWORD_LEN: usize = 8;

/// A session token as the client holds it. Only its hash is stored.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionToken(pub String);

impl fmt::Debug for SessionToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionToken(<redacted>)")
    }
}

impl SessionToken {
    pub(crate) fn generate() -> Self {
        let mut bytes = [0u8; 32];
        OsRng.fill_bytes(&mut bytes);
        Self(bytes.iter().map(|b| format!("{b:02x}")).collect())
    }

    /// What the database keys the session by.
    pub fn hash(&self) -> String {
        blake3::hash(self.0.as_bytes()).to_hex().to_string()
    }
}

/// A successful login.
#[derive(Debug, Clone)]
pub struct Login {
    pub user: LocalUserId,
    pub role: Role,
    pub token: SessionToken,
    /// Its device is trusted for the user after it (P4-01); always false
    /// for an API session, which has no device.
    pub remembered: bool,
}

/// How long a trusted device may go unused before its trust lapses, by
/// default (`auth.trust_idle_days`, P4-01).
pub const TRUST_IDLE: Duration = Duration::from_secs(180 * 24 * 3600);

#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    /// Unknown user, wrong password or disabled user: deliberately one
    /// answer, so the error does not reveal which.
    #[error("invalid credentials")]
    InvalidCredentials,
    #[error("too many failed logins; try again in {retry_after:?}")]
    RateLimited { retry_after: Duration },
    #[error("login failed: {0}")]
    Internal(String),
}

impl From<StoreError> for LoginError {
    fn from(e: StoreError) -> Self {
        LoginError::Internal(e.to_string())
    }
}

impl From<crate::store::users::UserError> for LoginError {
    fn from(e: crate::store::users::UserError) -> Self {
        LoginError::Internal(e.to_string())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PasswordError {
    #[error("the password must be at least {MIN_PASSWORD_LEN} characters")]
    TooShort,
    #[error("cannot hash the password: {0}")]
    Hash(String),
}

/// Hash a new password with Argon2id (the crate's recommended parameters),
/// as a PHC string that carries its parameters.
pub async fn hash_password(password: &str) -> Result<String, PasswordError> {
    if password.chars().count() < MIN_PASSWORD_LEN {
        return Err(PasswordError::TooShort);
    }
    hash_secret(password).await
}

/// Wrong PINs in a row after which a device's session is revoked (P2-08).
pub const MAX_WRONG_PINS: u32 = 10;

/// A PIN as a user may set it: 4 to 8 digits.
pub fn valid_pin(pin: &str) -> bool {
    pin_digits(pin).is_some()
}

/// How many digits `pin` has, if it is a PIN a user may set.
pub use spoolframe_types::pin::pin_digits;

/// Hash a new PIN as passwords are (Argon2id, PHC string).
pub async fn hash_pin(pin: &str) -> Result<String, PasswordError> {
    hash_secret(pin).await
}

/// How many Argon2 hashes or verifications may run at once: one a core.
/// Each takes about 19 MiB and a core for tens of milliseconds, and the
/// blocking pool would otherwise run up to 512 (PX-10, finding 10).
pub fn argon2_permits() -> usize {
    std::thread::available_parallelism().map_or(4, std::num::NonZero::get)
}

/// Run `work`, an Argon2 hash or verification, off the async threads,
/// once one of [`argon2_permits`] is free.
async fn argon2<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    static PERMITS: std::sync::LazyLock<Arc<tokio::sync::Semaphore>> =
        std::sync::LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(argon2_permits())));
    let permit = PERMITS.clone().acquire_owned().await.expect("never closed");
    // Held by the work itself: a caller that goes away does not free it
    // while the work still runs.
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await
}

async fn hash_secret(secret: &str) -> Result<String, PasswordError> {
    let password = secret.to_owned();
    argon2(move || {
        let salt = SaltString::generate(&mut OsRng);
        Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map(|hash| hash.to_string())
            .map_err(|e| PasswordError::Hash(e.to_string()))
    })
    .await
    .map_err(|e| PasswordError::Hash(e.to_string()))?
}

/// Check `password` against a PHC string. Argon2 is slow on purpose, so it
/// runs off the async threads.
async fn verify_password(password: &str, phc: String) -> bool {
    let password = password.to_owned();
    argon2(move || {
        PasswordHash::new(&phc)
            .map(|hash| {
                Argon2::default()
                    .verify_password(password.as_bytes(), &hash)
                    .is_ok()
            })
            .unwrap_or(false)
    })
    .await
    .unwrap_or(false)
}

/// A hash to verify against when the user does not exist, so an unknown
/// name takes as long as a wrong password.
fn decoy_hash() -> String {
    static DECOY: OnceLock<String> = OnceLock::new();
    DECOY
        .get_or_init(|| {
            let salt = SaltString::generate(&mut OsRng);
            Argon2::default()
                .hash_password(b"decoy password, never valid", &salt)
                .expect("hashing a fixed password cannot fail")
                .to_string()
        })
        .clone()
}

/// The failure limit's keys for a user's PIN from `source`: apart from
/// password logins, so wrong PINs and wrong passwords do not use up each
/// other's tries.
fn pin_keys(user: LocalUserId, source: &str) -> (String, String) {
    (format!("pin:{}", user), format!("pin@{source}"))
}

/// How often every key is looked at for failures out of the window. In
/// between, only the keys asked about are.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// The most keys held. A key costs a name of up to 64 characters and a
/// few timestamps, so this is a few tens of megabytes at worst.
const MAX_KEYS: usize = 100_000;

/// Failed logins, per username and per source, within a sliding window.
/// A caller whose source says nothing about who it is (a loopback peer of
/// the admin API) gives none, and only the username counts.
///
/// What an attempt costs does not grow with the attempts of others: the
/// keys it names are judged exactly, all the others are swept once per
/// [`SWEEP_INTERVAL`], and at most [`MAX_KEYS`] are held. When a new key
/// finds it full, the keys whose failures are all out of the window go,
/// then those whose last failure is oldest (PX-10, finding 9): names and
/// sources are free, so refusing new keys instead would let anyone lock
/// everyone out. Evicting a key forgets its failures; filling the limiter
/// to evict one takes [`MAX_KEYS`] new keys, each one an attempt, which
/// the stranger sign-in bucket ([`STRANGER_ATTEMPTS`]) spreads over hours.
#[derive(Debug)]
pub struct RateLimiter {
    failures: Mutex<Failures>,
    window: Duration,
    max_keys: usize,
}

#[derive(Debug, Default)]
struct Failures {
    by_key: HashMap<String, VecDeque<Instant>>,
    /// When every key was last looked at.
    swept: Option<Instant>,
    /// Per username, the last [`KNOWN_SOURCES`] sources a right attempt
    /// came from (PX-09 finding 24): a username's own failures do not
    /// refuse them. Only a right attempt adds one, so the map is as large
    /// as the accounts.
    known: HashMap<String, VecDeque<String>>,
}

/// How many sources a username is remembered at.
const KNOWN_SOURCES: usize = 16;

impl Default for RateLimiter {
    fn default() -> Self {
        Self::with_window(FAILURE_WINDOW)
    }
}

impl RateLimiter {
    /// Failures count for `window` ([`FAILURE_WINDOW`] by default; tests
    /// that need many failures shorten it).
    pub fn with_window(window: Duration) -> Self {
        Self {
            failures: Mutex::default(),
            window,
            max_keys: MAX_KEYS,
        }
    }

    /// How many keys are held.
    #[cfg(test)]
    fn held(&self) -> usize {
        self.failures.lock().by_key.len()
    }

    fn user_key(username: &str) -> String {
        format!("user:{}", username.trim().to_lowercase())
    }

    fn source_key(source: &str) -> String {
        format!("source:{source}")
    }

    fn keys(username: &str, source: Option<&str>) -> impl Iterator<Item = String> {
        [Some(Self::user_key(username)), source.map(Self::source_key)]
            .into_iter()
            .flatten()
    }

    /// `Some(wait)` if either key has used up its failures.
    pub fn check(&self, username: &str, source: Option<&str>, now: Instant) -> Option<Duration> {
        let mut failures = self.failures.lock();
        self.limited(&mut failures, username, source, now)
    }

    /// How long `username` stays limited by its own failures, if it is:
    /// what the admin pages show (P3-18). A limited source refuses the
    /// user's logins too, but is not theirs to show.
    pub fn user_limited(&self, username: &str, now: Instant) -> Option<Duration> {
        let mut failures = self.failures.lock();
        let key = Self::user_key(username);
        self.prune(&mut failures, &key, now);
        failures
            .by_key
            .get(&key)
            .filter(|times| times.len() >= MAX_FAILURES)
            .map(|times| self.window - now.duration_since(times[0]))
    }

    fn expired(&self, at: Instant, now: Instant) -> bool {
        now.duration_since(at) >= self.window
    }

    /// Once per [`SWEEP_INTERVAL`], drop every failure older than the
    /// window, and every key left with none: under all keys, not only the
    /// ones asked about, or a name tried once and never again would be
    /// kept for the life of the process.
    fn sweep(&self, failures: &mut Failures, now: Instant) {
        if failures
            .swept
            .is_some_and(|at| now.duration_since(at) < SWEEP_INTERVAL)
        {
            return;
        }
        failures.swept = Some(now);
        failures.by_key.retain(|_, times| {
            while times.front().is_some_and(|t| self.expired(*t, now)) {
                times.pop_front();
            }
            !times.is_empty()
        });
    }

    /// Drop `key`'s failures older than the window, and `key` with them if
    /// none is left: what is asked about is judged as of now, whenever the
    /// last sweep was.
    fn prune(&self, failures: &mut Failures, key: &str, now: Instant) {
        let Some(times) = failures.by_key.get_mut(key) else {
            return;
        };
        while times.front().is_some_and(|t| self.expired(*t, now)) {
            times.pop_front();
        }
        if times.is_empty() {
            failures.by_key.remove(key);
        }
    }

    /// Whether `username`'s own failures refuse an attempt from `source`:
    /// not from where it signed in before (PX-09 finding 24). Five wrong
    /// passwords from anywhere locked a name out for everyone, its owner
    /// too; now only for the places it never signed in from. A source
    /// that guesses is still refused by its own key.
    fn known(failures: &Failures, user: &str, source: Option<&str>) -> bool {
        source.is_some_and(|source| {
            failures
                .known
                .get(user)
                .is_some_and(|known| known.iter().any(|s| s == source))
        })
    }

    fn limited(
        &self,
        failures: &mut Failures,
        username: &str,
        source: Option<&str>,
        now: Instant,
    ) -> Option<Duration> {
        self.sweep(failures, now);
        let user = Self::user_key(username);
        let mut wait = None;
        for key in Self::keys(username, source) {
            if key == user && Self::known(failures, &user, source) {
                continue;
            }
            self.prune(failures, &key, now);
            if let Some(times) = failures.by_key.get(&key)
                && times.len() >= MAX_FAILURES
            {
                wait = wait.max(Some(self.window - now.duration_since(times[0])));
            }
        }
        wait
    }

    /// Make room for the keys of an attempt by `username` from `source`
    /// that are not held yet: drop every key out of the window, then the
    /// keys whose last failure is oldest. The attempt's own keys stay.
    fn make_room(
        &self,
        failures: &mut Failures,
        username: &str,
        source: Option<&str>,
        now: Instant,
    ) {
        let keys: Vec<String> = Self::keys(username, source).collect();
        let new = keys
            .iter()
            .filter(|key| !failures.by_key.contains_key(*key))
            .count();
        if failures.by_key.len() + new <= self.max_keys {
            return;
        }
        failures.swept = None;
        self.sweep(failures, now);
        while failures.by_key.len() + new > self.max_keys {
            // Only when full: a scan of at most MAX_KEYS, at the rate
            // attempts can be made.
            let oldest = failures
                .by_key
                .iter()
                .filter(|(key, _)| !keys.contains(key))
                .min_by_key(|(_, times)| times.back().copied())
                .map(|(key, _)| key.clone());
            match oldest {
                Some(key) => failures.by_key.remove(&key),
                None => break,
            };
        }
    }

    /// Count a failure.
    pub fn record_failure(&self, username: &str, source: Option<&str>, now: Instant) {
        let mut failures = self.failures.lock();
        self.sweep(&mut failures, now);
        self.make_room(&mut failures, username, source, now);
        for key in Self::keys(username, source) {
            self.prune(&mut failures, &key, now);
            failures.by_key.entry(key).or_default().push_back(now);
        }
    }

    /// Count an attempt at `now`, before it is checked: `Err(wait)`, and
    /// nothing counted, if either key has used up its failures. It stays
    /// counted as a failure unless [`RateLimiter::record_success`] takes it
    /// back, so attempts sent at once cannot all be checked before the
    /// first of them has failed.
    pub fn begin(
        &self,
        username: &str,
        source: Option<&str>,
        now: Instant,
    ) -> Result<(), Duration> {
        let mut failures = self.failures.lock();
        if let Some(wait) = self.limited(&mut failures, username, source, now) {
            return Err(wait);
        }
        self.make_room(&mut failures, username, source, now);
        for key in Self::keys(username, source) {
            failures.by_key.entry(key).or_default().push_back(now);
        }
        Ok(())
    }

    /// The attempt begun at `began` was right, and nothing else changes:
    /// it is taken back from both keys. For a guess at a code, where a
    /// right one says nothing about who is guessing (A-13).
    pub fn take_back(&self, username: &str, source: Option<&str>, began: Instant) {
        let mut failures = self.failures.lock();
        for key in Self::keys(username, source) {
            if let Some(times) = failures.by_key.get_mut(&key) {
                if let Some(at) = times.iter().position(|t| *t == began) {
                    times.remove(at);
                }
                if times.is_empty() {
                    failures.by_key.remove(&key);
                }
            }
        }
    }

    /// The attempt begun at `began` was right: it is taken back, and the
    /// username's failures are cleared. The source's other failures stay:
    /// one account's right password must not reset guessing at others from
    /// the same place.
    pub fn record_success(&self, username: &str, source: Option<&str>, began: Instant) {
        let mut failures = self.failures.lock();
        let user = Self::user_key(username);
        failures.by_key.remove(&user);
        if let Some(source) = source {
            let known = failures.known.entry(user).or_default();
            known.retain(|s| s != source);
            if known.len() == KNOWN_SOURCES {
                known.pop_front();
            }
            known.push_back(source.to_owned());
        }
        let Some(source) = source.map(Self::source_key) else {
            return;
        };
        if let Some(times) = failures.by_key.get_mut(&source)
            && let Some(at) = times.iter().position(|t| *t == began)
        {
            times.remove(at);
            if times.is_empty() {
                failures.by_key.remove(&source);
            }
        }
    }
}

/// How many sign-in attempts devices that are not signed in may make, all
/// of them together (PX-10, finding 9): a burst of 20, then one every
/// 200 ms (5 a second). A household's new devices sign in a few times a
/// day; a flood from free endpoint ids is held to the rate one Argon2
/// verification a core can keep up with, and needs hours, not minutes, to
/// fill [`RateLimiter`]. A signed-in device's attempts are not counted.
pub const STRANGER_ATTEMPTS: Bucket = Bucket {
    capacity: 20,
    refill_every: Duration::from_millis(200),
};

/// A token bucket's size and rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bucket {
    pub capacity: u32,
    /// One token comes back each time this passes.
    pub refill_every: Duration,
}

/// A token bucket: [`Bucket::capacity`] at first, one more each
/// [`Bucket::refill_every`].
#[derive(Debug)]
pub struct TokenBucket {
    bucket: Bucket,
    state: Mutex<(u32, Instant)>,
}

impl TokenBucket {
    pub fn new(bucket: Bucket) -> Self {
        Self {
            bucket,
            state: Mutex::new((bucket.capacity, Instant::now())),
        }
    }

    /// Take a token at `now`, or `Err(wait)` until the next one.
    pub fn take(&self, now: Instant) -> Result<(), Duration> {
        let mut state = self.state.lock();
        let (tokens, since) = &mut *state;
        let every = self.bucket.refill_every.max(Duration::from_nanos(1));
        let elapsed = now.saturating_duration_since(*since);
        let earned = (elapsed.as_nanos() / every.as_nanos()).min(u128::from(u32::MAX)) as u32;
        if earned > 0 {
            *tokens = tokens.saturating_add(earned).min(self.bucket.capacity);
            *since += every * earned;
            if *tokens == self.bucket.capacity {
                *since = now;
            }
        }
        if *tokens == 0 {
            return Err(every.saturating_sub(now.saturating_duration_since(*since)));
        }
        *tokens -= 1;
        Ok(())
    }
}

/// Why a PIN did not unlock a profile (P2-08).
#[derive(Debug, thiserror::Error)]
pub enum UnlockError {
    #[error("wrong PIN")]
    Wrong,
    #[error("too many wrong PINs; try again in {retry_after:?}")]
    RateLimited { retry_after: Duration },
    /// The last of [`MAX_WRONG_PINS`] in a row: the session on this device
    /// is revoked.
    #[error("too many wrong PINs; sign in again")]
    Revoked,
    #[error("unlock failed: {0}")]
    Internal(String),
}

impl From<StoreError> for UnlockError {
    fn from(e: StoreError) -> Self {
        UnlockError::Internal(e.to_string())
    }
}

/// A guess at a code, counted as wrong until [`Auth::guessed_right`]
/// takes it back.
#[derive(Debug)]
#[must_use = "a guess is counted as wrong unless it is taken back"]
pub struct Guess {
    what: String,
    source: Option<String>,
    began: Instant,
}

/// How an approved device sign-in ended (P3-07).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    /// The device has its session.
    SignedIn,
    /// The code ran out first; the device was told so, and no session
    /// was left behind.
    Expired,
}

/// Logins and sessions.
#[derive(Debug, Clone)]
pub struct Auth {
    store: Store,
    limiter: Arc<RateLimiter>,
    /// Wrong PINs in a row, per user and device.
    wrong_pins: Arc<Mutex<HashMap<(LocalUserId, String), u32>>>,
    /// Devices waiting to be approved on another (P3-07).
    sign_ins: Arc<DeviceSignIns>,
    /// Sign-in attempts from devices that are not signed in (PX-10).
    strangers: Arc<TokenBucket>,
    /// How long a trusted device may go unused (P4-01).
    trust_idle: Duration,
    /// What failures, attempts and sign-in codes are timed by: its
    /// store's, so sessions and limits agree on what now is (PX-09).
    clock: Clock,
    /// Which refused requests are logged (finding 46).
    refusals: Arc<Refusals>,
    /// Which refused `Hello`s are logged: one per device an hour.
    hellos: Arc<Refusals>,
}

impl Auth {
    pub fn new(store: Store) -> Self {
        Self::with_failure_window(store, FAILURE_WINDOW)
    }

    /// As [`Auth::new`], with failures counting for `window`.
    ///
    /// Its clock is `store`'s ([`Store::with_clock`]).
    pub fn with_failure_window(store: Store, window: Duration) -> Self {
        let clock = store.clock().clone();
        Self {
            store,
            limiter: Arc::new(RateLimiter::with_window(window)),
            wrong_pins: Arc::default(),
            sign_ins: Arc::new(DeviceSignIns::default().with_clock(clock.clone())),
            strangers: Arc::new(TokenBucket::new(STRANGER_ATTEMPTS)),
            trust_idle: TRUST_IDLE,
            refusals: Arc::new(Refusals::new(clock.clone())),
            hellos: Arc::new(Refusals::hellos(clock.clone())),
            clock,
        }
    }

    /// Trust unused for `idle` lapses ([`TRUST_IDLE`] by default).
    pub fn with_trust_idle(mut self, idle: Duration) -> Self {
        self.trust_idle = idle;
        self
    }

    fn trust_idle_secs(&self) -> i64 {
        i64::try_from(self.trust_idle.as_secs()).unwrap_or(i64::MAX)
    }

    /// Trust the device of `token`'s live endpoint session for its user
    /// (P4-01); whether it is trusted after it.
    pub async fn remember(&self, token: &SessionToken) -> Result<bool, StoreError> {
        self.store.remember_session_device(&token.hash()).await
    }

    /// The users `endpoint` may resume (P4-01), lapsed trust left out and
    /// removed.
    pub async fn trusted_profiles(
        &self,
        endpoint: &str,
    ) -> Result<Vec<TrustedProfile>, StoreError> {
        self.store
            .trusted_profiles(endpoint, self.trust_idle_secs())
            .await
    }

    /// A new session for `user` on the device `binding` names, if it is
    /// trusted for them (P4-01): `None` otherwise, whatever the reason.
    pub async fn resume(
        &self,
        user: LocalUserId,
        binding: &SessionBinding,
    ) -> Result<Option<SessionToken>, StoreError> {
        let token = SessionToken::generate();
        let resumed = self
            .store
            .resume_session(
                &token.hash(),
                user,
                binding,
                self.store.now() + ENDPOINT_SESSION.as_secs() as i64,
                self.trust_idle_secs(),
            )
            .await?;
        Ok(match resumed {
            Resumed::Session => Some(token),
            Resumed::Refused => None,
        })
    }

    /// Sign-in attempts from devices that are not signed in come from
    /// `bucket` ([`STRANGER_ATTEMPTS`] by default; tests shrink it).
    pub fn with_stranger_attempts(mut self, bucket: Bucket) -> Self {
        self.strangers = Arc::new(TokenBucket::new(bucket));
        self
    }

    /// One sign-in attempt by a device that is not signed in: `Err(wait)`
    /// while they have all used up their share.
    pub fn stranger_attempt(&self) -> Result<(), Duration> {
        self.strangers.take(self.clock.now())
    }

    /// Device sign-in codes live `lifetime` (tests shorten it).
    pub fn with_sign_in_lifetime(mut self, lifetime: Duration) -> Self {
        self.sign_ins =
            Arc::new(DeviceSignIns::with_lifetime(lifetime).with_clock(self.clock.clone()));
        self
    }

    pub fn sign_ins(&self) -> &DeviceSignIns {
        &self.sign_ins
    }

    /// Open a session on the device `claim` names, as `user`, who approved
    /// it from a session of their own (P3-07): that session is the proof,
    /// and hand it to the device. The caller must require recent
    /// authentication before claiming the code: the approved session
    /// receives a fresh authentication timestamp.
    ///
    /// The device is trusted for `user` (P4-01) only if it asked to be
    /// and the approver's `remember` agrees: the approver decides, and
    /// cannot trust a device that did not ask.
    ///
    /// The code may run out while the session is made, and the device be
    /// told so: then the session, which nobody holds, is revoked, any trust
    /// made for it removed, and the approver is told it expired too. The
    /// device and the approver never hear two different outcomes.
    pub async fn approve_device(
        &self,
        user: LocalUserId,
        claim: &Claim,
        remember: bool,
    ) -> Result<Approval, StoreError> {
        let binding = SessionBinding::Endpoint {
            endpoint_id: claim.endpoint.clone(),
            device_name: claim.device_name.clone(),
            platform: claim.platform,
        };
        let token = match self.open_session(user, &binding).await {
            Ok(token) => token,
            Err(e) => {
                // Failed or already gone, the device stops waiting.
                let _ = self.sign_ins.fulfil(&claim.request, None);
                return Err(e);
            }
        };
        let hash = token.hash();
        let made = async {
            let was = self.store.device_trusted(user, &claim.endpoint).await?;
            let remembered = if claim.remember && remember {
                self.store.remember_session_device(&hash).await?
            } else {
                was
            };
            Ok::<_, StoreError>((was, remembered))
        }
        .await;
        let (was, remembered) = match made {
            Ok(made) => made,
            Err(e) => {
                let _ = self.sign_ins.fulfil(&claim.request, None);
                self.store.discard_unheld_session(&hash).await?;
                return Err(e);
            }
        };
        if self
            .sign_ins
            .fulfil(&claim.request, Some(Handover { token, remembered }))
        {
            Ok(Approval::SignedIn)
        } else {
            // Nobody holds it: trust the device had before stays, and
            // trust made for this sign-in goes.
            self.store.discard_unheld_session(&hash).await?;
            if remembered && !was {
                self.store.forget_device(user, &claim.endpoint).await?;
            }
            Ok(Approval::Expired)
        }
    }

    /// Open a session bound as `binding` for `user`, who just proved who
    /// they are some other way than a password (an approval, a passkey).
    pub async fn open_session(
        &self,
        user: LocalUserId,
        binding: &SessionBinding,
    ) -> Result<SessionToken, StoreError> {
        let lifetime = match binding {
            SessionBinding::Endpoint { .. } => ENDPOINT_SESSION,
            SessionBinding::Api => API_SESSION_IDLE,
        };
        let token = SessionToken::generate();
        self.store
            .create_session(
                &token.hash(),
                user,
                binding,
                self.store.now() + lifetime.as_secs() as i64,
            )
            .await?;
        Ok(token)
    }

    /// Check `pin` as `user`'s, entered on `endpoint`: to unlock their
    /// profile there, or to change or remove the PIN. A user without a PIN
    /// passes. Wrong PINs are limited as passwords are, on keys of their
    /// own and the same for every use, so no use is a way round the limit;
    /// [`MAX_WRONG_PINS`] in a row revoke the user's sessions on that
    /// device. What a right PIN opens is the caller's.
    pub async fn check_pin(
        &self,
        user: LocalUserId,
        endpoint: &str,
        pin: &str,
    ) -> Result<(), UnlockError> {
        let Some(phc) = self.store.pin_phc(user).await? else {
            return Ok(());
        };
        // Counted as wrong before it is checked: PINs sent at once would
        // otherwise all be checked against a limit none had yet reached.
        let (what, from) = pin_keys(user, endpoint);
        let began = self.clock.now();
        if let Err(retry_after) = self.limiter.begin(&what, Some(&from), began) {
            return Err(UnlockError::RateLimited { retry_after });
        }
        let key = (user, endpoint.to_owned());
        if valid_pin(pin) && verify_password(pin, phc).await {
            self.limiter.record_success(&what, Some(&from), began);
            self.wrong_pins.lock().remove(&key);
            return Ok(());
        }
        let wrong = {
            let mut counts = self.wrong_pins.lock();
            let count = counts.entry(key.clone()).or_default();
            *count += 1;
            *count
        };
        if wrong >= MAX_WRONG_PINS {
            self.wrong_pins.lock().remove(&key);
            let revoked = self.store.revoke_device_sessions(user, endpoint).await?;
            tracing::warn!(
                user = %user,
                endpoint,
                revoked,
                "too many wrong PINs; sessions revoked"
            );
            return Err(UnlockError::Revoked);
        }
        Err(UnlockError::Wrong)
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Which refused requests are logged, shared by every listener.
    pub fn refusals(&self) -> &Refusals {
        &self.refusals
    }

    /// Which refused `Hello`s are logged ([`Refusals::hellos`]).
    pub fn refused_hellos(&self) -> &Refusals {
        &self.hellos
    }

    /// Whose session `token` is, live or not (finding 46).
    pub async fn session_owner(&self, token: &str) -> Result<Option<SessionOwner>, StoreError> {
        self.store
            .session_owner(&SessionToken(token.to_owned()).hash())
            .await
    }

    /// Why a password sign-in as `username` was refused, for the log: the
    /// caller is only ever told "invalid credentials" (finding 46).
    pub async fn why_password_refused(&self, username: &str) -> &'static str {
        match self.store.password_login(username).await {
            Ok(None) => "unknown user",
            Ok(Some(account)) if account.disabled => "user disabled",
            Ok(Some(PasswordLogin { phc: None, .. })) => "no password",
            Ok(Some(_)) => "wrong password",
            Err(_) => "invalid credentials",
        }
    }

    /// A guess at a short code (a setup or device sign-in code) as `what`
    /// from `source`, counted as wrong before it is checked, as a password
    /// is: `Err(wait)` if the limit is reached. It stays counted unless
    /// [`Auth::guessed_right`] takes it back, so guesses sent at once
    /// cannot all be checked before the first has failed (A-13).
    pub fn guess(&self, what: String, source: Option<String>) -> Result<Guess, Duration> {
        let began = self.clock.now();
        self.limiter.begin(&what, source.as_deref(), began)?;
        Ok(Guess {
            what,
            source,
            began,
        })
    }

    /// `guess` was right: it is not counted. Nothing else is cleared: a
    /// right code says nothing about the guesses that came before it.
    pub fn guessed_right(&self, guess: Guess) {
        self.limiter
            .take_back(&guess.what, guess.source.as_deref(), guess.began);
    }

    /// The failure limit, for an attempt that is not a password login:
    /// `Some(wait)` if `what` from `source` is limited.
    pub fn attempt_limited(&self, what: &str, source: Option<&str>) -> Option<Duration> {
        self.limiter.check(what, source, self.clock.now())
    }

    /// How long `username`'s password logins stay refused for its own
    /// failed ones, if they are (P3-18).
    pub fn login_limited(&self, username: &str) -> Option<Duration> {
        self.limiter.user_limited(username, self.clock.now())
    }

    /// Count an attempt that is not a password login (a passkey's
    /// assertion) before it is checked, as [`RateLimiter::begin`] does:
    /// `Err(wait)` if `what` from `source` is limited. It stays counted as
    /// a failure unless [`Auth::attempt_succeeded`] takes it back.
    pub fn attempt_begin(&self, what: &str, source: Option<&str>) -> Result<Instant, Duration> {
        let began = self.clock.now();
        self.limiter.begin(what, source, began).map(|()| began)
    }

    pub fn attempt_succeeded(&self, what: &str, source: Option<&str>, began: Instant) {
        self.limiter.record_success(what, source, began);
    }

    /// Log in with a password and open a session bound as `binding`.
    /// `source` (an EndpointId, a remote address) is what the failure
    /// limit counts besides the username; `None` if it says nothing about
    /// who is trying.
    ///
    /// With `remember`, an endpoint session's device is trusted for the
    /// user (P4-01); an API session's never is.
    pub async fn login_password(
        &self,
        username: &str,
        password: &str,
        binding: SessionBinding,
        source: Option<&str>,
        remember: bool,
    ) -> Result<Login, LoginError> {
        let account = self.check_password(username, password, source).await?;
        let lifetime = match binding {
            SessionBinding::Endpoint { .. } => ENDPOINT_SESSION,
            SessionBinding::Api => API_SESSION_IDLE,
        };

        let token = SessionToken::generate();
        self.store
            .create_session(
                &token.hash(),
                account.id,
                &binding,
                self.store.now() + lifetime.as_secs() as i64,
            )
            .await?;
        let remembered = self
            .remembered(&token, &binding, account.id, remember)
            .await?;
        Ok(Login {
            user: account.id,
            role: account.role,
            token,
            remembered,
        })
    }

    /// After a sign-in that opened `token`, bound as `binding`, for `user`:
    /// trust its device if the user asked (`remember`), and say whether
    /// the device is trusted for them now (P4-01).
    pub async fn remembered(
        &self,
        token: &SessionToken,
        binding: &SessionBinding,
        user: LocalUserId,
        remember: bool,
    ) -> Result<bool, StoreError> {
        let SessionBinding::Endpoint { endpoint_id, .. } = binding else {
            return Ok(false);
        };
        if remember {
            self.remember(token).await
        } else {
            self.store.device_trusted(user, endpoint_id).await
        }
    }

    /// `username`'s account if `password` is theirs and they may log in,
    /// under the failure limit for `username` from `source`.
    pub async fn check_password(
        &self,
        username: &str,
        password: &str,
        source: Option<&str>,
    ) -> Result<PasswordLogin, LoginError> {
        // A name no user can have is not looked up, checked or counted:
        // the limiter keeps what it counts, and this is as long as the
        // caller likes.
        let Ok(username) = normalize_username(username) else {
            return Err(LoginError::InvalidCredentials);
        };
        let username = username.as_str();
        let account = self.store.password_login(username).await?;
        let (phc, usable) = match &account {
            Some(a) => (
                a.phc.clone().unwrap_or_else(decoy_hash),
                !a.disabled && a.phc.is_some(),
            ),
            None => (decoy_hash(), false),
        };
        // Counted as a failure before it is checked: logins sent at once
        // would otherwise all be checked against a limit none had yet
        // reached.
        let began = self.clock.now();
        if let Err(retry_after) = self.limiter.begin(username, source, began) {
            return Err(LoginError::RateLimited { retry_after });
        }
        let verified = verify_password(password, phc).await;
        match account {
            Some(account) if verified && usable => {
                self.limiter.record_success(username, source, began);
                Ok(account)
            }
            _ => Err(LoginError::InvalidCredentials),
        }
    }

    /// Open an API session for a user who just proved who they are some
    /// other way (the first-admin setup).
    pub async fn open_api_session(&self, user: LocalUserId) -> Result<SessionToken, StoreError> {
        let token = SessionToken::generate();
        self.store
            .create_session(
                &token.hash(),
                user,
                &SessionBinding::Api,
                self.store.now() + API_SESSION_IDLE.as_secs() as i64,
            )
            .await?;
        Ok(token)
    }

    /// The user behind an API session token, if it is live. Renews the
    /// session for [`API_SESSION_IDLE`], within [`API_SESSION`] of its
    /// sign-in.
    pub async fn api_session(&self, token: &str) -> Result<Option<ApiSessionUser>, StoreError> {
        self.store
            .use_api_session(
                &SessionToken(token.to_owned()).hash(),
                API_SESSION_IDLE.as_secs() as i64,
                API_SESSION.as_secs() as i64,
            )
            .await
    }

    /// The user behind an endpoint session token used from `endpoint_id`,
    /// if it is live there. Renews the session.
    pub async fn endpoint_session(
        &self,
        token: &str,
        endpoint_id: &str,
    ) -> Result<Option<SessionUser>, StoreError> {
        self.store
            .use_endpoint_session(
                &SessionToken(token.to_owned()).hash(),
                endpoint_id,
                ENDPOINT_SESSION.as_secs() as i64,
            )
            .await
    }

    /// Whose endpoint session token is, used from `endpoint_id`, if it
    /// is live there: a local user's ([`Auth::endpoint_session`]) or a
    /// granted portable user's (P7-06). Renews the session.
    pub async fn endpoint_who(
        &self,
        token: &str,
        endpoint_id: &str,
    ) -> Result<Option<crate::principal::Who>, StoreError> {
        use crate::principal::Who;
        if let Some(user) = self.endpoint_session(token, endpoint_id).await? {
            return Ok(Some(Who::Local(user)));
        }
        Ok(self
            .store
            .use_portable_session(
                &SessionToken(token.to_owned()).hash(),
                endpoint_id,
                ENDPOINT_SESSION.as_secs() as i64,
            )
            .await?
            .map(Who::Portable))
    }

    /// Open a session for granted portable user `credential.user` (P7-06)
    /// on `endpoint_id`, as an endpoint session is (same lifetime, renewed
    /// on use), never past `credential`'s expiry.
    pub async fn open_portable_session(
        &self,
        credential: &crate::store::grants::CredentialBinding,
        endpoint_id: &str,
        device_name: Option<&str>,
        platform: spoolframe_types::Platform,
    ) -> Result<SessionToken, StoreError> {
        let token = SessionToken::generate();
        self.store
            .create_portable_session(
                &token.hash(),
                credential,
                endpoint_id,
                device_name,
                platform,
                self.store.now() + ENDPOINT_SESSION.as_secs() as i64,
            )
            .await?;
        Ok(token)
    }

    /// Open a session for `user`, whose account is tied to the portable
    /// identity `credential` names, signed in with it (P7-06): as
    /// [`Auth::open_session`], but never past the credential's expiry,
    /// revoked with its key, and not a recent sign-in.
    pub async fn open_credential_session(
        &self,
        user: LocalUserId,
        binding: &SessionBinding,
        credential: &crate::store::grants::CredentialBinding,
    ) -> Result<SessionToken, StoreError> {
        let token = SessionToken::generate();
        self.store
            .create_credential_session(
                &token.hash(),
                user,
                binding,
                self.store.now() + ENDPOINT_SESSION.as_secs() as i64,
                credential,
            )
            .await?;
        Ok(token)
    }

    pub async fn logout(&self, token: &str) -> Result<(), StoreError> {
        self.store
            .revoke_session(&SessionToken(token.to_owned()).hash())
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::state_dir;

    mod api_sessions;
    mod device_sign_in;
    mod pins;

    /// Auth over a store in a state directory of its own, which goes
    /// with it.
    struct TestAuth {
        // Declared first, so the store closes before its directory goes.
        auth: Auth,
        _dir: spoolframe_test_dir::TestDir,
    }

    impl std::ops::Deref for TestAuth {
        type Target = Auth;

        fn deref(&self) -> &Auth {
            &self.auth
        }
    }

    async fn auth_with_alice(test: &str) -> TestAuth {
        let dir = state_dir(test).await;
        let store = Store::open(dir.as_ref()).await.unwrap();
        let phc = hash_password("correct horse").await.unwrap();
        store
            .add_user("alice", None, &phc, Role::Consumer)
            .await
            .unwrap();
        TestAuth {
            auth: Auth::new(store),
            _dir: dir,
        }
    }

    async fn login(auth: &Auth, user: &str, password: &str) -> Result<Login, LoginError> {
        auth.login_password(
            user,
            password,
            SessionBinding::Api,
            Some("192.0.2.1"),
            false,
        )
        .await
    }

    #[tokio::test]
    async fn the_right_password_logs_in() {
        let auth = auth_with_alice("right").await;
        let login = login(&auth, "Alice", "correct horse").await.unwrap();
        assert!(auth.api_session(&login.token.0).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_wrong_password_and_an_unknown_user_get_the_same_error() {
        let auth = auth_with_alice("wrong").await;
        assert!(matches!(
            login(&auth, "alice", "battery staple").await,
            Err(LoginError::InvalidCredentials)
        ));
        assert!(matches!(
            login(&auth, "mallory", "correct horse").await,
            Err(LoginError::InvalidCredentials)
        ));
    }

    #[tokio::test]
    async fn a_disabled_user_cannot_log_in() {
        let auth = auth_with_alice("disabled").await;
        auth.store().disable_user("alice").await.unwrap();
        assert!(matches!(
            login(&auth, "alice", "correct horse").await,
            Err(LoginError::InvalidCredentials)
        ));
    }

    #[tokio::test]
    async fn repeated_failures_are_rate_limited_even_with_the_right_password() {
        let auth = auth_with_alice("limit").await;
        for _ in 0..MAX_FAILURES {
            let _ = login(&auth, "alice", "nope nope").await;
        }
        assert!(matches!(
            login(&auth, "alice", "correct horse").await,
            Err(LoginError::RateLimited { .. })
        ));
    }

    #[tokio::test]
    async fn logins_sent_at_once_are_limited_as_if_sent_in_turn() {
        let auth = auth_with_alice("burst").await;
        let guesses: Vec<String> = (0..MAX_FAILURES + 3)
            .map(|i| format!("wrong guess {i}"))
            .collect();
        let answers = futures_util::future::join_all(
            guesses.iter().map(|guess| login(&auth, "alice", guess)),
        )
        .await;
        let checked = answers
            .iter()
            .filter(|answer| matches!(answer, Err(LoginError::InvalidCredentials)))
            .count();
        let refused = answers
            .iter()
            .filter(|answer| matches!(answer, Err(LoginError::RateLimited { .. })))
            .count();
        assert_eq!((checked, refused), (MAX_FAILURES, 3));
    }

    #[tokio::test]
    async fn pins_sent_at_once_are_limited_as_if_sent_in_turn() {
        let auth = auth_with_alice("pin-burst").await;
        let alice = auth.store().password_login("alice").await.unwrap().unwrap();
        let phc = hash_pin("4321").await.unwrap();
        auth.store().set_pin(alice.id, &phc, 4).await.unwrap();
        let guesses: Vec<String> = (0..MAX_FAILURES + 3).map(|i| format!("000{i}")).collect();
        let answers = futures_util::future::join_all(
            guesses
                .iter()
                .map(|guess| auth.check_pin(alice.id, "tv", guess)),
        )
        .await;
        let checked = answers
            .iter()
            .filter(|answer| matches!(answer, Err(UnlockError::Wrong)))
            .count();
        let refused = answers
            .iter()
            .filter(|answer| matches!(answer, Err(UnlockError::RateLimited { .. })))
            .count();
        assert_eq!((checked, refused), (MAX_FAILURES, 3));
    }

    #[tokio::test]
    async fn a_name_no_user_can_have_is_refused_and_not_kept() {
        let auth = auth_with_alice("long-name").await;
        for name in ["a".repeat(65), "x".repeat(1 << 20), "al ice".to_owned()] {
            assert!(matches!(
                login(&auth, &name, "correct horse").await,
                Err(LoginError::InvalidCredentials)
            ));
        }
        assert_eq!(auth.limiter.held(), 0);
    }

    #[tokio::test]
    async fn the_token_is_not_stored() {
        let auth = auth_with_alice("hashed").await;
        let login = login(&auth, "alice", "correct horse").await.unwrap();
        assert_ne!(login.token.hash(), login.token.0);
        assert!(
            auth.store()
                .api_session(&login.token.0)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            auth.store()
                .api_session(&login.token.hash())
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn logout_ends_the_session() {
        let auth = auth_with_alice("logout").await;
        let login = login(&auth, "alice", "correct horse").await.unwrap();
        auth.logout(&login.token.0).await.unwrap();
        assert!(auth.api_session(&login.token.0).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_short_password_is_refused() {
        assert!(matches!(
            hash_password("short").await,
            Err(PasswordError::TooShort)
        ));
    }

    #[test]
    fn the_limit_counts_per_username_and_per_source() {
        let limiter = RateLimiter::default();
        let t0 = Instant::now();
        for _ in 0..MAX_FAILURES {
            limiter.record_failure("alice", Some("a"), t0);
        }
        // Alice is limited from anywhere.
        assert!(limiter.check("alice", Some("b"), t0).is_some());
        // Source "a" is limited for anyone.
        assert!(limiter.check("bob", Some("a"), t0).is_some());
        // Others are not.
        assert!(limiter.check("bob", Some("b"), t0).is_none());
    }

    #[test]
    fn a_user_is_shown_limited_only_by_their_own_username_s_failures() {
        let limiter = RateLimiter::default();
        let t0 = Instant::now();
        for _ in 0..MAX_FAILURES {
            limiter.record_failure("Alice", Some("a"), t0);
        }
        let later = t0 + Duration::from_secs(60);
        assert_eq!(
            limiter.user_limited("alice", later),
            Some(FAILURE_WINDOW - Duration::from_secs(60))
        );
        // Bob shares only the source: he is refused from there, but it is
        // not his account that is limited.
        assert!(limiter.check("bob", Some("a"), later).is_some());
        assert_eq!(limiter.user_limited("bob", later), None);
        // One short of the limit is not limited.
        for _ in 1..MAX_FAILURES {
            limiter.record_failure("carol", None, t0);
        }
        assert_eq!(limiter.user_limited("carol", later), None);
        assert_eq!(limiter.user_limited("alice", t0 + FAILURE_WINDOW), None);
    }

    #[test]
    fn the_limit_lifts_after_the_window() {
        let limiter = RateLimiter::default();
        let t0 = Instant::now();
        for _ in 0..MAX_FAILURES {
            limiter.record_failure("alice", Some("a"), t0);
        }
        let wait = limiter.check("alice", Some("a"), t0).unwrap();
        assert_eq!(wait, FAILURE_WINDOW);
        assert!(
            limiter
                .check("alice", Some("a"), t0 + FAILURE_WINDOW)
                .is_none()
        );
    }

    #[test]
    fn failures_older_than_the_window_are_not_kept() {
        let limiter = RateLimiter::default();
        let t0 = Instant::now();
        for name in ["alice", "bob", "carol"] {
            limiter.record_failure(name, Some(name), t0);
        }
        assert_eq!(limiter.held(), 6);
        // Nobody asks about those names again; the next failure drops them.
        limiter.record_failure("dave", Some("d"), t0 + FAILURE_WINDOW);
        assert_eq!(limiter.held(), 2);
    }

    /// Every call used to walk every key. Between two sweeps a key nobody
    /// asks about is left alone, and still never counts once it is asked
    /// about.
    #[test]
    fn between_sweeps_only_the_keys_asked_about_are_looked_at() {
        let limiter = RateLimiter::default();
        let t0 = Instant::now();
        for _ in 0..MAX_FAILURES {
            limiter.record_failure("alice", Some("a"), t0);
        }
        // The last sweep before alice's failures are out of the window.
        let swept = t0 + FAILURE_WINDOW - Duration::from_secs(10);
        limiter.record_failure("bob", Some("b"), swept);
        let later = swept + Duration::from_secs(20);
        limiter.record_failure("carol", Some("c"), later);
        assert_eq!(limiter.held(), 6);
        assert!(limiter.check("alice", Some("a"), later).is_none());
        assert_eq!(limiter.held(), 4);
    }

    /// A limiter with room for `max_keys`.
    fn limiter_of(max_keys: usize) -> RateLimiter {
        RateLimiter {
            max_keys,
            ..RateLimiter::default()
        }
    }

    #[test]
    fn a_full_limiter_makes_room_from_its_oldest_keys() {
        let limiter = limiter_of(4);
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);
        for _ in 0..MAX_FAILURES {
            limiter.record_failure("alice", Some("a"), t0);
        }
        limiter.record_failure("bob", Some("b"), t1);
        // Carol and her source are counted, in place of alice's keys.
        assert_eq!(limiter.check("carol", Some("c"), t1), None);
        limiter.begin("carol", Some("c"), t1).unwrap();
        assert_eq!(limiter.held(), 4);
        assert!(limiter.check("alice", Some("x"), t1).is_none());
        // What it still holds is judged as ever.
        for _ in 1..MAX_FAILURES {
            limiter.record_failure("bob", Some("b"), t1);
        }
        assert!(limiter.check("bob", Some("y"), t1).is_some());
    }

    #[test]
    fn a_full_limiter_drops_expired_keys_before_live_ones() {
        let limiter = limiter_of(4);
        let t0 = Instant::now();
        limiter.record_failure("alice", Some("a"), t0);
        let later = t0 + FAILURE_WINDOW - Duration::from_secs(1);
        for _ in 0..MAX_FAILURES {
            limiter.record_failure("bob", Some("b"), later);
        }
        let now = t0 + FAILURE_WINDOW;
        limiter.record_failure("carol", Some("c"), now);
        assert!(limiter.check("bob", Some("z"), now).is_some());
    }

    #[test]
    fn the_stranger_bucket_gives_its_burst_then_one_per_interval() {
        let bucket = TokenBucket::new(Bucket {
            capacity: 2,
            refill_every: Duration::from_secs(1),
        });
        let t0 = Instant::now();
        assert!(bucket.take(t0).is_ok());
        assert!(bucket.take(t0).is_ok());
        assert!(
            matches!(bucket.take(t0), Err(wait) if wait <= Duration::from_secs(1) && !wait.is_zero())
        );
        let t1 = t0 + Duration::from_millis(1500);
        assert!(bucket.take(t1).is_ok());
        assert!(bucket.take(t1).is_err());
        // Idle, it fills back to its burst and no more.
        let t2 = t1 + Duration::from_secs(60);
        assert!(bucket.take(t2).is_ok());
        assert!(bucket.take(t2).is_ok());
        assert!(bucket.take(t2).is_err());
    }

    /// PX-09 finding 9 (A-4): endpoint ids are free, so anyone could fill
    /// the limiter, and a full one refused every new name and source.
    #[tokio::test]
    async fn a_full_limiter_still_lets_a_fresh_valid_login_in() {
        let test = auth_with_alice("full-limiter").await;
        let auth = Auth {
            limiter: Arc::new(limiter_of(20)),
            ..test.auth.clone()
        };
        // Names and sources are free: far more of them than it holds.
        for endpoint in 0..10 {
            let source = format!("endpoint-{endpoint}");
            for i in 0..MAX_FAILURES {
                let name = format!("user-{endpoint}-{i}");
                let _ = auth
                    .login_password(&name, "guess", SessionBinding::Api, Some(&source), false)
                    .await;
            }
        }
        let fresh = auth
            .login_password(
                "alice",
                "correct horse",
                SessionBinding::Api,
                Some("endpoint-fresh"),
                false,
            )
            .await;
        assert!(fresh.is_ok(), "{fresh:?}");
    }

    /// PX-09 finding 10 (A-5): logins sent at once each ran Argon2 at once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn argon2_runs_at_most_once_a_core_at_a_time() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let runs: Vec<_> = (0..3 * argon2_permits())
            .map(|_| {
                let (active, peak) = (active.clone(), peak.clone());
                tokio::spawn(argon2(move || {
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(20));
                    active.fetch_sub(1, Ordering::SeqCst);
                }))
            })
            .collect();
        for run in runs {
            run.await.unwrap().unwrap();
        }
        assert!(peak.load(Ordering::SeqCst) <= argon2_permits());
    }

    #[test]
    fn a_right_attempt_leaves_nothing_behind() {
        let limiter = RateLimiter::default();
        let t0 = Instant::now();
        limiter.begin("alice", Some("a"), t0).unwrap();
        limiter.record_success("alice", Some("a"), t0);
        assert_eq!(limiter.held(), 0);
    }

    #[test]
    fn success_clears_the_username_but_not_the_source() {
        let limiter = RateLimiter::default();
        let t0 = Instant::now();
        for _ in 0..MAX_FAILURES - 1 {
            limiter.record_failure("alice", Some("a"), t0);
        }
        limiter.begin("alice", Some("a"), t0).unwrap();
        // The source is at its limit while the attempt is being checked.
        assert!(limiter.check("bob", Some("a"), t0).is_some());
        limiter.record_success("alice", Some("a"), t0);
        assert!(limiter.check("alice", Some("b"), t0).is_none());
        // The right attempt is taken back; the wrong ones before it stay.
        assert!(limiter.check("bob", Some("a"), t0).is_none());
        limiter.record_failure("bob", Some("a"), t0);
        assert!(limiter.check("carol", Some("a"), t0).is_some());
    }

    /// A-13: a right code is not counted, and clears nothing: unlike a
    /// password, it says nothing about the guesses before it.
    #[test]
    fn a_right_guess_is_taken_back_and_clears_nothing() {
        let limiter = RateLimiter::default();
        let t0 = Instant::now();
        for _ in 0..MAX_FAILURES - 1 {
            limiter.begin("devcode:1", Some("a"), t0).unwrap();
        }
        let right = t0 + Duration::from_millis(1);
        limiter.begin("devcode:1", Some("a"), right).unwrap();
        limiter.take_back("devcode:1", Some("a"), right);
        // The four wrong ones stay, from this source and from another.
        limiter.begin("devcode:1", Some("a"), right).unwrap();
        assert!(limiter.begin("devcode:1", Some("b"), right).is_err());
        assert!(limiter.check("devcode:2", Some("a"), right).is_some());
    }
}
