//! What the pages decide, apart from the pages: host-compiled, so
//! `cargo test` covers it without a browser.

use serde::Deserialize;
use serde_json::Value;
use spoolframe_admin_api::{
    PasskeyEntry, Platform, Role, SettingEntry, SettingLayer, UserInfo, error_code,
};

/// Where a browser opening the web UI belongs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Landing {
    /// A server with no users: register the first admin.
    Setup,
    Login,
    /// An admin's pages, from the Overview (P3-02): what the server is
    /// doing is what an admin opens the web UI for.
    Admin,
    /// Everyone else's: their own account.
    Profile,
}

impl Landing {
    /// Its page: where the start sends the browser, and where the logo
    /// goes once signed in.
    pub fn path(self) -> &'static str {
        match self {
            Landing::Setup => "/setup",
            Landing::Login => "/login",
            Landing::Admin => "/admin",
            Landing::Profile => "/profile",
        }
    }
}

/// From whether setup is open and who the cookie signs in (`None`: no one).
/// The role is read at each load, so a demoted admin lands on their
/// profile next time.
pub fn landing(setup_open: bool, signed_in: Option<Role>) -> Landing {
    match signed_in {
        Some(Role::Admin) => Landing::Admin,
        Some(_) => Landing::Profile,
        None if setup_open => Landing::Setup,
        None => Landing::Login,
    }
}

/// An error as the server answers it: a JSON-RPC error object, from the
/// socket or as an `/auth` response body alike.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ApiError {
    pub code: i32,
    pub message: String,
    /// Kept optional (PX-15 left it): JSON-RPC 2.0 lets an error leave
    /// `data` out, and jsonrpsee does when there is none.
    #[serde(default)]
    pub data: Option<Value>,
}

impl ApiError {
    /// One that never reached the server, or came back unreadable.
    pub fn transport(message: impl Into<String>) -> Self {
        Self {
            code: 0,
            message: message.into(),
            data: None,
        }
    }

    /// What to tell the person.
    pub fn explain(&self) -> String {
        match self.code {
            error_code::INVALID_CREDENTIALS => "Wrong username or password.".into(),
            error_code::RATE_LIMITED => {
                let ms = self
                    .data
                    .as_ref()
                    .and_then(|d| d.get("retryAfterMs"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                format!(
                    "Too many failed tries. Try again in {}.",
                    minutes(ms.div_ceil(60_000))
                )
            }
            error_code::INVALID_SETUP_CODE => {
                "That is not the setup code. It is in the server's log.".into()
            }
            error_code::ALREADY_SET_UP => "This server is already set up: sign in.".into(),
            error_code::UNAUTHENTICATED => "You are signed out. Sign in again.".into(),
            error_code::PASSKEY_REFUSED => {
                "That passkey did not sign you in. Try again, or use your password.".into()
            }
            error_code::RECENT_SIGN_IN_REQUIRED => "Confirm it is you first.".into(),
            error_code::NO_SUCH_SIGN_IN_CODE => {
                "No device is showing that code now. A code lasts 5 minutes: start again on the device."
                    .into()
            }
            error_code::FORBIDDEN => "Only an admin can do that.".into(),
            0 => format!("The server cannot be reached ({}).", self.message),
            _ => self.message.clone(),
        }
    }
}

/// What the users page says when the server refuses a change to
/// `username`: by the refusal's code where the page knows better words
/// than the server's.
pub fn user_change_error(e: &ApiError, username: &str) -> String {
    match e.code {
        error_code::LAST_ADMIN => {
            format!("{username} is the last admin. Make someone else an admin first.")
        }
        error_code::USER_EXISTS => format!("The username {username} is taken."),
        _ => e.explain(),
    }
}

/// Where the add-user form shows a refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddUserError {
    /// Under the username field: the name is not one a user can have.
    Username(String),
    /// Under the form.
    Form(String),
}

/// A refusal of adding `username`, as the form shows it.
pub fn add_user_error(e: &ApiError, username: &str) -> AddUserError {
    // INVALID_INPUT is also a password that is too short; the server's
    // message says which.
    if e.code == error_code::INVALID_INPUT && e.message.starts_with("invalid username") {
        return AddUserError::Username("Use 1–64 letters, digits, “.”, “_” or “-”.".into());
    }
    AddUserError::Form(user_change_error(e, username))
}

/// Whether My profile offers "Remove password": there is one, and a
/// passkey or a linked sign-in provider signs the user in without it.
pub fn can_remove_password(has_password: bool, passkeys: bool, links: bool) -> bool {
    has_password && (passkeys || links)
}

/// A refusal of removing a password: the user's own (`whose` `None`), or
/// `whose`'s, by an admin.
pub fn password_remove_error(e: &ApiError, whose: Option<&str>) -> String {
    match (e.code, whose) {
        (error_code::LAST_ADMIN, None) => {
            "You're the last admin with a password. Give another admin a password first.".into()
        }
        (error_code::LAST_ADMIN, Some(username)) => format!(
            "{username} is the last admin with a password. Give another admin a password first."
        ),
        (error_code::LAST_SIGN_IN_METHOD, None) => {
            "Your password is your only way to sign in. Add a passkey or link a sign-in provider first."
                .into()
        }
        (error_code::LAST_SIGN_IN_METHOD, Some(username)) => format!(
            "The password is {username}'s only way to sign in. They need a passkey or a linked sign-in provider first."
        ),
        _ => e.explain(),
    }
}

/// What went wrong in the browser's own passkey prompt, from the name of
/// the `DOMException` it threw. `adding`: making a passkey, not using one.
pub fn passkey_prompt_error(name: &str, adding: bool) -> String {
    match (name, adding) {
        // The browser gives one name for all of these, on purpose.
        ("NotAllowedError", _) => "Cancelled, or it took too long. Nothing was changed.".into(),
        ("InvalidStateError", true) => "That device already has a passkey for this account.".into(),
        ("SecurityError", _) => "This page's address is not the one passkeys are bound to.".into(),
        ("NotSupportedError", _) => "That device cannot hold this kind of passkey.".into(),
        ("AbortError", _) => "Cancelled.".into(),
        _ => format!("The browser's passkey prompt failed ({name})."),
    }
}

/// Why the setup page cannot make the first admin a passkey, if it cannot:
/// then it sets a password only. `secure`: the page is in a secure
/// context; `webauthn`: the browser can make passkeys there; `server`: the
/// server's own reason, if it has none.
pub fn setup_without_passkeys(
    secure: bool,
    webauthn: bool,
    server: Option<&str>,
) -> Option<String> {
    let why = if !secure {
        "Passkeys need HTTPS or localhost".to_owned()
    } else if !webauthn {
        "This browser cannot make passkeys".to_owned()
    } else {
        format!("This server cannot make passkeys: {}", server?)
    };
    Some(format!(
        "{why}, so the admin gets a password. Add a passkey later in My profile."
    ))
}

/// What a passkey is called in a list: its name, or, for one made in a
/// Spoolframe app (P3-19), "Spoolframe app · <name>", its name being the
/// device's.
pub fn passkey_title(passkey: &PasskeyEntry) -> String {
    if passkey.app {
        format!("Spoolframe app · {}", passkey.name)
    } else {
        passkey.name.clone()
    }
}

/// Why a passkey that does not sign in does not, for its row: `server` is
/// "this server's" in My profile, "the server's" for an admin. An app
/// passkey made for the relying party every server once shared (PX-14) is
/// old, and is added again in the app; a web UI's was made for another
/// address. `None` for one that signs in.
pub fn unusable_note(passkey: &PasskeyEntry, server: &str) -> Option<String> {
    if passkey.usable {
        None
    } else if passkey.app {
        Some(
            "An old app passkey: it signs in nowhere now. Remove it, and add it again in the app."
                .to_owned(),
        )
    } else {
        Some(format!(
            "Made for {}: it does not work at {server} address now.",
            passkey.rp_id
        ))
    }
}

/// Whether a page at host `page` can make and use passkeys bound to
/// `rp_id`: the browser takes its own host, or a parent domain of it.
pub fn page_serves(page: &str, rp_id: &str) -> bool {
    page == rp_id || page.ends_with(&format!(".{rp_id}"))
}

/// Whether a page at `protocol` (`location.protocol`, with its colon) and
/// `hostname` (`location.hostname`) is plain HTTP across the network
/// (PX-23): what is typed and sent there, a password and the session
/// cookie among it, can be read on the way. HTTPS (a proxy's) is not, nor
/// is loopback (an SSH tunnel's, or the server's own browser): `localhost`,
/// `*.localhost`, 127.0.0.0/8 and `::1`. The same rule the server's
/// `drops_secure_cookies` applies to the page's `Origin`.
pub fn plain_http_across_network(protocol: &str, hostname: &str) -> bool {
    if protocol != "http:" {
        return false;
    }
    // A browser gives an IPv6 host in its brackets.
    let bare = hostname
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(hostname);
    match bare.parse::<std::net::IpAddr>() {
        Ok(ip) => !ip.is_loopback(),
        Err(_) => {
            let name = hostname.to_ascii_lowercase();
            !name.is_empty() && name != "localhost" && !name.ends_with(".localhost")
        }
    }
}

/// A device sign-in code as it is shown, `BCDF-GHJK`, however it was typed
/// or linked; unchanged if it is not eight characters.
pub fn display_code(code: &str) -> String {
    let letters: String = code
        .chars()
        .filter(|c| !matches!(c, '-' | ' '))
        .map(|c| c.to_ascii_uppercase())
        .collect();
    match letters.split_at_checked(4) {
        Some((a, b)) if letters.len() == 8 => format!("{a}-{b}"),
        _ => code.to_owned(),
    }
}

const BASE64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// `bytes` as base64url without padding: how WebAuthn's JSON carries its
/// binary fields.
pub fn base64url(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .fold(0u32, |n, byte| (n << 8) | u32::from(*byte))
            << (8 * (3 - chunk.len()));
        for i in 0..=chunk.len() {
            text.push(char::from(BASE64URL[(n >> (18 - 6 * i)) as usize & 63]));
        }
    }
    text
}

