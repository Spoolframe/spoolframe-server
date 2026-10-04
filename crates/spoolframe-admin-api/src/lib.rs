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
pub use spoolframe_types::{LibraryKind, Role};

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
    /// A setting the server's locked config file sets (P3-10): only that
    /// file can change it.
    pub const SETTING_LOCKED: i32 = -32020;
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
    /// The tesseract languages bitmap subtitles are read as text in
    /// (P2-07), as tesseract names them (`eng`); none when OCR is off.
    pub ocr_languages: Vec<String>,
    /// Where the web UI answers (P3-10): `http://<address>:<port>/`.
    pub web_urls: Vec<String>,
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
    /// The PMS's version.
    pub version: Option<String>,
    /// Where the server reaches it: `plex.url`, without the token.
    pub url: Option<String>,
    /// Every library section Spoolframe can serve, served or not.
    #[serde(default)]
    pub libraries: Vec<LibraryStatus>,
    /// The titles of its sections Spoolframe does not serve (photos,
    /// other videos), with what they hold: `Photos · photo`.
    #[serde(default)]
    pub unsupported: Vec<UnsupportedSection>,
}

/// One library section of a Plex server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryStatus {
    /// Its key in Plex (P3-11): what `libraryAdd` takes.
    pub section: String,
    /// Plex's name for it.
    pub title: String,
    pub kind: spoolframe_types::LibraryKind,
    /// Its top-level items: movies, shows or artists.
    pub items: u32,
    /// The Spoolframe library it is, when it is one.
    pub library: Option<String>,
}

/// A library section Spoolframe does not serve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnsupportedSection {
    pub title: String,
    /// Plex's `type`: `photo`, `clip`.
    pub plex_type: Option<String>,
}

/// The libraries clients see and the Plex servers they come from (P3-11).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibrariesOverview {
    /// In the order clients show them.
    pub libraries: Vec<LibraryEntry>,
    /// The first server first.
    pub servers: Vec<PlexServerEntry>,
    /// Libraries have been chosen. While not, every library of the first
    /// Plex server is served under Plex's names, and the first change
    /// chooses them all before it is made.
    pub chosen: bool,
    /// The locked config file sets the libraries: only it can change them.
    pub locked: bool,
}

/// One library as clients see it, and how it is doing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryEntry {
    /// What clients know it by: `plex:2`, `plex:cabin:1`.
    pub id: String,
    /// Spoolframe's name for it.
    pub name: String,
    pub kind: spoolframe_types::LibraryKind,
    /// Its Plex server's id, and what Spoolframe calls that server.
    pub server: String,
    pub server_name: String,
    /// Its section's key in Plex, and Plex's name for it when the server
    /// answers.
    pub section: String,
    pub plex_title: Option<String>,
    /// Its top-level items, when the server answers.
    pub items: Option<u32>,
    /// A few of its media files checked with the saved path mappings.
    pub paths: Option<LibraryPathCheck>,
    /// Why it is unavailable: its server is down, or no longer has it.
    pub error: Option<String>,
    /// Which way its watch state syncs with linked Plex accounts (P3-12).
    pub sync: SyncDirection,
}

/// Which way a library's watch state is synced with each linked user's
/// Plex account (P3-12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SyncDirection {
    /// Spoolframe keeps its own watched state.
    Off,
    /// Plays in Plex mark things watched here; nothing is written to Plex.
    FromPlex,
    /// Plays here mark things watched in Plex; nothing is taken from it.
    ToPlex,
    /// Both ways: the newer change wins, and nothing bounces back.
    Both,
}

impl SyncDirection {
    /// Plex's state is taken into Spoolframe.
    pub fn pulls(self) -> bool {
        matches!(self, SyncDirection::FromPlex | SyncDirection::Both)
    }

    /// Spoolframe's state is sent to Plex.
    pub fn pushes(self) -> bool {
        matches!(self, SyncDirection::ToPlex | SyncDirection::Both)
    }

    /// As the config file says it: `off`, `from_plex`, `to_plex`, `both`.
    pub fn config_name(self) -> &'static str {
        match self {
            SyncDirection::Off => "off",
            SyncDirection::FromPlex => "from_plex",
            SyncDirection::ToPlex => "to_plex",
            SyncDirection::Both => "both",
        }
    }

    pub fn from_config_name(name: &str) -> Option<Self> {
        [
            SyncDirection::Off,
            SyncDirection::FromPlex,
            SyncDirection::ToPlex,
            SyncDirection::Both,
        ]
        .into_iter()
        .find(|d| d.config_name() == name)
    }
}

