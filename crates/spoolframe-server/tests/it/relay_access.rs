//! The main server's answer to its own relay (P6-04): which tier an
//! endpoint is in. Only a relay `network.own_relays` names is answered.
//! And the relay itself, letting through the tiers its `access` allows:
//! an invite's holder by its kind's (P6-08).

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use iroh::endpoint::{Connection, presets};
use iroh::{Endpoint, RelayMode, SecretKey};
use iroh_relay::server::Server as RelayServer;
use spoolframe_client_sdk::{Client, ClientOptions, Device, MemoryStorage, SdkError};
use spoolframe_protocol::codec::{read_frame, write_frame};
use spoolframe_protocol::invite::Invite;
use spoolframe_protocol::relay_access::{
    AccessAnswer, AccessQuery, RELAY_ACCESS_ALPN, Tier, invite_token,
};
use spoolframe_protocol::{RedeemInvite, Reply, Request, Response};
use spoolframe_server::invites::InviteCode;
use spoolframe_server::network::Relay;
use spoolframe_server::own_relay::{self, Access, RelayConfig, Serve};
use spoolframe_server::store::invites::{InviteKind, InviteOrigin, NewInvite};
use spoolframe_types::Role;

use crate::support::{self, ServerOptions, TestServer};

const PASSWORD: &str = "battery staple";

/// A relay's endpoint, with `key`.
async fn relay_endpoint(key: SecretKey) -> Endpoint {
    Endpoint::builder(presets::Minimal)
        .secret_key(key)
        .relay_mode(RelayMode::Disabled)
        .alpns(vec![RELAY_ACCESS_ALPN.to_vec()])
        .bind()
        .await
        .unwrap()
}

/// A server that answers the relay with `key`.
async fn server_for(test: &str, key: &SecretKey) -> TestServer {
    let options = ServerOptions {
        own_relays: vec![key.public()],
        ..ServerOptions::default()
    };
    TestServer::start_with(support::fresh_dir(test).await, options).await
}

async fn ask(conn: &Connection, endpoint: String) -> Tier {
    ask_with(conn, endpoint, None).await
}

/// The tier of `endpoint`, connected to the relay with `invite`'s token.
async fn ask_with(conn: &Connection, endpoint: String, invite: Option<String>) -> Tier {
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    write_frame(&mut send, &AccessQuery { endpoint, invite })
        .await
        .unwrap();
    send.finish().unwrap();
    let answer: AccessAnswer = read_frame(&mut recv).await.unwrap();
    answer.tier
}

/// A relay's connection to `server`.
async fn relay_to(server: &TestServer, key: SecretKey) -> (Endpoint, Connection) {
    let relay = relay_endpoint(key).await;
    let conn = relay
        .connect(server.server.addr(), RELAY_ACCESS_ALPN)
        .await
        .unwrap();
    (relay, conn)
}

#[tokio::test]
async fn the_server_and_its_signed_in_devices_are_the_owner_s_and_others_strangers() {
    let key = SecretKey::generate();
    let server = server_for("relay-access-tiers", &key).await;
    server.add_user("alice", PASSWORD, None).await;
    let phone = server.client().await;
    phone.hello().await;
    phone.login("alice", PASSWORD).await;

    let (relay, conn) = relay_to(&server, key).await;
    assert_eq!(
        ask(&conn, server.server.id().to_string()).await,
        Tier::Owner
    );
    // A consumer's device as much as an admin's.
    assert_eq!(
        ask(&conn, phone.endpoint.id().to_string()).await,
        Tier::Owner
    );
    let stranger = SecretKey::generate().public().to_string();
    assert_eq!(ask(&conn, stranger).await, Tier::Stranger);
    relay.close().await;
    phone.close().await;
    server.stop().await;
}