/// The bytes of base64url `text`, padded or not; `None` if it is not
/// base64url.
pub fn from_base64url(text: &str) -> Option<Vec<u8>> {
    let text = text.trim_end_matches('=');
    if text.len() % 4 == 1 {
        return None;
    }
    let mut bytes = Vec::with_capacity(text.len() * 3 / 4);
    for chunk in text.as_bytes().chunks(4) {
        let mut n = 0u32;
        for letter in chunk {
            let value = BASE64URL.iter().position(|l| l == letter)?;
            n = (n << 6) | value as u32;
        }
        n <<= 6 * (4 - chunk.len());
        for i in 0..chunk.len() - 1 {
            bytes.push((n >> (16 - 8 * i)) as u8);
        }
    }
    Some(bytes)
}

/// Whether `username` is the one enabled admin, whom the server will not
/// demote or disable: the page says so before anyone tries.
pub fn last_admin(users: &[UserInfo], username: &str) -> bool {
    let mut admins = users
        .iter()
        .filter(|u| u.role == Role::Admin && !u.disabled);
    matches!(
        (admins.next(), admins.next()),
        (Some(only), None) if only.username == username
    )
}

/// `at` (Unix seconds) as a person reads it, seen at `now`: recent times
/// relative, older ones as a date.
pub fn ago(now: i64, at: i64) -> String {
    let secs = (now - at).max(0);
    match secs {
        0..60 => "just now".into(),
        60..3600 => plural(secs / 60, "minute"),
        3600..86_400 => plural(secs / 3600, "hour"),
        86_400..2_592_000 => plural(secs / 86_400, "day"),
        _ => date(at),
    }
}

fn plural(n: i64, unit: &str) -> String {
    if n == 1 {
        format!("1 {unit} ago")
    } else {
        format!("{n} {unit}s ago")
    }
}

/// `YYYY-MM-DD` (UTC) of Unix seconds `at`.
pub fn date(at: i64) -> String {
    // Howard Hinnant's civil-from-days.
    let z = at.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// How `user` can sign in, as the users page's column says it: the
/// password, a passkey, a sign-in provider (P3-04).
pub fn sign_in_methods(user: &UserInfo) -> String {
    let ways: Vec<&str> = [
        (user.has_password, "Password"),
        (user.has_passkey, "passkey"),
        (user.has_sso, "provider"),
    ]
    .into_iter()
    .filter_map(|(has, way)| has.then_some(way))
    .collect();
    match ways.split_first() {
        None => "None".into(),
        Some((first, rest)) => {
            let mut text = format!("{}{}", first[..1].to_uppercase(), &first[1..]);
            for way in rest {
                text.push_str(", ");
                text.push_str(way);
            }
            text
        }
    }
}

/// `n` of `unit`, its plural an "s" more: "1 session", "4 sessions".
pub fn count(n: u64, unit: &str) -> String {
    if n == 1 {
        format!("1 {unit}")
    } else {
        format!("{n} {unit}s")
    }
}

/// The avatar's class (WebUsers, WebAdminPhone): an admin is teal; anyone
/// else takes a tone by the letter the avatar shows, so the same letter is
/// the same colour on every page and every load ("A" mauve, "S" peach, as
/// the boards draw Alice and Steve). Grey while disabled.
pub fn avatar_class(user: &UserInfo) -> &'static str {
    // Teal is the admin's alone.
    const TONES: [&str; 5] = [
        "avatar avatar-peach",
        "avatar avatar-blue",
        "avatar avatar-mauve",
        "avatar avatar-pink",
        "avatar avatar-green",
    ];
    if user.disabled {
        return "avatar avatar-off";
    }
    if user.role == Role::Admin {
        return "avatar avatar-teal";
    }
    let shown = user.display_name.as_deref().unwrap_or(&user.username);
    let letter = shown
        .chars()
        .next()
        .map_or(0, |c| c.to_lowercase().next().map_or(0, u32::from));
    TONES[letter as usize % TONES.len()]
}

