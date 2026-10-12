//! Pairing (P7-03) between two clients on this machine: a TV that shows a
//! code and a phone that holds an identity's root. No relays, no LAN
//! discovery: the phone dials the addresses the code carries.

use std::future::Future;

use spoolframe_protocol::portable::verify_device_proof;
use spoolframe_types::{ClientCapabilities, Platform};

use super::*;
use crate::client::{ClientOptions, Device};
use crate::portable::GuestPairing;
use crate::storage::{MemoryStorage, Storage, keys};

/// No test takes this long unless something hangs.
const HANG: Duration = Duration::from_secs(60);
const SERVER: [u8; 32] = [0x5a; 32];
const NONCE: [u8; 32] = [0xa5; 32];

async fn within<T>(test: impl Future<Output = T>) -> T {
    tokio::time::timeout(HANG, test)
        .await
        .expect("the test hung")
}

async fn open(name: &str) -> (Arc<MemoryStorage>, Client) {
    let storage = Arc::new(MemoryStorage::default());
    let device = Device {
        name: name.into(),
        client_name: "sdk-pairing-test".into(),
        client_version: "0".into(),
        platform: Platform::Cli,
        capabilities: ClientCapabilities::default(),
    };
    let options = ClientOptions {
        lan_discovery: false,
        relays: false,
        ..ClientOptions::default()
    };
    let client = Client::open_with_options(storage.clone(), device, options)
        .await
        .unwrap();
    (storage, client)
}

/// A TV whose owner allows guests.
async fn tv() -> (Arc<MemoryStorage>, Client) {
    let (storage, tv) = open("tv").await;
    tv.portable().set_allow_guests(true).await.unwrap();
    (storage, tv)
}

/// A phone holding a new identity's root.
async fn holder(name: &str) -> (Client, PortableUserId) {
    let (_, phone) = open("phone").await;
    let (id, _) = phone.portable().create(name).await.unwrap();
    (phone, id)
}

async fn device_key_stored(storage: &MemoryStorage, id: PortableUserId) -> bool {
    storage
        .secret(&keys::portable_device(&id.to_string()))
        .await
        .unwrap()
        .is_some()
}

fn pairing_error(result: Result<impl std::fmt::Debug, SdkError>) -> PairingError {
    match result {
        Err(SdkError::Pairing(e)) => e,
        other => panic!("not a pairing error: {other:?}"),
    }
}

/// Pair `tv` with `phone`'s identity `id`, start to end.
async fn pair(tv: &Client, phone: &Client, id: PortableUserId) -> Paired {
    let pairing = tv.start_pairing("Living room TV").await.unwrap();
    let offer = phone.scan_pairing(pairing.link(), id).await.unwrap();
    offer.authorize(None).await.unwrap();
    pairing.wait().await.unwrap()
}

#[tokio::test]
async fn a_guest_pairs_by_qr_and_the_device_proves_as_them() {
    within(async {
        let (_, tv) = tv().await;
        let (phone, id) = holder("Steve").await;
        let pairing = tv.start_pairing("Living room TV").await.unwrap();
        assert!(pairing.link().starts_with("spoolframe://pair?e="));
        assert!(pairing.expires_in() <= CODE_TTL);

        let offer = phone.scan_pairing(pairing.link(), id).await.unwrap();
        assert_eq!(offer.device_name(), "Living room TV");
        assert_eq!(offer.identity(), id);
        assert_eq!(offer.fingerprint(), offer.device_key().fingerprint());
        assert_eq!(offer.fingerprint().len(), 19);
        offer.authorize(None).await.unwrap();

        let paired = pairing.wait().await.unwrap();
        let guest = tv.portable();
        assert_eq!(paired.identity, id);
        assert_eq!(paired.device_key, offer.device_key());
        assert_eq!(guest.device_key(id).await.unwrap(), paired.device_key);
        let credential = guest.credential(id).await.unwrap().unwrap();
        assert_eq!(credential.device_key, paired.device_key);
        let now = i64::try_from(crate::servers::now()).unwrap();
        assert_eq!(credential.verify(now), Ok(()));
        assert_eq!(guest.guests().await.unwrap(), [id]);

        let issued = phone.portable().issued(id).await.unwrap();
        assert_eq!(issued.len(), 1);
        assert_eq!(issued[0].device_key, paired.device_key);
        assert_eq!(issued[0].label.as_deref(), Some("Living room TV"));

        let (sent, signature) = guest.prove(id, &SERVER, &NONCE).await.unwrap();
        assert_eq!(sent, credential);
        assert_eq!(
            verify_device_proof(&sent, &SERVER, &NONCE, &signature),
            Ok(())
        );
    })
    .await;
}

