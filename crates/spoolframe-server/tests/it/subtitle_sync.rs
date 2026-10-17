//! Subtitle sync and offset (PX-03) over a real Iroh connection, with real
//! ffmpeg and ffprobe on a film with a subtitle file beside it, and the
//! speech its sync finds from a stand-in: that the requests reach the
//! timings, what one user sets is what another plays, a player reading
//! the track is told and reads it again without a restart, and a client
//! without the feature is sent none of it. The syncing itself is tested
//! in `subtitle_sync::tests` and spoolframe-media's `subsync`.

use crate::support;

use std::time::Duration;

use iroh::endpoint::RecvStream;
use spoolframe_media::subsync::{self, Span, TextFormat};
use spoolframe_protocol::codec::read_frame;
use spoolframe_protocol::features::SUBTITLE_SYNC;
use spoolframe_protocol::streams::uni;
use spoolframe_protocol::{
    BitmapSubtitles, Error, Event, Hello, PROTOCOL_VERSION, PlaybackHandle, Reply, Request,
    Response, Selection, SessionId,
};
use spoolframe_types::{
    AudioCodec, ClientCapabilities, Container, MediaTracks, SubtitleFormat, SubtitleSync,
    SubtitleTiming, VideoCodec,
};
use support::{ServerOptions, TestClient, TestServer, fresh_dir, movie};
use tokio::io::AsyncReadExt;

/// Forty lines, where the film's speech has them.
fn cues() -> Vec<Span> {
    (0..40)
        .map(|i| Span {
            start_ms: 2_000 + i * 3_100 + (i % 3) * 400,
            end_ms: 3_200 + i * 3_100 + (i % 3) * 400 + (i % 4) * 300,
        })
        .collect()
}

fn srt(cues: &[Span], shift_ms: i64) -> String {
    let time = |ms: i64| {
        format!(
            "00:{:02}:{:02},{:03}",
            ms / 60_000 % 60,
            ms / 1000 % 60,
            ms % 1000
        )
    };
    cues.iter()
        .enumerate()
        .map(|(i, c)| {
            format!(
                "{}\n{} --> {}\nLine {i}\n\n",
                i + 1,
                time(c.start_ms + shift_ms),
                time(c.end_ms + shift_ms)
            )
        })
        .collect()
}

async fn ready(test: &str) -> (TestServer, String) {
    let server = TestServer::start_with(
        fresh_dir(test).await,
        ServerOptions {
            real_media: true,
            speech: Some(cues()),
            ..ServerOptions::default()
        },
    )
    .await;
    server.generate_media(10, "libx264", "aac");
    // The subtitle beside the film, 3.2 s late.
    let text = srt(&cues(), 3_200);
    std::fs::write(server.media.with_extension("en.srt"), &text).unwrap();
    server
        .add_user("alice", "correct horse", Some("Alice"))
        .await;
    server.add_user("bob", "correct horse", Some("Bob")).await;
    (server, text)
}

/// A Linux app that plays the film directly with SubRip beside it,
/// listing `features`, signed in as `user`.
async fn app(server: &TestServer, user: &str, features: &[&str]) -> (TestClient, SessionId) {
    app_showing(server, user, features, SubtitleFormat::Srt).await
}

/// An app that shows subtitles in `format` only.
async fn app_showing(
    server: &TestServer,
    user: &str,
    features: &[&str],
    format: SubtitleFormat,
) -> (TestClient, SessionId) {
    let client = server.client().await;
    let hello = Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "test".into(),
        client_version: "0".into(),
        platform: spoolframe_types::Platform::Linux,
        device_name: None,
        capabilities: Some(ClientCapabilities {
            video: [VideoCodec::H264].into(),
            audio: [AudioCodec::Aac].into(),
            containers: [Container::Matroska].into(),
            subtitles: [format].into(),
            ..ClientCapabilities::default()
        }),
        experimental: Vec::new(),
        features: features.iter().map(|f| f.to_string()).collect(),
    };
    assert!(matches!(
        client.call(None, Request::Hello(hello)).await,
        Response::Ok(Reply::ServerInfo(_))
    ));
    let session = client.login(user, "correct horse").await;
    (client, session)
}

