//! The admin API server: JSON-RPC over WebSocket on TCP (ARCHITECTURE
//! §35), as the other foundry services serve theirs. The listener's HTTP
//! side (the browser's sign-in, the pages) is `crate::admin_http`.
//!
//! Loopback by default: passwords and session tokens cross it in clear,
//! so reaching it from elsewhere means a TLS reverse proxy in front, and
//! an explicit `allow_remote`.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, OnceLock};

use jsonrpsee::Extensions;
use jsonrpsee::core::{RpcResult, async_trait};
use jsonrpsee::server::ServerHandle;
use jsonrpsee::types::ErrorObjectOwned;
use serde_json::json;
use spoolframe_admin_api::{
    AdminApiServer, Credentials, DeviceEntry, DeviceSignIn, LibraryStatus, LoggedIn, MeApiServer,
    NewUser, PasskeyChallenge, PasskeyEntry, PasskeyHost, PasskeySetupBegin, PasskeySetupFinish,
    PasswordChange, PathFailures, PathReport, PlexStatus, PlexSyncReport, Role, ServerStatus,
    SessionEntry, SetupRequest, TranscodingStatus, UserInfo, error_code,
};
use spoolframe_catalog::Catalog;
use spoolframe_plex::PlexCatalog;
use spoolframe_plex::check::Scope;

pub use crate::admin_http::{Origins, TrustedProxies};
use crate::auth::{
    Auth, LoginError, PasswordError, RECENT_SIGN_IN, SessionToken, hash_password, hash_pin,
    valid_pin,
};
use crate::device_sign_in::ClaimError;
use crate::passkeys::{CeremonyError, Passkeys, SignInError};
use crate::plex_sync::{PlexSync, SyncError};
use crate::store::passkeys::{FirstPasskey, PasskeyError, PasskeyRecord, passkey_name};
use crate::store::pins::{DeviceRecord, PinError};
use crate::store::sessions::{ApiSessionUser, SessionBinding, SessionRecord};
use crate::store::users::{UserError, UserRecord, normalize_username};
use crate::transcode::Transcoding;

/// The address the caller is at: its connection's, or behind a trusted
/// proxy the one the proxy forwards for, `None` if the proxy named none
/// ([`TrustedProxies::caller`]).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Peer(pub(crate) Option<IpAddr>);

impl Peer {
    /// What the failure limit counts for this caller besides the username:
    /// its address. A loopback address is shared by everyone who comes
    /// through this machine (the CLI, an SSH tunnel, a proxy), so it names
    /// no one: counting it would let any of them lock all the others out.
    /// So does a trusted proxy that did not say whom it carries.
    fn source(self) -> Option<String> {
        let ip = self.0?.to_canonical();
        (!ip.is_loopback()).then(|| ip.to_string())
    }
}

/// The failure limit's source for the caller.
fn source(ext: &Extensions) -> RpcResult<Option<String>> {
    match ext.get::<Peer>() {
        Some(peer) => Ok(peer.source()),
        None => Err(internal("a request without its peer address")),
    }
}

/// The caller's session token, from the upgrade's Bearer header or the
/// browser's cookie (`crate::admin_http`), if there was one.
#[derive(Debug, Clone)]
pub(crate) struct Bearer(pub(crate) String);

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

/// What `serverStatus` reports. The endpoint starts after the admin API,
/// so its key is filled in when it is known.
#[derive(Debug)]
pub struct StatusSource {
    pub name: String,
    pub transcoding: Arc<Transcoding>,
    /// Whether the endpoint announces itself on the LAN.
    pub mdns: bool,
    endpoint_id: OnceLock<String>,
    addresses: OnceLock<Vec<String>>,
}

impl StatusSource {
    pub fn new(name: impl Into<String>, transcoding: Arc<Transcoding>) -> Self {
        Self {
            name: name.into(),
            transcoding,
            mdns: false,
            endpoint_id: OnceLock::new(),
            addresses: OnceLock::new(),
        }
    }

    pub fn with_mdns(mut self, mdns: bool) -> Self {
        self.mdns = mdns;
        self
    }

    pub fn set_endpoint_id(&self, id: String) {
        let _ = self.endpoint_id.set(id);
    }

    /// Where the endpoint listens, once it does.
    pub fn set_addresses(&self, addresses: Vec<String>) {
        let _ = self.addresses.set(addresses);
    }

