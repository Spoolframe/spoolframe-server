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
        fresh_dir(test),
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
    until(
        "the idle playback's ffmpeg is gone",
        PLAYBACK_IDLE * 4,
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
    let (server, client, session) = ready("encode-busy", options, 120).await;
    started(&client, &session, None).await;
    assert!(matches!(
        start(&client, &session, None).await,
        Response::Err(Error::Busy)
    ));
    client.close().await;
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

#[tokio::test]
async fn a_failed_encode_fails_its_reads_with_ffmpeg_s_reason() {
    let dir = fresh_dir("encode-failed");
    std::fs::create_dir_all(&dir).unwrap();
    let stub = dir.join("failing-ffmpeg");
    std::fs::write(
        &stub,
        "#!/bin/sh\necho 'input: Invalid data found when processing input' >&2\nexit 1\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
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
            reason: "input: Invalid data found when processing input".into()
        })
    );
    client.close().await;
    server.stop().await;
}

/// A server with a 30 s H.264 + AC-3 file (keyframes every 2 s), and a
/// client signed in with `caps`.
async fn hls_ready(test: &str, caps: ClientCapabilities) -> (TestServer, TestClient, SessionId) {
    hls_ready_with(fresh_dir(test), caps, ServerOptions::default()).await
}

async fn hls_ready_with(
    dir: PathBuf,
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
    let dir = fresh_dir("hls-short-ahead");
    std::fs::create_dir_all(&dir).unwrap();
    let slow = dir.join("slow-ffmpeg");
    std::fs::write(&slow, "#!/bin/sh\nexec ffmpeg -readrate 4 \"$@\"\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).unwrap();
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
