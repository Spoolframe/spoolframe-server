//! The admin API server: JSON-RPC over WebSocket on TCP (ARCHITECTURE
//! §35), as the other foundry services serve theirs.
//!
//! Loopback by default: passwords and session tokens cross it in clear,
//! so reaching it from elsewhere means a TLS reverse proxy in front, and
//! an explicit `allow_remote`.

use std::net::SocketAddr;
use std::task::{Context, Poll};

use jsonrpsee::Extensions;
use jsonrpsee::core::{RpcResult, async_trait};
use jsonrpsee::server::{HttpRequest, Server, ServerHandle};
use jsonrpsee::types::ErrorObjectOwned;
use serde_json::json;
use spoolframe_admin_api::{
    AdminApiServer, Credentials, LoggedIn, NewUser, SetupRequest, UserInfo, error_code,
};
use tower::{Layer, Service};

use crate::auth::{Auth, LoginError, PasswordError, hash_password};
use crate::store::sessions::{ApiSessionUser, SessionBinding};
use crate::store::users::{UserError, normalize_username};

/// What the failure limit counts for admin logins besides the username.
/// The API listens on loopback, where the peer address says nothing.
const SOURCE: &str = "admin-api";

/// The bearer token of the WebSocket upgrade, if there was one.
#[derive(Debug, Clone)]
struct Bearer(String);

/// Start the admin API on `listen`.
/// The one-time code that `setup` needs: printed in the server's log at
/// start while there are no users, so only someone who can read that log
/// (the person installing the server) can claim the first admin.
#[derive(Clone, PartialEq, Eq)]
pub struct SetupCode(String);

impl std::fmt::Debug for SetupCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SetupCode(<redacted>)")
    }
}

impl std::fmt::Display for SetupCode {
    /// Grouped by four for reading aloud and typing: `K7QD-M2XP-9RTB`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let groups: Vec<&str> = (0..self.0.len())
            .step_by(4)
            .map(|i| &self.0[i..(i + 4).min(self.0.len())])
            .collect();
        f.write_str(&groups.join("-"))
    }
}

impl SetupCode {
    /// No 0/O, 1/I/L or U: nothing to misread.
    const ALPHABET: &'static [u8] = b"ABCDEFGHJKMNPQRSTVWXYZ23456789";
    const LEN: usize = 12;

    /// A fresh random code (about 59 bits).
    pub fn generate() -> Self {
        use password_hash::rand_core::{OsRng, RngCore};
        let code = (0..Self::LEN)
            .map(|_| {
                let i = OsRng.next_u32() as usize % Self::ALPHABET.len();
                Self::ALPHABET[i] as char
            })
            .collect();
        Self(code)
    }

    /// A code as given; dashes, spaces and case do not matter.
    pub fn new(code: &str) -> Self {
        Self(
            code.chars()
                .filter(|c| !matches!(c, '-' | ' '))
                .map(|c| c.to_ascii_uppercase())
                .collect(),
        )
    }

    /// Compared through hashes, so the time taken says nothing about how
    /// much of a guess was right.
    fn matches(&self, given: &str) -> bool {
        blake3::hash(self.0.as_bytes()) == blake3::hash(Self::new(given).0.as_bytes())
    }
}

/// Start the admin API on `listen`. `setup_code` is the code `setup`
/// accepts; `None` when the server already had users at start.
pub async fn serve(
    listen: SocketAddr,
    auth: Auth,
    setup_code: Option<SetupCode>,
) -> std::io::Result<(SocketAddr, ServerHandle)> {
    let server = Server::builder()
        .set_http_middleware(tower::ServiceBuilder::new().layer(BearerLayer))
        .build(listen)
        .await?;
    let addr = server.local_addr()?;
    let handle = server.start(AdminApi { auth, setup_code }.into_rpc());
    Ok((addr, handle))
}

struct AdminApi {
    auth: Auth,
    setup_code: Option<SetupCode>,
}

fn error(code: i32, message: impl Into<String>) -> ErrorObjectOwned {
    ErrorObjectOwned::owned::<()>(code, message.into(), None)
}