/// The users as the list shows them (WebUsers): admins first, then
/// everyone else, each in the server's order.
pub fn users_in_order(mut users: Vec<UserInfo>) -> Vec<UserInfo> {
    users.sort_by_key(|u| u.role != Role::Admin);
    users
}

/// One of a user's devices as the users page's panel shows it, with who
/// else has signed in on it (WebUsers' "also: Alice, Steve").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedDevice {
    pub device: spoolframe_admin_api::DeviceEntry,
    /// The other users on the same device key, by the names the page shows.
    pub also: Vec<String>,
}

/// `whose` devices, most recently seen first, each with the other users
/// in `everyone` (name, devices) who signed in on the same key.
pub fn shared_devices(
    whose: &str,
    everyone: &[(String, Vec<spoolframe_admin_api::DeviceEntry>)],
) -> Vec<SharedDevice> {
    let Some((_, own)) = everyone.iter().find(|(name, _)| name == whose) else {
        return Vec::new();
    };
    let mut shared: Vec<SharedDevice> = own
        .iter()
        .map(|device| SharedDevice {
            also: everyone
                .iter()
                .filter(|(name, devices)| {
                    name != whose && devices.iter().any(|d| d.endpoint_id == device.endpoint_id)
                })
                .map(|(name, _)| name.clone())
                .collect(),
            device: device.clone(),
        })
        .collect();
    shared.sort_by_key(|s| std::cmp::Reverse(s.device.last_seen_at));
    shared
}

/// A device key as the panel shows its start: "dc97 3189 4e65 8276", in
/// groups so it can be read out and compared.
pub fn key_start(endpoint_id: &str) -> String {
    endpoint_id
        .chars()
        .take(16)
        .collect::<Vec<_>>()
        .chunks(4)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join(" ")
}

/// One library's media paths, as the Server page's row says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathState {
    /// Every file is readable.
    Ok { files: u64 },
    /// Some file's Plex path matched no mapping: `plex_path` is one.
    NoMapping { plex_path: String },
    /// Files a mapping matched cannot be read here: `count`, and where one
    /// of them was looked for.
    Missing { count: u64, local_path: String },
}

/// What `report` says, the worst first: a missing mapping is fixed in the
/// config, and fixing it may also account for missing files.
pub fn path_state(report: &spoolframe_admin_api::PathReport) -> PathState {
    if report.no_mapping.count > 0 {
        let plex_path = report
            .no_mapping
            .examples
            .first()
            .map(|e| example_paths(e).0)
            .unwrap_or_default();
        return PathState::NoMapping { plex_path };
    }
    if report.unreadable.count > 0 {
        let local_path = report
            .unreadable
            .examples
            .first()
            .map(|e| example_paths(e).1)
            .unwrap_or_default();
        return PathState::Missing {
            count: report.unreadable.count,
            local_path,
        };
    }
    PathState::Ok { files: report.ok }
}

/// The Plex path and the local one of a path check's example,
/// `"<plex path> -> <local path>: <reason>"`. The reason has no colon
/// space of its own, a path may.
fn example_paths(example: &str) -> (String, String) {
    match example.split_once(" -> ") {
        Some((plex, rest)) => {
            let local = rest.rsplit_once(": ").map_or(rest, |(local, _)| local);
            (plex.to_owned(), local.to_owned())
        }
        None => (example.to_owned(), example.to_owned()),
    }
}

/// Where a path check's example was looked for here: its local path.
pub fn missing_path(example: &str) -> String {
    example_paths(example).1
}

/// `n` with its thousands apart, as the boards write counts: "18,930".
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// A list typed in one field, its entries split at commas and spaces:
/// "127.0.0.1, 10.0.0.0/8" for a list setting such as
/// `admin.trusted_proxies`.
pub fn entries(typed: &str) -> Vec<String> {
    typed
        .split([',', ' ', '\t', '\n'])
        .filter(|e| !e.is_empty())
        .map(str::to_owned)
        .collect()
}

/// `list` with `typed` added at its end, trimmed (PX-22's list fields);
/// `Err` says why not: nothing typed, or listed already.
pub fn add_entry(list: &[String], typed: &str) -> Result<Vec<String>, String> {
    let typed = typed.trim();
    if typed.is_empty() {
        return Err("Type something to add.".into());
    }
    if list.iter().any(|e| e == typed) {
        return Err(format!("{typed} is listed already."));
    }
    let mut out = list.to_vec();
    out.push(typed.to_owned());
    Ok(out)
}

/// A list setting's value at Save, as `save` takes it: `None` takes the
/// runtime value out. An emptied list is `None` when that leaves nothing
/// under it, or an empty list, but an empty list when the file or the
/// defaults would bring entries back.
pub fn list_setting(items: Vec<Value>, current: Option<&SettingEntry>) -> Option<Value> {
    if !items.is_empty() {
        return Some(Value::Array(items));
    }
    let under = match current {
        None => return None,
        Some(e) if e.layer == Some(SettingLayer::Runtime) => e.overrides.as_ref(),
        Some(e) => e.value.as_ref(),
    };
    match under {
        Some(Value::Array(under)) if !under.is_empty() => Some(Value::Array(Vec::new())),
        _ => None,
    }
}

/// A list setting's strings, for [`list_setting`].
pub fn string_values(items: &[String]) -> Vec<Value> {
    items.iter().cloned().map(Value::String).collect()
}

/// A Plex URL as the Server page shows it: its host and port.
pub fn host_port(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.split(['/', '?']).next().unwrap_or(rest)
}

/// Spoolframe's build as the Server page shows it (P3-18): `0.5.0 ·
/// 9f3c2a1`, or the version alone when the build named no revision.
pub fn server_version(version: &str, revision: Option<&str>) -> String {
    match revision {
        Some(rev) => format!("{version} · {rev}"),
        None => version.to_owned(),
    }
}

/// The Server page's "Passkey domain" (PX-14): the relying party the apps
/// make this server's passkeys for, `<server id>.spoolframe.app`, as a
/// password manager lists them.
pub fn passkey_domain(server_id: &str) -> String {
    format!("{server_id}.{}", spoolframe_admin_api::APP_RP_DOMAIN)
}

/// Plex's version to its minor, as the boards show it: `1.41.0.8992-…`
/// is 1.41.
pub fn plex_version(v: &str) -> String {
    v.split('.').take(2).collect::<Vec<_>>().join(".")
}

