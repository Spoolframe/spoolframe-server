//! Bitmap subtitles read as text (P2-07), through the protocol, with real
//! ffmpeg, ffprobe and tesseract on a file with a PGS track.

use crate::support;

use std::time::{Duration, Instant};

use spoolframe_protocol::{
    BitmapSubtitles, Error, PlannedSubtitle, PlaybackHandle, PlaybackPlan, Reply, Request,
    Response, Selection, SessionId, SubtitleDelivery,
};
use spoolframe_types::{AudioCodec, ClientCapabilities, Container, SubtitleFormat, VideoCodec};
use support::{ServerOptions, TestClient, TestServer, fresh_dir, movie};

/// The text the PGS track shows, as tesseract reads it back.
const SRT: &str = "1\n00:00:01,000 --> 00:00:03,500\nHello there\n\n\
                   2\n00:00:05,000 --> 00:00:07,250\nGeneral Kenobi\n\n";

/// A Linux-like client: plays the file directly, takes SubRip, and draws
/// PGS only if `pgs`.
fn capabilities(pgs: bool) -> ClientCapabilities {
    let mut subtitles = vec![SubtitleFormat::Srt];
    if pgs {
        subtitles.push(SubtitleFormat::Pgs);
    }
    ClientCapabilities {
        video: [VideoCodec::H264].into(),
        audio: [AudioCodec::Aac].into(),
        containers: [Container::Matroska].into(),
        subtitles: subtitles.into_iter().collect(),
        ..ClientCapabilities::default()
    }
}

async fn ready(test: &str, ocr: bool, pgs: bool) -> (TestServer, TestClient, SessionId) {
    let server = TestServer::start_with(
        fresh_dir(test).await,
        ServerOptions {
            real_media: true,
            ocr,
            ..ServerOptions::default()
        },
    )
    .await;
    server.generate_media(10, "libx264", "aac");
    server.add_pgs(&[
        ("Hello there", 1_000, 3_500),
        ("General Kenobi", 5_000, 7_250),
    ]);
    server.add_user("alice", "correct horse", None).await;
    let client = server.client().await;
    client.hello_with(None, Some(capabilities(pgs))).await;
    let session = client.login("alice", "correct horse").await;
    (server, client, session)
}

/// The PGS track picked.
fn with_pgs() -> Option<Selection> {
    Some(Selection {
        audio: Some(0),
        subtitle: Some(0),
    })
}

async fn start(
    client: &TestClient,
    session: &SessionId,
    bitmap: BitmapSubtitles,
) -> (PlaybackHandle, PlaybackPlan) {
    let response = client
        .call(
            Some(session),
            Request::StartPlayback {
                id: movie().id().clone(),
                selection: with_pgs(),
                start_ms: None,
                bitmap,
            },
        )
        .await;
    match response {
        Response::Ok(Reply::PlaybackStarted { handle, plan, .. }) => (handle, plan),
        other => panic!("playback did not start: {other:?}"),
    }
}

/// `ReadSubtitle` until the text is there, as a client does.
async fn text(client: &TestClient, session: &SessionId, handle: &PlaybackHandle) -> String {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut percents = Vec::new();
    loop {
        let response = client
            .call(
                Some(session),
                Request::ReadSubtitle {
                    handle: handle.clone(),
                },
            )
            .await;
        match response {
            Response::Ok(Reply::Subtitle { format, bytes }) => {
                assert_eq!(format, SubtitleFormat::Srt);
                return String::from_utf8(bytes).unwrap();
            }
            Response::Err(Error::SubtitleNotReady { done_percent }) => {
                percents.push(done_percent);
                assert!(Instant::now() < deadline, "still reading: {percents:?}");
            }
            other => panic!("no subtitle: {other:?}"),
        }
    }
}

fn read_as_srt() -> Option<PlannedSubtitle> {
    Some(PlannedSubtitle {
        index: 0,
        format: SubtitleFormat::Srt,
        delivery: SubtitleDelivery::External,
    })
}