    fn status(&self) -> ServerStatus {
        let encoders = self.transcoding.encoders();
        ServerStatus {
            name: self.name.clone(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            endpoint_id: self.endpoint_id.get().cloned(),
            fingerprint: self
                .endpoint_id
                .get()
                .and_then(|id| spoolframe_protocol::fingerprint::of_hex(id)),
            transcoding: TranscodingStatus {
                video_encoders: encoders.video.iter().map(|e| e.name.clone()).collect(),
                audio_encoders: encoders.audio.iter().map(|e| e.name.clone()).collect(),
                hardware_decoders: encoders
                    .hardware_decoders
                    .iter()
                    .map(|d| format!("{} ({:?})", d.codec, d.hardware).to_lowercase())
                    .collect(),
            },
            mdns: self.mdns,
            addresses: self.addresses.get().cloned().unwrap_or_default(),
        }
    }
}

/// What the admin API acts on.
pub struct Admin {
    pub auth: Auth,
    /// The code `setup` accepts; `None` when the server had users at start.
    pub setup_code: Option<SetupCode>,
    pub status: Arc<StatusSource>,
    pub plex_sync: Option<Arc<PlexSync>>,
    /// For `plexStatus` and `plexCheckPaths`; `None` in tests without one.
    pub plex: Option<Arc<PlexCatalog>>,
    /// The relying party passkeys are bound to (P3-05).
    pub passkeys: Arc<Passkeys>,
}

/// Start the admin API on `listen`. Browsers are let in from `origins`;
/// `proxies` are believed about whom they carry.
pub async fn serve(
    listen: SocketAddr,
    admin: Admin,
    origins: Origins,
    proxies: TrustedProxies,
) -> std::io::Result<(SocketAddr, ServerHandle)> {
    let api = AdminApi {
        auth: admin.auth,
        setup_code: admin.setup_code,
        status: admin.status,
        plex_sync: admin.plex_sync,
        plex: admin.plex,
        passkeys: admin.passkeys,
        public_url: origins.public_url().map(str::to_owned),
    };
    let mut methods = AdminApiServer::into_rpc(api.clone());
    methods
        .merge(MeApiServer::into_rpc(api.clone()))
        .expect("the me and admin namespaces never share a name");
    crate::admin_http::serve(listen, api, methods.into(), origins, proxies).await
}

#[derive(Clone)]
pub(crate) struct AdminApi {
    auth: Auth,
    setup_code: Option<SetupCode>,
    status: Arc<StatusSource>,
    plex_sync: Option<Arc<PlexSync>>,
    plex: Option<Arc<PlexCatalog>>,
    passkeys: Arc<Passkeys>,
    /// Where browsers reach the web UI (`admin.public_url`), if anywhere.
    public_url: Option<String>,
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
            UserError::LastAdmin(_) => error(error_code::LAST_ADMIN, e.to_string()),
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

impl From<PasskeyError> for ErrorObjectOwned {
    fn from(e: PasskeyError) -> Self {
        match &e {
            PasskeyError::NoSuchPasskey => error(error_code::NO_SUCH_PASSKEY, e.to_string()),
            PasskeyError::Exists | PasskeyError::InvalidName => {
                error(error_code::INVALID_INPUT, e.to_string())
            }
            PasskeyError::LastSignInMethod => error(error_code::LAST_SIGN_IN_METHOD, e.to_string()),
            PasskeyError::Store(_) => internal(e),
        }
    }
}

impl From<SignInError> for ErrorObjectOwned {
    fn from(e: SignInError) -> Self {
        match e {
            SignInError::Refused => error(error_code::PASSKEY_REFUSED, e.to_string()),
            SignInError::RateLimited { retry_after } => {
                LoginError::RateLimited { retry_after }.into()
            }
            SignInError::Unavailable(why) => error(error_code::NOT_CONFIGURED, why),
            SignInError::Internal(e) => internal(e),
        }
    }
}

impl From<CeremonyError> for ErrorObjectOwned {
    fn from(e: CeremonyError) -> Self {
        match e {
            CeremonyError::Unavailable(why) => error(error_code::NOT_CONFIGURED, why),
            CeremonyError::NoCeremony | CeremonyError::Refused(_) => {
                error(error_code::INVALID_INPUT, e.to_string())
            }
            CeremonyError::Passkey(e) => e.into(),
            CeremonyError::Internal(e) => internal(e),
        }
    }
}

impl AdminApi {
    /// What a signed-out page shows and says: the server's name, and the
    /// public URL to send a browser to when its own address cannot keep
    /// the session cookie (plain HTTP on a real host).
    pub(crate) fn site(&self) -> serde_json::Value {
        json!({ "name": self.status.name, "publicUrl": self.public_url })
    }

    /// Why the server cannot offer passkeys, if it cannot.
    pub(crate) fn passkeys_unavailable(&self) -> Option<&str> {
        self.passkeys.unavailable()
    }

    /// Whether `setup` would take a code now: no users, and a code was
    /// made at start. What a browser's first page decides on.
    pub(crate) async fn setup_open(&self) -> RpcResult<bool> {
        if self.setup_code.is_none() {
            return Ok(false);
        }
        Ok(self.auth.store().user_count().await? == 0)
    }

    /// Whether `given` is the setup code, while setup can be done at all;
    /// guesses are limited as failed logins are, under one name.
    fn setup_code(&self, ext: &Extensions, given: &str) -> RpcResult<()> {
        let Some(code) = &self.setup_code else {
            return Err(UserError::AlreadySetUp.into());
        };
        let source = source(ext)?;
        if let Some(retry_after) = self.auth.attempt_limited("setup", source.as_deref()) {
            return Err(LoginError::RateLimited { retry_after }.into());
        }
        if !code.matches(given) {
            self.auth.attempt_failed("setup", source.as_deref());
            return Err(error(
                error_code::INVALID_SETUP_CODE,
                "that is not the setup code in the server's log",
            ));
        }
        Ok(())
    }

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

    /// The caller, if an admin. The role is the user's now, not when the
    /// session began: a demoted admin's next `admin.*` call is refused.
    async fn admin(&self, ext: &Extensions) -> RpcResult<ApiSessionUser> {
        let (_, user) = self.caller(ext).await?;
        if user.role.is_admin() {
            Ok(user)
        } else {
            Err(error(error_code::FORBIDDEN, "admins only"))
        }
    }

    /// The caller, if they proved who they are in this session within
    /// [`RECENT_SIGN_IN`] (P3-03): what a sensitive change needs, so a
    /// session left open, or its stolen cookie, cannot make one.
    async fn recently_signed_in(&self, ext: &Extensions) -> RpcResult<(String, ApiSessionUser)> {
        let (token, user) = self.caller(ext).await?;
        let age = crate::store::now() - user.authenticated_at;
        if age > RECENT_SIGN_IN.as_secs() as i64 {
            return Err(error(
                error_code::RECENT_SIGN_IN_REQUIRED,
                "confirm it is you: enter your password, or sign in again",
            ));
        }
        Ok((token, user))
    }

