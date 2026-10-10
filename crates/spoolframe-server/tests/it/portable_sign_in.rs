//! Signing in with a portable user's device credential (P7-06), over a
//! real connection: local-first resolution (the tied account here, else
//! the grant, else nothing), what a portable session may and may not do,
//! every way a credential is refused and counted, revocation by a newer
//! identity state, and a revoked grant ending the session.

use ed25519_dalek::{Signer, SigningKey};
use spoolframe_protocol::portable::{
    DevicePublicKey, MAX_CREDENTIAL_VALIDITY, PortableIdentityState, PortableUserId, RevokedKey,
    UserDeviceCredential, device_proof_message,
};
use spoolframe_protocol::{Error, PortableSignIn, Reply, Request, Response, SessionId};
use spoolframe_types::{Item, Role};

use crate::support::{TestClient, TestServer, movie};

const PASSWORD: &str = "correct horse";
const REFUSED: Response = Response::Err(Error::InvalidCredentials);

fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn identity(seed: u8) -> PortableUserId {
    PortableUserId::from_bytes(key(seed).verifying_key().to_bytes()).unwrap()
}

fn device_key(seed: u8) -> DevicePublicKey {
    DevicePublicKey::from_bytes(key(seed).verifying_key().to_bytes()).unwrap()
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Identity `user`'s credential for device `seed`, from a day ago, signed
/// by root `signer`.
fn signed_credential(user: u8, seed: u8, signer: u8) -> UserDeviceCredential {
    let issued_at = now() - 86_400;
    UserDeviceCredential::new(
        identity(user),
        device_key(seed),
        1,
        issued_at,
        issued_at + MAX_CREDENTIAL_VALIDITY,
        |m| key(signer).sign(m),
    )
}

fn credential(user: u8, seed: u8) -> UserDeviceCredential {
    signed_credential(user, seed, user)
}

/// A new sign-in's nonce on `tv`.
async fn begin(tv: &TestClient) -> [u8; 32] {
    match tv.call(None, Request::BeginPortableSignIn).await {
        Response::Ok(Reply::PortableSignInBegun {
            nonce,
            expires_in_secs,
        }) => {
            assert_eq!(expires_in_secs, 300);
            nonce
        }
        other => panic!("not begun: {other:?}"),
    }
}

/// Sign in on `tv` with `credential`, its proof made by device key
/// `prover`, carrying `states`.
async fn sign_in_as(
    tv: &TestClient,
    credential: UserDeviceCredential,
    prover: u8,
    states: Vec<PortableIdentityState>,
) -> Response {
    let nonce = begin(tv).await;
    let proof = key(prover).sign(&device_proof_message(
        tv.conn.remote_id().as_bytes(),
        &nonce,
    ));
    tv.call(
        None,
        Request::FinishPortableSignIn(PortableSignIn {
            credential,
            proof,
            states,
            display_name: None,
        }),
    )
    .await
}

/// Sign identity `user` in on `tv` from device `seed`.
async fn sign_in(tv: &TestClient, user: u8, seed: u8) -> Response {
    sign_in_as(tv, credential(user, seed), seed, vec![]).await
}

fn session(response: Response) -> SessionId {
    match response {
        Response::Ok(Reply::LoggedIn {
            session,
            remembered,
        }) => {
            assert!(!remembered);
            session
        }
        other => panic!("not signed in: {other:?}"),
    }
}

/// A server with alice, and a TV that said hello.
async fn living_room(test: &str) -> (TestServer, TestClient) {
    let server = TestServer::start(test).await;
    server.add_user("alice", PASSWORD, Some("Alice")).await;
    let tv = server.client().await;
    tv.hello_as(Some("Living room")).await;
    (server, tv)
}

#[tokio::test]
async fn a_granted_identity_signs_in_as_itself_and_is_no_user() {
    let (server, tv) = living_room("portable-granted").await;
    server
        .store
        .grant_portable(identity(7), Some("Sam"), None)
        .await
        .unwrap();
    let users = server.store.list_users().await.unwrap();
    let session = session(sign_in(&tv, 7, 9).await);
    match tv.call(Some(&session), Request::WhoAmI).await {
        Response::Ok(Reply::WhoAmI(me)) => {
            assert_eq!(me.user_id, format!("portable:{}", identity(7)));
            assert_eq!(me.username, identity(7).to_string());
            assert_eq!(me.display_name, "Sam");
            assert_eq!(me.role, Role::Consumer);
            assert!(!me.locked);
            assert_eq!(me.pin, None);
            assert_eq!(me.portable, Some(identity(7)));
        }
        other => panic!("no WhoAmI: {other:?}"),
    }
    assert_eq!(server.store.list_users().await.unwrap(), users);
    assert_eq!(
        server.store.grants().await.unwrap()[0].live_sessions,
        1,
        "the grant counts the session"
    );
}

#[tokio::test]
async fn a_portable_user_browses_and_keeps_watch_state_of_their_own() {
    let (server, tv) = living_room("portable-watch").await;
    server
        .store
        .grant_portable(identity(7), None, None)
        .await
        .unwrap();
    let guest = session(sign_in(&tv, 7, 9).await);
    let alice = tv.login("alice", PASSWORD).await;
    assert!(matches!(
        tv.call(Some(&guest), Request::Libraries).await,
        Response::Ok(Reply::Libraries { .. })
    ));
    let id = movie().id().clone();
    assert_eq!(
        tv.call(Some(&guest), Request::MarkWatched { id: id.clone() })
            .await,
        Response::Ok(Reply::Done)
    );
    let watched = |response: Response| match response {
        Response::Ok(Reply::Item {
            item: Item::Movie(movie),
            ..
        }) => movie.playback.watched,
        other => panic!("no item: {other:?}"),
    };
    assert!(watched(
        tv.call(Some(&guest), Request::Item { id: id.clone() })
            .await
    ));
    // Theirs, not alice's on the same TV.
    assert!(!watched(tv.call(Some(&alice), Request::Item { id }).await));
}

#[tokio::test]
async fn what_only_an_account_here_has_is_refused_to_a_portable_user() {
    let (server, tv) = living_room("portable-local-only").await;
    server
        .store
        .grant_portable(identity(7), None, None)
        .await
        .unwrap();
    let guest = session(sign_in(&tv, 7, 9).await);
    let refused = Response::Err(Error::NotForPortableUsers);
    for request in [
        Request::ListSessions,
        Request::ListDevices,
        Request::ListPasskeys,
        Request::TieBegin,
        Request::Untie,
        Request::BeginPasskeyRegistration,
        Request::SetPin {
            pin: "1234".into(),
            proof: None,
        },
        Request::LookupDeviceSignIn {
            code: "ABCD-EFGH".into(),
        },
        // Invites are an admin's, and a portable user is no one's (PX-44).
        Request::ListInvites,
        Request::RevokeInvite { id: "any".into() },
    ] {
        let name = format!("{request:?}");
        assert_eq!(tv.call(Some(&guest), request).await, refused, "{name}");
    }
    // No Plex link to show.
    assert_eq!(
        tv.call(Some(&guest), Request::GetPlexLink).await,
        Response::Ok(Reply::PlexLink { link: None })
    );
    // Signing out works as for anyone. Alice stays signed in on the TV,
    // so the server keeps its connection open.
    tv.login("alice", PASSWORD).await;
    assert_eq!(
        tv.call(Some(&guest), Request::Logout).await,
        Response::Ok(Reply::LoggedOut)
    );
    assert_eq!(
        tv.call(Some(&guest), Request::WhoAmI).await,
        Response::Err(Error::Unauthenticated)
    );
}

#[tokio::test]
async fn a_tied_identity_signs_in_as_the_account_even_with_a_grant() {
    let (server, tv) = living_room("portable-tied").await;
    let alice = server
        .store
        .list_users()
        .await
        .unwrap()
        .into_iter()
        .find(|u| u.username == "alice")
        .unwrap()
        .id;
    server.store.tie(alice, identity(7), "root").await.unwrap();
    server
        .store
        .grant_portable(identity(7), None, None)
        .await
        .unwrap();
    let session = session(sign_in(&tv, 7, 9).await);
    match tv.call(Some(&session), Request::WhoAmI).await {
        Response::Ok(Reply::WhoAmI(me)) => {
            assert_eq!(me.username, "alice");
            assert_eq!(me.user_id, format!("local:{alice}"));
            assert_eq!(me.portable, Some(identity(7)));
        }
        other => panic!("no WhoAmI: {other:?}"),
    }
    // A local session: the account's own requests work.
    assert!(matches!(
        tv.call(Some(&session), Request::ListSessions).await,
        Response::Ok(Reply::Sessions { .. })
    ));
}

#[tokio::test]
async fn a_tied_account_that_is_disabled_is_refused() {
    let (server, tv) = living_room("portable-tied-disabled").await;
    let alice = server
        .store
        .list_users()
        .await
        .unwrap()
        .into_iter()
        .find(|u| u.username == "alice")
        .unwrap()
        .id;
    server.store.tie(alice, identity(7), "root").await.unwrap();
    server.store.disable_user("alice").await.unwrap();
    assert_eq!(sign_in(&tv, 7, 9).await, REFUSED);
}

#[tokio::test]
async fn neither_tied_nor_granted_is_refused_and_counted() {
    let (_server, tv) = living_room("portable-stranger").await;
    let mut refused = 0;
    loop {
        match sign_in(&tv, 7, 9).await {
            REFUSED => refused += 1,
            Response::Err(Error::RateLimited { .. }) => break,
            other => panic!("unexpected: {other:?}"),
        }
        assert!(refused < 50, "never limited");
    }
    assert!(refused >= 3, "limited after {refused}");
}

#[tokio::test]
async fn a_credential_that_does_not_hold_is_refused() {
    let (server, tv) = living_room("portable-forged").await;
    server
        .store
        .grant_portable(identity(7), None, None)
        .await
        .unwrap();
    // Proved by another device key than the one it names.
    assert_eq!(sign_in_as(&tv, credential(7, 9), 11, vec![]).await, REFUSED);
    // Signed by another identity's root.
    assert_eq!(
        sign_in_as(&tv, signed_credential(7, 9, 8), 9, vec![]).await,
        REFUSED
    );
    // Tampered: moved onto another key.
    let moved = UserDeviceCredential {
        device_key: device_key(11),
        ..credential(7, 9)
    };
    assert_eq!(sign_in_as(&tv, moved, 11, vec![]).await, REFUSED);
    // A proof over a nonce the server never gave.
    let proof = key(9).sign(&device_proof_message(
        tv.conn.remote_id().as_bytes(),
        &[0; 32],
    ));
    begin(&tv).await;
    assert_eq!(
        tv.call(
            None,
            Request::FinishPortableSignIn(PortableSignIn {
                credential: credential(7, 9),
                proof,
                states: vec![],
                display_name: None,
            }),
        )
        .await,
        REFUSED
    );
    // The good one still signs in.
    session(sign_in(&tv, 7, 9).await);
}

#[tokio::test]
async fn a_nonce_is_used_once() {
    let (server, tv) = living_room("portable-nonce-once").await;
    server
        .store
        .grant_portable(identity(7), None, None)
        .await
        .unwrap();
    let nonce = begin(&tv).await;
    let finish = || {
        Request::FinishPortableSignIn(PortableSignIn {
            credential: credential(7, 9),
            proof: key(9).sign(&device_proof_message(
                tv.conn.remote_id().as_bytes(),
                &nonce,
            )),
            states: vec![],
            display_name: None,
        })
    };
    session(tv.call(None, finish()).await);
    assert_eq!(tv.call(None, finish()).await, REFUSED);
}

/// Identity 7's state at `revision`, revoking `revoked` device keys.
fn state(revision: u64, revoked: &[u8]) -> PortableIdentityState {
    let revoked = revoked
        .iter()
        .map(|seed| RevokedKey {
            key: device_key(*seed),
            revoked_at: now() - 60,
        })
        .collect();
    PortableIdentityState::new(identity(7), revision, revoked, now() - 60, |m| {
        key(7).sign(m)
    })
    .unwrap()
}

#[tokio::test]
async fn a_revocation_brought_by_any_device_stops_the_key() {
    let (server, tv) = living_room("portable-revoked").await;
    server
        .store
        .grant_portable(identity(7), None, None)
        .await
        .unwrap();
    let guest = session(sign_in(&tv, 7, 9).await);
    // Alice stays signed in on the TV, so its connection stays open.
    tv.login("alice", PASSWORD).await;
    // The phone (device 10) brings the state that revokes the TV's key:
    // the TV's session ends with it (review, finding 1).
    let phone = server.client().await;
    phone.hello_as(Some("Sam's phone")).await;
    session(sign_in_as(&phone, credential(7, 10), 10, vec![state(1, &[9])]).await);
    assert_eq!(
        tv.call(Some(&guest), Request::WhoAmI).await,
        Response::Err(Error::Unauthenticated)
    );
    assert_eq!(sign_in(&tv, 7, 9).await, REFUSED);
    // A state carried with the very credential it revokes counts too.
    assert_eq!(
        sign_in_as(&tv, credential(7, 12), 12, vec![state(2, &[9, 12])]).await,
        REFUSED
    );
}

#[tokio::test]
async fn revoking_the_grant_ends_the_session() {
    let (server, tv) = living_room("portable-grant-revoked").await;
    server
        .store
        .grant_portable(identity(7), None, None)
        .await
        .unwrap();
    let guest = session(sign_in(&tv, 7, 9).await);
    // Alice stays signed in on the TV, so the server keeps its
    // connection open.
    tv.login("alice", PASSWORD).await;
    assert!(server.store.revoke_grant(identity(7)).await.unwrap());
    assert_eq!(
        tv.call(Some(&guest), Request::WhoAmI).await,
        Response::Err(Error::Unauthenticated)
    );
    assert_eq!(sign_in(&tv, 7, 9).await, REFUSED);
}

#[tokio::test]
async fn the_admin_lists_and_revokes_grants_and_the_users_list_is_unchanged() {
    use spoolframe_admin_api::{AdminApiClient, error_code};
    let (_addr, root, dir) = crate::settings::served("portable-grant-admin", "").await;
    let store = spoolframe_server::store::Store::open(dir.as_ref())
        .await
        .unwrap();
    store
        .grant_portable(identity(7), Some("Sam"), None)
        .await
        .unwrap();
    let users = root.user_list().await.unwrap();
    assert_eq!(users.len(), 1, "only the admin: {users:?}");
    let grants = root.grant_list().await.unwrap();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].id, identity(7).to_string());
    assert_eq!(grants[0].fingerprint, identity(7).fingerprint());
    assert_eq!(grants[0].display_name.as_deref(), Some("Sam"));
    root.grant_revoke(identity(7).to_string()).await.unwrap();
    assert!(root.grant_list().await.unwrap().is_empty());
    assert_eq!(
        root.grant_revoke(identity(7).to_string())
            .await
            .err()
            .map(crate::admin::code),
        Some(error_code::NO_SUCH_GRANT)
    );
}

