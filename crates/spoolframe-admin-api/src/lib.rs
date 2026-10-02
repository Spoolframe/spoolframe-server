//! The server's admin API: JSON-RPC over WebSocket (ARCHITECTURE §35).
//!
//! The `spoolframe-server` CLI uses it now and the web UI (P3) later. Two
//! namespaces (P2-09):
//!
//! - `me.*`: the caller's own account, for every role (`me.login`,
//!   `me.setPassword`, …).
//! - `admin.*`: other users and the server, for admins only; `FORBIDDEN`
//!   for anyone else. `admin.setup` is the exception: it creates the first
//!   admin, with the setup code from the server's log, while there are no
//!   users.
//!
//! A session token from `admin.setup` or `me.login` goes in the
//! `Authorization: Bearer` header of the WebSocket upgrade.

use std::fmt;

use serde::{Deserialize, Serialize};
pub use spoolframe_types::Role;

/// JSON-RPC error codes the admin API returns, beside the standard ones.
pub mod error_code {
    /// No session, or one that is revoked, expired or for a disabled user.
    pub const UNAUTHENTICATED: i32 = -32001;
    /// The session's user is not an admin.
    pub const FORBIDDEN: i32 = -32002;
    /// Wrong username or password; the same for an unknown user.
    pub const INVALID_CREDENTIALS: i32 = -32003;
    /// Too many failed logins; `data` is `{"retryAfterMs": n}`.
    pub const RATE_LIMITED: i32 = -32004;
    /// `setup` when the server already has users.
    pub const ALREADY_SET_UP: i32 = -32005;
    pub const NO_SUCH_USER: i32 = -32006;
    pub const USER_EXISTS: i32 = -32007;
    /// A username or password that is not acceptable; the message says why.
    pub const INVALID_INPUT: i32 = -32008;
    /// `setup` with a setup code that is not the one in the server's log.
    pub const INVALID_SETUP_CODE: i32 = -32009;
    /// A feature the server's config does not turn on.
    pub const NOT_CONFIGURED: i32 = -32010;
    /// A backend the server depends on (Plex) failed.
    pub const UNAVAILABLE: i32 = -32011;
    /// Demoting or disabling the last enabled admin: the server would be
    /// left with nobody able to manage it.
    pub const LAST_ADMIN: i32 = -32012;
    /// `me.revokeSession` with an id that is not one of the caller's.
    pub const NO_SUCH_SESSION: i32 = -32013;
    /// Not a device the user has signed in from.
    pub const NO_SUCH_DEVICE: i32 = -32014;
}

/// A username and password.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// The first admin's registration. `code` is the one-time setup code the
/// server prints in its log at start while it has no users: whoever can
/// read the server's log may claim it, nobody else on the network.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupRequest {
    pub code: String,
    pub username: String,
    pub password: String,
}

impl fmt::Debug for SetupRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SetupRequest")
            .field("code", &"<redacted>")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// A new API session.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoggedIn {
    /// Send as `Authorization: Bearer <token>`.
    pub token: String,
    pub username: String,
    pub role: Role,
}

impl fmt::Debug for LoggedIn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoggedIn")
            .field("token", &"<redacted>")
            .field("username", &self.username)
            .field("role", &self.role)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInfo {
    pub username: String,
    /// What clients show; `None` means the username.
    #[serde(default)]
    pub display_name: Option<String>,
    pub role: Role,
    /// Unix seconds.
    pub created_at: i64,
    pub disabled: bool,
    pub has_password: bool,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewUser {
    pub username: String,
    #[serde(default)]
    pub display_name: Option<String>,
    pub password: String,
    /// `Consumer` if left out.
    #[serde(default)]
    pub role: Role,
}

impl fmt::Debug for NewUser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewUser")
            .field("username", &self.username)
            .field("display_name", &self.display_name)
            .field("password", &"<redacted>")
            .field("role", &self.role)
            .finish()
    }
}

/// A change of the caller's own password: the current one proves it is
/// them, not someone holding their session.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PasswordChange {
    pub current_password: String,
    pub new_password: String,
}