    /// The caller and `typed` as a device sign-in code (P3-07), if they
    /// may try one now. Any live web session may: it was made by a full
    /// sign-in, and the PIN lock (P2-08) is a device's, which a browser is
    /// not. A malformed code is a wrong one.
    async fn sign_in_code(
        &self,
        ext: &Extensions,
        typed: &str,
    ) -> RpcResult<(ApiSessionUser, String)> {
        let (_, user) = self.caller(ext).await?;
        let (what, from) = sign_in_code_keys(ext, user)?;
        if let Some(retry_after) = self.auth.attempt_limited(&what, from.as_deref()) {
            return Err(LoginError::RateLimited { retry_after }.into());
        }
        match spoolframe_protocol::device_sign_in::normalize(typed) {
            Some(code) => Ok((user, code)),
            None => Err(self.wrong_sign_in_code(ext, user)?),
        }
    }

    /// Count a code that is not pending against `user` and where they
    /// are: codes are short, so guessing them is limited as passwords are.
    /// The answer for such a code; `Err` only if the request has no peer.
    fn wrong_sign_in_code(
        &self,
        ext: &Extensions,
        user: ApiSessionUser,
    ) -> RpcResult<ErrorObjectOwned> {
        let (what, from) = sign_in_code_keys(ext, user)?;
        self.auth.attempt_failed(&what, from.as_deref());
        Ok(error(
            error_code::NO_SUCH_SIGN_IN_CODE,
            "that is not a sign-in code a device is showing now",
        ))
    }

    /// The account `user` is.
    async fn account(&self, user: ApiSessionUser) -> RpcResult<UserRecord> {
        self.auth
            .store()
            .user_by_id(user.user)
            .await?
            .ok_or_else(|| error(error_code::UNAUTHENTICATED, "session is not valid"))
    }

    /// A stored passkey as the API shows it.
    fn passkey_entry(&self, p: PasskeyRecord) -> PasskeyEntry {
        PasskeyEntry {
            usable: self.passkeys.rp_id() == Some(p.rp_id.as_str()),
            id: p.credential_id,
            name: p.name,
            created_at: p.created_at,
            last_used_at: p.last_used_at,
            rp_id: p.rp_id,
        }
    }

    async fn passkey_entries(
        &self,
        user: crate::store::users::LocalUserId,
    ) -> RpcResult<Vec<PasskeyEntry>> {
        let passkeys = self
            .auth
            .store()
            .list_passkeys(user)
            .await
            .map_err(internal)?;
        Ok(passkeys
            .into_iter()
            .map(|p| self.passkey_entry(p))
            .collect())
    }

    async fn user_info(&self, username: &str) -> RpcResult<UserInfo> {
        let name = normalize_username(username)?;
        let users = self.auth.store().list_users().await?;
        users
            .into_iter()
            .find(|u| u.username == name)
            .map(info)
            .ok_or_else(|| UserError::NoSuchUser(name).into())
    }
}

impl From<PinError> for ErrorObjectOwned {
    fn from(e: PinError) -> Self {
        match &e {
            PinError::NoPin => error(error_code::INVALID_INPUT, e.to_string()),
            PinError::NoSuchDevice => error(error_code::NO_SUCH_DEVICE, e.to_string()),
            PinError::Store(_) => internal(e),
        }
    }
}

/// The failure limit's keys for device sign-in codes tried by `user` from
/// a browser: the user's is the one their phone counts under too
/// (`crate::client`), so both share one allowance.
fn sign_in_code_keys(
    ext: &Extensions,
    user: ApiSessionUser,
) -> RpcResult<(String, Option<String>)> {
    Ok((
        format!("devcode:{}", user.user.0),
        source(ext)?.map(|address| format!("devcode@{address}")),
    ))
}

fn device(d: DeviceRecord) -> DeviceEntry {
    DeviceEntry {
        endpoint_id: d.endpoint_id,
        name: d.name,
        last_seen_at: d.last_seen_at,
        pin_required: d.pin_required,
    }
}

impl AdminApi {
    fn plex(&self) -> RpcResult<&PlexCatalog> {
        self.plex
            .as_deref()
            .ok_or_else(|| error(error_code::NOT_CONFIGURED, "no Plex server is configured"))
    }

    /// The caller's own account.
    async fn me(&self, ext: &Extensions) -> RpcResult<UserRecord> {
        let (_, user) = self.caller(ext).await?;
        self.auth
            .store()
            .user_by_id(user.user)
            .await?
            .ok_or_else(|| error(error_code::UNAUTHENTICATED, "session is not valid"))
    }

