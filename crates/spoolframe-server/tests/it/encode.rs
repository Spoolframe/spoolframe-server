//! Encode jobs for converted playback (P2-03), with real ffmpeg and
//! ffprobe on generated media: an H.264 + AC-3 Matroska file, for a device
//! that plays H.264 and AAC from a progressive stream, so the audio is
//! converted and the video copied.

use std::path::{Path, PathBuf};
use std::time::Duration;

use spoolframe_protocol::{
    Error, MAX_READ, MediaReadResponse, PlaybackHandle, PlaybackPlan, Reply, Request, Response,
    SessionId,
};

use spoolframe_protocol::{ManifestKind, MediaRead};
use spoolframe_types::{ClientCapabilities, VideoLimits};

use crate::support::{
    PLAYBACK_IDLE, ServerOptions, TestClient, TestServer, fresh_dir, hls_capabilities, movie,
    progressive_capabilities,
};

async fn ready(
    test: &str,
    options: ServerOptions,
    secs: u32,
) -> (TestServer, TestClient, SessionId) {
    let server = TestServer::start_with(
        fresh_dir(test).await,
        ServerOptions {
            real_media: true,
            ..options
        },
    )
    .await;
    server.generate_media(secs, "libx264", "ac3");
    server.add_user("alice", "correct horse", None).await;
    let client = server.client().await;
    client
        .hello_with(None, Some(progressive_capabilities()))
        .await;
    let session = client.login("alice", "correct horse").await;
    (server, client, session)
}

async fn start(client: &TestClient, session: &SessionId, start_ms: Option<u64>) -> Response {
    client
        .call(
            Some(session),
            Request::StartPlayback {
                id: movie().id().clone(),
                selection: None,
                start_ms,
                bitmap: Default::default(),
            },
        )
        .await
}

async fn started(
    client: &TestClient,
    session: &SessionId,
    start_ms: Option<u64>,
) -> PlaybackHandle {
    match start(client, session, start_ms).await {
        Response::Ok(Reply::PlaybackStarted {
            handle,
            plan: PlaybackPlan::Converted { .. },
            epoch: 0,
            ..
        }) => handle,
        other => panic!("no converted playback: {other:?}"),
    }
}

/// The ffmpeg processes working for `handle`.
fn ffmpeg_for(handle: &PlaybackHandle) -> Vec<u32> {
    std::fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            std::fs::read(format!("/proc/{pid}/cmdline"))
                .is_ok_and(|cmd| String::from_utf8_lossy(&cmd).contains(&handle.0))
        })
        .collect()
}

async fn until(what: &str, within: Duration, mut done: impl FnMut() -> bool) {
    tokio::time::timeout(within, async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("not within {within:?}: {what}"));
}

/// The first and last packet times in `file`, in seconds. Live Matroska
/// has no duration in its header: it is written as it grows.
fn probe(file: &Path) -> (f64, f64) {
    let out = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "packet=pts_time"])
        .args(["-of", "csv=p=0"])
        .arg(file)
        .output()
        .unwrap();
    let times: Vec<f64> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().trim_end_matches(',').parse().ok())
        .collect();
    assert!(!times.is_empty(), "no packets in {}", file.display());
    let first = times.iter().copied().fold(f64::INFINITY, f64::min);
    let last = times.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    (first, last)
}

/// For tests that read the output only once it is finished: nobody reads
/// meanwhile, so the throttle must not hold the encode back.
fn unthrottled() -> ServerOptions {
    ServerOptions {
        read_ahead: Duration::from_secs(3600),
        ..ServerOptions::default()
    }
}

fn output(server: &TestServer, handle: &PlaybackHandle) -> PathBuf {
    server.job_dir(handle, 0).join("out.mkv")
}

