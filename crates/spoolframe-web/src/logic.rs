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
            error_code::FORBIDDEN => "Only an admin can do that.".into(),
            0 => format!("The server cannot be reached ({}).", self.message),
            _ => self.message.clone(),
        }
    }
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

    fn user(name: &str, role: Role, disabled: bool) -> UserInfo {
        UserInfo {
            username: name.into(),
            display_name: None,
            role,
            created_at: 0,
            disabled,
            has_password: true,
            last_seen_at: None,
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
    fn an_unknown_error_shows_the_server_s_own_words() {
        let e = ApiError {
            code: error_code::INVALID_INPUT,
            message: "the password must be at least 8 characters".into(),
            data: None,
        };
        assert_eq!(e.explain(), "the password must be at least 8 characters");
    }
}
