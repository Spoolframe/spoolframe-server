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
pub use spoolframe_types::{
    Album, Artist, Artwork, ArtworkKind, AudioRole, Episode, Item, LibraryKind, Marker, MarkerKind,
    Movie, Platform, PlaybackState, Role, SdhPreference, Season, Show, SubtitleMode, SubtitleRole,
    Track, TrackPreferences,
};

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
    /// Not one of the user's linked sign-in provider accounts (P3-04).
    pub const NO_SUCH_LINK: i32 = -32021;
    /// The provider account is linked to another user, or the user has
    /// one linked at that provider already (P3-04).
    pub const LINK_REFUSED: i32 = -32022;
    /// Not a playback open now or ended in the last hour (P3-15).
    pub const NO_SUCH_PLAYBACK: i32 = -32023;
    /// End conversion on a playback that plays the file directly (P3-15).
    pub const NO_CONVERSION: i32 = -32024;
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
    /// When the password was set or last changed, Unix seconds; `None`
    /// without one.
    pub password_changed_at: Option<i64>,
    /// When any of their sessions was last used, Unix seconds; `None` if
    /// they have never signed in.
    #[serde(default)]
    pub last_seen_at: Option<i64>,
    /// They have a passkey (P3-05): with `has_password`, how they can sign
    /// in.
    pub has_passkey: bool,
    /// They have linked a sign-in provider account (P3-04).
    pub has_sso: bool,
    /// They have a profile PIN (P2-08): `admin.userClearPin` has
    /// something to clear, and a device can be set to ask for it.
    pub has_pin: bool,
    /// The devices they have signed in from, as `admin.userDevices` lists
    /// them.
    pub device_count: u64,
    /// Their live sessions, as `admin.userSessions` lists them.
    pub session_count: u64,
    /// Seconds until password sign-ins as them are checked again, after
    /// too many wrong ones under their username (P1-11, P3-18); `None`
    /// when they are checked now.
    pub rate_limited_secs: Option<u64>,
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
    /// What kind of client the device runs, for a client session: for an
    /// admin API session `Web` or `Cli`, by what signed it in (P3-18).
    /// `None` for a device not seen since before the server kept it, or
    /// an admin API session from before it kept which.
    pub platform: Option<Platform>,
}

/// One of a user's devices (P2-08).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceEntry {
    /// Hex `EndpointId`.
    pub endpoint_id: String,
    #[serde(default)]
    pub name: Option<String>,
    /// What kind of client it runs; `None` for a device not seen since
    /// before the server kept it.
    pub platform: Option<Platform>,
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
    /// host (before `admin.public_url` changed) signs no one in. An app
    /// passkey is always usable, in the apps.
    pub usable: bool,
    /// Made in a Spoolframe app (P3-19), for the relying party every server
    /// shares (`spoolframe.app`), and of use in the apps only: shown as
    /// "Spoolframe app · <name>", where its name is the device's.
    pub app: bool,
}

/// A user's account at a sign-in provider, linked to them (P3-04). Linking
/// happens in the browser (`/auth/oidc/begin`, then the provider, then
/// `/auth/oidc/callback`), never over this API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SsoLink {
    /// The provider's name as the config gives it; `None` when no
    /// configured provider has this issuer any more (the link signs no
    /// one in then).
    pub provider: Option<String>,
    /// The provider's issuer: what `ssoUnlink` takes.
    pub issuer: String,
    /// Its email or username at the provider, when the provider said, for
    /// recognising it.
    pub display: Option<String>,
    /// Unix seconds.
    pub linked_at: i64,
    /// When it last signed them in; `None` if it never has.
    pub last_used_at: Option<i64>,
}

/// A device waiting to be signed in by someone approving its code (P3-07).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceSignIn {
    /// What the device calls itself; `None` if it gave no name.
    pub device_name: Option<String>,
    /// The app on it, e.g. `spoolframe-linux`.
    pub client_name: String,
    /// What kind of client it runs, as it said when it connected.
    pub platform: Platform,
    /// Unix seconds.
    pub requested_at: i64,
}

/// What the server is and can do, for an admin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerStatus {
    pub name: String,
    /// Spoolframe's own version, the crate's.
    pub version: String,
    /// The source revision it was built from (P3-18), when the build
    /// named one: the nix package does, a plain `cargo build` does not.
    pub revision: Option<String>,
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
    /// How many files frame previews are made of ahead at once when
    /// `previews.workers` is not set (P3-13).
    pub preview_workers_default: DefaultWorkers,
}

/// A default worked out from the machine's CPUs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DefaultWorkers {
    pub count: u32,
    pub counted: CoreCount,
}

/// What a [`DefaultWorkers`] counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CoreCount {
    /// The machine's physical cores.
    PhysicalCores,
    /// The CPUs the server may use, fewer than the cores.
    AvailableCpus,
    /// The logical CPUs the server may use: the cores are not known.
    LogicalCpus,
}

impl CoreCount {
    /// As the settings page says it: "8 (physical cores)".
    pub fn label(self) -> &'static str {
        match self {
            Self::PhysicalCores => "physical cores",
            Self::AvailableCpus => "CPUs it may use",
            Self::LogicalCpus => "logical CPUs",
        }
    }
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
    /// Its section's key in Plex, and Plex's name for it: as the server
    /// answers, or, while it is down, as it last answered (P3-18); `None`
    /// for a server that has never answered here.
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

/// A check of every library's media paths (`plexCheckPaths`), and when
/// it finished (P3-18).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathCheckRun {
    /// Unix seconds.
    pub at: i64,
    pub reports: Vec<PathReport>,
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
    /// Plex's id for it, as [`PlexAccountServer::machine_id`]: the same
    /// server found and listed by the account is one choice.
    pub machine_id: Option<String>,
}

