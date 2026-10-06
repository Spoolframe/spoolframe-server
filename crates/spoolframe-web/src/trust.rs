//! Trusted devices in the pages (P4-03): what the web approval page sends
//! for a device that asked to be remembered, and what My profile and the
//! admin's user page say before and after a change to a device's trust,
//! auto-login or PIN.

/// What the approval page sends as `deviceSignInApprove`'s `remember`: the
/// approver's answer counts only for a device that asked (P4-01).
pub fn remembers(asked: bool, ticked: bool) -> bool {
    asked && ticked
}

/// The line under "Remember this device" on the approval page, as the box
/// stands; `None` for a device that did not ask, which shows no box.
pub fn remember_line(asked: bool, ticked: bool) -> Option<&'static str> {
    match (asked, ticked) {
        (false, _) => None,
        (true, true) => {
            Some("This device will be remembered. It can open your profile without a password.")
        }
        (true, false) => Some("It signs in just this once."),
    }
}

use spoolframe_admin_api::{DeviceEntry, SessionEntry};

/// Whose device a page changes: the signed-in user's own (My profile), or
/// a user's an admin manages, by username.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Whose {
    Mine,
    Theirs(String),
}

/// One of a device row's switches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Switch {
    Pin,
    Trusted,
    AutoLogin,
}

/// A change to one device, as a switch makes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Trust,
    StopTrusting,
    AutoLoginOn,
    AutoLoginOff,
    AskForPin,
    StopAskingForPin,
}

/// What a page asks before a change: a title, what follows from it, and
/// the button that makes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub title: String,
    pub text: String,
    pub action: &'static str,
    /// It takes something away: the button is the dangerous kind.
    pub danger: bool,
}

/// The change pressing `switch` on `device`'s row makes.
pub fn change(switch: Switch, device: &DeviceEntry) -> Change {
    match switch {
        Switch::Pin if device.pin_required => Change::StopAskingForPin,
        Switch::Pin => Change::AskForPin,
        Switch::Trusted if device.trusted => Change::StopTrusting,
        Switch::Trusted => Change::Trust,
        Switch::AutoLogin if device.auto_login => Change::AutoLoginOff,
        Switch::AutoLogin => Change::AutoLoginOn,
    }
}

/// Whether `switch` can be pressed on `device`'s row: auto-login only on a
/// trusted device, and the PIN asked for only once the user has one.
pub fn can_press(switch: Switch, device: &DeviceEntry, has_pin: bool) -> bool {
    match switch {
        Switch::Pin => has_pin || device.pin_required,
        Switch::Trusted => true,
        // On, it can always go off: the server keeps it only with trust.
        Switch::AutoLogin => device.trusted || device.auto_login,
    }
}

/// `setDeviceTrust`'s `(trusted, autoLogin)` for `change`; `None` for a
/// change to the PIN, which is `setPinRequired`'s.
pub fn trust_request(change: Change) -> Option<(bool, bool)> {
    match change {
        Change::Trust | Change::AutoLoginOff => Some((true, false)),
        Change::StopTrusting => Some((false, false)),
        Change::AutoLoginOn => Some((true, true)),
        Change::AskForPin | Change::StopAskingForPin => None,
    }
}

/// What is asked before `change` on `device`, called `name`; `None` when
/// it is made at once.
pub fn question(
    change: Change,
    device: &DeviceEntry,
    name: &str,
    whose: &Whose,
) -> Option<Question> {
    let (profile, who, needs) = match whose {
        Whose::Mine => ("your profile".to_owned(), "You are".to_owned(), "need"),
        Whose::Theirs(user) => (format!("{user}'s profile"), format!("{user} is"), "needs"),
    };
    match change {
        Change::StopTrusting => Some(Question {
            title: format!("Stop trusting {name}?"),
            text: format!(
                "{name} can no longer open {profile} without a password. {who} signed out there now, and {needs} a full sign-in to come back."
            ),
            action: "Stop trusting",
            danger: true,
        }),
        Change::AutoLoginOn => {
            let pin = if device.pin_required {
                " The PIN is no longer asked for there."
            } else {
                ""
            };
            Some(Question {
                title: format!("Turn on auto-login on {name}?"),
                text: format!(
                    "{name} opens {profile} by itself when it starts.{pin} It no longer opens anyone else's profile at start."
                ),
                action: "Turn on auto-login",
                danger: false,
            })
        }
        Change::AskForPin if device.auto_login => Some(Question {
            title: format!("Ask for the PIN on {name}?"),
            text: format!(
                "Auto-login turns off there: {name} no longer opens {profile} by itself at start."
            ),
            action: "Ask for the PIN",
            danger: false,
        }),
        Change::Trust | Change::AutoLoginOff | Change::AskForPin | Change::StopAskingForPin => None,
    }
}

