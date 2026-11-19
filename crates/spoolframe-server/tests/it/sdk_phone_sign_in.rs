//! Signing a TV in with a phone, end to end (feature
//! `spoolframe:phone-sign-in`): the TV knows no server, the phone is
//! signed in to several, and the person picks some. The TV and the phone
//! are two SDK clients in this process, with no LAN discovery and no
//! relays: the phone dials the TV at the addresses its code carries, and
//! the TV the servers at those the phone hands it.

use std::sync::Arc;

use spoolframe_client_sdk::storage::MemoryStorage;
use spoolframe_client_sdk::types::Platform;
use spoolframe_client_sdk::{
    Client, Device, HeldPhoneSignIn, HeldSignIn, HoldEnd, PairingError, PhoneSignIn, SdkError,
    SignInFailure, SignInMethod, SignInOfferOutcome,
};
use spoolframe_types::ClientCapabilities;

use crate::support::{self, TestServer};

const PASSWORD: &str = "correct horse";

fn device(name: &str, platform: Platform) -> Device {
    Device {
        name: name.into(),
        client_name: format!("sdk-{name}"),
        client_version: "0".into(),
        platform,
        capabilities: ClientCapabilities::default(),
    }
}

async fn new_client(name: &str, platform: Platform) -> Client {
    support::sdk_client(Arc::new(MemoryStorage::default()), device(name, platform))
        .await
        .unwrap()
}

/// The phone's offers, held on the TV.
async fn held(shown: &PhoneSignIn) -> HeldSignIn {
    match shown.wait().await.unwrap() {
        HeldPhoneSignIn::SignIn(held) => held,
        other => panic!("not a sign-in: {other:?}"),
    }
}

fn id(server: &TestServer) -> String {
    server.server.id().to_string()
}

/// `count` servers, each with alice.
async fn servers(test: &str, count: usize) -> Vec<TestServer> {
    let mut servers = Vec::new();
    for n in 0..count {
        let server = TestServer::start(&format!("{test}-{n}")).await;
        server.add_user("alice", PASSWORD, Some("Alice")).await;
        servers.push(server);
    }
    servers
}

/// A phone that knows each of `servers`, verified, and is signed in to
/// each as alice.
async fn phone(servers: &[TestServer]) -> Client {
    let phone = new_client("phone", Platform::Ios).await;
    for server in servers {
        let addrs = server.server.addr().ip_addrs().copied().collect();
        phone.add_address(&id(server), addrs).unwrap();
        phone.connect(&id(server)).await.unwrap();
        phone.pin(&id(server), true).await.unwrap();
        phone
            .sign_in(&id(server), "alice", PASSWORD, false)
            .await
            .unwrap();
    }
    phone
}

async fn stop(servers: Vec<TestServer>) {
    for server in servers {
        server.stop().await;
    }
}

