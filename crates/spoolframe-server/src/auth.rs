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

use crate::store::sessions::{ApiSessionUser, SessionBinding, SessionUser};
use crate::store::users::{LocalUserId, PasswordLogin};
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
    fn generate() -> Self {
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
    (4..=8).contains(&pin.len()) && pin.bytes().all(|b| b.is_ascii_digit())
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

/// Failed logins, per username and per source, within a sliding window.
#[derive(Debug)]
pub struct RateLimiter {
    failures: Mutex<HashMap<String, VecDeque<Instant>>>,
    window: Duration,
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
        }
    }

    fn keys(username: &str, source: &str) -> [String; 2] {
        [
            format!("user:{}", username.trim().to_lowercase()),
            format!("source:{source}"),
        ]
    }

    /// `Some(wait)` if either key has used up its failures.
    pub fn check(&self, username: &str, source: &str, now: Instant) -> Option<Duration> {
        let mut failures = self.failures.lock().expect("never held across a panic");
        Self::keys(username, source)
            .iter()
            .filter_map(|key| {
                let times = failures.get_mut(key)?;
                while times
                    .front()
                    .is_some_and(|t| now.duration_since(*t) >= self.window)
                {
                    times.pop_front();
                }
                (times.len() >= MAX_FAILURES).then(|| self.window - now.duration_since(times[0]))
            })
            .max()
    }

    pub fn record_failure(&self, username: &str, source: &str, now: Instant) {
        let mut failures = self.failures.lock().expect("never held across a panic");
        for key in Self::keys(username, source) {
            failures.entry(key).or_default().push_back(now);
        }
    }

    /// A successful login clears the username's failures. The source's
    /// stay: one account's right password must not reset guessing at
    /// others from the same place.
    pub fn record_success(&self, username: &str, source: &str) {
        let mut failures = self.failures.lock().expect("never held across a panic");
        let [user, _] = Self::keys(username, source);
        failures.remove(&user);
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
        }
    }

    /// Unlock `user`'s profile on `endpoint` with `pin`. A user without a
    /// PIN has nothing to unlock. Wrong PINs are limited as passwords are,
    /// on keys of their own; [`MAX_WRONG_PINS`] in a row revoke the user's
    /// sessions on that device.
    pub async fn unlock(
        &self,
        user: LocalUserId,
        endpoint: &str,
        pin: &str,
    ) -> Result<(), UnlockError> {
        let (what, from) = pin_keys(user, endpoint);
        if let Some(retry_after) = self.limiter.check(&what, &from, Instant::now()) {
            return Err(UnlockError::RateLimited { retry_after });
        }
        let Some(phc) = self.store.pin_phc(user).await? else {
            return Ok(());
        };
        let key = (user, endpoint.to_owned());
        if valid_pin(pin) && verify_password(pin, phc).await {
            self.limiter.record_success(&what, &from);
            self.wrong_pins.lock().expect("never poisoned").remove(&key);
            return Ok(());
        }
        self.limiter.record_failure(&what, &from, Instant::now());
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
    pub fn attempt_limited(&self, what: &str, source: &str) -> Option<Duration> {
        self.limiter.check(what, source, Instant::now())
    }

    pub fn attempt_failed(&self, what: &str, source: &str) {
        self.limiter.record_failure(what, source, Instant::now());
    }

    /// Log in with a password and open a session bound as `binding`.
    /// `source` (an EndpointId, a remote address) is what the failure
    /// limit counts besides the username.
    pub async fn login_password(
        &self,
        username: &str,
        password: &str,
        binding: SessionBinding,
        source: &str,
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
        source: &str,
    ) -> Result<PasswordLogin, LoginError> {
        if let Some(retry_after) = self.limiter.check(username, source, Instant::now()) {
            return Err(LoginError::RateLimited { retry_after });
        }
        let account = self.store.password_login(username).await?;
        let (phc, usable) = match &account {
            Some(a) => (
                a.phc.clone().unwrap_or_else(decoy_hash),
                !a.disabled && a.phc.is_some(),
            ),
            None => (decoy_hash(), false),
        };
        let verified = verify_password(password, phc).await;
        let account = match account {
            Some(account) if verified && usable => account,
            _ => {
                self.limiter
                    .record_failure(username, source, Instant::now());
                return Err(LoginError::InvalidCredentials);
            }
        };
        self.limiter.record_success(username, source);
        Ok(account)
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
        auth.login_password(user, password, SessionBinding::Api, "127.0.0.1")
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
            limiter.record_failure("alice", "a", t0);
        }
        // Alice is limited from anywhere.
        assert!(limiter.check("alice", "b", t0).is_some());
        // Source "a" is limited for anyone.
        assert!(limiter.check("bob", "a", t0).is_some());
        // Others are not.
        assert!(limiter.check("bob", "b", t0).is_none());
    }

    #[test]
    fn the_limit_lifts_after_the_window() {
        let limiter = RateLimiter::default();
        let t0 = Instant::now();
        for _ in 0..MAX_FAILURES {
            limiter.record_failure("alice", "a", t0);
        }
        let wait = limiter.check("alice", "a", t0).unwrap();
        assert_eq!(wait, FAILURE_WINDOW);
        assert!(limiter.check("alice", "a", t0 + FAILURE_WINDOW).is_none());
    }

    #[test]
    fn success_clears_the_username_but_not_the_source() {
        let limiter = RateLimiter::default();
        let t0 = Instant::now();
        for _ in 0..MAX_FAILURES {
            limiter.record_failure("alice", "a", t0);
        }
        limiter.record_success("alice", "a");
        assert!(limiter.check("alice", "b", t0).is_none());
        assert!(limiter.check("bob", "a", t0).is_some());
    }
}
