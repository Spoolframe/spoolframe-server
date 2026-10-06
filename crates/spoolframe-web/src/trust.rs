//! Trusted devices in the pages (P4-03): what the web approval page sends
//! for a device that asked to be remembered, and what My profile and the
//! admin's user page say before and after a change to a device's trust,
//! auto-login or PIN.

/// What the approval page sends as `deviceSignInApprove`'s `remember`: the
/// approver's answer counts only for a device that asked (P4-01).
pub fn remembers(asked: bool, ticked: bool) -> bool {
    asked && ticked
}

/// What the approval page's box says (WebApproveStates 1 and 6), as it
/// stands: `who` is the approver, whose profile the device opens, and
/// `word` what the page calls the device ("TV"). `None` for a device that
/// did not ask to be remembered, which shows no box.
pub fn remember_line(asked: bool, ticked: bool, who: &str, word: &str) -> Option<String> {
    match (asked, ticked) {
        (false, _) => None,
        (true, true) => Some(format!(
            "It asked to be. It can open {who}'s profile without a password until it signs out. Untick to sign it in just this once."
        )),
        (true, false) => Some(format!(
            "Unticked: the {word} signs in this once and needs a sign-in again after it signs out."
        )),
    }
}

use spoolframe_admin_api::{DeviceEntry, SessionEntry};

/// Whose device a page changes: the signed-in user's own (My profile), or
/// a user's an admin manages, by the name the admin pages show them by.
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
    /// The dialog's icon (WebTrustStates).
    pub icon: Icon,
}

/// A trust dialog's icon, as the board draws each one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    /// Auto-login: the app opening by itself, a play mark.
    Start,
    /// The PIN: a lock.
    Pin,
    /// Trust taken away: a shield with a mark.
    Untrust,
    /// The device deleted: a trash can, in red.
    Delete,
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

/// `user`'s `device`, as an admin's page names someone else's device:
/// "Alice's Living room Apple TV", but "Alice's laptop" left as it is.
pub fn their_device(user: &str, device: &str) -> String {
    if device.starts_with(&format!("{user}'s ")) || device.starts_with(&format!("{user}’s ")) {
        device.to_owned()
    } else {
        format!("{user}'s {device}")
    }
}

/// What is asked before `change` on `device`, called `name`; `None` when
/// it is made at once. The WebTrustStates board's words: for another
/// user, `whose` carries the name the admin pages show them by.
pub fn question(
    change: Change,
    device: &DeviceEntry,
    name: &str,
    whose: &Whose,
) -> Option<Question> {
    let (profile, pin) = match whose {
        Whose::Mine => ("your profile".to_owned(), "your PIN".to_owned()),
        Whose::Theirs(user) => (format!("{user}'s profile"), format!("{user}'s PIN")),
    };
    match change {
        Change::StopTrusting => Some(Question {
            title: match whose {
                Whose::Mine => format!("Stop trusting {name}?"),
                Whose::Theirs(user) => format!("Stop trusting {}?", their_device(user, name)),
            },
            text: match whose {
                Whose::Mine => {
                    "It's signed out of your profile now, and needs your password next time."
                        .to_owned()
                }
                Whose::Theirs(user) => format!(
                    "{user} is signed out there now and needs a password next time. {user} isn't told by email; the device just asks to sign in."
                ),
            },
            action: "Stop trusting",
            danger: true,
            icon: Icon::Untrust,
        }),
        Change::AutoLoginOn => {
            let others = if device.pin_required {
                format!(
                    "{} is turned off on that device, and any other profile set to open there stops.",
                    capitalized(&pin)
                )
            } else {
                "Any other profile set to open there stops.".to_owned()
            };
            Some(Question {
                title: format!("Open {profile} automatically on {name}?"),
                text: format!(
                    "When the app starts there, it opens {profile} without asking. {others}"
                ),
                action: "Turn on",
                danger: false,
                icon: Icon::Start,
            })
        }
        Change::AskForPin if device.auto_login => Some(Question {
            title: format!("Ask for {pin} on {name}?"),
            text: format!(
                "Auto-login turns off there: the app shows Who's watching and asks for {pin}."
            ),
            action: "Turn PIN on",
            danger: false,
            icon: Icon::Pin,
        }),
        Change::Trust | Change::AutoLoginOff | Change::AskForPin | Change::StopAskingForPin => None,
    }
}

/// `text` with its first letter upper case.
fn capitalized(text: &str) -> String {
    let mut letters = text.chars();
    match letters.next() {
        Some(first) => first.to_uppercase().chain(letters).collect(),
        None => String::new(),
    }
}