#[tokio::test]
async fn the_device_shows_the_fingerprint_the_phone_does_before_authorizing() {
    within(async {
        let (storage, tv) = tv().await;
        let (phone, id) = holder("Steve").await;
        let pairing = tv.start_pairing("Living room TV").await.unwrap();
        // Waiting from before the phone dials, as a TV would.
        let (connected, offer) =
            tokio::join!(pairing.connected(), phone.scan_pairing(pairing.link(), id));
        let (connected, offer) = (connected.unwrap(), offer.unwrap());
        assert_eq!(connected.identity, id);
        assert_eq!(connected.device_key, offer.device_key());
        assert_eq!(connected.fingerprint, offer.fingerprint());
        // Nothing authorized yet.
        assert!(device_key_stored(&storage, id).await);
        assert_eq!(tv.portable().credential(id).await.unwrap(), None);

        offer.authorize(None).await.unwrap();
        let paired = pairing.wait().await.unwrap();
        assert_eq!(paired.fingerprint, connected.fingerprint);
        assert_eq!(paired.device_key, connected.device_key);
        // After the end, the same phone, and the same end again.
        assert_eq!(pairing.connected().await.unwrap(), connected);
        assert_eq!(pairing.wait().await.unwrap(), paired);
    })
    .await;
}

#[tokio::test]
async fn connected_is_the_end_s_error_when_no_phone_comes() {
    within(async {
        let (_, tv) = tv().await;
        let pairing = tv
            .start_pairing_for("TV", Duration::from_millis(100))
            .await
            .unwrap();
        assert_eq!(
            pairing_error(pairing.connected().await),
            PairingError::Expired
        );
        assert_eq!(pairing_error(pairing.wait().await), PairingError::Expired);
        assert_eq!(
            pairing_error(pairing.connected().await),
            PairingError::Expired
        );
    })
    .await;
}

#[tokio::test]
async fn connected_is_aborted_when_cancelled_without_a_phone() {
    within(async {
        let (_, tv) = tv().await;
        let pairing = tv.start_pairing("TV").await.unwrap();
        let (connected, ()) = tokio::join!(pairing.connected(), async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            pairing.cancel();
        });
        assert!(matches!(pairing_error(connected), PairingError::Aborted(_)));
        assert!(matches!(
            pairing_error(pairing.wait().await),
            PairingError::Aborted(_)
        ));
    })
    .await;
}

#[tokio::test]
async fn connected_is_the_end_s_error_after_a_wrong_code() {
    within(async {
        let (_, tv) = tv().await;
        let (phone, id) = holder("Steve").await;
        let pairing = tv.start_pairing("TV").await.unwrap();
        let mut link = pairing::parse_link(pairing.link()).unwrap();
        link.code = PairingCode::from_bytes([9; 32]);
        phone.scan_pairing(&link.link(), id).await.unwrap_err();
        assert_eq!(
            pairing_error(pairing.connected().await),
            PairingError::WrongCode
        );
        assert_eq!(pairing_error(pairing.wait().await), PairingError::WrongCode);
    })
    .await;
}

#[tokio::test]
async fn a_phone_that_connected_stays_connected_after_declining() {
    within(async {
        let (_, tv) = tv().await;
        let (phone, id) = holder("Steve").await;
        let pairing = tv.start_pairing("TV").await.unwrap();
        let offer = phone.scan_pairing(pairing.link(), id).await.unwrap();
        offer.decline().await.unwrap();
        assert_eq!(pairing_error(pairing.wait().await), PairingError::Declined);
        let connected = pairing.connected().await.unwrap();
        assert_eq!(connected.identity, id);
        assert_eq!(connected.fingerprint, offer.fingerprint());
    })
    .await;
}

