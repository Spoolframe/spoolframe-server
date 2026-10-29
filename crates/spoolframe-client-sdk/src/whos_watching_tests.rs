//! "Who's watching" merged and its start chosen, with no server.

use super::*;
use crate::SavedProfile;
use spoolframe_protocol::TrustedProfile;

fn trusted(username: &str, auto_login: bool) -> TrustedProfile {
    TrustedProfile {
        user_id: format!("id-{username}"),
        username: username.into(),
        display_name: username.to_uppercase(),
        pin_required: false,
        auto_login,
        pin_digits: None,
    }
}

fn saved(username: &str, method: Option<SignInMethod>, portable: bool) -> SavedProfile {
    SavedProfile {
        username: username.into(),
        display_name: username.to_uppercase(),
        sign_in: ProfileSignIn::Password,
        method,
        portable: portable.then(|| "ab".repeat(32).parse().unwrap()),
    }
}

fn names(watching: &WhosWatching) -> Vec<&str> {
    watching
        .profiles
        .iter()
        .map(|p| p.username.as_str())
        .collect()
}

#[test]
fn trusted_then_portable_then_those_to_sign_in_again_each_once() {
    let watching = merge(
        Vec::new(),
        vec![trusted("anna", false)],
        vec![
            saved("bob", Some(SignInMethod::Password), false),
            saved("anna", Some(SignInMethod::Password), false),
            saved("guest", Some(SignInMethod::Portable), true),
        ],
        None,
    );
    assert_eq!(names(&watching), ["anna", "guest", "bob"]);
}

#[test]
fn each_opens_as_it_can() {
    let watching = merge(
        Vec::new(),
        vec![trusted("anna", false)],
        vec![
            saved("bob", Some(SignInMethod::Passkey), false),
            saved("guest", Some(SignInMethod::Portable), true),
        ],
        None,
    );
    let opens: Vec<_> = watching
        .profiles
        .iter()
        .map(|p| match &p.opens {
            ProfileOpens::Session(_) => "session".to_owned(),
            ProfileOpens::Trusted { user_id } => format!("trusted {user_id}"),
            ProfileOpens::Portable { .. } => "portable".to_owned(),
            ProfileOpens::SignIn { method } => format!("sign in {method:?}"),
        })
        .collect();
    assert_eq!(
        opens,
        ["trusted id-anna", "portable", "sign in Some(Passkey)"]
    );
}

#[test]
fn no_one_goes_to_the_sign_in_form() {
    let watching = merge(Vec::new(), Vec::new(), Vec::new(), None);
    assert_eq!(watching.launch_target(true), LaunchTarget::SignIn);
}

#[test]
fn the_profile_opened_at_start_opens() {
    let watching = merge(
        Vec::new(),
        vec![trusted("anna", false), trusted("bob", true)],
        Vec::new(),
        Some("anna"),
    );
    assert_eq!(
        watching.launch_target(true),
        LaunchTarget::Open {
            username: "bob".into()
        }
    );
}

#[test]
fn going_back_to_pick_opens_nothing() {
    let watching = merge(Vec::new(), vec![trusted("bob", true)], Vec::new(), None);
    assert_eq!(watching.launch_target(false), LaunchTarget::Pick);
}

/// Only a session opens by itself as the last; a profile whose session
/// ended is picked.
#[test]
fn the_last_profile_with_no_session_is_picked() {
    let watching = merge(
        Vec::new(),
        vec![trusted("anna", false)],
        Vec::new(),
        Some("anna"),
    );
    assert!(watching.get("anna").unwrap().last);
    assert_eq!(watching.launch_target(true), LaunchTarget::Pick);
}

/// A PIN pad knows its length before the PIN is typed.
#[test]
fn a_trusted_profile_says_how_long_its_pin_is() {
    let mut anna = trusted("anna", false);
    anna.pin_required = true;
    anna.pin_digits = Some(6);
    let watching = merge(Vec::new(), vec![anna], Vec::new(), None);
    let anna = watching.get("anna").unwrap();
    assert_eq!((anna.locked, anna.pin_digits), (true, Some(6)));
}