/// A device signed in only as a granted portable user (P7-06) is a
/// friend's on the relay, not the owner's.
#[tokio::test]
async fn a_portable_user_s_device_is_a_friend_s() {
    use ed25519_dalek::{Signer, SigningKey};
    use spoolframe_protocol::PortableSignIn;
    use spoolframe_protocol::portable::{
        DevicePublicKey, MAX_CREDENTIAL_VALIDITY, PortableUserId, UserDeviceCredential,
        device_proof_message,
    };
    let key = SecretKey::generate();
    let server = server_for("relay-access-portable", &key).await;
    let root = SigningKey::from_bytes(&[7; 32]);
    let device = SigningKey::from_bytes(&[9; 32]);
    let user = PortableUserId::from_bytes(root.verifying_key().to_bytes()).unwrap();
    server.store.grant_portable(user, None, None).await.unwrap();
    let tv = server.client().await;
    tv.hello().await;
    let Response::Ok(Reply::PortableSignInBegun { nonce, .. }) =
        tv.call(None, Request::BeginPortableSignIn).await
    else {
        panic!("not begun");
    };
    let now = server.store.clock().unix_secs();
    let credential = UserDeviceCredential::new(
        user,
        DevicePublicKey::from_bytes(device.verifying_key().to_bytes()).unwrap(),
        1,
        now - 60,
        now - 60 + MAX_CREDENTIAL_VALIDITY,
        |m| root.sign(m),
    );
    let proof = device.sign(&device_proof_message(
        tv.conn.remote_id().as_bytes(),
        &nonce,
    ));
    let signed_in = tv
        .call(
            None,
            Request::FinishPortableSignIn(PortableSignIn {
                credential,
                proof,
                states: vec![],
                display_name: None,
            }),
        )
        .await;
    assert!(
        matches!(signed_in, Response::Ok(Reply::LoggedIn { .. })),
        "{signed_in:?}"
    );

    let (relay, conn) = relay_to(&server, key).await;
    assert_eq!(ask(&conn, tv.endpoint.id().to_string()).await, Tier::Friend);
    relay.close().await;
    tv.close().await;
    server.stop().await;
}

/// A live `create-user` invite with `role`, `uses` times: its code and
/// id.
async fn invite(server: &TestServer, role: Role, uses: u32) -> (InviteCode, String) {
    let code = InviteCode::generate();
    let made = server
        .store
        .create_invite(NewInvite {
            kind: InviteKind::CreateUser,
            capability_hash: &code.hash(),
            role,
            note: None,
            uses,
            expires_at: server.store.clock().unix_secs() + 3600,
            created_by: "root",
            origin: &InviteOrigin::Web,
        })
        .await
        .unwrap();
    (code, made.id)
}

/// A live `portable-account` invite (P7), put in the database by hand:
/// the server refuses to make one until P7. Its code.
async fn portable_invite(server: &TestServer) -> InviteCode {
    let code = InviteCode::generate();
    crate::app_passkeys::sql(
        server,
        "INSERT INTO invites (id, kind, capability_hash, role, uses, uses_left, expires_at, \
                              created_by, created_at) \
         VALUES ('p7', 'portable-account', ?1, 'consumer', 1, 1, 9999999999, 'root', 0)",
        &code.hash(),
    )
    .await;
    code
}

/// `code`'s token, as `endpoint` would connect with it.
fn token(code: &InviteCode, endpoint: &iroh::EndpointId) -> Option<String> {
    Some(invite_token(code.as_str(), endpoint.as_bytes()).unwrap())
}

fn redeem(code: &InviteCode, username: &str) -> Request {
    Request::RedeemInvite(RedeemInvite {
        code: code.shown(),
        username: username.into(),
        display_name: None,
        password: "correct horse battery".into(),
    })
}

/// P6-08: an invite's holder is its kind's tier before redeeming it: an
/// account here, of any role, is an owner's; a portable user's (P7) a
/// friend's. A token for another endpoint, a revoked invite's or a
/// used-up one's is a stranger's.
#[tokio::test]
async fn a_live_invite_s_token_is_its_kind_s_tier_and_any_other_a_stranger() {
    let key = SecretKey::generate();
    let server = server_for("relay-access-invites", &key).await;
    let (consumer, _) = invite(&server, Role::Consumer, 1).await;
    let (admin, _) = invite(&server, Role::Admin, 1).await;
    let portable = portable_invite(&server).await;
    let (revoked, id) = invite(&server, Role::Consumer, 1).await;
    server.store.revoke_invite(&id).await.unwrap();
    let (used_up, _) = invite(&server, Role::Consumer, 1).await;
    server
        .store
        .redeem_invite(&used_up.hash(), "first", None, "$argon2id$x")
        .await
        .unwrap();

    let (relay, conn) = relay_to(&server, key).await;
    let phone = SecretKey::generate().public();
    let other = SecretKey::generate().public();
    let asked = |code: &InviteCode, made_for: &iroh::EndpointId| {
        ask_with(&conn, phone.to_string(), token(code, made_for))
    };
    assert_eq!(asked(&consumer, &phone).await, Tier::Owner);
    assert_eq!(asked(&admin, &phone).await, Tier::Owner);
    assert_eq!(asked(&portable, &phone).await, Tier::Friend);
    assert_eq!(asked(&consumer, &other).await, Tier::Stranger);
    assert_eq!(asked(&revoked, &phone).await, Tier::Stranger);
    assert_eq!(asked(&used_up, &phone).await, Tier::Stranger);
    relay.close().await;
    server.stop().await;
}