/// The user's goal: a TV that knows no server is signed in to two of the
/// three the phone is, picked on the phone, and keeps them.
#[tokio::test]
async fn a_tv_is_signed_in_to_the_servers_picked_on_the_phone() {
    let servers = servers("sdk-psi-seed", 3).await;
    let phone = phone(&servers).await;
    let tv = new_client("tv", Platform::Tvos).await;
    assert!(tv.servers().await.unwrap().is_empty());

    let shown = tv
        .start_phone_sign_in("Living Room TV", false)
        .await
        .unwrap();
    // The TV's "Signed in to 1 of 2": unknown until the offers come.
    let mut progress = shown.server_sign_ins();
    assert_eq!(progress.total(), None);
    let offer = phone.scan_phone_sign_in(shown.link()).await.unwrap();
    assert_eq!(offer.device_name(), "Living Room TV");
    assert_eq!(offer.platform(), Platform::Tvos);
    let mut choices = offer.servers().await.unwrap();
    assert_eq!(choices.len(), 3);
    for choice in &choices {
        assert_eq!(choice.session.user().username, "alice");
    }
    // The person leaves the third unticked.
    let picked: Vec<String> = [&servers[0], &servers[1]].iter().map(|s| id(s)).collect();
    choices.retain(|c| picked.contains(&c.server.endpoint_id));
    // The phone's answer waits for the person at the TV, who sees who
    // signs it in, and where, and confirms.
    let (results, signed) = tokio::join!(offer.approve(&choices, true), async {
        let held = held(&shown).await;
        // "phone wants to sign this TV in": its device name.
        assert_eq!(held.phone_name(), Some("phone"));
        let mut offered: Vec<_> = held
            .offers()
            .iter()
            .map(|o| (o.server.endpoint_id.clone(), o.user_name.clone()))
            .collect();
        offered.sort();
        let mut expected: Vec<_> = picked
            .iter()
            .map(|p| (p.clone(), "Alice".to_owned()))
            .collect();
        expected.sort();
        assert_eq!(offered, expected);
        // Held: nothing saved yet.
        assert!(tv.servers().await.unwrap().is_empty());
        held.confirm().await
    });
    let (results, signed) = (results.unwrap(), signed.unwrap());
    assert_eq!(results.len(), 2);
    for result in &results {
        assert!(
            matches!(result.outcome, SignInOfferOutcome::SignedIn),
            "{result:?}"
        );
        assert_eq!(result.user_name, "Alice");
    }

    let mut each = Vec::new();
    while let Some(done) = progress.next().await {
        assert!(done.result.is_ok(), "{done:?}");
        each.push(done.server);
    }
    assert_eq!(progress.total(), Some(2));
    each.sort();
    let mut expected_each = picked.clone();
    expected_each.sort();
    assert_eq!(each, expected_each);
    assert!(signed.failed.is_empty(), "{:?}", signed.failed);
    let mut on_tv: Vec<String> = signed.sessions.iter().map(|s| s.server()).collect();
    on_tv.sort();
    let mut expected = picked.clone();
    expected.sort();
    assert_eq!(on_tv, expected);
    for session in &signed.sessions {
        assert_eq!(session.user().username, "alice");
        // The phone chose to remember the TV.
        assert_eq!(session.remembered(), Some(true));
        session.libraries().await.unwrap();
    }

    // Saved on the TV, pinned but not verified: the phone vouched.
    let saved = tv.servers().await.unwrap();
    assert_eq!(saved.len(), 2);
    for entry in &saved {
        assert!(picked.contains(&entry.endpoint_id), "{entry:?}");
        assert!(entry.pinned && !entry.verified, "{entry:?}");
        // Signed in, not used: nobody at the TV opened a profile yet.
        assert_eq!(entry.last_used_at, None, "{entry:?}");
        // Kept, as an approved sign-in is.
        let profiles = tv.saved_profiles(&entry.endpoint_id).await.unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].method, Some(SignInMethod::Device));
    }
    tv.close().await;
    phone.close().await;
    stop(servers).await;
}

/// With guests off, a guest's phone that scans the TV's code is told so
/// (`NotAllowed`) and spends nothing: the same code then signs the TV in
/// to the phone's account.
#[tokio::test]
async fn with_guests_off_a_guest_is_refused_and_the_same_code_signs_the_tv_in() {
    let servers = servers("sdk-psi-guest-off", 1).await;
    let phone = phone(&servers).await;
    let guest = new_client("guest-phone", Platform::Ios).await;
    let (identity, _) = guest.portable().create("Steve").await.unwrap();
    let tv = new_client("tv", Platform::Tvos).await;
    assert!(!tv.portable().allow_guests().await.unwrap());
    let shown = tv.start_phone_sign_in("TV", false).await.unwrap();
    let refused = guest.scan_pairing(shown.link(), identity).await;
    assert!(
        matches!(refused, Err(SdkError::Pairing(PairingError::NotAllowed))),
        "{refused:?}"
    );
    let offer = phone.scan_phone_sign_in(shown.link()).await.unwrap();
    let choices = offer.servers().await.unwrap();
    let (results, signed) = tokio::join!(offer.approve(&choices, false), async {
        held(&shown).await.confirm().await
    });
    let results = results.unwrap();
    assert!(
        matches!(results[0].outcome, SignInOfferOutcome::SignedIn),
        "{results:?}"
    );
    let signed = signed.unwrap();
    assert_eq!(signed.sessions.len(), 1);
    assert_eq!(signed.sessions[0].user().username, "alice");
    tv.close().await;
    guest.close().await;
    phone.close().await;
    stop(servers).await;
}

