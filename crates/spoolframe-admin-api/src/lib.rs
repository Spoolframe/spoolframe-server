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
    APP_RP_DOMAIN, Album, Artist, Artwork, ArtworkKind, AudioRole, BitmapPreference, Episode, Item,
    LibraryKind, Marker, MarkerKind, Movie, Platform, PlaybackState, QuotaOf, Role, SdhPreference,
    Season, Show, SubtitleDownload, SubtitleDownloaded, SubtitleMode, SubtitleOrigin,
    SubtitleResult, SubtitleRole, SubtitleSearch, SubtitleTrack, Track, TrackPreferences,
};

/// JSON-RPC error codes the admin API returns, beside the standard ones.
/// What makes an address a public one (`network.wan`, P6-02): one rule,
/// so the web UI refuses a typed address with the words the server's
/// config check uses.
pub mod wan {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    /// Why `ip` cannot be reached from the Internet, if it cannot: the
    /// range it is in, named (`"a private address (192.168.0.0/16)"`).
    /// Documentation ranges (192.0.2.0/24, 2001:db8::/32 and the like)
    /// are let through: they are what examples are written with.
    pub fn not_public(ip: IpAddr) -> Option<&'static str> {
        match ip.to_canonical() {
            IpAddr::V4(v4) => not_public_v4(v4),
            IpAddr::V6(v6) => not_public_v6(v6),
        }
    }

    fn not_public_v4(ip: Ipv4Addr) -> Option<&'static str> {
        let [a, b, ..] = ip.octets();
        Some(match (a, b) {
            (0, _) => "an unspecified address (0.0.0.0/8)",
            (127, _) => "a loopback address (127.0.0.0/8)",
            (10, _) => "a private address (10.0.0.0/8)",
            (172, 16..=31) => "a private address (172.16.0.0/12)",
            (192, 168) => "a private address (192.168.0.0/16)",
            (100, 64..=127) => "a carrier-grade NAT address (100.64.0.0/10)",
            (169, 254) => "a link-local address (169.254.0.0/16)",
            (224..=239, _) => "a multicast address (224.0.0.0/4)",
            (240.., _) => "a reserved address (240.0.0.0/4)",
            _ => return None,
        })
    }

    fn not_public_v6(ip: Ipv6Addr) -> Option<&'static str> {
        let first = ip.segments()[0];
        if ip.is_unspecified() {
            Some("an unspecified address (::)")
        } else if ip.is_loopback() {
            Some("a loopback address (::1)")
        } else if first & 0xffc0 == 0xfe80 {
            Some("a link-local address (fe80::/10)")
        } else if first & 0xffc0 == 0xfec0 {
            Some("a site-local address (fec0::/10)")
        } else if first & 0xfe00 == 0xfc00 {
            Some("a unique local address (fc00::/7)")
        } else if first & 0xff00 == 0xff00 {
            Some("a multicast address (ff00::/8)")
        } else {
            None
        }
    }

    /// A `static` entry: a public address with its port,
    /// `"203.0.113.7:7420"` or `"[2001:db8::1]:7420"`.
    pub fn check_static(text: &str) -> Result<SocketAddr, String> {
        let text = text.trim();
        let addr: SocketAddr = text
            .parse()
            .map_err(|_| format!("{text:?} is not an address and a port"))?;
        if addr.port() == 0 {
            return Err(format!("{text:?}: the port is 0"));
        }
        let ip = addr.ip().to_canonical();
        if let Some(why) = not_public(ip) {
            return Err(format!(
                "{text:?} is {why}, which no device on the Internet reaches"
            ));
        }
        Ok(SocketAddr::new(ip, addr.port()))
    }

    /// A typed public IP (`mapped_port.public_ip`).
    pub fn check_public_ip(text: &str) -> Result<IpAddr, String> {
        let text = text.trim();
        let ip: IpAddr = text
            .parse()
            .map_err(|_| format!("{text:?} is not an IP address"))?;
        let ip = ip.to_canonical();
        match not_public(ip) {
            Some(why) => Err(format!(
                "{text:?} is {why}, which no device on the Internet reaches"
            )),
            None => Ok(ip),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_refusal_names_the_range() {
            assert_eq!(
                check_static("100.64.1.2:7420").unwrap_err(),
                "\"100.64.1.2:7420\" is a carrier-grade NAT address (100.64.0.0/10), which no \
                 device on the Internet reaches"
            );
            assert_eq!(
                check_public_ip("10.0.0.5").unwrap_err(),
                "\"10.0.0.5\" is a private address (10.0.0.0/8), which no device on the \
                 Internet reaches"
            );
            assert_eq!(
                check_static("[2001:db8::1]:7420"),
                Ok("[2001:db8::1]:7420".parse().unwrap())
            );
        }
    }
}

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
    /// Not one of the user's linked sign-in provider accounts (P3-04); or,
    /// to sever, not a portable identity the account is tied to (P7-04,
    /// P7-09).
    pub const NO_SUCH_LINK: i32 = -32021;
    /// The provider account is linked to another user, or the user has
    /// one linked at that provider already (P3-04).
    pub const LINK_REFUSED: i32 = -32022;
    /// Not a playback open now or ended in the last hour (P3-15).
    pub const NO_SUCH_PLAYBACK: i32 = -32023;
    /// End conversion on a playback that plays the file directly (P3-15).
    pub const NO_CONVERSION: i32 = -32024;
    /// Not an invite that can still make an account (P6-01): unknown,
    /// revoked, expired or used up.
    pub const NO_SUCH_INVITE: i32 = -32025;
    /// Not a portable user with a grant here (P7-06).
    pub const NO_SUCH_GRANT: i32 = -32026;
    /// The session was signed in with a one-time password an admin reset
    /// to: it may only `me.setPassword` (with the one-time password as the
    /// current one), `logout` or `whoAmI` until then.
    pub const PASSWORD_CHANGE_REQUIRED: i32 = -32027;
    /// Not an app connected to the caller's account (PX-42), or for an
    /// admin, to any account.
    pub const NO_SUCH_APP: i32 = -32028;
    /// Not a playlist the caller sees (PX-40).
    pub const NO_SUCH_PLAYLIST: i32 = -32029;
    /// A playlist the caller sees but may not make this change to: an
    /// entry edit on a read-only share, or a rename by an editor.
    pub const PLAYLIST_READ_ONLY: i32 = -32030;
    /// An item of a kind the playlist does not take.
    pub const KIND_NOT_ALLOWED: i32 = -32031;
    /// Someone changed the playlist in a way the edit cannot be redone
    /// over; `data` is `{"revision": n}`, the playlist's now.
    pub const PLAYLIST_CONFLICT: i32 = -32032;
    /// Not the caller's to do to what they see (PX-53): a playlist's
    /// cover set or taken away by someone who may not edit it, or on
    /// Liked or All.
    pub const NOT_ALLOWED: i32 = -32033;
    /// A picture over 20 MB, or over 40 megapixels by its header (PX-53):
    /// not decoded.
    pub const IMAGE_TOO_LARGE: i32 = -32034;
    /// Not a JPEG, PNG, WebP or HEIC picture the server can decode
    /// (PX-53).
    pub const IMAGE_UNREADABLE: i32 = -32035;
    /// No subtitle downloads left today (PX-02): `data` has `whose`
    /// (`account`, the server's OpenSubtitles account, or `user`, the
    /// caller's own daily limit) and `resetsAt`, Unix seconds, when known.
    pub const QUOTA_EXHAUSTED: i32 = -32036;
}

/// Invites (P6-01): what an admin makes and sees of them.
pub mod invite {
    /// Accounts an invite makes when the admin does not say.
    pub const DEFAULT_USES: u32 = 1;
    /// The most accounts one invite may make.
    pub const MAX_USES: u32 = 100;
    /// How long an invite lasts when the admin does not say: 7 days.
    pub const DEFAULT_EXPIRES_SECS: u64 = 7 * DAY;
    /// The shortest it may last: an hour.
    pub const MIN_EXPIRES_SECS: u64 = 3600;
    /// The longest: 30 days.
    pub const MAX_EXPIRES_SECS: u64 = 30 * DAY;
    /// The most characters of an invite's note.
    pub const MAX_NOTE_CHARS: usize = 64;
    /// What `createdBy` says of an invite made with
    /// `spoolframe-server invite create`, or any caller that is not a
    /// browser's page.
    pub const MADE_BY_CLI: &str = "server CLI";

    const DAY: u64 = 24 * 3600;
}

/// MCP's sign-in (PX-42): the apps allowed to sign in.
pub mod mcp {
    use serde::{Deserialize, Serialize};

    /// The setting that lists them, each an [`AllowedRedirect`].
    pub const ALLOWED_REDIRECTS: &str = "mcp.allowed_redirects";

    /// The apps a server lets sign in until an admin changes the list, as
    /// (name, pattern). Claude Code listens on `localhost` at a port it
    /// picks, at `/callback`. Codex's `codex mcp login` listens on
    /// `127.0.0.1` at a port it picks and, as this server does not send
    /// `iss` in its authorization response, returns to `/callback/<id>`,
    /// an id per MCP server (openai/codex, codex-rs/rmcp-client:
    /// `local_redirect_uri`, `append_callback_id_to_redirect_uri`).
    pub const DEFAULT_ALLOWED_REDIRECTS: [(&str, &str); 3] = [
        ("Claude Code", "http://localhost:*/callback"),
        (
            "Claude (claude.ai and desktop)",
            "https://claude.ai/api/mcp/auth_callback",
        ),
        ("Codex", "http://127.0.0.1:*/callback/*"),
    ];

    /// One app allowed to sign in: a name for the admin, and the redirect
    /// URIs it returns to. In the pattern, `*` as the port is any port,
    /// and a path ending in `*` is any path that starts with the rest.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct AllowedRedirect {
        pub name: String,
        pub pattern: String,
    }

    /// [`DEFAULT_ALLOWED_REDIRECTS`] as entries.
    pub fn default_allowed_redirects() -> Vec<AllowedRedirect> {
        DEFAULT_ALLOWED_REDIRECTS
            .iter()
            .map(|(name, pattern)| AllowedRedirect {
                name: (*name).to_owned(),
                pattern: (*pattern).to_owned(),
            })
            .collect()
    }
}

/// What redeeming an invite does (P6-01).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InviteKind {
    /// Makes an account on this server.
    CreateUser,
    /// Grants a portable identity access here (P7-05): its holder watches
    /// as themselves, signed in from their own phone, and no account is
    /// made. Its role is always the consumer's.
    PortableAccount,
    /// Pairs another server with this one: refused until P8.
    Federation,
}

/// An invite to make (`inviteCreate`). Each `None` is the default:
/// [`invite::DEFAULT_USES`] uses, [`invite::DEFAULT_EXPIRES_SECS`], and
/// the consumer role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewInvite {
    pub kind: InviteKind,
    /// Who it is for, for the admins' list; at most
    /// [`invite::MAX_NOTE_CHARS`] characters.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub note: Option<String>,
    /// How many accounts it may make: 1 to [`invite::MAX_USES`].
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub uses: Option<u32>,
    /// How long it lasts, in seconds from now: an hour to 30 days.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub expires: Option<u64>,
    /// The role its accounts get.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub role: Option<Role>,
}

