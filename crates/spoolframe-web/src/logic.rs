//! What the pages decide, apart from the pages: host-compiled, so
//! `cargo test` covers it without a browser.

use serde::Deserialize;
use serde_json::Value;
use spoolframe_admin_api::{Role, UserInfo, error_code};

/// Where a browser opening the web UI belongs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Landing {
    /// A server with no users: register the first admin.
    Setup,
    Login,
    /// An admin's pages.
    Admin,
    /// Everyone else's: their own account.
    Profile,
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

/// Whether a page at host `page` can make and use passkeys bound to
/// `rp_id`: the browser takes its own host, or a parent domain of it.
pub fn page_serves(page: &str, rp_id: &str) -> bool {
    page == rp_id || page.ends_with(&format!(".{rp_id}"))
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

/// How `user` can sign in, as the users page's column says it.
pub fn sign_in_methods(user: &UserInfo) -> &'static str {
    match (user.has_password, user.has_passkey) {
        (true, true) => "Password, passkey",
        (true, false) => "Password",
        (false, true) => "Passkey",
        (false, false) => "None",
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

/// The avatar's class: a colour of the user's own, the same on every page
/// and every load, and grey while they are disabled.
pub fn avatar_class(username: &str, disabled: bool) -> &'static str {
    const TONES: [&str; 6] = [
        "avatar avatar-teal",
        "avatar avatar-mauve",
        "avatar avatar-peach",
        "avatar avatar-blue",
        "avatar avatar-pink",
        "avatar avatar-green",
    ];
    if disabled {
        return "avatar avatar-off";
    }
    // FNV-1a: stable across builds, unlike std's hasher.
    let hash = username.bytes().fold(0x811c_9dc5_u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    });
    TONES[hash as usize % TONES.len()]
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

/// A Plex URL as the Server page shows it: its host and port.
pub fn host_port(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.split(['/', '?']).next().unwrap_or(rest)
}

fn minutes(n: u64) -> String {
    match n {
        0 | 1 => "a minute".into(),
        n => format!("{n} minutes"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use spoolframe_admin_api::{PathFailures, PathReport};

    #[test]
    fn each_role_lands_on_its_own_pages() {
        assert_eq!(landing(false, Some(Role::Admin)), Landing::Admin);
        assert_eq!(landing(false, Some(Role::Consumer)), Landing::Profile);
        // A role this build does not know gets the least.
        assert_eq!(landing(false, Some(Role::Unknown)), Landing::Profile);
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
            last_seen_at: None,
            has_passkey: false,
            has_pin: false,
            device_count: 0,
            session_count: 0,
        }
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
    }

    #[test]
    fn counts_are_singular_only_for_one() {
        assert_eq!(count(1, "session"), "1 session");
        assert_eq!(count(0, "session"), "0 sessions");
        assert_eq!(count(4, "session"), "4 sessions");
    }

    #[test]
    fn a_user_keeps_their_avatar_colour_and_a_disabled_one_is_grey() {
        assert_eq!(avatar_class("alice", false), avatar_class("alice", false));
        // Not one colour for everyone.
        let names = ["alice", "andrey", "bob", "sam", "steve", "root"];
        let tones: std::collections::HashSet<_> =
            names.iter().map(|n| avatar_class(n, false)).collect();
        assert!(tones.len() > 2, "{tones:?}");
        assert_eq!(avatar_class("alice", true), "avatar avatar-off");
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