/// "Confirm it's you" first where a server asks: the offer it refused is
/// not sent, the TV still waits, and once confirmed it goes through.
#[tokio::test]
async fn an_old_phone_session_confirms_it_is_them_first() {
    let servers = servers("sdk-psi-recent", 1).await;
    let phone = phone(&servers).await;
    let tv = new_client("tv", Platform::Tvos).await;
    let shown = tv.start_phone_sign_in("TV", false).await.unwrap();
    let offer = phone.scan_phone_sign_in(shown.link()).await.unwrap();
    let choices = offer.servers().await.unwrap();
    assert_eq!(choices.len(), 1);
    crate::app_passkeys::sql(
        &servers[0],
        "UPDATE sessions SET authenticated_at = authenticated_at - 3600 \
         WHERE user_id IN (SELECT id FROM users WHERE username = ?1)",
        "alice",
    )
    .await;

    let results = offer.approve(&choices, false).await.unwrap();
    assert!(
        matches!(
            results[0].outcome,
            SignInOfferOutcome::NotOffered(SignInFailure::RecentSignInRequired)
        ),
        "{results:?}"
    );
    choices[0].session.confirm_password(PASSWORD).await.unwrap();
    let (results, signed) = tokio::join!(offer.approve(&choices, false), async {
        held(&shown).await.confirm().await
    });
    let results = results.unwrap();
    assert!(
        matches!(results[0].outcome, SignInOfferOutcome::SignedIn),
        "{results:?}"
    );
    assert_eq!(signed.unwrap().sessions.len(), 1);
    tv.close().await;
    phone.close().await;
    stop(servers).await;
}

/// One server of three needs "Confirm it's you": the other two go
/// through, the TV still waits, and once confirmed that one alone is
/// offered and sent again. The TV ends signed in to all three, from one
/// scan, and is asked to confirm once.
#[tokio::test]
async fn a_server_that_failed_is_retried_alone_with_no_new_scan() {
    let servers = servers("sdk-psi-retry", 3).await;
    let phone = phone(&servers).await;
    let tv = new_client("tv", Platform::Tvos).await;
    let shown = tv.start_phone_sign_in("TV", false).await.unwrap();
    let mut progress = shown.server_sign_ins();
    let offer = phone.scan_phone_sign_in(shown.link()).await.unwrap();
    let choices = offer.servers().await.unwrap();
    assert_eq!(choices.len(), 3);
    let stale = id(&servers[2]);
    crate::app_passkeys::sql(
        &servers[2],
        "UPDATE sessions SET authenticated_at = authenticated_at - 3600 \
         WHERE user_id IN (SELECT id FROM users WHERE username = ?1)",
        "alice",
    )
    .await;

    let (results, signed) = tokio::join!(offer.approve(&choices, false), async {
        held(&shown).await.confirm().await
    });
    let results = results.unwrap();
    for result in &results {
        if result.endpoint_id == stale {
            assert!(
                matches!(
                    result.outcome,
                    SignInOfferOutcome::NotOffered(SignInFailure::RecentSignInRequired)
                ),
                "{result:?}"
            );
        } else {
            assert!(
                matches!(result.outcome, SignInOfferOutcome::SignedIn),
                "{result:?}"
            );
        }
    }
    assert_eq!(signed.unwrap().sessions.len(), 2);

    // "Confirm it's you" on the phone, then that one alone, again.
    let again: Vec<_> = choices
        .into_iter()
        .filter(|c| c.server.endpoint_id == stale)
        .collect();
    again[0].session.confirm_password(PASSWORD).await.unwrap();
    let retried = offer.approve(&again, false).await.unwrap();
    assert_eq!(retried.len(), 1);
    assert!(
        matches!(retried[0].outcome, SignInOfferOutcome::SignedIn),
        "{retried:?}"
    );

    let mut each = Vec::new();
    while let Some(done) = progress.next().await {
        assert!(done.result.is_ok(), "{done:?}");
        each.push(done.server);
    }
    assert_eq!(progress.total(), Some(3));
    each.sort();
    let mut all: Vec<String> = servers.iter().map(id).collect();
    all.sort();
    assert_eq!(each, all);
    let mut saved: Vec<String> = tv
        .servers()
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.endpoint_id)
        .collect();
    saved.sort();
    assert_eq!(saved, all);
    for server in &all {
        assert_eq!(tv.saved_profiles(server).await.unwrap().len(), 1);
    }
    tv.close().await;
    phone.close().await;
    stop(servers).await;
}