/// An invite as admins see it: never its code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InviteEntry {
    /// What `inviteRevoke` takes.
    pub id: String,
    pub kind: InviteKind,
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub note: Option<String>,
    pub role: Role,
    /// How many accounts it could make, and how many it still can.
    pub uses: u32,
    pub uses_left: u32,
    /// Unix seconds.
    pub expires_at: i64,
    /// The admin who made it, or [`invite::MADE_BY_CLI`].
    pub created_by: String,
    /// Unix seconds.
    pub created_at: i64,
}

/// A new invite, with its code: shown this once, since the server keeps
/// only its hash.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InviteCreated {
    pub invite: InviteEntry,
    /// As shown: `QW7K-2MFP-XR4T`.
    pub code: String,
    /// The link that carries it, the server's key and where to find it:
    /// the `spoolframe://` form, what the QR code shows.
    pub link: String,
    /// The same as an https link on spoolframe.app (PX-45), the invite in
    /// its fragment: what is copied and sent.
    pub share_link: String,
}

/// The server's link without a code (`inviteLink`), in both forms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerLink {
    /// `spoolframe://invite?…`: what the QR code shows.
    pub link: String,
    /// `https://spoolframe.app/i#…` (PX-45): what is copied and sent.
    pub share_link: String,
}

impl fmt::Debug for InviteCreated {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InviteCreated")
            .field("invite", &self.invite)
            .field("code", &"<redacted>")
            .field("link", &"<redacted>")
            .field("share_link", &"<redacted>")
            .finish()
    }
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
    /// Signed in with a one-time password an admin reset to: show "Choose
    /// a new password" (`me.setPassword`, the one-time password as the
    /// current one). Every other call is `PASSWORD_CHANGE_REQUIRED` until
    /// then.
    pub password_change_required: bool,
}

impl fmt::Debug for LoggedIn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoggedIn")
            .field("token", &"<redacted>")
            .field("username", &self.username)
            .field("role", &self.role)
            .field("password_change_required", &self.password_change_required)
            .finish()
    }
}

/// A one-time password an admin reset a user's to (`userResetPassword`):
/// shown this once, since the server keeps only its hash.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OneTimePassword {
    /// As the user types it: `QW7K-2MFP-XR4T-9HCD`.
    pub password: String,
    /// When it stops working, Unix seconds: then only another reset helps.
    pub expires_at: i64,
    /// The user's sessions it ended: those signed in with a password, or
    /// from before sessions kept how they were signed in.
    pub revoked_sessions: u64,
    /// Devices no longer trusted for them, on the same rule.
    pub untrusted_devices: u64,
}

impl fmt::Debug for OneTimePassword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OneTimePassword")
            .field("password", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field("revoked_sessions", &self.revoked_sessions)
            .field("untrusted_devices", &self.untrusted_devices)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInfo {
    pub username: String,
    /// What clients show; `None` means the username.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub display_name: Option<String>,
    pub role: Role,
    /// Unix seconds.
    pub created_at: i64,
    pub disabled: bool,
    /// When they left the server from an app (PX-47), Unix seconds; they
    /// are `disabled` too, and `userEnable` restores them. `None` for an
    /// account that did not leave.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub left_at: Option<i64>,
    pub has_password: bool,
    /// When the password was set or last changed, Unix seconds; `None`
    /// without one.
    pub password_changed_at: Option<i64>,
    /// The password is a one-time one an admin reset to, and the user has
    /// not chosen a new one yet; `password_expires_at` says until when it
    /// works.
    pub password_must_change: bool,
    /// When that one-time password stops working, Unix seconds.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub password_expires_at: Option<i64>,
    /// When any of their sessions was last used, Unix seconds; `None` if
    /// they have never signed in.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
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
    /// The portable identities their account is tied to (P7-04, P7-09),
    /// the oldest tie first, shown on the account, never as other users;
    /// empty when it is tied to none.
    pub portable: Vec<PortableTie>,
}

/// One of a local account's ties to a portable identity (P7-04, P7-09).
/// Only the identity's app makes one; the web UI shows and severs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PortableTie {
    /// The `PortableUserId`: 64 lowercase hex digits.
    pub id: String,
    /// The short code to compare with the phone's, as a server's
    /// fingerprint is written: `K7QX-3MPA-9TRD-WF2H`.
    pub fingerprint: String,
    /// Unix seconds.
    pub tied_at: i64,
    /// Unix seconds of the identity's last sign-in into the account with
    /// its credential (P7-06); `None` until one.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub last_used_at: Option<i64>,
}

/// A portable user granted access here (P7-06): someone whose account
/// this server does not host, who watches as themselves. Not a user: the
/// users list never shows them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PortableGrant {
    /// The `PortableUserId`: 64 lowercase hex digits. What `grantRevoke`
    /// takes.
    pub id: String,
    /// The short code to compare with their phone's.
    pub fingerprint: String,
    /// What they asked to be called; `None`: show the fingerprint.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub display_name: Option<String>,
    /// Unix seconds.
    pub granted_at: i64,
    /// The invite that made it (P7-05), as `inviteList` names invites.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub invite_id: Option<String>,
    /// Their sessions live now.
    pub live_sessions: u64,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewUser {
    pub username: String,
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub display_name: Option<String>,
    pub password: String,
    /// `Consumer` if left out.
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
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub device_name: Option<String>,
    /// Unix seconds.
    pub created_at: i64,
    pub last_used_at: i64,
    /// The session this call is made in.
    pub current: bool,
    /// The device's key (hex `EndpointId`) for a client session; `None` for
    /// a web or CLI session.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub endpoint_id: Option<String>,
    /// What kind of client the device runs, for a client session: for an
    /// admin API session `Web` or `Cli`, by what signed it in (P3-18).
    /// `None` for a device not seen since before the server kept it, or
    /// an admin API session from before it kept which.
    pub platform: Option<Platform>,
}

/// An app connected to a user's account through MCP's OAuth sign-in
/// (PX-42): what My profile › Connected apps lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectedApp {
    /// For `revokeConnectedApp`.
    pub id: String,
    /// The name the app registered: its own word for itself.
    pub client_name: String,
    /// The host its sign-in returned to (`claude.ai`).
    pub return_host: String,
    /// What the user allowed it: `library:read`, `playlists:read`,
    /// `playlists:write`, `admin`.
    pub scopes: Vec<String>,
    /// When the user connected it, Unix seconds.
    pub connected_at: i64,
    /// Its last call, Unix seconds; `None` before its first.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub last_call_at: Option<i64>,
    /// Its calls since midnight UTC.
    pub calls_today: u32,
    /// Whose account it acts for.
    pub username: String,
}

/// One entry of the audit list (PX-42): what was done, by whom, through
/// what.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditEvent {
    pub id: i64,
    /// Unix seconds.
    pub at: i64,
    /// Who: their username, or their id when they are no user here now.
    pub actor: String,
    /// Through what: an MCP client's name, or `web`.
    pub client: String,
    /// What: an MCP tool's name, `app.connect` or `app.revoke`.
    pub action: String,
    /// To what: a username, an app's name, a session's or playback's id.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub target: Option<String>,
    /// `ok`, or why not.
    pub outcome: String,
}

/// A page of the audit list, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditPage {
    pub events: Vec<AuditEvent>,
    /// How many there are in all.
    pub total: u32,
}

/// One of a user's devices (P2-08).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceEntry {
    /// Hex `EndpointId`.
    pub endpoint_id: String,
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub name: Option<String>,
    /// What kind of client it runs; `None` for a device not seen since
    /// before the server kept it.
    pub platform: Option<Platform>,
    /// Unix seconds.
    pub last_seen_at: i64,
    /// The user's PIN is asked for on it.
    pub pin_required: bool,
    /// It may open the user's profile without a password (P4-01).
    pub trusted: bool,
    /// It opens the user's profile by itself at start (P4-01). Only with
    /// `trusted`; at most one user per device.
    pub auto_login: bool,
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
    /// Whether it signs in: a web UI's made for the host the server is at
    /// now (one made before `admin.public_url` changed signs no one in), or
    /// an app's made for this server's relying party,
    /// `<server id>.spoolframe.app` (PX-14). An app's made for the one
    /// every server once shared, `spoolframe.app`, does not: it is to be
    /// added again in the app.
    pub usable: bool,
    /// Made in a Spoolframe app (P3-19), and of use in the apps only: shown
    /// as "Spoolframe app · <name>", where its name is the device's.
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
    /// The device asked to be remembered (P4-01): the approval says "This
    /// device will be remembered", ticked, and the approver may untick it
    /// (`deviceSignInApprove`'s `remember`).
    pub remember: bool,
    /// This server's key fingerprint, as the device shows it beside the
    /// code: the approval page shows it too, to be matched before saying
    /// yes. Always known: a code is pending only once a device reached the
    /// server's endpoint.
    pub server_fingerprint: String,
    /// Where the device's request came from, as the server saw it: its IP
    /// address (PX-09 finding 27). `None` if the server could not tell.
    pub address: Option<String>,
    /// That address is a public one, not a home, office or VPN network's:
    /// a device far from the approver, which is what a sign-in link sent
    /// to someone else (device-code phishing) looks like.
    pub from_internet: bool,
    /// That address is a global unicast IPv6 one. With `from_internet`
    /// false the device is still on the server's own network (in its
    /// IPv6 prefix), and the page says "on your network (public IPv6)".
    pub public_ipv6: bool,
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
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub endpoint_id: Option<String>,
    /// That key's fingerprint (P1-21), for checking a client's pin.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub fingerprint: Option<String>,
    /// The server id (PX-14): made once, kept when the key is replaced.
    /// The apps' passkeys for this server are made for
    /// `<server id>.`[`APP_RP_DOMAIN`], which the Server page shows as
    /// the passkey domain. Known from the start, unlike the key.
    pub server_id: String,
    pub transcoding: TranscodingStatus,
    /// Whether the server announces itself on the LAN (mDNS).
    pub mdns: bool,
    /// Where clients reach it: the endpoint's bound addresses.
    pub addresses: Vec<String>,
    /// The addresses a signed-in device is told to dial (PX-12): the
    /// endpoint's, as `network.announce` filters them, read now. Empty
    /// while the endpoint is starting.
    pub told_addresses: Vec<String>,
    /// Whether `network.wan` is set (P6-02): the server advertises public
    /// addresses, and says here which it found.
    pub wan: bool,
    /// The public addresses advertised now (P6-02), each with where it
    /// came from; they are among `told_addresses` too. Empty without
    /// `network.wan`, and while none is found.
    pub public_addresses: Vec<PublicAddress>,
    /// What went wrong finding them, one line each: a router that refused
    /// the port mapping, an IP lookup that failed.
    pub public_notes: Vec<String>,
    /// The router's answer about the mapped port.
    pub wan_mapping: MappingStatus,
    /// The public IP lookup, for the mapped port.
    pub wan_lookup: LookupStatus,
    /// The tesseract languages bitmap subtitles are read as text in
    /// (P2-07), as tesseract names them (`eng`); none when OCR is off.
    pub ocr_languages: Vec<String>,
    /// Where the web UI answers (P3-10): `http://<address>:<port>/`.
    pub web_urls: Vec<String>,
    /// How many files frame previews are made of ahead at once when
    /// `previews.workers` is not set (P3-13).
    pub preview_workers_default: DefaultWorkers,
    /// The client protocol's version (`spoolframe_protocol::
    /// PROTOCOL_VERSION`). New capability comes as a feature, not a new
    /// version, so the lists below say what this server speaks.
    pub protocol_version: u32,
    /// Its regular features (`spoolframe:playlists`, …): always on in
    /// this build, used with each client that lists them too.
    pub features: Vec<String>,
    /// Those a client sees before signing in. Empty today.
    pub pre_auth_features: Vec<String>,
    /// The experimental features turned on here, each by its setting
    /// (`http-playback` by `http_playback.listen`).
    pub experimental_features: Vec<String>,
}