    /// The account called `username`.
    async fn record(&self, username: &str) -> RpcResult<UserRecord> {
        let name = normalize_username(username)?;
        self.auth
            .store()
            .list_users()
            .await?
            .into_iter()
            .find(|u| u.username == name)
            .ok_or_else(|| UserError::NoSuchUser(name).into())
    }
}

fn info(u: UserRecord) -> UserInfo {
    UserInfo {
        username: u.username,
        display_name: u.display_name,
        role: u.role,
        created_at: u.created_at,
        disabled: u.disabled_at.is_some(),
        has_password: u.has_password,
        last_seen_at: u.last_seen_at,
        has_passkey: u.has_passkey,
        has_pin: u.has_pin,
        device_count: u.device_count,
        session_count: u.session_count,
    }
}

/// A session as the API shows it; `current` is the caller's own.
fn session_entry(s: SessionRecord, current: &str) -> SessionEntry {
    SessionEntry {
        id: session_id(&s.token_hash),
        current: s.token_hash == current,
        device_name: s.device_name,
        created_at: s.created_at,
        last_used_at: s.last_used_at,
        endpoint_id: s.endpoint_id,
    }
}

/// A session's id for `me.revokeSession`: derived from its row's key, so it
/// is stable, and of no use without the session itself.
fn session_id(token_hash: &str) -> String {
    blake3::hash(format!("spoolframe session id {token_hash}").as_bytes()).to_hex()[..16].to_owned()
}

#[async_trait]
impl MeApiServer for AdminApi {
    async fn login(&self, ext: &Extensions, credentials: Credentials) -> RpcResult<LoggedIn> {
        let source = source(ext)?;
        let login = self
            .auth
            .login_password(
                &credentials.username,
                &credentials.password,
                SessionBinding::Api,
                source.as_deref(),
            )
            .await?;
        let username = normalize_username(&credentials.username)?;
        tracing::info!(
            username = %username,
            peer = ?ext.get::<Peer>().and_then(|p| p.0),
            "signed in with a password (API session)"
        );
        Ok(LoggedIn {
            token: login.token.0,
            username,
            role: login.role,
        })
    }

    async fn passkey_login_begin(&self, _ext: &Extensions) -> RpcResult<PasskeyChallenge> {
        let (ceremony, options) = self.passkeys.begin_sign_in()?;
        Ok(PasskeyChallenge {
            ceremony,
            options: serde_json::to_value(options).map_err(internal)?,
        })
    }

    async fn passkey_login_finish(
        &self,
        ext: &Extensions,
        ceremony: String,
        credential: serde_json::Value,
    ) -> RpcResult<LoggedIn> {
        let source = source(ext)?;
        let signed = serde_json::from_value(credential).map_err(|e| {
            error(
                error_code::INVALID_INPUT,
                format!("not what a browser's passkey signs: {e}"),
            )
        })?;
        let user = self
            .passkeys
            .finish_sign_in(&self.auth, &ceremony, &signed, source.as_deref())
            .await?;
        let token = self
            .auth
            .open_api_session(user.id)
            .await
            .map_err(internal)?;
        tracing::info!(
            username = %user.username,
            peer = ?ext.get::<Peer>().and_then(|p| p.0),
            "signed in with a passkey (API session)"
        );
        Ok(LoggedIn {
            token: token.0,
            username: user.username,
            role: user.role,
        })
    }

    async fn logout(&self, ext: &Extensions) -> RpcResult<()> {
        let (token, _) = self.caller(ext).await?;
        self.auth.logout(&token).await.map_err(internal)
    }

    async fn who_am_i(&self, ext: &Extensions) -> RpcResult<UserInfo> {
        let (_, user) = self.caller(ext).await?;
        let record = self.auth.store().user_by_id(user.user).await?;
        record
            .map(info)
            .ok_or_else(|| error(error_code::UNAUTHENTICATED, "session is not valid"))
    }

    async fn set_password(&self, ext: &Extensions, change: PasswordChange) -> RpcResult<()> {
        let (_, user) = self.caller(ext).await?;
        let me = self.account(user).await?;
        if me.has_password {
            let source = source(ext)?;
            self.auth
                .check_password(&me.username, &change.current_password, source.as_deref())
                .await?;
        } else {
            // Set up with a passkey, with no password to prove it is them:
            // a recent sign-in does, as for adding a passkey.
            self.recently_signed_in(ext).await?;
        }
        let phc = hash_password(&change.new_password).await?;
        self.auth.store().set_password(&me.username, &phc).await?;
        tracing::info!(username = %me.username, "password changed by its user");
        Ok(())
    }

    async fn sessions(&self, ext: &Extensions) -> RpcResult<Vec<SessionEntry>> {
        let (token, user) = self.caller(ext).await?;
        let current = crate::auth::SessionToken(token).hash();
        let sessions = self
            .auth
            .store()
            .list_sessions(user.user)
            .await
            .map_err(internal)?;
        Ok(sessions
            .into_iter()
            .map(|s| session_entry(s, &current))
            .collect())
    }

    async fn pin_set(&self, ext: &Extensions, pin: String) -> RpcResult<()> {
        let me = self.me(ext).await?;
        if !valid_pin(&pin) {
            return Err(error(error_code::INVALID_INPUT, "a PIN is 4 to 8 digits"));
        }
        let phc = hash_pin(&pin).await?;
        self.auth
            .store()
            .set_pin(me.id, &phc)
            .await
            .map_err(internal)?;
        tracing::info!(username = %me.username, "PIN set");
        Ok(())
    }

    async fn pin_clear(&self, ext: &Extensions) -> RpcResult<()> {
        let me = self.me(ext).await?;
        self.auth.store().clear_pin(me.id).await.map_err(internal)
    }

    async fn devices(&self, ext: &Extensions) -> RpcResult<Vec<DeviceEntry>> {
        let me = self.me(ext).await?;
        let devices = self
            .auth
            .store()
            .user_devices(me.id)
            .await
            .map_err(internal)?;
        Ok(devices.into_iter().map(device).collect())
    }

    async fn set_pin_required(
        &self,
        ext: &Extensions,
        endpoint_id: String,
        required: bool,
    ) -> RpcResult<()> {
        let me = self.me(ext).await?;
        Ok(self
            .auth
            .store()
            .set_pin_required(me.id, &endpoint_id, required)
            .await?)
    }