/// What the page says once `change` is made.
pub fn done(change: Change, name: &str, whose: &Whose) -> String {
    let (profile, who, whose_pin) = match whose {
        Whose::Mine => (
            "your profile".to_owned(),
            "you are".to_owned(),
            "the PIN".to_owned(),
        ),
        Whose::Theirs(user) => (
            format!("{user}'s profile"),
            format!("{user} is"),
            format!("{user}'s PIN"),
        ),
    };
    match change {
        Change::Trust => match whose {
            Whose::Mine => format!("{name} is trusted"),
            Whose::Theirs(user) => format!("{name} is trusted for {user}"),
        },
        Change::StopTrusting => format!("{name} is no longer trusted · {who} signed out there"),
        Change::AutoLoginOn => format!("{name} opens {profile} at start"),
        Change::AutoLoginOff => format!("{name} no longer opens {profile} at start"),
        Change::AskForPin => format!("{name} asks for {whose_pin}"),
        Change::StopAskingForPin => format!("{name} no longer asks for {whose_pin}"),
    }
}

/// What "Confirm it's you" says the change is: "Confirm it's you before
/// {about}."
pub fn about(change: Change) -> &'static str {
    match change {
        Change::Trust => "trusting a device",
        Change::StopTrusting => "no longer trusting a device",
        Change::AutoLoginOn | Change::AutoLoginOff => "changing auto-login",
        Change::AskForPin => "asking for the PIN",
        Change::StopAskingForPin => "no longer asking for the PIN",
    }
}

/// What the page says once a device row's sessions are signed out: a
/// trusted device is forgotten too, as any revocation of its session ends
/// its trust (P4-01).
pub fn signed_out(name: &str, trusted: bool) -> String {
    if trusted {
        format!("{name} is signed out, and no longer trusted.")
    } else {
        format!("{name} is signed out.")
    }
}

/// One row of My profile's "Devices and sessions": a device the user has
/// signed in on, with its live sessions, or a browser's or the CLI's
/// session, which has no device and so no PIN or trust.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub device: Option<DeviceEntry>,
    pub sessions: Vec<SessionEntry>,
}

impl Row {
    /// This browser's own session is on it.
    pub fn current(&self) -> bool {
        self.sessions.iter().any(|s| s.current)
    }
}

/// The card's rows: each device once, with its sessions, then each
/// session without a device. This browser first, then the most recently
/// used.
pub fn rows(devices: Vec<DeviceEntry>, sessions: Vec<SessionEntry>) -> Vec<Row> {
    let mut rows: Vec<Row> = devices
        .into_iter()
        .map(|device| Row {
            device: Some(device),
            sessions: Vec::new(),
        })
        .collect();
    for session in sessions {
        let on = session.endpoint_id.as_deref().and_then(|id| {
            rows.iter_mut()
                .find(|r| r.device.as_ref().is_some_and(|d| d.endpoint_id == id))
        });
        match on {
            Some(row) => row.sessions.push(session),
            None => rows.push(Row {
                device: None,
                sessions: vec![session],
            }),
        }
    }
    rows.sort_by_key(|r| (!r.current(), std::cmp::Reverse(r.last_used())));
    rows
}

