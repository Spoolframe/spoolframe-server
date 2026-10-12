//! Tying a local account to portable identities (P7-04, P7-09), over a
//! real connection: the challenge, the proof by the root or by a device
//! key, every way a proof is refused, the rules that let an account tie
//! several identities and keep each identity to one account here, and
//! severing one, which leaves the account and its other ties as they were.

use std::time::Duration;

use ed25519_dalek::{Signer, SigningKey};
use spoolframe_protocol::portable::{
    DevicePublicKey, MAX_CREDENTIAL_VALIDITY, PortableIdentityState, PortableUserId, RevokedKey,
    TieProof, UserDeviceCredential, tie_message,
};
use spoolframe_protocol::{Error, PortableTie, Reply, Request, Response, SessionId, WhoAmIReply};
use spoolframe_types::Item;

use crate::app_passkeys::web_passkey;
use crate::support::{LogCapture, TestClient, TestServer, alone, movie};

const PASSWORD: &str = "correct horse";

const DONE: Response = Response::Ok(Reply::Done);
const REFUSED: Response = Response::Err(Error::InvalidCredentials);
const OVER: Response = Response::Err(Error::TieChallengeExpired);

fn root(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn identity(seed: u8) -> PortableUserId {
    PortableUserId::from_bytes(root(seed).verifying_key().to_bytes()).unwrap()
}

fn device_key(seed: u8) -> DevicePublicKey {
    DevicePublicKey::from_bytes(root(seed).verifying_key().to_bytes()).unwrap()
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Identity `user`'s credential for device `seed`, from a day ago.
fn credential(user: u8, seed: u8) -> UserDeviceCredential {
    let issued_at = now() - 86_400;
    UserDeviceCredential::new(
        identity(user),
        device_key(seed),
        1,
        issued_at,
        issued_at + MAX_CREDENTIAL_VALIDITY,
        |m| root(user).sign(m),
    )
}

/// A server with alice and bob, and alice signed in on her phone.
async fn alice(test: &str) -> (TestServer, TestClient, SessionId) {
    let server = TestServer::start(test).await;
    server.add_user("alice", PASSWORD, Some("Alice")).await;
    server.add_user("bob", PASSWORD, None).await;
    let phone = server.client().await;
    phone.hello_as(Some("Alice's phone")).await;
    let session = phone.login("alice", PASSWORD).await;
    (server, phone, session)
}

/// The nonce of a new challenge in `session`.
async fn begin(phone: &TestClient, session: &SessionId) -> [u8; 32] {
    match phone.call(Some(session), Request::TieBegin).await {
        Response::Ok(Reply::TieChallenge {
            nonce,
            server,
            expires_in_secs,
        }) => {
            assert_eq!(&server, phone.conn.remote_id().as_bytes());
            assert_eq!(expires_in_secs, 300);
            nonce
        }
        other => panic!("not begun: {other:?}"),
    }
}

/// What tying `username`'s account to `user` signs, on `phone`'s server.
fn message(phone: &TestClient, nonce: &[u8; 32], user: u8, username: &str) -> Vec<u8> {
    tie_message(
        phone.conn.remote_id().as_bytes(),
        nonce,
        &identity(user),
        username,
    )
    .unwrap()
}

fn finish(user: u8, proof: TieProof) -> Request {
    Request::TieFinish {
        user: identity(user),
        proof,
    }
}

/// Root `user`'s proof for `username` over `nonce`.
fn by_root(phone: &TestClient, nonce: &[u8; 32], user: u8, username: &str) -> TieProof {
    TieProof::Root {
        signature: root(user)
            .sign(&message(phone, nonce, user, username))
            .into(),
        root: None,
        genesis: None,
    }
}

/// Begin and tie `session`'s account (`username`) to root `user`.
async fn tie(phone: &TestClient, session: &SessionId, user: u8, username: &str) -> Response {
    let nonce = begin(phone, session).await;
    let proof = by_root(phone, &nonce, user, username);
    phone.call(Some(session), finish(user, proof)).await
}

/// The account's ties as `WhoAmI` lists them.
async fn ties(phone: &TestClient, session: &SessionId) -> Vec<PortableTie> {
    match phone.call(Some(session), Request::WhoAmI).await {
        Response::Ok(Reply::WhoAmI(WhoAmIReply { user: me, .. })) => me.portable,
        other => panic!("no WhoAmI: {other:?}"),
    }
}

/// The identities the account is tied to, the oldest tie first.
async fn tied_to(phone: &TestClient, session: &SessionId) -> Vec<PortableUserId> {
    ties(phone, session)
        .await
        .into_iter()
        .map(|t| t.id)
        .collect()
}

fn untie(user: u8) -> Request {
    Request::Untie {
        user: identity(user),
    }
}

#[tokio::test]
async fn the_root_ties_the_account_and_no_user_is_added() {
    let (server, phone, session) = alice("tie-root").await;
    let users = server.store.list_users().await.unwrap().len();
    assert_eq!(tied_to(&phone, &session).await, []);
    assert_eq!(tie(&phone, &session, 7, "alice").await, DONE);
    assert_eq!(tied_to(&phone, &session).await, [identity(7)]);
    assert_eq!(server.store.list_users().await.unwrap().len(), users);
    let alice = server.store.tied_account(identity(7)).await.unwrap();
    let alice_id = server
        .store
        .list_users()
        .await
        .unwrap()
        .into_iter()
        .find(|u| u.username == "alice")
        .unwrap()
        .id;
    assert_eq!(alice, Some(alice_id));
    let ties = server.store.ties_of(alice_id).await.unwrap();
    assert_eq!(ties[0].proof_kind, "root");
}

#[tokio::test]
async fn a_device_with_a_credential_ties_without_the_root() {
    let (server, phone, session) = alice("tie-device").await;
    let nonce = begin(&phone, &session).await;
    let proof = TieProof::Device {
        credential: credential(7, 9),
        signature: root(9).sign(&message(&phone, &nonce, 7, "alice")),
    };
    assert_eq!(phone.call(Some(&session), finish(7, proof)).await, DONE);
    assert_eq!(tied_to(&phone, &session).await, [identity(7)]);
    let alice = server
        .store
        .tied_account(identity(7))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        server.store.ties_of(alice).await.unwrap()[0].proof_kind,
        "device"
    );
}

#[tokio::test]
async fn a_proof_made_for_anything_else_is_refused() {
    let (_server, phone, session) = alice("tie-elsewhere").await;
    let other_server = [0x11; 32];
    // Each made with a fresh challenge, so only what it was made for
    // differs.
    let nonce = begin(&phone, &session).await;
    let for_another_server = TieProof::Root {
        signature: root(7)
            .sign(&tie_message(&other_server, &nonce, &identity(7), "alice").unwrap())
            .into(),
        root: None,
        genesis: None,
    };
    assert_eq!(
        phone
            .call(Some(&session), finish(7, for_another_server))
            .await,
        REFUSED
    );

    begin(&phone, &session).await;
    let for_another_nonce = by_root(&phone, &[0; 32], 7, "alice");
    assert_eq!(
        phone
            .call(Some(&session), finish(7, for_another_nonce))
            .await,
        REFUSED
    );

    // Identity 8 signs for itself; the request names 7.
    let nonce = begin(&phone, &session).await;
    let for_another_identity = by_root(&phone, &nonce, 8, "alice");
    assert_eq!(
        phone
            .call(Some(&session), finish(7, for_another_identity))
            .await,
        REFUSED
    );

    let nonce = begin(&phone, &session).await;
    let for_bob = by_root(&phone, &nonce, 7, "bob");
    assert_eq!(
        phone.call(Some(&session), finish(7, for_bob)).await,
        REFUSED
    );

    // A device credential of identity 8 for identity 7's tie.
    let nonce = begin(&phone, &session).await;
    let theirs = TieProof::Device {
        credential: credential(8, 9),
        signature: root(9).sign(&message(&phone, &nonce, 7, "alice")),
    };
    assert_eq!(phone.call(Some(&session), finish(7, theirs)).await, REFUSED);

    assert_eq!(tied_to(&phone, &session).await, []);
}

#[tokio::test]
async fn a_challenge_is_used_once_and_not_for_long() {
    let (server, phone, session) = alice("tie-nonce").await;
    // Never begun.
    let proof = by_root(&phone, &[0; 32], 7, "alice");
    assert_eq!(phone.call(Some(&session), finish(7, proof)).await, OVER);

    // A wrong proof uses the challenge up: the right one then is late.
    let nonce = begin(&phone, &session).await;
    let wrong = by_root(&phone, &nonce, 7, "bob");
    assert_eq!(phone.call(Some(&session), finish(7, wrong)).await, REFUSED);
    let right = by_root(&phone, &nonce, 7, "alice");
    assert_eq!(
        phone.call(Some(&session), finish(7, right.clone())).await,
        OVER
    );

    // Past five minutes.
    let nonce = begin(&phone, &session).await;
    server.clock.advance(Duration::from_secs(5 * 60));
    let late = by_root(&phone, &nonce, 7, "alice");
    assert_eq!(phone.call(Some(&session), finish(7, late)).await, OVER);

    // Another session of the same user cannot answer this one's.
    let tablet = server.client().await;
    tablet.hello_as(Some("Alice's tablet")).await;
    let other = tablet.login("alice", PASSWORD).await;
    let nonce = begin(&phone, &session).await;
    let proof = by_root(&phone, &nonce, 7, "alice");
    assert_eq!(tablet.call(Some(&other), finish(7, proof)).await, OVER);
    assert_eq!(tied_to(&phone, &session).await, []);
}

#[tokio::test]
async fn a_revoked_device_key_ties_nothing() {
    let (server, phone, session) = alice("tie-revoked").await;
    let state = PortableIdentityState::new(
        identity(7),
        1,
        vec![RevokedKey {
            key: device_key(9),
            revoked_at: now() - 60,
        }],
        now(),
        |m| root(7).sign(m),
    )
    .unwrap();
    server
        .store
        .accept_identity_state(&state, now())
        .await
        .unwrap();
    let nonce = begin(&phone, &session).await;
    let proof = TieProof::Device {
        credential: credential(7, 9),
        signature: root(9).sign(&message(&phone, &nonce, 7, "alice")),
    };
    assert_eq!(phone.call(Some(&session), finish(7, proof)).await, REFUSED);
    assert_eq!(tied_to(&phone, &session).await, []);
}

#[tokio::test]
async fn an_identity_tied_here_is_not_tied_to_a_second_account() {
    let (server, phone, alice) = alice("tie-taken").await;
    assert_eq!(tie(&phone, &alice, 7, "alice").await, DONE);
    let laptop = server.client().await;
    laptop.hello_as(Some("Bob's laptop")).await;
    let bob = laptop.login("bob", PASSWORD).await;
    assert_eq!(
        tie(&laptop, &bob, 7, "bob").await,
        Response::Err(Error::PortableIdTaken)
    );
    assert_eq!(tied_to(&laptop, &bob).await, []);
    // Bob cannot sever alice's tie either.
    assert!(matches!(
        laptop.call(Some(&bob), untie(7)).await,
        Response::Err(Error::NotFound { .. })
    ));
    assert_eq!(tied_to(&phone, &alice).await, [identity(7)]);
    // Once alice severs hers, it is free.
    assert_eq!(phone.call(Some(&alice), untie(7)).await, DONE);
    assert_eq!(tie(&laptop, &bob, 7, "bob").await, DONE);
}

/// P7-09: a tied account ties a second identity without severing the
/// first, and `WhoAmI` lists both with their dates, the oldest first.
#[tokio::test]
async fn a_tied_account_ties_another_identity_and_keeps_the_first() {
    let (server, phone, session) = alice("tie-second").await;
    assert_eq!(tie(&phone, &session, 7, "alice").await, DONE);
    server.clock.advance(Duration::from_secs(60));
    assert_eq!(tie(&phone, &session, 8, "alice").await, DONE);
    let ties = ties(&phone, &session).await;
    assert_eq!(
        ties.iter().map(|t| t.id).collect::<Vec<_>>(),
        [identity(7), identity(8)]
    );
    assert_eq!(ties[1].tied_at - ties[0].tied_at, 60);
    assert!(ties.iter().all(|t| t.last_used_at.is_none()), "{ties:?}");
}

/// Tying an identity tied to this account already is done, and changes
/// nothing: no second row, the first tie's date stands.
#[tokio::test]
async fn tying_an_identity_tied_here_again_changes_nothing() {
    let (server, phone, session) = alice("tie-again").await;
    assert_eq!(tie(&phone, &session, 7, "alice").await, DONE);
    let before = ties(&phone, &session).await;
    server.clock.advance(Duration::from_secs(60));
    assert_eq!(tie(&phone, &session, 7, "alice").await, DONE);
    assert_eq!(ties(&phone, &session).await, before);
}

/// Severing one tie leaves the others, and the account's rows and
/// sessions as they were.
#[tokio::test]
async fn severing_leaves_the_account_as_it_was() {
    let (server, phone, session) = alice("untie-intact").await;
    web_passkey(&server, "alice", "AQID", "YubiKey").await;
    let tablet = server.client().await;
    tablet.hello_as(Some("Alice's tablet")).await;
    let other = tablet.login("alice", PASSWORD).await;
    let id = movie().id().clone();
    assert_eq!(
        phone
            .call(Some(&session), Request::MarkWatched { id: id.clone() })
            .await,
        DONE
    );
    assert_eq!(tie(&phone, &session, 7, "alice").await, DONE);
    assert_eq!(tie(&phone, &session, 8, "alice").await, DONE);
    let passkeys = phone.call(Some(&session), Request::ListPasskeys).await;
    let sessions = phone.call(Some(&session), Request::ListSessions).await;

    assert_eq!(phone.call(Some(&session), untie(7)).await, DONE);

    assert_eq!(tied_to(&phone, &session).await, [identity(8)]);
    assert_eq!(
        phone.call(Some(&session), Request::ListPasskeys).await,
        passkeys
    );
    assert_eq!(
        phone.call(Some(&session), Request::ListSessions).await,
        sessions
    );
    // The other device's session too.
    assert!(matches!(
        tablet.call(Some(&other), Request::WhoAmI).await,
        Response::Ok(Reply::WhoAmI(_))
    ));
    match phone.call(Some(&session), Request::Item { id }).await {
        Response::Ok(Reply::Item {
            item: Item::Movie(movie),
            ..
        }) => assert!(movie.playback.watched),
        other => panic!("no item: {other:?}"),
    }
    // Nothing left to sever for 7; 8 goes the same way.
    assert!(matches!(
        phone.call(Some(&session), untie(7)).await,
        Response::Err(Error::NotFound { .. })
    ));
    assert_eq!(phone.call(Some(&session), untie(8)).await, DONE);
    assert_eq!(tied_to(&phone, &session).await, []);
}

#[tokio::test]
async fn tying_and_severing_need_a_recent_sign_in() {
    let (server, phone, session) = alice("tie-recent").await;
    let nonce = begin(&phone, &session).await;
    crate::recent_admin::age(&server.dir, "alice").await;
    let not_recent = Response::Err(Error::RecentSignInRequired);
    assert_eq!(
        phone.call(Some(&session), Request::TieBegin).await,
        not_recent
    );
    let proof = by_root(&phone, &nonce, 7, "alice");
    assert_eq!(
        phone.call(Some(&session), finish(7, proof)).await,
        not_recent
    );
    assert_eq!(phone.call(Some(&session), untie(7)).await, not_recent);
}

/// A tie and a sever are each one INFO line naming the user and the
/// identity's fingerprint; a refused proof is one refusal line.
#[tokio::test]
async fn ties_and_refused_proofs_are_logged() {
    if !alone("ties::ties_and_refused_proofs_are_logged") {
        return;
    }
    let (capture, _guard) = LogCapture::start();
    let (_server, phone, session) = alice("tie-logged").await;
    let fingerprint = identity(7).fingerprint();

    let nonce = begin(&phone, &session).await;
    let wrong = by_root(&phone, &nonce, 7, "bob");
    assert_eq!(phone.call(Some(&session), finish(7, wrong)).await, REFUSED);
    let refused = capture.refused();
    assert_eq!(refused.len(), 1, "{refused:#?}");
    assert_eq!(refused[0].field("kind"), Some("sign_in_failed"));
    assert_eq!(refused[0].field("reason"), Some("tie proof refused"));
    assert_eq!(refused[0].field("user"), Some("alice"));

    assert_eq!(tie(&phone, &session, 7, "alice").await, DONE);
    let tied = capture.saying("portable identity tied, in an app");
    assert_eq!(tied.len(), 1, "{tied:#?}");
    assert_eq!(tied[0].level, tracing::Level::INFO);
    assert_eq!(tied[0].field("user"), Some("alice"));
    assert_eq!(tied[0].field("portable"), Some(fingerprint.as_str()));

    assert_eq!(phone.call(Some(&session), untie(7)).await, DONE);
    let untied = capture.saying("portable identity untied by its user, in an app");
    assert_eq!(untied.len(), 1, "{untied:#?}");
    assert_eq!(untied[0].field("portable"), Some(fingerprint.as_str()));
}

// The admin API: the ties are badges on the account, never other users,
// and both its user and an admin sever each.

/// Tie `username`'s account in the state in `dir` to identity `user`, as
/// a proved tie leaves it: tied `user` seconds past 1791504000, so ties
/// list in the order of their seeds, and used `used`.
async fn tied_in(dir: &std::path::Path, username: &str, user: u8, used: Option<i64>) {
    let pool = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::new().filename(dir.join("spoolframe.db")),
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO portable_ties \
           (local_user_id, portable_user_id, tied_at, proof_kind, last_used_at) \
         SELECT id, ?2, ?3, 'root', ?4 FROM users WHERE username = ?1",
    )
    .bind(username)
    .bind(identity(user).to_string())
    .bind(1_791_504_000 + i64::from(user))
    .bind(used)
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
}