/// One server's sign-in runs out before the TV redeems it, and the phone
/// offers it again: it counts once. The TV's total stays 3, its latest
/// result is signed in, and it is in the sessions and not among the
/// failures.
#[tokio::test]
async fn a_server_retried_after_failing_on_the_tv_counts_once() {
    let servers = servers("sdk-psi-once", 3).await;
    let phone = phone(&servers).await;
    let tv = new_client("tv", Platform::Tvos).await;
    let shown = tv.start_phone_sign_in("TV", false).await.unwrap();
    let mut progress = shown.server_sign_ins();
    let offer = phone.scan_phone_sign_in(shown.link()).await.unwrap();
    let choices = offer.servers().await.unwrap();
    let late = id(&servers[2]);
    let (results, held) = tokio::join!(offer.approve(&choices, false), async {
        let held = held(&shown).await;
        // Its grant runs out while the person at the TV looks.
        servers[2]
            .clock
            .advance(std::time::Duration::from_secs(10 * 60));
        held.confirm().await.unwrap();
        held
    });
    let results = results.unwrap();
    let failed: Vec<_> = results
        .iter()
        .filter(|r| r.outcome != SignInOfferOutcome::SignedIn)
        .map(|r| r.endpoint_id.clone())
        .collect();
    assert_eq!(failed, std::slice::from_ref(&late), "{results:?}");

    // Again, that one alone: "Confirm it's you" first if it asks.
    let again: Vec<_> = choices
        .into_iter()
        .filter(|c| c.server.endpoint_id == late)
        .collect();
    let mut retried = offer.approve(&again, false).await.unwrap();
    if retried[0].outcome == SignInOfferOutcome::NotOffered(SignInFailure::RecentSignInRequired) {
        again[0].session.confirm_password(PASSWORD).await.unwrap();
        retried = offer.approve(&again, false).await.unwrap();
    }
    assert_eq!(
        retried[0].outcome,
        SignInOfferOutcome::SignedIn,
        "{retried:?}"
    );
    offer.finish().await;

    let mut latest = std::collections::BTreeMap::new();
    while let Some(done) = progress.next().await {
        latest.insert(done.server, done.result.is_ok());
    }
    assert_eq!(progress.total(), Some(3));
    assert_eq!(latest.len(), 3);
    assert!(latest.values().all(|ok| *ok), "{latest:?}");
    // The sign-in as it ended: every server signed in, none failed.
    let signed = held.confirm().await.unwrap();
    assert!(signed.failed.is_empty(), "{:?}", signed.failed);
    let mut on_tv: Vec<String> = signed.sessions.iter().map(|s| s.server()).collect();
    on_tv.sort();
    let mut all: Vec<String> = servers.iter().map(id).collect();
    all.sort();
    assert_eq!(on_tv, all);
    tv.close().await;
    phone.close().await;
    stop(servers).await;
}

/// Once the person at the TV confirmed, the sign-in is theirs: closing
/// the code's screen stops no later round, and a new code can be shown
/// while the phone still retries.
#[tokio::test]
async fn a_retry_lands_after_the_code_s_screen_is_gone_and_a_new_code_shows() {
    let servers = servers("sdk-psi-detached", 2).await;
    let phone = phone(&servers).await;
    let tv = new_client("tv", Platform::Tvos).await;
    let shown = tv.start_phone_sign_in("TV", false).await.unwrap();
    let mut progress = shown.server_sign_ins();
    let offer = phone.scan_phone_sign_in(shown.link()).await.unwrap();
    let choices = offer.servers().await.unwrap();
    let stale = id(&servers[1]);
    crate::app_passkeys::sql(
        &servers[1],
        "UPDATE sessions SET authenticated_at = authenticated_at - 3600 \
         WHERE user_id IN (SELECT id FROM users WHERE username = ?1)",
        "alice",
    )
    .await;
    let (results, ()) = tokio::join!(offer.approve(&choices, false), async {
        held(&shown).await.confirm().await.unwrap();
    });
    assert!(
        results
            .unwrap()
            .iter()
            .any(|r| r.outcome
                == SignInOfferOutcome::NotOffered(SignInFailure::RecentSignInRequired))
    );
    // The QR screen goes, and another code shows while the phone retries.
    drop(shown);
    let next_code = tv.start_phone_sign_in("TV", false).await.unwrap();

    let again: Vec<_> = choices
        .into_iter()
        .filter(|c| c.server.endpoint_id == stale)
        .collect();
    again[0].session.confirm_password(PASSWORD).await.unwrap();
    let retried = offer.approve(&again, false).await.unwrap();
    assert_eq!(
        retried[0].outcome,
        SignInOfferOutcome::SignedIn,
        "{retried:?}"
    );
    let mut done = Vec::new();
    while let Some(each) = progress.next().await {
        assert!(each.result.is_ok(), "{each:?}");
        done.push(each.server);
    }
    done.sort();
    let mut all: Vec<String> = servers.iter().map(id).collect();
    all.sort();
    assert_eq!(done, all);
    assert_eq!(tv.servers().await.unwrap().len(), 2);
    drop(next_code);
    tv.close().await;
    phone.close().await;
    stop(servers).await;
}