async fn tracks(client: &TestClient, session: &SessionId) -> MediaTracks {
    match client
        .call(
            Some(session),
            Request::Tracks {
                id: movie().id().clone(),
            },
        )
        .await
    {
        Response::Ok(Reply::Tracks(tracks)) => tracks,
        other => panic!("no tracks: {other:?}"),
    }
}

async fn timing_call(client: &TestClient, session: &SessionId, request: Request) -> SubtitleTiming {
    match client.call(Some(session), request).await {
        Response::Ok(Reply::SubtitleTiming(timing)) => timing,
        other => panic!("no timing: {other:?}"),
    }
}

async fn events(client: &TestClient) -> RecvStream {
    let mut events = tokio::time::timeout(Duration::from_secs(5), client.conn.accept_uni())
        .await
        .expect("no events stream within 5 s")
        .unwrap();
    assert_eq!(events.read_u8().await.unwrap(), uni::EVENTS);
    events
}

/// The next subtitle timing event on `events`.
async fn next_timing(events: &mut RecvStream) -> (u32, SubtitleTiming) {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(30), read_frame::<_, Event>(events))
            .await
            .expect("no event in time")
            .unwrap();
        if let Event::SubtitleTimingChanged { id, track, timing } = event {
            assert_eq!(&id, movie().id());
            return (track, timing);
        }
    }
}

/// Play the film with its subtitle file.
async fn play(client: &TestClient, session: &SessionId) -> PlaybackHandle {
    match client
        .call(
            Some(session),
            Request::StartPlayback {
                id: movie().id().clone(),
                selection: Some(Selection {
                    audio: Some(0),
                    subtitle: Some(0),
                }),
                start_ms: None,
                bitmap: BitmapSubtitles::Auto,
                quality: Default::default(),
                display: None,
            },
        )
        .await
    {
        Response::Ok(Reply::PlaybackStarted { handle, .. }) => handle,
        other => panic!("playback did not start: {other:?}"),
    }
}

async fn read(
    client: &TestClient,
    session: &SessionId,
    handle: &PlaybackHandle,
    version: Option<u64>,
) -> Result<(String, u64), Error> {
    match client
        .call(
            Some(session),
            Request::ReadSubtitle {
                handle: handle.clone(),
                offset: 0,
                version,
            },
        )
        .await
    {
        Response::Ok(Reply::SubtitleChunk { bytes, version, .. }) => {
            Ok((String::from_utf8(bytes).unwrap(), version))
        }
        Response::Err(e) => Err(e),
        other => panic!("{other:?}"),
    }
}

fn starts(text: &str) -> Vec<i64> {
    subsync::cues(text.as_bytes(), TextFormat::Srt)
        .unwrap()
        .iter()
        .map(|c| c.start_ms)
        .collect()
}

/// The ticket's offset: +1.5 s set by Bob is what Alice gets, as she
/// plays, without a restart; Reset brings it back.
#[tokio::test]
async fn an_offset_one_user_sets_is_what_another_plays() {
    let (server, text) = ready("subsync-offset").await;
    let (alice, alice_session) = app(&server, "alice", &[SUBTITLE_SYNC]).await;
    let mut told = events(&alice).await;
    let (bob, bob_session) = app(&server, "bob", &[SUBTITLE_SYNC]).await;
    let before = tracks(&alice, &alice_session).await;
    let timing = before.subtitles[0]
        .timing
        .clone()
        .expect("a sidecar's timing");
    assert_eq!((timing.sync, timing.offset_ms), (SubtitleSync::Original, 0));
    let handle = play(&alice, &alice_session).await;
    let (first, version) = read(&alice, &alice_session, &handle, None).await.unwrap();
    assert_eq!(first, text);

    let id = movie().id().clone();
    let set = timing_call(
        &bob,
        &bob_session,
        Request::SetSubtitleOffset {
            id: id.clone(),
            track: 0,
            offset_ms: 1_500,
        },
    )
    .await;
    assert_eq!(set.offset_by.as_deref(), Some("Bob"));
    // Alice's player is told, and the file it was reading is gone.
    let (track, timing) = next_timing(&mut told).await;
    assert_eq!((track, timing.offset_ms), (0, 1_500));
    assert_eq!(
        read(&alice, &alice_session, &handle, Some(version)).await,
        Err(Error::SubtitleChanged)
    );
    let (moved, _) = read(&alice, &alice_session, &handle, None).await.unwrap();
    let expected: Vec<i64> = starts(&text).iter().map(|s| s + 1_500).collect();
    assert_eq!(starts(&moved), expected);
    let shown = tracks(&alice, &alice_session).await.subtitles[0]
        .timing
        .clone()
        .unwrap();
    assert_eq!(
        (shown.offset_ms, shown.offset_by.as_deref()),
        (1_500, Some("Bob"))
    );

    timing_call(
        &bob,
        &bob_session,
        Request::SetSubtitleOffset {
            id,
            track: 0,
            offset_ms: 0,
        },
    )
    .await;
    let (back, _) = read(&alice, &alice_session, &handle, None).await.unwrap();
    assert_eq!(back, text);
}