/// A plex.tv PIN to approve (Sign in with Plex).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlexPin {
    /// For `plexPinCheck`.
    pub id: u64,
    /// Plex's sign-in page for it, to open in a new window. The PIN is a
    /// strong one, whose code is not for typing at plex.tv/link: it is
    /// in this address, and nowhere else.
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

/// What a user picked for one series or movie (P1-20), which its next
/// playback uses instead of their preferences (P3-03 lists them).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackOverrideEntry {
    /// The series' or movie's id: what `me.clearTrackOverride` takes.
    pub id: String,
    /// Its title, when the catalog can say.
    pub title: Option<String>,
    /// The audio picked; `None` follows the preferences.
    pub audio: Option<AudioPick>,
    /// The subtitles picked; `None` follows the preferences.
    pub subtitle: Option<SubtitlePick>,
}

/// An audio track, by what it is rather than where.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioPick {
    /// ISO 639; `None` for a track that does not say.
    pub language: Option<String>,
    pub role: AudioRole,
}

/// Subtitles picked: a track by what it is, or none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SubtitlePick {
    Track {
        language: Option<String>,
        role: SubtitleRole,
    },
    Off,
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

impl LibraryPathCheck {
    /// Why the library cannot be added, when none of its sample files is
    /// on this machine: Spoolframe plays the files itself (P1-04), so a
    /// library whose folders are not mounted here would play nothing.
    /// `None` when any is found, or when there were none to look at.
    ///
    /// One sentence for the add flow and `libraryAdd` alike, so both say
    /// the same thing.
    pub fn refusal(&self) -> Option<String> {
        if self.checked == 0 || self.ok > 0 {
            return None;
        }
        let none = match self.checked {
            1 => "its sample file is not on this machine".to_owned(),
            n => format!("none of its {n} sample files are on this machine"),
        };
        let mount = match self.roots.as_slice() {
            [] => "Mount its folder here, or map it to where it is".to_owned(),
            [one] => format!("Mount {one} here, or map it to where it is"),
            [rest @ .., last] => format!(
                "Mount {} and {last} here, or map them to where they are",
                rest.join(", ")
            ),
        };
        Some(format!("{} can't be added: {none}. {mount}.", self.library))
    }
}

#[cfg(test)]
mod path_check_tests {
    use super::*;

    fn check(roots: &[&str], checked: u64, ok: u64) -> LibraryPathCheck {
        LibraryPathCheck {
            library: "Movies".into(),
            roots: roots.iter().map(|r| (*r).to_owned()).collect(),
            checked,
            ok,
            failures: Vec::new(),
            suggestion: None,
        }
    }

    #[test]
    fn a_library_none_of_whose_sample_is_here_is_refused_naming_its_folder() {
        assert_eq!(
            check(&["/data/movies"], 5, 0).refusal().as_deref(),
            Some(
                "Movies can't be added: none of its 5 sample files are on this machine. \
                 Mount /data/movies here, or map it to where it is."
            )
        );
        assert_eq!(
            check(&["/data/a", "/data/b"], 1, 0).refusal().as_deref(),
            Some(
                "Movies can't be added: its sample file is not on this machine. \
                 Mount /data/a and /data/b here, or map them to where they are."
            )
        );
    }

    #[test]
    fn a_library_with_any_sample_file_here_or_none_to_look_at_is_not_refused() {
        assert_eq!(check(&["/data/movies"], 5, 1).refusal(), None);
        assert_eq!(check(&["/data/movies"], 0, 0).refusal(), None);
    }
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

/// What kind of item a catalog row is (P3-16).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CatalogKind {
    Movie,
    Show,
    Season,
    Episode,
    Artist,
    Album,
    Track,
}

/// How many items of one kind a library has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KindCount {
    pub kind: CatalogKind,
    pub count: u32,
}

/// A quick filter of a library's rows (P3-16).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CatalogFilter {
    /// The files the library's last path check found missing: the same
    /// list `plexCheckPaths` and `plex check-paths` report, one row per
    /// movie, episode or track.
    Missing,
    /// Items Plex has not matched to an agent.
    Unmatched,
    /// Items with no artwork: clients show a placeholder.
    NoArtwork,
}

/// Where a library's check of every media file is (P3-16). The server
/// keeps the last one of each library until its Plex servers, libraries
/// or path mappings change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum PathsChecked {
    /// Not since the server started, or since the mappings changed.
    NotChecked,
    /// Running, since `since` (Unix seconds).
    #[serde(rename_all = "camelCase")]
    Checking { since: i64 },
    /// Done at `at` (Unix seconds): `ok` files found, `missing` not.
    #[serde(rename_all = "camelCase")]
    Checked { at: i64, ok: u64, missing: u64 },
    /// It could not run: the Plex server did not answer.
    #[serde(rename_all = "camelCase")]
    Failed { at: i64, error: String },
}

/// One library as an admin browses it (P3-16).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogLibrary {
    /// What clients know it by: `plex:2`.
    pub id: String,
    /// Spoolframe's name for it.
    pub name: String,
    pub kind: LibraryKind,
    /// Its Plex server's id and name, and Plex's name for the section,
    /// when the server answers.
    pub server: String,
    pub server_name: String,
    pub plex_title: Option<String>,
    /// How many items of each kind it has, top level first.
    pub counts: Vec<KindCount>,
    /// Its top-level items Plex has not matched, and with no artwork.
    pub unmatched: u32,
    pub no_artwork: u32,
    pub paths: PathsChecked,
}

/// What `catalogRows` lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogQuery {
    pub library: String,
    /// Only items of this kind; the library's top level (movies, shows,
    /// albums) if left out, and with `text`, every kind a title search
    /// finds there.
    pub kind: Option<CatalogKind>,
    /// Only items whose titles match, as P3-09's search matches them.
    pub text: Option<String>,
    pub filter: Option<CatalogFilter>,
    pub offset: u32,
    /// At most 200.
    pub limit: u32,
}

