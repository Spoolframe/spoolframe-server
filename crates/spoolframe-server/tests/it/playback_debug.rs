//! Playback debug (P3-15) through the admin API, beside a client endpoint
//! that plays: every playback with its plan, reads and conversion, Stop
//! and End conversion, and "Plan for…" against what `PlaybackPlan` says.

use std::net::SocketAddr;
use std::time::Duration;

use jsonrpsee::ws_client::WsClient;
use spoolframe_admin_api::{
    AdminApiClient, CacheState, EndedWhy, MeApiClient, NewUser, PictureSubtitles, PlanFor,
    PlanKind, PreviewCache, ReadMark, Role, StreamKind, StreamOut, error_code,
};
use spoolframe_protocol::{
    Error, MediaReadResponse, PlaybackHandle, PlaybackPlan, Reply, Request, Response, Selection,
    SessionId,
};
use spoolframe_server::admin::{self, Admin, Origins, SetupCode, TrustedProxies};

use crate::admin::{CODE, admin, anonymous, code, creds, setup, with_token};
use crate::support::{
    ServerOptions, TestClient, TestServer, fresh_dir, movie, progressive_capabilities,
};

/// An admin API beside `server`, on the auth it shares: its root admin,
/// and alice, a consumer, whose password is "battery staple".
async fn admin_beside(server: &TestServer) -> (WsClient, WsClient) {
    let (addr, handle) = admin::serve(
        "127.0.0.1:0".parse().unwrap(),
        Admin {
            playback: Some(server.debug.clone()),
            ..admin(server.auth.clone(), Some(SetupCode::new(CODE)))
        },
        Origins::default(),
        TrustedProxies::default(),
    )
    .await
    .unwrap();
    std::mem::forget(handle);
    clients(addr).await
}

async fn clients(addr: SocketAddr) -> (WsClient, WsClient) {
    let root = anonymous(addr)
        .await
        .setup(setup(CODE, "root", "correct horse"))
        .await
        .unwrap();
    let root = with_token(addr, &root.token).await;
    root.user_add(NewUser {
        username: "alice".into(),
        display_name: Some("Alice".into()),
        password: "battery staple".into(),
        role: Role::Consumer,
    })
    .await
    .unwrap();
    let alice = anonymous(addr)
        .await
        .login(creds("alice", "battery staple"))
        .await
        .unwrap();
    (root, with_token(addr, &alice.token).await)
}

/// A server with an 8 s H.264 + AC-3 file, a device that plays AAC from a
/// progressive stream (so the audio is encoded), and alice playing it.
async fn converting(
    test: &str,
    options: ServerOptions,
) -> (TestServer, WsClient, TestClient, SessionId, PlaybackHandle) {
    let server = TestServer::start_with(
        fresh_dir(test).await,
        ServerOptions {
            real_media: true,
            read_ahead: Duration::from_secs(3600),
            ..options
        },
    )
    .await;
    server.generate_media(8, "libx264", "ac3");
    let (root, _alice) = admin_beside(&server).await;
    let client = server.client().await;
    client
        .hello_with(Some("Den PC"), Some(progressive_capabilities()))
        .await;
    let session = client.login("alice", "battery staple").await;
    let started = client
        .call(
            Some(&session),
            Request::StartPlayback {
                id: movie().id().clone(),
                selection: None,
                start_ms: None,
                bitmap: Default::default(),
            },
        )
        .await;
    let Response::Ok(Reply::PlaybackStarted { handle, .. }) = started else {
        panic!("not started: {started:?}");
    };
    (server, root, client, session, handle)
}

async fn until<F: std::future::Future<Output = bool>>(what: &str, mut done: impl FnMut() -> F) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !done().await {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("not within 20 s: {what}"));
}

/// Reads `length` bytes at `offset` until the encode has written them.
async fn read_some(
    client: &TestClient,
    session: &SessionId,
    handle: &PlaybackHandle,
    offset: u64,
    length: u64,
) -> u64 {
    loop {
        match client.read_at(session, handle, 0, offset, length).await {
            (MediaReadResponse::Ok(reply), _) if reply.sent > 0 => return reply.sent,
            (MediaReadResponse::Ok(_), _) => {}
            (other, _) => panic!("read failed: {other:?}"),
        }
    }
}