/// Review, finding 1: a tied account's session opened with a credential
/// ends when a state revoking the key arrives.
#[tokio::test]
async fn a_tied_account_s_credential_session_ends_with_its_key() {
    let (server, tv) = living_room("portable-tied-revoked").await;
    let alice = alice_id(&server).await;
    server.store.tie(alice, identity(7), "root").await.unwrap();
    let tied = session(sign_in(&tv, 7, 9).await);
    let other = tv.login("alice", PASSWORD).await;
    server
        .store
        .accept_identity_state(&state(1, &[9]), now())
        .await
        .unwrap();
    assert_eq!(
        tv.call(Some(&tied), Request::WhoAmI).await,
        Response::Err(Error::Unauthenticated)
    );
    // Her password session is no credential's: it stays.
    assert!(matches!(
        tv.call(Some(&other), Request::WhoAmI).await,
        Response::Ok(Reply::WhoAmI(_))
    ));
}

async fn alice_id(server: &TestServer) -> spoolframe_server::store::users::LocalUserId {
    server
        .store
        .list_users()
        .await
        .unwrap()
        .into_iter()
        .find(|u| u.username == "alice")
        .unwrap()
        .id
}

/// Review, finding 4: a credential is no fresh proof of the account: what
/// needs a recent sign-in asks to confirm first.
#[tokio::test]
async fn a_tied_credential_sign_in_is_not_a_recent_sign_in() {
    let (server, tv) = living_room("portable-tied-not-recent").await;
    let alice = alice_id(&server).await;
    server.store.tie(alice, identity(7), "root").await.unwrap();
    let tied = session(sign_in(&tv, 7, 9).await);
    let recent = Response::Err(Error::RecentSignInRequired);
    for request in [
        Request::BeginPasskeyRegistration,
        Request::Untie,
        Request::TieBegin,
    ] {
        let name = format!("{request:?}");
        assert_eq!(tv.call(Some(&tied), request).await, recent, "{name}");
    }
    // Confirming with the password makes it recent.
    assert_eq!(
        tv.call(
            Some(&tied),
            Request::ConfirmPassword(spoolframe_protocol::ConfirmPassword {
                password: PASSWORD.into(),
            }),
        )
        .await,
        Response::Ok(Reply::Confirmed)
    );
    assert!(matches!(
        tv.call(Some(&tied), Request::BeginPasskeyRegistration)
            .await,
        Response::Ok(Reply::PasskeyRegistrationBegun(_))
    ));
}