/// P6-08: a token that matches no live invite is a wrong code (A-13).
/// Past the limit the right one is not looked at, and the device's
/// redemption is refused as a guesser's too: one limiter for both.
#[tokio::test]
async fn wrong_tokens_count_as_wrong_codes() {
    let key = SecretKey::generate();
    let server = server_for("relay-access-guesses", &key).await;
    let (code, _) = invite(&server, Role::Consumer, 1).await;
    let guesser = server.client().await;
    guesser.hello().await;
    let me = guesser.endpoint.id();

    let (relay, conn) = relay_to(&server, key).await;
    for _ in 0..spoolframe_server::auth::MAX_FAILURES {
        let wrong = InviteCode::generate();
        assert_eq!(
            ask_with(&conn, me.to_string(), token(&wrong, &me)).await,
            Tier::Stranger
        );
    }
    assert_eq!(
        ask_with(&conn, me.to_string(), token(&code, &me)).await,
        Tier::Stranger,
        "the right token was looked at past the limit"
    );
    assert!(
        matches!(
            guesser.call(None, redeem(&code, "mallory")).await,
            Response::Err(spoolframe_protocol::Error::RateLimited { .. })
        ),
        "the guesser's redemption was let through"
    );
    relay.close().await;
    guesser.close().await;
    server.stop().await;
}