/// One page of a library's rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogPage {
    pub rows: Vec<CatalogRow>,
    /// How many rows the whole listing has.
    pub total: u32,
    /// The library's path check, which `Missing` lists and container rows
    /// count their missing files from.
    pub paths: PathsChecked,
}

/// An item as the catalog lists it to an admin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogRow {
    /// As clients get it.
    pub item: Item,
    /// Plex matched it to an agent.
    pub matched: bool,
    /// Plex's GUID, `plex://show/…`, or `local://…` when unmatched.
    pub guid: Option<String>,
    /// Other agents' ids: `imdb://tt…`, `tmdb://…`, `tvdb://…`.
    pub agents: Vec<String>,
    /// Unix seconds.
    pub added_at: Option<i64>,
    pub updated_at: Option<i64>,
    /// A show's seasons, an album's tracks.
    pub children: Option<u32>,
    /// A show's or season's episodes.
    pub leaves: Option<u32>,
    /// A movie's, episode's or track's files, looked for when listed;
    /// under `Missing`, as the path check found them.
    pub files: Vec<FileState>,
    /// A show's, season's, artist's or album's files the last path check
    /// found missing; `None` until there is one.
    pub missing: Option<u64>,
}

/// One media file: where Plex has it, where it is here, whether it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileState {
    pub plex_path: String,
    /// The path looked at here.
    pub mapped_path: String,
    /// The `plex.path_mapping` rule used; `None` when none covers the
    /// Plex path, which is then looked at as it is.
    pub mapping: Option<PathMappingEntry>,
    pub found: bool,
    /// Why it cannot be read, when it cannot.
    pub problem: Option<String>,
}

/// One item as an admin inspects it (P3-16).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogItem {
    /// The library it is in, by id and name, when it is one served.
    pub library: Option<CatalogCrumb>,
    /// Its show and season, or artist and album, outermost first.
    pub ancestors: Vec<CatalogCrumb>,
    /// The Plex server it comes from.
    pub server: String,
    pub server_name: String,
    /// Exactly what `spoolframe/client/1` answers `Item` with, as JSON:
    /// `{"type": "item", "item": …, "children": […], "markers": […]}`,
    /// with the caller's own playback state.
    pub reply: serde_json::Value,
    pub matched: bool,
    pub guid: Option<String>,
    /// Its stable ids (P1-14): Plex's GUID and the agents', as watch
    /// state is linked by them.
    pub external_ids: Vec<String>,
    pub added_at: Option<i64>,
    pub updated_at: Option<i64>,
    /// A show's or an artist's folders, as Plex has them.
    pub folders: Vec<String>,
    /// Each version of its media, the one played first; each file looked
    /// for now.
    pub versions: Vec<MediaVersion>,
    /// Its children, as rows.
    pub children: Vec<CatalogRow>,
    /// The Plex web app's page for it, when the server says its id.
    pub open_in_plex: Option<String>,
    /// The library's path check, which `children` count from.
    pub paths: PathsChecked,
}

/// An item named in a trail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogCrumb {
    pub id: String,
    pub title: String,
}

/// One version of an item's media, as Plex describes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaVersion {
    /// Plex's media id.
    pub id: Option<u64>,
    pub container: Option<String>,
    pub video_codec: Option<String>,
    /// `1080`, `4k`, `sd`, as Plex says it.
    pub video_resolution: Option<String>,
    pub video_profile: Option<String>,
    pub audio_codec: Option<String>,
    pub audio_channels: Option<u32>,
    /// kbit/s.
    pub bitrate: Option<u64>,
    pub duration_ms: Option<u64>,
    pub parts: Vec<MediaPart>,
}

/// One file of a version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaPart {
    /// Plex's part id.
    pub id: Option<u64>,
    /// Bytes.
    pub size: Option<u64>,
    pub duration_ms: Option<u64>,
    pub file: FileState,
}

/// One piece of an item's artwork, as the server has it (P3-16).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtworkInfo {
    /// The `ArtworkId` clients fetch it by.
    pub id: String,
    pub kind: ArtworkKind,
    /// Its size as the catalog has it, in pixels, when it could be read.
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub bytes: Option<u64>,
    /// Why it could not be fetched, when it could not.
    pub error: Option<String>,
}

/// One sample of what the server is doing (P3-14), for the Activity
/// page's numbers and small graphs. Rates are per second over the time
/// since the sample before.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityPoint {
    /// When, in Unix milliseconds.
    pub at: i64,
    /// Clients connected over Iroh.
    pub connections_app: u64,
    /// Admin API sockets: the web UI's and the CLI's.
    pub connections_web: u64,
    /// Open playbacks of the file as it is.
    pub playbacks_direct: u64,
    /// Open playbacks whose streams are copied into another container.
    pub playbacks_remux: u64,
    /// Open playbacks that are encoded.
    pub playbacks_transcode: u64,
    /// Encodes whose ffmpeg runs, paused or not.
    pub conversions_running: u64,
    /// Of those, the ones held back for their reader.
    pub conversions_paused: u64,
    /// Seconds of media encoded per second, by every encode.
    pub output_rate: f64,
    pub media_reads_per_sec: f64,
    pub media_bytes_per_sec: f64,
    pub artwork_requests_per_sec: f64,
    /// Of the artwork asked for in the interval, the share the cache had;
    /// `None` when none was asked for.
    #[serde(default)]
    pub artwork_hit_rate: Option<f64>,
    pub subtitle_reads_per_sec: f64,
    pub plex_requests_per_sec: f64,
    pub plex_errors_per_sec: f64,
    /// Frame previews waiting or being made.
    pub previews_queued: u64,
    /// Bitmap subtitle tracks waiting or being read as text.
    pub ocr_queued: u64,
    /// The server's resident memory; `None` where it cannot be read.
    #[serde(default)]
    pub memory_bytes: Option<u64>,
    #[serde(default)]
    pub open_files: Option<u64>,
}

