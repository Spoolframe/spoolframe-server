//! Media read throughput over a real Iroh connection (P1-08). Ignored by
//! default; run by hand, in release, with a large file:
//!
//! ```text
//! SPOOLFRAME_THROUGHPUT_FILE=/path/to/big.mkv \
//!   cargo test --release -p spoolframe-server --test it throughput -- --ignored --nocapture
//! ```
//!
//! `SPOOLFRAME_THROUGHPUT_INFLIGHT` sets how many 16 MiB reads are kept in
//! flight (default 4), as a client would.

use crate::support;

use std::sync::Arc;
use std::time::Instant;

use spoolframe_protocol::{MAX_READ, MediaReadResponse, Reply, Request, Response, Selection};
use support::{TestServer, mkv_capabilities, movie};
use tokio::sync::Semaphore;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "a measurement, run by hand with SPOOLFRAME_THROUGHPUT_FILE"]
async fn read_a_large_file() {
    let source = std::env::var("SPOOLFRAME_THROUGHPUT_FILE")
        .expect("SPOOLFRAME_THROUGHPUT_FILE names the file to read");
    let inflight: usize = std::env::var("SPOOLFRAME_THROUGHPUT_INFLIGHT")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(4);

    let server = TestServer::start("throughput").await;
    std::fs::create_dir_all(server.media.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&source, &server.media).unwrap();
    server.add_user("alice", "correct horse", None).await;
    let client = server.client().await;
    client.hello_with(None, Some(mkv_capabilities())).await;
    let session = client.login("alice", "correct horse").await;
    let Response::Ok(Reply::PlaybackStarted { handle, size, .. }) = client
        .call(
            Some(&session),
            Request::StartPlayback {
                id: movie().id().clone(),
                selection: Some(Selection::default()),
                start_ms: None,
                bitmap: Default::default(),
            },
        )
        .await
    else {
        panic!("did not start");
    };

    let slots = Arc::new(Semaphore::new(inflight));
    let started = Instant::now();
    let mut reads = Vec::new();
    let mut offset = 0;
    while offset < size {
        let permit = slots.clone().acquire_owned().await.unwrap();
        let (client, session, handle) = (client.clone(), session.clone(), handle.clone());
        reads.push(tokio::spawn(async move {
            let (response, bytes) = client.read(&session, &handle, offset, MAX_READ).await;
            drop(permit);
            assert!(matches!(response, MediaReadResponse::Ok(_)));
            bytes.len() as u64
        }));
        offset += MAX_READ;
    }
    let mut total = 0;
    for read in reads {
        total += read.await.unwrap();
    }
    let secs = started.elapsed().as_secs_f64();
    assert_eq!(total, size);
    println!(
        "read {:.2} GiB in {secs:.2} s with {inflight} reads in flight: {:.0} MiB/s ({:.0} Mbit/s)",
        total as f64 / (1u64 << 30) as f64,
        total as f64 / (1u64 << 20) as f64 / secs,
        total as f64 * 8.0 / 1e6 / secs,
    );
    client.close().await;
    server.stop().await;
}