/// A public address the server advertises (P6-02).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicAddress {
    /// `203.0.113.7:7420`, `[2001:db8::1]:7420`.
    pub address: String,
    pub source: AddressSource,
}

/// Where a public address came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AddressSource {
    /// `network.wan.static`, as written.
    Static,
    /// The mapped port with its public IP: the router's answer, or
    /// `public_ip` as written.
    MappedPort,
    /// The mapped port with its public IP looked up.
    Lookup,
}

impl AddressSource {
    /// As the status page says it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::MappedPort => "mapped port",
            Self::Lookup => "lookup",
        }
    }
}

/// The router's answer about the mapped port (`mode = "auto"`, P6-02).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "state",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum MappingStatus {
    /// No router is asked: no `network.wan`, no mapped port, or one the
    /// operator forwarded (`mode = "manual"`).
    NotUsed,
    /// Asking, or asking again after the mapping lapsed.
    Asking,
    /// Mapped: the server's port, and the address the router gave it.
    Granted { local_port: u16, external: String },
    /// No router answers, or it said no.
    Refused { reason: String },
}

/// The public IP lookup (`public_ip = "lookup"`, P6-02).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "state",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum LookupStatus {
    /// Nothing is looked up: no mapped port, or its IP is written down.
    NotUsed,
    /// The router reported a public address itself: no need to ask.
    NotNeeded,
    /// Asking now.
    Asking,
    /// The service's answer, and when it came (Unix seconds).
    Found { ip: String, via: String, at: u64 },
    /// It failed; `retrying` while tries are left.
    Failed { error: String, retrying: bool },
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
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub error: Option<String>,
    /// What the PMS calls itself.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub server_name: Option<String>,
    /// The PMS's version.
    pub version: Option<String>,
    /// Where the server reaches it: `plex.url`, without the token.
    pub url: Option<String>,
    /// Every library section Spoolframe can serve, served or not.
    pub libraries: Vec<LibraryStatus>,
    /// The titles of its sections Spoolframe does not serve (photos,
    /// other videos), with what they hold: `Photos · photo`.
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
    /// Codecs with a working hardware decoder, as `hevc (vaapi)` or
    /// `hevc 10-bit (nvdec)`: the API it decodes through, and the test
    /// sample's depth where it was over 8 bits.
    pub hardware_decoders: Vec<String>,
    /// `transcode.gpu_filters`: tone map and burn in pictures on the GPU
    /// where it can.
    pub gpu_filters: bool,
    /// The GPU tone maps HDR (`tonemap_vaapi`).
    pub gpu_tonemap: bool,
    /// The GPU lays pictures over video (`overlay_vaapi`).
    pub gpu_overlay: bool,
    /// The tone maps each GPU runs (PGPU-04), in `transcode.tone_map =
    /// "auto"`'s order per GPU.
    pub gpu_tone_maps: Vec<GpuToneMap>,
    /// The encode budget's use now; `None` where the server runs no
    /// conversions.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub budget: Option<EncodeBudget>,
    /// The GPUs encodes run on (PGPU-05), in the order jobs try them:
    /// what the server's start lines say of each, for the Server page.
    pub gpus: Vec<GpuCard>,
    /// The host's other GPUs, and why none of their work goes there.
    pub other_gpus: Vec<OtherGpu>,
    /// Entries of `transcode.gpus` that took no card, as the log says
    /// them: `transcode.gpus entry 2 (nvenc device=1) names a GPU that is
    /// not here`.
    pub gpu_notes: Vec<String>,
    /// Where typical conversions would run now, as the jobs pick
    /// (PGPU-02, PGPU-05): the first choice, then each next while the
    /// ones before are full.
    pub routes: Vec<GpuRoute>,
    /// The codec the server would convert a 4K HDR10 film to now, for
    /// each kind of device (PGPU-06): `transcode.video_codecs` and the
    /// cards here, as the planner takes them.
    pub picks: Vec<CodecPick>,
}

/// The codec a 4K HDR10 film is converted to for `device` (PGPU-06), and
/// whether its HDR is kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodecPick {
    pub device: PickDevice,
    /// The protocol's codec name: `h264`, `hevc`, `av1`; `None` where
    /// nothing here encodes a codec it plays.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub codec: Option<String>,
    pub hdr_kept: bool,
}

/// A kind of device [`CodecPick`] is asked for: each plays H.264, HEVC
/// and AV1, decoding as said in hardware.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PickDevice {
    /// Shows HDR10, decodes AV1 and HEVC in hardware (iPhone 15 Pro, an
    /// M3 Mac).
    HdrAv1,
    /// Shows HDR10, decodes HEVC in hardware, AV1 not (Apple TV 4K).
    HdrHevc,
    /// Shows no HDR, decodes all three in hardware.
    Sdr,
}

impl PickDevice {
    /// The device as the Server page names it.
    pub fn label(self) -> &'static str {
        match self {
            PickDevice::HdrAv1 => "An HDR screen that decodes AV1",
            PickDevice::HdrHevc => "An HDR screen without AV1 (Apple TV 4K)",
            PickDevice::Sdr => "An SDR screen",
        }
    }
}

/// A GPU the server encodes on (PGPU-05).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuCard {
    /// `transcode`'s key for it, `vaapi:0000:03:00.0`: what a
    /// [`GpuRoute`] names.
    pub key: String,
    /// The driver's name for it: `AMD Radeon RX 7900 XTX`.
    pub name: String,
    pub api: GpuApi,
    /// Its render node, or for NVIDIA its CUDA index, with the PCI
    /// address: `/dev/dri/by-path/pci-0000:03:00.0-render`,
    /// `cuda:0 (0000:01:00.0)`.
    pub place: String,
    pub kind: GpuKind,
    /// How `kind` was known: `amdgpu fusion flag: not an APU`.
    pub classified: String,
    /// The entry that took it: `transcode.gpus entry 2 (vaapi pick=all)`.
    pub entry: String,
    /// GPU video encodes on it now, and at most.
    pub jobs: u32,
    pub max_jobs: u32,
    /// The limit is its entry's `max_jobs`, not `transcode.max_gpu_jobs`.
    pub own_limit: bool,
    /// ffmpeg's encoders on it: `hevc_vaapi`.
    pub encodes: Vec<String>,
    /// What it decodes: `hevc 10-bit (vaapi)`.
    pub decodes: Vec<String>,
    /// Its filters: `scale`, `overlay`, `tone map`.
    pub filters: Vec<String>,
    /// How it tone maps HDR, best first; empty when only the CPU does.
    pub tone_maps: Vec<ToneMapper>,
    /// What its failed tests say it lacks: `av1 encode`.
    pub lacks: Vec<String>,
}

/// Discrete or integrated (PGPU-05).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum GpuKind {
    Discrete,
    Integrated,
}

impl GpuKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Discrete => "discrete",
            Self::Integrated => "integrated",
        }
    }
}

/// A GPU of the host's that encodes nothing here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OtherGpu {
    pub name: String,
    /// `0000:6d:00.0`.
    pub pci: String,
    /// The kernel driver: `amdgpu`, `nvidia`.
    pub driver: String,
    pub kind: GpuKind,
    pub why: OtherGpuWhy,
}

/// Why a GPU does none of the work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum OtherGpuWhy {
    /// No entry of `transcode.gpus` takes it.
    NoEntry,
    /// An entry takes it, and none of its tests passed.
    #[serde(rename_all = "camelCase")]
    NothingWorks {
        api: GpuApi,
        entry: String,
        /// The first failed test: `h264_vaapi`.
        #[serde(deserialize_with = "spoolframe_types::required::option")]
        test: Option<String>,
    },
}

/// A typical conversion (PGPU-02), for the Server page's routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RouteJob {
    /// A 1080p 8-bit source of `source`'s codec, scaled to 720p.
    Convert,
    /// A 4K HDR10 source, tone mapped to 1080p SDR.
    ToneMap,
    /// A 4K HDR10 source to 1080p, its HDR kept (PGPU-06): HEVC and AV1
    /// only.
    KeepHdr,
    /// A 1080p source with picture subtitles burned in.
    BurnIn,
}

/// Where a conversion runs: on which card, and what of it there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RouteShare {
    /// Decoded, filtered and encoded on the GPU: nothing on the CPU.
    Whole,
    /// Decoded and encoded on the GPU; a filter on the CPU.
    DecodeAndEncode,
    /// Encoded on the GPU from what the CPU decoded.
    EncodeOnly,
    /// Encoded in software from what the GPU decoded.
    DecodeOnly,
    /// All in software.
    Cpu,
    /// Nothing here encodes the codec: never planned.
    None,
}

impl RouteShare {
    pub fn label(self) -> &'static str {
        match self {
            Self::Whole => "all on GPU",
            Self::DecodeAndEncode => "decode + encode",
            Self::EncodeOnly => "encode only",
            Self::DecodeOnly => "decode only",
            Self::Cpu => "CPU",
            Self::None => "not offered",
        }
    }
}

/// One cell of the routes: `job` of a `source` video to `target`, and
/// where it goes: the first step the jobs' pick takes, then each next
/// while every card before it is full.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuRoute {
    pub job: RouteJob,
    /// The protocol's codec names: `h264`, `hevc`, `av1`.
    pub source: String,
    pub target: String,
    /// Never empty. One step with [`RouteShare::None`] when nothing here
    /// encodes `target`.
    pub steps: Vec<RouteStep>,
    /// With every card of `steps` full, the playback is refused as busy
    /// until one has room: a GPU encode never falls back to the CPU.
    /// False where the last step takes no GPU slot (a software encode).
    pub busy_after: bool,
}

/// Where a route runs at one step: on which card, and what of it there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteStep {
    /// A [`GpuCard::key`]; `None` for one on the CPU alone.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub gpu: Option<String>,
    pub share: RouteShare,
    pub decoder: VideoDecoder,
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub encoder: Option<VideoEncoderUsed>,
    pub tone_map: FilterPlace,
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub tone_mapper: Option<ToneMapper>,
    pub burn_in: FilterPlace,
    /// Why the tone map or burn-in runs on the CPU here, when one does:
    /// `GPU filters are off (transcode.gpu_filters)`, `encoded in
    /// software`, or what the server's own notes say.
    pub cpu_why: Vec<String>,
}

/// The encode budget (`transcode.max_jobs`) and the caps beside it, now.
/// Budget amounts are in quarters of one software video encode: a GPU
/// video encode weighs 2, or 1 all on the GPU; an audio-only one 1; a
/// remux none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EncodeBudget {
    pub used_quarters: u32,
    pub max_quarters: u32,
    /// Remuxes running, and `transcode.max_remux`.
    pub remuxes: u32,
    pub max_remux: u32,
    /// GPU video encodes running, and `transcode.max_gpu_jobs`.
    pub gpu_jobs: u32,
    pub max_gpu_jobs: u32,
}