/// What the page says once `change` is made: the board's toasts.
pub fn done(change: Change, name: &str, whose: &Whose) -> String {
    let (profile, pin) = match whose {
        Whose::Mine => ("your profile".to_owned(), "your PIN".to_owned()),
        Whose::Theirs(user) => (format!("{user}'s profile"), format!("{user}'s PIN")),
    };
    match change {
        Change::Trust => match whose {
            Whose::Mine => format!("{name} is trusted"),
            Whose::Theirs(user) => format!("{name} is trusted for {user}"),
        },
        Change::StopTrusting => match whose {
            Whose::Mine => format!("Stopped trusting {name} · signed out there"),
            Whose::Theirs(user) => format!(
                "Stopped trusting {} · signed out there",
                their_device(user, name)
            ),
        },
        Change::AutoLoginOn => format!("{name} opens {profile} at start · PIN off there"),
        Change::AutoLoginOff => format!("{name} no longer opens {profile} at start"),
        Change::AskForPin => format!("{name} asks for {pin}"),
        Change::StopAskingForPin => format!("{name} no longer asks for {pin}"),
    }
}

/// What "Confirm it's you" says a trust change is (WebTrustStates 1):
/// "Confirm it's you before {about}."
pub fn about(whose: &Whose) -> String {
    match whose {
        Whose::Mine => "changing how your devices sign in".to_owned(),
        Whose::Theirs(user) => format!("changing how {user}'s devices sign in"),
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

/// What is asked before deleting `name` (canvas v75): what it means, and
/// Delete in red. For another user, `whose` carries the name the admin
/// pages show them by.
pub fn forget_question(name: &str, whose: &Whose) -> Question {
    Question {
        title: match whose {
            Whose::Mine => format!("Delete {name}?"),
            Whose::Theirs(user) => format!("Delete {}?", their_device(user, name)),
        },
        text: "It is signed out and forgotten. If it signs in again it shows up as new.".to_owned(),
        action: "Delete",
        danger: true,
        icon: Icon::Delete,
    }
}

/// What the page says once `name` is deleted.
pub fn forgotten(name: &str) -> String {
    format!("Deleted {name}")
}

/// What "Confirm it's you" says deleting a device is: "Confirm it's you
/// before {about}."
pub fn forget_about(whose: &Whose) -> String {
    match whose {
        Whose::Mine => "deleting a device".to_owned(),
        Whose::Theirs(user) => format!("deleting {user}'s device"),
    }
}

/// How long a trusted device may go unused before its trust lapses: the
/// server's `auth.trust_idle_days` default. The admin API does not expose
/// the setting, and My profile is any user's, so the pages say the
/// default.
pub const TRUST_IDLE_DAYS: u32 = 180;

/// Why a browser's row has no switches: under the legend, and on the
/// row itself on a phone.
pub const BROWSER_NOTE: &str = "A browser can't be trusted or open by itself.";

/// One line of the legend under the device switches: the column's name
/// in bold, then what it does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegendLine {
    pub switch: Switch,
    pub term: &'static str,
    pub text: String,
}

/// The element id of `switch`'s legend line, which its column head, its
/// phone label and each of its switches name as their description.
pub fn legend_id(switch: Switch) -> &'static str {
    match switch {
        Switch::Pin => "device-legend-pin",
        Switch::Trusted => "device-legend-trusted",
        Switch::AutoLogin => "device-legend-auto-login",
    }
}

