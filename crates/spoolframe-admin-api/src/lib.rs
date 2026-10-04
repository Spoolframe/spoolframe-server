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
    /// A sensitive change (adding or removing a passkey, a first password)
    /// in a session whose user last proved who they are more than 10
    /// minutes ago: confirm with `me.confirmPassword` or `me.confirmPasskey`,
    /// or sign in again.
    pub const RECENT_SIGN_IN_REQUIRED: i32 = -32015;
    /// Not one of the user's passkeys.
    pub const NO_SUCH_PASSKEY: i32 = -32016;
    /// Removing the user's last way to sign in.
    pub const LAST_SIGN_IN_METHOD: i32 = -32017;
    /// A passkey sign-in that did not sign anyone in: an unknown or removed
    /// passkey, a request that is over, a signature that does not verify,
    /// a disabled user. Deliberately one answer.
    pub const PASSKEY_REFUSED: i32 = -32018;
    /// Not a device sign-in code that can be approved: a typo, a guess, or
    /// one already used or expired.
    pub const NO_SUCH_SIGN_IN_CODE: i32 = -32019;
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

/// The first admin's registration with a passkey and no password (P3-01,
/// P3-05): the setup code and the username, as `SetupRequest` has them.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PasskeySetupBegin {
    pub code: String,
    pub username: String,
}

impl fmt::Debug for PasskeySetupBegin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PasskeySetupBegin")
            .field("code", &"<redacted>")
            .field("username", &self.username)
            .finish()
    }
}

/// Finishing it: the setup code again, so a request cannot outlive it, the
/// `ceremony` from `setupPasskeyBegin`, what the passkey is called, and
/// what the browser made (`RegistrationResponseJSON`).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PasskeySetupFinish {
    pub code: String,
    pub ceremony: String,
    pub name: String,
    pub credential: serde_json::Value,
}

impl fmt::Debug for PasskeySetupFinish {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PasskeySetupFinish")
            .field("code", &"<redacted>")
            .field("ceremony", &"<redacted>")
            .field("name", &self.name)
            .finish_non_exhaustive()
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
    /// When any of their sessions was last used, Unix seconds; `None` if
    /// they have never signed in.
    #[serde(default)]
    pub last_seen_at: Option<i64>,
    /// They have a passkey (P3-05): with `has_password`, how they can sign
    /// in.
    pub has_passkey: bool,
    /// They have a profile PIN (P2-08): `admin.userClearPin` has
    /// something to clear, and a device can be set to ask for it.
    pub has_pin: bool,
    /// The devices they have signed in from, as `admin.userDevices` lists
    /// them.
    pub device_count: u64,
    /// Their live sessions, as `admin.userSessions` lists them.
    pub session_count: u64,
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
/// them, not someone holding their session. A user who has no password
/// yet (passkeys only) proves it with a recent sign-in instead, and
/// `current_password` is not read.
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
    /// The device's key (hex `EndpointId`) for a client session; `None` for
    /// a web or CLI session.
    #[serde(default)]
    pub endpoint_id: Option<String>,
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

/// Where passkeys work (P3-05): WebAuthn ties each one to a host, the
/// relying party, which is the host of the server's `admin.public_url`, or
/// `localhost` without one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PasskeyHost {
    /// The relying-party ID: the host passkeys are bound to.
    pub rp_id: String,
    /// The one origin a browser can make or use them at; any port when
    /// `rp_id` is `localhost`.
    pub origin: String,
}

/// A passkey sign-in that was begun (P3-05), or a first admin's
/// registration with one (`setupPasskeyBegin`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PasskeyChallenge {
    /// Names the request when finishing it; of use once, for 5 minutes.
    pub ceremony: String,
    /// For a sign-in, what `navigator.credentials.get` takes, as JSON
    /// (`{"publicKey": PublicKeyCredentialRequestOptionsJSON}`, binary
    /// fields base64url). No credentials are listed: the browser offers
    /// the passkeys it finds for this host. For a registration, what
    /// `navigator.credentials.create` takes, as `me.passkeyAddBegin`
    /// answers it.
    pub options: serde_json::Value,
}

/// One of a user's passkeys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PasskeyEntry {
    /// For `me.passkeyRename` and `me.passkeyRemove`: its credential id.
    pub id: String,
    /// What its user called it.
    pub name: String,
    /// Unix seconds.
    pub created_at: i64,
    /// `None` if it has never signed them in.
    pub last_used_at: Option<i64>,
    /// The host it was made for.
    pub rp_id: String,
    /// Whether that is the host the server is at now: one made for another
    /// host (before `admin.public_url` changed) signs no one in.
    pub usable: bool,
}

/// A device waiting to be signed in by someone approving its code (P3-07).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceSignIn {
    /// What the device calls itself; `None` if it gave no name.
    pub device_name: Option<String>,
    /// The app on it, e.g. `spoolframe-linux`.
    pub client_name: String,
    /// Unix seconds.
    pub requested_at: i64,
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
    /// Whether the server announces itself on the LAN (mDNS).
    #[serde(default)]
    pub mdns: bool,
    /// Where clients reach it: the endpoint's bound addresses.
    #[serde(default)]
    pub addresses: Vec<String>,
}

