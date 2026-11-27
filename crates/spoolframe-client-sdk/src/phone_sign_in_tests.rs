//! Signing a TV in with a phone, between two clients on this machine and
//! no server: the code, the listener and the exchange up to the offers.
//! Offers redeemed on real servers are the server's integration tests'
//! (`sdk_phone_sign_in`). No relays, no LAN discovery: the phone dials
//! the addresses the code carries.

use std::future::Future;

use spoolframe_protocol::pairing::{PairingCode, parse_link};
use spoolframe_types::{ClientCapabilities, Platform};

use std::sync::Arc;

use super::*;
use crate::client::{ClientOptions, Device};
use crate::storage::{MemoryStorage, Storage};

/// No test takes this long unless something hangs.
const HANG: Duration = Duration::from_secs(60);

async fn within<T>(test: impl Future<Output = T>) -> T {
    tokio::time::timeout(HANG, test)
        .await
        .expect("the test hung")
}

async fn open(name: &str, platform: Platform) -> Client {
    open_on(Arc::new(MemoryStorage::default()), name, platform).await
}

async fn open_on(storage: Arc<MemoryStorage>, name: &str, platform: Platform) -> Client {
    let device = Device {
        name: name.into(),
        client_name: format!("sdk-{name}"),
        client_version: "0".into(),
        platform,
        capabilities: ClientCapabilities::default(),
    };
    let options = ClientOptions {
        lan_discovery: false,
        relays: false,
        ..ClientOptions::default()
    };
    Client::open_with_options(storage, device, options)
        .await
        .unwrap()
}

async fn tv() -> Client {
    open("tv", Platform::Tvos).await
}

async fn phone() -> Client {
    open("phone", Platform::Ios).await
}

fn pairing_error(result: Result<impl std::fmt::Debug, SdkError>) -> PairingError {
    match result {
        Err(SdkError::Pairing(e)) => e,
        other => panic!("not a pairing error: {other:?}"),
    }
}

#[tokio::test]
async fn the_phone_learns_what_the_tv_is() {
    within(async {
        let tv = tv().await;
        let shown = tv
            .start_phone_sign_in("Living Room TV", false)
            .await
            .unwrap();
        assert!(shown.link().starts_with("spoolframe://pair?e="));
        assert!(shown.expires_in() <= CODE_TTL);
        let phone = phone().await;
        let (connected, offer) =
            tokio::join!(shown.connected(), phone.scan_phone_sign_in(shown.link()));
        let offer = offer.unwrap();
        assert_eq!(connected.unwrap(), PhoneConnected::SigningIn);
        assert_eq!(offer.device_name(), "Living Room TV");
        assert_eq!(offer.platform(), Platform::Tvos);
        assert_eq!(offer.client_name(), "sdk-tv");
        assert_eq!(offer.device(), tv.inner.endpoint.id());
        // A phone with no servers has no one to sign the TV in as.
        assert!(offer.servers().await.unwrap().is_empty());
        assert!(matches!(offer.send().await, Err(SdkError::InvalidInput(_))));
        // The TV waits for the first round, until the phone declines.
        assert!(offer.takes_another_round().await);
        offer.decline().await.unwrap();
        assert!(!offer.takes_another_round().await);
        assert_eq!(pairing_error(shown.wait().await), PairingError::Declined);
    })
    .await;
}

#[tokio::test]
async fn a_wrong_code_is_refused_and_ends_the_code() {
    within(async {
        let tv = tv().await;
        let shown = tv.start_phone_sign_in("TV", false).await.unwrap();
        let mut link = parse_link(shown.link()).unwrap();
        link.code = PairingCode::from_bytes([9; 32]);
        let phone = phone().await;
        assert_eq!(
            pairing_error(phone.scan_phone_sign_in(&link.link()).await),
            PairingError::WrongCode
        );
        assert_eq!(pairing_error(shown.wait().await), PairingError::WrongCode);
        // The right code, now spent with the wrong one, is refused too.
        assert!(phone.scan_phone_sign_in(shown.link()).await.is_err());
    })
    .await;
}