    async fn confirm_password(&self, ext: &Extensions, password: String) -> RpcResult<()> {
        let (token, user) = self.caller(ext).await?;
        let me = self.account(user).await?;
        let source = source(ext)?;
        self.auth
            .check_password(&me.username, &password, source.as_deref())
            .await?;
        self.auth
            .store()
            .confirm_session(&SessionToken(token).hash())
            .await
            .map_err(internal)
    }

    async fn device_sign_in_lookup(
        &self,
        ext: &Extensions,
        code: String,
    ) -> RpcResult<DeviceSignIn> {
        let (user, code) = self.sign_in_code(ext, &code).await?;
        match self.auth.sign_ins().lookup(&code) {
            Some(waiting) => Ok(DeviceSignIn {
                device_name: waiting.device_name,
                client_name: waiting.client_name,
                requested_at: waiting.requested_at,
            }),
            None => Err(self.wrong_sign_in_code(ext, user)?),
        }
    }

    async fn device_sign_in_approve(&self, ext: &Extensions, code: String) -> RpcResult<()> {
        let (user, code) = self.sign_in_code(ext, &code).await?;
        let sign_ins = self.auth.sign_ins();
        // No endpoint approves here: a web session is never the device
        // that waits.
        let claim = match sign_ins.claim(&code, None) {
            Ok(claim) => claim,
            Err(ClaimError::Unknown | ClaimError::SameDevice) => {
                return Err(self.wrong_sign_in_code(ext, user)?);
            }
        };
        match self.auth.approve_device(user.user, &claim).await {
            Ok(token) => {
                sign_ins.fulfil(&claim.request, Some(token));
                tracing::info!(
                    user = user.user.0,
                    device = %claim.endpoint,
                    "device sign-in approved from the web"
                );
                Ok(())
            }
            Err(e) => {
                sign_ins.fulfil(&claim.request, None);
                Err(internal(e))
            }
        }
    }

    async fn device_sign_in_deny(&self, ext: &Extensions, code: String) -> RpcResult<()> {
        let (user, code) = self.sign_in_code(ext, &code).await?;
        if self.auth.sign_ins().deny(&code) {
            tracing::info!(user = user.user.0, "device sign-in denied from the web");
            Ok(())
        } else {
            Err(self.wrong_sign_in_code(ext, user)?)
        }
    }

    async fn confirm_passkey(
        &self,
        ext: &Extensions,
        ceremony: String,
        credential: serde_json::Value,
    ) -> RpcResult<()> {
        let (token, user) = self.caller(ext).await?;
        let source = source(ext)?;
        let signed = serde_json::from_value(credential).map_err(|e| {
            error(
                error_code::INVALID_INPUT,
                format!("not what a browser's passkey signs: {e}"),
            )
        })?;
        let signer = self
            .passkeys
            .finish_sign_in(&self.auth, &ceremony, &signed, source.as_deref())
            .await?;
        if signer.id != user.user {
            tracing::warn!(
                user = user.user.0,
                passkey_of = %signer.username,
                "a passkey of another user's was offered as a confirmation"
            );
            return Err(SignInError::Refused.into());
        }
        self.auth
            .store()
            .confirm_session(&SessionToken(token).hash())
            .await
            .map_err(internal)
    }

    async fn passkey_host(&self, ext: &Extensions) -> RpcResult<PasskeyHost> {
        self.caller(ext).await?;
        let (rp_id, origin) = self.passkeys.host()?;
        Ok(PasskeyHost {
            rp_id: rp_id.to_owned(),
            origin: origin.to_owned(),
        })
    }

    async fn passkeys(&self, ext: &Extensions) -> RpcResult<Vec<PasskeyEntry>> {
        let (_, user) = self.caller(ext).await?;
        self.passkey_entries(user.user).await
    }

    async fn passkey_add_begin(&self, ext: &Extensions) -> RpcResult<serde_json::Value> {
        let (token, user) = self.recently_signed_in(ext).await?;
        let me = self.account(user).await?;
        let options = self
            .passkeys
            .begin_registration(self.auth.store(), &SessionToken(token).hash(), &me)
            .await?;
        serde_json::to_value(options).map_err(internal)
    }

    async fn passkey_add_finish(
        &self,
        ext: &Extensions,
        name: String,
        credential: serde_json::Value,
    ) -> RpcResult<PasskeyEntry> {
        let (token, user) = self.caller(ext).await?;
        let me = self.account(user).await?;
        let made = serde_json::from_value(credential).map_err(|e| {
            error(
                error_code::INVALID_INPUT,
                format!("not what a browser makes: {e}"),
            )
        })?;
        let record = self
            .passkeys
            .finish_registration(
                self.auth.store(),
                &SessionToken(token).hash(),
                &me,
                &name,
                &made,
            )
            .await?;
        Ok(self.passkey_entry(record))
    }

    async fn passkey_rename(&self, ext: &Extensions, id: String, name: String) -> RpcResult<()> {
        let (_, user) = self.caller(ext).await?;
        Ok(self
            .auth
            .store()
            .rename_passkey(user.user, &id, &name)
            .await?)
    }

    async fn passkey_remove(&self, ext: &Extensions, id: String) -> RpcResult<()> {
        let (_, user) = self.recently_signed_in(ext).await?;
        self.auth.store().remove_passkey(user.user, &id).await?;
        tracing::info!(user = user.user.0, "passkey removed by its user");
        Ok(())
    }