impl EncodeBudget {
    /// "1.25 of 2", as `max_jobs` is written.
    pub fn used_of_max(&self) -> String {
        format!(
            "{} of {}",
            quarters(self.used_quarters),
            quarters(self.max_quarters)
        )
    }
}

/// An amount of the encode budget, in software video encodes: `0.25`, `2`.
pub fn quarters(n: u32) -> String {
    let whole = n / 4;
    match n % 4 {
        0 => format!("{whole}"),
        1 => format!("{whole}.25"),
        2 => format!("{whole}.5"),
        _ => format!("{whole}.75"),
    }
}

/// What a conversion asks of the machine, which sets its weight against
/// the encode budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConversionLoad {
    /// Every stream copied: weighs nothing, under `transcode.max_remux`.
    Remux,
    /// Only the audio encoded.
    Audio,
    /// The video encoded on a GPU, under `transcode.max_gpu_jobs` too.
    Gpu,
    /// The video encoded in software.
    Software,
    /// The video decoded, scaled and encoded on a GPU, with nothing on
    /// the CPU; under `transcode.max_gpu_jobs` too.
    GpuOnly,
}

impl ConversionLoad {
    pub fn label(self) -> &'static str {
        match self {
            ConversionLoad::Remux => "remux",
            ConversionLoad::Audio => "audio encode",
            ConversionLoad::Gpu => "GPU video encode",
            ConversionLoad::Software => "software video encode",
            ConversionLoad::GpuOnly => "video all on the GPU",
        }
    }
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
    /// When `restart_pending`, what the server runs with until it starts
    /// again (PX-22): "Now", beside `value`, "After restart". `None` there
    /// when nothing set it then; always `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running: Option<serde_json::Value>,
    /// A deprecated setting (PGPU-05: `transcode.hardware`,
    /// `transcode.vaapi_device`): what to set instead, as it reads for
    /// the settings now (`transcode.gpus = [ … ]`). Still read meanwhile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprecated: Option<String>,
}

/// Every setting, and what the page around them needs to know.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub entries: Vec<SettingEntry>,
    /// The server's config file, if it has one.
    pub config_file: Option<String>,
    /// `admin.restart` can apply restart settings. Always true now: a
    /// server with no supervisor starts itself again in place.
    pub can_restart: bool,
    /// How `admin.restart` brings the server back.
    pub restart_mode: RestartMode,
}

/// What `admin.configReload` changed (PX-33), by setting key.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigReload {
    /// Settings now in effect: the live ones that changed.
    pub applied: Vec<String>,
    /// Restart settings that changed: `restartPending` until the next
    /// start.
    pub after_restart: Vec<String>,
}

/// What `admin.uplinkMeasure` found (PX-46): the home's upload, from
/// timed uploads to Cloudflare's public speed test.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UplinkMeasurement {
    /// The upload, in Mbit/s: the median of the samples.
    pub upload_mbps: f64,
    /// Each upload's rate, in Mbit/s, in the order made.
    pub samples_mbps: Vec<f64>,
    /// What `network.uplink.budget_mbps` would be at 80% of it, rounded
    /// down, at least 1.
    pub suggested_budget_mbps: u64,
}

/// How the server starts again after `admin.restart`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RestartMode {
    /// It exits, and the supervisor that runs it (systemd, where
    /// `SPOOLFRAME_SUPERVISED` is set) starts it again.
    Supervisor,
    /// It stops, then runs its own binary again in the same process: the
    /// same PID, arguments, environment and standard streams. For a
    /// server started by hand.
    InPlace,
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

/// The caller's track preferences (`me.trackPreferences`), and whether
/// this server reads picture subtitles as text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackPreferencesInfo {
    pub preferences: TrackPreferences,
    /// `subtitles.ocr` is on and tesseract has at least one language.
    /// When false, `preferences.subtitles.bitmap` has no effect: every
    /// picture subtitle is shown as pictures.
    pub ocr_offered: bool,
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
    /// with the caller's own playback state, the playlists holding it,
    /// and its hearts (PX-43): a track's or album's `liked` (`"all"`,
    /// `"some"`, `"none"`) and an album's `liked_children`, its tracks
    /// liked.
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
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub artwork_hit_rate: Option<f64>,
    pub subtitle_reads_per_sec: f64,
    pub plex_requests_per_sec: f64,
    pub plex_errors_per_sec: f64,
    /// Frame previews waiting or being made.
    pub previews_queued: u64,
    /// Bitmap subtitle tracks waiting or being read as text.
    pub ocr_queued: u64,
    /// The server's resident memory; `None` where it cannot be read.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub memory_bytes: Option<u64>,
    /// What that memory is; `None` where it cannot be read.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub memory: Option<MemoryParts>,
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub open_files: Option<u64>,
}

/// The server's resident memory by what it is. `heapBytes`, `filesBytes`
/// and `sharedBytes` add up to the resident memory; the caches' memory
/// tiers are part of the heap.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryParts {
    /// Anonymous memory: the heap and stacks.
    pub heap_bytes: u64,
    /// Pages of files mapped in: the binary and its libraries, mostly.
    pub files_bytes: u64,
    pub shared_bytes: u64,
    /// Each cache's memory tier, by its name ("Artwork").
    pub caches: Vec<MemoryPart>,
}

impl MemoryParts {
    /// The heap that is not a cache's.
    pub fn other_heap_bytes(&self) -> u64 {
        let caches: u64 = self.caches.iter().map(|c| c.bytes).sum();
        self.heap_bytes.saturating_sub(caches)
    }
}

/// One named part of the memory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryPart {
    pub name: String,
    pub bytes: u64,
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
    /// Conversions refused because they did not fit the encode budget
    /// (`transcode.max_jobs`) or their own cap.
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
    #[serde(deserialize_with = "spoolframe_types::required::option")]
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
    /// Only lines after this one: the `next` of the page before. `None`:
    /// the last lines.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub after: Option<u64>,
    /// Only lines at least this severe: `warn` gives warnings and errors.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub level: Option<LogLevel>,
    /// Only lines whose text or target contains this, in any case.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub text: Option<String>,
    /// At most this many lines; 500 if `None`.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
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
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub last_export_at: Option<i64>,
    /// Spans and log records dropped since `droppingSince`: since exports
    /// last stopped going through.
    pub dropped: u64,
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub dropping_since: Option<i64>,
    /// The last failure (the collector unreachable, what it answered, or
    /// no answer in time), and when, Unix ms.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub last_error: Option<String>,
    #[serde(deserialize_with = "spoolframe_types::required::option")]
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
    #[serde(deserialize_with = "spoolframe_types::required::option")]
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
/// (P3-15): `direct`, `remux`, `transcode`, `unsupported`; `range`, `hls`,
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
    /// What it asks of the machine, and what that weighs against the
    /// encode budget, in quarters ([`EncodeBudget`]).
    pub load: ConversionLoad,
    pub weight_quarters: u32,
}

/// A GPU's video API (PX-24).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum GpuApi {
    /// VA-API: Intel and AMD on Linux.
    Vaapi,
    /// NVIDIA: NVENC to encode, NVDEC (CUDA) to decode.
    Nvidia,
    /// Intel Quick Sync.
    Qsv,
    /// Vulkan: NVIDIA's GPU decoding for libplacebo (PGPU-04).
    Vulkan,
}

/// What tone maps HDR to SDR (PGPU-04).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ToneMapper {
    Cpu,
    /// The VA-API driver's own (`tonemap_vaapi`).
    Vaapi,
    /// libplacebo on ffmpeg's Vulkan device: the frames stay on the GPU.
    LibplaceboShared,
    /// libplacebo on a Vulkan device of its own, on the same GPU: the
    /// frames go through system memory.
    LibplaceboOwn,
}

impl ToneMapper {
    /// `CPU`, `VA-API`, `libplacebo (shared)`, `libplacebo (own)`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Cpu => "CPU",
            Self::Vaapi => "VA-API",
            Self::LibplaceboShared => "libplacebo (shared)",
            Self::LibplaceboOwn => "libplacebo (own)",
        }
    }
}

/// A tone map a GPU runs (PGPU-04).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuToneMap {
    pub gpu: GpuApi,
    pub mapper: ToneMapper,
}

/// What decodes a playback's video (PX-24).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum VideoDecoder {
    /// Nothing: direct play, or a copied video.
    None,
    Software,
    #[serde(rename_all = "camelCase")]
    Gpu {
        api: GpuApi,
    },
}

/// What encodes a playback's video (PX-24).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoEncoderUsed {
    /// The protocol's codec name: `h264`, `hevc`.
    pub codec: String,
    /// ffmpeg's encoder: `h264_nvenc`, `libx264`.
    pub name: String,
    /// The GPU it runs on; `None` in software.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub gpu: Option<GpuApi>,
}

/// Where a filter of the video runs (PX-24).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FilterPlace {
    /// Not done: an SDR source or HDR kept; no subtitle burned in.
    None,
    Gpu,
    Cpu,
}

/// How a playback is made (PX-24), from the plan and the filter graph
/// the server built, not from ffmpeg's output. The delivery (direct,
/// remux, transcode) is the row's `plan`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackMade {
    pub decoder: VideoDecoder,
    /// `None` when the video is not encoded.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub encoder: Option<VideoEncoderUsed>,
    /// HDR tone mapped to SDR.
    pub tone_map: FilterPlace,
    /// What tone mapped it (PGPU-04); `None` when nothing is tone mapped.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub tone_mapper: Option<ToneMapper>,
    /// A subtitle burned into the picture.
    pub burn_in: FilterPlace,
    /// The picture written, px; `None` with no video.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub width: Option<u32>,
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub height: Option<u32>,
    /// The encoder's stated target, bit/s, when it has one.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub target_bitrate: Option<u64>,
    /// What was written, bit/s: HLS segments' bytes over their durations,
    /// or a progressive file over the time encoded. `None` for direct
    /// play, and until something is written.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub measured_bitrate: Option<u64>,
}

/// Where a playback's keyframes came from (PX-24, PX-25).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "from", rename_all = "camelCase")]
pub enum KeyframesRead {
    /// None were read: only a copied video's HLS needs them.
    None,
    /// The file's own index.
    Index { ms: u64 },
    /// Every packet of the file.
    Scan { ms: u64 },
    /// An earlier scan of the file, kept on disk.
    Stored { ms: u64 },
}

/// When a playback's first segment was ready (PX-24).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum FirstSegment {
    /// Direct play has none: the file is read as it is.
    NotApplicable,
    /// Its conversion has written none yet.
    Waiting,
    /// This long after the request.
    Ready { ms: u64 },
    /// Its first encode ended before it wrote one.
    Never,
}