#[tokio::test]
async fn a_pgs_track_is_read_as_text_and_the_file_still_plays_directly() {
    let (server, client, session) = ready("ocr-direct", true, false).await;
    let (handle, plan) = start(&client, &session, BitmapSubtitles::Auto).await;
    let PlaybackPlan::Direct { subtitle, .. } = plan else {
        panic!("not direct: {plan:?}");
    };
    assert_eq!(subtitle, read_as_srt());
    assert_eq!(text(&client, &session, &handle).await, SRT);
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_track_read_once_is_ready_at_once_after() {
    let (server, client, session) = ready("ocr-cached", true, false).await;
    let (first, _) = start(&client, &session, BitmapSubtitles::Auto).await;
    text(&client, &session, &first).await;
    let (second, _) = start(&client, &session, BitmapSubtitles::Auto).await;
    let asked = Instant::now();
    assert_eq!(text(&client, &session, &second).await, SRT);
    assert!(
        asked.elapsed() < Duration::from_secs(1),
        "{:?}",
        asked.elapsed()
    );
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn text_is_read_even_for_a_client_that_draws_pgs() {
    let (server, client, session) = ready("ocr-asked", true, true).await;
    let (handle, plan) = start(&client, &session, BitmapSubtitles::Text).await;
    assert!(
        matches!(&plan, PlaybackPlan::Direct { subtitle, .. } if *subtitle == read_as_srt()),
        "{plan:?}"
    );
    assert_eq!(text(&client, &session, &handle).await, SRT);
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn text_without_tesseract_cannot_play_and_says_why() {
    let (server, client, session) = ready("ocr-none", false, true).await;
    let response = client
        .call(
            Some(&session),
            Request::StartPlayback {
                id: movie().id().clone(),
                selection: with_pgs(),
                start_ms: None,
                bitmap: BitmapSubtitles::Text,
            },
        )
        .await;
    let Response::Err(Error::CannotPlay { reasons }) = response else {
        panic!("played: {response:?}");
    };
    assert_eq!(
        reasons,
        ["subtitle 0: pgs cannot be read as text: the server has no text recognition (tesseract)"]
    );
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_text_track_the_client_cannot_show_arrives_converted_beside_the_file() {
    let server = TestServer::start_with(
        fresh_dir("convert-ass").await,
        ServerOptions {
            real_media: true,
            ..ServerOptions::default()
        },
    )
    .await;
    server.generate_media(10, "libx264", "aac");
    server.add_text_track(SRT, "ass");
    server.add_user("alice", "correct horse", None).await;
    let client = server.client().await;
    // SubRip, not ASS.
    client.hello_with(None, Some(capabilities(false))).await;
    let session = client.login("alice", "correct horse").await;
    let (handle, plan) = start(&client, &session, BitmapSubtitles::Auto).await;
    let PlaybackPlan::Direct { subtitle, .. } = plan else {
        panic!("not direct: {plan:?}");
    };
    assert_eq!(subtitle, read_as_srt());
    // The same cues; ASS styling is gone.
    assert_eq!(text(&client, &session, &handle).await, SRT);
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_subtitle_change_in_direct_play_changes_only_the_text() {
    let server = TestServer::start_with(
        fresh_dir("change-direct").await,
        ServerOptions {
            real_media: true,
            ..ServerOptions::default()
        },
    )
    .await;
    server.generate_media(10, "libx264", "aac");
    server.add_text_track(SRT, "ass");
    server.add_user("alice", "correct horse", None).await;
    let client = server.client().await;
    client.hello_with(None, Some(capabilities(false))).await;
    let session = client.login("alice", "correct horse").await;
    let response = client
        .call(
            Some(&session),
            Request::StartPlayback {
                id: movie().id().clone(),
                selection: Some(Selection {
                    audio: Some(0),
                    subtitle: None,
                }),
                start_ms: None,
                bitmap: BitmapSubtitles::Auto,
            },
        )
        .await;
    let Response::Ok(Reply::PlaybackStarted { handle, size, .. }) = response else {
        panic!("no playback: {response:?}");
    };
    let change = |subtitle: Option<u32>| Request::ChangeSubtitle {
        handle: handle.clone(),
        subtitle,
        bitmap: BitmapSubtitles::Auto,
        position_ms: 3_000,
    };

    let response = client.call(Some(&session), change(Some(0))).await;
    let Response::Ok(Reply::PlaybackChanged {
        plan: PlaybackPlan::Direct { subtitle, .. },
        size: same_size,
        epoch: 0,
    }) = response
    else {
        panic!("not changed in place: {response:?}");
    };
    assert_eq!(subtitle, read_as_srt());
    assert_eq!(same_size, size);
    assert_eq!(text(&client, &session, &handle).await, SRT);

    // A track that does not exist: refused, and the playback is as it was.
    let response = client.call(Some(&session), change(Some(7))).await;
    assert!(
        matches!(response, Response::Err(Error::CannotPlay { .. })),
        "{response:?}"
    );
    assert_eq!(text(&client, &session, &handle).await, SRT);

    // Subtitles off.
    let response = client.call(Some(&session), change(None)).await;
    assert!(
        matches!(response, Response::Ok(Reply::PlaybackChanged { .. })),
        "{response:?}"
    );
    let response = client
        .call(
            Some(&session),
            Request::ReadSubtitle {
                handle: handle.clone(),
            },
        )
        .await;
    assert!(
        matches!(response, Response::Err(Error::NotFound { .. })),
        "{response:?}"
    );
    client.close().await;
    server.stop().await;
}