#[tokio::test]
async fn a_converted_playback_encodes_into_its_own_directory() {
    let (server, client, session) = ready("encode-output", unthrottled(), 8).await;
    let handle = started(&client, &session, None).await;
    let file = output(&server, &handle);
    until("the encode finishes", Duration::from_secs(20), || {
        file.exists() && ffmpeg_for(&handle).is_empty()
    })
    .await;
    let (first, last) = probe(&file);
    assert!(first < 0.1, "first packet at {first}");
    assert!((last - 8.0).abs() < 0.5, "last packet at {last}");
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn stopping_a_playback_kills_its_encode_and_removes_its_files() {
    let options = ServerOptions {
        // Holds the job still, so it is surely running when stopped.
        read_ahead: Duration::from_millis(500),
        ..ServerOptions::default()
    };
    let (server, client, session) = ready("encode-stop", options, 120).await;
    let handle = started(&client, &session, None).await;
    until("ffmpeg runs", Duration::from_secs(10), || {
        !ffmpeg_for(&handle).is_empty()
    })
    .await;
    assert_eq!(
        client
            .call(
                Some(&session),
                Request::StopPlayback {
                    handle: handle.clone()
                }
            )
            .await,
        Response::Ok(Reply::Done)
    );
    until("ffmpeg is gone", Duration::from_secs(5), || {
        ffmpeg_for(&handle).is_empty()
    })
    .await;
    assert!(!server.job_dir(&handle, 0).parent().unwrap().exists());
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn an_idle_playback_s_encode_ends_with_it() {
    let options = ServerOptions {
        read_ahead: Duration::from_millis(500),
        ..ServerOptions::default()
    };
    let (server, client, session) = ready("encode-idle", options, 120).await;
    let handle = started(&client, &session, None).await;
    until("ffmpeg runs", Duration::from_secs(10), || {
        !ffmpeg_for(&handle).is_empty()
    })
    .await;
    // Idle past the limit, as the server's own sweep finds it.
    server.clock.advance(PLAYBACK_IDLE);
    server.debug.playbacks.sweep();
    until(
        "the idle playback's ffmpeg is gone",
        Duration::from_secs(10),
        || ffmpeg_for(&handle).is_empty(),
    )
    .await;
    assert!(!server.job_dir(&handle, 0).exists());
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn one_conversion_too_many_is_busy() {
    let options = ServerOptions {
        max_jobs: 1,
        read_ahead: Duration::from_millis(500),
        ..ServerOptions::default()
    };
    let (server, tv, session) = ready("encode-busy", options, 120).await;
    // Another device, signed in first: a sign-in waits its turn for
    // Argon2 (PX-10), and the TV's playback idles out after PLAYBACK_IDLE.
    let desktop = server.client().await;
    desktop
        .hello_with(None, Some(progressive_capabilities()))
        .await;
    let other = desktop.login("alice", "correct horse").await;
    started(&tv, &session, None).await;
    // While the TV's encode holds the only slot.
    assert!(matches!(
        start(&desktop, &other, None).await,
        Response::Err(Error::Busy)
    ));
    tv.close().await;
    desktop.close().await;
    server.stop().await;
}

/// An app killed mid-film and opened again starts the film again: its
/// abandoned encode must not hold the slot the new one needs. With one
/// slot the device cannot keep "now + next", and the new start wins.
#[tokio::test]
async fn a_device_starting_again_ends_its_earlier_encode() {
    let options = ServerOptions {
        max_jobs: 1,
        read_ahead: Duration::from_millis(500),
        ..ServerOptions::default()
    };
    let (server, tv, session) = ready("encode-restart", options, 120).await;
    let first = started(&tv, &session, None).await;
    until("ffmpeg runs", Duration::from_secs(10), || {
        !ffmpeg_for(&first).is_empty()
    })
    .await;
    let restarted = tv
        .reconnect_with(&server, Some(progressive_capabilities()))
        .await;
    let second = started(&restarted, &session, None).await;
    assert!(ffmpeg_for(&first).is_empty());
    assert!(!server.job_dir(&first, 0).exists());
    until("the new ffmpeg runs", Duration::from_secs(10), || {
        !ffmpeg_for(&second).is_empty()
    })
    .await;
    tv.close().await;
    restarted.close().await;
    server.stop().await;
}

/// A killed app that does not come back: its encode ends once its
/// connection is gone past the grace, not when the playback idles out.
#[tokio::test]
async fn a_gone_device_s_encode_ends_after_the_grace() {
    let options = ServerOptions {
        max_jobs: 1,
        read_ahead: Duration::from_millis(500),
        disconnect_grace: Duration::from_millis(300),
        ..ServerOptions::default()
    };
    let (server, tv, session) = ready("encode-gone", options, 120).await;
    let handle = started(&tv, &session, None).await;
    until("ffmpeg runs", Duration::from_secs(10), || {
        !ffmpeg_for(&handle).is_empty()
    })
    .await;
    tv.close().await;
    until(
        "the gone device's ffmpeg is gone",
        Duration::from_secs(10),
        || ffmpeg_for(&handle).is_empty(),
    )
    .await;
    // The slot is free for someone else.
    let desktop = server.client().await;
    desktop
        .hello_with(None, Some(progressive_capabilities()))
        .await;
    let other = desktop.login("alice", "correct horse").await;
    started(&desktop, &other, None).await;
    desktop.close().await;
    server.stop().await;
}

/// A device that reconnects within the grace keeps its playback.
#[tokio::test]
async fn a_device_back_within_the_grace_keeps_its_encode() {
    let options = ServerOptions {
        read_ahead: Duration::from_millis(500),
        disconnect_grace: Duration::from_millis(500),
        ..ServerOptions::default()
    };
    let (server, tv, session) = ready("encode-back", options, 120).await;
    let handle = started(&tv, &session, None).await;
    until("ffmpeg runs", Duration::from_secs(10), || {
        !ffmpeg_for(&handle).is_empty()
    })
    .await;
    let back = tv.reconnect(&server).await;
    // The old connection closes; the device (its endpoint) stays.
    tv.conn.close(0u32.into(), b"replaced");
    tokio::time::sleep(Duration::from_millis(1000)).await;
    // Still its playback: it reads on from where it was.
    let (response, _) = back.read_at(&session, &handle, 0, 0, 4096).await;
    assert!(matches!(response, MediaReadResponse::Ok(_)), "{response:?}");
    back.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_start_position_keeps_the_film_s_own_time() {
    let (server, client, session) = ready("encode-start", unthrottled(), 20).await;
    let handle = started(&client, &session, Some(10_000)).await;
    let file = output(&server, &handle);
    until("the encode finishes", Duration::from_secs(20), || {
        file.exists() && ffmpeg_for(&handle).is_empty()
    })
    .await;
    let (start, last) = probe(&file);
    // From the keyframe at or before 10 s, in the film's own time.
    assert!((8.0..=10.5).contains(&start), "first packet at {start}");
    assert!((last - 20.0).abs() < 0.5, "last packet at {last}");
    client.close().await;
    server.stop().await;
}

/// Read epoch `epoch` of `handle` to its end; the bytes, and the replies'
/// `complete` flags in order.
async fn read_all(
    client: &TestClient,
    session: &SessionId,
    handle: &PlaybackHandle,
    epoch: u32,
) -> (Vec<u8>, Vec<bool>) {
    let mut bytes = Vec::new();
    let mut flags = Vec::new();
    loop {
        let (response, chunk) = client
            .read_at(session, handle, epoch, bytes.len() as u64, MAX_READ)
            .await;
        let MediaReadResponse::Ok(reply) = response else {
            panic!("read failed: {response:?}");
        };
        assert_eq!(reply.epoch, epoch);
        assert_eq!(reply.sent, chunk.len() as u64);
        bytes.extend(chunk);
        flags.push(reply.complete);
        if reply.complete && bytes.len() as u64 == reply.size {
            return (bytes, flags);
        }
    }
}

fn write(server: &TestServer, name: &str, bytes: &[u8]) -> PathBuf {
    let path = server.dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

#[tokio::test]
async fn the_output_is_read_while_it_is_encoded_and_complete_comes_last() {
    let (server, client, session) = ready("progressive-read", ServerOptions::default(), 20).await;
    let handle = started(&client, &session, None).await;
    let (bytes, flags) = read_all(&client, &session, &handle, 0).await;
    assert_eq!(flags.iter().filter(|c| **c).count(), 1, "{flags:?}");
    assert!(flags.last().unwrap());
    let (first, last) = probe(&write(&server, "read.mkv", &bytes));
    assert!(first < 0.1, "first packet at {first}");
    assert!((last - 20.0).abs() < 0.5, "last packet at {last}");
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_read_past_what_is_written_waits_for_it() {
    // Kept far behind: the job pauses at 0.5 s ahead of its reader.
    let options = ServerOptions {
        read_ahead: Duration::from_millis(500),
        ..ServerOptions::default()
    };
    let (server, client, session) = ready("progressive-wait", options, 60).await;
    let handle = started(&client, &session, None).await;
    // Well past what a paused job has written: the read itself lets the
    // encode go on, and waits for the bytes.
    let (response, bytes) = client.read_at(&session, &handle, 0, 2 << 20, 4096).await;
    let MediaReadResponse::Ok(reply) = response else {
        panic!("read failed: {response:?}");
    };
    assert!(reply.size > 2 << 20, "size {}", reply.size);
    assert_eq!(bytes.len(), 4096);
    client.close().await;
    server.stop().await;
}

/// ffmpeg keeps each 2 s Matroska cluster to itself until it is whole. A
/// read-ahead shorter than that once held the encode back before it had
/// written a byte: the reader had nothing to read, and waited for ever.
#[tokio::test]
async fn a_slow_encode_with_a_read_ahead_shorter_than_a_cluster_is_read_to_its_end() {
    // At twice real time: the job looks at the encode before its first
    // cluster is written.
    let (dir, ffmpeg) = removable_ffmpeg("progressive-short-ahead").await;
    let options = ServerOptions {
        real_media: true,
        ffmpeg,
        read_ahead: Duration::from_millis(500),
        ..ServerOptions::default()
    };
    let server = TestServer::start_with(dir, options).await;
    server.generate_media(8, "libx264", "ac3");
    server.add_user("alice", "correct horse", None).await;
    let client = server.client().await;
    client
        .hello_with(None, Some(progressive_capabilities()))
        .await;
    let session = client.login("alice", "correct horse").await;
    let handle = started(&client, &session, None).await;
    let read = read_all(&client, &session, &handle, 0);
    let (bytes, _) = tokio::time::timeout(Duration::from_secs(30), read)
        .await
        .expect("the output is read to its end");
    let (_, last) = probe(&write(&server, "short-ahead.mkv", &bytes));
    assert!((last - 8.0).abs() < 0.5, "last packet at {last}");
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_seek_restarts_the_encode_in_a_new_epoch() {
    let (server, client, session) = ready("progressive-seek", ServerOptions::default(), 20).await;
    let handle = started(&client, &session, None).await;
    let seeked = client
        .call(
            Some(&session),
            Request::SeekPlayback {
                handle: handle.clone(),
                position_ms: 10_000,
            },
        )
        .await;
    assert_eq!(seeked, Response::Ok(Reply::PlaybackSeeked { epoch: 1 }));
    let (stale, _) = client.read_at(&session, &handle, 0, 0, 100).await;
    assert_eq!(
        stale,
        MediaReadResponse::Err(Error::PlaybackRestarted { epoch: 1 })
    );
    let (bytes, _) = read_all(&client, &session, &handle, 1).await;
    let (first, last) = probe(&write(&server, "seek.mkv", &bytes));
    assert!((8.0..=10.5).contains(&first), "first packet at {first}");
    assert!((last - 20.0).abs() < 0.5, "last packet at {last}");
    // The first epoch's files are gone.
    assert!(!server.job_dir(&handle, 0).exists());
    client.close().await;
    server.stop().await;
}

/// A state directory with a stand-in for ffmpeg that runs the real one,
/// and can be removed so that the next encode cannot start. It reads at
/// twice real time: at full speed it converts the test's film in a fifth
/// of a second, before the job looks at it once, and an encode that has
/// ended is not one a seek can break.
async fn removable_ffmpeg(test: &str) -> (crate::support::StateDir, PathBuf) {
    let dir = fresh_dir(test).await;
    std::fs::create_dir_all(&dir).unwrap();
    let ffmpeg = dir.join("removable-ffmpeg");
    crate::support::write_script(&ffmpeg, "#!/bin/sh\nexec ffmpeg -readrate 2 \"$@\"\n");
    (dir, ffmpeg)
}

/// Whether `handle`'s ffmpeg is held back for its reader (stopped): it
/// runs, and cannot end until something reads.
fn held_back(handle: &PlaybackHandle) -> bool {
    ffmpeg_for(handle).iter().any(|pid| {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| stat.contains(") T "))
    })
}

/// A seek once stopped the encode before it started the next. When the
/// next could not start, the playback was left with a dead encode whose
/// reads returned nothing, at once, for ever.
#[tokio::test]
async fn a_seek_that_cannot_start_leaves_the_playback_playing() {
    let (dir, ffmpeg) = removable_ffmpeg("progressive-seek-failed").await;
    let server = TestServer::start_with(
        dir,
        ServerOptions {
            real_media: true,
            max_jobs: 1,
            ffmpeg: ffmpeg.clone(),
            read_ahead: Duration::from_millis(500),
            ..ServerOptions::default()
        },
    )
    .await;
    server.generate_media(12, "libx264", "ac3");
    server.add_user("alice", "correct horse", None).await;
    let client = server.client().await;
    client
        .hello_with(None, Some(progressive_capabilities()))
        .await;
    let session = client.login("alice", "correct horse").await;
    let handle = started(&client, &session, None).await;
    // Held back, so it is surely running at the seek.
    until("the encode is held back", Duration::from_secs(10), || {
        held_back(&handle)
    })
    .await;
    std::fs::remove_file(&ffmpeg).unwrap();
    let seeked = client
        .call(
            Some(&session),
            Request::SeekPlayback {
                handle: handle.clone(),
                position_ms: 6_000,
            },
        )
        .await;
    assert!(
        matches!(seeked, Response::Err(Error::PlaybackFailed { .. })),
        "{seeked:?}"
    );
    // Epoch 0 still encodes, to its end.
    let read = read_all(&client, &session, &handle, 0);
    let (bytes, _) = tokio::time::timeout(Duration::from_secs(20), read)
        .await
        .expect("the first epoch is read to its end");
    let (_, last) = probe(&write(&server, "kept.mkv", &bytes));
    assert!((last - 12.0).abs() < 0.5, "last packet at {last}");
    client.close().await;
    server.stop().await;
}

/// The same for HLS, where a read of a far segment restarts the encode.
#[tokio::test]
async fn an_hls_restart_that_cannot_start_leaves_the_playback_playing() {
    let (dir, ffmpeg) = removable_ffmpeg("hls-restart-failed").await;
    let options = ServerOptions {
        max_jobs: 1,
        ffmpeg: ffmpeg.clone(),
        // Held back with segment 1 (0 to 2 s) written: segment 4 is then
        // near enough to what is written to be waited for.
        read_ahead: Duration::from_secs(3),
        ..ServerOptions::default()
    };
    let (server, client, session) = hls_ready_with(dir, hls_capabilities(), options).await;
    let handle = started(&client, &session, None).await;
    // Held back, so it is surely running at the restart.
    until("the encode is held back", Duration::from_secs(10), || {
        held_back(&handle)
    })
    .await;
    std::fs::remove_file(&ffmpeg).unwrap();
    let (far, _) = client
        .media(MediaRead {
            session: session.clone(),
            handle: handle.clone(),
            offset: 0,
            length: MAX_READ,
            epoch: 0,
            segment: Some(15),
        })
        .await;
    assert!(
        matches!(far, MediaReadResponse::Err(Error::PlaybackFailed { .. })),
        "{far:?}"
    );
    // The first run still encodes. Segment 4 (6 to 8 s) is one it had not
    // got to when it was held back, a little over 3 s in, and near enough
    // to what it has written to be waited for: it arrives.
    let fourth = tokio::time::timeout(
        Duration::from_secs(20),
        segment(&client, &session, &handle, 4),
    )
    .await
    .expect("segment 4 arrives");
    assert!(!fourth.is_empty());
    client.close().await;
    server.stop().await;
}

/// PX-09 finding 23: ffmpeg's last words name the server's paths (its
/// scratch directory, the playback's handle, the nix store). They go to
/// the log and the admin's playback page (`playback_debug`), not to the
/// client, which is told the conversion failed.
#[tokio::test]
async fn a_failed_encode_fails_its_reads_without_ffmpeg_s_words() {
    let dir = fresh_dir("encode-failed").await;
    std::fs::create_dir_all(&dir).unwrap();
    let stub = dir.join("failing-ffmpeg");
    crate::support::write_script(
        &stub,
        "#!/bin/sh\necho '/var/cache/spoolframe/transcode/spoolframe-jobs/3f2a/input: Invalid data found when processing input' >&2\nexit 1\n",
    );
    let server = TestServer::start_with(
        dir,
        ServerOptions {
            real_media: true,
            ffmpeg: stub,
            ..ServerOptions::default()
        },
    )
    .await;
    server.generate_media(4, "libx264", "ac3");
    server.add_user("alice", "correct horse", None).await;
    let client = server.client().await;
    client
        .hello_with(None, Some(progressive_capabilities()))
        .await;
    let session = client.login("alice", "correct horse").await;
    let handle = started(&client, &session, None).await;
    let (response, _) = client.read_at(&session, &handle, 0, 0, 100).await;
    assert_eq!(
        response,
        MediaReadResponse::Err(Error::PlaybackFailed {
            reason: spoolframe_server::encode::job::FFMPEG_FAILED.into()
        })
    );
    client.close().await;
    server.stop().await;
}

/// Nor does a conversion that cannot start say where the server keeps
/// ffmpeg.
#[tokio::test]
async fn a_conversion_that_cannot_start_does_not_name_the_server_s_paths() {
    let dir = fresh_dir("encode-no-ffmpeg").await;
    std::fs::create_dir_all(&dir).unwrap();
    let missing = dir.join("no-such-ffmpeg");
    let server = TestServer::start_with(
        dir,
        ServerOptions {
            real_media: true,
            ffmpeg: missing,
            ..ServerOptions::default()
        },
    )
    .await;
    server.generate_media(4, "libx264", "ac3");
    server.add_user("alice", "correct horse", None).await;
    let client = server.client().await;
    client
        .hello_with(None, Some(progressive_capabilities()))
        .await;
    let session = client.login("alice", "correct horse").await;
    match start(&client, &session, None).await {
        Response::Err(Error::PlaybackFailed { reason }) => {
            assert!(!reason.contains('/'), "{reason}");
            assert!(!reason.contains("no-such-ffmpeg"), "{reason}");
        }
        other => panic!("started without ffmpeg: {other:?}"),
    }
    client.close().await;
    server.stop().await;
}

/// ffmpeg can wait for ever without failing. Such an encode once kept its
/// slot and its reads waiting for as long as the client asked; now it is
/// ended, and its reads say why.
#[tokio::test]
async fn a_stalled_encode_fails_its_reads() {
    let dir = fresh_dir("encode-stalled").await;
    std::fs::create_dir_all(&dir).unwrap();
    let stub = dir.join("stalled-ffmpeg");
    crate::support::write_script(&stub, "#!/bin/sh\nexec sleep 30\n");
    let server = TestServer::start_with(
        dir,
        ServerOptions {
            real_media: true,
            ffmpeg: stub,
            stalled_after: Duration::from_millis(500),
            ..ServerOptions::default()
        },
    )
    .await;
    server.generate_media(4, "libx264", "ac3");
    server.add_user("alice", "correct horse", None).await;
    let client = server.client().await;
    client
        .hello_with(None, Some(progressive_capabilities()))
        .await;
    let session = client.login("alice", "correct horse").await;
    let handle = started(&client, &session, None).await;
    let (response, _) = client.read_at(&session, &handle, 0, 0, 100).await;
    assert_eq!(
        response,
        MediaReadResponse::Err(Error::PlaybackFailed {
            reason: "the conversion stalled: ffmpeg made no progress for 500ms".into()
        })
    );
    client.close().await;
    server.stop().await;
}

/// A server with a 30 s H.264 + AC-3 file (keyframes every 2 s), and a
/// client signed in with `caps`.
async fn hls_ready(test: &str, caps: ClientCapabilities) -> (TestServer, TestClient, SessionId) {
    hls_ready_with(fresh_dir(test).await, caps, ServerOptions::default()).await
}

async fn hls_ready_with(
    dir: crate::support::StateDir,
    caps: ClientCapabilities,
    options: ServerOptions,
) -> (TestServer, TestClient, SessionId) {
    let server = TestServer::start_with(
        dir,
        ServerOptions {
            real_media: true,
            ..options
        },
    )
    .await;
    server.generate_media(30, "libx264", "ac3");
    server.add_user("alice", "correct horse", None).await;
    let client = server.client().await;
    client.hello_with(None, Some(caps)).await;
    let session = client.login("alice", "correct horse").await;
    (server, client, session)
}

async fn manifest(
    client: &TestClient,
    session: &SessionId,
    handle: &PlaybackHandle,
    kind: ManifestKind,
) -> String {
    match client
        .call(
            Some(session),
            Request::GetManifest {
                handle: handle.clone(),
                kind,
            },
        )
        .await
    {
        Response::Ok(Reply::Manifest { text }) => text,
        other => panic!("no manifest: {other:?}"),
    }
}

/// HLS segment `n` (0: init) whole, asking again while it is not ready.
async fn segment(
    client: &TestClient,
    session: &SessionId,
    handle: &PlaybackHandle,
    n: u32,
) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let (response, chunk) = client
            .media(MediaRead {
                session: session.clone(),
                handle: handle.clone(),
                offset: bytes.len() as u64,
                length: MAX_READ,
                epoch: 0,
                segment: Some(n),
            })
            .await;
        let MediaReadResponse::Ok(reply) = response else {
            panic!("segment {n}: {response:?}");
        };
        bytes.extend(chunk);
        if reply.complete && bytes.len() as u64 == reply.size {
            return bytes;
        }
    }
}

fn media_segments(text: &str) -> Vec<f32> {
    match m3u8_rs::parse_playlist_res(text.as_bytes()).expect("a playlist") {
        m3u8_rs::Playlist::MediaPlaylist(p) => {
            assert!(p.end_list);
            p.segments.iter().map(|s| s.duration).collect()
        }
        other => panic!("not a media playlist: {other:?}"),
    }
}

/// Play every segment of `handle` through the protocol: the durations from
/// the playlist, and the first and last packet times of init + segments.
async fn play_hls(
    server: &TestServer,
    client: &TestClient,
    session: &SessionId,
    handle: &PlaybackHandle,
    name: &str,
) -> (Vec<f32>, (f64, f64)) {
    let durations = media_segments(&manifest(client, session, handle, ManifestKind::Media).await);
    let mut stream = segment(client, session, handle, 0).await;
    for n in 1..=durations.len() as u32 {
        stream.extend(segment(client, session, handle, n).await);
    }
    (durations, probe(&write(server, name, &stream)))
}

#[tokio::test]
async fn hls_of_a_copied_video_is_cut_at_its_keyframes_and_plays_whole() {
    let (server, client, session) = hls_ready("hls-copy", hls_capabilities()).await;
    let handle = started(&client, &session, None).await;
    let multivariant = manifest(&client, &session, &handle, ManifestKind::Multivariant).await;
    assert!(
        matches!(
            m3u8_rs::parse_playlist_res(multivariant.as_bytes()),
            Ok(m3u8_rs::Playlist::MasterPlaylist(_))
        ),
        "{multivariant}"
    );
    assert!(
        multivariant.contains("avc1.640028,mp4a.40.2"),
        "{multivariant}"
    );
    // The 360p test pattern, 25 frames a second, says no transfer.
    assert!(
        multivariant.contains(",RESOLUTION=640x360,FRAME-RATE=25.000,VIDEO-RANGE=SDR\n"),
        "{multivariant}"
    );

    let (durations, (first, last)) =
        play_hls(&server, &client, &session, &handle, "copy.mp4").await;
    // Keyframes every 2 s: fifteen 2 s segments.
    assert_eq!(durations.len(), 15, "{durations:?}");
    assert!(
        durations.iter().all(|d| (d - 2.0).abs() < 0.05),
        "{durations:?}"
    );
    assert!(first < 0.1, "first packet at {first}");
    assert!((last - 30.0).abs() < 0.5, "last packet at {last}");
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn hls_of_an_encoded_video_is_a_six_second_grid_and_plays_whole() {
    let caps = ClientCapabilities {
        // 360p source, 180p client: the video is encoded.
        max_video: Some(VideoLimits {
            height: Some(180),
            ..VideoLimits::default()
        }),
        ..hls_capabilities()
    };
    let (server, client, session) = hls_ready("hls-encode", caps).await;
    let handle = started(&client, &session, None).await;
    let (durations, (first, last)) =
        play_hls(&server, &client, &session, &handle, "encode.mp4").await;
    assert_eq!(durations.len(), 5, "{durations:?}");
    assert!(
        durations.iter().all(|d| (d - 6.0).abs() < 0.05),
        "{durations:?}"
    );
    assert!(first < 0.1, "first packet at {first}");
    assert!((last - 30.0).abs() < 0.5, "last packet at {last}");
    client.close().await;
    server.stop().await;
}

/// ffmpeg writes an fMP4 segment's file only as it closes it. A read-ahead
/// shorter than a segment once paused the encode before the segment after
/// the one being read could appear, and the read waited for ever.
#[tokio::test]
async fn hls_plays_whole_with_a_read_ahead_shorter_than_a_segment() {
    let caps = ClientCapabilities {
        max_video: Some(VideoLimits {
            height: Some(180),
            ..VideoLimits::default()
        }),
        ..hls_capabilities()
    };
    // At full speed the encode can finish between two throttle checks and
    // never be paused; at 4× real time the throttle always catches it.
    let dir = fresh_dir("hls-short-ahead").await;
    std::fs::create_dir_all(&dir).unwrap();
    let slow = dir.join("slow-ffmpeg");
    crate::support::write_script(&slow, "#!/bin/sh\nexec ffmpeg -readrate 4 \"$@\"\n");
    let options = ServerOptions {
        ffmpeg: slow,
        read_ahead: Duration::from_millis(500),
        ..ServerOptions::default()
    };
    let (server, client, session) = hls_ready_with(dir, caps, options).await;
    let handle = started(&client, &session, None).await;
    let play = play_hls(&server, &client, &session, &handle, "short-ahead.mp4");
    let (durations, _) = tokio::time::timeout(Duration::from_secs(60), play)
        .await
        .expect("every segment arrives");
    assert_eq!(durations.len(), 5, "{durations:?}");
    client.close().await;
    server.stop().await;
}

/// A film longer than the scratch space holds: the encode removes the
/// segments behind its reader to make room, and a segment asked for again
/// after that is encoded again (P2-03).
#[tokio::test]
async fn hls_plays_whole_in_a_scratch_space_smaller_than_the_film() {
    // At 8× real time the encode is still writing when the space fills;
    // at full speed it would write the whole film before the first read.
    let dir = fresh_dir("hls-small-scratch").await;
    std::fs::create_dir_all(&dir).unwrap();
    let slow = dir.join("slow-ffmpeg");
    crate::support::write_script(&slow, "#!/bin/sh\nexec ffmpeg -readrate 8 \"$@\"\n");
    let options = ServerOptions {
        ffmpeg: slow,
        read_ahead: Duration::from_secs(2),
        // The 40 s film's segments are about 4.5 MiB together.
        scratch_limit: 2 << 20,
        ..ServerOptions::default()
    };
    let (server, client, session) = hls_ready_with(dir, hls_capabilities(), options).await;
    server.generate_media(40, "libx264", "ac3");
    let handle = started(&client, &session, None).await;
    let play = play_hls(&server, &client, &session, &handle, "small-scratch.mp4");
    let (durations, (first, last)) = tokio::time::timeout(Duration::from_secs(60), play)
        .await
        .expect("every segment arrives");
    assert_eq!(durations.len(), 20, "{durations:?}");
    assert!(first < 0.1, "first packet at {first}");
    assert!((last - 40.0).abs() < 0.5, "last packet at {last}");
    // The first segment went to make room: it is encoded again.
    assert!(!server.job_dir(&handle, 0).join("seg1.m4s").exists());
    let again = tokio::time::timeout(
        Duration::from_secs(20),
        segment(&client, &session, &handle, 1),
    )
    .await
    .expect("segment 1 arrives again");
    assert!(!again.is_empty());
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn asking_for_a_late_segment_first_restarts_the_encode_there() {
    let (server, client, session) = hls_ready("hls-jump", hls_capabilities()).await;
    let handle = started(&client, &session, None).await;
    // The last segment, before anything else: far past what is written.
    let last = segment(&client, &session, &handle, 15).await;
    let init = segment(&client, &session, &handle, 0).await;
    let (first, end) = probe(&write(&server, "jump.mp4", &[init, last].concat()));
    assert!((first - 28.0).abs() < 0.1, "first packet at {first}");
    assert!((end - 30.0).abs() < 0.5, "last packet at {end}");
    // The first run's files are gone; the new one is epoch 1.
    assert!(!server.job_dir(&handle, 0).exists());
    assert!(server.job_dir(&handle, 1).exists());
    client.close().await;
    server.stop().await;
}

/// Where each segment of `durations` starts, in seconds.
fn segment_starts(durations: &[f32]) -> Vec<f64> {
    durations
        .iter()
        .scan(0.0, |at, d| {
            let start = *at;
            *at += f64::from(*d);
            Some(start)
        })
        .collect()
}

/// The playlist's segment durations, and the first packet time of each
/// segment as ffmpeg wrote it. No segment is empty.
async fn listed_and_written(
    server: &TestServer,
    client: &TestClient,
    session: &SessionId,
    handle: &PlaybackHandle,
) -> (Vec<f32>, Vec<f64>) {
    let durations = media_segments(&manifest(client, session, handle, ManifestKind::Media).await);
    let init = segment(client, session, handle, 0).await;
    let mut firsts = Vec::new();
    for n in 1..=durations.len() as u32 {
        let bytes = segment(client, session, handle, n).await;
        assert!(!bytes.is_empty(), "segment {n} of {durations:?} is empty");
        let file = write(server, "one.mp4", &[init.clone(), bytes].concat());
        firsts.push(probe(&file).0);
    }
    (durations, firsts)
}

/// ffmpeg does not cut at the first keyframe it sees. A playlist with a
/// cut there listed one segment more than ffmpeg wrote, and every segment
/// held the time the playlist gave to the one after it.
#[tokio::test]
async fn hls_of_a_copied_video_with_a_late_first_keyframe_lists_what_ffmpeg_writes() {
    let (server, client, session) = hls_ready("hls-late-keyframe", hls_capabilities()).await;
    // 10 s, the first keyframe at about 0.5 s.
    server.generate_media_with_late_video(10, "libx264", "ac3");
    let handle = started(&client, &session, None).await;
    let (durations, firsts) = listed_and_written(&server, &client, &session, &handle).await;
    // The first holds 0 to the second keyframe (2.5 s); then one per GOP.
    assert_eq!(durations.len(), 5, "{durations:?}");
    for (first, start) in firsts.iter().zip(segment_starts(&durations)) {
        assert!(
            (first - start).abs() < 0.1,
            "segments start at {firsts:?}, the playlist says {durations:?}"
        );
    }
    client.close().await;
    server.stop().await;
}

/// A first keyframe before time 0 is the one ffmpeg does not cut at. It
/// was left out of the keyframes, so the second was taken for the first:
/// the playlist had no cut at 1.96 s, where ffmpeg cuts, and listed one
/// segment fewer than ffmpeg wrote.
#[tokio::test]
async fn hls_of_a_copied_video_with_a_first_keyframe_before_zero_lists_what_ffmpeg_writes() {
    let (server, client, session) = hls_ready("hls-early-keyframe", hls_capabilities()).await;
    // 10 s, keyframes at -0.04 s, 1.96 s, 3.96 s and so on.
    server.generate_media_cut_after_a_keyframe(10, "libx264", "ac3");
    let handle = started(&client, &session, None).await;
    let (durations, firsts) = listed_and_written(&server, &client, &session, &handle).await;
    // One per GOP: the first to 1.96 s.
    assert_eq!(durations.len(), 5, "{durations:?}");
    for (first, start) in firsts.iter().zip(segment_starts(&durations)) {
        assert!(
            (first - start).abs() < 0.1,
            "segments start at {firsts:?}, the playlist says {durations:?}"
        );
    }
    client.close().await;
    server.stop().await;
}

/// The times stored in fMP4, before any edit list is applied: what
/// AVPlayer places WebVTT cues on. And what each track says it holds.
#[derive(Debug, Default)]
struct Mp4Times {
    /// Each track's first `stsd` sample entry (`hvc1`, `mp4a`), in track
    /// order.
    sample_entries: Vec<String>,
    /// Each track's `mdhd` timescale, in track order.
    timescales: Vec<u32>,
    /// The media time of every `elst` entry; -1 is an empty edit.
    edits: Vec<i64>,
    /// Each track fragment's `tfdt`, in track order.
    decode_times: Vec<u64>,
}

fn big_endian(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0, |n, b| n << 8 | u64::from(*b))
}

/// Read the boxes of `bytes` (an `init.mp4` or a segment) into `times`.
fn mp4_times(bytes: &[u8], times: &mut Mp4Times) {
    let mut rest = bytes;
    while rest.len() >= 8 {
        let size = big_endian(&rest[..4]) as usize;
        if size < 8 || size > rest.len() {
            break;
        }
        let body = &rest[8..size];
        match &rest[4..8] {
            b"moov" | b"trak" | b"edts" | b"mdia" | b"minf" | b"stbl" | b"moof" | b"traf" => {
                mp4_times(body, times);
            }
            // After the version and the entry count: the first entry's
            // size, then its type.
            b"stsd" => times
                .sample_entries
                .push(String::from_utf8_lossy(&body[12..16]).into_owned()),
            // Version 1 has 64-bit dates before the timescale.
            b"mdhd" => {
                let at = if body[0] == 1 { 20 } else { 12 };
                times.timescales.push(big_endian(&body[at..at + 4]) as u32);
            }
            // Entries of (duration, media time, rate), 64-bit in version 1.
            b"elst" => {
                let wide = body[0] == 1;
                for entry in body[8..].chunks_exact(if wide { 20 } else { 12 }) {
                    times.edits.push(if wide {
                        big_endian(&entry[8..16]) as i64
                    } else {
                        i64::from(big_endian(&entry[4..8]) as u32 as i32)
                    });
                }
            }
            b"tfdt" => {
                let width = if body[0] == 1 { 8 } else { 4 };
                times.decode_times.push(big_endian(&body[4..4 + width]));
            }
            _ => {}
        }
        rest = &rest[size..];
    }
}

/// Segment `n` holds the film's own time, `start` to `end` in seconds, in
/// its stored timestamps: `init.mp4` has no empty edit to move it there,
/// the video's `tfdt` is the segment's start, and the audio's is in the
/// segment (a run's first segment has its sound from where the seek was
/// aimed, after the keyframe).
async fn assert_stores_film_time(
    client: &TestClient,
    session: &SessionId,
    handle: &PlaybackHandle,
    n: u32,
    (start, end): (f64, f64),
) {
    let mut times = Mp4Times::default();
    mp4_times(&segment(client, session, handle, n).await, &mut times);
    // After the segment: the init of the run that wrote it.
    mp4_times(&segment(client, session, handle, 0).await, &mut times);
    assert!(
        !times.edits.contains(&-1),
        "segment {n}: an empty edit in init.mp4: {times:?}"
    );
    let seconds: Vec<f64> = times
        .decode_times
        .iter()
        .zip(&times.timescales)
        .map(|(t, scale)| *t as f64 / f64::from(*scale))
        .collect();
    let [video, audio] = seconds[..] else {
        panic!("segment {n}: not a video and an audio track: {times:?}");
    };
    assert!(
        (video - start).abs() < 0.1,
        "segment {n} is {start} to {end} s, its video is stored at {video} s: {times:?}"
    );
    assert!(
        (start - 0.1..end).contains(&audio),
        "segment {n} is {start} to {end} s, its audio is stored at {audio} s: {times:?}"
    );
}

/// Start a playback at `start_ms` and check segment `n`, where it starts,
/// and the one after against the playlist's clock.
async fn starts_with_film_time(
    test: &str,
    caps: ClientCapabilities,
    start_ms: Option<u64>,
    n: u32,
) {
    let (server, client, session) = hls_ready(test, caps).await;
    let handle = started(&client, &session, start_ms).await;
    let durations =
        media_segments(&manifest(&client, &session, &handle, ManifestKind::Media).await);
    let starts = segment_starts(&durations);
    for n in [n, n + 1] {
        let span = (starts[n as usize - 1], starts[n as usize]);
        assert_stores_film_time(&client, &session, &handle, n, span).await;
    }
    // One run wrote both: it started at `n`.
    assert!(!server.job_dir(&handle, 1).exists());
    client.close().await;
    server.stop().await;
}

/// AVPlayer places WebVTT cues on the timestamps stored in the segments,
/// and the cues are in the film's time. A run started at 20 s stored its
/// segments from 0, with the 20 s as an empty edit in `init.mp4`: no cue
/// was shown.
#[tokio::test]
async fn hls_started_far_in_stores_the_film_s_own_time() {
    // 21 s is in segment 11, 20 to 22 s.
    starts_with_film_time("hls-times-far", hls_capabilities(), Some(21_000), 11).await;
}

#[tokio::test]
async fn hls_started_at_two_seconds_stores_the_film_s_own_time() {
    starts_with_film_time("hls-times-two", hls_capabilities(), Some(2_000), 2).await;
}

#[tokio::test]
async fn hls_started_at_zero_stores_the_film_s_own_time() {
    starts_with_film_time("hls-times-zero", hls_capabilities(), None, 1).await;
}

#[tokio::test]
async fn hls_of_an_encoded_video_started_far_in_stores_the_film_s_own_time() {
    let caps = ClientCapabilities {
        max_video: Some(VideoLimits {
            height: Some(180),
            ..VideoLimits::default()
        }),
        ..hls_capabilities()
    };
    // 13 s is in segment 3 of the 6 s grid, 12 to 18 s.
    starts_with_film_time("hls-times-encoded", caps, Some(13_000), 3).await;
}

/// A far seek starts a new run in a new epoch, with an `init.mp4` of its
/// own: that one stores the film's time too.
///
/// A segment restarts the encode only if it is far past what the running
/// one has written, and at full speed the first run can write the whole
/// film before segment 14 is asked for. So the first run (epoch 0) reads
/// only the film's first 8 s: it ends at segment 4, and segment 14 is
/// always far past it. The run that restarts there reads the whole film.
#[tokio::test]
async fn hls_restarted_far_ahead_stores_the_film_s_own_time() {
    let dir = fresh_dir("hls-times-restart").await;
    std::fs::create_dir_all(&dir).unwrap();
    let ffmpeg = dir.join("short-first-ffmpeg");
    crate::support::write_script(
        &ffmpeg,
        "#!/bin/sh\nfor out; do :; done\n\
         case \"$out\" in */0/*) exec ffmpeg -t 8 \"$@\" ;; esac\n\
         exec ffmpeg \"$@\"\n",
    );
    let options = ServerOptions {
        ffmpeg,
        ..ServerOptions::default()
    };
    let (server, client, session) = hls_ready_with(dir, hls_capabilities(), options).await;
    let handle = started(&client, &session, None).await;
    assert_stores_film_time(&client, &session, &handle, 1, (0.0, 2.0)).await;
    assert_stores_film_time(&client, &session, &handle, 14, (26.0, 28.0)).await;
    assert_stores_film_time(&client, &session, &handle, 15, (28.0, 30.0)).await;
    assert!(server.job_dir(&handle, 1).exists());
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn an_explicit_hls_seek_keeps_segment_numbers_and_film_time() {
    for encode in [false, true] {
        let mut caps = hls_capabilities();
        if encode {
            caps.max_video = Some(VideoLimits {
                height: Some(180),
                ..VideoLimits::default()
            });
        }
        let (server, client, session) = hls_ready("hls-explicit-seek", caps).await;
        let handle = started(&client, &session, None).await;
        let playlist = manifest(&client, &session, &handle, ManifestKind::Media).await;
        let starts = segment_starts(&media_segments(&playlist));
        // Inside segment 2, not at its boundary. Starting numbering at 1
        // would serve the following segment's content under this number.
        let position_ms = if encode { 9_000 } else { 3_000 };
        assert_eq!(
            client
                .call(
                    Some(&session),
                    Request::SeekPlayback {
                        handle: handle.clone(),
                        position_ms,
                    }
                )
                .await,
            Response::Ok(Reply::PlaybackSeeked { epoch: 1 }),
        );
        for n in [2, 3] {
            assert_stores_film_time(
                &client,
                &session,
                &handle,
                n,
                (starts[n as usize - 1], starts[n as usize]),
            )
            .await;
        }
        assert_eq!(
            manifest(&client, &session, &handle, ManifestKind::Media).await,
            playlist
        );
        // Reading these segments must not repair a bad seek by restarting.
        let (read, _) = client
            .media(MediaRead {
                session: session.clone(),
                handle: handle.clone(),
                offset: 0,
                length: 1,
                epoch: 0,
                segment: Some(3),
            })
            .await;
        assert!(matches!(read, MediaReadResponse::Ok(reply) if reply.epoch == 1));
        client.close().await;
        server.stop().await;
    }
}

/// AVPlayer plays HEVC in fMP4 only under the sample entry `hvc1`, and
/// the playlist says `hvc1`. ffmpeg writes `hev1` unless told: an iPhone
/// was given a 4K film as `hev1`, and showed nothing.
#[tokio::test]
async fn hls_of_a_copied_hevc_video_is_hvc1_as_the_playlist_says() {
    use spoolframe_protocol::ConvertedVideo;
    use spoolframe_types::VideoCodec;
    let caps = ClientCapabilities {
        video: [VideoCodec::H264, VideoCodec::Hevc].into(),
        ..hls_capabilities()
    };
    let (server, client, session) = hls_ready("hls-hvc1", caps).await;
    server.generate_media(6, "libx265", "ac3");
    let handle = match start(&client, &session, None).await {
        Response::Ok(Reply::PlaybackStarted {
            handle,
            plan:
                PlaybackPlan::Converted {
                    video:
                        Some(ConvertedVideo::Copy {
                            codec: VideoCodec::Hevc,
                        }),
                    ..
                },
            ..
        }) => handle,
        other => panic!("no playback with its HEVC copied: {other:?}"),
    };
    let init = segment(&client, &session, &handle, 0).await;
    let mut boxes = Mp4Times::default();
    mp4_times(&init, &mut boxes);
    assert_eq!(boxes.sample_entries, ["hvc1", "mp4a"]);

    let multivariant = manifest(&client, &session, &handle, ManifestKind::Multivariant).await;
    let declared = multivariant
        .split_once("CODECS=\"")
        .and_then(|(_, rest)| rest.split_once('.'))
        .map(|(entry, _)| entry);
    assert_eq!(
        declared,
        Some(boxes.sample_entries[0].as_str()),
        "{multivariant}"
    );

    // And it plays: the first segment's packets are read with that init.
    let first = segment(&client, &session, &handle, 1).await;
    let (start, _) = probe(&write(&server, "hvc1.mp4", &[init, first].concat()));
    assert!(start < 0.1, "first packet at {start}");
    client.close().await;
    server.stop().await;
}

/// An Apple TV's playback failed with nothing in the server's log: its
/// app serves the playlists to AVPlayer and fetches each segment as it is
/// asked for, and a failed fetch was said only to the app. A segment or a
/// WebVTT segment read answered with an error is now a warning, naming the
/// playback and the segment.
#[tokio::test]
async fn a_failed_hls_segment_read_is_logged() {
    if !crate::support::alone("encode::a_failed_hls_segment_read_is_logged") {
        return;
    }
    let (server, client, session) = hls_ready("hls-read-warn", hls_capabilities()).await;
    let handle = started(&client, &session, None).await;
    let (warnings, _guard) = crate::support::Warnings::capture();
    let (response, _) = client
        .media(MediaRead {
            session: session.clone(),
            handle: handle.clone(),
            offset: 0,
            length: MAX_READ,
            epoch: 0,
            segment: Some(999),
        })
        .await;
    assert!(
        matches!(response, MediaReadResponse::Err(Error::NotFound { .. })),
        "{response:?}"
    );
    // No subtitle in this playback: no WebVTT segment either.
    let response = client
        .call(
            Some(&session),
            Request::ReadSubtitleSegment {
                handle: handle.clone(),
                n: 1,
            },
        )
        .await;
    assert!(
        matches!(response, Response::Err(Error::NotFound { .. })),
        "{response:?}"
    );
    let text = warnings.text();
    let lines: Vec<&str> = text.lines().filter(|l| l.contains(" WARN ")).collect();
    // Named by what it plays: the handle is never logged.
    let id = movie().id().to_string();
    for wanted in ["segment=999", "subtitle_segment=1"] {
        assert!(
            lines
                .iter()
                .any(|l| l.contains(wanted) && l.contains(id.as_str())),
            "no warning with {wanted} for {id}: {text}"
        );
    }
    client.close().await;
    server.stop().await;
}

/// An iOS app starts the next track 140 ms after this one, for gapless
/// audio: this one's HLS must play on. The device keeps "now + next", so
/// only a third start ends the first.
#[tokio::test]
async fn a_device_s_next_hls_playback_leaves_the_one_playing() {
    let (server, client, session) = hls_ready("hls-next", hls_capabilities()).await;
    let now = started(&client, &session, None).await;
    segment(&client, &session, &now, 0).await;
    let next = started(&client, &session, None).await;
    assert!(!segment(&client, &session, &now, 1).await.is_empty());
    assert!(!segment(&client, &session, &next, 1).await.is_empty());
    // The third: the first is abandoned, and its slot is the third's.
    let third = started(&client, &session, None).await;
    let (response, _) = client
        .media(MediaRead {
            session: session.clone(),
            handle: now.clone(),
            offset: 0,
            length: MAX_READ,
            epoch: 0,
            segment: Some(2),
        })
        .await;
    assert!(
        matches!(response, MediaReadResponse::Err(Error::PlaybackExpired)),
        "{response:?}"
    );
    assert!(!segment(&client, &session, &next, 2).await.is_empty());
    assert!(!segment(&client, &session, &third, 1).await.is_empty());
    client.close().await;
    server.stop().await;
}

/// AVPlayer refuses a segment over the variant's BANDWIDTH ("Segment
/// exceeds specified bandwidth for variant", CoreMediaErrorDomain
/// -12318). FLAC copied into HLS was declared at 640 kbit/s, under a
/// third of what CD-quality FLAC is.
#[tokio::test]
async fn hls_of_copied_flac_declares_at_least_its_real_rate() {
    let server = TestServer::start_with(
        fresh_dir("hls-flac-bandwidth").await,
        ServerOptions {
            real_media: true,
            ..ServerOptions::default()
        },
    )
    .await;
    server.generate_flac_noise(20);
    server.add_user("alice", "correct horse", None).await;
    let client = server.client().await;
    let caps = ClientCapabilities {
        audio: [spoolframe_types::AudioCodec::Flac].into(),
        ..hls_capabilities()
    };
    client.hello_with(None, Some(caps)).await;
    let session = client.login("alice", "correct horse").await;
    let handle = started(&client, &session, None).await;
    let declared = match m3u8_rs::parse_playlist_res(
        manifest(&client, &session, &handle, ManifestKind::Multivariant)
            .await
            .as_bytes(),
    ) {
        Ok(m3u8_rs::Playlist::MasterPlaylist(p)) => p.variants[0].bandwidth,
        other => panic!("not a multivariant playlist: {other:?}"),
    };
    let durations =
        media_segments(&manifest(&client, &session, &handle, ManifestKind::Media).await);
    let mut peak = 0f64;
    for (n, secs) in (1..).zip(&durations) {
        let bits = segment(&client, &session, &handle, n).await.len() as f64 * 8.0;
        peak = peak.max(bits / f64::from(*secs));
    }
    assert!(
        declared as f64 >= peak,
        "BANDWIDTH={declared}, and a segment is {peak:.0} bit/s"
    );
    client.close().await;
    server.stop().await;
}

/// The NAL unit types of the arrays in the first `hvcC` record of `init`.
fn hvcc_arrays(init: &[u8]) -> Vec<u8> {
    let at = init
        .windows(4)
        .position(|w| w == b"hvcC")
        .expect("no hvcC box");
    let size = big_endian(&init[at - 4..at]) as usize;
    let record = &init[at + 4..at - 4 + size];
    let Some((&count, mut rest)) = record.get(22..).and_then(<[u8]>::split_first) else {
        return Vec::new();
    };
    let mut types = Vec::new();
    for _ in 0..count {
        types.push(rest[0] & 0x3F);
        let units = big_endian(&rest[1..3]);
        rest = &rest[3..];
        for _ in 0..units {
            let length = big_endian(&rest[..2]) as usize;
            rest = &rest[2 + length..];
        }
    }
    types
}

/// `hvc1` says the parameter sets are in the sample entry's `hvcC`, and
/// AVPlayer decodes with those alone. A 4K WEB-DL whose Matroska track
/// keeps them only in the stream was copied into an `init.mp4` whose `hvcC`
/// was 8 bytes, a box with no record: an Apple TV refused it as an
/// unsupported format (CoreMediaErrorDomain -19601).
#[tokio::test]
async fn hls_of_a_copied_hevc_video_with_its_parameter_sets_in_the_stream_names_them_in_hvcc() {
    use spoolframe_protocol::ConvertedVideo;
    use spoolframe_types::VideoCodec;
    let caps = ClientCapabilities {
        video: [VideoCodec::H264, VideoCodec::Hevc].into(),
        ..hls_capabilities()
    };
    let (server, client, session) = hls_ready("hls-hvcc", caps).await;
    server.generate_hevc_with_parameter_sets_in_the_stream_only(6);
    let handle = match start(&client, &session, None).await {
        Response::Ok(Reply::PlaybackStarted {
            handle,
            plan:
                PlaybackPlan::Converted {
                    video:
                        Some(ConvertedVideo::Copy {
                            codec: VideoCodec::Hevc,
                        }),
                    ..
                },
            ..
        }) => handle,
        other => panic!("no playback with its HEVC copied: {other:?}"),
    };
    let init = segment(&client, &session, &handle, 0).await;
    // VPS, SPS and PPS.
    let types = hvcc_arrays(&init);
    for nal in [32, 33, 34] {
        assert!(types.contains(&nal), "hvcC arrays {types:?}, no {nal}");
    }
    client.close().await;
    server.stop().await;
}

/// WebVTT segment `n` of `handle`'s subtitle rendition, asking again while
/// its text is being made.
async fn subtitle_segment(
    client: &TestClient,
    session: &SessionId,
    handle: &PlaybackHandle,
    n: u32,
) -> String {
    use spoolframe_types::SubtitleFormat;
    loop {
        let response = client
            .call(
                Some(session),
                Request::ReadSubtitleSegment {
                    handle: handle.clone(),
                    n,
                },
            )
            .await;
        match response {
            Response::Ok(Reply::Subtitle { format, bytes }) => {
                assert_eq!(format, SubtitleFormat::WebVtt);
                return String::from_utf8(bytes).unwrap();
            }
            Response::Err(spoolframe_protocol::Error::SubtitleNotReady { .. }) => {}
            other => panic!("segment {n}: {other:?}"),
        }
    }
}

/// The segment (from 1) of `durations` that holds `secs`.
fn holding(durations: &[f32], secs: f32) -> u32 {
    let mut start = 0.0;
    durations
        .iter()
        .position(|d| {
            let holds = start <= secs && secs < start + d;
            start += d;
            holds
        })
        .unwrap() as u32
        + 1
}

/// A text subtitle goes to an HLS client as a WebVTT rendition on the
/// video's cuts, each cue in the segment that holds its time (P2-07).
#[tokio::test]
async fn an_hls_playback_carries_its_subtitle_as_a_webvtt_rendition() {
    use spoolframe_protocol::Selection;
    use spoolframe_types::SubtitleFormat;
    let caps = ClientCapabilities {
        subtitles: [SubtitleFormat::WebVtt].into(),
        ..hls_capabilities()
    };
    let (server, client, session) = hls_ready("hls-subs", caps).await;
    server.add_text_track(
        "1\n00:00:01,000 --> 00:00:02,000\nOne\n\n\
         2\n00:00:13,000 --> 00:00:14,500\nThirteen\n\n",
        "subrip",
    );
    let response = client
        .call(
            Some(&session),
            Request::StartPlayback {
                id: movie().id().clone(),
                selection: Some(Selection {
                    audio: Some(0),
                    subtitle: Some(0),
                }),
                start_ms: None,
                bitmap: Default::default(),
            },
        )
        .await;
    let Response::Ok(Reply::PlaybackStarted { handle, .. }) = response else {
        panic!("no playback: {response:?}");
    };

    let multivariant = manifest(&client, &session, &handle, ManifestKind::Multivariant).await;
    assert!(
        multivariant.contains("TYPE=SUBTITLES") && multivariant.contains("SUBTITLES=\"subs\""),
        "{multivariant}"
    );
    let subtitles = manifest(&client, &session, &handle, ManifestKind::Subtitles).await;
    let video = manifest(&client, &session, &handle, ManifestKind::Media).await;
    // The same cuts as the video.
    let durations = media_segments(&subtitles);
    assert_eq!(durations, media_segments(&video));

    // The segment that holds 13 s, and the first.
    let holding_13 = holding(&durations, 13.0);
    let read = |n: u32| subtitle_segment(&client, &session, &handle, n);
    let at_13 = read(holding_13).await;
    assert!(at_13.contains("X-TIMESTAMP-MAP="), "{at_13}");
    assert!(
        at_13.contains("00:00:13.000 --> 00:00:14.500\nThirteen"),
        "{at_13}"
    );
    assert!(!at_13.contains("One"), "{at_13}");
    let first = read(1).await;
    assert!(
        first.contains("One") && !first.contains("Thirteen"),
        "{first}"
    );
    client.close().await;
    server.stop().await;
}

/// A VobSub or DVB track goes to an HLS client that draws neither as its
/// text, read by OCR, in the same WebVTT rendition (P3-08).
#[tokio::test]
async fn an_hls_playback_reads_a_vobsub_or_dvb_track_into_its_webvtt_rendition() {
    use spoolframe_protocol::{ConvertedSubtitle, PlaybackPlan, Selection, SubtitleAction};
    use spoolframe_types::SubtitleFormat;
    let caps = ClientCapabilities {
        subtitles: [SubtitleFormat::WebVtt].into(),
        ..hls_capabilities()
    };
    for (codec, from) in [
        ("dvdsub", SubtitleFormat::VobSub),
        ("dvbsub", SubtitleFormat::DvbSub),
    ] {
        let options = ServerOptions {
            ocr: true,
            ..ServerOptions::default()
        };
        let dir = fresh_dir(&format!("hls-ocr-{codec}")).await;
        let (server, client, session) = hls_ready_with(dir, caps.clone(), options).await;
        // VobSub's stop is in 1024/90000 s: lengths of 512 ms multiples.
        server.add_bitmap_track(
            codec,
            &[
                ("Hello there", 1_000, 3_560),
                ("General Kenobi", 13_000, 15_048),
            ],
        );
        let response = client
            .call(
                Some(&session),
                Request::StartPlayback {
                    id: movie().id().clone(),
                    selection: Some(Selection {
                        audio: Some(0),
                        subtitle: Some(0),
                    }),
                    start_ms: None,
                    bitmap: Default::default(),
                },
            )
            .await;
        let Response::Ok(Reply::PlaybackStarted {
            handle,
            plan: PlaybackPlan::Converted { subtitle, .. },
            ..
        }) = response
        else {
            panic!("{codec}: no converted playback: {response:?}");
        };
        assert_eq!(
            subtitle,
            Some(ConvertedSubtitle {
                index: 0,
                action: SubtitleAction::Ocr {
                    from,
                    to: SubtitleFormat::WebVtt
                },
            }),
            "{codec}"
        );
        let video = manifest(&client, &session, &handle, ManifestKind::Media).await;
        let durations = media_segments(&video);
        let at_13 = subtitle_segment(&client, &session, &handle, holding(&durations, 13.0)).await;
        assert!(
            at_13.contains("00:00:13.000 --> 00:00:15.048\nGeneral Kenobi"),
            "{codec}: {at_13}"
        );
        let first = subtitle_segment(&client, &session, &handle, 1).await;
        assert!(
            first.contains("00:00:01.000 --> 00:00:03.560\nHello there"),
            "{codec}: {first}"
        );
        client.close().await;
        server.stop().await;
    }
}

async fn change(
    client: &TestClient,
    session: &SessionId,
    handle: &PlaybackHandle,
    subtitle: Option<u32>,
    position_ms: u64,
) -> (PlaybackPlan, u32) {
    let response = client
        .call(
            Some(session),
            Request::ChangeSubtitle {
                handle: handle.clone(),
                subtitle,
                bitmap: Default::default(),
                position_ms,
            },
        )
        .await;
    match response {
        Response::Ok(Reply::PlaybackChanged { plan, epoch, .. }) => (plan, epoch),
        other => panic!("not changed: {other:?}"),
    }
}

/// Picking a subtitle the client cannot draw burns it in: the encode
/// restarts where the viewer is, as a new epoch; dropping it again goes
/// back to the copied video (P2-07).
#[tokio::test]
async fn a_subtitle_change_restarts_a_converted_playback_where_it_is() {
    use spoolframe_protocol::{ConvertedSubtitle, ConvertedVideo, SubtitleAction};
    let (server, client, session) = ready("subtitle-change", ServerOptions::default(), 12).await;
    server.add_pgs(&[("Hello there", 1_000, 3_500)]);
    let handle = started(&client, &session, None).await;

    let (plan, epoch) = change(&client, &session, &handle, Some(0), 4_000).await;
    assert_eq!(epoch, 1);
    let PlaybackPlan::Converted {
        video, subtitle, ..
    } = plan
    else {
        panic!("not converted: {plan:?}");
    };
    assert!(
        matches!(video, Some(ConvertedVideo::Encode { .. })),
        "{video:?}"
    );
    assert!(
        matches!(
            subtitle,
            Some(ConvertedSubtitle {
                action: SubtitleAction::BurnIn { .. },
                ..
            })
        ),
        "{subtitle:?}"
    );
    let (stale, _) = client.read_at(&session, &handle, 0, 0, 100).await;
    assert_eq!(
        stale,
        MediaReadResponse::Err(Error::PlaybackRestarted { epoch: 1 })
    );
    let (bytes, _) = read_all(&client, &session, &handle, 1).await;
    let (first, _) = probe(&write(&server, "burned.mkv", &bytes));
    assert!((3.5..=4.5).contains(&first), "first packet at {first}");

    let (plan, epoch) = change(&client, &session, &handle, None, 6_000).await;
    assert_eq!(epoch, 2);
    assert!(
        matches!(
            plan,
            PlaybackPlan::Converted {
                video: Some(ConvertedVideo::Copy { .. }),
                subtitle: None,
                ..
            }
        ),
        "{plan:?}"
    );
    let (stale, _) = client.read_at(&session, &handle, 1, 0, 100).await;
    assert_eq!(
        stale,
        MediaReadResponse::Err(Error::PlaybackRestarted { epoch: 2 })
    );
    client.close().await;
    server.stop().await;
}

/// A subtitle change replaces the playback's conversion while it still
/// holds its slot. The new one runs in that slot: it once asked for
/// another, and with none free every change was refused as `Busy`.
#[tokio::test]
async fn a_subtitle_change_needs_no_second_slot() {
    let options = ServerOptions {
        max_jobs: 1,
        ..ServerOptions::default()
    };
    let (server, tv, session) = ready("subtitle-change-slot", options, 12).await;
    server.add_pgs(&[("Hello there", 1_000, 3_500)]);
    // Signed in first: a sign-in waits its turn for Argon2 (PX-10), and
    // the TV's playback idles out after PLAYBACK_IDLE.
    let desktop = server.client().await;
    desktop
        .hello_with(None, Some(progressive_capabilities()))
        .await;
    let other = desktop.login("alice", "correct horse").await;
    let handle = started(&tv, &session, None).await;
    let (_, epoch) = change(&tv, &session, &handle, Some(0), 4_000).await;
    assert_eq!(epoch, 1);
    // Still one slot, and the TV's playback holds it.
    assert!(matches!(
        start(&desktop, &other, None).await,
        Response::Err(Error::Busy)
    ));
    tv.close().await;
    desktop.close().await;
    server.stop().await;
}

#[tokio::test]
async fn simultaneous_subtitle_changes_get_distinct_epochs_in_one_slot() {
    let (server, client, session) = ready(
        "subtitle-concurrent",
        ServerOptions {
            max_jobs: 1,
            ..ServerOptions::default()
        },
        12,
    )
    .await;
    let handle = started(&client, &session, None).await;
    let (a, b) = tokio::join!(
        change(&client, &session, &handle, None, 1_000),
        change(&client, &session, &handle, None, 2_000),
    );
    let mut epochs = [a.1, b.1];
    epochs.sort();
    assert_eq!(epochs, [1, 2]);
    let (stale, _) = client.read_at(&session, &handle, 1, 0, 1).await;
    assert_eq!(
        stale,
        MediaReadResponse::Err(Error::PlaybackRestarted { epoch: 2 })
    );
    let (bytes, _) = read_all(&client, &session, &handle, 2).await;
    assert!(!bytes.is_empty());
    client.close().await;
    server.stop().await;
}

/// One frame at `secs` of `file`, grey, 640×360.
fn frame(file: &Path, secs: f64) -> Vec<u8> {
    let out = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-ss", &secs.to_string(), "-i"])
        .arg(file)
        .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "gray", "-"])
        .output()
        .unwrap();
    assert_eq!(
        out.stdout.len(),
        640 * 360,
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// How many pixels inside a box two frames clearly differ in: by more
/// than re-encoding moves a pixel. Subtitle strokes are thin, so their
/// count says more than an average over the box.
fn changed(a: &[u8], b: &[u8], (x0, y0, x1, y1): (usize, usize, usize, usize)) -> usize {
    (y0..y1)
        .flat_map(|y| (x0..x1).map(move |x| y * 640 + x))
        .filter(|&i| a[i].abs_diff(b[i]) > 40)
        .count()
}

/// A burned-in PGS cue shows in the picture where it was placed, scaled
/// from the subtitle's canvas (1920×1080, as Blu-ray PGS is, also over 4K
/// video) to the video's (640×360).
#[tokio::test]
async fn a_burned_in_subtitle_shows_in_the_picture_where_it_was_placed() {
    use spoolframe_protocol::{BitmapSubtitles, Selection};
    let (server, client, session) = ready("burn-in", unthrottled(), 6).await;
    // At (640, 960) on the canvas, 640×64: (213, 320) to (427, 341) scaled.
    server.add_pgs(&[("Hello there", 1_000, 3_500)]);
    let response = client
        .call(
            Some(&session),
            Request::StartPlayback {
                id: movie().id().clone(),
                selection: Some(Selection {
                    audio: Some(0),
                    subtitle: Some(0),
                }),
                start_ms: None,
                bitmap: BitmapSubtitles::BurnIn,
            },
        )
        .await;
    let Response::Ok(Reply::PlaybackStarted { handle, .. }) = response else {
        panic!("no playback: {response:?}");
    };
    let (bytes, _) = read_all(&client, &session, &handle, 0).await;
    let burned = frame(&write(&server, "burned.mkv", &bytes), 2.0);
    let source = frame(&server.media, 2.0);
    let text = changed(&burned, &source, (213, 320, 427, 341));
    let elsewhere = changed(&burned, &source, (0, 0, 640, 300));
    assert!(text > 100, "only {text} pixels changed in the cue's box");
    assert!(
        elsewhere < 50,
        "{elsewhere} pixels changed away from the cue"
    );
    // After the cue, nothing is burned in.
    let after = changed(
        &frame(&write(&server, "burned.mkv", &bytes), 5.0),
        &frame(&server.media, 5.0),
        (213, 320, 427, 341),
    );
    assert!(
        after < 20,
        "{after} pixels changed in the cue's box after it"
    );
    client.close().await;
    server.stop().await;
}

/// Start the film at `start_ms` with a PGS file beside it burned in, and
/// read it whole: the output, and the time of its first packet.
async fn burned_pgs_sidecar(test: &str, start_ms: Option<u64>) -> (TestServer, PathBuf, f64) {
    // The cue of the test above, in the same place.
    burned_sidecar(test, start_ms, |server| {
        server.write_pgs_sidecar(&[("Hello there", 1_000, 3_500)]);
    })
    .await
}

/// The same for any picture subtitle file, which `write` puts beside the
/// film.
async fn burned_sidecar(
    test: &str,
    start_ms: Option<u64>,
    write_sidecar: impl FnOnce(&TestServer),
) -> (TestServer, PathBuf, f64) {
    use spoolframe_protocol::{BitmapSubtitles, Selection};
    let (server, client, session) = ready(test, unthrottled(), 6).await;
    write_sidecar(&server);
    let response = client
        .call(
            Some(&session),
            Request::StartPlayback {
                id: movie().id().clone(),
                selection: Some(Selection {
                    audio: Some(0),
                    subtitle: Some(0),
                }),
                start_ms,
                bitmap: BitmapSubtitles::BurnIn,
            },
        )
        .await;
    let Response::Ok(Reply::PlaybackStarted { handle, .. }) = response else {
        panic!("no playback: {response:?}");
    };
    let read = read_all(&client, &session, &handle, 0);
    let (bytes, _) = tokio::time::timeout(Duration::from_secs(30), read)
        .await
        .expect("the encode ends");
    let burned = write(&server, "burned.mkv", &bytes);
    let (first, _) = probe(&burned);
    client.close().await;
    (server, burned, first)
}

/// How many pixels of the cue's box differ between `burned` (whose first
/// packet is at `first`) and the film, at `secs` of the film.
fn in_cue_box(server: &TestServer, burned: &Path, first: f64, secs: f64) -> usize {
    changed(
        &frame(burned, secs - first),
        &frame(&server.media, secs),
        (213, 320, 427, 341),
    )
}

/// A PGS file beside the film is burned in as an embedded PGS track is:
/// its pictures laid over the video, at their own times. It once went to
/// the filter that draws text, which refuses pictures, and the playback
/// failed.
#[tokio::test]
async fn a_burned_in_pgs_file_beside_the_film_shows_in_the_picture() {
    let (server, burned, first) = burned_pgs_sidecar("burn-in-sidecar", None).await;
    // The cue is shown from 1 s to 3.5 s: late in it, and after it.
    let during = in_cue_box(&server, &burned, first, 3.2);
    assert!(
        during > 100,
        "only {during} pixels changed in the cue's box"
    );
    let after = in_cue_box(&server, &burned, first, 5.0);
    assert!(
        after < 20,
        "{after} pixels changed in the cue's box after it"
    );
    server.stop().await;
}

/// Started in the middle of a cue, the cue is in the picture.
#[tokio::test]
async fn a_burned_in_pgs_file_shows_a_cue_that_began_before_the_start() {
    let (server, burned, first) = burned_pgs_sidecar("burn-in-sidecar-seek", Some(2_000)).await;
    assert!((1.5..=2.5).contains(&first), "first packet at {first}");
    let during = in_cue_box(&server, &burned, first, 3.0);
    assert!(
        during > 100,
        "only {during} pixels changed in the cue's box"
    );
    let after = in_cue_box(&server, &burned, first, 5.0);
    assert!(
        after < 20,
        "{after} pixels changed in the cue's box after it"
    );
    server.stop().await;
}

/// The packet times of `file`'s first subtitle stream, in seconds.
fn subtitle_times(file: &Path) -> Vec<f64> {
    let out = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "s:0"])
        .args(["-show_entries", "packet=pts_time", "-of", "csv=p=0"])
        .arg(file)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().trim_end_matches(',').parse().ok())
        .collect()
}

/// A PGS file beside the film, sent on as a stream to a client that draws
/// PGS, keeps its cues' times. Played from the start, ffmpeg once moved
/// the file back by its first cue's time, and every cue showed that much
/// early.
#[tokio::test]
async fn a_copied_pgs_file_beside_the_film_keeps_its_cues_times() {
    use spoolframe_protocol::{ConvertedSubtitle, Selection, SubtitleAction};
    use spoolframe_types::SubtitleFormat;
    let (server, other, _) = ready("copy-sidecar", unthrottled(), 6).await;
    other.close().await;
    let caps = ClientCapabilities {
        subtitles: [SubtitleFormat::Pgs].into(),
        ..progressive_capabilities()
    };
    let client = server.client().await;
    client.hello_with(None, Some(caps)).await;
    let session = client.login("alice", "correct horse").await;
    server.write_pgs_sidecar(&[("Hello there", 3_000, 4_500)]);
    let response = client
        .call(
            Some(&session),
            Request::StartPlayback {
                id: movie().id().clone(),
                selection: Some(Selection {
                    audio: Some(0),
                    subtitle: Some(0),
                }),
                start_ms: None,
                bitmap: Default::default(),
            },
        )
        .await;
    let Response::Ok(Reply::PlaybackStarted { handle, plan, .. }) = response else {
        panic!("no playback: {response:?}");
    };
    assert!(
        matches!(
            plan,
            PlaybackPlan::Converted {
                subtitle: Some(ConvertedSubtitle {
                    action: SubtitleAction::Copy { .. },
                    ..
                }),
                ..
            }
        ),
        "{plan:?}"
    );
    let read = read_all(&client, &session, &handle, 0);
    let (bytes, _) = tokio::time::timeout(Duration::from_secs(30), read)
        .await
        .expect("the encode ends");
    let times = subtitle_times(&write(&server, "copied.mkv", &bytes));
    let first = times
        .first()
        .copied()
        .expect("the subtitle is in the output");
    assert!(
        (first - 3.0).abs() < 0.1,
        "the cue's packets are at {times:?}"
    );
    client.close().await;
    server.stop().await;
}

/// A VobSub subtitle is two files, `Movie.en.idx` and `Movie.en.sub`, and
/// ffmpeg finds the second by the name of the first. The job read the
/// `.idx` through a link named `sidecar`, where ffmpeg found no `.sub`:
/// the playback was offered, and failed.
#[tokio::test]
async fn a_copied_vobsub_file_beside_the_film_is_in_the_output() {
    use spoolframe_protocol::{ConvertedSubtitle, Selection, SubtitleAction};
    use spoolframe_types::SubtitleFormat;
    let (server, other, _) = ready("copy-vobsub", unthrottled(), 6).await;
    other.close().await;
    let caps = ClientCapabilities {
        subtitles: [SubtitleFormat::VobSub].into(),
        ..progressive_capabilities()
    };
    let client = server.client().await;
    client.hello_with(None, Some(caps)).await;
    let session = client.login("alice", "correct horse").await;
    server.write_vobsub_sidecar(&[("Hello there", 3_000, 4_500), ("Again", 5_000, 5_500)]);
    let response = client
        .call(
            Some(&session),
            Request::StartPlayback {
                id: movie().id().clone(),
                selection: Some(Selection {
                    audio: Some(0),
                    subtitle: Some(0),
                }),
                start_ms: None,
                bitmap: Default::default(),
            },
        )
        .await;
    let Response::Ok(Reply::PlaybackStarted { handle, plan, .. }) = response else {
        panic!("no playback: {response:?}");
    };
    assert!(
        matches!(
            plan,
            PlaybackPlan::Converted {
                subtitle: Some(ConvertedSubtitle {
                    action: SubtitleAction::Copy {
                        format: SubtitleFormat::VobSub
                    },
                    ..
                }),
                ..
            }
        ),
        "{plan:?}"
    );
    let read = read_all(&client, &session, &handle, 0);
    let (bytes, _) = tokio::time::timeout(Duration::from_secs(30), read)
        .await
        .expect("the encode ends");
    let times = subtitle_times(&write(&server, "copied.mkv", &bytes));
    assert_eq!(times, [3.0, 5.0]);
    client.close().await;
    server.stop().await;
}

/// A VobSub file beside the film is burned in as a PGS file is. It was
/// refused, for the same missing `.sub`.
#[tokio::test]
async fn a_burned_in_vobsub_file_beside_the_film_shows_in_the_picture() {
    let (server, burned, first) = burned_sidecar("burn-in-vobsub", None, |server| {
        server.write_vobsub_sidecar(&[("Hello there", 1_000, 3_500), ("Again", 4_000, 4_400)]);
    })
    .await;
    let before = in_cue_box(&server, &burned, first, 0.5);
    assert!(
        before < 20,
        "{before} pixels changed in the cue's box before it"
    );
    let during = in_cue_box(&server, &burned, first, 3.2);
    assert!(
        during > 100,
        "only {during} pixels changed in the cue's box"
    );
    let after = in_cue_box(&server, &burned, first, 5.0);
    assert!(
        after < 20,
        "{after} pixels changed in the cue's box after it"
    );
    server.stop().await;
}

/// A segment can go between the moment the server sees it finished and
/// the moment it opens it: removed for room, or with all the job's files
/// when the job fails for a full scratch space. That is the state made
/// here: an encoder whose playlist lists a segment that is not on disk.
/// The read was answered as the whole of an empty segment.
#[tokio::test]
async fn a_segment_gone_before_it_is_opened_is_not_an_empty_segment() {
    let dir = fresh_dir("hls-segment-gone").await;
    std::fs::create_dir_all(&dir).unwrap();
    let stub = dir.join("listing-ffmpeg");
    crate::support::write_script(
        &stub,
        "#!/bin/sh\n\
         for playlist; do :; done\n\
         dir=$(dirname \"$playlist\")\n\
         printf 'init' > \"$dir/init.mp4\"\n\
         printf '#EXTM3U\\n#EXTINF:2.000000,\\nseg1.m4s\\n' > \"$playlist\"\n\
         exec sleep 30\n",
    );
    let options = ServerOptions {
        ffmpeg: stub,
        ..ServerOptions::default()
    };
    let (server, client, session) = hls_ready_with(dir, hls_capabilities(), options).await;
    let handle = started(&client, &session, None).await;
    // The init segment is whole once the job has seen segment 1 listed.
    assert_eq!(segment(&client, &session, &handle, 0).await, b"init");
    let (response, bytes) = client
        .media(MediaRead {
            session: session.clone(),
            handle: handle.clone(),
            offset: 0,
            length: MAX_READ,
            epoch: 0,
            segment: Some(1),
        })
        .await;
    let MediaReadResponse::Ok(reply) = response else {
        panic!("segment 1: {response:?}");
    };
    // Not ready: the client asks again.
    assert_eq!((reply.complete, reply.size, reply.sent), (false, 0, 0));
    assert!(bytes.is_empty());
    client.close().await;
    server.stop().await;
}