fn hex(user: u8) -> String {
    identity(user).to_string()
}

#[tokio::test]
async fn the_admin_sees_the_ties_on_the_account_and_severs_one() {
    use spoolframe_admin_api::{AdminApiClient, NewUser, PortableTie, Role, error_code};
    let (_addr, root, dir) = crate::settings::served("tie-admin", "").await;
    root.user_add(NewUser {
        username: "bob".into(),
        display_name: None,
        password: PASSWORD.into(),
        role: Role::Consumer,
    })
    .await
    .unwrap();
    tied_in(dir.as_ref(), "bob", 7, Some(1_791_590_400)).await;
    tied_in(dir.as_ref(), "bob", 8, None).await;
    let users = root.user_list().await.unwrap();
    assert_eq!(users.len(), 2, "{users:?}");
    let bob = users.iter().find(|u| u.username == "bob").unwrap();
    assert_eq!(
        bob.portable,
        [
            PortableTie {
                id: hex(7),
                fingerprint: identity(7).fingerprint(),
                tied_at: 1_791_504_007,
                last_used_at: Some(1_791_590_400),
            },
            PortableTie {
                id: hex(8),
                fingerprint: identity(8).fingerprint(),
                tied_at: 1_791_504_008,
                last_used_at: None,
            },
        ]
    );
    let root_user = users.iter().find(|u| u.username == "root").unwrap();
    assert_eq!(root_user.portable, []);

    root.user_untie("bob".into(), hex(7)).await.unwrap();
    let users = root.user_list().await.unwrap();
    assert_eq!(users.len(), 2);
    let bob = users.iter().find(|u| u.username == "bob").unwrap();
    assert_eq!(
        bob.portable
            .iter()
            .map(|t| t.id.clone())
            .collect::<Vec<_>>(),
        [hex(8)]
    );
    for id in [hex(7), hex(9), "not an identity".into()] {
        assert_eq!(
            root.user_untie("bob".into(), id.clone())
                .await
                .err()
                .map(crate::admin::code),
            Some(error_code::NO_SUCH_LINK),
            "{id}"
        );
    }
    // Another account's tie is not severed through this one.
    assert_eq!(
        root.user_untie("root".into(), hex(8))
            .await
            .err()
            .map(crate::admin::code),
        Some(error_code::NO_SUCH_LINK)
    );
}