/// Counts since the server started (P3-14).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityTotals {
    pub uptime_secs: u64,
    pub playbacks_started: u64,
    pub playbacks_ended: u64,
    /// `PlaybackExpired` sent to clients.
    pub playbacks_expired: u64,
    /// `PlaybackFailed` sent to clients.
    pub playbacks_failed: u64,
    /// Conversions refused because `transcode.max_jobs` were running.
    pub conversions_refused: u64,
    /// Encodes ended for making no progress.
    pub conversions_stalled: u64,
    pub media_reads: u64,
    pub media_read_bytes: u64,
    pub artwork_requests: u64,
    pub artwork_hits: u64,
    pub subtitle_reads: u64,
    pub sync_runs_ok: u64,
    pub sync_runs_failed: u64,
    pub previews_done: u64,
    pub previews_failed: u64,
    pub ocr_done: u64,
    pub ocr_failed: u64,
}

/// One Plex server's requests (P3-14), counted since its settings last
/// changed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlexServerActivity {
    /// Its id in the config: `plex` for the first.
    pub id: String,
    pub name: String,
    pub requests: u64,
    pub errors: u64,
    /// The mean time a request took; `None` before any.
    #[serde(default)]
    pub mean_latency_ms: Option<f64>,
}

/// One user's live sessions (P3-14).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserSessionCount {
    /// `local:<uuid>`.
    pub user_id: String,
    pub username: String,
    /// Bound to a device's endpoint.
    pub app: u64,
    /// The admin API's: the web UI's and the CLI's.
    pub web: u64,
}

/// What the Activity page shows (P3-14): now, the last hour sampled every
/// `sampleSecs`, oldest first, and the counts behind them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Activity {
    pub sample_secs: u32,
    pub now: ActivityPoint,
    pub history: Vec<ActivityPoint>,
    pub totals: ActivityTotals,
    pub plex: Vec<PlexServerActivity>,
    /// Read from the database every `sampleSecs`.
    pub sessions: Vec<UserSessionCount>,
}

/// How severe a log line is (P3-14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

/// One line of the server's log, as stderr has it (P3-14).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogLine {
    /// Numbered from 1 since the server started, never again: what a
    /// page follows new lines by.
    pub seq: u64,
    /// When, in Unix milliseconds.
    pub at: i64,
    pub level: LogLevel,
    /// Where in the server it comes from: `spoolframe_server::client`.
    pub target: String,
    /// Its spans, its message and its fields.
    pub text: String,
}

/// Which log lines `logs` gives.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogQuery {
    /// Only lines after this one: the `next` of the page before. Left
    /// out, the last lines.
    #[serde(default)]
    pub after: Option<u64>,
    /// Only lines at least this severe: `warn` gives warnings and errors.
    #[serde(default)]
    pub level: Option<LogLevel>,
    /// Only lines whose text or target contains this, in any case.
    #[serde(default)]
    pub text: Option<String>,
    /// At most this many lines; 500 if left out.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Some of the server's last log lines (P3-14), oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogPage {
    pub lines: Vec<LogLine>,
    /// What to ask `after` next, to follow new lines.
    pub next: u64,
    /// Lines after the cursor were dropped before they were asked for:
    /// more came than the buffer holds.
    pub missed: bool,
    /// How many lines the server keeps (`logs.buffer_lines`).
    pub capacity: u32,
}

/// One resource attribute every OTLP export carries (P3-14).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceAttribute {
    pub key: String,
    pub value: String,
}

/// The OTLP export as it is going (P3-14). Header values are never here:
/// they are write-only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OtlpStatus {
    pub endpoint: String,
    /// `grpc` or `http`.
    pub protocol: String,
    pub traces: bool,
    pub metrics: bool,
    pub logs: bool,
    /// The names of the headers sent, never their values.
    pub header_names: Vec<String>,
    /// When an export last went through, Unix ms.
    #[serde(default)]
    pub last_export_at: Option<i64>,
    /// Spans and log records dropped since `droppingSince`: since exports
    /// last stopped going through.
    pub dropped: u64,
    #[serde(default)]
    pub dropping_since: Option<i64>,
    /// The last failure (the collector unreachable, what it answered, or
    /// no answer in time), and when, Unix ms.
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub last_error_at: Option<i64>,
    /// How often metrics are exported; records go at least this often.
    pub metrics_every_secs: u32,
}

/// An address refused `GET /metrics` in the last hour (P3-14).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefusedScrape {
    pub address: String,
    pub count: u64,
    /// Unix ms.
    pub last_at: i64,
}

/// `GET /metrics` as it is (P3-14).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsEndpointStatus {
    pub enabled: bool,
    /// The path on the admin listener: `/metrics`.
    pub path: String,
    pub refused: Vec<RefusedScrape>,
}

/// The Settings › Telemetry tab's state (P3-14).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryStatus {
    /// `None` while no endpoint is set.
    #[serde(default)]
    pub otlp: Option<OtlpStatus>,
    /// What an export carries, shown whether it is on or not.
    pub resource: Vec<ResourceAttribute>,
    pub metrics: MetricsEndpointStatus,
}

/// How a playback is served (P3-15): the file as it is, its streams
/// copied into another container, encoded, or not at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlanKind {
    Direct,
    Remux,
    Transcode,
    Unsupported,
}

/// A plan in the words of the server's "playback started" log line
/// (P3-15): `direct`, `converted`, `unsupported`; `range`, `hls`,
/// `progressive`; `copy:hevc`, `encode:h264:1080p:tonemap`,
/// `copy:eac3`, `ocr:pgs>srt`, `none`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanLine {
    pub kind: String,
    pub delivery: String,
    pub video: String,
    pub audio: String,
    pub subtitle: String,
}

