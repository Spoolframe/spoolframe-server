//! Password login (ARCHITECTURE §22), the sessions it creates, and the
//! limit on failed attempts.
//!
//! A login ends at a [`LocalUserId`]; the session does not record how it
//! was reached (§24B), so other methods (OIDC, P4) plug in beside this one.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use password_hash::rand_core::{OsRng, RngCore};

use spoolframe_types::Role;

use crate::device_sign_in::{Claim, DeviceSignIns};
use crate::store::sessions::{ApiSessionUser, SessionBinding, SessionUser};
use crate::store::users::{LocalUserId, PasswordLogin, normalize_username};
use crate::store::{Store, StoreError, now};

/// How long a client (Iroh endpoint) session lasts after its last use.
/// Use renews it, so a TV in use never signs out.
pub const ENDPOINT_SESSION: Duration = Duration::from_secs(90 * 24 * 3600);
/// How long an admin API session lasts.
pub const API_SESSION: Duration = Duration::from_secs(12 * 3600);

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
}

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
pub fn pin_digits(pin: &str) -> Option<u8> {
    let digits = u8::try_from(pin.len()).ok()?;
    ((4..=8).contains(&digits) && pin.bytes().all(|b| b.is_ascii_digit())).then_some(digits)
}

/// Hash a new PIN as passwords are (Argon2id, PHC string).
pub async fn hash_pin(pin: &str) -> Result<String, PasswordError> {
    hash_secret(pin).await
}