/// How long a playback took to start, from its request (PX-24): what the
/// server's "playback start timed" log line says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackStart {
    /// From the request to the plan.
    pub plan_ms: u64,
    /// The keyframes' read alone.
    pub keyframes: KeyframesRead,
    /// From the request to the first segment.
    pub first_segment: FirstSegment,
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
    /// How it is made (PX-24).
    pub made: PlaybackMade,
    /// How long it took to start (PX-24).
    pub start: PlaybackStart,
    /// Its device's link, as the server's "link" log lines read it every
    /// 10 s; `None` before the first interval. Added after the web UI
    /// shipped with the server, so an older server leaves it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<PlaybackLink>,
    /// A player fetched it from the HTTP playback port (PX-49), as well as
    /// or instead of its device over Iroh: an AirPlay receiver, say.
    /// False from an older server.
    #[serde(default)]
    pub over_http: bool,
}

/// A playback's link to its device (the server's "link" lines): the last
/// interval, and the totals so far.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackLink {
    /// `lan …`, `direct …` or `relay …`.
    pub path: String,
    pub rtt_ms: u64,
    /// The last interval's send rate, bits per second.
    pub send_bps: u64,
    /// The last interval's packets lost, in hundredths of a percent.
    pub lost_hundredths: u32,
    /// Path changes in the last interval.
    pub switches: u64,
    /// Sent over the playback so far, bits per second.
    pub average_bps: u64,
    pub max_rtt_ms: u64,
    /// Lost over the playback so far, hundredths of a percent.
    pub total_lost_hundredths: u32,
    pub total_switches: u64,
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
    /// What it played is in a library no longer served (B-10).
    AccessRemoved,
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
    /// Pictures: drawn by the device if it can, else burned in. Planned as
    /// the client protocol's `burn_in`, which no longer forces a burn-in.
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

/// An external text subtitle's timing (PX-03): its sync, and the manual
/// offset on top, the same for every user.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubtitleTimingInfo {
    /// Its place in the item's subtitles, as clients number them.
    pub track: u32,
    /// `Film.en.srt`: the file's name.
    pub label: String,
    pub sync: SubtitleSyncState,
    /// Added to every cue, ms; positive shows the text later.
    pub offset_ms: i64,
    /// Who set the offset last, by display name.
    pub offset_by: Option<String>,
    /// When, Unix seconds.
    pub offset_at: Option<i64>,
    /// Where a downloaded track came from (PX-02); `None` for a file
    /// beside the media.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download: Option<spoolframe_types::SubtitleDownload>,
}

/// Where a subtitle's sync is (PX-03).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum SubtitleSyncState {
    /// As the file has it: never synced, or put back.
    Original,
    /// Being synced, or waiting its turn; served as it was meanwhile.
    Syncing,
    #[serde(rename_all = "camelCase")]
    Synced {
        offset_ms: i64,
        /// The frame-rate ratio: 1 for none, 1.0427 for 23.976 to 25.
        factor: f64,
        splits: u32,
        /// 0 to 100.
        score: u8,
    },
    /// Scored too low to trust: served with its original timing.
    #[serde(rename_all = "camelCase")]
    Uncertain { score: u8 },
    #[serde(rename_all = "camelCase")]
    Failed { reason: String },
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

/// The kinds of item a playlist may hold (PX-40), ticked when it is made;
/// they can later only be widened.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistKinds {
    pub movies: bool,
    pub episodes: bool,
    pub tracks: bool,
}

/// One user a playlist is shared with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistShare {
    /// `local:<uuid>` or `portable:<hex>`, as `playlistShareCandidates`
    /// lists them.
    pub user_id: String,
    /// They may add, remove and move its entries; never rename it, change
    /// its kinds or sharing, or delete it.
    pub can_edit: bool,
}

/// Who sees a playlist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum PlaylistSharing {
    /// Its owner alone.
    Personal,
    /// Its owner and these users. Listed only to whoever manages it;
    /// anyone else gets it empty.
    Shared { with: Vec<PlaylistShare> },
    /// Every user of the server; admins edit it. Only an admin makes one.
    ServerWide,
}

/// How a playlist stands to the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlaylistOwner {
    /// Theirs, personal or shared by them.
    Mine,
    /// Someone else's, shared with them.
    SharedWithMe,
    /// Seen by every user.
    ServerWide,
}

/// A built-in playlist (PX-43): every user's own, never renamed, shared
/// or deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlaylistBuiltin {
    /// The tracks the caller liked (`me.like`), newest first unless
    /// moved; entries are moved and removed, never added, by
    /// `me.playlistEdit`.
    Liked,
    /// Every track the caller may see, worked out at each read. Read-only.
    All,
}

/// A playlist as the caller sees it, without its entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistInfo {
    pub id: String,
    pub name: String,
    pub kinds: PlaylistKinds,
    pub owner: PlaylistOwner,
    pub sharing: PlaylistSharing,
    /// They may change its entries (`me.playlistEdit`).
    pub can_edit: bool,
    /// They may rename it, widen its kinds, change its sharing
    /// (`me.playlistUpdate`), and delete it.
    pub can_manage: bool,
    /// What `me.playlistEdit` names; raised by every change.
    pub revision: u64,
    /// Which built-in playlist it is (PX-43); `None` for one a user made.
    #[serde(default)]
    pub builtin: Option<PlaylistBuiltin>,
    /// Its picture's artwork id (PX-53): the uploaded cover, else the
    /// mosaic of its first four covers, a placeholder, or Liked's or
    /// All's tile. Fetched from `GET /playlists/art?id=<playlist>&size=…`;
    /// it changes when the picture does.
    #[serde(default)]
    pub artwork: Option<String>,
    /// The picture is an uploaded cover, which `me.playlistRemoveArt`
    /// takes away.
    #[serde(default)]
    pub custom_art: bool,
}

/// One entry of a playlist: its item as the catalog has it now, with the
/// caller's watch state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistEntryInfo {
    /// Its place in the playlist, as edits name it; the same item may be
    /// in a playlist twice.
    pub entry: u64,
    pub item: Item,
    /// Still in the library, but its file cannot be read now.
    pub unavailable: bool,
    /// A track in the caller's Liked (PX-43): its heart.
    #[serde(default)]
    pub liked: bool,
}

/// A playlist and one page of its entries (`me.playlist`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistDetail {
    pub playlist: PlaylistInfo,
    pub entries: Vec<PlaylistEntryInfo>,
    /// Where `entries` start in the whole list.
    pub offset: u32,
    /// How many entries the caller sees in all.
    pub total: u32,
    /// How many of them can be played now.
    pub playable: u32,
}

/// A playlist to make (`me.playlistCreate`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewPlaylist {
    /// 1 to 100 characters.
    pub name: String,
    /// At least one.
    pub kinds: PlaylistKinds,
    pub sharing: PlaylistSharing,
}

/// What `me.playlistUpdate` changes; `None` leaves it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistChanges {
    #[serde(default)]
    pub name: Option<String>,
    /// Only wider than now.
    #[serde(default)]
    pub kinds: Option<PlaylistKinds>,
    #[serde(default)]
    pub sharing: Option<PlaylistSharing>,
}

/// One change to a playlist's entries (`me.playlistEdit`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum PlaylistEditOp {
    /// Add item `id` (a MediaId) before entry `before`, or at the end. A
    /// season, show, album or artist adds its episodes or tracks.
    Add {
        id: String,
        before: Option<u64>,
    },
    Remove {
        entry: u64,
    },
    /// Put the entry before `before`, or at the end.
    Move {
        entry: u64,
        before: Option<u64>,
    },
}

/// Someone a playlist can be shared with: a local user, or a portable user
/// granted access here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareableUser {
    /// As `PlaylistShare::user_id` takes it.
    pub user_id: String,
    pub display_name: String,
}

/// What importing the caller's Plex playlists made, or would make.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlexPlaylistImport {
    /// One per Plex playlist with at least one item found here.
    pub made: Vec<PlexPlaylistMade>,
    /// The items, or whole playlists, not found here, and why.
    pub unmapped: Vec<PlexPlaylistUnmapped>,
    /// Plex servers that could not be read for the caller, and why.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlexPlaylistMade {
    /// Its name in Plex, and here.
    pub name: String,
    /// The playlist made; `None` on a dry run.
    pub playlist_id: Option<String>,
    pub entries: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlexPlaylistUnmapped {
    /// The Plex playlist's name.
    pub playlist: String,
    /// The item's title; `None` for the whole playlist.
    pub title: Option<String>,
    pub reason: String,
}