impl Row {
    /// When it was last used: the device's last connection, or its
    /// sessions' last use, whichever is later.
    fn last_used(&self) -> i64 {
        let seen = self.device.as_ref().map_or(i64::MIN, |d| d.last_seen_at);
        self.sessions
            .iter()
            .map(|s| s.last_used_at)
            .fold(seen, i64::max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spoolframe_admin_api::Platform;

    fn tv() -> DeviceEntry {
        DeviceEntry {
            endpoint_id: "aa11".into(),
            name: Some("Living room TV".into()),
            platform: Some(Platform::Tvos),
            last_seen_at: 100,
            pin_required: false,
            trusted: false,
            auto_login: false,
        }
    }

    fn trusted_tv() -> DeviceEntry {
        DeviceEntry {
            trusted: true,
            ..tv()
        }
    }

    fn session(id: &str, endpoint: Option<&str>, last_used_at: i64, current: bool) -> SessionEntry {
        SessionEntry {
            id: id.into(),
            device_name: None,
            created_at: 0,
            last_used_at,
            current,
            endpoint_id: endpoint.map(Into::into),
            platform: endpoint.map_or(Some(Platform::Web), |_| Some(Platform::Tvos)),
        }
    }

    fn device(id: &str, last_seen_at: i64) -> DeviceEntry {
        DeviceEntry {
            endpoint_id: id.into(),
            last_seen_at,
            ..tv()
        }
    }

    fn alice() -> Whose {
        Whose::Theirs("alice".into())
    }

    #[test]
    fn the_trusted_switch_trusts_a_device_that_is_not() {
        assert_eq!(change(Switch::Trusted, &tv()), Change::Trust);
    }

    #[test]
    fn the_trusted_switch_stops_trusting_a_trusted_device() {
        assert_eq!(change(Switch::Trusted, &trusted_tv()), Change::StopTrusting);
    }

    #[test]
    fn the_auto_login_switch_turns_it_on_and_off() {
        let on = DeviceEntry {
            auto_login: true,
            ..trusted_tv()
        };
        assert_eq!(
            (
                change(Switch::AutoLogin, &trusted_tv()),
                change(Switch::AutoLogin, &on)
            ),
            (Change::AutoLoginOn, Change::AutoLoginOff)
        );
    }

    #[test]
    fn the_pin_switch_asks_for_the_pin_or_stops() {
        let asks = DeviceEntry {
            pin_required: true,
            ..tv()
        };
        assert_eq!(
            (change(Switch::Pin, &tv()), change(Switch::Pin, &asks)),
            (Change::AskForPin, Change::StopAskingForPin)
        );
    }

    #[test]
    fn auto_login_cannot_be_pressed_on_a_device_that_is_not_trusted() {
        assert!(!can_press(Switch::AutoLogin, &tv(), true));
    }

    #[test]
    fn auto_login_can_be_pressed_on_a_trusted_device() {
        assert!(can_press(Switch::AutoLogin, &trusted_tv(), true));
    }

    #[test]
    fn without_a_pin_the_pin_cannot_be_asked_for() {
        assert!(!can_press(Switch::Pin, &tv(), false));
    }

    #[test]
    fn without_a_pin_a_device_that_still_asks_for_it_can_stop() {
        let asks = DeviceEntry {
            pin_required: true,
            ..tv()
        };
        assert!(can_press(Switch::Pin, &asks, false));
    }

    #[test]
    fn each_trust_change_sends_what_it_says() {
        assert_eq!(
            [
                Change::Trust,
                Change::StopTrusting,
                Change::AutoLoginOn,
                Change::AutoLoginOff,
            ]
            .map(trust_request),
            [
                Some((true, false)),
                Some((false, false)),
                Some((true, true)),
                Some((true, false)),
            ]
        );
    }

    #[test]
    fn a_pin_change_is_not_a_trust_request() {
        assert_eq!(trust_request(Change::AskForPin), None);
    }

    #[test]
    fn trusting_is_made_at_once() {
        assert_eq!(
            question(Change::Trust, &tv(), "Living room TV", &Whose::Mine),
            None
        );
    }

    #[test]
    fn stopping_trust_on_your_own_device_says_it_signs_you_out_there() {
        let asked = question(
            Change::StopTrusting,
            &trusted_tv(),
            "Living room TV",
            &Whose::Mine,
        )
        .unwrap();
        assert_eq!(
            asked.text,
            "Living room TV can no longer open your profile without a password. You are signed out there now, and need a full sign-in to come back."
        );
    }

    #[test]
    fn stopping_trust_for_another_user_says_they_are_signed_out_there() {
        let asked = question(
            Change::StopTrusting,
            &trusted_tv(),
            "Living room TV",
            &alice(),
        )
        .unwrap();
        assert_eq!(
            asked.text,
            "Living room TV can no longer open alice's profile without a password. alice is signed out there now, and needs a full sign-in to come back."
        );
    }

    #[test]
    fn stopping_trust_is_asked_as_a_danger() {
        let asked = question(
            Change::StopTrusting,
            &trusted_tv(),
            "Living room TV",
            &Whose::Mine,
        )
        .unwrap();
        assert_eq!(
            (asked.title.as_str(), asked.action, asked.danger),
            ("Stop trusting Living room TV?", "Stop trusting", true)
        );
    }

    #[test]
    fn auto_login_on_a_device_that_asks_for_the_pin_says_the_pin_goes() {
        let asks = DeviceEntry {
            pin_required: true,
            ..trusted_tv()
        };
        let asked = question(Change::AutoLoginOn, &asks, "Living room TV", &Whose::Mine).unwrap();
        assert_eq!(
            asked.text,
            "Living room TV opens your profile by itself when it starts. The PIN is no longer asked for there. It no longer opens anyone else's profile at start."
        );
    }

    #[test]
    fn auto_login_on_a_device_without_the_pin_says_nothing_of_it() {
        let asked = question(
            Change::AutoLoginOn,
            &trusted_tv(),
            "Living room TV",
            &alice(),
        )
        .unwrap();
        assert_eq!(
            asked.text,
            "Living room TV opens alice's profile by itself when it starts. It no longer opens anyone else's profile at start."
        );
    }

    #[test]
    fn auto_login_off_is_made_at_once() {
        assert_eq!(
            question(
                Change::AutoLoginOff,
                &trusted_tv(),
                "Living room TV",
                &Whose::Mine
            ),
            None
        );
    }

    #[test]
    fn asking_for_the_pin_where_auto_login_is_on_says_auto_login_goes() {
        let auto = DeviceEntry {
            auto_login: true,
            ..trusted_tv()
        };
        let asked = question(Change::AskForPin, &auto, "Living room TV", &Whose::Mine).unwrap();
        assert_eq!(
            asked.text,
            "Auto-login turns off there: Living room TV no longer opens your profile by itself at start."
        );
    }

    #[test]
    fn asking_for_the_pin_without_auto_login_is_made_at_once() {
        assert_eq!(
            question(
                Change::AskForPin,
                &trusted_tv(),
                "Living room TV",
                &Whose::Mine
            ),
            None
        );
    }

    #[test]
    fn stopping_trust_says_who_was_signed_out() {
        assert_eq!(
            [Whose::Mine, alice()].map(|w| done(Change::StopTrusting, "Living room TV", &w)),
            [
                "Living room TV is no longer trusted · you are signed out there".to_owned(),
                "Living room TV is no longer trusted · alice is signed out there".to_owned(),
            ]
        );
    }

    #[test]
    fn trusting_says_for_whom() {
        assert_eq!(
            done(Change::Trust, "Living room TV", &alice()),
            "Living room TV is trusted for alice"
        );
    }

    #[test]
    fn confirming_it_is_you_names_the_trust_change() {
        assert_eq!(about(Change::StopTrusting), "no longer trusting a device");
    }

    #[test]
    fn signing_out_a_trusted_device_says_it_is_no_longer_trusted() {
        assert_eq!(
            signed_out("Living room TV", true),
            "Living room TV is signed out, and no longer trusted."
        );
    }

    #[test]
    fn signing_out_a_device_that_is_not_trusted_says_only_that() {
        assert_eq!(
            signed_out("Living room TV", false),
            "Living room TV is signed out."
        );
    }

    #[test]
    fn a_device_is_one_row_with_its_sessions() {
        let found = rows(
            vec![device("aa11", 50)],
            vec![
                session("s1", Some("aa11"), 60, false),
                session("s2", Some("aa11"), 70, false),
            ],
        );
        assert_eq!(
            found.iter().map(|r| r.sessions.len()).collect::<Vec<_>>(),
            [2]
        );
    }

    #[test]
    fn a_device_with_no_live_session_is_still_a_row() {
        let found = rows(vec![device("aa11", 50)], vec![]);
        assert_eq!(
            found[0].device.as_ref().map(|d| d.endpoint_id.as_str()),
            Some("aa11")
        );
    }

    #[test]
    fn a_browser_session_is_a_row_without_a_device() {
        let found = rows(vec![], vec![session("web", None, 10, false)]);
        assert_eq!(found[0].device, None);
    }

    #[test]
    fn this_browser_comes_first_then_the_most_recently_used() {
        let found = rows(
            vec![device("old", 10), device("new", 90)],
            vec![
                session("here", None, 5, true),
                session("cli", None, 50, false),
                session("s-old", Some("old"), 95, false),
            ],
        );
        let order: Vec<String> = found
            .iter()
            .map(|r| match &r.device {
                Some(d) => d.endpoint_id.clone(),
                None => r.sessions[0].id.clone(),
            })
            .collect();
        assert_eq!(order, ["here", "old", "new", "cli"]);
    }

    #[test]
    fn a_device_that_asked_is_remembered_while_the_box_is_ticked() {
        assert!(remembers(true, true));
    }

    #[test]
    fn unticking_the_box_signs_in_just_this_once() {
        assert!(!remembers(true, false));
    }

    #[test]
    fn a_device_that_did_not_ask_is_never_remembered() {
        assert!(!remembers(false, true));
    }

    #[test]
    fn the_ticked_box_says_the_device_will_be_remembered() {
        assert_eq!(
            remember_line(true, true),
            Some("This device will be remembered. It can open your profile without a password.")
        );
    }

    #[test]
    fn the_unticked_box_says_it_signs_in_just_this_once() {
        assert_eq!(
            remember_line(true, false),
            Some("It signs in just this once.")
        );
    }

    #[test]
    fn a_device_that_did_not_ask_shows_no_box() {
        assert_eq!(remember_line(false, true), None);
    }
}