impl fmt::Debug for PasswordChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PasswordChange { <redacted> }")
    }
}

/// One of the caller's live sessions, never its token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEntry {
    /// For `me.revokeSession`; stable for the session, and no use to anyone
    /// without one.
    pub id: String,
    /// The device's name; `None` for an admin API session or an unnamed
    /// device.
    #[serde(default)]
    pub device_name: Option<String>,
    /// Unix seconds.
    pub created_at: i64,
    pub last_used_at: i64,
    /// The session this call is made in.
    pub current: bool,
}

/// One of a user's devices (P2-08).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceEntry {
    /// Hex `EndpointId`.
    pub endpoint_id: String,
    #[serde(default)]
    pub name: Option<String>,
    /// Unix seconds.
    pub last_seen_at: i64,
    /// The user's PIN is asked for on it.
    pub pin_required: bool,
}

/// What the server is and can do, for an admin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerStatus {
    pub name: String,
    pub version: String,
    /// The key clients pin; `None` while the endpoint is starting.
    #[serde(default)]
    pub endpoint_id: Option<String>,
    /// That key's fingerprint (P1-21), for checking a client's pin.
    #[serde(default)]
    pub fingerprint: Option<String>,
    pub transcoding: TranscodingStatus,
}

/// The encoders that work on this machine (P2-02), preferred first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscodingStatus {
    /// ffmpeg's names, e.g. `hevc_vaapi`, `libx264`.
    pub video_encoders: Vec<String>,
    pub audio_encoders: Vec<String>,
    /// Codecs with a working hardware decoder, as `hevc (vaapi)`.
    pub hardware_decoders: Vec<String>,
}

/// What a Plex watch sync run did, or would do (PX-04): one line per item.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlexSyncReport {
    pub pulled: Vec<String>,
    pub pushed: Vec<String>,
    pub failed: Vec<String>,
}

/// The caller's own account, for every role.
#[cfg_attr(
    all(feature = "client", feature = "server"),
    jsonrpsee::proc_macros::rpc(client, server, namespace = "me")
)]
#[cfg_attr(
    all(feature = "client", not(feature = "server")),
    jsonrpsee::proc_macros::rpc(client, namespace = "me")
)]
#[cfg_attr(
    all(feature = "server", not(feature = "client")),
    jsonrpsee::proc_macros::rpc(server, namespace = "me")
)]
pub trait MeApi {
    #[method(name = "login")]
    async fn login(&self, credentials: Credentials) -> jsonrpsee::core::RpcResult<LoggedIn>;

    /// End the caller's session.
    #[method(name = "logout", with_extensions)]
    async fn logout(&self) -> jsonrpsee::core::RpcResult<()>;

    /// The caller, as an admin would see them; `role` says what they may
    /// do, read at each call.
    #[method(name = "whoAmI", with_extensions)]
    async fn who_am_i(&self) -> jsonrpsee::core::RpcResult<UserInfo>;

    #[method(name = "setPassword", with_extensions)]
    async fn set_password(&self, change: PasswordChange) -> jsonrpsee::core::RpcResult<()>;

    /// The caller's live sessions, on every device, most recently used
    /// first.
    #[method(name = "sessions", with_extensions)]
    async fn sessions(&self) -> jsonrpsee::core::RpcResult<Vec<SessionEntry>>;

    /// End one of the caller's own sessions.
    #[method(name = "revokeSession", with_extensions)]
    async fn revoke_session(&self, id: String) -> jsonrpsee::core::RpcResult<()>;

    /// Set or change the caller's PIN (P2-08): 4 to 8 digits. The PIN, and
    /// where it is asked for, need only the session: a real sign-in made
    /// it, and a PIN is never a way to sign in.
    #[method(name = "pinSet", with_extensions)]
    async fn pin_set(&self, pin: String) -> jsonrpsee::core::RpcResult<()>;