#[tokio::test]
async fn the_label_is_the_phone_s_when_it_gives_one() {
    within(async {
        let (_, tv) = tv().await;
        let (phone, id) = holder("Steve").await;
        let pairing = tv.start_pairing("Living room TV").await.unwrap();
        let offer = phone.scan_pairing(pairing.link(), id).await.unwrap();
        offer.authorize(Some("Mum's TV")).await.unwrap();
        pairing.wait().await.unwrap();
        let issued = phone.portable().issued(id).await.unwrap();
        assert_eq!(issued[0].label.as_deref(), Some("Mum's TV"));
    })
    .await;
}

#[tokio::test]
async fn guests_are_off_by_default_and_no_code_is_shown() {
    within(async {
        let (_, tv) = open("tv").await;
        assert!(!tv.portable().allow_guests().await.unwrap());
        assert_eq!(
            pairing_error(tv.start_pairing("TV").await),
            PairingError::NotAllowed
        );
    })
    .await;
}

#[tokio::test]
async fn a_device_with_guests_off_takes_no_pairing_connection() {
    within(async {
        let (storage, tv) = open("tv").await;
        let (phone, id) = holder("Steve").await;
        // A code made by hand for the TV: it never showed one.
        let link = PairingLink {
            endpoint: *tv.inner.endpoint.id().as_bytes(),
            code: PairingCode::from_bytes([3; 32]),
            name: "TV".into(),
            addrs: tv.pairing_addrs(),
            relays: Vec::new(),
        }
        .link();
        let err = phone.scan_pairing(&link, id).await.unwrap_err();
        assert!(matches!(err, SdkError::Unreachable(_)), "{err:?}");
        assert!(!device_key_stored(&storage, id).await);
        assert_eq!(tv.portable().credential(id).await.unwrap(), None);
    })
    .await;
}

#[tokio::test]
async fn guests_turned_off_while_a_code_shows_refuse_before_any_key() {
    within(async {
        let (storage, tv) = tv().await;
        let (phone, id) = holder("Steve").await;
        let pairing = tv.start_pairing("TV").await.unwrap();
        tv.portable().set_allow_guests(false).await.unwrap();
        assert_eq!(
            pairing_error(phone.scan_pairing(pairing.link(), id).await),
            PairingError::NotAllowed
        );
        assert_eq!(
            pairing_error(pairing.wait().await),
            PairingError::NotAllowed
        );
        assert!(!device_key_stored(&storage, id).await);
    })
    .await;
}

#[tokio::test]
async fn a_wrong_code_ends_the_pairing_with_nothing_installed() {
    within(async {
        let (storage, tv) = tv().await;
        let (phone, id) = holder("Steve").await;
        let pairing = tv.start_pairing("TV").await.unwrap();
        let mut link = pairing::parse_link(pairing.link()).unwrap();
        link.code = PairingCode::from_bytes([9; 32]);
        assert_eq!(
            pairing_error(phone.scan_pairing(&link.link(), id).await),
            PairingError::WrongCode
        );
        assert_eq!(pairing_error(pairing.wait().await), PairingError::WrongCode);
        assert!(!device_key_stored(&storage, id).await);
        assert_eq!(tv.portable().credential(id).await.unwrap(), None);
        // The right code no longer works either.
        assert!(phone.scan_pairing(pairing.link(), id).await.is_err());
    })
    .await;
}

#[tokio::test]
async fn an_expired_code_ends_the_pairing_and_no_longer_connects() {
    within(async {
        let (storage, tv) = tv().await;
        let (phone, id) = holder("Steve").await;
        let pairing = tv
            .start_pairing_for("TV", Duration::from_millis(100))
            .await
            .unwrap();
        assert_eq!(pairing_error(pairing.wait().await), PairingError::Expired);
        assert_eq!(pairing.expires_in(), Duration::ZERO);
        assert!(phone.scan_pairing(pairing.link(), id).await.is_err());
        assert!(!device_key_stored(&storage, id).await);
    })
    .await;
}