/// Which part of a file a plan row is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StreamKind {
    Container,
    Video,
    Audio,
    Subtitle,
}

/// What a plan does with one part of the file (P3-15). Codec and format
/// names are the protocol's (`hevc`, `eac3`, `web_vtt`, `matroska`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
pub enum StreamOut {
    /// Played as it is in the file.
    Direct,
    /// Copied into the new container as it is.
    #[serde(rename_all = "camelCase")]
    Copy { codec: String },
    /// Decoded and encoded again.
    #[serde(rename_all = "camelCase")]
    Encode {
        codec: String,
        /// Video: the height encoded to.
        height: Option<u32>,
        /// Audio: the channels encoded to.
        channels: Option<u32>,
        /// Bits per second: the cap for video, the rate for audio.
        bitrate: Option<u64>,
        /// HDR mapped to SDR.
        tone_map: bool,
        /// The encoder that runs it (`h264_vaapi`, `libx264`), when the
        /// server has one.
        encoder: Option<String>,
    },
    /// Pictures read as text (OCR), sent beside the stream.
    #[serde(rename_all = "camelCase")]
    ReadAsText { from: String, to: String },
    /// Text converted to another format.
    #[serde(rename_all = "camelCase")]
    Convert { from: String, to: String },
    /// Drawn into the video, which is then encoded.
    #[serde(rename_all = "camelCase")]
    BurnIn { format: String },
    /// A subtitle file the client fetches beside the stream as it is.
    #[serde(rename_all = "camelCase")]
    Beside { format: String },
    /// Into another container.
    #[serde(rename_all = "camelCase")]
    Remux { to: String },
    /// The plan cannot play it.
    Unplayable,
}

/// One row of a plan (P3-15): a stream (or the container), what it is in
/// the file, what the plan does with it, and the planner's reasons that
/// name it, word for word.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanStream {
    pub stream: StreamKind,
    /// The track's number as the plan's reasons and the client's
    /// selection count it (0 for the first audio track); `None` for the
    /// video and the container.
    pub index: Option<u32>,
    /// What it is in the file: `hevc main 10 2160p hdr10`, `dts 7.1 eng`.
    pub source: String,
    pub out: StreamOut,
    pub reasons: Vec<String>,
}

/// A plan as playback debug shows it (P3-15): what `PlaybackPlan`
/// returns, laid out by stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanView {
    pub kind: PlanKind,
    pub line: PlanLine,
    pub streams: Vec<PlanStream>,
    /// The plan's reasons that name no stream.
    pub reasons: Vec<String>,
    /// Exactly the `PlaybackPlan` the client protocol gives, as JSON.
    pub plan: serde_json::Value,
}

/// How far a playback has read in its encode now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "unit", rename_all = "camelCase")]
pub enum ReadMark {
    /// Bytes into the file or the progressive stream.
    #[serde(rename_all = "camelCase")]
    Byte { offset: u64 },
    /// An HLS segment.
    #[serde(rename_all = "camelCase")]
    Segment { n: u32 },
}

/// A playback's conversion now (P3-15).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversionState {
    /// How far into the film the encode has got, ms.
    pub converted_ms: u64,
    pub bytes: u64,
    /// HLS: the last segment finished.
    pub last_segment: Option<u32>,
    /// Held back by the throttle for its reader.
    pub paused: bool,
    /// ffmpeg runs, paused or not.
    pub running: bool,
    /// An admin ended it.
    pub ended: bool,
    /// ffmpeg's failure, when it failed.
    pub failed: Option<String>,
    /// It encodes a stream, rather than only copying them.
    pub encodes: bool,
}

/// One playback as Server › Playback lists it (P3-15).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackRow {
    /// What the admin API names it by; never its handle.
    pub id: String,
    /// `local:<uuid>`.
    pub user_id: String,
    pub username: String,
    pub display_name: String,
    /// The device's own name, else its app's.
    pub device: String,
    pub platform: Platform,
    /// Its `MediaId`.
    pub item: String,
    /// `Alien`; `Severance · S2 E4`.
    pub title: String,
    /// An episode's title; a track's artist.
    pub subtitle: Option<String>,
    pub duration_ms: Option<u64>,
    pub plan: PlanKind,
    pub line: PlanLine,
    /// Why it is converted or cannot play, as the plan says.
    pub reasons: Vec<String>,
    /// The encode's epoch; 0 for direct play.
    pub epoch: u32,
    /// Bumped by each subtitle change and seek.
    pub revision: u64,
    /// Unix ms.
    pub started_at: i64,
    /// Where the client last said it was, ms.
    pub position_ms: Option<u64>,
    /// Where in the film the last read was, ms.
    pub read_to_ms: Option<u64>,
    pub furthest: Option<ReadMark>,
    /// Unix ms.
    pub last_read_at: Option<i64>,
    pub conversion: Option<ConversionState>,
}

/// How a playback ended (P3-15).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum EndedWhy {
    /// Its client stopped it.
    Finished,
    /// An admin stopped it.
    Stopped,
    /// No reads for 5 minutes.
    Idle,
    /// Its device started another conversion.
    Replaced,
    /// Its device was gone too long.
    DeviceGone,
    /// Its session ended or its user was disabled.
    SignedOut,
    /// Its conversion failed.
    #[serde(rename_all = "camelCase")]
    Failed { reason: String },
}

/// A playback that ended in the last hour, as it was at the end.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndedPlayback {
    /// Unix ms.
    pub at: i64,
    pub why: EndedWhy,
    pub playback: PlaybackRow,
}

/// Server › Playback (P3-15): what plays now, newest first, and what
/// ended in the last hour, newest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackList {
    /// The server's clock, Unix ms: what "N s ago" counts from.
    pub now: i64,
    pub open: Vec<PlaybackRow>,
    pub ended: Vec<EndedPlayback>,
}