/// The Plex card's line: "Plex Media Server 1.41 · <host:port> · N
/// libraries", without what is not known.
pub fn plex_line(version: Option<&str>, url: Option<&str>, libraries: usize) -> String {
    let mut parts = vec![match version {
        Some(v) => format!("Plex Media Server {}", plex_version(v)),
        None => "Plex Media Server".to_owned(),
    }];
    parts.extend(url.map(|u| host_port(u).to_owned()));
    parts.push(match libraries {
        1 => "1 library".to_owned(),
        n => format!("{n} libraries"),
    });
    parts.join(" · ")
}

fn minutes(n: u64) -> String {
    match n {
        0 | 1 => "a minute".into(),
        n => format!("{n} minutes"),
    }
}

/// A user's status on the Users list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserStatus {
    Disabled,
    /// Their password sign-ins are refused for a while (P3-18): why, and
    /// for how long.
    Limited(String),
    Active,
}

pub fn user_status(user: &UserInfo) -> UserStatus {
    match (user.disabled, user.rate_limited_secs) {
        (true, _) => UserStatus::Disabled,
        (false, Some(secs)) => UserStatus::Limited(format!(
            "Too many wrong passwords. Password sign-ins as {} are refused for {}.",
            user.username,
            minutes(secs.div_ceil(60))
        )),
        (false, None) => UserStatus::Active,
    }
}

/// What kind of client a session or device is, for its "Kind" and its
/// "Linux app · 2 hours ago": the platform its app said, "App" for one
/// that has not said since the server began keeping it, and "Web" or
/// "Command-line client" for a session with no device (the admin API's).
pub fn client_kind(endpoint_id: Option<&str>, platform: Option<Platform>) -> &'static str {
    match (endpoint_id, platform) {
        // The admin API's: the web UI or the CLI, by what signed it in
        // (P3-18), unless it is from before the server kept which.
        (None, Some(platform)) => platform.label(),
        (None, None) => "Web or CLI",
        (Some(_), Some(platform)) => platform.label(),
        (Some(_), None) => "App",
    }
}

/// A browser's name from its `User-Agent`, as My profile names a browser
/// session (WebProfile's "Firefox on Linux"): the browser, then the
/// system it runs on when the string says. `None` for one it does not
/// know.
pub fn browser_name(user_agent: &str) -> Option<String> {
    let has = |part: &str| user_agent.contains(part);
    // Most browsers say "Safari" and "Chrome" too: the specific first.
    let browser = if has("Edg/") || has("EdgA/") || has("EdgiOS/") {
        "Edge"
    } else if has("OPR/") || has("Opera") {
        "Opera"
    } else if has("Firefox/") || has("FxiOS/") {
        "Firefox"
    } else if has("Chrome/") || has("CriOS/") || has("Chromium/") {
        "Chrome"
    } else if has("Safari/") {
        "Safari"
    } else {
        return None;
    };
    let system = if has("iPhone") {
        Some("iPhone")
    } else if has("iPad") {
        Some("iPad")
    } else if has("Android") {
        Some("Android")
    } else if has("CrOS") {
        Some("ChromeOS")
    } else if has("Windows") {
        Some("Windows")
    } else if has("Macintosh") || has("Mac OS X") {
        Some("macOS")
    } else if has("Linux") || has("X11") {
        Some("Linux")
    } else {
        None
    };
    Some(match system {
        Some(system) => format!("{browser} on {system}"),
        None => browser.to_owned(),
    })
}

/// What kind of client a session is, as the pages' second line says it:
/// a browser is a "Web browser", the CLI a "Command line", and a device's
/// session its app.
pub fn session_kind(endpoint_id: Option<&str>, platform: Option<Platform>) -> &'static str {
    match (endpoint_id, platform) {
        (None, Some(Platform::Web)) => "Web browser",
        (None, Some(Platform::Cli)) => "Command line",
        (None, None) => "Web browser or command line",
        _ => client_kind(endpoint_id, platform),
    }
}

/// What every page calls a session (My profile, an admin's user page and
/// the users list alike): its device's name; for this browser's own, the
/// browser as `user_agent` names it; for another browser, "Web browser".
pub fn session_name(
    endpoint_id: Option<&str>,
    device_name: Option<&str>,
    platform: Option<Platform>,
    current: bool,
    user_agent: Option<&str>,
) -> String {
    match (endpoint_id, device_name) {
        (Some(_), Some(name)) => name.to_owned(),
        (Some(_), None) => "A device with no name".to_owned(),
        (None, _) if current && platform == Some(Platform::Web) => user_agent
            .and_then(browser_name)
            .unwrap_or_else(|| "Web browser".to_owned()),
        (None, _) => session_kind(None, platform).to_owned(),
    }
}