/// Plex as the server sees it now (P3-02).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlexStatus {
    pub reachable: bool,
    /// Why not, when it is not.
    #[serde(default)]
    pub error: Option<String>,
    /// What the PMS calls itself.
    #[serde(default)]
    pub server_name: Option<String>,
    /// Where the server reaches it: `plex.url`, without the token.
    pub url: Option<String>,
    #[serde(default)]
    pub libraries: Vec<LibraryStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryStatus {
    pub title: String,
    pub kind: spoolframe_types::LibraryKind,
    /// Its top-level items: movies, shows or artists.
    pub items: u32,
}

/// One library's media paths, checked (P1-04).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathReport {
    pub library: String,
    /// Files this host can read.
    pub ok: u64,
    /// Unreadable, and no mapping matched: a mapping is probably missing.
    pub no_mapping: PathFailures,
    /// Unreadable though a mapping matched: wrong mapping or missing mount.
    pub unreadable: PathFailures,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathFailures {
    pub count: u64,
    /// `"<plex path> -> <local path>: <reason>"`, a few.
    pub examples: Vec<String>,
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
    #[method(name = "login", with_extensions)]
    async fn login(&self, credentials: Credentials) -> jsonrpsee::core::RpcResult<LoggedIn>;

    /// Begin signing in with a passkey, without a username (P3-05).
    /// `NOT_CONFIGURED`, saying why, where the server has no passkeys.
    #[method(name = "passkeyLoginBegin", with_extensions)]
    async fn passkey_login_begin(&self) -> jsonrpsee::core::RpcResult<PasskeyChallenge>;

    /// Finish it: `credential` is what the browser's passkey signed
    /// (`AuthenticationResponseJSON`). The session is an ordinary one, as
    /// `login` opens. A failure is `PASSKEY_REFUSED`, and counts in the
    /// limit on failed logins (`RATE_LIMITED`).
    #[method(name = "passkeyLoginFinish", with_extensions)]
    async fn passkey_login_finish(
        &self,
        ceremony: String,
        credential: serde_json::Value,
    ) -> jsonrpsee::core::RpcResult<LoggedIn>;

    /// End the caller's session.
    #[method(name = "logout", with_extensions)]
    async fn logout(&self) -> jsonrpsee::core::RpcResult<()>;

    /// The caller, as an admin would see them; `role` says what they may
    /// do, read at each call.
    #[method(name = "whoAmI", with_extensions)]
    async fn who_am_i(&self) -> jsonrpsee::core::RpcResult<UserInfo>;

    /// Change the caller's password, given the current one. A caller with
    /// no password (passkeys only) sets their first one instead, which
    /// needs a recent sign-in (`RECENT_SIGN_IN_REQUIRED`).
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

    /// Prove again who the caller is, with their password: for the next 10
    /// minutes the session may make sensitive changes
    /// (`RECENT_SIGN_IN_REQUIRED`), as one just signed in may.
    #[method(name = "confirmPassword", with_extensions)]
    async fn confirm_password(&self, password: String) -> jsonrpsee::core::RpcResult<()>;

    /// The device showing sign-in code `code` (P3-07), typed in any case,
    /// with or without its dash, to ask "Sign in on '<device>'?" before
    /// approving it. A code that is not pending is `NO_SUCH_SIGN_IN_CODE`,
    /// and such codes are limited as failed logins are (`RATE_LIMITED`).
    #[method(name = "deviceSignInLookup", with_extensions)]
    async fn device_sign_in_lookup(&self, code: String)
    -> jsonrpsee::core::RpcResult<DeviceSignIn>;

    /// Sign that device in as the caller: it gets a session of its own. A
    /// code is good once.
    #[method(name = "deviceSignInApprove", with_extensions)]
    async fn device_sign_in_approve(&self, code: String) -> jsonrpsee::core::RpcResult<()>;

    /// Refuse it: the device is told so.
    #[method(name = "deviceSignInDeny", with_extensions)]
    async fn device_sign_in_deny(&self, code: String) -> jsonrpsee::core::RpcResult<()>;

    /// As `confirmPassword`, with one of the caller's own passkeys: begin
    /// with `passkeyLoginBegin` (or `/auth/passkey/begin`), then pass what
    /// the passkey signed. Another user's passkey is `PASSKEY_REFUSED`.
    #[method(name = "confirmPasskey", with_extensions)]
    async fn confirm_passkey(
        &self,
        ceremony: String,
        credential: serde_json::Value,
    ) -> jsonrpsee::core::RpcResult<()>;

    /// The host passkeys are bound to here; `NOT_CONFIGURED`, saying why,
    /// where the server cannot offer them (plain HTTP on another host than
    /// `localhost`).
    #[method(name = "passkeyHost", with_extensions)]
    async fn passkey_host(&self) -> jsonrpsee::core::RpcResult<PasskeyHost>;

    /// The caller's passkeys, oldest first.
    #[method(name = "passkeys", with_extensions)]
    async fn passkeys(&self) -> jsonrpsee::core::RpcResult<Vec<PasskeyEntry>>;

    /// Begin adding a passkey (P3-05). Needs a recent sign-in. The answer
    /// is what `navigator.credentials.create` takes, as JSON
    /// (`{"publicKey": PublicKeyCredentialCreationOptionsJSON}`, binary
    /// fields base64url); the challenge is kept for this session for 5
    /// minutes.
    #[method(name = "passkeyAddBegin", with_extensions)]
    async fn passkey_add_begin(&self) -> jsonrpsee::core::RpcResult<serde_json::Value>;

    /// Finish adding it: `credential` is what the browser made
    /// (`RegistrationResponseJSON`), `name` what the user calls it.
    #[method(name = "passkeyAddFinish", with_extensions)]
    async fn passkey_add_finish(
        &self,
        name: String,
        credential: serde_json::Value,
    ) -> jsonrpsee::core::RpcResult<PasskeyEntry>;

    #[method(name = "passkeyRename", with_extensions)]
    async fn passkey_rename(&self, id: String, name: String) -> jsonrpsee::core::RpcResult<()>;

    /// Remove one of the caller's passkeys. Needs a recent sign-in; their
    /// last way to sign in stays (`LAST_SIGN_IN_METHOD`).
    #[method(name = "passkeyRemove", with_extensions)]
    async fn passkey_remove(&self, id: String) -> jsonrpsee::core::RpcResult<()>;
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
    #[method(name = "setup", with_extensions)]
    async fn setup(&self, request: SetupRequest) -> jsonrpsee::core::RpcResult<LoggedIn>;

    /// Begin creating the first admin with a passkey instead of a password
    /// (P3-01, P3-05), under the same rules as `setup`: no users yet, the
    /// code from the server's log (`INVALID_SETUP_CODE`, limited as
    /// `setup` is). `NOT_CONFIGURED`, saying why, where the server has no
    /// passkeys.
    #[method(name = "setupPasskeyBegin", with_extensions)]
    async fn setup_passkey_begin(
        &self,
        request: PasskeySetupBegin,
    ) -> jsonrpsee::core::RpcResult<PasskeyChallenge>;

    /// Finish it: the admin is created with the passkey as their only way
    /// to sign in, and signed in. `ALREADY_SET_UP` if another setup
    /// finished first.
    #[method(name = "setupPasskeyFinish", with_extensions)]
    async fn setup_passkey_finish(
        &self,
        request: PasskeySetupFinish,
    ) -> jsonrpsee::core::RpcResult<LoggedIn>;

    #[method(name = "userList", with_extensions)]
    async fn user_list(&self) -> jsonrpsee::core::RpcResult<Vec<UserInfo>>;

    #[method(name = "userAdd", with_extensions)]
    async fn user_add(&self, user: NewUser) -> jsonrpsee::core::RpcResult<UserInfo>;

    /// Set anyone's password, without the current one; for a user with
    /// passkeys only, that adds one.
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

    /// A user's live sessions on every device, most recently used first.
    #[method(name = "userSessions", with_extensions)]
    async fn user_sessions(
        &self,
        username: String,
    ) -> jsonrpsee::core::RpcResult<Vec<SessionEntry>>;

    /// End one of a user's sessions, by its id from `userSessions`.
    #[method(name = "userRevokeSession", with_extensions)]
    async fn user_revoke_session(
        &self,
        username: String,
        id: String,
    ) -> jsonrpsee::core::RpcResult<()>;

    /// A user's passkeys (P3-05). An admin can see and remove them, never
    /// add one: only its user can.
    #[method(name = "userPasskeys", with_extensions)]
    async fn user_passkeys(
        &self,
        username: String,
    ) -> jsonrpsee::core::RpcResult<Vec<PasskeyEntry>>;

    /// Remove one of a user's passkeys, by its id from `userPasskeys`.
    #[method(name = "userRemovePasskey", with_extensions)]
    async fn user_remove_passkey(
        &self,
        username: String,
        id: String,
    ) -> jsonrpsee::core::RpcResult<()>;

    /// Plex now: reachable or not, its name, and the libraries with their
    /// item counts. Plex being down is an answer here, not an error.
    #[method(name = "plexStatus", with_extensions)]
    async fn plex_status(&self) -> jsonrpsee::core::RpcResult<PlexStatus>;

    /// Check every media path of every library, or of the one titled
    /// `library`, as `plex check-paths` does. Reads the whole library: it
    /// takes a while.
    #[method(name = "plexCheckPaths", with_extensions)]
    async fn plex_check_paths(
        &self,
        library: Option<String>,
    ) -> jsonrpsee::core::RpcResult<Vec<PathReport>>;

    /// The server's name, key, version and encoders.
    #[method(name = "serverStatus", with_extensions)]
    async fn server_status(&self) -> jsonrpsee::core::RpcResult<ServerStatus>;

    /// Sync watch state with Plex now (PX-04); with `dryRun`, only say
    /// what would change.
    #[method(name = "plexWatchSync", with_extensions)]
    async fn plex_watch_sync(&self, dry_run: bool) -> jsonrpsee::core::RpcResult<PlexSyncReport>;
}