    /// Remove the caller's PIN, and its requirement on every device.
    #[method(name = "pinClear", with_extensions)]
    async fn pin_clear(&self) -> jsonrpsee::core::RpcResult<()>;

    /// The devices the caller has signed in from.
    #[method(name = "devices", with_extensions)]
    async fn devices(&self) -> jsonrpsee::core::RpcResult<Vec<DeviceEntry>>;

    /// Ask for the caller's PIN on one of their devices, or stop asking.
    #[method(name = "setPinRequired", with_extensions)]
    async fn set_pin_required(
        &self,
        endpoint_id: String,
        required: bool,
    ) -> jsonrpsee::core::RpcResult<()>;
}

/// Other users and the server: admins only.
#[cfg_attr(
    all(feature = "client", feature = "server"),
    jsonrpsee::proc_macros::rpc(client, server, namespace = "admin")
)]
#[cfg_attr(
    all(feature = "client", not(feature = "server")),
    jsonrpsee::proc_macros::rpc(client, namespace = "admin")
)]
#[cfg_attr(
    all(feature = "server", not(feature = "client")),
    jsonrpsee::proc_macros::rpc(server, namespace = "admin")
)]
pub trait AdminApi {
    /// Create the first admin. Works only while the server has no users,
    /// and only with the setup code from the server's log.
    #[method(name = "setup")]
    async fn setup(&self, request: SetupRequest) -> jsonrpsee::core::RpcResult<LoggedIn>;

    #[method(name = "userList", with_extensions)]
    async fn user_list(&self) -> jsonrpsee::core::RpcResult<Vec<UserInfo>>;

    #[method(name = "userAdd", with_extensions)]
    async fn user_add(&self, user: NewUser) -> jsonrpsee::core::RpcResult<UserInfo>;

    /// Set anyone's password, without the current one.
    #[method(name = "userSetPassword", with_extensions)]
    async fn user_set_password(&self, credentials: Credentials) -> jsonrpsee::core::RpcResult<()>;

    /// Change a user's role. The last enabled admin cannot be demoted
    /// (`LAST_ADMIN`). Takes effect on their next call, on every session.
    #[method(name = "userSetRole", with_extensions)]
    async fn user_set_role(
        &self,
        username: String,
        role: Role,
    ) -> jsonrpsee::core::RpcResult<UserInfo>;

    /// Disable a user and revoke their sessions; returns how many. The last
    /// enabled admin cannot be disabled (`LAST_ADMIN`).
    #[method(name = "userDisable", with_extensions)]
    async fn user_disable(&self, username: String) -> jsonrpsee::core::RpcResult<u64>;

    #[method(name = "userEnable", with_extensions)]
    async fn user_enable(&self, username: String) -> jsonrpsee::core::RpcResult<()>;

    /// Remove a user's PIN, and its requirement everywhere (P2-08). An
    /// admin can clear a PIN, never set one: the user sets a new one.
    #[method(name = "userClearPin", with_extensions)]
    async fn user_clear_pin(&self, username: String) -> jsonrpsee::core::RpcResult<()>;

    /// The devices a user has signed in from.
    #[method(name = "userDevices", with_extensions)]
    async fn user_devices(&self, username: String) -> jsonrpsee::core::RpcResult<Vec<DeviceEntry>>;

    /// Ask for a user's PIN on one of their devices, or stop asking.
    #[method(name = "userSetPinRequired", with_extensions)]
    async fn user_set_pin_required(
        &self,
        username: String,
        endpoint_id: String,
        required: bool,
    ) -> jsonrpsee::core::RpcResult<()>;

    /// The server's name, key, version and encoders.
    #[method(name = "serverStatus", with_extensions)]
    async fn server_status(&self) -> jsonrpsee::core::RpcResult<ServerStatus>;

    /// Sync watch state with Plex now (PX-04); with `dryRun`, only say
    /// what would change.
    #[method(name = "plexWatchSync", with_extensions)]
    async fn plex_watch_sync(&self, dry_run: bool) -> jsonrpsee::core::RpcResult<PlexSyncReport>;
}
