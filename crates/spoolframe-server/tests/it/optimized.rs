//! Playback from optimized copies (PX-37), with real ffmpeg and ffprobe:
//! a 640×360 H.264 film at 8 Mbit/s, played at 3 Mbit/s, and copies of
//! it the server's pool makes. Which copy fits is tested in
//! `optimized::play::tests`.

use std::time::Duration;

use spoolframe_protocol::{
    AudioAction, ConvertedAudio, PlaybackHandle, PlaybackPlan, Quality, Reply, Request, Response,
    SessionId,
};
use spoolframe_server::downloads::DeviceClass;
use spoolframe_server::optimized::Deleted;
use spoolframe_server::optimized::requests::{Asked, Scope};
use spoolframe_server::store::optimized::{
    CopyRow, CopyState, CopyTarget, CopyVideo, NewCopy, Profile, Recipe,
};
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
    copied: false,
};

/// A copy too short to stand in for the film's video.
const TINY: CopyVideo = CopyVideo {
    codec: VideoCodec::H264,
    height: 144,
    max_bitrate: Some(200_000),
    tone_map: false,
    copied: false,
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
    made_for(server, CopyTarget::Profile("Mobile".into()), video).await
}

/// The film's copy for `target`, made as `video` says.
async fn made_for(server: &TestServer, target: CopyTarget, video: CopyVideo) -> CopyRow {
    let row = server
        .pool()
        .request(asked(server, target, video).await)
        .await
        .unwrap();
    let until = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let state = server
            .pool()
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

/// The film's copy for `target`, as `video` says, asked for.
async fn asked(server: &TestServer, target: CopyTarget, video: CopyVideo) -> NewCopy {
    NewCopy {
        media_id: movie().id().to_string(),
        media_path: server.media.display().to_string(),
        source_stamp: spoolframe_server::files::stamp_text(&server.media)
            .await
            .unwrap(),
        target,
        video,
    }
}

async fn start(client: &TestClient, session: &SessionId) -> (PlaybackHandle, PlaybackPlan) {
    start_item(client, session, movie().id()).await
}

async fn start_item(
    client: &TestClient,
    session: &SessionId,
    id: &spoolframe_types::MediaId,
) -> (PlaybackHandle, PlaybackPlan) {
    start_at(client, session, id, None).await
}

/// Start `id` at `start_ms`.
async fn start_at(
    client: &TestClient,
    session: &SessionId,
    id: &spoolframe_types::MediaId,
    start_ms: Option<u64>,
) -> (PlaybackHandle, PlaybackPlan) {
    let response = client
        .call(
            Some(session),
            Request::StartPlayback {
                id: id.clone(),
                selection: None,
                start_ms,
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

#[tokio::test]
async fn with_no_copy_the_capped_film_is_encoded() {
    let (server, client, session) = ready("optimized-none", "ac3", true).await;
    let (_, plan) = start(&client, &session).await;
    assert!(matches!(&plan, PlaybackPlan::Converted { .. }), "{plan:?}");
    client.close().await;
    server.stop().await;
}

/// A copy is its film and its stored tracks, read together: it plays
/// remuxed, never by range, whatever the device reads.
#[tokio::test]
async fn a_plan_that_would_encode_plays_a_ready_copy_remuxed() {
    let (server, client, session) = ready("optimized-direct", "ac3", true).await;
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

/// The film's ffmpeg command line, for its one open playback.
async fn command(server: &TestServer) -> String {
    let id = server.debug.list().open[0].id.clone();
    server.debug.detail(&id).await.unwrap().command.unwrap()
}

/// The copy's stored video is read beside the film and copied: no
/// encode, so a seek is a remux from there, at once.
#[tokio::test]
async fn a_seek_in_a_played_copy_is_a_remux_of_its_stored_video() {
    let (server, client, session) = ready("optimized-seek", "ac3", true).await;
    made(&server, FITS).await;
    start_at(&client, &session, movie().id(), Some(3_000)).await;
    let command = command(&server).await;
    assert!(
        command.contains("-ss 3.000")
            && command.contains("/video'")
            && command.contains("-c:v copy")
            && !command.contains("libx264"),
        "{command}"
    );
    client.close().await;
    server.stop().await;
}

/// A DTS film's copy plays from three files with no encode: the film
/// (nothing of it but what the copy keeps), the stored video and the
/// stored audio.
#[tokio::test]
async fn a_copy_with_stored_audio_plays_from_the_film_and_both_tracks() {
    let (server, client, session) = ready("optimized-three-inputs", "dca", false).await;
    made(&server, FITS).await;
    let (_, plan) = start(&client, &session).await;
    let command = command(&server).await;
    assert_eq!(
        (
            spoolframe_server::planner::Kind::of(&plan),
            command.matches(" -i ").count(),
            command.contains("-c:a copy"),
        ),
        (spoolframe_server::planner::Kind::Remux, 3, true),
        "{command}"
    );
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
        server.pool().delete(&copy.id).await.unwrap(),
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

/// The film's own video, copied: a download's copy in original quality
/// (PX-38) converts only its audio.
const ORIGINAL: CopyVideo = CopyVideo {
    codec: VideoCodec::H264,
    height: 360,
    max_bitrate: None,
    tone_map: false,
    copied: true,
};

/// A download's copy for an Apple device.
const FOR_APPLE: CopyTarget = CopyTarget::Original {
    class: DeviceClass::Apple,
};

/// A server with binge-ahead off (the server's own default), the film in
/// `audio`, and a signed-in device that plays E-AC-3 too: a copy's
/// converted audio fits it, E-AC-3 or AAC.
async fn ready_off(test: &str, audio: &str) -> (TestServer, TestClient, SessionId) {
    let server = TestServer::start_with(
        fresh_dir(test).await,
        ServerOptions {
            real_media: true,
            binge_ahead: false,
            ..ServerOptions::default()
        },
    )
    .await;
    write_film(&server, audio);
    server.add_user("alice", "correct horse", None).await;
    let client = server.client().await;
    let mut caps = device(false);
    caps.audio.insert(AudioCodec::Eac3);
    client.hello_with(None, Some(caps)).await;
    let session = client.login("alice", "correct horse").await;
    (server, client, session)
}

/// Until `id`, an on-demand request, has every item's copy made.
async fn request_done(server: &TestServer, id: &str) {
    let until = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let progress = server.pool().request_progress(id).await.unwrap().unwrap();
        assert_eq!(progress.failed, 0, "{progress:?}");
        if progress.items > 0 && progress.done == progress.items {
            return;
        }
        assert!(
            std::time::Instant::now() < until,
            "the request is never done: {progress:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// With binge-ahead off, playback still plays from the copies there are
/// (the user's decision, 2026-10-20): the film an admin optimized on
/// demand plays remuxed from its copy, with no live encode.
#[tokio::test]
async fn with_binge_ahead_off_a_playback_plays_a_ready_on_demand_copy() {
    let (server, client, session) = ready_off("optimized-off", "ac3").await;
    server
        .store
        .put_optimized_profile(&Profile {
            name: "Small".into(),
            video_codec: VideoCodec::H264,
            height: 360,
            max_bitrate: 1_000_000,
        })
        .await
        .unwrap();
    let request = server
        .pool()
        .put_request(Asked {
            scope: Scope::Item(movie().id().clone()),
            profile: "Small".into(),
            pinned: false,
            created_by: "admin".into(),
        })
        .await
        .unwrap();
    request_done(&server, &request.row.id).await;
    let (_, plan) = start(&client, &session).await;
    assert_eq!(
        spoolframe_server::planner::Kind::of(&plan),
        spoolframe_server::planner::Kind::Remux,
        "{plan:?}"
    );
    client.close().await;
    server.stop().await;
}

/// The copy stores the film's DTS converted, alone: its video is the
/// film's own, read from the film whenever the copy is (the track store).
#[tokio::test]
async fn with_binge_ahead_off_a_download_s_copy_stores_only_the_converted_audio() {
    let (server, client, _) = ready_off("optimized-off-original", "dca").await;
    let copy = made_for(&server, FOR_APPLE, ORIGINAL).await;
    let tracks = server.store.optimized_copy_tracks(&copy.id).await.unwrap();
    assert!(
        matches!(tracks.as_slice(), [t] if matches!(t.recipe, Recipe::Audio(_))),
        "{tracks:?}"
    );
    client.close().await;
    server.stop().await;
}

/// Asked for again, the copy made is the one there is: no new row, and
/// nothing is made.
#[tokio::test]
async fn a_second_request_for_a_download_s_copy_makes_none() {
    let (server, client, _) = ready_off("optimized-off-again", "dca").await;
    let first = made_for(&server, FOR_APPLE, ORIGINAL).await;
    let made_at = server
        .store
        .optimized_copy(&first.id)
        .await
        .unwrap()
        .unwrap()
        .made_at;
    let tracks = || async {
        let mut made: Vec<_> = (server.store.optimized_tracks().await.unwrap())
            .into_iter()
            .map(|t| (t.id, t.state, t.made_at))
            .collect();
        made.sort_by(|a, b| a.0.cmp(&b.0));
        made
    };
    let before = tracks().await;
    let again = server
        .pool()
        .request(asked(&server, FOR_APPLE, ORIGINAL).await)
        .await
        .unwrap();
    // Nothing new is asked for or made again: the tracks are the same
    // rows, made when they were.
    assert_eq!(
        (
            again.id,
            again.state,
            again.made_at,
            server.store.optimized_copies().await.unwrap().len(),
            tracks().await,
        ),
        (first.id, CopyState::Ready, made_at, 1, before)
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

/// One ffmpeg, reading the copy's stored audio as its second input,
/// copying it.
#[tokio::test]
async fn mixed_tracks_are_one_ffmpeg_reading_the_copy_s_audio_as_it_is() {
    let (server, client, session) = ready("optimized-mixed-args", "dca", false).await;
    made(&server, TINY).await;
    start(&client, &session).await;
    let command = command(&server).await;
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

/// Off, an episode played converted has nothing made ahead, though its
/// encode has written to its end.
#[tokio::test]
async fn with_binge_ahead_off_an_episode_played_converted_has_nothing_made_ahead() {
    let (server, client, session) = ready_off("optimized-binge-off", "ac3").await;
    let [first, _, _] = write_episodes(&server);
    let (_handle, plan) = start_item(&client, &session, &first).await;
    assert_eq!(
        spoolframe_server::planner::Kind::of(&plan),
        spoolframe_server::planner::Kind::Transcode,
        "{plan:?}"
    );
    let until = std::time::Instant::now() + Duration::from_secs(60);
    while server.jobs.encoding().running() > 0 {
        assert!(std::time::Instant::now() < until, "the encode never ends");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // On, the next episode is asked for as the encode catches up.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        server.store.optimized_copies().await.unwrap().is_empty(),
        "a copy was asked for"
    );
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