/// The caller's own account, for every role.
/// Only with `client` or `server`: the macro makes one or both.
#[cfg(any(feature = "client", feature = "server"))]
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
    /// needs a recent sign-in (`RECENT_SIGN_IN_REQUIRED`). Every other
    /// session of the caller's, on the web and on devices, is revoked
    /// (PX-10); this one stays.
    #[method(name = "setPassword", with_extensions)]
    async fn set_password(&self, change: PasswordChange) -> jsonrpsee::core::RpcResult<()>;

    /// The caller's live sessions, on every device, most recently used
    /// first.
    #[method(name = "sessions", with_extensions)]
    async fn sessions(&self) -> jsonrpsee::core::RpcResult<Vec<SessionEntry>>;

    /// End one of the caller's own sessions.
    #[method(name = "revokeSession", with_extensions)]
    async fn revoke_session(&self, id: String) -> jsonrpsee::core::RpcResult<()>;

    /// The apps connected to the caller's account through MCP (PX-42),
    /// most recently connected first.
    #[method(name = "connectedApps", with_extensions)]
    async fn connected_apps(&self) -> jsonrpsee::core::RpcResult<Vec<ConnectedApp>>;

    /// Disconnect one of them: its tokens stop at once, for good.
    /// Reconnecting means signing in and consenting again. `NO_SUCH_APP`
    /// for one that is not the caller's.
    #[method(name = "revokeConnectedApp", with_extensions)]
    async fn revoke_connected_app(&self, id: String) -> jsonrpsee::core::RpcResult<()>;

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
    /// them, and whether this server reads picture subtitles as text.
    #[method(name = "trackPreferences", with_extensions)]
    async fn track_preferences(&self) -> jsonrpsee::core::RpcResult<TrackPreferencesInfo>;

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
    /// `remember` is the approver's answer to the device's request to be
    /// remembered (P4-01): it is trusted only if it asked and this is
    /// true.
    #[method(name = "deviceSignInApprove", with_extensions)]
    async fn device_sign_in_approve(
        &self,
        code: String,
        remember: bool,
    ) -> jsonrpsee::core::RpcResult<()>;

    /// Trust one of the caller's devices (an `endpointId` from `devices`),
    /// or stop, and say whether it opens their profile at start (P4-01).
    /// Needs a recent sign-in either way (`RECENT_SIGN_IN_REQUIRED`).
    /// `autoLogin` needs `trusted` (`INVALID_INPUT`); it turns auto-login
    /// off for the device's other users, and the PIN requirement off for
    /// the caller there. Stopping trust ends the caller's sessions on that
    /// device. A device they never signed in on is `NO_SUCH_DEVICE`.
    #[method(name = "setDeviceTrust", with_extensions)]
    async fn set_device_trust(
        &self,
        endpoint_id: String,
        trusted: bool,
        auto_login: bool,
    ) -> jsonrpsee::core::RpcResult<()>;

    /// Delete one of the caller's devices (an `endpointId` from
    /// `devices`; P4-03): their sessions there are deleted, and with them
    /// its trust and PIN requirement. It leaves `devices`, and if it signs
    /// in again it is listed as new. Other users' sessions on that device
    /// are kept. Always needs a recent sign-in (`RECENT_SIGN_IN_REQUIRED`).
    /// A device they never signed in on is `NO_SUCH_DEVICE`.
    #[method(name = "forgetDevice", with_extensions)]
    async fn forget_device(&self, endpoint_id: String) -> jsonrpsee::core::RpcResult<()>;

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

    /// Remove the caller's password: a passkey or a linked provider signs
    /// them in from then on. Needs a recent sign-in. Refused for their last
    /// way to sign in (`LAST_SIGN_IN_METHOD`) and for the last enabled
    /// admin with a password (`LAST_ADMIN`). Their other sessions end and
    /// their trusted devices are forgotten, as with a new password.
    #[method(name = "passwordRemove", with_extensions)]
    async fn password_remove(&self) -> jsonrpsee::core::RpcResult<()>;

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

    /// Sever the caller's tie to the portable identity `id` (P7-04,
    /// P7-09; `PortableTie::id`): only that link goes; the account's other
    /// ties, its sessions and its watch state stay. Needs a recent
    /// sign-in. `NO_SUCH_LINK` for an identity the account is not tied to.
    /// Tying is done from the identity's app, never here: the web UI
    /// never holds the root.
    #[method(name = "portableUntie", with_extensions)]
    async fn portable_untie(&self, id: String) -> jsonrpsee::core::RpcResult<()>;

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

    /// Every playlist the caller sees (PX-40): their own, those shared with
    /// them, and the server-wide ones, by name. An admin manages every
    /// server-wide one; `NOT_CONFIGURED` on a server without a library.
    #[method(name = "playlists", with_extensions)]
    async fn playlists(&self) -> jsonrpsee::core::RpcResult<Vec<PlaylistInfo>>;

    /// One playlist and a page of its entries, from `offset`, at most
    /// `limit` (200). `NO_SUCH_PLAYLIST` for one the caller does not see.
    #[method(name = "playlist", with_extensions)]
    async fn playlist(
        &self,
        id: String,
        offset: u32,
        limit: u32,
    ) -> jsonrpsee::core::RpcResult<PlaylistDetail>;

    /// Make a playlist owned by the caller. A server-wide one is an
    /// admin's to make (`FORBIDDEN`).
    #[method(name = "playlistCreate", with_extensions)]
    async fn playlist_create(
        &self,
        playlist: NewPlaylist,
    ) -> jsonrpsee::core::RpcResult<PlaylistInfo>;

    /// Rename a playlist, widen its kinds, or change its sharing: its
    /// owner's to do, or any admin's for a server-wide one
    /// (`PLAYLIST_READ_ONLY` for anyone else who sees it).
    #[method(name = "playlistUpdate", with_extensions)]
    async fn playlist_update(
        &self,
        id: String,
        changes: PlaylistChanges,
    ) -> jsonrpsee::core::RpcResult<PlaylistInfo>;

    /// Change a playlist's entries, as of `revision`: its new revision. An
    /// edit someone else's change overtook is redone when it can be, else
    /// `PLAYLIST_CONFLICT` (`data`: `{"revision": n}`, the one now).
    /// `KIND_NOT_ALLOWED` for an item of a kind it does not take;
    /// `PLAYLIST_READ_ONLY` on a read-only share.
    #[method(name = "playlistEdit", with_extensions)]
    async fn playlist_edit(
        &self,
        id: String,
        revision: u64,
        edits: Vec<PlaylistEditOp>,
    ) -> jsonrpsee::core::RpcResult<u64>;

    /// Delete a playlist: its owner's, or any admin's for a server-wide
    /// one.
    #[method(name = "playlistDelete", with_extensions)]
    async fn playlist_delete(&self, id: String) -> jsonrpsee::core::RpcResult<()>;

    /// Take a playlist's uploaded cover away (PX-53): its mosaic again.
    /// Whoever may edit its entries; anyone else who sees it, and anyone
    /// for Liked and All, `NOT_ALLOWED`. A cover is uploaded with
    /// `POST /playlists/art?id=<playlist>`, the picture as the body (a
    /// JSON-RPC message is no place for 20 MB), answered with the
    /// `PlaylistInfo` after, or this API's error as JSON.
    #[method(name = "playlistRemoveArt", with_extensions)]
    async fn playlist_remove_art(&self, id: String) -> jsonrpsee::core::RpcResult<PlaylistInfo>;

    /// Who the caller can share a playlist with: every other enabled local
    /// user and every portable user granted access, by name.
    #[method(name = "playlistShareCandidates", with_extensions)]
    async fn playlist_share_candidates(&self) -> jsonrpsee::core::RpcResult<Vec<ShareableUser>>;

    /// Import the caller's Plex playlists once, as personal playlists of
    /// theirs; with `dryRun`, say what would be made and changed nothing.
    /// `NOT_CONFIGURED` when they have no Plex account linked.
    #[method(name = "playlistPlexImport", with_extensions)]
    async fn playlist_plex_import(
        &self,
        dry_run: bool,
    ) -> jsonrpsee::core::RpcResult<PlexPlaylistImport>;

    /// Like tracks or albums (PX-43), by MediaId: their tracks not liked
    /// yet go to the top of the caller's Liked, an album's in disc and
    /// track order. Liked as it is after. 1 to 100 items;
    /// `KIND_NOT_ALLOWED` for anything but a track or an album.
    #[method(name = "like", with_extensions)]
    async fn like(&self, items: Vec<String>) -> jsonrpsee::core::RpcResult<PlaylistInfo>;

    /// Take tracks, or every track of albums, out of the caller's Liked
    /// (PX-43). Liked as it is after.
    #[method(name = "unlike", with_extensions)]
    async fn unlike(&self, items: Vec<String>) -> jsonrpsee::core::RpcResult<PlaylistInfo>;
}