async fn hash_secret(secret: &str) -> Result<String, PasswordError> {
    let password = secret.to_owned();
    tokio::task::spawn_blocking(move || {
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
    tokio::task::spawn_blocking(move || {
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
    (format!("pin:{}", user.0), format!("pin@{source}"))
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
/// [`SWEEP_INTERVAL`], and at most [`MAX_KEYS`] are held. While it is
/// full, an attempt under a key it does not hold is refused as limited:
/// there is no room to count it, and an attempt that cannot be counted
/// must not be let through.
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
}

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
        self.failures.lock().unwrap().by_key.len()
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

    /// `Some(wait)` if either key has used up its failures, or the limiter
    /// is full and does not hold one of them.
    pub fn check(&self, username: &str, source: Option<&str>, now: Instant) -> Option<Duration> {
        let mut failures = self.failures.lock().expect("never held across a panic");
        self.limited(&mut failures, username, source, now)
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

    fn limited(
        &self,
        failures: &mut Failures,
        username: &str,
        source: Option<&str>,
        now: Instant,
    ) -> Option<Duration> {
        self.sweep(failures, now);
        let mut wait = None;
        let mut new = 0;
        for key in Self::keys(username, source) {
            self.prune(failures, &key, now);
            match failures.by_key.get(&key) {
                Some(times) if times.len() >= MAX_FAILURES => {
                    wait = wait.max(Some(self.window - now.duration_since(times[0])));
                }
                Some(_) => {}
                None => new += 1,
            }
        }
        if wait.is_none() && failures.by_key.len() + new > self.max_keys {
            // Room comes with a sweep.
            return Some(SWEEP_INTERVAL);
        }
        wait
    }

    /// Count a failure. While the limiter is full, not under a key it does
    /// not hold: [`RateLimiter::check`] refuses such an attempt before it
    /// is made.
    pub fn record_failure(&self, username: &str, source: Option<&str>, now: Instant) {
        let mut failures = self.failures.lock().expect("never held across a panic");
        self.sweep(&mut failures, now);
        for key in Self::keys(username, source) {
            self.prune(&mut failures, &key, now);
            if failures.by_key.contains_key(&key) || failures.by_key.len() < self.max_keys {
                failures.by_key.entry(key).or_default().push_back(now);
            }
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
        let mut failures = self.failures.lock().expect("never held across a panic");
        if let Some(wait) = self.limited(&mut failures, username, source, now) {
            return Err(wait);
        }
        for key in Self::keys(username, source) {
            failures.by_key.entry(key).or_default().push_back(now);
        }
        Ok(())
    }

    /// The attempt begun at `began` was right: it is taken back, and the
    /// username's failures are cleared. The source's other failures stay:
    /// one account's right password must not reset guessing at others from
    /// the same place.
    pub fn record_success(&self, username: &str, source: Option<&str>, began: Instant) {
        let mut failures = self.failures.lock().expect("never held across a panic");
        failures.by_key.remove(&Self::user_key(username));
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

/// Logins and sessions.
#[derive(Debug, Clone)]
pub struct Auth {
    store: Store,
    limiter: Arc<RateLimiter>,
    /// Wrong PINs in a row, per user and device.
    wrong_pins: Arc<Mutex<HashMap<(LocalUserId, String), u32>>>,
    /// Devices waiting to be approved on another (P3-07).
    sign_ins: Arc<DeviceSignIns>,
}

impl Auth {
    pub fn new(store: Store) -> Self {
        Self::with_failure_window(store, FAILURE_WINDOW)
    }

    /// As [`Auth::new`], with failures counting for `window`.
    pub fn with_failure_window(store: Store, window: Duration) -> Self {
        Self {
            store,
            limiter: Arc::new(RateLimiter::with_window(window)),
            wrong_pins: Arc::default(),
            sign_ins: Arc::default(),
        }
    }

    /// Device sign-in codes live `lifetime` (tests shorten it).
    pub fn with_sign_in_lifetime(mut self, lifetime: Duration) -> Self {
        self.sign_ins = Arc::new(DeviceSignIns::with_lifetime(lifetime));
        self
    }

    pub fn sign_ins(&self) -> &DeviceSignIns {
        &self.sign_ins
    }

    /// Open a session on the device `claim` names, as `user`, who approved
    /// it from a session of their own (P3-07): that session is the proof.
    /// The device gets the token through [`DeviceSignIns::fulfil`].
    pub async fn approve_device(
        &self,
        user: LocalUserId,
        claim: &Claim,
    ) -> Result<SessionToken, StoreError> {
        let token = SessionToken::generate();
        let binding = SessionBinding::Endpoint {
            endpoint_id: claim.endpoint.clone(),
            device_name: claim.device_name.clone(),
            platform: claim.platform,
        };
        self.store
            .create_session(
                &token.hash(),
                user,
                &binding,
                now() + ENDPOINT_SESSION.as_secs() as i64,
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
        let began = Instant::now();
        if let Err(retry_after) = self.limiter.begin(&what, Some(&from), began) {
            return Err(UnlockError::RateLimited { retry_after });
        }
        let key = (user, endpoint.to_owned());
        if valid_pin(pin) && verify_password(pin, phc).await {
            self.limiter.record_success(&what, Some(&from), began);
            self.wrong_pins.lock().expect("never poisoned").remove(&key);
            return Ok(());
        }
        let wrong = {
            let mut counts = self.wrong_pins.lock().expect("never poisoned");
            let count = counts.entry(key.clone()).or_default();
            *count += 1;
            *count
        };
        if wrong >= MAX_WRONG_PINS {
            self.wrong_pins.lock().expect("never poisoned").remove(&key);
            let revoked = self.store.revoke_device_sessions(user, endpoint).await?;
            tracing::warn!(
                user = user.0,
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

    /// The failure limit, for an attempt that is not a password login
    /// (a setup code): `Some(wait)` if `what` from `source` is limited.
    pub fn attempt_limited(&self, what: &str, source: Option<&str>) -> Option<Duration> {
        self.limiter.check(what, source, Instant::now())
    }

    pub fn attempt_failed(&self, what: &str, source: Option<&str>) {
        self.limiter.record_failure(what, source, Instant::now());
    }

    /// Count an attempt that is not a password login (a passkey's
    /// assertion) before it is checked, as [`RateLimiter::begin`] does:
    /// `Err(wait)` if `what` from `source` is limited. It stays counted as
    /// a failure unless [`Auth::attempt_succeeded`] takes it back.
    pub fn attempt_begin(&self, what: &str, source: Option<&str>) -> Result<Instant, Duration> {
        let began = Instant::now();
        self.limiter.begin(what, source, began).map(|()| began)
    }

    pub fn attempt_succeeded(&self, what: &str, source: Option<&str>, began: Instant) {
        self.limiter.record_success(what, source, began);
    }

    /// Log in with a password and open a session bound as `binding`.
    /// `source` (an EndpointId, a remote address) is what the failure
    /// limit counts besides the username; `None` if it says nothing about
    /// who is trying.
    pub async fn login_password(
        &self,
        username: &str,
        password: &str,
        binding: SessionBinding,
        source: Option<&str>,
    ) -> Result<Login, LoginError> {
        let account = self.check_password(username, password, source).await?;
        let lifetime = match binding {
            SessionBinding::Endpoint { .. } => ENDPOINT_SESSION,
            SessionBinding::Api => API_SESSION,
        };

        let token = SessionToken::generate();
        self.store
            .create_session(
                &token.hash(),
                account.id,
                &binding,
                now() + lifetime.as_secs() as i64,
            )
            .await?;
        Ok(Login {
            user: account.id,
            role: account.role,
            token,
        })
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
        let began = Instant::now();
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
                now() + API_SESSION.as_secs() as i64,
            )
            .await?;
        Ok(token)
    }

    /// The user behind an API session token, if it is live.
    pub async fn api_session(&self, token: &str) -> Result<Option<ApiSessionUser>, StoreError> {
        self.store
            .api_session(&SessionToken(token.to_owned()).hash())
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
        auth.login_password(user, password, SessionBinding::Api, Some("192.0.2.1"))
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
    fn a_full_limiter_refuses_what_it_cannot_count_and_judges_what_it_holds() {
        let limiter = limiter_of(4);
        let t0 = Instant::now();
        limiter.record_failure("alice", Some("a"), t0);
        limiter.record_failure("bob", Some("b"), t0);
        // No room for carol's name or her source.
        assert_eq!(limiter.check("carol", Some("c"), t0), Some(SWEEP_INTERVAL));
        assert!(limiter.begin("carol", Some("a"), t0).is_err());
        limiter.record_failure("carol", Some("c"), t0);
        assert_eq!(limiter.held(), 4);
        // The names and sources it holds are judged as ever.
        assert!(limiter.check("alice", Some("b"), t0).is_none());
        limiter.begin("alice", Some("a"), t0).unwrap();
        // With the old failures gone there is room again.
        let later = t0 + FAILURE_WINDOW;
        assert!(limiter.check("carol", Some("c"), later).is_none());
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
}
