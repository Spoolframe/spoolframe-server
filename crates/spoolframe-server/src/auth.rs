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

use crate::store::sessions::{ApiSessionUser, SessionBinding, SessionUser};
use crate::store::users::LocalUserId;
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
    pub is_admin: bool,
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
    let password = password.to_owned();
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

/// Failed logins, per username and per source, within a sliding window.
#[derive(Debug, Default)]
pub struct RateLimiter {
    failures: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl RateLimiter {
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
                    .is_some_and(|t| now.duration_since(*t) >= FAILURE_WINDOW)
                {
                    times.pop_front();
                }
                (times.len() >= MAX_FAILURES).then(|| FAILURE_WINDOW - now.duration_since(times[0]))
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

/// Logins and sessions.
#[derive(Debug, Clone)]
pub struct Auth {
    store: Store,
    limiter: Arc<RateLimiter>,
}

impl Auth {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            limiter: Arc::default(),
        }
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
            is_admin: account.is_admin,
            token,
        })
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

    async fn auth_with_alice(test: &str) -> Auth {
        let store = Store::open(&state_dir(test)).await.unwrap();
        let phc = hash_password("correct horse").await.unwrap();
        store.add_user("alice", None, &phc, false).await.unwrap();
        Auth::new(store)
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