/// A Plex server the libraries come from (P3-11).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlexServerEntry {
    /// `plex` for the first, else the id it was added under.
    pub id: String,
    /// What Spoolframe calls it: Plex's own name when it was added.
    pub name: String,
    /// Reachable or not, and its sections, served or not.
    pub status: PlexStatus,
    /// The server has a token for it (the token itself is never shown).
    pub token_set: bool,
    /// The locked config file says where its token is.
    pub token_locked: bool,
    /// The locked config file sets it: only that file can change it.
    pub locked: bool,
    pub path_mapping: Vec<PathMappingEntry>,
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

/// Where a setting's value comes from (P3-10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SettingLayer {
    /// The built-in defaults.
    Default,
    /// The server's config file (`--config`).
    File,
    /// Changed in the web UI or with `config set`.
    Runtime,
}

/// When a change to a setting takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SettingApply {
    /// At once.
    Live,
    /// At the next start.
    Restart,
}

/// One setting as the server has it now (P3-10).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingEntry {
    /// Dotted, as `config show` names it: `plex.url`.
    pub key: String,
    /// The value in effect, as JSON; `None` when no layer sets it, and
    /// always for a secret.
    pub value: Option<serde_json::Value>,
    /// A secret (the Plex token): never shown, only whether it is set.
    pub secret: bool,
    /// For a secret, whether it is set; otherwise whether `value` is.
    pub set: bool,
    /// Where `value` comes from; `None` when nothing sets it.
    pub layer: Option<SettingLayer>,
    /// Set by the server's config file, which is locked: it cannot be
    /// changed here.
    pub locked: bool,
    /// What the config file says, when a runtime value overrides it.
    pub overrides: Option<serde_json::Value>,
    pub apply: SettingApply,
    /// A restart setting changed since the server started: what the
    /// server runs with is not `value` yet.
    pub restart_pending: bool,
}

/// Every setting, and what the page around them needs to know.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub entries: Vec<SettingEntry>,
    /// The server's config file, if it has one.
    pub config_file: Option<String>,
    /// The server runs under a supervisor (systemd) that starts it again,
    /// so `admin.restart` can apply restart settings.
    pub can_restart: bool,
}

/// The setup wizard's progress (P3-10): which steps are confirmed, by
/// their ids (`admin`, `server`, `plex`, …), and whether it is over.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupState {
    pub finished: bool,
    pub done: Vec<String>,
}

/// A Plex Media Server that answered Plex's LAN discovery (GDM).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredPlex {
    pub name: String,
    /// `http://<address>:<port>`.
    pub url: String,
    pub version: Option<String>,
}

/// A plex.tv PIN to approve (Sign in with Plex).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlexPin {
    /// For `plexPinCheck`.
    pub id: u64,
    /// What a person types at plex.tv/link.
    pub code: String,
    /// Plex's sign-in page for it, to open in a new tab.
    pub auth_url: String,
    /// Seconds until plex.tv forgets it.
    pub expires_in: Option<u64>,
}

/// Where a PIN is. The token itself never leaves the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum PlexPinState {
    Waiting,
    /// Approved: the server keeps the token as `plex.token`. `account` is
    /// who it signs in, when plex.tv says.
    Linked {
        account: Option<String>,
    },
    /// plex.tv has forgotten it: begin another.
    Expired,
}

/// A user's link to their Plex account (P3-12). The token never leaves
/// the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlexLink {
    /// The Plex account's username and email, as plex.tv said when it was
    /// linked; both `None` for a link the config makes.
    pub username: Option<String>,
    pub email: Option<String>,
    /// Linked by the server's config file (`plex.watch_sync.user`) as the
    /// Plex servers' owner, with each server's own token: only that file
    /// can unlink it.
    pub by_config: bool,
    /// When it was linked, Unix seconds; `None` for the config's.
    pub linked_at: Option<i64>,
}

/// Where linking a Plex account is (`me.plexLinkCheck`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum PlexLinkState {
    /// Not approved at plex.tv yet: ask again in a few seconds.
    Waiting,
    /// Linked: the token is kept as the user's secret.
    Linked { link: PlexLink },
    /// plex.tv has forgotten the PIN (it expired, or was not allowed):
    /// nothing was linked.
    Expired,
}