#[tokio::test]
async fn a_code_used_once_does_not_pair_again() {
    within(async {
        let (_, tv) = tv().await;
        let (phone, steve) = holder("Steve").await;
        let pairing = tv.start_pairing("TV").await.unwrap();
        let link = pairing.link().to_owned();
        let offer = phone.scan_pairing(&link, steve).await.unwrap();
        offer.authorize(None).await.unwrap();
        pairing.wait().await.unwrap();

        let (other, ann) = holder("Ann").await;
        assert!(other.scan_pairing(&link, ann).await.is_err());
        assert_eq!(tv.portable().guests().await.unwrap(), [steve]);
    })
    .await;
}

#[tokio::test]
async fn a_declined_pairing_installs_nothing() {
    within(async {
        let (storage, tv) = tv().await;
        let (phone, id) = holder("Steve").await;
        let pairing = tv.start_pairing("TV").await.unwrap();
        let offer = phone.scan_pairing(pairing.link(), id).await.unwrap();
        offer.decline().await.unwrap();
        assert_eq!(pairing_error(pairing.wait().await), PairingError::Declined);
        assert_eq!(tv.portable().credential(id).await.unwrap(), None);
        assert!(!device_key_stored(&storage, id).await);
        assert_eq!(phone.portable().issued(id).await.unwrap(), []);
        // Answered once only.
        assert!(offer.authorize(None).await.is_err());
    })
    .await;
}

#[tokio::test]
async fn a_key_the_device_had_before_a_failed_pairing_stays() {
    within(async {
        let (_, tv) = tv().await;
        let (phone, id) = holder("Steve").await;
        let before = tv.portable().device_key(id).await.unwrap();
        let pairing = tv.start_pairing("TV").await.unwrap();
        let offer = phone.scan_pairing(pairing.link(), id).await.unwrap();
        assert_eq!(offer.device_key(), before);
        offer.decline().await.unwrap();
        pairing.wait().await.unwrap_err();
        assert_eq!(tv.portable().device_key(id).await.unwrap(), before);
    })
    .await;
}

#[tokio::test]
async fn a_phone_that_goes_away_part_way_leaves_nothing() {
    within(async {
        let (storage, tv) = tv().await;
        let (phone, id) = holder("Steve").await;
        let pairing = tv.start_pairing("TV").await.unwrap();
        let offer = phone.scan_pairing(pairing.link(), id).await.unwrap();
        drop(offer);
        assert!(matches!(
            pairing_error(pairing.wait().await),
            PairingError::Aborted(_)
        ));
        assert_eq!(tv.portable().credential(id).await.unwrap(), None);
        assert!(!device_key_stored(&storage, id).await);
    })
    .await;
}

#[tokio::test]
async fn a_phone_without_the_root_sends_nothing() {
    within(async {
        let (storage, tv) = tv().await;
        let (_, phone) = open("phone").await;
        let (_, someone) = holder("Someone").await;
        let pairing = tv.start_pairing("TV").await.unwrap();
        assert_eq!(
            pairing_error(phone.scan_pairing(pairing.link(), someone).await),
            PairingError::NoRoot
        );
        // The TV heard nothing: its code still waits, and no key was made.
        assert!(pairing.expires_in() > Duration::ZERO);
        assert!(!device_key_stored(&storage, someone).await);
        pairing.cancel();
        assert!(pairing.wait().await.is_err());
    })
    .await;
}

#[tokio::test]
async fn a_root_gone_before_authorizing_is_refused_and_nothing_installed() {
    within(async {
        let (storage, tv) = tv().await;
        let (phone, id) = holder("Steve").await;
        let pairing = tv.start_pairing("TV").await.unwrap();
        let offer = phone.scan_pairing(pairing.link(), id).await.unwrap();
        phone.portable().forget(id).await.unwrap();
        assert_eq!(
            pairing_error(offer.authorize(None).await),
            PairingError::NoRoot
        );
        assert!(matches!(
            pairing_error(pairing.wait().await),
            PairingError::Aborted(_)
        ));
        assert_eq!(tv.portable().credential(id).await.unwrap(), None);
        assert!(!device_key_stored(&storage, id).await);
    })
    .await;
}