    async fn revoke_session(&self, ext: &Extensions, id: String) -> RpcResult<()> {
        let (_, user) = self.caller(ext).await?;
        // Only among the caller's own: anyone else's is not found.
        let sessions = self
            .auth
            .store()
            .list_sessions(user.user)
            .await
            .map_err(internal)?;
        let Some(session) = sessions
            .into_iter()
            .find(|s| session_id(&s.token_hash) == id)
        else {
            return Err(error(error_code::NO_SUCH_SESSION, "no such session"));
        };
        self.auth
            .store()
            .revoke_session(&session.token_hash)
            .await
            .map_err(internal)
    }
}

#[async_trait]
impl AdminApiServer for AdminApi {
    async fn setup(&self, ext: &Extensions, request: SetupRequest) -> RpcResult<LoggedIn> {
        self.setup_code(ext, &request.code)?;
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
            role: Role::Admin,
        })
    }

    async fn setup_passkey_begin(
        &self,
        ext: &Extensions,
        request: PasskeySetupBegin,
    ) -> RpcResult<PasskeyChallenge> {
        self.setup_code(ext, &request.code)?;
        // Refused now rather than after the browser's prompt: a passkey
        // made for nothing stays on the device.
        if !self.setup_open().await? {
            return Err(UserError::AlreadySetUp.into());
        }
        let username = normalize_username(&request.username)?;
        let (ceremony, options) = self.passkeys.begin_setup(&username)?;
        Ok(PasskeyChallenge {
            ceremony,
            options: serde_json::to_value(options).map_err(internal)?,
        })
    }

    async fn setup_passkey_finish(
        &self,
        ext: &Extensions,
        request: PasskeySetupFinish,
    ) -> RpcResult<LoggedIn> {
        // Again here: a request begun with the code does not outlive it.
        self.setup_code(ext, &request.code)?;
        let made = serde_json::from_value(request.credential).map_err(|e| {
            error(
                error_code::INVALID_INPUT,
                format!("not what a browser makes: {e}"),
            )
        })?;
        // Before the request is used up: a name can be typed again.
        let name = passkey_name(&request.name)?;
        let passkey = self.passkeys.finish_setup(&request.ceremony, &made)?;
        let user = self
            .auth
            .store()
            .add_first_admin_with_passkey(
                &passkey.username,
                FirstPasskey {
                    handle: &passkey.handle,
                    credential_id: &passkey.credential_id,
                    name,
                    credential: &passkey.credential,
                    rp_id: &passkey.rp_id,
                },
            )
            .await?;
        let token = self.auth.open_api_session(user).await.map_err(internal)?;
        tracing::info!(
            username = %passkey.username,
            passkey = name,
            rp_id = %passkey.rp_id,
            "first admin created, with a passkey and no password"
        );
        Ok(LoggedIn {
            token: token.0,
            username: passkey.username,
            role: Role::Admin,
        })
    }

    async fn user_list(&self, ext: &Extensions) -> RpcResult<Vec<UserInfo>> {
        self.admin(ext).await?;
        Ok(self
            .auth
            .store()
            .list_users()
            .await?
            .into_iter()
            .map(info)
            .collect())
    }