/// A user and their Plex link, for the admin's Plex sync panel and user
/// page, and for My profile (P3-12).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlexUser {
    pub username: String,
    pub display_name: Option<String>,
    /// `None`: not linked, and not synced.
    pub link: Option<PlexLink>,
    /// Their last sync run since the server started, if any.
    pub last_run: Option<PlexSyncRun>,
    /// A run of theirs is going on now.
    pub running: bool,
}

/// What one user's sync run did (P3-12).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlexSyncRun {
    /// When it ended, Unix seconds.
    pub at: i64,
    /// The libraries synced.
    pub libraries: u32,
    /// Items whose state was taken from Plex, sent to Plex, left as they
    /// were (the same on both sides, or held back by a library's
    /// direction), and that failed.
    pub pulled: u32,
    pub pushed: u32,
    pub skipped: u32,
    pub failed: u32,
    /// What was skipped whole, for people: "Cabin movies skipped: Cabin
    /// can't be reached".
    pub notes: Vec<String>,
}

/// Every user, their Plex link and their last run (P3-12).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlexSyncOverview {
    /// How often each linked user is synced when nothing changes.
    pub interval_secs: u64,
    pub users: Vec<PlexUser>,
}

/// A Plex Media Server the caller's linked Plex account reaches, for
/// adding its libraries (P3-11's "From your Plex account").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlexAccountServer {
    pub name: String,
    /// Plex's id for it (its machine identifier).
    pub machine_id: String,
    /// The address to add it by: one on the LAN if plex.tv knows one, else
    /// a direct one; `None` when it is reachable only through Plex's relay.
    pub url: Option<String>,
    /// `url` is on the LAN.
    pub local: bool,
    /// The account's own server, rather than one shared with it.
    pub owned: bool,
    /// The id it is added under here, if it is.
    pub added: Option<String>,
}

/// One `plex.path_mapping` rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathMappingEntry {
    /// The Plex library it is for; every library if `None`.
    pub library: Option<String>,
    pub from: String,
    pub to: String,
}

/// One library's sample of media files, checked with some mappings
/// (P3-10): the wizard's live check, before the mappings are saved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryPathCheck {
    pub library: String,
    /// The library's folders, as Plex sees them.
    pub roots: Vec<String>,
    /// How many files were looked at.
    pub checked: u64,
    pub ok: u64,
    /// `"<plex path> -> <local path>: <reason>"`, a few.
    pub failures: Vec<String>,
    /// A mapping that would make the failing files readable, found by
    /// looking for them in the usual mount points.
    pub suggestion: Option<PathMappingEntry>,
}

