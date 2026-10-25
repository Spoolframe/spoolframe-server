//! A stream as long as the device's receive window, through a path that
//! loses a packet (PX-62). The server writes a 16 MiB read, as much as one
//! read may be (`MAX_READ`); the device reads none of it until the server
//! has filled the window, as a player whose buffer is ahead does; and one
//! packet of the body is lost on the way. QUIC must hold the whole window
//! and hand it over once the lost packet comes again.
//!
//! The noq-proto release iroh builds on (1.4.0) kept each full-size
//! packet past a gap, or ahead of an unread stretch, as its own buffer and
//! closed the connection with `INTERNAL_ERROR` ("too many gaps in stream
//! buffer") once it held more than 2,048 of them: any window above about
//! 2.4 MiB failed this test, and the 8 MiB case fails against it still.
//! The workspace's noq-proto (vendor/noq-proto) merges them, as quinn does.
//!
//! In-process a lost packet comes again within a millisecond, so the loss
//! alone cannot pile a window's worth behind it as a 300 ms path did: the
//! unread stretch stands in for the round trip.

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use iroh::endpoint::{QuicTransportConfig, VarInt, presets};
use iroh::{Endpoint, EndpointAddr, RelayMode};
use spoolframe_client_sdk::STREAM_WINDOW;
use tokio::net::UdpSocket;

const ALPN: &[u8] = b"spoolframe-test/stream-window/1";
/// As much as one read may send (`spoolframe_protocol::media::MAX_READ`).
const BODY: usize = 16 << 20;
/// Datagrams this long or longer carry the body: QUIC's smallest packet.
/// Acknowledgements and iroh's own messages are far shorter.
const FULL_SIZE: usize = 1200;
/// Which full-size packet from the server is lost once the body is being
/// written. Not the first: just after the handshake the server may still
/// be probing the path's MTU, and that probe is not the body.
const LOST: u64 = 64;
/// No test takes this long unless something hangs.
const HANG: Duration = Duration::from_secs(120);

async fn within<T>(test: impl Future<Output = T>) -> T {
    tokio::time::timeout(HANG, test)
        .await
        .expect("the test hung")
}

/// The path between the device and the server, and what it did.
struct LossyPath {
    /// Where the device dials the server.
    addr: SocketAddr,
    /// Set once the server writes the body: from then on the `LOST`th
    /// full-size datagram from the server is dropped.
    armed: Arc<AtomicBool>,
    /// Datagrams dropped.
    dropped: Arc<AtomicU64>,
    task: tokio::task::JoinHandle<()>,
}

/// A UDP forwarder to `server`. The device is on IPv4 loopback and the
/// server on IPv6 loopback, each with that one socket, so the addresses
/// iroh learns of the other go nowhere and every packet passes here.
async fn lossy_path(server: SocketAddr) -> LossyPath {
    let front = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let addr = front.local_addr().unwrap();
    let armed = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicU64::new(0));
    let task = tokio::spawn({
        let (armed, dropped) = (armed.clone(), dropped.clone());
        async move {
            // One socket towards the server for each device socket, so that
            // its answers go back to where the packet came from.
            let mut backs: HashMap<SocketAddr, Arc<UdpSocket>> = HashMap::new();
            let seen = Arc::new(AtomicU64::new(0));
            let mut buf = vec![0u8; 65_536];
            while let Ok((n, from)) = front.recv_from(&mut buf).await {
                let back = match backs.get(&from) {
                    Some(back) => back.clone(),
                    None => {
                        let back = Arc::new(UdpSocket::bind("[::1]:0").await.unwrap());
                        let (answers, front) = (back.clone(), front.clone());
                        let (armed, seen, dropped) = (armed.clone(), seen.clone(), dropped.clone());
                        tokio::spawn(async move {
                            let mut buf = vec![0u8; 65_536];
                            while let Ok((n, _)) = answers.recv_from(&mut buf).await {
                                if n >= FULL_SIZE
                                    && armed.load(Ordering::SeqCst)
                                    && seen.fetch_add(1, Ordering::SeqCst) + 1 == LOST
                                {
                                    dropped.fetch_add(1, Ordering::SeqCst);
                                    continue;
                                }
                                let _ = front.send_to(&buf[..n], from).await;
                            }
                        });
                        backs.insert(from, back.clone());
                        back
                    }
                };
                let _ = back.send_to(&buf[..n], server).await;
            }
        }
    });
    LossyPath {
        addr,
        armed,
        dropped,
        task,
    }
}