/// One point of a playback's reads (P3-15).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadPoint {
    /// Unix ms.
    pub at: i64,
    /// Where in the film the read was, ms.
    pub read_ms: Option<u64>,
    /// How far the conversion had got, ms; none for direct play.
    pub converted_ms: Option<u64>,
}

/// One playback's page (P3-15).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackDetail {
    pub now: i64,
    pub playback: PlaybackRow,
    /// Set once it has ended: when and how.
    pub ended: Option<EndedPlayback>,
    pub plan: PlanView,
    /// The ffmpeg command line its conversion runs: paths shown, tokens
    /// (the playback handle, a Plex token) never.
    pub command: Option<String>,
    /// ffmpeg's last stderr lines, oldest first, tokens hidden.
    pub stderr: Vec<String>,
    /// The last `readsWindowSecs` of reads, one point a second at most.
    pub reads: Vec<ReadPoint>,
    pub reads_window_secs: u32,
}

/// Picture subtitles in "Plan for…": as the client's setting says them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PictureSubtitles {
    /// Drawn by the device if it can, else read as text, else burned in.
    Auto,
    /// Always read as text.
    Text,
    /// Always burned in.
    BurnIn,
}

/// What "Plan for…" plans (P3-15).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanFor {
    /// A [`DeviceChoice::id`].
    pub device: String,
    /// The audio track (0 for the first); none: the file's default.
    pub audio: Option<u32>,
    pub subtitle: Option<u32>,
    pub pictures: PictureSubtitles,
}

/// A device "Plan for…" can plan for (P3-15): one connected now, with
/// what its `Hello` said it plays, or a stored profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceChoice {
    /// `device:<endpoint id>` or `profile:<name>`.
    pub id: String,
    pub name: String,
    pub connected: bool,
    /// What the choice stands for, in a line.
    pub note: String,
}

/// One stream of a file as ffprobe sees it (P3-15).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeStream {
    /// The stream's index in the file; none for a file beside it.
    pub index: Option<u32>,
    pub kind: StreamKind,
    /// Its number among its kind, as plans and selections count it.
    pub track: u32,
    /// ffprobe's codec name.
    pub codec: String,
    /// `main 10 · 2160p · 23.976 fps · hdr10`, `5.1 · eng`.
    pub details: String,
    pub language: Option<String>,
    pub title: Option<String>,
    /// Its role (P1-20): `main`, `commentary`, `forced`, `sdh`, ….
    pub role: Option<String>,
    pub default: bool,
    pub forced: bool,
    /// The file beside the media file it is in.
    pub file: Option<String>,
}

/// A media file as the server's ffprobe describes it (P1-06).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Probe {
    pub path: String,
    /// ffprobe's container name.
    pub container: String,
    pub duration_ms: Option<u64>,
    pub bit_rate: Option<u64>,
    pub size: Option<u64>,
    pub streams: Vec<ProbeStream>,
}

/// Where a cached piece of work is (P3-15).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum CacheState {
    /// Nothing made, nothing being made.
    NotMade,
    /// Behind another track.
    Waiting,
    #[serde(rename_all = "camelCase")]
    Making {
        done_percent: u8,
    },
    Ready,
    #[serde(rename_all = "camelCase")]
    Failed {
        reason: String,
    },
    /// The server cannot make it.
    #[serde(rename_all = "camelCase")]
    Unavailable {
        reason: String,
    },
}

/// A subtitle track's text, as the server makes it (P2-07, P3-08).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubtitleCache {
    pub track: u32,
    /// `pgs eng forced`.
    pub label: String,
    /// Read from pictures (OCR), not converted from text.
    pub ocr: bool,
    /// The format made: `srt`, `web_vtt`; none while nothing is.
    pub to: Option<String>,
    pub state: CacheState,
}

/// An item's frame previews (P3-13), as the cache has them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum PreviewCache {
    #[serde(rename_all = "camelCase")]
    Ready {
        count: u32,
        interval_ms: u64,
        sheets: u32,
    },
    #[serde(rename_all = "camelCase")]
    Making { done_percent: u8 },
    /// Not made, nor queued: made when it is first played.
    NotMade,
    /// It will have none.
    #[serde(rename_all = "camelCase")]
    None { reason: String },
}

