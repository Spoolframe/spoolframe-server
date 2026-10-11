//! Frame previews over the client protocol (P3-13): `GetPreviews`, the
//! sheets on the artwork stream, a playback starting them, and a playback
//! never held up by them.

use std::path::Path;
use std::time::{Duration, Instant};

use spoolframe_protocol::{
    ArtworkResponse, ArtworkSize, Error, MediaReadResponse, PlaybackPlan, Previews, Reply, Request,
    Response, SessionId,
};

use crate::support::{
    ServerOptions, TestClient, TestServer, fresh_dir, movie, progressive_capabilities, write_script,
};
use spoolframe_server::encode::Units;
use spoolframe_server::previews::{PreviewSettings, When};

fn on_play() -> PreviewSettings {
    PreviewSettings {
        when: When::OnPlay,
        interval: Duration::from_secs(10),
        cache_limit_bytes: 1 << 30,
        workers: 2,
    }
}

async fn ready(test: &str, options: ServerOptions) -> (TestServer, TestClient, SessionId) {
    let server = TestServer::start_with(
        fresh_dir(test).await,
        ServerOptions {
            real_media: true,
            ..options
        },
    )
    .await;
    server.add_user("alice", "correct horse", None).await;
    let client = server.client().await;
    client
        .hello_with(None, Some(progressive_capabilities()))
        .await;
    let session = client.login("alice", "correct horse").await;
    (server, client, session)
}

/// Previews made on play, real media otherwise as the default.
fn making() -> ServerOptions {
    ServerOptions {
        previews: on_play(),
        ..ServerOptions::default()
    }
}

/// The movie as `secs` of a tiny picture whose grey says the time, as in
/// the server's own previews tests: `16 + (s mod 20) × 10` for the
/// 10-second span `s`, a keyframe every 4 s, in `codec`, without sound.
fn tiny_film(server: &TestServer, secs: u32, codec: &str) {
    tiny_film_at(&server.media, secs, codec);
}

fn tiny_film_at(file: &Path, secs: u32, codec: &str) {
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let status = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!(
            "color=c=black:s=64x36:r=1:d={secs},\
             geq=lum='16+mod(floor(T/10),20)*10':cb=128:cr=128"
        ))
        .args([
            "-c:v", codec, "-g", "4", "-pix_fmt", "yuv420p", "-f", "matroska",
        ])
        .arg(file)
        .status()
        .unwrap();
    assert!(status.success());
}

async fn get_previews(client: &TestClient, session: &SessionId) -> Response {
    client
        .call(
            Some(session),
            Request::GetPreviews {
                id: movie().id().clone(),
            },
        )
        .await
}