/// Two accounts on one server that share a display name are two
/// profiles to the TV: both sign in, and the total counts both.
#[tokio::test]
async fn two_accounts_with_one_display_name_both_sign_in() {
    let server = TestServer::start("sdk-psi-namesakes").await;
    server.add_user("alice", PASSWORD, Some("Alice")).await;
    server.add_user("alice2", PASSWORD, Some("Alice")).await;
    let phone = new_client("phone", Platform::Ios).await;
    let addrs = server.server.addr().ip_addrs().copied().collect();
    phone.add_address(&id(&server), addrs).unwrap();
    phone.connect(&id(&server)).await.unwrap();
    phone.pin(&id(&server), true).await.unwrap();
    for username in ["alice", "alice2"] {
        phone
            .sign_in(&id(&server), username, PASSWORD, false)
            .await
            .unwrap();
    }
    let tv = new_client("tv", Platform::Tvos).await;
    let shown = tv.start_phone_sign_in("TV", false).await.unwrap();
    let mut progress = shown.server_sign_ins();
    let offer = phone.scan_phone_sign_in(shown.link()).await.unwrap();
    let choices = offer.servers().await.unwrap();
    assert_eq!(choices.len(), 2);
    let (results, signed) = tokio::join!(offer.approve(&choices, false), async {
        held(&shown).await.confirm().await
    });
    for result in results.unwrap() {
        assert_eq!(result.outcome, SignInOfferOutcome::SignedIn, "{result:?}");
    }
    let mut on_tv: Vec<String> = signed
        .unwrap()
        .sessions
        .iter()
        .map(|s| s.user().username.clone())
        .collect();
    on_tv.sort();
    assert_eq!(on_tv, ["alice", "alice2"]);
    while progress.next().await.is_some() {}
    assert_eq!(progress.total(), Some(2));
    tv.close().await;
    phone.close().await;
    server.stop().await;
}

/// Approving the same profiles again offers none the TV is signed in to
/// already: no second session is made for them, and the TV's sign-ins
/// count each once.
#[tokio::test]
async fn approving_the_same_list_again_makes_no_second_session() {
    let servers = servers("sdk-psi-twice", 2).await;
    let phone = phone(&servers).await;
    let tv = new_client("tv", Platform::Tvos).await;
    let shown = tv.start_phone_sign_in("TV", false).await.unwrap();
    let mut progress = shown.server_sign_ins();
    let offer = phone.scan_phone_sign_in(shown.link()).await.unwrap();
    let choices = offer.servers().await.unwrap();
    let (first, signed) = tokio::join!(offer.approve(&choices, false), async {
        held(&shown).await.confirm().await
    });
    for result in first.unwrap() {
        assert_eq!(result.outcome, SignInOfferOutcome::SignedIn, "{result:?}");
    }
    assert_eq!(signed.unwrap().sessions.len(), 2);
    let again = offer.approve(&choices, false).await.unwrap();
    assert_eq!(again.len(), 2);
    for result in &again {
        assert_eq!(result.outcome, SignInOfferOutcome::SignedIn, "{result:?}");
    }
    let mut given = 0;
    while progress.next().await.is_some() {
        given += 1;
    }
    assert_eq!(progress.total(), Some(2));
    assert_eq!(given, 2, "a server was signed in to twice");
    tv.close().await;
    phone.close().await;
    stop(servers).await;
}

/// A phone that hangs up while the TV holds its offers is not a
/// rejection: nobody at the TV said no. The code ends aborted, the
/// phone left, and nothing is saved.
#[tokio::test]
async fn a_phone_that_hangs_up_mid_hold_is_not_a_rejection() {
    let servers = servers("sdk-psi-left", 1).await;
    let phone = phone(&servers).await;
    let tv = new_client("tv", Platform::Tvos).await;
    let shown = tv.start_phone_sign_in("TV", false).await.unwrap();
    let offer = Arc::new(phone.scan_phone_sign_in(shown.link()).await.unwrap());
    let choices = offer.servers().await.unwrap();
    let approving = {
        let offer = offer.clone();
        tokio::spawn(async move { offer.approve(&choices, false).await })
    };
    let held = held(&shown).await;
    approving.abort();
    let _ = approving.await;
    // The phone drops its offer: its connection goes with it.
    drop(offer);
    // The TV hears the connection close before anyone there answers,
    // as a person takes far longer than a hang-up takes to arrive.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let ended = held.confirm().await;
    assert!(
        matches!(
            ended,
            Err(SdkError::Pairing(PairingError::Aborted(ref why))) if why == "the phone left"
        ),
        "{ended:?}"
    );
    assert!(tv.servers().await.unwrap().is_empty());
    tv.close().await;
    phone.close().await;
    stop(servers).await;
}