#[tokio::test]
async fn a_consumer_is_forbidden_all_of_it() {
    let server = TestServer::start("forbidden").await;
    let (_root, alice) = admin_beside(&server).await;
    let id = movie().id().to_string();
    let request = PlanFor {
        device: "profile:linux".into(),
        audio: None,
        subtitle: None,
        pictures: PictureSubtitles::Auto,
    };
    assert_eq!(
        code(alice.playbacks().await.unwrap_err()),
        error_code::FORBIDDEN
    );
    assert_eq!(
        code(alice.playback("x".into()).await.unwrap_err()),
        error_code::FORBIDDEN
    );
    assert_eq!(
        code(alice.playback_stop("x".into()).await.unwrap_err()),
        error_code::FORBIDDEN
    );
    assert_eq!(
        code(alice.playback_end_conversion("x".into()).await.unwrap_err()),
        error_code::FORBIDDEN
    );
    assert_eq!(
        code(alice.catalog_debug(id.clone()).await.unwrap_err()),
        error_code::FORBIDDEN
    );
    assert_eq!(
        code(alice.catalog_plan_for(id, request).await.unwrap_err()),
        error_code::FORBIDDEN
    );
    server.stop().await;
}

/// The ticket's first check: a converted playback appears with its plan,
/// its reasons, its epoch and its read position, which moves as the client
/// reads; its page shows the command line with no token in it.
#[tokio::test]
async fn a_converted_playback_is_listed_with_its_plan_and_reads() {
    let (server, root, client, session, handle) =
        converting("listed", ServerOptions::default()).await;
    let list = root.playbacks().await.unwrap();
    assert_eq!(list.open.len(), 1);
    let row = &list.open[0];
    assert_eq!(row.username, "alice");
    assert_eq!(row.display_name, "Alice");
    assert_eq!(row.device, "Den PC");
    assert_eq!(row.plan, PlanKind::Transcode);
    assert_eq!(row.line.delivery, "progressive");
    assert!(
        row.reasons
            .iter()
            .any(|r| r == "audio 0: ac3 not supported"),
        "{:?}",
        row.reasons
    );
    assert_eq!(row.epoch, 0);
    assert_ne!(row.id, handle.0);
    assert!(row.read_to_ms.is_none(), "nothing read yet");

    let first = read_some(&client, &session, &handle, 0, 4096).await;
    let row = root.playbacks().await.unwrap().open.remove(0);
    assert_eq!(row.furthest, Some(ReadMark::Byte { offset: first }));
    assert!(row.last_read_at.is_some());
    let at_first = row.read_to_ms;
    // Further in: the furthest read and its place in the film move on.
    until("the encode writes more", || async {
        matches!(
            client.read_at(&session, &handle, 0, 50_000, 4096).await,
            (MediaReadResponse::Ok(r), _) if r.sent > 0
        )
    })
    .await;
    let row = root.playbacks().await.unwrap().open.remove(0);
    assert!(
        matches!(row.furthest, Some(ReadMark::Byte { offset }) if offset > 50_000),
        "{:?}",
        row.furthest
    );
    assert!(
        row.read_to_ms >= at_first,
        "{:?} then {at_first:?}",
        row.read_to_ms
    );

    let detail = root.playback(row.id.clone()).await.unwrap();
    let command = detail.command.clone().expect("a command line");
    assert!(command.starts_with("ffmpeg "), "{command}");
    assert!(command.contains("<hidden>"), "{command}");
    let whole = serde_json::to_string(&detail).unwrap();
    assert!(!whole.contains(&handle.0), "the handle is in {whole}");
    assert!(!detail.reads.is_empty());
    assert!(
        detail
            .plan
            .streams
            .iter()
            .any(|s| s.stream == StreamKind::Audio
                && matches!(&s.out, StreamOut::Encode { codec, .. } if codec == "aac")),
        "{:?}",
        detail.plan.streams
    );
    client.close().await;
    server.stop().await;
}

/// "Stop" ends the playback, and the client sees it end: told it was
/// stopped, not that it expired, which it would start again.
#[tokio::test]
async fn stop_ends_the_playback_and_the_client_is_told() {
    let (server, root, client, session, handle) =
        converting("stop", ServerOptions::default()).await;
    read_some(&client, &session, &handle, 0, 1024).await;
    let id = root.playbacks().await.unwrap().open[0].id.clone();
    root.playback_stop(id.clone()).await.unwrap();
    assert_eq!(
        client.read_at(&session, &handle, 0, 0, 1024).await.0,
        MediaReadResponse::Err(Error::PlaybackStopped)
    );
    let list = root.playbacks().await.unwrap();
    assert!(list.open.is_empty());
    assert_eq!(list.ended[0].why, EndedWhy::Stopped);
    // Its page stays for the hour.
    assert!(root.playback(id.clone()).await.unwrap().ended.is_some());
    assert_eq!(
        code(root.playback_stop(id).await.unwrap_err()),
        error_code::NO_SUCH_PLAYBACK
    );
    client.close().await;
    server.stop().await;
}

