//! Connections that stop answering without closing (P1-15): every request
//! has a time limit, a connection that does not answer is dropped, a
//! request that only reads is asked again on a new one, and
//! `Client::resume` finds dead connections before a request does.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use spoolframe_client_sdk::types::{ClientCapabilities, Platform};
use spoolframe_client_sdk::{
    Client, ClientOptions, ConnectionState, Device, MemoryStorage, SdkError, Session, Timeouts,
};

use crate::stub::Stub;

/// Short, so a test waits out a timeout in a second, not 15.
const TIMEOUTS: Timeouts = Timeouts {
    request: Duration::from_secs(1),
    ping: Duration::from_millis(300),
};

/// No test takes this long unless something hangs.
const HANG: Duration = Duration::from_secs(60);

/// `test`, failing instead of hanging.
async fn within<T>(test: impl Future<Output = T>) -> T {
    tokio::time::timeout(HANG, test)
        .await
        .expect("the test hung")
}

fn device() -> Device {
    Device {
        name: "phone".into(),
        client_name: "sdk-stall-test".into(),
        client_version: "0".into(),
        platform: Platform::Ios,
        capabilities: ClientCapabilities::default(),
    }
}

async fn client() -> Client {
    // Off the LAN: the stub is given by address.
    let options = ClientOptions {
        timeouts: TIMEOUTS,
        lan_discovery: false,
        ..ClientOptions::default()
    };
    Client::open_with_options(Arc::new(MemoryStorage::default()), device(), options)
        .await
        .unwrap()
}

/// Pin `stub`, as an app does after the fingerprint was checked.
async fn pin(client: &Client, stub: &Stub) {
    client.add_address(&stub.id(), stub.addrs()).unwrap();
    client.connect(&stub.id()).await.unwrap();
    client.pin(&stub.id(), true).await.unwrap();
}

/// A client signed in to `stub`.
async fn signed_in(stub: &Stub) -> (Client, Session) {
    let client = client().await;
    pin(&client, stub).await;
    let alice = client
        .sign_in(&stub.id(), "alice", "pw", false)
        .await
        .unwrap();
    (client, alice)
}