#[tokio::test]
async fn a_user_severs_one_of_their_ties_from_the_web_after_a_recent_sign_in() {
    use spoolframe_admin_api::{MeApiClient, error_code};
    let (_addr, root, dir) = crate::settings::served("tie-me", "").await;
    tied_in(dir.as_ref(), "root", 7, None).await;
    tied_in(dir.as_ref(), "root", 8, None).await;
    let fingerprints = |me: spoolframe_admin_api::UserInfo| {
        me.portable
            .into_iter()
            .map(|tie| tie.fingerprint)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        fingerprints(root.who_am_i().await.unwrap()),
        [identity(7).fingerprint(), identity(8).fingerprint()]
    );
    crate::recent_admin::age(dir.as_ref(), "root").await;
    assert_eq!(
        root.portable_untie(hex(7))
            .await
            .err()
            .map(crate::admin::code),
        Some(error_code::RECENT_SIGN_IN_REQUIRED)
    );
    root.confirm_password("correct horse".into()).await.unwrap();
    root.portable_untie(hex(7)).await.unwrap();
    assert_eq!(
        fingerprints(root.who_am_i().await.unwrap()),
        [identity(8).fingerprint()]
    );
    assert_eq!(
        root.portable_untie(hex(7))
            .await
            .err()
            .map(crate::admin::code),
        Some(error_code::NO_SUCH_LINK)
    );
}