fn internal(e: impl std::fmt::Display) -> ErrorObjectOwned {
    tracing::error!(error = %e, "admin API");
    error(
        jsonrpsee::types::error::INTERNAL_ERROR_CODE,
        "internal error",
    )
}

impl From<LoginError> for ErrorObjectOwned {
    fn from(e: LoginError) -> Self {
        match e {
            LoginError::InvalidCredentials => {
                error(error_code::INVALID_CREDENTIALS, "invalid credentials")
            }
            LoginError::RateLimited { retry_after } => ErrorObjectOwned::owned(
                error_code::RATE_LIMITED,
                "too many failed logins",
                Some(json!({ "retryAfterMs": retry_after.as_millis() as u64 })),
            ),
            LoginError::Internal(e) => internal(e),
        }
    }
}

impl From<UserError> for ErrorObjectOwned {
    fn from(e: UserError) -> Self {
        match &e {
            UserError::Exists(_) => error(error_code::USER_EXISTS, e.to_string()),
            UserError::NoSuchUser(_) => error(error_code::NO_SUCH_USER, e.to_string()),
            UserError::InvalidUsername(_) => error(error_code::INVALID_INPUT, e.to_string()),
            UserError::AlreadySetUp => error(error_code::ALREADY_SET_UP, e.to_string()),
            UserError::Store(_) => internal(e),
        }
    }
}

impl From<PasswordError> for ErrorObjectOwned {
    fn from(e: PasswordError) -> Self {
        match e {
            PasswordError::TooShort => error(error_code::INVALID_INPUT, e.to_string()),
            PasswordError::Hash(_) => internal(e),
        }
    }
}

impl AdminApi {
    /// The caller's live session, checked on every call so a revoked
    /// session or a disabled user stops at once.
    async fn caller(&self, ext: &Extensions) -> RpcResult<(String, ApiSessionUser)> {
        let Some(Bearer(token)) = ext.get::<Bearer>() else {
            return Err(error(error_code::UNAUTHENTICATED, "log in first"));
        };
        match self.auth.api_session(token).await.map_err(internal)? {
            Some(user) => Ok((token.clone(), user)),
            None => Err(error(error_code::UNAUTHENTICATED, "session is not valid")),
        }
    }

    async fn admin(&self, ext: &Extensions) -> RpcResult<ApiSessionUser> {
        let (_, user) = self.caller(ext).await?;
        if user.is_admin {
            Ok(user)
        } else {
            Err(error(error_code::FORBIDDEN, "admins only"))
        }
    }

    async fn user_info(&self, username: &str) -> RpcResult<UserInfo> {
        let name = normalize_username(username)?;
        let users = self.auth.store().list_users().await?;
        users
            .into_iter()
            .find(|u| u.username == name)
            .map(|u| UserInfo {
                username: u.username,
                display_name: u.display_name,
                is_admin: u.is_admin,
                created_at: u.created_at,
                disabled: u.disabled_at.is_some(),
                has_password: u.has_password,
            })
            .ok_or_else(|| UserError::NoSuchUser(name).into())
    }
}

#[async_trait]
impl AdminApiServer for AdminApi {
    async fn setup(&self, request: SetupRequest) -> RpcResult<LoggedIn> {
        let Some(code) = &self.setup_code else {
            return Err(UserError::AlreadySetUp.into());
        };
        if let Some(retry_after) = self.auth.attempt_limited("setup", SOURCE) {
            return Err(LoginError::RateLimited { retry_after }.into());
        }
        if !code.matches(&request.code) {
            self.auth.attempt_failed("setup", SOURCE);
            return Err(error(
                error_code::INVALID_SETUP_CODE,
                "that is not the setup code in the server's log",
            ));
        }
        let phc = hash_password(&request.password).await?;
        let user = self
            .auth
            .store()
            .add_first_admin(&request.username, &phc)
            .await?;
        let token = self.auth.open_api_session(user).await.map_err(internal)?;
        tracing::info!(username = %request.username, "first admin created");
        Ok(LoggedIn {
            token: token.0,
            username: normalize_username(&request.username)?,
            is_admin: true,
        })
    }

