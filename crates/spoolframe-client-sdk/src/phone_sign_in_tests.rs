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
use crate::storage::MemoryStorage;

/// No test takes this long unless something hangs.
const HANG: Duration = Duration::from_secs(60);

async fn within<T>(test: impl Future<Output = T>) -> T {
    tokio::time::timeout(HANG, test)
        .await
        .expect("the test hung")
}

async fn open(name: &str, platform: Platform) -> Client {
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
    Client::open_with_options(Arc::new(MemoryStorage::default()), device, options)
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
        offer.decline().await.unwrap();
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