/// A confirm page closes itself when the phone hangs up mid-hold:
/// `ended` says the phone left, with nobody at the TV having answered.
#[tokio::test]
async fn a_hold_s_end_says_the_phone_left_before_any_answer() {
    let servers = servers("sdk-psi-ended", 1).await;
    let phone = phone(&servers).await;
    let tv = new_client("tv", Platform::Tvos).await;
    let shown = tv.start_phone_sign_in("TV", false).await.unwrap();
    let offer = Arc::new(phone.scan_phone_sign_in(shown.link()).await.unwrap());
    let choices = offer.servers().await.unwrap();
    let approving = {
        let offer = offer.clone();
        tokio::spawn(async move { offer.approve(&choices, false).await })
    };
    let held = held(&shown).await;
    approving.abort();
    let _ = approving.await;
    drop(offer);
    let ended = tokio::time::timeout(std::time::Duration::from_secs(10), held.ended()).await;
    assert_eq!(ended, Ok(HoldEnd::PhoneLeft));
    tv.close().await;
    phone.close().await;
    stop(servers).await;
}

/// A code stopped on the TV while it holds a phone's offers ends the
/// hold, and `ended` says so.
#[tokio::test]
async fn a_hold_s_end_says_the_code_ended_when_it_is_cancelled() {
    let servers = servers("sdk-psi-ended-code", 1).await;
    let phone = phone(&servers).await;
    let tv = new_client("tv", Platform::Tvos).await;
    let shown = tv.start_phone_sign_in("TV", false).await.unwrap();
    let offer = Arc::new(phone.scan_phone_sign_in(shown.link()).await.unwrap());
    let choices = offer.servers().await.unwrap();
    let approving = {
        let offer = offer.clone();
        tokio::spawn(async move { offer.approve(&choices, false).await })
    };
    let held = held(&shown).await;
    shown.cancel();
    let ended = tokio::time::timeout(std::time::Duration::from_secs(10), held.ended()).await;
    assert_eq!(ended, Ok(HoldEnd::CodeEnded));
    approving.abort();
    tv.close().await;
    phone.close().await;
    stop(servers).await;
}

/// The person at the TV turns the phone's sign-in down, or walks away
/// from it: nothing is redeemed, no server saved, and the phone hears
/// each was declined.
#[tokio::test]
async fn a_sign_in_turned_down_on_the_tv_keeps_nothing() {
    let servers = servers("sdk-psi-reject", 1).await;
    let phone = phone(&servers).await;
    for drop_it in [false, true] {
        let tv = new_client("tv", Platform::Tvos).await;
        let shown = tv.start_phone_sign_in("TV", false).await.unwrap();
        let mut progress = shown.server_sign_ins();
        let offer = phone.scan_phone_sign_in(shown.link()).await.unwrap();
        let choices = offer.servers().await.unwrap();
        let (results, ()) = tokio::join!(offer.approve(&choices, false), async {
            let held = held(&shown).await;
            if drop_it {
                drop(held);
            } else {
                held.reject().await.unwrap();
            }
        });
        let results = results.unwrap();
        assert!(
            matches!(
                results[0].outcome,
                SignInOfferOutcome::Failed(SignInFailure::Declined)
            ),
            "{results:?}"
        );
        assert!(tv.servers().await.unwrap().is_empty());
        assert!(
            tv.saved_profiles(&id(&servers[0]))
                .await
                .unwrap()
                .is_empty()
        );
        assert!(progress.next().await.is_none());
        // Nothing made for the TV on the server either.
        let alice = phone.profiles(&id(&servers[0])).await.unwrap().remove(0);
        let devices = alice.devices().await.unwrap();
        assert!(
            !devices.iter().any(|d| d.name.as_deref() == Some("TV")),
            "{devices:?}"
        );
        tv.close().await;
    }
    phone.close().await;
    stop(servers).await;
}