/// Review, finding 3: a stranger's states are checked and dropped, never
/// kept; another identity's, or more than two, refuse the sign-in.
#[tokio::test]
async fn states_are_kept_only_for_an_identity_known_here() {
    let (server, tv) = living_room("portable-states").await;
    assert_eq!(
        sign_in_as(&tv, credential(7, 9), 9, vec![state(1, &[13])]).await,
        REFUSED
    );
    assert!(
        server
            .store
            .identity_state(identity(7))
            .await
            .unwrap()
            .is_none(),
        "a stranger's state was kept"
    );
    server
        .store
        .grant_portable(identity(7), None, None)
        .await
        .unwrap();
    let theirs =
        PortableIdentityState::new(identity(8), 1, vec![], now() - 60, |m| key(8).sign(m)).unwrap();
    assert_eq!(
        sign_in_as(&tv, credential(7, 9), 9, vec![theirs]).await,
        REFUSED
    );
    assert_eq!(
        sign_in_as(
            &tv,
            credential(7, 9),
            9,
            vec![state(1, &[13]), state(2, &[13]), state(3, &[13])]
        )
        .await,
        REFUSED
    );
    session(sign_in_as(&tv, credential(7, 9), 9, vec![state(1, &[13])]).await);
    assert_eq!(
        server
            .store
            .identity_state(identity(7))
            .await
            .unwrap()
            .map(|s| s.revision),
        Some(1)
    );
}