#[tokio::test]
async fn one_pairing_at_a_time_and_cancelling_frees_the_next() {
    within(async {
        let (_, tv) = tv().await;
        let first = tv.start_pairing("TV").await.unwrap();
        assert_eq!(
            pairing_error(tv.start_pairing("TV").await),
            PairingError::AlreadyPairing
        );
        drop(first);
        // The task lets go of the endpoint as it ends.
        let mut second = tv.start_pairing("TV").await;
        for _ in 0..50 {
            if second.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            second = tv.start_pairing("TV").await;
        }
        second.unwrap();
    })
    .await;
}

#[tokio::test]
async fn two_guests_keep_apart_and_one_signs_out_alone() {
    within(async {
        let (_, tv) = tv().await;
        let (steve_phone, steve) = holder("Steve").await;
        let (ann_phone, ann) = holder("Ann").await;
        let steve_paired = pair(&tv, &steve_phone, steve).await;
        let ann_paired = pair(&tv, &ann_phone, ann).await;
        assert_ne!(steve_paired.device_key, ann_paired.device_key);

        let guests = tv.portable();
        let mut listed = guests.guests().await.unwrap();
        listed.sort();
        let mut both = vec![steve, ann];
        both.sort();
        assert_eq!(listed, both);
        guests.prove(steve, &SERVER, &NONCE).await.unwrap();
        guests.prove(ann, &SERVER, &NONCE).await.unwrap();

        guests.forget_guest(steve).await.unwrap();
        assert_eq!(
            guests.prove(steve, &SERVER, &NONCE).await.unwrap_err(),
            SdkError::UnknownIdentity(steve.to_string())
        );
        guests.prove(ann, &SERVER, &NONCE).await.unwrap();
        assert_eq!(guests.guests().await.unwrap(), [ann]);
    })
    .await;
}

#[tokio::test]
async fn an_identity_whose_root_is_here_is_not_forgotten_as_a_guest() {
    let (phone, id) = holder("Steve").await;
    assert!(matches!(
        phone.portable().forget_guest(id).await,
        Err(SdkError::InvalidInput(_))
    ));
    assert_eq!(phone.portable().list().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_guest_setting_that_cannot_be_read_is_an_error_not_on() {
    let storage = Arc::new(MemoryStorage::default());
    storage
        .set_data(keys::ALLOW_GUESTS, Some(b"maybe".to_vec()))
        .await
        .unwrap();
    let portable = Portable::new(storage);
    assert!(matches!(
        portable.allow_guests().await,
        Err(SdkError::Storage(_))
    ));
}

#[tokio::test]
async fn text_that_is_not_a_pairing_code_is_refused_before_dialling() {
    let (phone, id) = holder("Steve").await;
    assert!(matches!(
        pairing_error(
            phone
                .scan_pairing("spoolframe://invite?server=ab", id)
                .await
        ),
        PairingError::NotALink(LinkError::NotALink)
    ));
    let long = format!("spoolframe://pair?x={}", "a".repeat(pairing::MAX_LINK));
    assert_eq!(
        pairing_error(phone.scan_pairing(&long, id).await),
        PairingError::NotALink(LinkError::TooLong)
    );
}

/// What the log gets, as text.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Captured {
        self.clone()
    }
}

/// Marks the copy of a test that [`alone`] runs.
const ALONE: &str = "SPOOLFRAME_TEST_ALONE";