/// The playback page says what each conversion weighs against the encode
/// budget: the generated media's encodes only its audio, a quarter.
#[tokio::test]
async fn a_conversion_s_weight_is_shown() {
    let (server, root, client, _session, _handle) =
        converting("weight", ServerOptions::default()).await;
    let id = root.playbacks().await.unwrap().open[0].id.clone();
    let conversion = root
        .playback(id)
        .await
        .unwrap()
        .playback
        .conversion
        .unwrap();
    assert_eq!(
        (conversion.load, conversion.weight_quarters),
        (spoolframe_admin_api::ConversionLoad::Audio, 1),
        "{conversion:?}"
    );
    client.close().await;
    server.stop().await;
}

/// "End conversion" stops ffmpeg only: what it wrote is still read, then
/// the end; a seek does not start it again.
#[tokio::test]
async fn end_conversion_stops_ffmpeg_and_the_playback_reads_what_was_written() {
    let (server, root, client, session, handle) =
        converting("end-conversion", ServerOptions::default()).await;
    read_some(&client, &session, &handle, 0, 1024).await;
    let id = root.playbacks().await.unwrap().open[0].id.clone();
    root.playback_end_conversion(id.clone()).await.unwrap();
    let detail = root.playback(id.clone()).await.unwrap();
    let conversion = detail.playback.conversion.unwrap();
    assert!(conversion.ended && !conversion.running, "{conversion:?}");
    let written = conversion.bytes;
    // Still open, and what was written is read to its end.
    assert!(root.playback(id).await.unwrap().ended.is_none());
    let (at_start, _) = client.read_at(&session, &handle, 0, 0, 1024).await;
    assert!(matches!(at_start, MediaReadResponse::Ok(r) if r.sent > 0 && r.complete));
    let (past, _) = client
        .read_at(&session, &handle, 0, written + 10, 1024)
        .await;
    assert!(
        matches!(&past, MediaReadResponse::Ok(r) if r.sent == 0 && r.complete),
        "{past:?}"
    );
    let seek = client
        .call(
            Some(&session),
            Request::SeekPlayback {
                handle: handle.clone(),
                position_ms: 4000,
            },
        )
        .await;
    assert_eq!(
        seek,
        Response::Err(Error::PlaybackFailed {
            reason: spoolframe_server::encode::ENDED.into()
        })
    );
    client.close().await;
    server.stop().await;
}

/// A failed conversion's page shows ffmpeg's last stderr lines, and keeps
/// them once its client has let it go.
#[tokio::test]
async fn a_failed_conversion_shows_ffmpeg_s_last_words() {
    let dir = fresh_dir("failed").await;
    std::fs::create_dir_all(&dir).unwrap();
    let stub = dir.join("failing-ffmpeg");
    crate::support::write_script(
        &stub,
        "#!/bin/sh\necho '[h264_vaapi @ 0x5612] No usable encoding profile found.' >&2\n\
         echo 'Error initializing output stream 0:0' >&2\nexit 1\n",
    );
    let (server, root, client, session, handle) = converting(
        "failed",
        ServerOptions {
            ffmpeg: stub,
            ..ServerOptions::default()
        },
    )
    .await;
    let (read, _) = client.read_at(&session, &handle, 0, 0, 100).await;
    assert!(matches!(
        read,
        MediaReadResponse::Err(Error::PlaybackFailed { .. })
    ));
    let id = root.playbacks().await.unwrap().open[0].id.clone();
    let detail = root.playback(id.clone()).await.unwrap();
    assert_eq!(
        detail.stderr,
        [
            "[h264_vaapi @ 0x5612] No usable encoding profile found.",
            "Error initializing output stream 0:0"
        ]
    );
    assert_eq!(
        detail.playback.conversion.unwrap().failed.as_deref(),
        Some("Error initializing output stream 0:0")
    );
    // The client lets it go, as a player does after a failure.
    client
        .call(
            Some(&session),
            Request::StopPlayback {
                handle: handle.clone(),
            },
        )
        .await;
    let ended = root.playback(id).await.unwrap();
    assert_eq!(
        ended.ended.unwrap().why,
        EndedWhy::Failed {
            reason: "Error initializing output stream 0:0".into()
        }
    );
    assert_eq!(ended.stderr.len(), 2);
    client.close().await;
    server.stop().await;
}