/// What one user's Plex watch sync run did, or would do (PX-04, P3-12):
/// one line per item.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlexSyncReport {
    pub username: String,
    pub pulled: Vec<String>,
    pub pushed: Vec<String>,
    pub failed: Vec<String>,
    /// Items left as they were.
    pub skipped: u32,
    /// What was skipped whole, as `PlexSyncRun.notes`.
    pub notes: Vec<String>,
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

    /// The caller, their Plex link and their last sync run (P3-12).
    #[method(name = "plexLink", with_extensions)]
    async fn plex_link(&self) -> jsonrpsee::core::RpcResult<PlexUser>;

    /// Begin linking the caller's Plex account: a plex.tv PIN, whose
    /// `authUrl` the page opens. `UNAVAILABLE` when plex.tv cannot be
    /// reached.
    #[method(name = "plexLinkBegin", with_extensions)]
    async fn plex_link_begin(&self) -> jsonrpsee::core::RpcResult<PlexPin>;

    /// Whether the PIN is approved yet; once it is, the account's token is
    /// kept as the caller's secret and their state syncs with it. Only a
    /// PIN the caller began (`INVALID_INPUT` otherwise).
    #[method(name = "plexLinkCheck", with_extensions)]
    async fn plex_link_check(&self, id: u64) -> jsonrpsee::core::RpcResult<PlexLinkState>;

    /// Unlink the caller's Plex account: the token is deleted and their
    /// state stops syncing. Watch state stays as it is on both sides.
    /// `NOT_CONFIGURED` for a link the config file makes.
    #[method(name = "plexUnlink", with_extensions)]
    async fn plex_unlink(&self) -> jsonrpsee::core::RpcResult<()>;
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

    /// Sync `username`'s watch state with their Plex account now, or
    /// every linked user's without one (P3-12), each in turn; with
    /// `dryRun`, only say what would change. `NOT_CONFIGURED` for a user
    /// who has not linked a Plex account.
    #[method(name = "plexWatchSync", with_extensions)]
    async fn plex_watch_sync(
        &self,
        username: Option<String>,
        dry_run: bool,
    ) -> jsonrpsee::core::RpcResult<Vec<PlexSyncReport>>;

    /// Every user, their Plex link and their last sync run (P3-12): who
    /// is linked to which Plex account, and how their sync is going.
    #[method(name = "plexSync", with_extensions)]
    async fn plex_sync(&self) -> jsonrpsee::core::RpcResult<PlexSyncOverview>;

    /// Unlink `username`'s Plex account, as `me.plexUnlink` does. An admin
    /// can unlink, not link.
    #[method(name = "userPlexUnlink", with_extensions)]
    async fn user_plex_unlink(&self, username: String) -> jsonrpsee::core::RpcResult<()>;

    /// The Plex servers the caller's linked Plex account reaches (P3-11's
    /// "From your Plex account"): `NOT_CONFIGURED` without a link, and
    /// `UNAVAILABLE` when plex.tv cannot be reached.
    #[method(name = "plexAccountServers", with_extensions)]
    async fn plex_account_servers(&self) -> jsonrpsee::core::RpcResult<Vec<PlexAccountServer>>;

    /// Every setting: its value, where it comes from, and whether it
    /// applies now or after a restart (P3-10).
    #[method(name = "settings", with_extensions)]
    async fn settings(&self) -> jsonrpsee::core::RpcResult<Settings>;

    /// Set `key` in the runtime settings. The value is checked as a config
    /// file is at start: one the server would refuse is `INVALID_INPUT`
    /// with the message the file would get. `SETTING_LOCKED` for what the
    /// locked config file sets. A live setting applies at once; a restart
    /// one is saved, and `restartPending`. `plex.token` is kept as a
    /// secret, never in the runtime settings.
    #[method(name = "settingSet", with_extensions)]
    async fn setting_set(
        &self,
        key: String,
        value: serde_json::Value,
    ) -> jsonrpsee::core::RpcResult<SettingEntry>;

    /// Take `key` out of the runtime settings: the config file's value or
    /// the default applies again.
    #[method(name = "settingUnset", with_extensions)]
    async fn setting_unset(&self, key: String) -> jsonrpsee::core::RpcResult<SettingEntry>;

    /// Stop the server so its supervisor starts it again, with the restart
    /// settings applied. `NOT_CONFIGURED` where nothing would start it
    /// again (`Settings::can_restart`).
    #[method(name = "restart", with_extensions)]
    async fn restart(&self) -> jsonrpsee::core::RpcResult<()>;

    /// The setup wizard's progress (P3-10).
    #[method(name = "setupState", with_extensions)]
    async fn setup_state(&self) -> jsonrpsee::core::RpcResult<SetupState>;

    /// Step `step` is confirmed: a wizard opened later resumes after it.
    #[method(name = "setupStepDone", with_extensions)]
    async fn setup_step_done(&self, step: String) -> jsonrpsee::core::RpcResult<SetupState>;

    /// The wizard is over: the web UI opens the admin pages from now on.
    #[method(name = "setupFinish", with_extensions)]
    async fn setup_finish(&self) -> jsonrpsee::core::RpcResult<SetupState>;

    /// The Plex Media Servers that answer Plex's LAN discovery, within
    /// about a second and a half.
    #[method(name = "plexDiscover", with_extensions)]
    async fn plex_discover(&self) -> jsonrpsee::core::RpcResult<Vec<DiscoveredPlex>>;

    /// The PMS at `url` as `plexStatus` would show it, without saving
    /// anything: with `token`, or else the one the server has.
    #[method(name = "plexProbe", with_extensions)]
    async fn plex_probe(
        &self,
        url: String,
        token: Option<String>,
    ) -> jsonrpsee::core::RpcResult<PlexStatus>;

    /// Begin signing in with Plex: a plex.tv PIN. `UNAVAILABLE` when
    /// plex.tv cannot be reached.
    #[method(name = "plexPinBegin", with_extensions)]
    async fn plex_pin_begin(&self) -> jsonrpsee::core::RpcResult<PlexPin>;

    /// Whether PIN `id` (from `plexPinBegin`) is approved; once it is, the
    /// server keeps the account's token as `plex.token`.
    #[method(name = "plexPinCheck", with_extensions)]
    async fn plex_pin_check(&self, id: u64) -> jsonrpsee::core::RpcResult<PlexPinState>;

    /// A few media files of each library, checked with `mappings` instead
    /// of the saved ones, with a suggested mapping where they fail. Of
    /// Plex server `server` (the first if left out), and of its sections
    /// `sections` (its served ones if left out): a library about to be
    /// added is checked before it is (P3-11).
    #[method(name = "plexPathCheck", with_extensions)]
    async fn plex_path_check(
        &self,
        mappings: Vec<PathMappingEntry>,
        server: Option<String>,
        sections: Option<Vec<String>>,
    ) -> jsonrpsee::core::RpcResult<Vec<LibraryPathCheck>>;

    /// The libraries clients see and the Plex servers they come from, each
    /// asked how it is doing now (P3-11). A server that is down is an
    /// answer here, not an error.
    #[method(name = "libraries", with_extensions)]
    async fn libraries(&self) -> jsonrpsee::core::RpcResult<LibrariesOverview>;

    /// Add section `section` of Plex server `server` as a library, last,
    /// called `name` (Plex's name if left out); its id. The server must
    /// answer, to say what the section holds. `INVALID_INPUT` for a
    /// section that is a library already, or that Spoolframe cannot
    /// serve.
    #[method(name = "libraryAdd", with_extensions)]
    async fn library_add(
        &self,
        server: String,
        section: String,
        name: Option<String>,
    ) -> jsonrpsee::core::RpcResult<String>;

    /// Call library `id` `name` in Spoolframe; Plex keeps its own name.
    #[method(name = "libraryRename", with_extensions)]
    async fn library_rename(&self, id: String, name: String) -> jsonrpsee::core::RpcResult<()>;

    /// Put library `id` at `position` (0 is first) in the order clients
    /// show.
    #[method(name = "libraryMove", with_extensions)]
    async fn library_move(&self, id: String, position: u32) -> jsonrpsee::core::RpcResult<()>;

    /// Sync library `id`'s watch state with linked Plex accounts this way
    /// (P3-12).
    #[method(name = "librarySetSync", with_extensions)]
    async fn library_set_sync(
        &self,
        id: String,
        sync: SyncDirection,
    ) -> jsonrpsee::core::RpcResult<()>;

    /// Stop serving library `id`. Its watch state is kept: adding it back
    /// restores it. Nothing changes in Plex.
    #[method(name = "libraryRemove", with_extensions)]
    async fn library_remove(&self, id: String) -> jsonrpsee::core::RpcResult<()>;

    /// Add the Plex server at `url`, with `token`, or else the one the
    /// caller's linked Plex account has for that server (P3-12), or else
    /// the token the first server has, which works for every server of its
    /// account.
    /// It must answer; it is named by what it calls itself. Without a
    /// first server, it becomes the first. No library is added.
    #[method(name = "plexServerAdd", with_extensions)]
    async fn plex_server_add(
        &self,
        url: String,
        token: Option<String>,
    ) -> jsonrpsee::core::RpcResult<PlexServerEntry>;

    /// Change where Plex server `id` is reached, or what it is called.
    #[method(name = "plexServerUpdate", with_extensions)]
    async fn plex_server_update(
        &self,
        id: String,
        url: Option<String>,
        name: Option<String>,
    ) -> jsonrpsee::core::RpcResult<()>;

    /// Keep `token` as Plex server `id`'s, as a secret.
    #[method(name = "plexServerSetToken", with_extensions)]
    async fn plex_server_set_token(
        &self,
        id: String,
        token: String,
    ) -> jsonrpsee::core::RpcResult<()>;

    /// Set Plex server `id`'s path mappings.
    #[method(name = "plexServerSetPaths", with_extensions)]
    async fn plex_server_set_paths(
        &self,
        id: String,
        mappings: Vec<PathMappingEntry>,
    ) -> jsonrpsee::core::RpcResult<()>;

    /// Remove Plex server `id`, its libraries and its token. Their watch
    /// state is kept. The first server cannot be removed (`INVALID_INPUT`):
    /// its address and token can be changed.
    #[method(name = "plexServerRemove", with_extensions)]
    async fn plex_server_remove(&self, id: String) -> jsonrpsee::core::RpcResult<()>;
}