/// A server on IPv6 loopback only, answering `ALPN` with its default
/// transport: the body goes out as the server's does.
async fn server() -> Endpoint {
    Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .alpns(vec![ALPN.to_vec()])
        .clear_ip_transports()
        .bind_addr("[::1]:0".parse::<SocketAddr>().unwrap())
        .unwrap()
        .bind()
        .await
        .unwrap()
}

/// A device on IPv4 loopback only, taking `window` bytes of a stream
/// before it reads, as the SDK's endpoint takes `STREAM_WINDOW`.
async fn device(window: u32) -> Endpoint {
    Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .clear_ip_transports()
        .bind_addr("127.0.0.1:0".parse::<SocketAddr>().unwrap())
        .unwrap()
        .transport_config(
            QuicTransportConfig::builder()
                .stream_receive_window(VarInt::from_u32(window))
                .build(),
        )
        .bind()
        .await
        .unwrap()
}

/// A 16 MiB body read whole by a device with `window` bytes of receive
/// window, which reads nothing until the window is full, over a path that
/// loses one packet of it.
async fn a_window_fills_past_a_lost_packet(window: u32) {
    let server = server().await;
    let path = lossy_path(server.bound_sockets()[0]).await;
    let body: Arc<Vec<u8>> = Arc::new((0..BODY).map(|i| (i % 251) as u8).collect());

    let serving = tokio::spawn({
        let (server, body, armed) = (server.clone(), body.clone(), path.armed.clone());
        async move {
            let conn = server
                .accept()
                .await
                .expect("the device dials")
                .await
                .unwrap();
            let (mut send, mut recv) = conn.accept_bi().await.unwrap();
            let mut request = [0u8; 1];
            recv.read_exact(&mut request).await.unwrap();
            armed.store(true, Ordering::SeqCst);
            let written = send.write_all(&body).await;
            if written.is_ok() {
                send.finish().unwrap();
                // Until the device has read it all, or lost the connection.
                let _ = send.stopped().await;
            }
            (written, conn.stats())
        }
    });

    let device = device(window).await;
    let conn = device
        .connect(EndpointAddr::new(server.id()).with_ip_addr(path.addr), ALPN)
        .await
        .unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send.write_all(&[1]).await.unwrap();

    // Nothing is read until the server has filled the window: what came
    // in is held, every packet of it. The unpatched release closes the
    // connection on the way, which ends the wait as well.
    let deadline = tokio::time::Instant::now() + HANG;
    loop {
        let received = conn.stats().udp_rx.bytes;
        if received >= u64::from(window) || conn.close_reason().is_some() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{received} bytes arrived of the window's {window}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let held = conn.stats().udp_rx.bytes;

    // As `BodyStream::next` reads: 64 KiB at a time.
    let mut got = Vec::with_capacity(BODY);
    let mut buf = vec![0u8; 64 << 10];
    loop {
        match recv.read(&mut buf).await {
            Ok(Some(n)) => got.extend_from_slice(&buf[..n]),
            Ok(None) => break,
            Err(e) => panic!(
                "the read failed after {} bytes, {held} bytes having arrived unread: {e:?}; \
                 the connection closed: {:?}",
                got.len(),
                conn.close_reason()
            ),
        }
    }
    assert_eq!(got.len(), BODY);
    assert!(got == *body, "the body read back differs");

    let (written, server_stats) = serving.await.unwrap();
    assert!(written.is_ok(), "{written:?}");
    assert_eq!(path.dropped.load(Ordering::SeqCst), 1, "one packet is lost");
    assert!(
        server_stats.lost_packets >= 1,
        "the server saw no loss: {server_stats:?}"
    );

    conn.close(VarInt::from_u32(0), b"done");
    device.close().await;
    server.close().await;
    path.task.abort();
}

/// The window the SDK's endpoint takes.
#[tokio::test(flavor = "multi_thread")]
async fn the_sdks_window_fills_past_a_lost_packet() {
    within(a_window_fills_past_a_lost_packet(STREAM_WINDOW)).await;
}

/// The window the SDK took until 0.14.4, which the unpatched release
/// cannot hold: about 5,800 packets of it, of 2,048 allowed.
#[tokio::test(flavor = "multi_thread")]
async fn an_8_mib_window_fills_past_a_lost_packet() {
    within(a_window_fills_past_a_lost_packet(8 << 20)).await;
}