    async fn login(&self, credentials: Credentials) -> RpcResult<LoggedIn> {
        let login = self
            .auth
            .login_password(
                &credentials.username,
                &credentials.password,
                SessionBinding::Api,
                SOURCE,
            )
            .await?;
        Ok(LoggedIn {
            token: login.token.0,
            username: normalize_username(&credentials.username)?,
            is_admin: login.is_admin,
        })
    }

    async fn logout(&self, ext: &Extensions) -> RpcResult<()> {
        let (token, _) = self.caller(ext).await?;
        self.auth.logout(&token).await.map_err(internal)
    }

    async fn user_list(&self, ext: &Extensions) -> RpcResult<Vec<UserInfo>> {
        self.admin(ext).await?;
        Ok(self
            .auth
            .store()
            .list_users()
            .await?
            .into_iter()
            .map(|u| UserInfo {
                username: u.username,
                display_name: u.display_name,
                is_admin: u.is_admin,
                created_at: u.created_at,
                disabled: u.disabled_at.is_some(),
                has_password: u.has_password,
            })
            .collect())
    }

    async fn user_add(&self, ext: &Extensions, user: NewUser) -> RpcResult<UserInfo> {
        self.admin(ext).await?;
        let phc = hash_password(&user.password).await?;
        self.auth
            .store()
            .add_user(
                &user.username,
                user.display_name.as_deref(),
                &phc,
                user.admin,
            )
            .await?;
        tracing::info!(username = %user.username, admin = user.admin, "user added");
        self.user_info(&user.username).await
    }

    async fn user_set_password(&self, ext: &Extensions, credentials: Credentials) -> RpcResult<()> {
        self.admin(ext).await?;
        let phc = hash_password(&credentials.password).await?;
        self.auth
            .store()
            .set_password(&credentials.username, &phc)
            .await?;
        Ok(())
    }

    async fn user_disable(&self, ext: &Extensions, username: String) -> RpcResult<u64> {
        self.admin(ext).await?;
        let revoked = self.auth.store().disable_user(&username).await?;
        tracing::info!(%username, revoked, "user disabled");
        Ok(revoked)
    }

    async fn user_enable(&self, ext: &Extensions, username: String) -> RpcResult<()> {
        self.admin(ext).await?;
        Ok(self.auth.store().enable_user(&username).await?)
    }
}

/// Puts the upgrade's bearer token, if any, into the request extensions,
/// where methods find it. It refuses nothing: `setup` and `login` are
/// called without one.
#[derive(Debug, Clone)]
struct BearerLayer;

impl<S> Layer<S> for BearerLayer {
    type Service = BearerService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        BearerService { inner }
    }
}

#[derive(Debug, Clone)]
struct BearerService<S> {
    inner: S,
}

impl<S, B> Service<HttpRequest<B>> for BearerService<S>
where
    S: Service<HttpRequest<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: HttpRequest<B>) -> Self::Future {
        let bearer = req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|t| Bearer(t.trim().to_owned()));
        if let Some(bearer) = bearer {
            req.extensions_mut().insert(bearer);
        }
        self.inner.call(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_reads_in_groups_and_matches_however_it_is_typed() {
        let code = SetupCode::new("K7QDM2XP9RTB");
        assert_eq!(code.to_string(), "K7QD-M2XP-9RTB");
        assert!(code.matches("k7qd-m2xp-9rtb"));
        assert!(code.matches("K7QD M2XP 9RTB"));
        assert!(!code.matches("K7QD-M2XP-9RTC"));
        assert!(!code.matches(""));
    }

    #[test]
    fn generated_codes_use_the_unambiguous_alphabet() {
        let code = SetupCode::generate();
        assert_eq!(code.0.len(), SetupCode::LEN);
        assert!(code.0.bytes().all(|b| SetupCode::ALPHABET.contains(&b)));
        assert_ne!(SetupCode::generate(), code);
    }
}