/// The ticket's check: "Plan for" with an Apple TV-like profile on an
/// HEVC/DTS/PGS file gives the plan `PlaybackPlan` returns for a device
/// that sends that profile; so does a connected device's own.
#[tokio::test]
async fn plan_for_gives_what_playback_plan_gives_that_device() {
    let server = TestServer::start_with(
        fresh_dir("plan-for").await,
        ServerOptions {
            ffprobe_fixture: "mkv-hevc-pgs.json",
            ..ServerOptions::default()
        },
    )
    .await;
    server.write_media(1000);
    let (root, _alice) = admin_beside(&server).await;
    let apple_tv = spoolframe_server::playback_debug::apple_tv();
    let client = server.client().await;
    client.hello_with(Some("Living room"), Some(apple_tv)).await;
    let session = client.login("alice", "battery staple").await;
    let id = movie().id().to_string();
    // DTS-HD MA (audio 1) and the first PGS track (subtitle 2).
    let selection = Selection {
        audio: Some(1),
        subtitle: Some(2),
    };
    for pictures in [
        PictureSubtitles::Auto,
        PictureSubtitles::Text,
        PictureSubtitles::BurnIn,
    ] {
        let bitmap = match pictures {
            PictureSubtitles::Auto => spoolframe_protocol::BitmapSubtitles::Auto,
            PictureSubtitles::Text => spoolframe_protocol::BitmapSubtitles::Text,
            PictureSubtitles::BurnIn => spoolframe_protocol::BitmapSubtitles::BurnIn,
        };
        let asked = client
            .call(
                Some(&session),
                Request::PlaybackPlan {
                    id: movie().id().clone(),
                    selection: Some(selection),
                    bitmap,
                },
            )
            .await;
        let Response::Ok(Reply::PlaybackPlan { plan: expected, .. }) = asked else {
            panic!("no plan: {asked:?}");
        };
        let devices = root.catalog_debug(id.clone()).await.unwrap().devices;
        let connected = devices
            .iter()
            .find(|d| d.connected && d.name == "Living room")
            .expect("the connected device is offered");
        for device in ["profile:apple-tv".to_owned(), connected.id.clone()] {
            let view = root
                .catalog_plan_for(
                    id.clone(),
                    PlanFor {
                        device: device.clone(),
                        audio: Some(1),
                        subtitle: Some(2),
                        pictures,
                    },
                )
                .await
                .unwrap();
            let got: PlaybackPlan = serde_json::from_value(view.plan.clone()).unwrap();
            assert_eq!(got, expected, "{device} {pictures:?}");
        }
    }
    // Auto: DTS encoded, PGS burned in, as the plan's reasons say.
    let view = root
        .catalog_plan_for(
            id.clone(),
            PlanFor {
                device: "profile:apple-tv".into(),
                audio: Some(1),
                subtitle: Some(2),
                pictures: PictureSubtitles::Auto,
            },
        )
        .await
        .unwrap();
    assert_eq!(view.kind, PlanKind::Transcode);
    let audio = view
        .streams
        .iter()
        .find(|s| s.stream == StreamKind::Audio)
        .unwrap();
    assert_eq!(audio.index, Some(1));
    assert_eq!(audio.reasons, ["audio 1: dts not supported"]);
    let subtitle = view
        .streams
        .iter()
        .find(|s| s.stream == StreamKind::Subtitle)
        .unwrap();
    assert!(
        matches!(subtitle.out, StreamOut::BurnIn { .. }),
        "{subtitle:?}"
    );
    client.close().await;
    server.stop().await;
}

/// The Debug tab's probe and caches, looked at without starting anything.
#[tokio::test]
async fn an_item_s_debug_shows_its_probe_and_its_caches() {
    let server = TestServer::start_with(
        fresh_dir("item").await,
        ServerOptions {
            ffprobe_fixture: "mkv-hevc-pgs.json",
            ..ServerOptions::default()
        },
    )
    .await;
    server.write_media(1000);
    let (root, _alice) = admin_beside(&server).await;
    let debug = root.catalog_debug(movie().id().to_string()).await.unwrap();
    let probe = debug.probe.unwrap();
    assert_eq!(probe.container, "matroska,webm");
    assert_eq!(probe.size, Some(1000));
    assert_eq!(probe.streams.len(), 15);
    let video = &probe.streams[0];
    assert_eq!(
        (video.kind, video.codec.as_str()),
        (StreamKind::Video, "hevc")
    );
    // Picture tracks, with no text recognition on this server.
    let pgs: Vec<_> = debug.subtitles.iter().filter(|s| s.ocr).collect();
    assert_eq!(pgs.len(), 6);
    assert!(
        matches!(pgs[0].state, CacheState::Unavailable { .. }),
        "{:?}",
        pgs[0]
    );
    assert!(
        matches!(debug.previews, PreviewCache::None { .. }),
        "{:?}",
        debug.previews
    );
    assert!(debug.devices.iter().any(|d| d.id == "profile:apple-tv"));
    server.stop().await;
}
