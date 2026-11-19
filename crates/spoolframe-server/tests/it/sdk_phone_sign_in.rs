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
    Client, Device, HeldPhoneSignIn, HeldSignIn, PhoneSignIn, SdkError, SignInMethod,
    SignInOfferOutcome,
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
        // Kept, as an approved sign-in is.
        let profiles = tv.saved_profiles(&entry.endpoint_id).await.unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].method, Some(SignInMethod::Device));
    }
    tv.close().await;
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
            SignInOfferOutcome::NotOffered(SdkError::RecentSignInRequired)
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
            matches!(results[0].outcome, SignInOfferOutcome::Declined),
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
