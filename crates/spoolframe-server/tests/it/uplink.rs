//! PX-46: the WAN uplink budget leaves the LAN alone. The test client
//! dials over loopback, which is the LAN, so these show what a device at
//! home sees with a budget set; the share's arithmetic and the WAN side
//! are `spoolframe_server::uplink`'s unit tests.

use std::net::{Ipv4Addr, Ipv6Addr};

use std::sync::Arc;
use std::time::Duration;

use crate::sdk::{client_on, settled_path};
use crate::support::{self, ServerOptions, TestClient, TestServer, mkv_capabilities, movie};
use spoolframe_client_sdk::{
    BitmapSubtitles, Client, ConnectionPath, MemoryStorage, PlaybackOptions, PlaybackQuality,
    Session,
};
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
    // In a namespace of its own, so ::1 is there even where the host or
    // CI's container has no IPv6.
    if !support::private_netns(
        module_path!(),
        "a_connection_with_one_path_and_none_selected_is_judged_by_that_path",
    ) {
        return;
    }
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
    // In a namespace of its own, so ::1 is there even where the host or
    // CI's container has no IPv6.
    if !support::private_netns(
        module_path!(),
        "an_unselected_path_off_the_lan_is_held_to_the_wan_limit",
    ) {
        return;
    }
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

/// Alice's session through the SDK on a server run with `settings`, its
/// path chosen, so the server has it to judge.
async fn signed_in(test: &str, settings: UplinkSettings) -> (TestServer, Client, Session) {
    let server = TestServer::start_with(
        support::fresh_dir(test).await,
        ServerOptions {
            uplink: settings,
            ..Default::default()
        },
    )
    .await;
    server.write_media(1000);
    server.add_user("alice", "correct horse", None).await;
    let client = client_on(&server, &Arc::new(MemoryStorage::default())).await;
    let alice = client
        .sign_in(
            &server.server.id().to_string(),
            "alice",
            "correct horse",
            false,
        )
        .await
        .unwrap();
    settled_path(&alice, Duration::from_secs(5)).await;
    (server, client, alice)
}

/// The test's loopback, listed as an operator lists a WireGuard peer.
fn loopback_listed_as_wan() -> UplinkSettings {
    UplinkSettings {
        wan_prefixes: vec!["127.0.0.0/8".parse().unwrap(), "::1/128".parse().unwrap()],
        ..UplinkSettings::default()
    }
}

/// A WireGuard peer: an address the device sees as private, which the
/// server's `wan_prefixes` lists. The device's own reading stays `Lan`;
/// the playback says what the server counted, at its start and on each
/// re-plan.
#[tokio::test]
async fn a_private_address_in_wan_prefixes_is_told_as_off_the_lan() {
    // In a namespace of its own, where loopback is the only way there: on
    // the host the SDK may go by another of the server's addresses.
    if !support::private_netns(
        module_path!(),
        "a_private_address_in_wan_prefixes_is_told_as_off_the_lan",
    ) {
        return;
    }
    let (server, client, alice) = signed_in("uplink-verdict-wan", loopback_listed_as_wan()).await;
    let playback = alice
        .open_playback(movie().id(), None, PlaybackOptions::default())
        .await
        .unwrap();
    assert_eq!(playback.snapshot().wan, Some(true));
    assert!(
        matches!(alice.connection_path(), ConnectionPath::Lan { .. }),
        "{:?}",
        alice.connection_path()
    );
    let handle = playback.snapshot().handle;
    // The same plan, by the path that sends nothing new to the player.
    let same = alice
        .change_quality(
            &handle,
            PlaybackQuality::Auto,
            BitmapSubtitles::Auto,
            Duration::ZERO,
        )
        .await
        .unwrap();
    assert_eq!(same.wan, Some(true));
    let subtitle = alice
        .change_subtitle(&handle, None, BitmapSubtitles::Auto, Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(subtitle.wan, Some(true));
    client.close().await;
    server.stop().await;
}

/// The same device with nothing listed is on the LAN, and is told so: not
/// `None`, which is an older server's.
#[tokio::test]
async fn a_plain_lan_play_is_told_as_on_the_lan() {
    // As above, the same device on the same path.
    if !support::private_netns(module_path!(), "a_plain_lan_play_is_told_as_on_the_lan") {
        return;
    }
    let (server, client, alice) = signed_in("uplink-verdict-lan", UplinkSettings::default()).await;
    let playback = alice
        .open_playback(movie().id(), None, PlaybackOptions::default())
        .await
        .unwrap();
    assert_eq!(playback.snapshot().wan, Some(false));
    let same = alice
        .change_quality(
            &playback.snapshot().handle,
            PlaybackQuality::Auto,
            BitmapSubtitles::Auto,
            Duration::ZERO,
        )
        .await
        .unwrap();
    assert_eq!(same.wan, Some(false));
    client.close().await;
    server.stop().await;
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