/// What each switch does, in the columns' order, for My profile
/// (`Whose::Mine`) or an admin's user page, which says "this user's". With
/// no PIN, the PIN line also says why its switch cannot be turned on;
/// auto-login's says it needs Trusted, why its switch is off until then.
pub fn legend(whose: &Whose, has_pin: bool) -> [LegendLine; 3] {
    let days = TRUST_IDLE_DAYS;
    let (pin, trusted, auto_login) = match whose {
        Whose::Mine => (
            format!(
                "Asks for your PIN before opening your profile here, even when the device is trusted.{}",
                if has_pin {
                    ""
                } else {
                    " You have no PIN yet: set one under Profile PIN to turn this on."
                }
            ),
            format!(
                "Remembered on this device: picking your profile on Who's watching opens it without your password. Ends when you sign out there, change your password, or don't use it for {days} days."
            ),
            "Opens your profile as soon as the app starts, instead of showing Who's watching. Needs Trusted; one profile per device.".to_owned(),
        ),
        Whose::Theirs(_) => (
            format!(
                "Asks for their PIN before opening this user's profile here, even when the device is trusted.{}",
                if has_pin {
                    ""
                } else {
                    " This user has no PIN, so no device can ask for one."
                }
            ),
            format!(
                "Remembered on this device: picking this user's profile on Who's watching opens it without their password. Ends when they sign out there, change their password, or don't use it for {days} days."
            ),
            "Opens this user's profile as soon as the app starts, instead of showing Who's watching. Needs Trusted; one profile per device.".to_owned(),
        ),
    };
    [
        LegendLine {
            switch: Switch::Pin,
            term: "PIN",
            text: pin,
        },
        LegendLine {
            switch: Switch::Trusted,
            term: "Trusted",
            text: trusted,
        },
        LegendLine {
            switch: Switch::AutoLogin,
            term: "Auto-login",
            text: auto_login,
        },
    ]
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

    /// It has Delete beside Sign out (canvas v75): an app's device, signed
    /// in or not. A browser's or the CLI's session has no device to
    /// delete, and this browser's row is this device.
    pub fn can_forget(&self) -> bool {
        self.device.is_some() && !self.current()
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
        Whose::Theirs("Alice".into())
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
    fn stopping_trust_on_your_own_device_says_it_needs_your_password_next_time() {
        let asked = question(
            Change::StopTrusting,
            &trusted_tv(),
            "Living room TV",
            &Whose::Mine,
        )
        .unwrap();
        assert_eq!(
            asked.text,
            "It's signed out of your profile now, and needs your password next time."
        );
    }

    #[test]
    fn stopping_trust_for_another_user_says_they_are_not_told_by_email() {
        let asked = question(
            Change::StopTrusting,
            &trusted_tv(),
            "Living room TV",
            &alice(),
        )
        .unwrap();
        assert_eq!(
            asked.text,
            "Alice is signed out there now and needs a password next time. Alice isn't told by email; the device just asks to sign in."
        );
    }

    #[test]
    fn stopping_trust_for_another_user_names_whose_device_it_is() {
        let asked = question(
            Change::StopTrusting,
            &trusted_tv(),
            "Living room TV",
            &alice(),
        )
        .unwrap();
        assert_eq!(asked.title, "Stop trusting Alice's Living room TV?");
    }

    #[test]
    fn a_device_already_named_for_its_user_is_not_named_twice() {
        assert_eq!(their_device("Alice", "Alice's laptop"), "Alice's laptop");
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
    fn auto_login_asks_to_open_the_profile_automatically() {
        let asked = question(
            Change::AutoLoginOn,
            &trusted_tv(),
            "Alice's laptop",
            &Whose::Mine,
        )
        .unwrap();
        assert_eq!(
            (asked.title.as_str(), asked.action),
            (
                "Open your profile automatically on Alice's laptop?",
                "Turn on"
            )
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
            "When the app starts there, it opens your profile without asking. Your PIN is turned off on that device, and any other profile set to open there stops."
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
            "When the app starts there, it opens Alice's profile without asking. Any other profile set to open there stops."
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
        let asked = question(Change::AskForPin, &auto, "Alice's laptop", &Whose::Mine).unwrap();
        assert_eq!(
            (asked.title.as_str(), asked.text.as_str(), asked.action),
            (
                "Ask for your PIN on Alice's laptop?",
                "Auto-login turns off there: the app shows Who's watching and asks for your PIN.",
                "Turn PIN on"
            )
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
    fn stopping_trust_says_it_was_signed_out_there() {
        assert_eq!(
            [Whose::Mine, alice()].map(|w| done(Change::StopTrusting, "Living room TV", &w)),
            [
                "Stopped trusting Living room TV · signed out there".to_owned(),
                "Stopped trusting Alice's Living room TV · signed out there".to_owned(),
            ]
        );
    }

    #[test]
    fn auto_login_on_says_the_pin_is_off_there() {
        assert_eq!(
            done(Change::AutoLoginOn, "Alice's laptop", &Whose::Mine),
            "Alice's laptop opens your profile at start · PIN off there"
        );
    }

    #[test]
    fn trusting_says_for_whom() {
        assert_eq!(
            done(Change::Trust, "Living room TV", &alice()),
            "Living room TV is trusted for Alice"
        );
    }

    #[test]
    fn confirming_it_is_you_says_whose_devices_change() {
        assert_eq!(
            [Whose::Mine, alice()].map(|w| about(&w)),
            [
                "changing how your devices sign in".to_owned(),
                "changing how Alice's devices sign in".to_owned(),
            ]
        );
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
    fn the_ticked_box_says_whose_profile_the_device_opens() {
        assert_eq!(
            remember_line(true, true, "Andrey", "TV").as_deref(),
            Some(
                "It asked to be. It can open Andrey's profile without a password until it signs out. Untick to sign it in just this once."
            )
        );
    }

    #[test]
    fn the_unticked_box_says_the_device_signs_in_just_this_once() {
        assert_eq!(
            remember_line(true, false, "Andrey", "TV").as_deref(),
            Some(
                "Unticked: the TV signs in this once and needs a sign-in again after it signs out."
            )
        );
    }

    fn legend_text(whose: &Whose, has_pin: bool) -> Vec<String> {
        legend(whose, has_pin).into_iter().map(|l| l.text).collect()
    }

    #[test]
    fn the_legend_names_the_columns_in_their_order() {
        assert_eq!(
            legend(&Whose::Mine, true).map(|l| (l.switch, l.term)),
            [
                (Switch::Pin, "PIN"),
                (Switch::Trusted, "Trusted"),
                (Switch::AutoLogin, "Auto-login"),
            ]
        );
    }

    #[test]
    fn my_legend_says_what_each_switch_does_to_your_profile() {
        assert_eq!(
            legend_text(&Whose::Mine, true),
            [
                "Asks for your PIN before opening your profile here, even when the device is trusted.",
                "Remembered on this device: picking your profile on Who's watching opens it without your password. Ends when you sign out there, change your password, or don't use it for 180 days.",
                "Opens your profile as soon as the app starts, instead of showing Who's watching. Needs Trusted; one profile per device.",
            ]
        );
    }

    #[test]
    fn an_admin_s_legend_says_this_user_s_profile() {
        assert_eq!(
            legend_text(&alice(), true),
            [
                "Asks for their PIN before opening this user's profile here, even when the device is trusted.",
                "Remembered on this device: picking this user's profile on Who's watching opens it without their password. Ends when they sign out there, change their password, or don't use it for 180 days.",
                "Opens this user's profile as soon as the app starts, instead of showing Who's watching. Needs Trusted; one profile per device.",
            ]
        );
    }

    #[test]
    fn without_a_pin_my_legend_says_to_set_one() {
        assert_eq!(
            legend_text(&Whose::Mine, false)[0],
            "Asks for your PIN before opening your profile here, even when the device is trusted. You have no PIN yet: set one under Profile PIN to turn this on."
        );
    }

    #[test]
    fn without_a_pin_an_admin_s_legend_says_no_device_can_ask_for_one() {
        assert_eq!(
            legend_text(&alice(), false)[0],
            "Asks for their PIN before opening this user's profile here, even when the device is trusted. This user has no PIN, so no device can ask for one."
        );
    }

    #[test]
    fn each_column_s_legend_line_has_its_own_id() {
        let ids = [Switch::Pin, Switch::Trusted, Switch::AutoLogin].map(legend_id);
        assert!(
            ids[0] != ids[1] && ids[1] != ids[2] && ids[0] != ids[2],
            "{ids:?}"
        );
    }

    #[test]
    fn a_device_that_did_not_ask_shows_no_box() {
        assert_eq!(remember_line(false, true, "Andrey", "TV"), None);
    }

    #[test]
    fn deleting_my_device_asks_by_its_name() {
        assert_eq!(
            forget_question("Living room TV", &Whose::Mine).title,
            "Delete Living room TV?"
        );
    }

    #[test]
    fn deleting_a_user_s_device_asks_by_their_name() {
        assert_eq!(
            forget_question("Living room TV", &alice()).title,
            "Delete Alice's Living room TV?"
        );
    }

    #[test]
    fn deleting_a_device_says_what_it_means() {
        assert_eq!(
            forget_question("Living room TV", &alice()).text,
            "It is signed out and forgotten. If it signs in again it shows up as new."
        );
    }

    #[test]
    fn deleting_is_the_red_delete_button() {
        let asked = forget_question("Living room TV", &Whose::Mine);
        assert_eq!(
            (asked.action, asked.danger, asked.icon),
            ("Delete", true, Icon::Delete)
        );
    }

    #[test]
    fn a_deleted_device_is_said_by_its_name() {
        assert_eq!(forgotten("Living room TV"), "Deleted Living room TV");
    }

    #[test]
    fn confirming_it_s_me_names_the_deletion() {
        assert_eq!(forget_about(&Whose::Mine), "deleting a device");
    }

    #[test]
    fn confirming_it_s_an_admin_names_whose_device() {
        assert_eq!(forget_about(&alice()), "deleting Alice's device");
    }

    #[test]
    fn an_app_s_device_can_be_deleted_signed_in_or_not() {
        let signed_out = Row {
            device: Some(tv()),
            sessions: Vec::new(),
        };
        let signed_in = Row {
            device: Some(tv()),
            sessions: vec![session("s1", Some("aa11"), 5, false)],
        };
        assert!(signed_out.can_forget() && signed_in.can_forget());
    }

    #[test]
    fn a_browser_s_session_has_no_device_to_delete() {
        let browser = Row {
            device: None,
            sessions: vec![session("s1", None, 5, false)],
        };
        assert!(!browser.can_forget());
    }

    #[test]
    fn this_device_is_not_deleted() {
        let this = Row {
            device: Some(tv()),
            sessions: vec![session("s1", Some("aa11"), 5, true)],
        };
        assert!(!this.can_forget());
    }
}
