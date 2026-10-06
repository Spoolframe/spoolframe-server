//! What the approval page says (P3-07, P4-03), apart from the page: the
//! WebApproveStates board's words for each state, and what a refused
//! code means there.

use serde_json::Value;
use spoolframe_admin_api::{Platform, error_code};

use crate::logic::ApiError;

/// What the page calls the device that shows the code: "the TV" for an
/// Apple TV, "the computer" for a desktop app, else "the device".
pub fn device_word(platform: Platform) -> &'static str {
    match platform {
        Platform::Tvos => "TV",
        Platform::Linux | Platform::Macos => "computer",
        _ => "device",
    }
}

/// Why a typed or linked code was refused, as the page shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// No device shows it now: unknown, used or expired.
    Wrong,
    /// Too many wrong codes: none is looked up until `until_ms` (Unix
    /// milliseconds, by the browser's clock).
    Limited { until_ms: i64 },
    /// Something else went wrong: the server's words.
    Other(String),
}

/// What refusal `e`, answered at `now_ms`, is.
pub fn refused(e: &ApiError, now_ms: i64) -> Refused {
    match e.code {
        error_code::NO_SUCH_SIGN_IN_CODE => Refused::Wrong,
        error_code::RATE_LIMITED => {
            let wait = e
                .data
                .as_ref()
                .and_then(|d| d.get("retryAfterMs"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            Refused::Limited {
                until_ms: now_ms.saturating_add(i64::try_from(wait).unwrap_or(i64::MAX)),
            }
        }
        _ => Refused::Other(e.explain()),
    }
}

/// The wait left until `until_ms`, seen at `now_ms`, as the board's "Try
/// again in 11:20": minutes and seconds, rounded up so it never shows 0:00
/// while still waiting; hours first past an hour. `None` once it is over.
pub fn countdown(now_ms: i64, until_ms: i64) -> Option<String> {
    let left = until_ms - now_ms;
    if left <= 0 {
        return None;
    }
    let secs = (left + 999) / 1000;
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    Some(if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    })
}

/// Under the consent card's title: what to compare before saying yes.
pub fn check_line(word: &str) -> String {
    format!("Check that the code and the fingerprint match the {word} screen.")
}

/// Who is approving, and what the device gets.
pub fn signed_in_as_tail(word: &str) -> String {
    format!(". The {word} gets its own session, which you can sign out from My profile.")
}

/// What the approved card says, as the device was remembered or not.
pub fn approved_line(remembered: bool) -> &'static str {
    if remembered {
        "It's remembered, so it can reopen your profile until it signs out. You can change that in My profile › Devices. You can close this page."
    } else {
        "It's signed in just this once. You can sign it out in My profile › Devices. You can close this page."
    }
}

/// What the denied card says about `name`.
pub fn denied_line(name: &str) -> String {
    format!("{name} was not signed in. If you didn't start this, nothing else is needed.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn error(code: i32, data: Option<Value>) -> ApiError {
        ApiError {
            code,
            message: "refused".into(),
            data,
        }
    }

    #[test]
    fn a_tv_is_called_the_tv() {
        assert_eq!(device_word(Platform::Tvos), "TV");
    }

    #[test]
    fn a_desktop_app_is_called_the_computer() {
        assert_eq!(
            [Platform::Linux, Platform::Macos].map(device_word),
            ["computer", "computer"]
        );
    }

    #[test]
    fn a_phone_is_called_the_device() {
        assert_eq!(device_word(Platform::Ios), "device");
    }

    #[test]
    fn a_code_no_device_shows_is_wrong() {
        assert_eq!(
            refused(&error(error_code::NO_SUCH_SIGN_IN_CODE, None), 0),
            Refused::Wrong
        );
    }

    #[test]
    fn too_many_wrong_codes_wait_as_long_as_the_server_says() {
        let limited = error(
            error_code::RATE_LIMITED,
            Some(json!({ "retryAfterMs": 680_000 })),
        );
        assert_eq!(
            refused(&limited, 1_000),
            Refused::Limited { until_ms: 681_000 }
        );
    }

    #[test]
    fn any_other_refusal_says_the_server_s_reason() {
        assert_eq!(
            refused(&error(error_code::UNAUTHENTICATED, None), 0),
            Refused::Other("You are signed out. Sign in again.".into())
        );
    }

    #[test]
    fn the_countdown_is_minutes_and_seconds() {
        assert_eq!(countdown(0, 680_000).as_deref(), Some("11:20"));
    }

    #[test]
    fn the_countdown_pads_the_seconds() {
        assert_eq!(countdown(0, 65_000).as_deref(), Some("1:05"));
    }

    #[test]
    fn the_countdown_rounds_a_part_second_up() {
        assert_eq!(countdown(0, 400).as_deref(), Some("0:01"));
    }

    #[test]
    fn the_countdown_names_hours_past_an_hour() {
        assert_eq!(countdown(0, 3_723_000).as_deref(), Some("1:02:03"));
    }

    #[test]
    fn the_countdown_ends_when_the_wait_does() {
        assert_eq!(countdown(5_000, 5_000), None);
    }

    #[test]
    fn the_check_line_names_the_screen_with_the_code() {
        assert_eq!(
            check_line("TV"),
            "Check that the code and the fingerprint match the TV screen."
        );
    }

    #[test]
    fn a_remembered_device_says_where_to_change_it() {
        assert!(approved_line(true).starts_with("It's remembered"));
    }

    #[test]
    fn a_device_signed_in_once_says_so() {
        assert!(approved_line(false).starts_with("It's signed in just this once"));
    }

    #[test]
    fn denying_says_nothing_else_is_needed() {
        assert_eq!(
            denied_line("Living room Apple TV"),
            "Living room Apple TV was not signed in. If you didn't start this, nothing else is needed."
        );
    }
}
