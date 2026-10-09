//! The main server's answer to its own relay (P6-04): which tier an
//! endpoint is in. Only a relay `network.own_relays` names is answered.

use iroh::endpoint::{Connection, presets};
use iroh::{Endpoint, RelayMode, SecretKey};
use spoolframe_protocol::codec::{read_frame, write_frame};
use spoolframe_protocol::relay_access::{AccessAnswer, AccessQuery, RELAY_ACCESS_ALPN, Tier};

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
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    write_frame(&mut send, &AccessQuery { endpoint })
        .await
        .unwrap();
    send.finish().unwrap();
    let answer: AccessAnswer = read_frame(&mut recv).await.unwrap();
    answer.tier
}

#[tokio::test]
async fn the_server_and_its_signed_in_devices_are_the_owner_s_and_others_strangers() {
    let key = SecretKey::generate();
    let server = server_for("relay-access-tiers", &key).await;
    server.add_user("alice", PASSWORD, None).await;
    let phone = server.client().await;
    phone.hello().await;
    phone.login("alice", PASSWORD).await;

    let relay = relay_endpoint(key).await;
    let conn = relay
        .connect(server.server.addr(), RELAY_ACCESS_ALPN)
        .await
        .unwrap();
    assert_eq!(
        ask(&conn, server.server.id().to_string()).await,
        Tier::Owner
    );
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
