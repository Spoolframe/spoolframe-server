//! Playback from optimized copies (PX-37), with real ffmpeg and ffprobe:
//! a 640×360 H.264 film at 8 Mbit/s, played at 3 Mbit/s, and copies of
//! it the server's pool makes. Which copy fits is tested in
//! `optimized::play::tests`.

use std::time::Duration;

use spoolframe_protocol::resource::ResourceRequest;
use spoolframe_protocol::{
    AudioAction, ConvertedAudio, MediaRead, PlaybackHandle, PlaybackPlan, Quality, Reply, Request,
    Response, SessionId,
};
use spoolframe_server::optimized::Deleted;
use spoolframe_server::store::optimized::{CopyRow, CopyState, CopyTarget, CopyVideo, NewCopy};
use spoolframe_types::{AudioCodec, ClientCapabilities, Container, Delivery, VideoCodec};

use crate::support::{ServerOptions, TestClient, TestServer, episode, fresh_dir, movie};

/// Under the film's 8 Mbit/s: its video is encoded to play.
const CAPPED: Quality = Quality::Bitrate {
    max_bitrate: 3_000_000,
};

/// A copy that fits [`CAPPED`]: the film's height, at 1 Mbit/s.
const FITS: CopyVideo = CopyVideo {
    codec: VideoCodec::H264,
    height: 360,
    max_bitrate: Some(1_000_000),
    tone_map: false,
};

/// A copy too short to stand in for the film's video.
const TINY: CopyVideo = CopyVideo {
    codec: VideoCodec::H264,
    height: 144,
    max_bitrate: Some(200_000),
    tone_map: false,
};

/// A device that plays H.264 with AC-3 or AAC in Matroska: by range, if
/// `range`, else as a progressive stream.
fn device(range: bool) -> ClientCapabilities {
    ClientCapabilities {
        video: [VideoCodec::H264].into(),
        audio: [AudioCodec::Ac3, AudioCodec::Aac].into(),
        containers: [Container::Matroska].into(),
        delivery: if range {
            vec![Delivery::Range, Delivery::Progressive]
        } else {
            vec![Delivery::Progressive]
        },
        ..ClientCapabilities::default()
    }
}

/// Write the movie: 4 s of H.264 at a constant 8 Mbit/s, with a stereo
/// track in `audio` (an ffmpeg encoder; `dca` is DTS).
fn write_film(server: &TestServer, audio: &str) {
    std::fs::create_dir_all(server.media.parent().unwrap()).unwrap();
    let status = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", "testsrc2=size=640x360:rate=25"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"])
        .args(["-t", "4", "-pix_fmt", "yuv420p", "-g", "50"])
        .args(["-c:v", "libx264", "-b:v", "8M", "-minrate", "8M"])
        .args([
            "-maxrate",
            "8M",
            "-bufsize",
            "2M",
            "-x264-params",
            "nal-hrd=cbr",
        ])
        .args(["-c:a", audio, "-strict", "-2", "-ac", "2"])
        .arg(&server.media)
        .status()
        .expect("ffmpeg runs");
    assert!(status.success(), "ffmpeg could not make the film");
}

/// A server with the film in `audio`, and a signed-in device.
async fn ready(test: &str, audio: &str, range: bool) -> (TestServer, TestClient, SessionId) {
    let server = TestServer::start_with(
        fresh_dir(test).await,
        ServerOptions {
            real_media: true,
            ..ServerOptions::default()
        },
    )
    .await;
    write_film(&server, audio);
    server.add_user("alice", "correct horse", None).await;
    let client = server.client().await;
    client.hello_with(None, Some(device(range))).await;
    let session = client.login("alice", "correct horse").await;
    (server, client, session)
}

/// The film's copy in profile `Mobile`, made as `video` says.
async fn made(server: &TestServer, video: CopyVideo) -> CopyRow {
    let asked = NewCopy {
        media_id: movie().id().to_string(),
        media_path: server.media.display().to_string(),
        source_stamp: spoolframe_server::files::stamp_text(&server.media)
            .await
            .unwrap(),
        target: CopyTarget::Profile("Mobile".into()),
        video,
    };
    let row = server.optimized.request(asked).await.unwrap();
    let until = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let state = server
            .optimized
            .progress(&row.id)
            .await
            .unwrap()
            .unwrap()
            .state;
        match state {
            CopyState::Ready => break,
            CopyState::Failed => panic!(
                "the copy failed: {:?}",
                server.store.optimized_copy(&row.id).await.unwrap()
            ),
            _ => {}
        }
        assert!(std::time::Instant::now() < until, "the copy is never made");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    row
}

