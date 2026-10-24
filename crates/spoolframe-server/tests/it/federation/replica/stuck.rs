//! C8-05: a paired server that takes SB's connection and never lets its
//! Hello through. It grants no stream, so SB's dial waits on it; nothing
//! else of SB's may wait with it.

use iroh::endpoint::{QuicTransportConfig, VarInt};
use spoolframe_server::store::federation::NewPeer;

use super::*;

/// The stuck pair's server id.
const STUCK_ID: &str = "ssssssssssssssss";

/// An endpoint on the federation ALPN granting no stream, holding every
/// connection made to it: told on `held` as each comes.
async fn stuck() -> (Endpoint, tokio::sync::mpsc::UnboundedReceiver<()>) {
    let endpoint = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .alpns(vec![FEDERATION_ALPN.to_vec()])
        .transport_config(
            QuicTransportConfig::builder()
                .max_concurrent_bidi_streams(VarInt::from_u32(0))
                .build(),
        )
        .bind()
        .await
        .unwrap();
    let (held, told) = tokio::sync::mpsc::unbounded_channel();
    let answering = endpoint.clone();
    tokio::spawn(async move {
        let mut connections = Vec::new();
        while let Some(incoming) = answering.accept().await {
            if let Ok(connection) = incoming.await {
                connections.push(connection);
                let _ = held.send(());
            }
        }
    });
    (endpoint, told)
}

/// SB, and the stuck server paired with it as SB's row says; once SB's
/// replica holds a dial to it, waiting on its Hello.
async fn sb_dialling_a_stuck_pair(test: &str) -> (TestServer, Endpoint, String) {
    let (sb, _) = sb_in(fresh_dir(test).await).await;
    let (stuck, mut held) = stuck().await;
    let addrs: Vec<String> = stuck.addr().ip_addrs().map(|a| a.to_string()).collect();
    let peer = sb
        .store
        .add_peer(
            NewPeer {
                server_id: STUCK_ID,
                endpoint_id: &stuck.id().to_string(),
                display_name: "Stuck server",
                addrs: &addrs,
                relays: &[],
            },
            "bob",
        )
        .await
        .unwrap();
    // The replica's next tick dials it.
    tokio::time::timeout(Duration::from_secs(60), held.recv())
        .await
        .expect("SB dials the stuck server");
    (sb, stuck, peer.id)
}

/// With the replica's dial of one pair stuck on its Hello, another pair
/// is dialled and synced, and the upkeep goes on: an expired token goes.
#[tokio::test]
async fn a_pair_that_never_answers_holds_up_no_other_pair_nor_the_upkeep() {
    let (sb, _stuck, stuck_peer) = sb_dialling_a_stuck_pair("fed-stuck-others").await;
    let pool = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::new()
            .filename(sb.dir.join(spoolframe_server::store::DB_FILE)),
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO federation_tokens \
             (token_hash, peer, principal, item, library, device, scope, created_at, expires_at) \
         VALUES ('expired', ?1, 'remote:x:y', 'plex:movie:1', 'plex:1', 'd', 'play', 0, 1)",
    )
    .bind(&stuck_peer)
    .execute(&pool)
    .await
    .unwrap();
    let tokens = || async {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM federation_tokens")
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    assert_eq!(tokens().await, 1);
    // Paired now: the replica dials it at once.
    let sa = TestServer::start_with(
        fresh_dir("fed-stuck-others-sa").await,
        ServerOptions {
            name: "Alice's server".into(),
            federation: true,
            ..ServerOptions::default()
        },
    )
    .await;
    federation(&sb)
        .redeem_link(&pairing_link(&sa, None).await, "bob")
        .await
        .unwrap();
    let origin = sa.server_id.to_string();
    // Within 10 s: before the stuck Hello is given up on, at 15 s, so the
    // others did not wait for it to fail.
    tokio::time::timeout(Duration::from_secs(10), async {
        while !federation(&sb).online(&origin) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("SA dialled beside the stuck dial");
    // The upkeep's tick is 15 s: within two.
    tokio::time::timeout(Duration::from_secs(30), async {
        while tokens().await > 0 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the expired token pruned beside the stuck dial");
    sa.stop().await;
    sb.stop().await;
}

/// Unpairing the stuck pair, while SB's dial to it waits on its Hello,
/// is done at once: it waits for a sync in flight, never for a dial.
#[tokio::test]
async fn unpairing_a_pair_whose_hello_hangs_is_done_at_once() {
    let (sb, _stuck, stuck_peer) = sb_dialling_a_stuck_pair("fed-stuck-unpair").await;
    let unpaired = tokio::time::timeout(
        Duration::from_secs(5),
        federation(&sb).unpair(&stuck_peer, "bob"),
    )
    .await
    .expect("not held up by the dial")
    .unwrap();
    assert!(unpaired.is_some());
    assert!(!federation(&sb).online(STUCK_ID));
    sb.stop().await;
}