    async fn user_add(&self, ext: &Extensions, user: NewUser) -> RpcResult<UserInfo> {
        self.admin(ext).await?;
        if user.role == Role::Unknown {
            return Err(error(error_code::INVALID_INPUT, "use admin or consumer"));
        }
        let phc = hash_password(&user.password).await?;
        self.auth
            .store()
            .add_user(
                &user.username,
                user.display_name.as_deref(),
                &phc,
                user.role,
            )
            .await?;
        tracing::info!(username = %user.username, role = %user.role, "user added");
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

    async fn user_set_role(
        &self,
        ext: &Extensions,
        username: String,
        role: Role,
    ) -> RpcResult<UserInfo> {
        self.admin(ext).await?;
        if role == Role::Unknown {
            return Err(error(error_code::INVALID_INPUT, "use admin or consumer"));
        }
        self.auth.store().set_role(&username, role).await?;
        tracing::info!(%username, %role, "role changed");
        self.user_info(&username).await
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

    async fn user_clear_pin(&self, ext: &Extensions, username: String) -> RpcResult<()> {
        self.admin(ext).await?;
        let user = self.record(&username).await?;
        self.auth
            .store()
            .clear_pin(user.id)
            .await
            .map_err(internal)?;
        tracing::info!(%username, "PIN cleared by an admin");
        Ok(())
    }

    async fn user_devices(
        &self,
        ext: &Extensions,
        username: String,
    ) -> RpcResult<Vec<DeviceEntry>> {
        self.admin(ext).await?;
        let user = self.record(&username).await?;
        let devices = self
            .auth
            .store()
            .user_devices(user.id)
            .await
            .map_err(internal)?;
        Ok(devices.into_iter().map(device).collect())
    }

    async fn user_set_pin_required(
        &self,
        ext: &Extensions,
        username: String,
        endpoint_id: String,
        required: bool,
    ) -> RpcResult<()> {
        self.admin(ext).await?;
        let user = self.record(&username).await?;
        self.auth
            .store()
            .set_pin_required(user.id, &endpoint_id, required)
            .await?;
        tracing::info!(%username, endpoint_id, required, "PIN requirement changed by an admin");
        Ok(())
    }

    async fn user_sessions(
        &self,
        ext: &Extensions,
        username: String,
    ) -> RpcResult<Vec<SessionEntry>> {
        let (token, _) = self.caller(ext).await?;
        self.admin(ext).await?;
        let user = self.record(&username).await?;
        let current = crate::auth::SessionToken(token).hash();
        let sessions = self
            .auth
            .store()
            .list_sessions(user.id)
            .await
            .map_err(internal)?;
        Ok(sessions
            .into_iter()
            .map(|s| session_entry(s, &current))
            .collect())
    }

    async fn user_revoke_session(
        &self,
        ext: &Extensions,
        username: String,
        id: String,
    ) -> RpcResult<()> {
        self.admin(ext).await?;
        let user = self.record(&username).await?;
        let sessions = self
            .auth
            .store()
            .list_sessions(user.id)
            .await
            .map_err(internal)?;
        let Some(session) = sessions
            .into_iter()
            .find(|s| session_id(&s.token_hash) == id)
        else {
            return Err(error(error_code::NO_SUCH_SESSION, "no such session"));
        };
        self.auth
            .store()
            .revoke_session(&session.token_hash)
            .await
            .map_err(internal)?;
        tracing::info!(%username, "session revoked by an admin");
        Ok(())
    }

    async fn user_passkeys(
        &self,
        ext: &Extensions,
        username: String,
    ) -> RpcResult<Vec<PasskeyEntry>> {
        self.admin(ext).await?;
        let user = self.record(&username).await?;
        self.passkey_entries(user.id).await
    }

    async fn user_remove_passkey(
        &self,
        ext: &Extensions,
        username: String,
        id: String,
    ) -> RpcResult<()> {
        self.admin(ext).await?;
        let user = self.record(&username).await?;
        self.auth.store().remove_passkey(user.id, &id).await?;
        tracing::info!(%username, "passkey removed by an admin");
        Ok(())
    }

    async fn plex_status(&self, ext: &Extensions) -> RpcResult<PlexStatus> {
        self.admin(ext).await?;
        let plex = self.plex()?;
        let down = |e: spoolframe_catalog::CatalogError| PlexStatus {
            reachable: false,
            error: Some(e.to_string()),
            server_name: None,
            url: Some(plex.base_url().to_owned()),
            libraries: Vec::new(),
        };
        let server_name = match plex.server_name().await {
            Ok(name) => name,
            Err(e) => return Ok(down(e)),
        };
        let libraries = match plex.libraries().await {
            Ok(libraries) => libraries,
            Err(e) => return Ok(down(e)),
        };
        let mut status = Vec::with_capacity(libraries.len());
        for library in libraries {
            let page = spoolframe_catalog::Page {
                offset: 0,
                limit: 1,
            };
            let items = match plex.items(&library.id, page).await {
                Ok(page) => page.total,
                Err(e) => return Ok(down(e)),
            };
            status.push(LibraryStatus {
                title: library.title,
                kind: library.kind,
                items,
            });
        }
        Ok(PlexStatus {
            reachable: true,
            error: None,
            server_name,
            url: Some(plex.base_url().to_owned()),
            libraries: status,
        })
    }

    async fn plex_check_paths(
        &self,
        ext: &Extensions,
        library: Option<String>,
    ) -> RpcResult<Vec<PathReport>> {
        self.admin(ext).await?;
        let reports = self
            .plex()?
            .check_paths(library.as_deref(), Scope::All)
            .await
            .map_err(|e| match e {
                spoolframe_catalog::CatalogError::NotFound(what) => {
                    error(error_code::INVALID_INPUT, format!("no {what}"))
                }
                other => error(error_code::UNAVAILABLE, other.to_string()),
            })?;
        let failures = |f: spoolframe_plex::check::Failures| PathFailures {
            count: f.count,
            examples: f.examples,
        };
        Ok(reports
            .into_iter()
            .map(|r| PathReport {
                library: r.library,
                ok: r.ok,
                no_mapping: failures(r.no_mapping),
                unreadable: failures(r.unreadable),
            })
            .collect())
    }

    async fn server_status(&self, ext: &Extensions) -> RpcResult<ServerStatus> {
        self.admin(ext).await?;
        Ok(self.status.status())
    }

    async fn plex_watch_sync(&self, ext: &Extensions, dry_run: bool) -> RpcResult<PlexSyncReport> {
        self.admin(ext).await?;
        let Some(sync) = &self.plex_sync else {
            return Err(error(
                error_code::NOT_CONFIGURED,
                "Plex watch sync is off: set plex { watch_sync { user = …; } }",
            ));
        };
        let report = sync.run(dry_run).await.map_err(|e| match e {
            SyncError::Plex(e) => error(error_code::UNAVAILABLE, e.to_string()),
            SyncError::Store(e) => internal(e),
        })?;
        Ok(PlexSyncReport {
            pulled: report.pulled,
            pushed: report.pushed,
            failed: report.failed,
        })
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
    fn a_loopback_peer_is_no_source_and_any_other_is_its_address() {
        let source = |ip: &str| Peer(Some(ip.parse().unwrap())).source();
        assert_eq!(source("127.0.0.1"), None);
        assert_eq!(source("::1"), None);
        assert_eq!(source("::ffff:127.0.0.1"), None);
        assert_eq!(source("192.0.2.7").as_deref(), Some("192.0.2.7"));
        assert_eq!(source("::ffff:192.0.2.7").as_deref(), Some("192.0.2.7"));
        assert_eq!(source("2001:db8::7").as_deref(), Some("2001:db8::7"));
        assert_eq!(Peer(None).source(), None);
    }

    /// The API over a store of its own, and alice's session in it: what
    /// her calls carry, and the session's key in the store.
    async fn api_with_alice(test: &str) -> (AdminApi, Extensions, String, impl Sized) {
        let dir = crate::store::tests::state_dir(test).await;
        let store = crate::store::Store::open(dir.as_ref()).await.unwrap();
        let phc = hash_password("correct horse").await.unwrap();
        store
            .add_user("alice", None, &phc, Role::Consumer)
            .await
            .unwrap();
        let auth = Auth::new(store);
        let login = auth
            .login_password("alice", "correct horse", SessionBinding::Api, None)
            .await
            .unwrap();
        let mut ext = Extensions::new();
        ext.insert(Bearer(login.token.0.clone()));
        ext.insert(Peer(Some("127.0.0.1".parse().unwrap())));
        (api(auth), ext, login.token.hash(), dir)
    }

    /// The API over `auth`, at `https://media.example.org`.
    fn api(auth: Auth) -> AdminApi {
        let transcoding = Transcoding::new(
            Box::new(spoolframe_media::encoders::FfmpegCommand::new(
                "/nonexistent/ffmpeg",
            )),
            Default::default(),
            std::env::temp_dir(),
        );
        let origins = Origins::Exactly("https://media.example.org".into());
        AdminApi {
            auth,
            setup_code: None,
            status: Arc::new(StatusSource::new("Den", Arc::new(transcoding))),
            plex_sync: None,
            plex: None,
            passkeys: Arc::new(Passkeys::new(&origins)),
            public_url: origins.public_url().map(str::to_owned),
        }
    }

    /// A user set up with a passkey has no password to prove it is them
    /// with, so a first password needs a recent sign-in, as a passkey does.
    #[tokio::test]
    async fn a_first_password_is_set_only_soon_after_signing_in() {
        let dir = crate::store::tests::state_dir("first-password").await;
        let store = crate::store::Store::open(dir.as_ref()).await.unwrap();
        let root = store
            .add_first_admin_with_passkey(
                "root",
                FirstPasskey {
                    handle: "handle-1",
                    credential_id: "cred-1",
                    name: "iPhone",
                    credential: "{}",
                    rp_id: "media.example.org",
                },
            )
            .await
            .unwrap();
        let auth = Auth::new(store.clone());
        let token = auth.open_api_session(root).await.unwrap();
        let api = api(auth);
        let mut ext = Extensions::new();
        ext.insert(Bearer(token.0.clone()));
        ext.insert(Peer(Some("127.0.0.1".parse().unwrap())));
        let first = || PasswordChange {
            current_password: String::new(),
            new_password: "correct horse".into(),
        };
        store
            .age_session(&token.hash(), RECENT_SIGN_IN.as_secs() as i64 + 1)
            .await;
        let stale = api.set_password(&ext, first()).await.unwrap_err();
        assert_eq!(stale.code(), error_code::RECENT_SIGN_IN_REQUIRED);
        assert!(!api.who_am_i(&ext).await.unwrap().has_password);

        store
            .age_session(&token.hash(), -(RECENT_SIGN_IN.as_secs() as i64))
            .await;
        api.set_password(&ext, first()).await.unwrap();
        api.auth
            .login_password("root", "correct horse", SessionBinding::Api, None)
            .await
            .unwrap();
    }

    /// P3-03: a sensitive change needs a sign-in, or a confirmation, within
    /// the last 10 minutes.
    #[tokio::test]
    async fn a_passkey_is_added_or_removed_only_soon_after_signing_in() {
        let (api, ext, session, _dir) = api_with_alice("recent").await;
        let store = api.auth.store().clone();
        let half = RECENT_SIGN_IN.as_secs() as i64 / 2;
        store.age_session(&session, half).await;
        api.passkey_add_begin(&ext).await.unwrap();

        let (_, alice) = api.caller(&ext).await.unwrap();
        store
            .add_passkey(alice.user, "cred-1", "MacBook", "{}", "media.example.org")
            .await
            .unwrap();
        store.age_session(&session, half + 1).await;
        let stale = api.passkey_add_begin(&ext).await.unwrap_err();
        assert_eq!(stale.code(), error_code::RECENT_SIGN_IN_REQUIRED);
        let stale = api.passkey_remove(&ext, "cred-1".into()).await.unwrap_err();
        assert_eq!(stale.code(), error_code::RECENT_SIGN_IN_REQUIRED);
        // What is not sensitive still works.
        api.passkey_rename(&ext, "cred-1".into(), "Laptop".into())
            .await
            .unwrap();
        assert_eq!(api.passkeys(&ext).await.unwrap()[0].name, "Laptop");

        // A wrong password confirms nothing.
        let wrong = api
            .confirm_password(&ext, "battery staple".into())
            .await
            .unwrap_err();
        assert_eq!(wrong.code(), error_code::INVALID_CREDENTIALS);
        let stale = api.passkey_remove(&ext, "cred-1".into()).await.unwrap_err();
        assert_eq!(stale.code(), error_code::RECENT_SIGN_IN_REQUIRED);

        api.confirm_password(&ext, "correct horse".into())
            .await
            .unwrap();
        api.passkey_remove(&ext, "cred-1".into()).await.unwrap();
        assert_eq!(api.passkeys(&ext).await.unwrap(), []);
        api.passkey_add_begin(&ext).await.unwrap();
    }

    #[test]
    fn generated_codes_use_the_unambiguous_alphabet() {
        let code = SetupCode::generate();
        assert_eq!(code.0.len(), SetupCode::LEN);
        assert!(code.0.bytes().all(|b| SetupCode::ALPHABET.contains(&b)));
        assert_ne!(SetupCode::generate(), code);
    }
}