#[tokio::test]
async fn the_listener_is_gone_once_the_code_is_dropped() {
    within(async {
        let tv = tv().await;
        let shown = tv.start_phone_sign_in("TV", false).await.unwrap();
        let link = shown.link().to_owned();
        drop(shown);
        assert!(matches!(
            phone().await.scan_phone_sign_in(&link).await,
            Err(SdkError::Unreachable(_))
        ));
        // And the next code may be shown.
        tv.start_phone_sign_in("TV", false).await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn one_code_at_a_time_a_pairing_s_or_a_phone_sign_in_s() {
    within(async {
        let tv = tv().await;
        tv.portable().set_allow_guests(true).await.unwrap();
        let _shown = tv.start_phone_sign_in("TV", false).await.unwrap();
        assert_eq!(
            pairing_error(tv.start_pairing("TV").await),
            PairingError::AlreadyPairing
        );
        assert_eq!(
            pairing_error(tv.start_phone_sign_in("TV", false).await),
            PairingError::AlreadyPairing
        );
    })
    .await;
}

/// Signing in to an account is not a guest's: it works with guests off.
/// A guest's phone that scans the code then is told guests are off,
/// typed, and its dial spends nothing: no key is made for its identity,
/// and the same code still signs the TV in.
#[tokio::test]
async fn with_guests_off_a_guest_is_refused_and_the_code_still_signs_in() {
    within(async {
        let tv = tv().await;
        assert!(!tv.portable().allow_guests().await.unwrap());
        let shown = tv.start_phone_sign_in("TV", false).await.unwrap();
        let phone = phone().await;
        let (identity, _) = phone.portable().create("Steve").await.unwrap();
        assert_eq!(
            pairing_error(phone.scan_pairing(shown.link(), identity).await),
            PairingError::NotAllowed
        );
        assert!(!tv.portable().has_device_key(identity).await.unwrap());
        let offer = phone.scan_phone_sign_in(shown.link()).await.unwrap();
        assert_eq!(offer.device_name(), "TV");
        offer.decline().await.unwrap();
    })
    .await;
}

/// With guests on, a guest's phone pairs its identity with the very code
/// a phone sign-in would use.
#[tokio::test]
async fn with_guests_on_the_same_code_pairs_a_guest() {
    within(async {
        let tv = tv().await;
        tv.portable().set_allow_guests(true).await.unwrap();
        let shown = tv.start_phone_sign_in("TV", false).await.unwrap();
        let phone = phone().await;
        let (identity, _) = phone.portable().create("Steve").await.unwrap();
        let offer = phone.scan_pairing(shown.link(), identity).await.unwrap();
        match shown.connected().await.unwrap() {
            PhoneConnected::Guest(connected) => {
                assert_eq!(connected.fingerprint, offer.fingerprint())
            }
            other => panic!("not a guest: {other:?}"),
        }
        let (authorized, paired) = tokio::join!(offer.authorize(None), async {
            match shown.wait().await.unwrap() {
                HeldPhoneSignIn::Guest(held) => held.confirm().await.unwrap(),
                other => panic!("not a guest: {other:?}"),
            }
        });
        authorized.unwrap();
        assert_eq!(paired.identity, identity);
        // The code is spent: no phone signs in with it now.
        assert!(phone.scan_phone_sign_in(shown.link()).await.is_err());
    })
    .await;
}

#[tokio::test]
async fn an_unused_code_expires() {
    within(async {
        let tv = tv().await;
        let shown = tv
            .start_phone_sign_in_for("TV", false, Duration::from_millis(100))
            .await
            .unwrap();
        assert_eq!(pairing_error(shown.wait().await), PairingError::Expired);
        assert_eq!(
            pairing_error(shown.connected().await),
            PairingError::Expired
        );
    })
    .await;
}

/// The code names a relay only when the app asked, and only the TV's
/// own home relay.
#[test]
fn the_code_names_a_relay_only_when_asked() {
    let home = "https://relay.example.org/".to_owned();
    assert_eq!(link_relays(false, Some(home.clone())), Vec::<String>::new());
    assert_eq!(link_relays(true, Some(home.clone())), [home]);
    assert_eq!(link_relays(true, None), Vec::<String>::new());
}

#[tokio::test]
async fn the_link_has_no_relay_unless_asked() {
    within(async {
        let tv = tv().await;
        let shown = tv.start_phone_sign_in("TV", false).await.unwrap();
        assert!(parse_link(shown.link()).unwrap().relays.is_empty());
        assert!(!shown.link().contains("&r="));
    })
    .await;
}

/// A phone that goes before sending its offers leaves the TV's code
/// ended, and nothing signed in.
#[tokio::test]
async fn a_phone_that_hangs_up_signs_nothing_in() {
    within(async {
        let tv = tv().await;
        let shown = tv.start_phone_sign_in("TV", false).await.unwrap();
        let phone = phone().await;
        let offer = phone.scan_phone_sign_in(shown.link()).await.unwrap();
        drop(offer);
        assert!(matches!(
            pairing_error(shown.wait().await),
            PairingError::Aborted(_)
        ));
        assert!(tv.servers().await.unwrap().is_empty());
    })
    .await;
}

/// Each server error a sign-in meets reads as the failure the app shows.
#[test]
fn a_server_s_error_reads_as_its_failure() {
    use spoolframe_protocol::features::Feature;
    let cases = [
        (
            SdkError::RateLimited {
                retry_after: Duration::from_secs(300),
            },
            SignInFailure::RateLimited,
        ),
        (SdkError::ServerFull, SignInFailure::RateLimited),
        (
            SdkError::RecentSignInRequired,
            SignInFailure::RecentSignInRequired,
        ),
        (SdkError::NotForPortableUsers, SignInFailure::NotAllowed),
        (SdkError::AdminRequired, SignInFailure::NotAllowed),
        (SdkError::NotAllowed, SignInFailure::NotAllowed),
        (
            SdkError::FeatureOff {
                feature: Feature::PhoneSignIn,
            },
            SignInFailure::FeatureOff,
        ),
        (
            SdkError::NotOnServer {
                request: "offer_device_sign_in".into(),
            },
            SignInFailure::FeatureOff,
        ),
        (
            SdkError::Unsupported {
                feature: "x".into(),
            },
            SignInFailure::FeatureOff,
        ),
        (
            SdkError::Unreachable("home".into()),
            SignInFailure::Unreachable,
        ),
        (
            SdkError::TimedOut("home".into()),
            SignInFailure::Unreachable,
        ),
        (SdkError::SignInExpired, SignInFailure::Expired),
        (
            SdkError::Pairing(PairingError::Declined),
            SignInFailure::Declined,
        ),
        (
            SdkError::Pairing(PairingError::Rejected),
            SignInFailure::Declined,
        ),
        (
            SdkError::Server("the moon is in the way".into()),
            SignInFailure::Refused(SdkError::Server("the moon is in the way".into()).to_string()),
        ),
    ];
    for (error, failure) in cases {
        assert_eq!(SignInFailure::of(&error), failure, "{error:?}");
    }
}

/// What the TV says crosses the wire as the same failure; a refusal's
/// words with it, and a kind from a newer TV (or none, from an older
/// one) reads as refused with the TV's words.
#[test]
fn a_tv_s_failure_crosses_the_wire_whole() {
    for failure in [
        SignInFailure::RateLimited,
        SignInFailure::RecentSignInRequired,
        SignInFailure::NotAllowed,
        SignInFailure::FeatureOff,
        SignInFailure::Unreachable,
        SignInFailure::Expired,
        SignInFailure::Refused("no thanks".into()),
    ] {
        let OfferOutcome::Failed { reason, kind } =
            OfferOutcome::failed(failure.kind(), &failure.to_string())
        else {
            panic!("a failure");
        };
        assert_eq!(SignInFailure::from_wire(kind, reason), failure);
    }
    assert_eq!(
        SignInFailure::from_wire(FailureKind::Unknown, "old words".into()),
        SignInFailure::Refused("old words".into())
    );
}

/// A server saved on `storage`, verified, with its server id pinned:
/// one [`SignInOffer::servers`] lists.
async fn save_server(storage: &MemoryStorage, n: u8) -> EndpointId {
    let endpoint = iroh::SecretKey::from_bytes(&[n; 32]).public();
    let mut saved = crate::servers::load(storage).await.unwrap();
    saved.push(crate::servers::SavedServer::pinned_now(
        format!("Server {n}"),
        endpoint.to_string(),
        format!("server-{n}"),
        vec!["127.0.0.1:9".parse().unwrap()],
        true,
    ));
    crate::servers::save(storage, &saved).await.unwrap();
    endpoint
}

/// A profile kept for `username` whose session the server ended.
fn ended(username: &str) -> crate::session::StoredSession {
    crate::session::StoredSession {
        token: None,
        username: username.into(),
        display_name: username.to_uppercase(),
        method: Some(crate::session::SignInMethod::Password),
        portable: None,
        admin: false,
    }
}

/// One server's kept profiles that cannot be read lose that server's
/// choices only: the other server's are still listed, and the list is
/// not an error.
#[tokio::test]
async fn one_unreadable_server_leaves_the_others_listed() {
    within(async {
        let tv = tv().await;
        let shown = tv.start_phone_sign_in("TV", false).await.unwrap();
        let storage = Arc::new(MemoryStorage::default());
        let broken = save_server(&storage, 1).await;
        let fine = save_server(&storage, 2).await;
        storage
            .set_secret(
                &crate::storage::keys::sessions(&broken.to_string()),
                Some(b"not a list of sessions".to_vec()),
            )
            .await
            .unwrap();
        crate::session::save(storage.as_ref(), &fine, &[ended("alice")])
            .await
            .unwrap();
        let phone = open_on(storage, "phone", Platform::Ios).await;
        let offer = phone.scan_phone_sign_in(shown.link()).await.unwrap();
        let choices = offer.servers().await.unwrap();
        assert_eq!(choices.len(), 1, "{choices:?}");
        assert_eq!(choices[0].server.endpoint_id, fine.to_string());
        assert!(choices[0].session_ended());
        assert_eq!(choices[0].username(), "alice");
        offer.decline().await.unwrap();
    })
    .await;
}

fn card() -> PairedServer {
    PairedServer {
        endpoint_id: "e".into(),
        server_id: "s".into(),
        name: "Home".into(),
        addrs: Vec::new(),
        relays: Vec::new(),
    }
}

/// A live session for `username` on a server, as `Client::profiles`
/// gives one, held by `client`.
fn live(client: &Client, username: &str) -> Session {
    Session {
        client: client.clone(),
        server: iroh::SecretKey::from_bytes(&[5; 32]).public(),
        token: spoolframe_protocol::SessionId(format!("token-{username}")),
        user: spoolframe_protocol::UserSummary {
            user_id: format!("local:{username}"),
            username: username.into(),
            display_name: username.to_uppercase(),
            role: spoolframe_types::Role::Consumer,
            locked: false,
            pin: None,
            portable: Vec::new(),
        },
        can_leave: true,
        remembered: None,
        password_change_required: Arc::default(),
    }
}

/// A profile kept with the session [`live`] makes for `username`.
fn kept(username: &str) -> crate::session::StoredSession {
    crate::session::StoredSession {
        token: Some(format!("token-{username}")),
        ..ended(username)
    }
}

fn listed(choices: &[SignInChoice]) -> Vec<(String, bool)> {
    choices
        .iter()
        .map(|c| (c.username().to_owned(), c.session_ended()))
        .collect()
}

/// A profile answered for live whose token the later read finds gone
/// (the server signed the phone out while another server was still
/// asked) is one choice, `SessionEnded`: not a live one that no longer
/// works, and not both.
#[tokio::test]
async fn a_session_that_ended_after_answering_is_listed_ended() {
    let phone = phone().await;
    let stored = [ended("alice"), ended("bob"), kept("carol")];
    let choices = server_choices(
        &card(),
        vec![live(&phone, "alice"), live(&phone, "carol")],
        &stored,
        PhoneSignInSupport::On,
    );
    assert_eq!(
        listed(&choices),
        [
            ("carol".to_owned(), false),
            ("alice".to_owned(), true),
            ("bob".to_owned(), true)
        ]
    );
}

/// A session the server answered for but that this device no longer
/// keeps, forgotten here (`forget_profile`) or replaced by another
/// sign-in as that user, is not listed live: its profile is gone, or
/// another session now stands for it.
#[tokio::test]
async fn a_session_forgotten_or_replaced_after_answering_is_not_listed() {
    let phone = phone().await;
    let replaced = crate::session::StoredSession {
        token: Some("a-newer-token".into()),
        ..ended("bob")
    };
    let choices = server_choices(
        &card(),
        vec![
            live(&phone, "alice"),
            live(&phone, "bob"),
            live(&phone, "carol"),
        ],
        &[replaced, kept("carol")],
        PhoneSignInSupport::On,
    );
    assert_eq!(listed(&choices), [("carol".to_owned(), false)]);
}

/// A server this phone has heard say it takes no phone sign-ins lists
/// nobody, ended profiles included: signing in again there could never
/// sign the TV in. One not heard from still lists its ended profiles.
#[tokio::test]
async fn a_server_without_phone_sign_in_lists_no_ended_profile() {
    let phone = phone().await;
    let stored = [ended("bob")];
    let off = server_choices(
        &card(),
        vec![live(&phone, "alice")],
        &stored,
        PhoneSignInSupport::Off,
    );
    assert!(off.is_empty(), "{off:?}");
    let unknown = server_choices(&card(), Vec::new(), &stored, PhoneSignInSupport::Unknown);
    assert_eq!(listed(&unknown), [("bob".to_owned(), true)]);
}

/// A guest's ended profile, kept under its identity's id, is not listed
/// as "Sign in again", as its live session is not; a local account tied
/// to an identity, which keeps its own username, is.
#[test]
fn a_guest_s_ended_profile_is_not_listed() {
    let id: spoolframe_protocol::portable::PortableUserId =
        "610b431103deb4f932b5a063abf7d0f5d3fdcb63a6ae542283d6d4e0fd737111"
            .parse()
            .unwrap();
    let guest = crate::session::StoredSession {
        username: id.to_string(),
        portable: Some(id),
        method: Some(crate::session::SignInMethod::Portable),
        ..ended("guest")
    };
    let tied = crate::session::StoredSession {
        portable: Some(id),
        method: Some(crate::session::SignInMethod::Portable),
        ..ended("carol")
    };
    let choices = server_choices(&card(), Vec::new(), &[guest, tied], PhoneSignInSupport::On);
    assert_eq!(listed(&choices), [("carol".to_owned(), true)]);
}

/// The TV reads its own redemptions with [`SignInFailure::of`]: a TV
/// told to sign in is not the phone's session ending, so it stays a
/// refusal there. Only the phone's offer says `SessionEnded`. The race
/// that gives the TV `SignInRequired` (a token revoked between its
/// redemption and its `WhoAmI`) is not one a test can time, so the two
/// classifiers are checked directly.
#[test]
fn only_the_phone_s_offer_reads_sign_in_required_as_session_ended() {
    assert!(matches!(
        SignInFailure::of(&SdkError::SignInRequired),
        SignInFailure::Refused(_)
    ));
    assert_eq!(
        SignInFailure::of_offer(&SdkError::SignInRequired),
        SignInFailure::SessionEnded
    );
    assert_eq!(
        SignInFailure::of_offer(&SdkError::RecentSignInRequired),
        SignInFailure::RecentSignInRequired
    );
}
