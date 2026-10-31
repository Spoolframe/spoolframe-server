//! PX-46: the WAN uplink budget leaves the LAN alone. The test client
//! dials over loopback, which is the LAN, so these show what a device at
//! home sees with a budget set; the share's arithmetic and the WAN side
//! are `spoolframe_server::uplink`'s unit tests.

use std::net::{Ipv4Addr, Ipv6Addr};

use crate::support::{self, ServerOptions, TestClient, TestServer, mkv_capabilities, movie};
use spoolframe_protocol::{Error, PlaybackPlan, Reply, Request, Response, Selection, SessionId};
use spoolframe_server::uplink::UplinkSettings;

/// A budget of 1 Mbit/s and one stream at most, on a server whose only
/// devices are on the LAN: two of them play at once, each as planned
/// with no budget at all.
#[tokio::test]
async fn a_device_on_the_lan_is_neither_counted_nor_capped() {
    let server = TestServer::start_with(
        support::fresh_dir("uplink-lan").await,
        ServerOptions {
            uplink: spoolframe_server::uplink::UplinkSettings {
                budget: Some(1_000_000),
                max_streams: Some(1),
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .await;
    server.write_media(1000);
    server.add_user("alice", "correct horse", None).await;
    let mut plans = Vec::new();
    for _ in 0..2 {
        let client = server.client().await;
        client.hello_with(None, Some(mkv_capabilities())).await;
        let session = client.login("alice", "correct horse").await;
        let response = client
            .call(
                Some(&session),
                Request::StartPlayback {
                    id: movie().id().clone(),
                    selection: Some(Selection::default()),
                    start_ms: None,
                    bitmap: Default::default(),
                    quality: Default::default(),
                    display: None,
                    capabilities: None,
                    onward_share: None,
                },
            )
            .await;
        let Response::Ok(Reply::PlaybackStarted { plan, .. }) = response else {
            panic!("a second LAN playback was refused: {response:?}");
        };
        plans.push(plan);
    }
    // Auto on the LAN is the original: played directly, as with no budget.
    for plan in plans {
        assert!(matches!(plan, PlaybackPlan::Direct { .. }), "{plan:?}");
    }
}

/// `new`, the second connection of a device whose first, `old`, keeps
/// iroh's path selected: iroh selects one per device, not per connection,
/// and the first socket's, on IPv6, stays, as iroh prefers IPv6 at about
/// the same round trip. `new` has one path, which is not it, and none
/// selected until `old` goes.
async fn second_socket(server: &TestServer) -> (TestClient, TestClient, SessionId) {
    let key = iroh::SecretKey::generate();
    let old = server
        .loopback_device(key.clone(), Ipv6Addr::LOCALHOST.into())
        .await;
    old.hello_with(None, Some(mkv_capabilities())).await;
    let new = server
        .loopback_device(key, Ipv4Addr::LOCALHOST.into())
        .await;
    new.hello_with(None, Some(mkv_capabilities())).await;
    let session = new.login("alice", "correct horse").await;
    (old, new, session)
}

async fn start(client: &TestClient, session: &SessionId) -> Response {
    client
        .call(
            Some(session),
            Request::StartPlayback {
                id: movie().id().clone(),
                selection: Some(Selection::default()),
                start_ms: None,
                bitmap: Default::default(),
                quality: Default::default(),
                display: None,
                capabilities: None,
                onward_share: None,
            },
        )
        .await
}

/// As an app that reconnects before its old connection is gone: on the
/// LAN all the same, its one path says so, and it is neither counted nor
/// capped. The second of its playbacks is not refused.
#[tokio::test]
async fn a_connection_with_one_path_and_none_selected_is_judged_by_that_path() {
    let server = one_stream("uplink-unselected", UplinkSettings::default()).await;
    let (_old, new, session) = second_socket(&server).await;
    for _ in 0..2 {
        let response = start(&new, &session).await;
        assert!(
            matches!(response, Response::Ok(Reply::PlaybackStarted { .. })),
            "{response:?}"
        );
    }
}

/// The same connection off the LAN stays under the WAN limit: its one path
/// is judged, not assumed.
#[tokio::test]
async fn an_unselected_path_off_the_lan_is_held_to_the_wan_limit() {
    let wan = UplinkSettings {
        wan_prefixes: vec!["127.0.0.0/8".parse().unwrap()],
        ..UplinkSettings::default()
    };
    let server = one_stream("uplink-unselected-wan", wan).await;
    let (_old, new, session) = second_socket(&server).await;
    let first = start(&new, &session).await;
    assert!(
        matches!(first, Response::Ok(Reply::PlaybackStarted { .. })),
        "{first:?}"
    );
    let second = start(&new, &session).await;
    assert!(
        matches!(second, Response::Err(Error::WanStreamsFull { .. })),
        "{second:?}"
    );
}

/// `settings` with one stream at most, a budget it fits, and alice.
async fn one_stream(test: &str, settings: UplinkSettings) -> TestServer {
    let server = TestServer::start_with(
        support::fresh_dir(test).await,
        ServerOptions {
            uplink: UplinkSettings {
                budget: Some(1_000_000_000),
                max_streams: Some(1),
                ..settings
            },
            ..Default::default()
        },
    )
    .await;
    server.write_media(1000);
    server.add_user("alice", "correct horse", None).await;
    server
}