/// An address as the Server page breaks it over lines: the part that may
/// wrap, and the part that may not. An IPv6 address wraps only between its
/// groups, and never inside its last group and port, so the port never
/// stands alone on a line; anything else does not wrap at all.
pub fn address_parts(address: &str) -> (&str, &str) {
    let Some(close) = address.rfind("]:") else {
        return ("", address);
    };
    match address[..close].rfind(':') {
        Some(colon) => address.split_at(colon + 1),
        None => ("", address),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_typed_list_splits_at_commas_and_spaces() {
        assert_eq!(
            entries(" 127.0.0.1,10.0.0.0/8  fd00::/8 ,"),
            ["127.0.0.1", "10.0.0.0/8", "fd00::/8"]
        );
        assert!(entries(" , ").is_empty());
    }

    #[test]
    fn an_entry_is_added_trimmed_at_the_end() {
        let list = vec!["eth0".to_owned()];
        assert_eq!(add_entry(&list, "  enp* ").unwrap(), ["eth0", "enp*"]);
    }

    #[test]
    fn an_entry_listed_already_is_refused() {
        let list = vec!["eth0".to_owned()];
        assert_eq!(
            add_entry(&list, "eth0").unwrap_err(),
            "eth0 is listed already."
        );
    }

    #[test]
    fn nothing_typed_is_not_an_entry() {
        assert!(add_entry(&[], "  ").is_err());
    }

    fn list_entry(
        layer: Option<SettingLayer>,
        value: Option<Value>,
        overrides: Option<Value>,
    ) -> SettingEntry {
        SettingEntry {
            key: "network.relays".into(),
            set: value.is_some(),
            value,
            secret: false,
            layer,
            locked: false,
            overrides,
            apply: spoolframe_admin_api::SettingApply::Restart,
            restart_pending: false,
            running: None,
        }
    }

    #[test]
    fn a_list_with_entries_is_saved_as_a_json_array() {
        assert_eq!(
            list_setting(string_values(&["default".into()]), None),
            Some(json!(["default"]))
        );
    }

    #[test]
    fn an_emptied_runtime_list_is_taken_out_when_nothing_is_under_it() {
        let e = list_entry(Some(SettingLayer::Runtime), Some(json!(["a"])), None);
        assert_eq!(list_setting(Vec::new(), Some(&e)), None);
    }

    #[test]
    fn an_emptied_list_stays_empty_over_the_files_entries() {
        let e = list_entry(
            Some(SettingLayer::Runtime),
            Some(json!(["a"])),
            Some(json!(["default"])),
        );
        assert_eq!(list_setting(Vec::new(), Some(&e)), Some(json!([])));
        let file = list_entry(Some(SettingLayer::File), Some(json!(["default"])), None);
        assert_eq!(list_setting(Vec::new(), Some(&file)), Some(json!([])));
    }

    #[test]
    fn an_empty_list_over_an_empty_default_is_left_alone() {
        let e = list_entry(Some(SettingLayer::Default), Some(json!([])), None);
        assert_eq!(list_setting(Vec::new(), Some(&e)), None);
        assert_eq!(list_setting(Vec::new(), None), None);
    }

    #[test]
    fn an_ipv4_address_does_not_wrap() {
        assert_eq!(
            address_parts("192.168.1.20:7890"),
            ("", "192.168.1.20:7890")
        );
    }

    #[test]
    fn an_ipv6_address_keeps_its_last_group_with_the_port() {
        assert_eq!(
            address_parts("[2603:8000:c33e:6ac6:a53:7cfe:42df:c5cf]:17530"),
            ("[2603:8000:c33e:6ac6:a53:7cfe:42df:", "c5cf]:17530")
        );
    }

    #[test]
    fn a_short_ipv6_address_keeps_its_last_group_with_the_port() {
        assert_eq!(address_parts("[fd00::20]:7890"), ("[fd00::", "20]:7890"));
    }

    const FIREFOX_LINUX: &str =
        "Mozilla/5.0 (X11; Linux x86_64; rv:131.0) Gecko/20100101 Firefox/131.0";
    const CHROME_MAC: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.0.0 Safari/537.36";
    const SAFARI_IPHONE: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1";
    const EDGE_WINDOWS: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.0.0 Safari/537.36 Edg/129.0.0.0";
    const CHROME_ANDROID: &str = "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.0.0 Mobile Safari/537.36";

    #[test]
    fn firefox_on_linux_is_named_as_the_board_names_it() {
        assert_eq!(
            browser_name(FIREFOX_LINUX).as_deref(),
            Some("Firefox on Linux")
        );
    }

    #[test]
    fn chrome_is_told_from_the_safari_it_also_claims() {
        assert_eq!(browser_name(CHROME_MAC).as_deref(), Some("Chrome on macOS"));
    }

    #[test]
    fn safari_on_an_iphone_is_not_called_macos() {
        assert_eq!(
            browser_name(SAFARI_IPHONE).as_deref(),
            Some("Safari on iPhone")
        );
    }

    #[test]
    fn edge_is_told_from_the_chrome_it_also_claims() {
        assert_eq!(
            browser_name(EDGE_WINDOWS).as_deref(),
            Some("Edge on Windows")
        );
    }

    #[test]
    fn android_is_told_from_the_linux_it_also_claims() {
        assert_eq!(
            browser_name(CHROME_ANDROID).as_deref(),
            Some("Chrome on Android")
        );
    }

    #[test]
    fn an_unknown_browser_has_no_name() {
        assert_eq!(browser_name("curl/8.9.1"), None);
    }

    #[test]
    fn this_browser_s_session_is_named_from_its_user_agent() {
        assert_eq!(
            session_name(None, None, Some(Platform::Web), true, Some(FIREFOX_LINUX)),
            "Firefox on Linux"
        );
    }

    #[test]
    fn another_browser_s_session_is_a_web_browser() {
        assert_eq!(
            session_name(None, None, Some(Platform::Web), false, Some(FIREFOX_LINUX)),
            "Web browser"
        );
    }

    #[test]
    fn this_browser_without_a_known_user_agent_is_a_web_browser() {
        assert_eq!(
            session_name(None, None, Some(Platform::Web), true, Some("curl/8.9.1")),
            "Web browser"
        );
    }

    #[test]
    fn a_cli_session_is_the_command_line() {
        assert_eq!(
            session_name(None, None, Some(Platform::Cli), false, None),
            "Command line"
        );
    }

    #[test]
    fn a_device_session_is_its_device_s_name() {
        assert_eq!(
            session_name(
                Some("aa11"),
                Some("Living room TV"),
                Some(Platform::Tvos),
                true,
                Some(FIREFOX_LINUX)
            ),
            "Living room TV"
        );
    }

    #[test]
    fn a_browser_session_s_kind_is_web_browser() {
        assert_eq!(session_kind(None, Some(Platform::Web)), "Web browser");
    }

    #[test]
    fn the_server_s_version_names_its_revision_when_the_build_had_one() {
        assert_eq!(server_version("0.5.0", Some("9f3c2a1")), "0.5.0 · 9f3c2a1");
        assert_eq!(server_version("0.5.0", None), "0.5.0");
    }

    #[test]
    fn the_passkey_domain_is_the_server_id_under_spoolframe_app() {
        assert_eq!(
            passkey_domain("mzxw6ytbojtg633c"),
            "mzxw6ytbojtg633c.spoolframe.app"
        );
    }

    #[test]
    fn the_plex_line_says_its_version_to_the_minor() {
        assert_eq!(plex_version("1.41.0.8992-f1a2b3c4d"), "1.41");
        assert_eq!(
            plex_line(
                Some("1.41.3.9314-a0bfb8370"),
                Some("http://192.168.1.10:32400/"),
                4
            ),
            "Plex Media Server 1.41 · 192.168.1.10:32400 · 4 libraries"
        );
        assert_eq!(plex_line(None, None, 1), "Plex Media Server · 1 library");
    }
    use spoolframe_admin_api::{PathFailures, PathReport};

    fn passkey(name: &str, rp_id: &str, app: bool) -> PasskeyEntry {
        PasskeyEntry {
            id: "id".into(),
            name: name.into(),
            created_at: 0,
            last_used_at: None,
            rp_id: rp_id.into(),
            usable: true,
            app,
        }
    }

    #[test]
    fn an_old_app_passkey_is_to_be_added_again_and_a_web_one_names_its_host() {
        let usable = passkey("Alice's iPhone", "mzxw6ytbojtg633c.spoolframe.app", true);
        assert_eq!(unusable_note(&usable, "this server's"), None);
        let old = PasskeyEntry {
            usable: false,
            ..passkey("Alice's iPhone", "spoolframe.app", true)
        };
        assert_eq!(
            unusable_note(&old, "this server's").as_deref(),
            Some(
                "An old app passkey: it signs in nowhere now. Remove it, and add it again in the app."
            )
        );
        let moved = PasskeyEntry {
            usable: false,
            ..passkey("MacBook", "old.example.org", false)
        };
        assert_eq!(
            unusable_note(&moved, "the server's").as_deref(),
            Some("Made for old.example.org: it does not work at the server's address now.")
        );
    }

    /// An app passkey (P3-19) is named for the device it was made on; the
    /// row says it is the app's.
    #[test]
    fn an_app_passkey_is_shown_as_the_app_s() {
        assert_eq!(
            passkey_title(&passkey("Alice's iPhone", "spoolframe.app", true)),
            "Spoolframe app · Alice's iPhone"
        );
        assert_eq!(
            passkey_title(&passkey("MacBook", "media.example.org", false)),
            "MacBook"
        );
    }

    #[test]
    fn a_session_or_device_is_named_by_the_kind_of_client_it_runs() {
        assert_eq!(
            client_kind(Some("ab"), Some(Platform::Tvos)),
            "Apple TV app"
        );
        assert_eq!(client_kind(Some("ab"), Some(Platform::Linux)), "Linux app");
        // Not said since the server began keeping it.
        assert_eq!(client_kind(Some("ab"), None), "App");
        assert_eq!(client_kind(None, None), "Web or CLI");
    }

    #[test]
    fn each_role_lands_on_its_own_pages() {
        assert_eq!(landing(false, Some(Role::Admin)), Landing::Admin);
        assert_eq!(landing(false, Some(Role::Consumer)), Landing::Profile);
        // A role this build does not know gets the least.
        assert_eq!(landing(false, Some(Role::Unknown)), Landing::Profile);
    }

    #[test]
    fn an_admin_starts_on_the_overview_and_everyone_else_on_their_profile() {
        assert_eq!(landing(false, Some(Role::Admin)).path(), "/admin");
        assert_eq!(landing(false, Some(Role::Consumer)).path(), "/profile");
    }

    #[test]
    fn signed_out_is_setup_only_while_it_is_open() {
        assert_eq!(landing(true, None), Landing::Setup);
        assert_eq!(landing(false, None), Landing::Login);
        // Someone signed in never sees setup.
        assert_eq!(landing(true, Some(Role::Admin)), Landing::Admin);
    }

    #[test]
    fn a_rate_limit_says_how_long_to_wait() {
        let e: ApiError = serde_json::from_value(json!({
            "code": error_code::RATE_LIMITED,
            "message": "too many failed logins",
            "data": { "retryAfterMs": 600_000 }
        }))
        .unwrap();
        assert_eq!(
            e.explain(),
            "Too many failed tries. Try again in 10 minutes."
        );
        let soon = ApiError {
            data: Some(json!({ "retryAfterMs": 5_000 })),
            ..e
        };
        assert_eq!(
            soon.explain(),
            "Too many failed tries. Try again in a minute."
        );
    }

    #[test]
    fn base64url_is_rfc_4648_s_without_padding() {
        // RFC 4648 section 10, in the URL-safe alphabet.
        for (bytes, text) in [
            (&b""[..], ""),
            (b"f", "Zg"),
            (b"fo", "Zm8"),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg"),
            (b"fooba", "Zm9vYmE"),
            (b"foobar", "Zm9vYmFy"),
            (&[0xfb, 0xff, 0xfe], "-__-"),
        ] {
            assert_eq!(base64url(bytes), text);
            assert_eq!(from_base64url(text).as_deref(), Some(bytes), "{text}");
        }
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(from_base64url(&base64url(&all)), Some(all));
    }

    #[test]
    fn base64url_is_read_padded_and_refused_when_it_is_not_base64url() {
        assert_eq!(from_base64url("Zg==").as_deref(), Some(&b"f"[..]));
        // The standard alphabet's letters, a space, a length no bytes have.
        for bad in ["Zm9v+A", "Zm9v/A", "Zm 9v", "Zm9vY"] {
            assert_eq!(from_base64url(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_plain_http_page_on_a_network_address_is_across_the_network() {
        for (protocol, hostname) in [
            ("http:", "192.168.1.10"),
            ("http:", "media.example.org"),
            ("http:", "nas"),
            ("http:", "[fd00::10]"),
            ("http:", "10.0.0.2"),
            // Only 127.0.0.0/8 is loopback.
            ("http:", "128.0.0.1"),
            // `localhost` as a label, not the name's end.
            ("http:", "localhost.example.org"),
        ] {
            assert!(
                plain_http_across_network(protocol, hostname),
                "{protocol}//{hostname}"
            );
        }
    }

    #[test]
    fn https_or_loopback_is_not_across_the_network() {
        for (protocol, hostname) in [
            // A reverse proxy's TLS.
            ("https:", "media.example.org"),
            ("https:", "192.168.1.10"),
            // An SSH tunnel, or the server's own browser.
            ("http:", "localhost"),
            ("http:", "LOCALHOST"),
            ("http:", "spoolframe.localhost"),
            ("http:", "127.0.0.1"),
            ("http:", "127.8.9.10"),
            ("http:", "[::1]"),
            // Not a page on the network at all.
            ("file:", ""),
        ] {
            assert!(
                !plain_http_across_network(protocol, hostname),
                "{protocol}//{hostname}"
            );
        }
    }

    #[test]
    fn a_page_serves_passkeys_of_its_host_and_its_parent_domains() {
        assert!(page_serves("media.example.org", "media.example.org"));
        assert!(page_serves("media.example.org", "example.org"));
        assert!(page_serves("localhost", "localhost"));
        assert!(!page_serves("127.0.0.1", "localhost"));
        assert!(!page_serves("example.org", "media.example.org"));
        assert!(!page_serves("evilexample.org", "example.org"));
    }

    #[test]
    fn a_code_shows_in_two_groups_however_it_came() {
        assert_eq!(display_code("BCDFGHJK"), "BCDF-GHJK");
        assert_eq!(display_code("bcdf-ghjk"), "BCDF-GHJK");
        assert_eq!(display_code("nonsense!"), "nonsense!");
    }

    #[test]
    fn a_cancelled_passkey_prompt_says_nothing_changed() {
        assert_eq!(
            passkey_prompt_error("NotAllowedError", true),
            "Cancelled, or it took too long. Nothing was changed."
        );
        // The same name means something else when signing in.
        assert_eq!(
            passkey_prompt_error("InvalidStateError", true),
            "That device already has a passkey for this account."
        );
        assert_eq!(
            passkey_prompt_error("InvalidStateError", false),
            "The browser's passkey prompt failed (InvalidStateError)."
        );
    }

    #[test]
    fn the_new_refusals_have_words_of_their_own() {
        let explain = |code| {
            ApiError {
                code,
                message: "server words".into(),
                data: None,
            }
            .explain()
        };
        assert!(explain(error_code::PASSKEY_REFUSED).contains("use your password"));
        assert!(explain(error_code::NO_SUCH_SIGN_IN_CODE).contains("5 minutes"));
        // The server's reason, where it has one.
        assert_eq!(explain(error_code::NOT_CONFIGURED), "server words");
        assert_eq!(explain(error_code::LAST_SIGN_IN_METHOD), "server words");
    }

    #[test]
    fn setup_offers_a_passkey_only_where_one_can_be_made() {
        assert_eq!(setup_without_passkeys(true, true, None), None);
        let http = setup_without_passkeys(false, false, None).unwrap();
        assert!(
            http.starts_with("Passkeys need HTTPS or localhost"),
            "{http}"
        );
        let browser = setup_without_passkeys(true, false, None).unwrap();
        assert!(browser.starts_with("This browser"), "{browser}");
        let server = setup_without_passkeys(true, true, Some("no HTTPS")).unwrap();
        assert!(
            server.contains(": no HTTPS, so the admin gets a password"),
            "{server}"
        );
        // The page's own reason first: the server's says nothing it can fix.
        let both = setup_without_passkeys(false, true, Some("no HTTPS")).unwrap();
        assert!(both.starts_with("Passkeys need"), "{both}");
    }

    fn refusal(code: i32, message: &str) -> ApiError {
        ApiError {
            code,
            message: message.into(),
            data: None,
        }
    }

    #[test]
    fn a_password_is_offered_for_removal_only_beside_another_way_in() {
        assert!(can_remove_password(true, true, false));
        assert!(can_remove_password(true, false, true));
        assert!(!can_remove_password(true, false, false));
        assert!(!can_remove_password(false, true, true));
    }

    #[test]
    fn removing_the_last_admin_password_says_whose_it_is() {
        let last = refusal(
            error_code::LAST_ADMIN,
            "\"andrey\" is the last enabled admin with a password; give another admin a password first",
        );
        assert_eq!(
            password_remove_error(&last, None),
            "You're the last admin with a password. Give another admin a password first."
        );
        assert_eq!(
            password_remove_error(&last, Some("andrey")),
            "andrey is the last admin with a password. Give another admin a password first."
        );
    }

    #[test]
    fn removing_the_last_way_in_says_what_to_add() {
        let only = refusal(
            error_code::LAST_SIGN_IN_METHOD,
            "\"sam\" has no other way to sign in: add a passkey or link a sign-in provider first",
        );
        assert_eq!(
            password_remove_error(&only, None),
            "Your password is your only way to sign in. Add a passkey or link a sign-in provider first."
        );
        assert_eq!(
            password_remove_error(&only, Some("sam")),
            "The password is sam's only way to sign in. They need a passkey or a linked sign-in provider first."
        );
        // Anything else as the server says it.
        let gone = refusal(error_code::NO_SUCH_USER, "no user \"sam\"");
        assert_eq!(password_remove_error(&gone, Some("sam")), "no user \"sam\"");
    }

    #[test]
    fn the_users_page_names_the_user_a_refusal_is_about() {
        let last = refusal(
            error_code::LAST_ADMIN,
            "\"andrey\" is the last enabled admin; make another admin first",
        );
        assert_eq!(
            user_change_error(&last, "andrey"),
            "andrey is the last admin. Make someone else an admin first."
        );
        let taken = refusal(error_code::USER_EXISTS, "user \"sam\" already exists");
        assert_eq!(
            user_change_error(&taken, "sam"),
            "The username sam is taken."
        );
        // Anything else as the server says it.
        let other = refusal(error_code::NO_SUCH_USER, "no user \"sam\"");
        assert_eq!(user_change_error(&other, "sam"), "no user \"sam\"");
    }

    #[test]
    fn a_bad_username_is_shown_at_its_field_and_a_short_password_is_not() {
        let bad = refusal(
            error_code::INVALID_INPUT,
            "invalid username \"sam smith\": use 1-64 letters, digits, '.', '_' or '-'",
        );
        assert_eq!(
            add_user_error(&bad, "sam smith"),
            AddUserError::Username("Use 1–64 letters, digits, “.”, “_” or “-”.".into())
        );
        let short = refusal(
            error_code::INVALID_INPUT,
            "the password must be at least 8 characters",
        );
        assert_eq!(
            add_user_error(&short, "sam"),
            AddUserError::Form("the password must be at least 8 characters".into())
        );
        let taken = refusal(error_code::USER_EXISTS, "user \"sam\" already exists");
        assert_eq!(
            add_user_error(&taken, "sam"),
            AddUserError::Form("The username sam is taken.".into())
        );
    }

    fn user(name: &str, role: Role, disabled: bool) -> UserInfo {
        UserInfo {
            username: name.into(),
            display_name: None,
            role,
            created_at: 0,
            disabled,
            has_password: true,
            password_changed_at: None,
            last_seen_at: None,
            has_passkey: false,
            has_sso: false,
            has_pin: false,
            device_count: 0,
            session_count: 0,
            rate_limited_secs: None,
        }
    }

    #[test]
    fn a_user_whose_sign_ins_are_limited_says_so_with_the_wait() {
        let mut steve = user("steve", Role::Consumer, false);
        assert_eq!(user_status(&steve), UserStatus::Active);
        steve.rate_limited_secs = Some(11 * 60 + 5);
        assert_eq!(
            user_status(&steve),
            UserStatus::Limited(
                "Too many wrong passwords. Password sign-ins as steve are refused for 12 minutes."
                    .into()
            )
        );
        steve.rate_limited_secs = Some(20);
        assert!(
            matches!(user_status(&steve), UserStatus::Limited(why) if why.ends_with("for a minute."))
        );
        // Disabled says more than limited.
        steve.disabled = true;
        assert_eq!(user_status(&steve), UserStatus::Disabled);
    }

    #[test]
    fn a_session_without_a_device_is_the_web_ui_s_or_the_cli_s() {
        assert_eq!(client_kind(None, Some(Platform::Web)), "Web");
        assert_eq!(
            client_kind(None, Some(Platform::Cli)),
            "Command-line client"
        );
    }

    #[test]
    fn the_one_enabled_admin_is_the_last() {
        let users = [
            user("root", Role::Admin, false),
            user("bob", Role::Consumer, false),
            // A disabled admin cannot stand in.
            user("old", Role::Admin, true),
        ];
        assert!(last_admin(&users, "root"));
        assert!(!last_admin(&users, "bob"));
        assert!(!last_admin(&users, "old"));
        let two = [
            user("root", Role::Admin, false),
            user("alice", Role::Admin, false),
        ];
        assert!(!last_admin(&two, "root"));
    }

    #[test]
    fn recent_times_read_relative_and_old_ones_as_dates() {
        let now = 1_791_000_000;
        assert_eq!(ago(now, now - 5), "just now");
        assert_eq!(ago(now, now - 60), "1 minute ago");
        assert_eq!(ago(now, now - 7_200), "2 hours ago");
        assert_eq!(ago(now, now - 3 * 86_400), "3 days ago");
        assert_eq!(ago(now, 1_735_689_600), "2025-01-01");
        // A clock a little behind the server's.
        assert_eq!(ago(now, now + 30), "just now");
    }

    #[test]
    fn sign_in_methods_name_each_way_in() {
        let mut u = user("sam", Role::Consumer, false);
        assert_eq!(sign_in_methods(&u), "Password");
        u.has_passkey = true;
        assert_eq!(sign_in_methods(&u), "Password, passkey");
        u.has_password = false;
        assert_eq!(sign_in_methods(&u), "Passkey");
        u.has_sso = true;
        assert_eq!(sign_in_methods(&u), "Passkey, provider");
        u.has_passkey = false;
        assert_eq!(sign_in_methods(&u), "Provider");
        u.has_sso = false;
        assert_eq!(sign_in_methods(&u), "None");
    }

    #[test]
    fn counts_are_singular_only_for_one() {
        assert_eq!(count(1, "session"), "1 session");
        assert_eq!(count(0, "session"), "0 sessions");
        assert_eq!(count(4, "session"), "4 sessions");
    }

    fn named(name: &str, shown: &str, role: Role) -> UserInfo {
        UserInfo {
            display_name: Some(shown.into()),
            ..user(name, role, false)
        }
    }

    #[test]
    fn an_admin_s_avatar_is_teal() {
        assert_eq!(
            avatar_class(&named("andrey", "Andrey", Role::Admin)),
            "avatar avatar-teal"
        );
    }

    #[test]
    fn alice_is_mauve_and_steve_peach_as_the_board_draws_them() {
        assert_eq!(
            [
                avatar_class(&named("alice", "Alice", Role::Consumer)),
                avatar_class(&named("steve", "Steve", Role::Consumer)),
            ],
            ["avatar avatar-mauve", "avatar avatar-peach"]
        );
    }

    #[test]
    fn a_consumer_is_never_the_admin_s_teal() {
        let teal = ('a'..='z')
            .map(|c| user(&c.to_string(), Role::Consumer, false))
            .filter(|u| avatar_class(u) == "avatar avatar-teal")
            .count();
        assert_eq!(teal, 0);
    }

    #[test]
    fn the_avatar_tone_follows_the_letter_it_shows() {
        assert_eq!(
            avatar_class(&named("x1", "Sam", Role::Consumer)),
            avatar_class(&named("x2", "steve", Role::Consumer))
        );
    }

    #[test]
    fn a_disabled_user_s_avatar_is_grey() {
        assert_eq!(
            avatar_class(&user("andrey", Role::Admin, true)),
            "avatar avatar-off"
        );
    }

    #[test]
    fn the_admins_come_first_in_the_server_s_order() {
        let order: Vec<String> = users_in_order(vec![
            user("alice", Role::Consumer, false),
            user("andrey", Role::Admin, false),
            user("steve", Role::Consumer, false),
            user("root", Role::Admin, false),
        ])
        .into_iter()
        .map(|u| u.username)
        .collect();
        assert_eq!(order, ["andrey", "root", "alice", "steve"]);
    }

    fn device(id: &str, seen: i64) -> spoolframe_admin_api::DeviceEntry {
        spoolframe_admin_api::DeviceEntry {
            endpoint_id: id.into(),
            name: None,
            platform: None,
            last_seen_at: seen,
            pin_required: false,
            trusted: false,
            auto_login: false,
        }
    }

    #[test]
    fn a_device_others_signed_in_on_names_them() {
        let everyone = vec![
            (
                "Andrey".to_owned(),
                vec![device("pc", 10), device("laptop", 5)],
            ),
            ("Alice".to_owned(), vec![device("pc", 8)]),
            ("Steve".to_owned(), vec![device("pc", 3)]),
        ];
        let found = shared_devices("Andrey", &everyone);
        assert_eq!(
            found
                .iter()
                .map(|s| (s.device.endpoint_id.as_str(), s.also.clone()))
                .collect::<Vec<_>>(),
            [
                ("pc", vec!["Alice".to_owned(), "Steve".to_owned()]),
                ("laptop", vec![]),
            ]
        );
    }

    #[test]
    fn the_devices_most_recently_seen_come_first() {
        let everyone = vec![(
            "Andrey".to_owned(),
            vec![device("old", 1), device("new", 9)],
        )];
        let found: Vec<_> = shared_devices("Andrey", &everyone)
            .into_iter()
            .map(|s| s.device.endpoint_id)
            .collect();
        assert_eq!(found, ["new", "old"]);
    }

    #[test]
    fn a_user_not_listed_has_no_devices() {
        assert_eq!(shared_devices("Nobody", &[]), vec![]);
    }

    #[test]
    fn a_key_s_start_is_read_in_groups_of_four() {
        assert_eq!(
            key_start("dc9731894e65827601a7af6f146bd96a"),
            "dc97 3189 4e65 8276"
        );
    }

    fn report(ok: u64, no_mapping: &[&str], unreadable: (u64, &[&str])) -> PathReport {
        PathReport {
            library: "Music".into(),
            ok,
            no_mapping: PathFailures {
                count: no_mapping.len() as u64,
                examples: no_mapping.iter().map(|e| e.to_string()).collect(),
            },
            unreadable: PathFailures {
                count: unreadable.0,
                examples: unreadable.1.iter().map(|e| e.to_string()).collect(),
            },
        }
    }

    #[test]
    fn a_library_s_paths_say_the_worst_first_with_an_example() {
        assert_eq!(
            path_state(&report(1204, &[], (0, &[]))),
            PathState::Ok { files: 1204 }
        );
        let missing = report(
            10,
            &[],
            (
                41,
                &[
                    "/data/music/a: b.flac -> /media/music/a: b.flac: No such file or directory (os error 2)",
                ],
            ),
        );
        assert_eq!(
            path_state(&missing),
            PathState::Missing {
                count: 41,
                local_path: "/media/music/a: b.flac".into()
            }
        );
        let unmapped = report(
            0,
            &["/mnt/kids/a.mkv -> /mnt/kids/a.mkv: No such file or directory (os error 2)"],
            (3, &["/x -> /y: gone"]),
        );
        assert_eq!(
            path_state(&unmapped),
            PathState::NoMapping {
                plex_path: "/mnt/kids/a.mkv".into()
            }
        );
    }

    #[test]
    fn counts_have_their_thousands_apart() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1204), "1,204");
        assert_eq!(thousands(18_930), "18,930");
        assert_eq!(thousands(1_000_000), "1,000,000");
    }

    #[test]
    fn a_plex_url_shows_as_its_host_and_port() {
        assert_eq!(host_port("http://192.168.1.10:32400"), "192.168.1.10:32400");
        assert_eq!(
            host_port("https://plex.example.org/pms"),
            "plex.example.org"
        );
        assert_eq!(host_port("plex:32400"), "plex:32400");
    }

    #[test]
    fn an_unknown_error_shows_the_server_s_own_words() {
        let e = ApiError {
            code: error_code::INVALID_INPUT,
            message: "the password must be at least 8 characters".into(),
            data: None,
        };
        assert_eq!(e.explain(), "the password must be at least 8 characters");
    }
}