/// A sync runs in the background: the track reads as it was until it is
/// done, then synced, and a player showing it is told.
#[tokio::test]
async fn a_synced_subtitle_reads_synced_after_its_sync() {
    let (server, text) = ready("subsync-sync").await;
    let (alice, session) = app(&server, "alice", &[SUBTITLE_SYNC]).await;
    let mut told = events(&alice).await;
    let handle = play(&alice, &session).await;
    let syncing = timing_call(
        &alice,
        &session,
        Request::SyncSubtitle {
            id: movie().id().clone(),
            track: 0,
        },
    )
    .await;
    assert_eq!(syncing.sync, SubtitleSync::Syncing);
    let done = loop {
        let (_, timing) = next_timing(&mut told).await;
        if timing.sync != SubtitleSync::Syncing {
            break timing;
        }
    };
    assert!(matches!(done.sync, SubtitleSync::Synced { .. }), "{done:?}");
    let (synced, _) = read(&alice, &session, &handle, None).await.unwrap();
    for (got, want) in starts(&synced).iter().zip(cues()) {
        assert!((got - want.start_ms).abs() <= 200, "{got} for {want:?}");
    }
    // The file beside the film is as it was.
    assert_eq!(
        std::fs::read_to_string(server.media.with_extension("en.srt")).unwrap(),
        text
    );
}

/// A subtitle converted for a client (SubRip to WebVTT) is converted from
/// its timed file, which ffmpeg reads through the server's own hidden
/// directory.
#[tokio::test]
async fn a_converted_subtitle_has_its_timing() {
    let (server, text) = ready("subsync-converted").await;
    let (bob, bob_session) = app(&server, "bob", &[SUBTITLE_SYNC]).await;
    timing_call(
        &bob,
        &bob_session,
        Request::SetSubtitleOffset {
            id: movie().id().clone(),
            track: 0,
            offset_ms: -1_000,
        },
    )
    .await;
    let (web, session) = app_showing(&server, "alice", &[], SubtitleFormat::WebVtt).await;
    let handle = play(&web, &session).await;
    let vtt = loop {
        match read(&web, &session, &handle, None).await {
            Ok((vtt, _)) => break vtt,
            Err(Error::SubtitleNotReady { .. }) => {
                tokio::time::sleep(Duration::from_millis(100)).await
            }
            Err(e) => panic!("{e}"),
        }
    };
    let got: Vec<i64> = subsync::cues(vtt.as_bytes(), TextFormat::WebVtt)
        .unwrap()
        .iter()
        .map(|c| c.start_ms)
        .collect();
    let expected: Vec<i64> = starts(&text).iter().map(|s| s - 1_000).collect();
    assert_eq!(got, expected);
}

/// A client that did not list the feature is sent no timing, and its
/// requests are refused.
#[tokio::test]
async fn a_client_without_the_feature_gets_no_timing() {
    let (server, _) = ready("subsync-unlisted").await;
    let (older, session) = app(&server, "alice", &[]).await;
    assert_eq!(tracks(&older, &session).await.subtitles[0].timing, None);
    let refused = older
        .call(
            Some(&session),
            Request::SetSubtitleOffset {
                id: movie().id().clone(),
                track: 0,
                offset_ms: 1_000,
            },
        )
        .await;
    assert!(
        matches!(refused, Response::Err(Error::Unsupported { .. })),
        "{refused:?}"
    );
}