/// P6-08: a consumer invite's holder relays as an owner before redeeming
/// it, and its device after, by the session it then has.
#[tokio::test]
async fn a_consumer_invite_s_holder_relays_as_owner_before_and_after() {
    let key = SecretKey::generate();
    let server = server_for("relay-access-consumer-invite", &key).await;
    let (code, _) = invite(&server, Role::Consumer, 1).await;
    let phone = server.client().await;
    phone.hello().await;
    let me = phone.endpoint.id();

    let (relay, conn) = relay_to(&server, key).await;
    assert_eq!(
        ask_with(&conn, me.to_string(), token(&code, &me)).await,
        Tier::Owner
    );
    assert!(matches!(
        phone.call(None, redeem(&code, "mum")).await,
        Response::Ok(Reply::LoggedIn { .. })
    ));
    // The invite is used up: the session says who the device is now.
    assert_eq!(ask(&conn, me.to_string()).await, Tier::Owner);
    relay.close().await;
    phone.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_relay_the_server_does_not_name_is_closed_on() {
    let named = SecretKey::generate();
    let server = server_for("relay-access-unnamed", &named).await;
    let other = relay_endpoint(SecretKey::generate()).await;
    let conn = other
        .connect(server.server.addr(), RELAY_ACCESS_ALPN)
        .await
        .unwrap();
    let asked = async {
        let (mut send, mut recv) = conn.open_bi().await?;
        write_frame(
            &mut send,
            &AccessQuery {
                endpoint: server.server.id().to_string(),
                invite: None,
            },
        )
        .await
        .map_err(std::io::Error::other)?;
        read_frame::<_, AccessAnswer>(&mut recv)
            .await
            .map_err(std::io::Error::other)
    };
    assert!(asked.await.is_err(), "an unnamed relay was answered");
    other.close().await;
    server.stop().await;
}

/// P6-07 (both reviews' first finding): a named relay holds at most
/// `MAX_CONNECTIONS` connections on the ALPN, so a relay whose key is
/// stolen cannot open enough to load the server's store. The ones it
/// holds keep answering.
#[tokio::test]
async fn a_named_relay_past_its_connections_is_closed_on() {
    use spoolframe_server::relay_access::MAX_CONNECTIONS;
    let key = SecretKey::generate();
    let server = server_for("relay-access-cap", &key).await;
    let relay = relay_endpoint(key).await;
    let me = server.server.id().to_string();
    let mut held = Vec::new();
    for _ in 0..MAX_CONNECTIONS {
        let conn = relay
            .connect(server.server.addr(), RELAY_ACCESS_ALPN)
            .await
            .unwrap();
        assert_eq!(ask(&conn, me.clone()).await, Tier::Owner);
        held.push(conn);
    }
    let over = relay
        .connect(server.server.addr(), RELAY_ACCESS_ALPN)
        .await
        .unwrap();
    let asked = async {
        let (mut send, mut recv) = over.open_bi().await?;
        write_frame(
            &mut send,
            &AccessQuery {
                endpoint: me.clone(),
                invite: None,
            },
        )
        .await
        .map_err(std::io::Error::other)?;
        read_frame::<_, AccessAnswer>(&mut recv)
            .await
            .map_err(std::io::Error::other)
    };
    assert!(
        asked.await.is_err(),
        "a connection past the cap was answered"
    );
    assert_eq!(ask(&held[0], me.clone()).await, Tier::Owner);

    // One closed makes room again.
    held.pop().unwrap().close(0u32.into(), b"done");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let again = relay
        .connect(server.server.addr(), RELAY_ACCESS_ALPN)
        .await
        .unwrap();
    assert_eq!(ask(&again, me).await, Tier::Owner);
    relay.close().await;
    server.stop().await;
}

/// A server whose only relay is one of its own (P6-04), on loopback over
/// plain HTTP, letting through `access`; once the server has taken it as
/// its home relay. The relay runs until dropped.
async fn server_with_own_relay(test: &str, access: Access) -> (TestServer, RelayServer) {
    let key = SecretKey::generate();
    // The relay's port is chosen first: the server is told its URL, and
    // the relay the server's address.
    let http = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap();
    let url = url::Url::parse(&format!("http://{http}")).unwrap();
    let options = ServerOptions {
        relays: vec![Relay::Custom(url.clone())],
        own_relays: vec![key.public()],
        ..ServerOptions::default()
    };
    let server = TestServer::start_with(support::fresh_dir(test).await, options).await;
    let config = RelayConfig {
        server: server.server.addr(),
        access,
        serve: Serve::Proxy,
        http,
        quic: None,
    };
    let relay = own_relay::spawn(&config, key, &std::env::temp_dir())
        .await
        .unwrap();
    let home = iroh::RelayUrl::from(url);
    let homed = tokio::time::timeout(Duration::from_secs(10), async {
        while !server.server.addr().relay_urls().any(|u| *u == home) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(homed.is_ok(), "the server did not home on its own relay");
    (server, relay)
}

/// A device that reaches servers through relays, and never on the LAN.
async fn device() -> Client {
    let options = ClientOptions {
        lan_discovery: false,
        relays: true,
        ..ClientOptions::default()
    };
    Client::open_with_options(
        Arc::new(MemoryStorage::default()),
        Device {
            name: "Mum's phone".into(),
            client_name: "sdk-test".into(),
            client_version: "0".into(),
            platform: spoolframe_client_sdk::types::Platform::Linux,
            capabilities: Default::default(),
        },
        options,
    )
    .await
    .unwrap()
}

/// A link to `server` with `code` and its relays, and no address: only
/// the relay can reach it, as from a phone on cellular.
fn relay_only_link(server: &TestServer, code: &InviteCode) -> String {
    Invite {
        server: server.server.id().to_string(),
        relays: server.server.told().relays(),
        code: Some(code.as_str().to_owned()),
        ..Invite::default()
    }
    .link()
}

fn deny_but(owner: bool, friends: bool) -> Access {
    Access {
        owner,
        friends,
        everyone: false,
    }
}

/// The ticket's first check: where everyone and friends are denied and
/// owners let through, a consumer invite's holder reaches the server
/// through its relay and redeems it.
#[tokio::test]
async fn an_account_invite_s_holder_redeems_through_a_relay_that_lets_owners_through() {
    let (server, relay) = server_with_own_relay("relay-invite-owners", deny_but(true, false)).await;
    let (code, _) = invite(&server, Role::Consumer, 1).await;
    let phone = device().await;
    let session = phone
        .redeem_invite(
            &relay_only_link(&server, &code),
            "mum",
            None,
            "correct horse battery",
        )
        .await;
    assert!(session.is_ok(), "{:?}", session.err());
    phone.close().await;
    server.stop().await;
    drop(relay);
}

/// And where owners are denied, the same holder is not let through.
#[tokio::test]
async fn an_account_invite_s_holder_is_refused_where_owners_are_not_let_through() {
    let (server, relay) =
        server_with_own_relay("relay-invite-no-owners", deny_but(false, true)).await;
    let (code, _) = invite(&server, Role::Admin, 1).await;
    let phone = device().await;
    let refused = phone
        .redeem_invite(
            &relay_only_link(&server, &code),
            "mum",
            None,
            "correct horse battery",
        )
        .await;
    assert!(
        matches!(refused, Err(SdkError::Unreachable(_))),
        "{:?}",
        refused.map(|_| ())
    );
    assert_eq!(server.store.user_count().await.unwrap(), 0);
    phone.close().await;
    server.stop().await;
    drop(relay);
}

/// A portable user's invite (P7) holder is a friend: it reaches the
/// server through a relay that lets friends through, and not through one
/// that does not. It is not redeemed: the server takes none until P7.
#[tokio::test]
async fn a_portable_invite_s_holder_is_a_friend_on_the_relay() {
    for (friends, test) in [
        (true, "relay-portable-friends"),
        (false, "relay-portable-no-friends"),
    ] {
        let (server, relay) = server_with_own_relay(test, deny_but(false, friends)).await;
        let code = portable_invite(&server).await;
        let phone = device().await;
        let reached = phone.connect_invite(&relay_only_link(&server, &code)).await;
        if friends {
            assert!(reached.is_ok(), "{:?}", reached.err());
        } else {
            assert!(
                matches!(reached, Err(SdkError::Unreachable(_))),
                "{:?}",
                reached.map(|_| ())
            );
        }
        phone.close().await;
        server.stop().await;
        drop(relay);
    }
}
