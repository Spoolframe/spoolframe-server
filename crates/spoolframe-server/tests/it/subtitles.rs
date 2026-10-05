//! Bitmap subtitles read as text (P2-07, P3-08), through the protocol,
//! with real ffmpeg, ffprobe and tesseract on a file with a PGS, VobSub or
//! DVB track.

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
                    offset: 0,
                    version: None,
                },
            )
            .await;
        match response {
            Response::Ok(Reply::SubtitleChunk { format, bytes, .. }) => {
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

/// The first chunk of a subtitle that is still being read answers as the
/// whole read does (P2-11). The stand-in tesseract never finishes a cue.
#[tokio::test]
async fn the_first_chunk_of_a_subtitle_still_being_read_is_not_ready() {
    let dir = fresh_dir("ocr-chunk-not-ready").await;
    std::fs::create_dir_all(&dir).unwrap();
    let tesseract = dir.join("endless-tesseract");
    support::write_script(
        &tesseract,
        "#!/bin/sh\n\
         if [ \"$1\" = --list-langs ]; then\n\
         printf 'List of available languages (1):\\neng\\n'\n\
         exit 0\n\
         fi\n\
         exec sleep 600\n",
    );
    let server = TestServer::start_with(
        dir,
        ServerOptions {
            real_media: true,
            ocr: true,
            tesseract,
            ..ServerOptions::default()
        },
    )
    .await;
    server.generate_media(10, "libx264", "aac");
    server.add_pgs(&[("Hello there", 1_000, 3_500)]);
    server.add_user("alice", "correct horse", None).await;
    let client = server.client().await;
    client.hello_with(None, Some(capabilities(false))).await;
    let session = client.login("alice", "correct horse").await;
    let (handle, _) = start(&client, &session, BitmapSubtitles::Auto).await;

    let response = client
        .call(
            Some(&session),
            Request::ReadSubtitle {
                handle,
                offset: 0,
                version: None,
            },
        )
        .await;
    assert!(
        matches!(response, Response::Err(Error::SubtitleNotReady { .. })),
        "{response:?}"
    );
    // Waiting, or being read, counts in the queue (P3-14).
    assert_eq!(ocr(&server, "spoolframe_ocr_queued", &[]), 1.0);
    client.close().await;
    server.stop().await;
}

/// A number from the server's metrics (P3-14).
fn ocr(server: &TestServer, name: &str, labels: &[(&str, &str)]) -> f64 {
    let families = server.metrics.families();
    families
        .iter()
        .find(|f| f.name == name)
        .and_then(|f| f.value(labels))
        .unwrap_or_else(|| panic!("no {name}{labels:?}"))
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
    assert_eq!(
        (
            ocr(&server, "spoolframe_ocr_queued", &[]),
            ocr(&server, "spoolframe_ocr_total", &[("outcome", "done")]),
            ocr(&server, "spoolframe_subtitle_reads_total", &[]) > 0.0,
        ),
        (0.0, 1.0, true)
    );
    client.close().await;
    server.stop().await;
}

/// What the VobSub and DVB tracks show. VobSub's stop is in 1024/90000 s,
/// so the lengths are multiples of 512 ms.
const BITMAP_CUES: [(&str, u64, u64); 2] = [
    ("Hello there", 1_000, 3_560),
    ("General Kenobi", 5_000, 7_048),
];

const BITMAP_SRT: &str = "1\n00:00:01,000 --> 00:00:03,560\nHello there\n\n\
                          2\n00:00:05,000 --> 00:00:07,048\nGeneral Kenobi\n\n";

/// A server whose film has an English track of `BITMAP_CUES` written by
/// ffmpeg's `codec`, or beside it as a VobSub pair if `codec` is `None`;
/// and alice on a client that draws neither VobSub nor DVB.
async fn bitmap_ready(test: &str, codec: Option<&str>) -> (TestServer, TestClient, SessionId) {
    let server = TestServer::start_with(
        fresh_dir(test).await,
        ServerOptions {
            real_media: true,
            ocr: true,
            ..ServerOptions::default()
        },
    )
    .await;
    server.generate_media(10, "libx264", "aac");
    match codec {
        Some(codec) => server.add_bitmap_track(codec, &BITMAP_CUES),
        None => server.write_vobsub_sidecar(&BITMAP_CUES),
    }
    server.add_user("alice", "correct horse", None).await;
    let client = server.client().await;
    client.hello_with(None, Some(capabilities(false))).await;
    let session = client.login("alice", "correct horse").await;
    (server, client, session)
}

/// `auto` and `text` alike read VobSub and DVB as text for a client that
/// draws neither, and the file plays directly with it beside it (P3-08).
#[tokio::test]
async fn vobsub_and_dvb_tracks_are_read_as_text_and_the_file_still_plays_directly() {
    for (test, codec, bitmap) in [
        ("ocr-vobsub", Some("dvdsub"), BitmapSubtitles::Auto),
        ("ocr-vobsub-pair", None, BitmapSubtitles::Auto),
        ("ocr-dvb", Some("dvbsub"), BitmapSubtitles::Text),
    ] {
        let (server, client, session) = bitmap_ready(test, codec).await;
        let (handle, plan) = start(&client, &session, bitmap).await;
        let PlaybackPlan::Direct { subtitle, .. } = plan else {
            panic!("{test}: not direct: {plan:?}");
        };
        assert_eq!(subtitle, read_as_srt(), "{test}");
        assert_eq!(text(&client, &session, &handle).await, BITMAP_SRT, "{test}");
        client.close().await;
        server.stop().await;
    }
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

/// The text made of a subtitle file beside the film is of the file as it
/// is now: one edited in place is converted again.
#[tokio::test]
async fn a_subtitle_file_edited_in_place_is_converted_again() {
    const FIXED: &str = "1\n00:00:02,000 --> 00:00:04,500\nHello there, again\n\n";
    let server = TestServer::start_with(
        fresh_dir("convert-edited").await,
        ServerOptions {
            real_media: true,
            ..ServerOptions::default()
        },
    )
    .await;
    server.generate_media(10, "libx264", "aac");
    server.write_ass_sidecar(SRT);
    server.add_user("alice", "correct horse", None).await;
    let client = server.client().await;
    // SubRip, not ASS.
    client.hello_with(None, Some(capabilities(false))).await;
    let session = client.login("alice", "correct horse").await;
    let (first, plan) = start(&client, &session, BitmapSubtitles::Auto).await;
    assert!(
        matches!(&plan, PlaybackPlan::Direct { subtitle, .. } if *subtitle == read_as_srt()),
        "{plan:?}"
    );
    assert_eq!(text(&client, &session, &first).await, SRT);

    server.write_ass_sidecar(FIXED);
    let (second, _) = start(&client, &session, BitmapSubtitles::Auto).await;
    assert_eq!(text(&client, &session, &second).await, FIXED);
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
                offset: 0,
                version: None,
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