/// Other users and the server: admins only.
/// Only with `client` or `server`: the macro makes one or both.
#[cfg(any(feature = "client", feature = "server"))]
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

    /// Adding an admin needs a recent sign-in (PX-10), as
    /// `me.*` changes do: `RECENT_SIGN_IN_REQUIRED` otherwise.
    #[method(name = "userAdd", with_extensions)]
    async fn user_add(&self, user: NewUser) -> jsonrpsee::core::RpcResult<UserInfo>;

    /// Set anyone's password, without the current one; for a user with
    /// passkeys only, that adds one. Needs a recent sign-in (PX-10), as
    /// `me.*` changes do: `RECENT_SIGN_IN_REQUIRED` otherwise. Every
    /// session of the user's is revoked (PX-10), but the caller's own
    /// when they set their own.
    #[method(name = "userSetPassword", with_extensions)]
    async fn user_set_password(&self, credentials: Credentials) -> jsonrpsee::core::RpcResult<()>;

    /// Reset a user's password to a one-time one, shown this once: they
    /// must choose a new one when they sign in with it, and it stops
    /// working after 7 days. Ends their sessions signed in with a
    /// password (or of unknown origin) and the devices trusted from them;
    /// passkey, sign-on provider and portable identity sessions stay, as
    /// do their passkeys, provider links and ties. Needs a recent sign-in
    /// (PX-10): `RECENT_SIGN_IN_REQUIRED` otherwise. Not the caller's own
    /// password (`INVALID_INPUT`: use `me.setPassword`).
    #[method(name = "userResetPassword", with_extensions)]
    async fn user_reset_password(
        &self,
        username: String,
    ) -> jsonrpsee::core::RpcResult<OneTimePassword>;

    /// Change a user's role. The last enabled admin cannot be demoted
    /// (`LAST_ADMIN`). Takes effect on their next call, on every session.
    /// Making an admin needs a recent sign-in (PX-10), as
    /// `me.*` changes do: `RECENT_SIGN_IN_REQUIRED` otherwise.
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
    /// Needs a recent sign-in (PX-10), as
    /// `me.*` changes do: `RECENT_SIGN_IN_REQUIRED` otherwise.
    #[method(name = "userClearPin", with_extensions)]
    async fn user_clear_pin(&self, username: String) -> jsonrpsee::core::RpcResult<()>;

    /// The devices a user has signed in from.
    #[method(name = "userDevices", with_extensions)]
    async fn user_devices(&self, username: String) -> jsonrpsee::core::RpcResult<Vec<DeviceEntry>>;

    /// Ask for a user's PIN on one of their devices, or stop asking.
    /// Stopping needs a recent sign-in (PX-10), as
    /// `me.*` changes do: `RECENT_SIGN_IN_REQUIRED` otherwise.
    #[method(name = "userSetPinRequired", with_extensions)]
    async fn user_set_pin_required(
        &self,
        username: String,
        endpoint_id: String,
        required: bool,
    ) -> jsonrpsee::core::RpcResult<()>;

    /// `me.setDeviceTrust` for one of a user's devices (P4-01). Needs a
    /// recent sign-in (`RECENT_SIGN_IN_REQUIRED`).
    #[method(name = "userSetDeviceTrust", with_extensions)]
    async fn user_set_device_trust(
        &self,
        username: String,
        endpoint_id: String,
        trusted: bool,
        auto_login: bool,
    ) -> jsonrpsee::core::RpcResult<()>;

    /// `me.forgetDevice` for one of a user's devices (P4-03). Needs a
    /// recent sign-in (`RECENT_SIGN_IN_REQUIRED`); a device the user never
    /// signed in on is `NO_SUCH_DEVICE`.
    #[method(name = "userForgetDevice", with_extensions)]
    async fn user_forget_device(
        &self,
        username: String,
        endpoint_id: String,
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

    /// Remove a user's password, as `passwordRemove` does the caller's.
    /// Needs a recent sign-in (PX-10): `RECENT_SIGN_IN_REQUIRED` otherwise.
    /// Every session of the user's is revoked, but the caller's own.
    #[method(name = "userRemovePassword", with_extensions)]
    async fn user_remove_password(&self, username: String) -> jsonrpsee::core::RpcResult<()>;

    /// Remove one of a user's passkeys, by its id from `userPasskeys`.
    /// Needs a recent sign-in (PX-10), as
    /// `me.*` changes do: `RECENT_SIGN_IN_REQUIRED` otherwise.
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
    /// `me.ssoUnlink` does; their last way to sign in stays. Needs a
    /// recent sign-in (PX-10), as
    /// `me.*` changes do: `RECENT_SIGN_IN_REQUIRED` otherwise.
    #[method(name = "userSsoUnlink", with_extensions)]
    async fn user_sso_unlink(
        &self,
        username: String,
        issuer: String,
    ) -> jsonrpsee::core::RpcResult<()>;

    /// Sever `username`'s tie to the portable identity `id` (P7-04,
    /// P7-09), as `me.portableUntie` does: only that link goes. Needs a
    /// recent sign-in (`RECENT_SIGN_IN_REQUIRED`). An admin can sever a
    /// tie, never make one. `NO_SUCH_LINK` for an identity the account is
    /// not tied to.
    #[method(name = "userUntie", with_extensions)]
    async fn user_untie(&self, username: String, id: String) -> jsonrpsee::core::RpcResult<()>;

    /// Keep `secret` as the client secret of the sign-in provider called
    /// `provider` (P3-04), or forget it (`None`): a secret, in the state
    /// directory, never shown. A provider whose config names a
    /// `client_secret_file` reads that instead (`SETTING_LOCKED`). Needs
    /// a recent sign-in (PX-10), as
    /// `me.*` changes do: `RECENT_SIGN_IN_REQUIRED` otherwise.
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
    /// is linked to which Plex account, and how their sync is going. It
    /// syncs nothing: `plexWatchSync` does.
    #[method(name = "plexSyncOverview", with_extensions)]
    async fn plex_sync_overview(&self) -> jsonrpsee::core::RpcResult<PlexSyncOverview>;

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
    /// one a server shared with it takes. Needs a recent sign-in
    /// (finding 44): `RECENT_SIGN_IN_REQUIRED` otherwise. `NOT_CONFIGURED`
    /// without an account, `INVALID_INPUT` when it does not reach `url`,
    /// and `UNAVAILABLE` when plex.tv cannot be reached.
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
    /// secret, never in the runtime settings. An `admin.*`, `auth.*`,
    /// `media.*` or `telemetry.*` key, or `plex.token`,
    /// `plex.token_file`, `plex.url` or `plex.servers`, needs a recent
    /// sign-in (PX-10), as `me.*` changes do: `RECENT_SIGN_IN_REQUIRED`
    /// otherwise.
    #[method(name = "settingSet", with_extensions)]
    async fn setting_set(
        &self,
        key: String,
        value: serde_json::Value,
    ) -> jsonrpsee::core::RpcResult<SettingEntry>;

    /// Take `key` out of the runtime settings: the config file's value or
    /// the default applies again. An `admin.*`, `auth.*`, `media.*` or
    /// `telemetry.*` key, or `plex.token`, `plex.token_file`, `plex.url`
    /// or `plex.servers`, needs a recent sign-in (PX-10), as `me.*`
    /// changes do: `RECENT_SIGN_IN_REQUIRED` otherwise.
    #[method(name = "settingUnset", with_extensions)]
    async fn setting_unset(&self, key: String) -> jsonrpsee::core::RpcResult<SettingEntry>;

    /// Restart the server, with the restart settings applied, the way
    /// `Settings::restart_mode` says. It answers first, then stops as for
    /// SIGTERM: connections closed, playbacks and encodes ended, the
    /// store closed. Under a supervisor it exits with 75 (EX_TEMPFAIL)
    /// for the supervisor to start it again. Started by hand, it runs
    /// its own binary again in place: the same PID, arguments,
    /// environment and standard streams, so a pipe its log goes to keeps
    /// receiving it. A failed exec is logged, and the process exits
    /// non-zero. Connected devices drop for a few seconds either way.
    /// Needs a recent sign-in: `RECENT_SIGN_IN_REQUIRED` otherwise.
    #[method(name = "restart", with_extensions)]
    async fn restart(&self) -> jsonrpsee::core::RpcResult<()>;

    /// Read the config file and the runtime settings again, as SIGHUP
    /// does (PX-33): checked as at start, the live settings applied, the
    /// restart ones `restartPending`, the encoders detected again. A file
    /// the server would not start with changes nothing: `INVALID_INPUT`
    /// with what is wrong and where. Needs a recent sign-in:
    /// `RECENT_SIGN_IN_REQUIRED` otherwise.
    #[method(name = "configReload", with_extensions)]
    async fn config_reload(&self) -> jsonrpsee::core::RpcResult<ConfigReload>;

    /// Measure the home's upload (PX-46) with Cloudflare's public speed
    /// test (`speed.cloudflare.com/__up`): a few uploads of 10 MB, each
    /// timed, for `network.uplink.budget_mbps`. Sends about 30 MB and
    /// takes up to half a minute; changes no setting. Cloudflare out of
    /// reach is `UNAVAILABLE` with why. Needs an admin.
    #[method(name = "uplinkMeasure", with_extensions)]
    async fn uplink_measure(&self) -> jsonrpsee::core::RpcResult<UplinkMeasurement>;

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

    /// Ask the router now to map the server's port (P6-02), and give the
    /// mapping back when it answers: what `mode = "auto"` would get, for
    /// the wizard to show before it is chosen. `Asking` if the router has
    /// not answered within about half a minute. `NOT_CONFIGURED` while the
    /// endpoint is starting.
    #[method(name = "wanTryMapping", with_extensions)]
    async fn wan_try_mapping(&self) -> jsonrpsee::core::RpcResult<MappingStatus>;

    /// Ask wtfismyip for this network's public IPv4 address now (P6-02).
    /// `UNAVAILABLE`, with why, when it does not answer with a public one.
    #[method(name = "wanLookup", with_extensions)]
    async fn wan_lookup(&self) -> jsonrpsee::core::RpcResult<String>;

    /// The PMS at `url` as `plexStatus` would show it, without saving
    /// anything: with `token`, or else the one the caller's linked Plex
    /// account (or the wizard's sign-in) has for that server, as plex.tv
    /// lists it, or else, for a server already added at `url`, its own.
    /// Never another server's token (PX-10): a URL found on the LAN may
    /// be anyone's.
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
    /// is. With `keepToken`, it needs a recent sign-in (finding 44):
    /// `RECENT_SIGN_IN_REQUIRED`, before the PIN is looked at, so an
    /// approved one can be checked again once confirmed.
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
    /// added is checked before it is (P3-11). Mappings are tried, not
    /// saved, and nothing is kept: `plexCheckPaths` checks every file
    /// under the saved ones.
    #[method(name = "plexPathTry", with_extensions)]
    async fn plex_path_try(
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
    /// none: never the first server's (PX-10).
    /// It must answer; it is named by what it calls itself. Without a
    /// first server, it becomes the first. No library is added.
    #[method(name = "plexServerAdd", with_extensions)]
    async fn plex_server_add(
        &self,
        url: String,
        token: Option<String>,
    ) -> jsonrpsee::core::RpcResult<PlexServerEntry>;

    /// Keep `token` as Plex server `id`'s, as a secret. Needs a recent
    /// sign-in (PX-10), as
    /// `me.*` changes do: `RECENT_SIGN_IN_REQUIRED` otherwise.
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

    /// An item's external text subtitles (SRT, WebVTT, ASS beside its
    /// file) and how each is timed (PX-03). `UNAVAILABLE` when the file
    /// cannot be read.
    #[method(name = "subtitleTimings", with_extensions)]
    async fn subtitle_timings(
        &self,
        id: String,
    ) -> jsonrpsee::core::RpcResult<Vec<SubtitleTimingInfo>>;

    /// Sync item `id`'s subtitle `track` to its file, in the background
    /// (PX-03): `syncing` now; `subtitleTimings` says when it is done. It
    /// is served as it was until then. `INVALID_INPUT` for a track that is
    /// not an external text subtitle, or one the item does not have.
    #[method(name = "subtitleSync", with_extensions)]
    async fn subtitle_sync(
        &self,
        id: String,
        track: u32,
    ) -> jsonrpsee::core::RpcResult<SubtitleTimingInfo>;

    /// Serve item `id`'s subtitle `track` with its original timing again,
    /// for everyone (PX-03); its offset stays.
    #[method(name = "subtitleUseOriginal", with_extensions)]
    async fn subtitle_use_original(
        &self,
        id: String,
        track: u32,
    ) -> jsonrpsee::core::RpcResult<SubtitleTimingInfo>;

    /// Move item `id`'s subtitle `track` by `offset_ms` on top of its
    /// timing, for everyone (PX-03), as set by the caller; 0 resets it. At
    /// most ten minutes either way (`INVALID_INPUT`).
    #[method(name = "subtitleSetOffset", with_extensions)]
    async fn subtitle_set_offset(
        &self,
        id: String,
        track: u32,
        offset_ms: i64,
    ) -> jsonrpsee::core::RpcResult<SubtitleTimingInfo>;

    /// Search OpenSubtitles for item `id`'s subtitles in `language` (`en`,
    /// `pt-BR`), or in the languages set (PX-02): best first, as the
    /// client protocol's `SearchSubtitles`. Kept a day per item and
    /// language. `NOT_CONFIGURED` without OpenSubtitles set up;
    /// `UNAVAILABLE` when it cannot be reached; `RATE_LIMITED` when it
    /// asks to wait.
    #[method(name = "subtitleSearch", with_extensions)]
    async fn subtitle_search(
        &self,
        id: String,
        language: Option<String>,
    ) -> jsonrpsee::core::RpcResult<spoolframe_types::SubtitleSearch>;

    /// Download result `file_id` of a search for item `id` (PX-02): a
    /// track of the item for everyone, at once, synced in the background
    /// when it was not found by the file's hash. The same result again is
    /// the same track. `INVALID_INPUT` for a result no search of the item
    /// listed lately (search again) or a file that is no subtitle;
    /// `QUOTA_EXHAUSTED`; `NOT_ALLOWED` for a caller whose downloads were
    /// turned off.
    #[method(name = "subtitleDownload", with_extensions)]
    async fn subtitle_download(
        &self,
        id: String,
        file_id: u64,
    ) -> jsonrpsee::core::RpcResult<spoolframe_types::SubtitleDownloaded>;

    /// Remove item `id`'s downloaded subtitle `download`, for everyone
    /// (PX-02): its files go, and its result is listed last, marked, in
    /// the item's searches. Removing one already gone does nothing.
    #[method(name = "subtitleRemove", with_extensions)]
    async fn subtitle_remove(&self, id: String, download: String)
    -> jsonrpsee::core::RpcResult<()>;

    /// Whether user `username` may download subtitles (PX-02): everyone
    /// may unless an admin turned it off.
    #[method(name = "userSubtitleDownloads", with_extensions)]
    async fn user_subtitle_downloads(&self, username: String) -> jsonrpsee::core::RpcResult<bool>;

    /// Let user `username` download subtitles, or not (PX-02). Searching
    /// stays theirs either way.
    #[method(name = "userSetSubtitleDownloads", with_extensions)]
    async fn user_set_subtitle_downloads(
        &self,
        username: String,
        allowed: bool,
    ) -> jsonrpsee::core::RpcResult<()>;

    /// Make an invite (P6-01): its code, shown this once, and its link.
    /// Needs a recent sign-in: the code makes an account
    /// (`RECENT_SIGN_IN_REQUIRED`). `INVALID_INPUT`, saying why, for a
    /// kind this server does not make yet (`federation` arrives with P8),
    /// a `portable-account` invite with a role other than consumer, or a
    /// note, a number of uses or a time that is out of range. `NOT_CONFIGURED` while the endpoint
    /// is starting: the link needs its key.
    #[method(name = "inviteCreate", with_extensions)]
    async fn invite_create(&self, invite: NewInvite) -> jsonrpsee::core::RpcResult<InviteCreated>;

    /// The invites that can still make an account, newest first; never a
    /// code. Used-up, expired and revoked ones are left out.
    #[method(name = "inviteList", with_extensions)]
    async fn invite_list(&self) -> jsonrpsee::core::RpcResult<Vec<InviteEntry>>;

    /// Revoke invite `id`: its unused code stops working; the accounts it
    /// made stay. Needs a recent sign-in. `NO_SUCH_INVITE` for one that
    /// cannot make an account anyway.
    #[method(name = "inviteRevoke", with_extensions)]
    async fn invite_revoke(&self, id: String) -> jsonrpsee::core::RpcResult<()>;

    /// The server's link without a code, in both forms: it finds and
    /// saves the server, and the person signs in with an account they
    /// have. It never expires. `NOT_CONFIGURED` while the endpoint is
    /// starting.
    #[method(name = "inviteLink", with_extensions)]
    async fn invite_link(&self) -> jsonrpsee::core::RpcResult<ServerLink>;

    /// The apps connected to every account through MCP (PX-42), most
    /// recently connected first.
    #[method(name = "connectedApps", with_extensions)]
    async fn all_connected_apps(&self) -> jsonrpsee::core::RpcResult<Vec<ConnectedApp>>;

    /// Disconnect any user's app: its tokens stop at once, for good.
    /// `NO_SUCH_APP` for one that is not connected.
    #[method(name = "revokeConnectedApp", with_extensions)]
    async fn revoke_any_connected_app(&self, id: String) -> jsonrpsee::core::RpcResult<()>;

    /// A page of the audit list (PX-42), newest first: what admins did
    /// through MCP clients, and which apps were connected and
    /// disconnected. `limit` is at most 200.
    #[method(name = "auditList", with_extensions)]
    async fn audit_list(&self, offset: u32, limit: u32) -> jsonrpsee::core::RpcResult<AuditPage>;

    /// The portable users granted access here (P7-06), oldest first.
    #[method(name = "grantList", with_extensions)]
    async fn grant_list(&self) -> jsonrpsee::core::RpcResult<Vec<PortableGrant>>;

    /// Take portable user `id`'s grant away (P7-06): their sessions end
    /// at once, and only an invite brings them back. Their watch state
    /// stays, for if they do. Needs a recent sign-in. `NO_SUCH_GRANT` for
    /// one with no grant.
    #[method(name = "grantRevoke", with_extensions)]
    async fn grant_revoke(&self, id: String) -> jsonrpsee::core::RpcResult<()>;

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

/// What was `serde(default)` before PX-15 is required: a server and a web
/// UI that disagree about a shape fail on the call, not quietly.
#[cfg(test)]
mod required_field_tests {
    use serde::de::DeserializeOwned;

    use super::*;

    /// `value` as JSON, then each of `fields` left out in turn: every one
    /// must be refused as missing, not filled in.
    fn assert_each_required<T: Serialize + DeserializeOwned>(value: &T, fields: &[&str]) {
        let json = serde_json::to_value(value).unwrap();
        for field in fields {
            let mut without = json.clone();
            assert!(
                without.as_object_mut().unwrap().remove(*field).is_some(),
                "{field} is not a key"
            );
            let Err(error) = serde_json::from_value::<T>(without) else {
                panic!("{field} was filled in");
            };
            assert!(
                error
                    .to_string()
                    .contains(&format!("missing field `{field}`")),
                "{field}: {error}"
            );
        }
    }

    /// PX-42.
    #[test]
    fn connected_apps_and_the_audit_list() {
        assert_each_required(
            &ConnectedApp {
                id: "g".into(),
                client_name: "Claude".into(),
                return_host: "claude.ai".into(),
                scopes: vec!["library:read".into()],
                connected_at: 1,
                last_call_at: None,
                calls_today: 0,
                username: "alice".into(),
            },
            &["lastCallAt", "callsToday", "scopes", "returnHost"],
        );
        assert_each_required(
            &AuditEvent {
                id: 1,
                at: 1,
                actor: "root".into(),
                client: "Claude".into(),
                action: "disable_user".into(),
                target: None,
                outcome: "ok".into(),
            },
            &["target", "outcome", "client"],
        );
    }

    #[test]
    fn users_sessions_and_devices() {
        assert_each_required(
            &UserInfo {
                username: "alice".into(),
                display_name: None,
                role: Role::Consumer,
                created_at: 0,
                disabled: false,
                left_at: None,
                has_password: true,
                password_changed_at: None,
                password_must_change: true,
                password_expires_at: Some(1),
                last_seen_at: None,
                has_passkey: false,
                has_sso: false,
                has_pin: false,
                device_count: 0,
                session_count: 0,
                rate_limited_secs: None,
                portable: Vec::new(),
            },
            &[
                "displayName",
                "lastSeenAt",
                "portable",
                "passwordMustChange",
                "passwordExpiresAt",
            ],
        );
        assert_each_required(
            &PortableTie {
                id: "ab".repeat(32),
                fingerprint: "K7QX-3MPA-9TRD-WF2H".into(),
                tied_at: 1_791_504_000,
                last_used_at: None,
            },
            &["id", "fingerprint", "tiedAt", "lastUsedAt"],
        );
        assert_each_required(
            &NewUser {
                username: "alice".into(),
                display_name: None,
                password: "correct horse".into(),
                role: Role::Consumer,
            },
            &["displayName", "role"],
        );
        assert_each_required(
            &SessionEntry {
                id: "1".into(),
                device_name: None,
                created_at: 0,
                last_used_at: 0,
                current: false,
                endpoint_id: None,
                platform: None,
            },
            &["deviceName", "endpointId"],
        );
        assert_each_required(
            &DeviceEntry {
                endpoint_id: "ab".into(),
                name: None,
                platform: None,
                last_seen_at: 0,
                pin_required: false,
                trusted: false,
                auto_login: false,
            },
            &["name"],
        );
    }

    /// Invites (P6-01): what is left out is a choice, so it is said.
    #[test]
    fn invites() {
        assert_each_required(
            &NewInvite {
                kind: InviteKind::CreateUser,
                note: None,
                uses: None,
                expires: None,
                role: None,
            },
            &["kind", "note", "uses", "expires", "role"],
        );
        assert_each_required(
            &InviteEntry {
                id: "1".into(),
                kind: InviteKind::CreateUser,
                note: None,
                role: Role::Consumer,
                uses: 1,
                uses_left: 1,
                expires_at: 0,
                created_by: invite::MADE_BY_CLI.into(),
                created_at: 0,
            },
            &["note", "usesLeft", "expiresAt", "createdBy"],
        );
    }

    #[test]
    fn invite_kinds_are_named_as_the_ticket_names_them() {
        for (kind, name) in [
            (InviteKind::CreateUser, "create-user"),
            (InviteKind::PortableAccount, "portable-account"),
            (InviteKind::Federation, "federation"),
        ] {
            assert_eq!(serde_json::to_value(kind).unwrap(), name);
        }
    }

    /// The Server page's GPUs (PGPU-05): a server and a web UI that
    /// disagree about them fail on the call.
    #[test]
    fn gpu_cards_and_routes() {
        assert_each_required(
            &TranscodingStatus {
                video_encoders: Vec::new(),
                audio_encoders: Vec::new(),
                hardware_decoders: Vec::new(),
                gpu_filters: false,
                gpu_tonemap: false,
                gpu_overlay: false,
                gpu_tone_maps: Vec::new(),
                budget: None,
                gpus: Vec::new(),
                other_gpus: Vec::new(),
                gpu_notes: Vec::new(),
                routes: Vec::new(),
                picks: Vec::new(),
            },
            &["budget", "gpus", "otherGpus", "gpuNotes", "routes", "picks"],
        );
        assert_each_required(
            &CodecPick {
                device: PickDevice::HdrHevc,
                codec: Some("hevc".into()),
                hdr_kept: true,
            },
            &["codec", "hdrKept"],
        );
        let step = RouteStep {
            gpu: None,
            share: RouteShare::Cpu,
            decoder: VideoDecoder::Software,
            encoder: None,
            tone_map: FilterPlace::None,
            tone_mapper: None,
            burn_in: FilterPlace::None,
            cpu_why: Vec::new(),
        };
        assert_each_required(&step, &["gpu", "share", "encoder", "toneMapper", "cpuWhy"]);
        assert_each_required(
            &GpuRoute {
                job: RouteJob::Convert,
                source: "hevc".into(),
                target: "h264".into(),
                steps: vec![step],
                busy_after: false,
            },
            &["steps", "busyAfter"],
        );
        assert_each_required(
            &OtherGpuWhy::NothingWorks {
                api: GpuApi::Vaapi,
                entry: "transcode.gpus entry 1 (vaapi pick=all)".into(),
                test: None,
            },
            &["api", "entry", "test"],
        );
    }

    #[test]
    fn server_and_plex_status() {
        assert_each_required(
            &ServerStatus {
                name: "Spoolframe".into(),
                version: "0.5.0".into(),
                revision: None,
                endpoint_id: None,
                fingerprint: None,
                server_id: "abc".into(),
                transcoding: TranscodingStatus {
                    video_encoders: Vec::new(),
                    audio_encoders: Vec::new(),
                    hardware_decoders: Vec::new(),
                    gpu_filters: false,
                    gpu_tonemap: false,
                    gpu_overlay: false,
                    gpu_tone_maps: Vec::new(),
                    budget: None,
                    gpus: Vec::new(),
                    other_gpus: Vec::new(),
                    gpu_notes: Vec::new(),
                    routes: Vec::new(),
                    picks: Vec::new(),
                },
                mdns: false,
                addresses: Vec::new(),
                told_addresses: Vec::new(),
                wan: false,
                public_addresses: Vec::new(),
                public_notes: Vec::new(),
                wan_mapping: MappingStatus::NotUsed,
                wan_lookup: LookupStatus::NotUsed,
                ocr_languages: Vec::new(),
                web_urls: Vec::new(),
                preview_workers_default: DefaultWorkers {
                    count: 1,
                    counted: CoreCount::LogicalCpus,
                },
                protocol_version: 15,
                features: vec!["spoolframe:playlists".into()],
                pre_auth_features: Vec::new(),
                experimental_features: Vec::new(),
            },
            &[
                "endpointId",
                "fingerprint",
                "mdns",
                "addresses",
                "protocolVersion",
                "preAuthFeatures",
                "experimentalFeatures",
            ],
        );
        assert_each_required(
            &PlexStatus {
                reachable: false,
                error: None,
                server_name: None,
                version: None,
                url: None,
                libraries: Vec::new(),
                unsupported: Vec::new(),
            },
            &["error", "serverName", "libraries", "unsupported"],
        );
    }

    #[test]
    fn activity_logs_and_telemetry() {
        assert_each_required(
            &ActivityPoint::default(),
            &["artworkHitRate", "memoryBytes", "memory", "openFiles"],
        );
        assert_each_required(
            &PlexServerActivity {
                id: "plex".into(),
                name: "Plex".into(),
                requests: 0,
                errors: 0,
                mean_latency_ms: None,
            },
            &["meanLatencyMs"],
        );
        assert_each_required(&LogQuery::default(), &["after", "level", "text", "limit"]);
        let otlp = OtlpStatus {
            endpoint: "http://collector:4318".into(),
            protocol: "http".into(),
            traces: true,
            metrics: true,
            logs: true,
            header_names: Vec::new(),
            last_export_at: None,
            dropped: 0,
            dropping_since: None,
            last_error: None,
            last_error_at: None,
            metrics_every_secs: 60,
        };
        assert_each_required(
            &otlp,
            &["lastExportAt", "droppingSince", "lastError", "lastErrorAt"],
        );
        assert_each_required(
            &TelemetryStatus {
                otlp: None,
                resource: Vec::new(),
                metrics: MetricsEndpointStatus {
                    enabled: false,
                    path: "/metrics".into(),
                    refused: Vec::new(),
                },
            },
            &["otlp"],
        );
    }
}