/// An item's Debug tab (P3-15).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemDebug {
    /// `None` when it could not be probed: `probeError` says why.
    pub probe: Option<Probe>,
    pub probe_error: Option<String>,
    pub devices: Vec<DeviceChoice>,
    pub subtitles: Vec<SubtitleCache>,
    pub previews: PreviewCache,
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

    /// End every session of the caller's but this one, on every device
    /// (P3-03); how many.
    #[method(name = "revokeOtherSessions", with_extensions)]
    async fn revoke_other_sessions(&self) -> jsonrpsee::core::RpcResult<u64>;

    /// What the caller is shown as (P3-03): at most 64 characters, on one
    /// line (`INVALID_INPUT` otherwise); `None` or blank shows the
    /// username. The caller as `whoAmI` answers after it.
    #[method(name = "setDisplayName", with_extensions)]
    async fn set_display_name(
        &self,
        display_name: Option<String>,
    ) -> jsonrpsee::core::RpcResult<UserInfo>;

    /// How the caller wants tracks chosen (P1-20), as their devices read
    /// them.
    #[method(name = "trackPreferences", with_extensions)]
    async fn track_preferences(&self) -> jsonrpsee::core::RpcResult<TrackPreferences>;

    /// Replace them, for every device of the caller's: the next playback
    /// anywhere uses them.
    #[method(name = "setTrackPreferences", with_extensions)]
    async fn set_track_preferences(
        &self,
        preferences: TrackPreferences,
    ) -> jsonrpsee::core::RpcResult<()>;

    /// What the caller picked for each series or movie, most recent first.
    #[method(name = "trackOverrides", with_extensions)]
    async fn track_overrides(&self) -> jsonrpsee::core::RpcResult<Vec<TrackOverrideEntry>>;

    /// Back to the preferences for one series or movie, by its `id` from
    /// `trackOverrides`. One that has none is not an error.
    #[method(name = "clearTrackOverride", with_extensions)]
    async fn clear_track_override(&self, id: String) -> jsonrpsee::core::RpcResult<()>;

    /// Set or change the caller's PIN (P2-08): 4 to 8 digits. Needs a
    /// recent sign-in (P3-03, `RECENT_SIGN_IN_REQUIRED`): a session left
    /// open must not change the PIN that guards the caller's devices.
    #[method(name = "pinSet", with_extensions)]
    async fn pin_set(&self, pin: String) -> jsonrpsee::core::RpcResult<()>;

    /// Remove the caller's PIN, and its requirement on every device. Needs
    /// a recent sign-in, as setting it does.
    #[method(name = "pinClear", with_extensions)]
    async fn pin_clear(&self) -> jsonrpsee::core::RpcResult<()>;

    /// The devices the caller has signed in from.
    #[method(name = "devices", with_extensions)]
    async fn devices(&self) -> jsonrpsee::core::RpcResult<Vec<DeviceEntry>>;

    /// Ask for the caller's PIN on one of their devices, or stop asking.
    /// Asking needs only the session; stopping weakens what guards the
    /// device, so it needs a recent sign-in, as changing the PIN does.
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
    /// code is good once. Requires a recent sign-in or confirmation.
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

    /// The caller's linked sign-in provider accounts (P3-04), oldest
    /// first.
    #[method(name = "ssoLinks", with_extensions)]
    async fn sso_links(&self) -> jsonrpsee::core::RpcResult<Vec<SsoLink>>;

    /// Unlink the caller's account at the provider with `issuer`. Needs a
    /// recent sign-in; their last way to sign in stays
    /// (`LAST_SIGN_IN_METHOD`). `NO_SUCH_LINK` for one they do not have.
    #[method(name = "ssoUnlink", with_extensions)]
    async fn sso_unlink(&self, issuer: String) -> jsonrpsee::core::RpcResult<()>;

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

    /// A user's linked sign-in provider accounts (P3-04). An admin can see
    /// and unlink them (for a lost provider account), never link one: only
    /// its user can.
    #[method(name = "userSsoLinks", with_extensions)]
    async fn user_sso_links(&self, username: String) -> jsonrpsee::core::RpcResult<Vec<SsoLink>>;

    /// Unlink a user's account at the provider with `issuer`, as
    /// `me.ssoUnlink` does; their last way to sign in stays.
    #[method(name = "userSsoUnlink", with_extensions)]
    async fn user_sso_unlink(
        &self,
        username: String,
        issuer: String,
    ) -> jsonrpsee::core::RpcResult<()>;

    /// Keep `secret` as the client secret of the sign-in provider called
    /// `provider` (P3-04), or forget it (`None`): a secret, in the state
    /// directory, never shown. A provider whose config names a
    /// `client_secret_file` reads that instead (`SETTING_LOCKED`).
    #[method(name = "ssoSetClientSecret", with_extensions)]
    async fn sso_set_client_secret(
        &self,
        provider: String,
        secret: Option<String>,
    ) -> jsonrpsee::core::RpcResult<()>;

    /// Plex now: reachable or not, its name, and the libraries with their
    /// item counts. Plex being down is an answer here, not an error.
    #[method(name = "plexStatus", with_extensions)]
    async fn plex_status(&self) -> jsonrpsee::core::RpcResult<PlexStatus>;

    /// Check every media path of every library, or of the one titled
    /// `library`, as `plex check-paths` does. Reads the whole library: it
    /// takes a while. A check of every library is kept, for
    /// `plexLastPathCheck`.
    #[method(name = "plexCheckPaths", with_extensions)]
    async fn plex_check_paths(
        &self,
        library: Option<String>,
    ) -> jsonrpsee::core::RpcResult<PathCheckRun>;

    /// The last `plexCheckPaths` of every library (P3-18), for Server ›
    /// Media paths to show with when it ran. The server keeps it until it
    /// restarts, or its Plex servers, libraries or path mappings change;
    /// `None` before.
    #[method(name = "plexLastPathCheck", with_extensions)]
    async fn plex_last_path_check(&self) -> jsonrpsee::core::RpcResult<Option<PathCheckRun>>;

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
    /// "From your Plex account"), or without a link the account the setup
    /// wizard last signed in with (Sign in with Plex, `plexPinCheck`):
    /// `NOT_CONFIGURED` without either, and `UNAVAILABLE` when plex.tv
    /// cannot be reached.
    #[method(name = "plexAccountServers", with_extensions)]
    async fn plex_account_servers(&self) -> jsonrpsee::core::RpcResult<Vec<PlexAccountServer>>;

    /// Keep, as `plex.token`, the token the caller's Plex account (as for
    /// `plexAccountServers`) uses on the server at `url`, one of its
    /// servers: what the setup wizard does when one of them is chosen.
    /// plex.tv hands out a token per server; the account's own is not
    /// one a server shared with it takes. `NOT_CONFIGURED` without an
    /// account, `INVALID_INPUT` when it does not reach `url`, and
    /// `UNAVAILABLE` when plex.tv cannot be reached.
    #[method(name = "plexAccountTokenUse", with_extensions)]
    async fn plex_account_token_use(&self, url: String) -> jsonrpsee::core::RpcResult<()>;

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

    /// Whether PIN `id` (from `plexPinBegin`) is approved. Once it is, the
    /// server remembers the account, for `plexAccountServers`,
    /// `plexProbe` and `plexServerAdd` to take each of its servers' own
    /// token from (P3-18); with `keepToken`, as the setup wizard's Plex
    /// step asks, it also keeps the account's token as `plex.token`, the
    /// first server's. Adding another Plex server leaves that token as it
    /// is.
    #[method(name = "plexPinCheck", with_extensions)]
    async fn plex_pin_check(
        &self,
        id: u64,
        keep_token: bool,
    ) -> jsonrpsee::core::RpcResult<PlexPinState>;

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
    /// section that is a library already, that Spoolframe cannot serve,
    /// or none of whose sample files is on this machine under the saved
    /// mappings ([`LibraryPathCheck::refusal`]'s words).
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
    /// Library `id` as an admin browses it (P3-16): its counts, and its
    /// path check, which starts if the library has none yet.
    /// `UNAVAILABLE` when its Plex server does not answer: the server
    /// keeps no copy of the catalog to show instead.
    #[method(name = "catalogLibrary", with_extensions)]
    async fn catalog_library(&self, id: String) -> jsonrpsee::core::RpcResult<CatalogLibrary>;

    /// One page of a library's rows. Only the rows on the page have their
    /// files looked for; `Missing` is the path check's list, and is empty
    /// until a check is done.
    #[method(name = "catalogRows", with_extensions)]
    async fn catalog_rows(&self, query: CatalogQuery) -> jsonrpsee::core::RpcResult<CatalogPage>;

    /// The rows of an item's children in library `library`: a show's
    /// seasons, a season's episodes, an album's tracks.
    #[method(name = "catalogChildren", with_extensions)]
    async fn catalog_children(
        &self,
        library: String,
        id: String,
    ) -> jsonrpsee::core::RpcResult<Vec<CatalogRow>>;

    /// One item: what clients get, its match, its files looked for now,
    /// its children.
    #[method(name = "catalogItem", with_extensions)]
    async fn catalog_item(&self, id: String) -> jsonrpsee::core::RpcResult<CatalogItem>;

    /// An item's artwork, each with its size as the catalog has it.
    #[method(name = "catalogArtwork", with_extensions)]
    async fn catalog_artwork(&self, id: String) -> jsonrpsee::core::RpcResult<Vec<ArtworkInfo>>;

    /// Check every media file of library `id` again, as `plexCheckPaths`
    /// does; it runs on, and `catalogLibrary` says when it is done.
    #[method(name = "catalogCheckPaths", with_extensions)]
    async fn catalog_check_paths(&self, id: String) -> jsonrpsee::core::RpcResult<PathsChecked>;

    /// What the server is doing now, and over the last hour (P3-14): the
    /// Activity page. Kept in memory and sampled every `sampleSecs`, so a
    /// page that asks every few seconds sees a new point as it is made.
    #[method(name = "activity", with_extensions)]
    async fn activity(&self) -> jsonrpsee::core::RpcResult<Activity>;

    /// The server's last log lines, as its journal has them (P3-14): the
    /// Logs page. Kept in memory, `logs.buffer_lines` of them, at the
    /// level the server logs at. A page follows new lines by asking again
    /// `after` the `next` it was given; the admin API has no
    /// subscriptions.
    #[method(name = "logs", with_extensions)]
    async fn logs(&self, query: LogQuery) -> jsonrpsee::core::RpcResult<LogPage>;

    /// How the OTLP export and `GET /metrics` are doing (P3-14): the
    /// Settings › Telemetry tab's status. The settings themselves are in
    /// `settings`; header values are never returned anywhere.
    #[method(name = "telemetryStatus", with_extensions)]
    async fn telemetry_status(&self) -> jsonrpsee::core::RpcResult<TelemetryStatus>;

    /// Every playback open now, and those that ended in the last hour
    /// (P3-15): Server › Playback. Nothing here changes a playback.
    #[method(name = "playbacks", with_extensions)]
    async fn playbacks(&self) -> jsonrpsee::core::RpcResult<PlaybackList>;

    /// One playback, open or ended in the last hour (P3-15): its plan in
    /// full, its conversion's command line and last stderr lines, its
    /// reads over the last 10 minutes. `NO_SUCH_PLAYBACK` for an id it
    /// does not know.
    #[method(name = "playback", with_extensions)]
    async fn playback(&self, id: String) -> jsonrpsee::core::RpcResult<PlaybackDetail>;

    /// Stop playback `id` (P3-15): it ends now, its conversion with it,
    /// and its client is told it was stopped, so it does not start it
    /// again. Its progress is kept. `NO_SUCH_PLAYBACK` unless it is open.
    #[method(name = "playbackStop", with_extensions)]
    async fn playback_stop(&self, id: String) -> jsonrpsee::core::RpcResult<()>;

    /// End playback `id`'s conversion (P3-15): ffmpeg stops and its slot
    /// is free; the playback goes on with what was written, then ends.
    /// `NO_CONVERSION` for direct play.
    #[method(name = "playbackEndConversion", with_extensions)]
    async fn playback_end_conversion(&self, id: String) -> jsonrpsee::core::RpcResult<()>;

    /// An item's Debug tab (P3-15): its file as ffprobe sees it, the
    /// devices "Plan for…" offers, and its subtitle text and previews as
    /// the caches have them. Looking starts nothing.
    #[method(name = "catalogDebug", with_extensions)]
    async fn catalog_debug(&self, id: String) -> jsonrpsee::core::RpcResult<ItemDebug>;

    /// The plan the server would make for item `id` on `request`'s
    /// device, exactly as `PlaybackPlan` would answer it (P3-15).
    /// Nothing plays.
    #[method(name = "catalogPlanFor", with_extensions)]
    async fn catalog_plan_for(
        &self,
        id: String,
        request: PlanFor,
    ) -> jsonrpsee::core::RpcResult<PlanView>;
}
