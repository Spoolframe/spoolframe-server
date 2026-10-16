//! Keyframes read ahead of any play (`experimental.index_ahead`): a film
//! in a transport stream, which has no index, is read whole before anyone
//! plays it, with the switch on, and only then.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::support::{ServerOptions, TestServer, fresh_dir};

/// 10 s of a small test pattern in MPEG-TS, where the test server's
/// catalog has its movie (named `.mkv`; what the file is decides, not its
/// name).
fn transport_stream(dir: &Path) -> PathBuf {
    let film = dir.join("media").join("movie.mkv");
    std::fs::create_dir_all(film.parent().unwrap()).unwrap();
    let status = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", "testsrc2=size=160x90:rate=25"])
        .args([
            "-t",
            "10",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-g",
            "50",
        ])
        .args(["-f", "mpegts"])
        .arg(&film)
        .status()
        .expect("ffmpeg runs");
    assert!(status.success(), "ffmpeg could not write the test film");
    film
}

/// `spoolframe_index_ahead_files_total{outcome}` now.
fn files(server: &TestServer, outcome: &str) -> f64 {
    server
        .metrics
        .families()
        .into_iter()
        .find(|f| f.name == "spoolframe_index_ahead_files_total")
        .and_then(|f| f.value(&[("outcome", outcome)]))
        .expect("the family is there from the start")
}

/// The keyframe lists kept in the server's cache.
fn kept(server: &TestServer) -> usize {
    std::fs::read_dir(server.dir.join("cache").join("keyframes"))
        .map(|d| d.count())
        .unwrap_or(0)
}

async fn start(test: &str, on: bool) -> TestServer {
    let dir = fresh_dir(test).await;
    transport_stream(&dir);
    TestServer::start_with(
        dir,
        ServerOptions {
            real_media: true,
            index_ahead: on,
            ..ServerOptions::default()
        },
    )
    .await
}

#[tokio::test]
async fn a_transport_stream_is_read_ahead_with_the_switch_on() {
    let server = start("on", true).await;
    let deadline = Instant::now() + Duration::from_secs(60);
    while files(&server, "scanned") < 1.0 {
        assert!(Instant::now() < deadline, "nothing read ahead");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(kept(&server), 1);
    assert_eq!(files(&server, "failed"), 0.0);
    server.stop().await;
}

#[tokio::test]
async fn nothing_is_read_ahead_with_the_switch_off() {
    let server = start("off", false).await;
    // As long as a read with the switch on takes, and more.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(files(&server, "scanned"), 0.0);
    assert_eq!(kept(&server), 0);
    server.stop().await;
}