/// Wait until `done`, checking every 20 ms.
async fn until(mut done: impl FnMut() -> bool) {
    while !done() {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

const CODE: &str = "BCDFGHJK";

#[tokio::test(flavor = "multi_thread")]
async fn a_request_to_a_frozen_server_times_out_and_the_next_one_dials_anew() {
    within(async {
        let stub = Stub::start().await;
        let (client, alice) = signed_in(&stub).await;
        stub.freeze();

        let started = Instant::now();
        let approve = alice.approve_device_sign_in(CODE, false).await;
        assert!(
            matches!(approve, Err(SdkError::Unreachable(_))),
            "{approve:?}"
        );
        // Request timeout, dead Ping, then a bounded reconnect attempt.
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "{:?}",
            started.elapsed()
        );

        stub.thaw();
        assert_eq!(alice.whoami().await.unwrap().username, "alice");
        let on = stub.requests_of("who_am_i");
        assert_ne!(on.last(), Some(&0), "asked on the dead connection: {on:?}");
        client.close().await;
        stub.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_on_a_dead_connection_is_asked_again_on_a_new_one() {
    within(async {
        let stub = Stub::start().await;
        let (client, alice) = signed_in(&stub).await;
        stub.kill_open_connections();

        assert!(alice.libraries().await.unwrap().is_empty());
        let on = stub.requests_of("libraries");
        assert_eq!(on.len(), 2, "{on:?}");
        assert_eq!(on[0], 0);
        assert_ne!(on[1], 0);
        client.close().await;
        stub.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_device_code_lookup_retries_the_same_invocation() {
    within(async {
        let stub = Stub::start().await;
        let (client, alice) = signed_in(&stub).await;
        stub.kill_open_connections();
        let lookup = alice.lookup_device_sign_in(CODE).await;
        assert!(lookup.is_ok(), "{lookup:?}");
        alice.whoami().await.unwrap();
        assert_eq!(stub.count("lookup_device_sign_in"), 2);
        let sent = stub.invocations("lookup_device_sign_in");
        assert!(sent[0].operation.is_some());
        assert_eq!(sent[0], sent[1]);
        client.close().await;
        stub.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn binary_replies_are_bounded_and_must_match_their_announced_length() {
    use spoolframe_client_sdk::types::{ArtworkId, Namespace};
    use spoolframe_client_sdk::{ArtworkSize, MAX_READ, PlaybackHandle};
    within(async {
        let stub = Stub::start().await;
        let (client, alice) = signed_in(&stub).await;
        let handle = PlaybackHandle("stub".into());
        // Both the protocol maximum and the requested length are enforced,
        // before reserving memory. Huge values used to panic or abort.
        for (announced, requested) in [(u64::MAX, MAX_READ), (MAX_READ + 1, u64::MAX), (5, 4)] {
            stub.body(announced, &[]);
            let read = alice.read(&handle, 0, requested).await;
            assert!(matches!(read, Err(SdkError::Protocol(_))), "{read:?}");
        }
        for (announced, body) in [(4, &b"abc"[..]), (2, &b"abc"[..])] {
            stub.body(announced, body);
            let read = alice.read(&handle, 0, 4).await;
            assert!(matches!(read, Err(SdkError::Protocol(_))), "{read:?}");
        }
        stub.body(3, b"abc");
        assert_eq!(alice.read(&handle, 0, 4).await.unwrap(), b"abc");
        stub.body(0, &[]);
        assert!(alice.read(&handle, 0, 0).await.unwrap().is_empty());
        let image = ArtworkId::new(Namespace::Plex, "/library/metadata/1/thumb/1").unwrap();
        for announced in [u64::MAX, (64 << 20) + 1] {
            stub.body(announced, &[]);
            let read = alice.artwork(&image, ArtworkSize::Original).await;
            assert!(matches!(read, Err(SdkError::Protocol(_))), "{read:?}");
        }
        stub.body(3, b"png");
        assert_eq!(
            alice.artwork(&image, ArtworkSize::Original).await.unwrap(),
            b"png"
        );
        client.close().await;
        stub.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_approval_on_a_dead_connection_is_retried() {
    within(async {
        let stub = Stub::start().await;
        let (client, alice) = signed_in(&stub).await;
        stub.kill_open_connections();

        let approve = alice.approve_device_sign_in(CODE, false).await;
        assert!(approve.is_ok(), "{approve:?}");
        // The dead connection was dropped: the next request works.
        alice.whoami().await.unwrap();
        assert_eq!(stub.count("approve_device_sign_in"), 2);
        client.close().await;
        stub.stop().await;
    })
    .await;
}

/// The server is alive and slow at one thing: the request times out, and
/// the connection, which answers its `Ping`, is kept and not asked again.
#[tokio::test(flavor = "multi_thread")]
async fn a_slow_answer_on_a_live_connection_keeps_the_connection() {
    within(async {
        let stub = Stub::start().await;
        let (client, alice) = signed_in(&stub).await;
        stub.stall("libraries");

        let libraries = alice.libraries().await;
        assert!(
            matches!(libraries, Err(SdkError::TimedOut(_))),
            "{libraries:?}"
        );
        assert_eq!(stub.count("libraries"), 1);
        alice.whoami().await.unwrap();
        assert_eq!(stub.connections(), 1);
        client.close().await;
        stub.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_drops_a_dead_connection_and_keeps_a_live_one() {
    within(async {
        let dead = Stub::start().await;
        let live = Stub::start().await;
        let (client, on_dead) = signed_in(&dead).await;
        pin(&client, &live).await;
        dead.kill_open_connections();

        let started = Instant::now();
        client.resume().await;
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );

        assert_eq!(live.count("ping"), 1);
        assert_eq!(live.connections(), 1);
        assert_eq!(dead.requests_of("ping"), [0]);
        // Dropped, and dialled again in the background.
        until(|| dead.connections() == 2).await;
        let mut state = client.state(&dead.id()).unwrap();
        while state.current() != ConnectionState::Connected {
            state.changed().await.unwrap();
        }
        // So the next request does not wait out a time limit.
        let started = Instant::now();
        on_dead.whoami().await.unwrap();
        assert!(
            started.elapsed() < TIMEOUTS.request,
            "{:?}",
            started.elapsed()
        );
        assert_eq!(dead.requests_of("who_am_i").last(), Some(&1));
        client.close().await;
        dead.stop().await;
        live.stop().await;
    })
    .await;
}

/// The app comes back and sends a request as it calls `resume`: the
/// request waits for the check, and goes on the new connection, not on
/// the dead one, where it would wait out its time limit.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_during_resume_waits_for_the_check() {
    within(async {
        let stub = Stub::start().await;
        let (client, alice) = signed_in(&stub).await;
        let before = stub.count("who_am_i");
        stub.kill_open_connections();

        let resume = tokio::spawn({
            let client = client.clone();
            async move { client.resume().await }
        });
        // The check is on its way.
        until(|| stub.count("ping") == 1).await;
        let started = Instant::now();
        alice.whoami().await.unwrap();
        assert!(
            started.elapsed() < TIMEOUTS.request,
            "{:?}",
            started.elapsed()
        );
        resume.await.unwrap();
        let on = stub.requests_of("who_am_i");
        assert_eq!(&on[before..], [1], "{on:?}");
        client.close().await;
        stub.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_does_not_dial_a_server_with_no_connection() {
    within(async {
        let stub = Stub::start().await;
        let client = client().await;
        client.add_address(&stub.id(), stub.addrs()).unwrap();
        // Known to the client, its state watched, never connected.
        let state = client.state(&stub.id()).unwrap();

        client.resume().await;
        // Long enough for a dial on loopback to have arrived.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(stub.connections(), 0);
        assert_eq!(state.current(), ConnectionState::Disconnected);
        client.close().await;
        stub.stop().await;
    })
    .await;
}

/// A segment read as a stream hands on its first bytes while the rest is
/// still on its way (PX-46): the stub sends 1 MiB 64 KiB at a time, a
/// quarter second apart, about 4 s in all. The first piece comes long
/// before the last, the bytes are the segment's, and the link meter has
/// a reading from them, as it has from the same segment read whole.
#[tokio::test(flavor = "multi_thread")]
async fn a_streamed_segment_hands_on_its_first_bytes_before_the_rest() {
    use spoolframe_client_sdk::PlaybackHandle;
    const PIECE: usize = 64 << 10;
    const EVERY: Duration = Duration::from_millis(250);
    within(async {
        let stub = Stub::start().await;
        let segment: Vec<u8> = (0..(1u32 << 20)).map(|i| (i % 251) as u8).collect();
        stub.body(segment.len() as u64, &segment);
        stub.trickle(PIECE, EVERY);
        let handle = PlaybackHandle("stub".into());

        let (client, alice) = signed_in(&stub).await;
        let started = Instant::now();
        let mut stream = alice
            .read_segment_stream(&handle, 0, 0, 3, 0, segment.len() as u64)
            .await
            .unwrap();
        assert_eq!(stream.sent, segment.len() as u64);
        assert!(stream.complete);
        let first = stream.next().await.unwrap().expect("a first piece");
        let first_at = started.elapsed();
        let mut bytes = first;
        while let Some(piece) = stream.next().await.unwrap() {
            bytes.extend_from_slice(&piece);
        }
        let all_at = started.elapsed();
        assert_eq!(bytes, segment);
        assert_eq!(stream.received(), segment.len() as u64);
        assert!(
            first_at < Duration::from_secs(1),
            "first piece after {first_at:?}"
        );
        assert!(all_at >= Duration::from_secs(3), "whole after {all_at:?}");
        // 1 MiB less its first piece over about 3.75 s: about 2 Mbit/s.
        let streamed = alice.link_rate().expect("a reading from the stream");
        assert!(
            (1_500_000..3_000_000).contains(&streamed),
            "{streamed} bit/s"
        );
        client.close().await;

        // The same segment read whole feeds the meter the same way.
        let (client, alice) = signed_in(&stub).await;
        let chunk = alice
            .read_segment(&handle, 0, 0, 3, 0, segment.len() as u64)
            .await
            .unwrap();
        assert_eq!(chunk.bytes, segment);
        let whole = alice.link_rate().expect("a reading from the read");
        assert!((1_500_000..3_000_000).contains(&whole), "{whole} bit/s");
        client.close().await;
        stub.stop().await;
    })
    .await;
}
