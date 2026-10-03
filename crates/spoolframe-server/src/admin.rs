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
    AdminApiServer, Credentials, DeviceEntry, LibraryStatus, LoggedIn, MeApiServer, NewUser,
    PasswordChange, PathFailures, PathReport, PlexStatus, PlexSyncReport, Role, ServerStatus,
    SessionEntry, SetupRequest, TranscodingStatus, UserInfo, error_code,
};
use spoolframe_catalog::Catalog;
use spoolframe_plex::PlexCatalog;
use spoolframe_plex::check::Scope;

pub use crate::admin_http::Origins;
use crate::auth::{Auth, LoginError, PasswordError, hash_password, hash_pin, valid_pin};
use crate::plex_sync::{PlexSync, SyncError};
use crate::store::pins::{DeviceRecord, PinError};
use crate::store::sessions::{ApiSessionUser, SessionBinding, SessionRecord};
use crate::store::users::{UserError, UserRecord, normalize_username};
use crate::transcode::Transcoding;

/// The address the caller's connection comes from (`crate::admin_http`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Peer(pub(crate) IpAddr);

impl Peer {
    /// What the failure limit counts for this caller besides the username:
    /// its address. A loopback address is shared by everyone who comes
    /// through this machine (the CLI, an SSH tunnel, a proxy), so it names
    /// no one: counting it would let any of them lock all the others out.
    fn source(self) -> Option<String> {
        let ip = self.0.to_canonical();
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
}

/// Start the admin API on `listen`. Browsers are let in from `origins`.
pub async fn serve(
    listen: SocketAddr,
    admin: Admin,
    origins: Origins,
) -> std::io::Result<(SocketAddr, ServerHandle)> {
    let api = AdminApi {
        auth: admin.auth,
        setup_code: admin.setup_code,
        status: admin.status,
        plex_sync: admin.plex_sync,
        plex: admin.plex,
    };
    let mut methods = AdminApiServer::into_rpc(api.clone());
    methods
        .merge(MeApiServer::into_rpc(api.clone()))
        .expect("the me and admin namespaces never share a name");
    crate::admin_http::serve(listen, api, methods.into(), origins).await
}

#[derive(Clone)]
pub(crate) struct AdminApi {
    auth: Auth,
    setup_code: Option<SetupCode>,
    status: Arc<StatusSource>,
    plex_sync: Option<Arc<PlexSync>>,
    plex: Option<Arc<PlexCatalog>>,
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

impl AdminApi {
    /// Whether `setup` would take a code now: no users, and a code was
    /// made at start. What a browser's first page decides on.
    pub(crate) async fn setup_open(&self) -> RpcResult<bool> {
        if self.setup_code.is_none() {
            return Ok(false);
        }
        Ok(self.auth.store().user_count().await? == 0)
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
        Ok(LoggedIn {
            token: login.token.0,
            username: normalize_username(&credentials.username)?,
            role: login.role,
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
        let Some(me) = self.auth.store().user_by_id(user.user).await? else {
            return Err(error(error_code::UNAUTHENTICATED, "session is not valid"));
        };
        let source = source(ext)?;
        self.auth
            .check_password(&me.username, &change.current_password, source.as_deref())
            .await?;
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
        let Some(code) = &self.setup_code else {
            return Err(UserError::AlreadySetUp.into());
        };
        let source = source(ext)?;
        if let Some(retry_after) = self.auth.attempt_limited("setup", source.as_deref()) {
            return Err(LoginError::RateLimited { retry_after }.into());
        }
        if !code.matches(&request.code) {
            self.auth.attempt_failed("setup", source.as_deref());
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

    async fn plex_status(&self, ext: &Extensions) -> RpcResult<PlexStatus> {
        self.admin(ext).await?;
        let plex = self.plex()?;
        let down = |e: spoolframe_catalog::CatalogError| PlexStatus {
            reachable: false,
            error: Some(e.to_string()),
            server_name: None,
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
        let source = |ip: &str| Peer(ip.parse().unwrap()).source();
        assert_eq!(source("127.0.0.1"), None);
        assert_eq!(source("::1"), None);
        assert_eq!(source("::ffff:127.0.0.1"), None);
        assert_eq!(source("192.0.2.7").as_deref(), Some("192.0.2.7"));
        assert_eq!(source("::ffff:192.0.2.7").as_deref(), Some("192.0.2.7"));
        assert_eq!(source("2001:db8::7").as_deref(), Some("2001:db8::7"));
    }

    #[test]
    fn generated_codes_use_the_unambiguous_alphabet() {
        let code = SetupCode::generate();
        assert_eq!(code.0.len(), SetupCode::LEN);
        assert!(code.0.bytes().all(|b| SetupCode::ALPHABET.contains(&b)));
        assert_ne!(SetupCode::generate(), code);
    }
}