/// Run `test` again in a process where no other test runs, and assert it
/// passed there; `true` in that copy, which runs the body. As the
/// server's tests do (`support::alone`): tracing decides once per call
/// site, for every thread, whether its lines are wanted, and a call site
/// another test reached first, with no subscriber, stays off for this
/// test's too.
fn alone(test: &str) -> bool {
    if std::env::var_os(ALONE).is_some() {
        return true;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(ALONE, "1")
        .output()
        .unwrap();
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{test} alone:\n{log}");
    assert!(log.contains("1 passed"), "{test} did not run:\n{log}");
    false
}

/// Finding 46's rules on the device: a refused pairing is one WARN line
/// on the refusal target, with why, and never the code.
#[tokio::test]
async fn a_refused_pairing_is_logged_once_without_the_code() {
    if !alone("pairing::tests::a_refused_pairing_is_logged_once_without_the_code") {
        return;
    }
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .finish();
    // Tests run on one thread: the tasks the pairing spawns log here too.
    let _guard = tracing::subscriber::set_default(subscriber);
    within(async {
        let (_, off) = open("phone").await;
        off.start_pairing("Phone").await.unwrap_err();

        let (_, tv) = tv().await;
        let (phone, id) = holder("Steve").await;
        let pairing = tv.start_pairing("TV").await.unwrap();
        let shown = pairing::parse_link(pairing.link()).unwrap().code;
        let mut link = pairing::parse_link(pairing.link()).unwrap();
        link.code = PairingCode::from_bytes([9; 32]);
        phone.scan_pairing(&link.link(), id).await.unwrap_err();
        pairing.wait().await.unwrap_err();

        let log = String::from_utf8(captured.0.lock().clone()).unwrap();
        let refused: Vec<_> = log
            .lines()
            .filter(|l| l.contains(refusals::TARGET))
            .collect();
        assert_eq!(refused.len(), 2, "{refused:#?}");
        assert!(refused.iter().all(|l| l.contains("WARN")), "{log}");
        assert!(refused[0].contains("guests are not allowed"), "{log}");
        assert!(refused[1].contains("the code does not match"), "{log}");
        assert!(refused[1].contains(&id.to_string()), "{log}");
        for code in [shown, link.code] {
            assert!(!log.contains(&code.to_text()), "{log}");
        }
    })
    .await;
}

/// Keep server `endpoint` (with server id `server_id`, at `addr`,
/// `verified` or not) on `storage`, and a session on it signed in as
/// portable identity `id` if `Some`.
async fn saved_with_session(
    storage: &MemoryStorage,
    endpoint: &str,
    server_id: &str,
    addr: &str,
    verified: bool,
    id: Option<PortableUserId>,
) {
    let mut saved = crate::servers::load(storage).await.unwrap();
    saved.push(crate::servers::SavedServer::pinned_now(
        "Home Media".into(),
        endpoint.into(),
        server_id.into(),
        vec![addr.parse().unwrap()],
        verified,
    ));
    crate::servers::save(storage, &saved).await.unwrap();
    let server: EndpointId = endpoint.parse().unwrap();
    crate::session::save(
        storage,
        &server,
        &[crate::session::StoredSession {
            token: Some("t".into()),
            username: "u".into(),
            display_name: "U".into(),
            method: Some(crate::session::SignInMethod::Portable),
            portable: id,
        }],
    )
    .await
    .unwrap();
}

/// P7-06: the phone hands the servers it signs in to as the identity and
/// has verified, and only those. The TV pins them unverified (the guest
/// vouches, not its owner), leaves a pin it had exactly as it was, and
/// leaves out one whose server id it has saved under another key.
#[tokio::test]
async fn the_phone_hands_the_identity_s_servers_and_the_tv_saves_them() {
    within(async {
        let (tv_storage, tv) = tv().await;
        let (phone_storage, phone) = open("phone").await;
        let (id, _) = phone.portable().create("Steve").await.unwrap();
        let (other, _) = phone.portable().create("Someone").await.unwrap();
        let key = |n: u8| iroh::SecretKey::from_bytes(&[n; 32]).public().to_string();
        let (home, friend, theirs, clash, unchecked) = (key(1), key(2), key(3), key(4), key(6));
        let phone_at = "127.0.0.1:7420";
        saved_with_session(&phone_storage, &home, "home-id", phone_at, true, Some(id)).await;
        saved_with_session(
            &phone_storage,
            &friend,
            "friend-id",
            phone_at,
            true,
            Some(id),
        )
        .await;
        saved_with_session(
            &phone_storage,
            &theirs,
            "theirs-id",
            phone_at,
            true,
            Some(other),
        )
        .await;
        saved_with_session(&phone_storage, &clash, "clash-id", phone_at, true, Some(id)).await;
        saved_with_session(
            &phone_storage,
            &unchecked,
            "unchecked-id",
            phone_at,
            false,
            Some(id),
        )
        .await;
        // The TV has "clash-id" saved already under another key, and
        // friend's key saved, unverified, at another address.
        saved_with_session(
            &tv_storage,
            &key(5),
            "clash-id",
            "10.0.0.5:7420",
            true,
            None,
        )
        .await;
        saved_with_session(
            &tv_storage,
            &friend,
            "friend-id",
            "10.0.0.2:7420",
            false,
            None,
        )
        .await;

        let paired = pair(&tv, &phone, id).await;
        let mut handed: Vec<_> = paired
            .servers
            .iter()
            .map(|s| s.endpoint_id.clone())
            .collect();
        handed.sort();
        let mut expected = [home.clone(), friend.clone()];
        expected.sort();
        assert_eq!(handed, expected);
        let saved = tv.servers().await.unwrap();
        let entry = |key: &str| saved.iter().find(|s| s.endpoint_id == key).unwrap().clone();
        let new = entry(&home);
        assert!(new.pinned && !new.verified, "{new:?}");
        assert!(new.addrs.contains(&phone_at.parse().unwrap()));
        let had = entry(&friend);
        assert!(had.pinned && !had.verified, "{had:?}");
        assert!(!had.addrs.contains(&phone_at.parse().unwrap()), "{had:?}");
        assert!(
            !saved
                .iter()
                .any(|s| [&theirs, &clash, &unchecked].contains(&&s.endpoint_id))
        );
    })
    .await;
}

/// #6: a guest paired by QR is listed with the server its phone handed
/// first, and when: what a "Shared computer" row shows. Forgetting the
/// guest takes the record too.
#[tokio::test]
async fn a_guest_paired_by_qr_is_listed_with_its_server_and_when() {
    within(async {
        let (tv_storage, tv) = tv().await;
        let (phone_storage, phone) = open("phone").await;
        let (id, _) = phone.portable().create("Steve").await.unwrap();
        let home = iroh::SecretKey::from_bytes(&[1; 32]).public().to_string();
        saved_with_session(
            &phone_storage,
            &home,
            "home-id",
            "127.0.0.1:7420",
            true,
            Some(id),
        )
        .await;
        let before = crate::servers::now();
        pair(&tv, &phone, id).await;

        let listed = tv.portable().guest_pairings().await.unwrap();
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].id, id);
        assert_eq!(
            listed[0].server,
            Some(GuestServer {
                endpoint_id: home,
                name: "Home Media".into(),
            })
        );
        let at = listed[0].paired_at.expect("when it was paired");
        assert!(at >= before && at <= crate::servers::now(), "{at}");

        tv.portable().forget_guest(id).await.unwrap();
        assert!(tv.portable().guest_pairings().await.unwrap().is_empty());
        assert_eq!(
            tv_storage
                .data(&keys::portable_paired(&id.to_string()))
                .await
                .unwrap(),
            None
        );
    })
    .await;
}

/// #6: a phone that hands no server pairs a guest with no server, but a
/// time all the same.
#[tokio::test]
async fn a_guest_paired_with_no_server_has_none() {
    within(async {
        let (_, tv) = tv().await;
        let (phone, id) = holder("Steve").await;
        pair(&tv, &phone, id).await;
        let listed = tv.portable().guest_pairings().await.unwrap();
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].server, None);
        assert!(listed[0].paired_at.is_some());
    })
    .await;
}

/// #6: a guest stored before the SDK kept where and when it was paired
/// is still listed, with neither.
#[tokio::test]
async fn a_guest_stored_before_pairings_were_kept_has_neither() {
    within(async {
        let (_, tv) = tv().await;
        let (phone, id) = holder("Steve").await;
        let key = tv.portable().device_key(id).await.unwrap();
        let credential = phone
            .portable()
            .issue_credential(id, key, None)
            .await
            .unwrap();
        tv.portable().install_credential(credential).await.unwrap();
        assert_eq!(
            tv.portable().guest_pairings().await.unwrap(),
            [GuestPairing {
                id,
                server: None,
                paired_at: None,
            }]
        );
    })
    .await;
}
