//! PX-46: the WAN uplink budget leaves the LAN alone. The test client
//! dials over loopback, which is the LAN, so these show what a device at
//! home sees with a budget set; the share's arithmetic and the WAN side
//! are `spoolframe_server::uplink`'s unit tests.

use crate::support::{self, ServerOptions, TestServer, mkv_capabilities, movie};
use spoolframe_protocol::{PlaybackPlan, Reply, Request, Response, Selection};

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