async fn made(client: &TestClient, session: &SessionId) -> Previews {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match get_previews(client, session).await {
            Response::Ok(Reply::Previews(previews)) => return previews,
            Response::Err(Error::PreviewsNotReady { .. }) => {}
            other => panic!("{other:?}"),
        }
        assert!(Instant::now() < deadline, "previews not made in time");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn a_player_gets_previews_and_finds_the_frame_for_a_time_on_its_sheet() {
    let (server, client, session) = ready("sheet", making()).await;
    tiny_film(&server, 300, "libx264");
    // Asked for first: being made, not there yet.
    assert!(matches!(
        get_previews(&client, &session).await,
        Response::Err(Error::PreviewsNotReady { .. })
    ));
    let previews = made(&client, &session).await;
    assert_eq!(previews.count, 30);
    assert_eq!(previews.sheets.len(), 1);
    assert_eq!((previews.tile_width, previews.tile_height), (320, 180));

    // 2:40 is tile 16, read off the sheet as a player would.
    let tile = previews.tile(160_000).unwrap();
    let (response, bytes) = client
        .artwork(
            &session,
            previews.sheets[tile.sheet].clone(),
            ArtworkSize::Original,
        )
        .await;
    let ArtworkResponse::Ok(reply) = response else {
        panic!("{response:?}");
    };
    assert_eq!(reply.content_type, "image/jpeg");
    let sheet = image::load_from_memory(&bytes).unwrap().to_rgb8();
    assert_eq!(sheet.dimensions(), (3200, 1800));
    let mut sum = 0u64;
    for y in tile.y + tile.height / 4..tile.y + tile.height * 3 / 4 {
        for x in tile.x + tile.width / 4..tile.x + tile.width * 3 / 4 {
            sum += u64::from(sheet.get_pixel(x, y).0[0]);
        }
    }
    let grey = sum as f64 / f64::from(tile.width / 2 * (tile.height / 2));
    let span = |s: u32| f64::from((s % 20) * 10) * 255.0 / 219.0;
    assert!(
        [15, 16].iter().any(|s| (span(*s) - grey).abs() < 5.0),
        "tile {} is grey {grey:.1}",
        tile.index
    );

    // Sized like any image: the whole sheet, scaled.
    let (_, large) = client
        .artwork(&session, previews.sheets[0].clone(), ArtworkSize::Large)
        .await;
    assert_eq!(image::load_from_memory(&large).unwrap().width(), 1280);
    // A sheet that is not there.
    let (missing, _) = client
        .artwork(
            &session,
            "preview:0123456789abcdef0123456789abcdef/0"
                .parse()
                .unwrap(),
            ArtworkSize::Original,
        )
        .await;
    assert!(matches!(
        missing,
        ArtworkResponse::Err(Error::NotFound { .. })
    ));
    client.close().await;
    server.stop().await;
}

/// PX-09 finding 25 (B-9): a sheet's id is all it took to fetch it, for
/// as long as the cache kept it. Now a sheet is served only while its
/// film still is: here the catalog no longer finds it, as when its
/// library stops being served, and the sheet a client was told of is
/// not found.
#[tokio::test]
async fn a_sheet_is_not_served_once_its_film_is_not() {
    let (server, client, session) = ready("sheet-unserved", making()).await;
    tiny_film(&server, 60, "libx264");
    let previews = made(&client, &session).await;
    let sheet = previews.sheets[0].clone();
    let (served, _) = client
        .artwork(&session, sheet.clone(), ArtworkSize::Original)
        .await;
    assert!(matches!(served, ArtworkResponse::Ok(_)), "{served:?}");
    std::fs::rename(&server.media, server.media.with_extension("gone")).unwrap();
    let (refused, _) = client.artwork(&session, sheet, ArtworkSize::Original).await;
    assert!(
        matches!(refused, ArtworkResponse::Err(Error::NotFound { .. })),
        "{refused:?}"
    );
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_server_that_makes_none_says_so() {
    let (server, client, session) = ready(
        "never",
        ServerOptions {
            previews: PreviewSettings {
                when: When::Never,
                ..on_play()
            },
            ..ServerOptions::default()
        },
    )
    .await;
    tiny_film(&server, 30, "libx264");
    assert_eq!(
        get_previews(&client, &session).await,
        Response::Err(Error::NoPreviews)
    );
    client.close().await;
    server.stop().await;
}

/// The sets in the previews cache.
fn sets(server: &TestServer) -> usize {
    std::fs::read_dir(server.dir.join("cache").join("previews"))
        .unwrap()
        .flatten()
        .filter(|e| e.path().join("previews.json").exists())
        .count()
}

async fn start(client: &TestClient, session: &SessionId) -> Response {
    client
        .call(
            Some(session),
            Request::StartPlayback {
                id: movie().id().clone(),
                selection: None,
                start_ms: None,
                bitmap: Default::default(),
                quality: Default::default(),
                display: None,
            },
        )
        .await
}

#[tokio::test]
async fn starting_a_playback_makes_its_previews() {
    let (server, client, session) = ready("on-play", making()).await;
    tiny_film(&server, 60, "libx264");
    assert!(matches!(
        start(&client, &session).await,
        Response::Ok(Reply::PlaybackStarted { .. })
    ));
    let deadline = Instant::now() + Duration::from_secs(60);
    while sets(&server) == 0 {
        assert!(Instant::now() < deadline, "the playback made no previews");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Made before the player asked.
    assert!(matches!(
        get_previews(&client, &session).await,
        Response::Ok(Reply::Previews(_))
    ));
    client.close().await;
    server.stop().await;
}

/// A slow ffmpeg for previews, so they are still being made when the
/// playback beside them has been read to its end.
fn slow_ffmpeg(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("slow-ffmpeg");
    write_script(&path, "#!/bin/sh\nsleep 0.1\nexec ffmpeg \"$@\"\n");
    path
}

/// Previews of a two-hour film are made while the same film plays,
/// converted, in the server's only conversion slot, with an encode that
/// stalls after 2 s without progress ending as failed. The playback
/// starts at once, is never refused as busy, and is read to its end; the
/// previews are still being made when it is.
#[tokio::test]
async fn previews_being_made_never_hold_up_a_playback() {
    let dir = fresh_dir("no-stall").await;
    std::fs::create_dir_all(&dir).unwrap();
    let options = ServerOptions {
        budget: Units::jobs(1),
        stalled_after: Duration::from_secs(2),
        read_ahead: Duration::from_secs(3600),
        previews_ffmpeg: slow_ffmpeg(&dir),
        ..ServerOptions::default()
    };
    let server = TestServer::start_with(
        dir,
        ServerOptions {
            real_media: true,
            previews: on_play(),
            ..options
        },
    )
    .await;
    server.add_user("alice", "correct horse", None).await;
    let client = server.client().await;
    client
        .hello_with(None, Some(progressive_capabilities()))
        .await;
    let session = client.login("alice", "correct horse").await;
    // MPEG-4 Part 2, which the client cannot play: converted.
    tiny_film(&server, 7200, "mpeg4");

    // The previews start, and get going.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match get_previews(&client, &session).await {
            Response::Err(Error::PreviewsNotReady { done_percent }) if done_percent > 0 => break,
            Response::Err(Error::PreviewsNotReady { .. }) => {}
            other => panic!("{other:?}"),
        }
        assert!(Instant::now() < deadline, "the previews never got going");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let response = tokio::time::timeout(Duration::from_secs(10), start(&client, &session))
        .await
        .expect("the playback waited for the previews");
    let Response::Ok(Reply::PlaybackStarted {
        handle,
        plan: PlaybackPlan::Converted { .. },
        ..
    }) = response
    else {
        panic!("no converted playback: {response:?}");
    };
    let mut read = 0u64;
    let started = Instant::now();
    loop {
        let (response, bytes) = client
            .read_at(&session, &handle, 0, read, spoolframe_protocol::MAX_READ)
            .await;
        let MediaReadResponse::Ok(reply) = response else {
            panic!("the playback failed beside the previews: {response:?}");
        };
        read += bytes.len() as u64;
        if reply.complete && read == reply.size {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the playback was not read to its end"
        );
    }
    assert!(
        matches!(
            get_previews(&client, &session).await,
            Response::Err(Error::PreviewsNotReady { .. })
        ),
        "the previews were not being made beside the playback"
    );
    client.close().await;
    server.stop().await;
}

/// `when = ahead`: the server makes previews of what is in the library
/// with no one playing it or asking (P3-13).
#[tokio::test]
async fn ahead_previews_are_made_for_a_film_nobody_has_played_or_asked_for() {
    let dir = fresh_dir("ahead").await;
    // The film is there before the server first looks: where the test
    // server's catalog has its movie.
    tiny_film_at(&dir.join("media").join("movie.mkv"), 60, "libx264");
    let server = TestServer::start_with(
        dir,
        ServerOptions {
            real_media: true,
            previews: PreviewSettings {
                when: When::Ahead,
                ..on_play()
            },
            ..ServerOptions::default()
        },
    )
    .await;
    let deadline = Instant::now() + Duration::from_secs(60);
    while sets(&server) == 0 {
        assert!(Instant::now() < deadline, "nothing made ahead");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    server.stop().await;
}