async fn start(client: &TestClient, session: &SessionId) -> (PlaybackHandle, PlaybackPlan) {
    start_item(client, session, movie().id()).await
}

async fn start_item(
    client: &TestClient,
    session: &SessionId,
    id: &spoolframe_types::MediaId,
) -> (PlaybackHandle, PlaybackPlan) {
    let response = client
        .call(
            Some(session),
            Request::StartPlayback {
                id: id.clone(),
                selection: None,
                start_ms: None,
                bitmap: Default::default(),
                quality: CAPPED,
                display: None,
            },
        )
        .await;
    match response {
        Response::Ok(Reply::PlaybackStarted { handle, plan, .. }) => (handle, plan),
        other => panic!("not started: {other:?}"),
    }
}

/// `length` bytes of the playback's file from `offset`.
async fn read(
    client: &TestClient,
    session: &SessionId,
    handle: &PlaybackHandle,
    offset: u64,
    length: u64,
) -> Vec<u8> {
    let (_, bytes) = client
        .resource(&ResourceRequest::Media {
            request: MediaRead {
                session: session.clone(),
                handle: handle.clone(),
                offset,
                length,
                epoch: 0,
                segment: None,
                variant: 0,
            },
            revision: 0,
        })
        .await;
    bytes
}

#[tokio::test]
async fn with_no_copy_the_capped_film_is_encoded() {
    let (server, client, session) = ready("optimized-none", "ac3", true).await;
    let (_, plan) = start(&client, &session).await;
    assert!(matches!(&plan, PlaybackPlan::Converted { .. }), "{plan:?}");
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_plan_that_would_encode_plays_a_ready_copy_directly() {
    let (server, client, session) = ready("optimized-direct", "ac3", true).await;
    made(&server, FITS).await;
    let (_, plan) = start(&client, &session).await;
    assert!(matches!(&plan, PlaybackPlan::Direct { .. }), "{plan:?}");
    client.close().await;
    server.stop().await;
}

/// A seek is a read elsewhere in the copy, with no encode to start.
#[tokio::test]
async fn a_read_near_the_end_of_a_played_copy_is_the_copy_s_own_bytes() {
    let (server, client, session) = ready("optimized-seek", "ac3", true).await;
    let copy = made(&server, FITS).await;
    let (handle, _) = start(&client, &session).await;
    let file = std::fs::read(server.optimized.file(&copy.id)).unwrap();
    let tail = file.len() as u64 - 4096;
    let bytes = read(&client, &session, &handle, tail, 4096).await;
    assert_eq!(bytes, file[tail as usize..]);
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_copy_of_the_film_as_it_was_is_not_played() {
    let (server, client, session) = ready("optimized-stale", "ac3", true).await;
    made(&server, FITS).await;
    // The film is made again: another file under the same name.
    write_film(&server, "ac3");
    let (_, plan) = start(&client, &session).await;
    assert!(matches!(&plan, PlaybackPlan::Converted { .. }), "{plan:?}");
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_copy_being_played_is_not_removed() {
    let (server, client, session) = ready("optimized-leased", "ac3", true).await;
    let copy = made(&server, FITS).await;
    let (_handle, _) = start(&client, &session).await;
    assert_eq!(
        server.optimized.delete(&copy.id).await.unwrap(),
        Deleted::Deferred
    );
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_playback_s_start_counts_as_a_use_of_its_copy() {
    let (server, client, session) = ready("optimized-used", "ac3", true).await;
    let copy = made(&server, FITS).await;
    let before = server
        .store
        .optimized_copy(&copy.id)
        .await
        .unwrap()
        .unwrap();
    server.clock.advance(Duration::from_secs(3600));
    start(&client, &session).await;
    let after = server
        .store
        .optimized_copy(&copy.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        after.last_used_at > before.last_used_at,
        "{before:?} {after:?}"
    );
    client.close().await;
    server.stop().await;
}

/// The film's DTS is not played: its copy's AAC is, beside a live encode
/// of the film's video, which the copy is too short to stand in for.
#[tokio::test]
async fn a_copy_s_converted_audio_plays_beside_a_live_video_encode() {
    let (server, client, session) = ready("optimized-mixed", "dca", false).await;
    made(&server, TINY).await;
    let (_, plan) = start(&client, &session).await;
    let PlaybackPlan::Converted {
        audio: Some(ConvertedAudio { action, .. }),
        ..
    } = plan
    else {
        panic!("no converted audio: {plan:?}");
    };
    assert_eq!(
        action,
        AudioAction::Copy {
            codec: AudioCodec::Aac
        }
    );
    client.close().await;
    server.stop().await;
}

/// One ffmpeg, reading the copy as its second input, copying its audio.
#[tokio::test]
async fn mixed_tracks_are_one_ffmpeg_reading_the_copy_s_audio_as_it_is() {
    let (server, client, session) = ready("optimized-mixed-args", "dca", false).await;
    made(&server, TINY).await;
    start(&client, &session).await;
    let id = server.debug.list().open[0].id.clone();
    let command = server.debug.detail(&id).await.unwrap().command.unwrap();
    let inputs = command.matches(" -i ").count();
    assert_eq!(
        (inputs, command.contains("-c:a copy")),
        (2, true),
        "{command}"
    );
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_conversion_with_a_copy_s_audio_runs_to_the_film_s_end() {
    let (server, client, session) = ready("optimized-mixed-runs", "dca", false).await;
    made(&server, TINY).await;
    start(&client, &session).await;
    let until = std::time::Instant::now() + Duration::from_secs(60);
    let state = loop {
        let state = server.debug.list().open[0].conversion.clone().unwrap();
        if !state.running {
            break state;
        }
        assert!(
            std::time::Instant::now() < until,
            "the conversion never ends"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(state.failed, None);
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_device_without_range_reads_a_copy_remuxed() {
    let (server, client, session) = ready("optimized-remux", "ac3", false).await;
    made(&server, FITS).await;
    let (_, plan) = start(&client, &session).await;
    assert_eq!(
        spoolframe_server::planner::Kind::of(&plan),
        spoolframe_server::planner::Kind::Remux,
        "{plan:?}"
    );
    client.close().await;
    server.stop().await;
}

/// Episodes 12 (S1E1), 13 (S1E2) and 15 (S2E1) of the fake show, each
/// the film.
fn write_episodes(server: &TestServer) -> [spoolframe_types::MediaId; 3] {
    ["12", "13", "15"].map(|key| {
        let id = episode(key, "11", 1, 1).id().clone();
        let path = spoolframe_server::test_support::episode_media(&server.media, &id);
        std::fs::copy(&server.media, path).unwrap();
        id
    })
}

/// The binge-ahead copy of `episode`, once it is made.
async fn made_ahead(server: &TestServer, episode: &spoolframe_types::MediaId) -> CopyRow {
    let until = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let copies = server.store.optimized_copies().await.unwrap();
        let ahead = copies
            .into_iter()
            .find(|c| c.target == CopyTarget::Ahead && c.media_id == episode.to_string());
        match ahead {
            Some(copy) if copy.state == CopyState::Ready => return copy,
            Some(copy) if copy.state == CopyState::Failed => panic!("the copy failed: {copy:?}"),
            _ => {}
        }
        assert!(
            std::time::Instant::now() < until,
            "the next episode is never made ahead"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn an_episode_played_converted_has_the_next_one_made_ahead_with_its_plan() {
    let (server, client, session) = ready("optimized-binge", "ac3", true).await;
    let [first, second, _] = write_episodes(&server);
    let (_handle, plan) = start_item(&client, &session, &first).await;
    let PlaybackPlan::Converted {
        video: Some(spoolframe_protocol::ConvertedVideo::Encode { height, .. }),
        ..
    } = plan
    else {
        panic!("the first episode's video is not encoded: {plan:?}");
    };
    let copy = made_ahead(&server, &second).await;
    assert_eq!(copy.video.height, height);
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn the_episode_made_ahead_plays_from_its_copy_with_no_encode() {
    let (server, client, session) = ready("optimized-binge-plays", "ac3", true).await;
    let [first, second, _] = write_episodes(&server);
    let (_handle, _) = start_item(&client, &session, &first).await;
    made_ahead(&server, &second).await;
    let (_, plan) = start_item(&client, &session, &second).await;
    assert_ne!(
        spoolframe_server::planner::Kind::of(&plan),
        spoolframe_server::planner::Kind::Transcode,
        "{plan:?}"
    );
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn the_episode_played_from_its_ahead_copy_has_the_next_made_ahead_alike() {
    let (server, client, session) = ready("optimized-binge-chain", "ac3", true).await;
    let [first, second, third] = write_episodes(&server);
    let (_first, _) = start_item(&client, &session, &first).await;
    let made = made_ahead(&server, &second).await;
    let (_second, _) = start_item(&client, &session, &second).await;
    let next = made_ahead(&server, &third).await;
    assert_eq!(next.video, made.video);
    client.close().await;
    server.stop().await;
}
