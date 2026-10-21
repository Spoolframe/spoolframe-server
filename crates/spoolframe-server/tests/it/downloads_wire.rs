//! Downloads over the wire (PX-38, `spoolframe:downloads`): the requests,
//! the `DownloadChanged` event and the `Download` resource read, against
//! a server with real ffmpeg and ffprobe, on films made at test time.

use std::path::Path;
use std::time::{Duration, Instant};

use iroh::endpoint::RecvStream;
use spoolframe_protocol::codec::read_frame;
use spoolframe_protocol::downloads::{
    DownloadInfo, DownloadOptionsInfo, DownloadSource, DownloadState, DownloadTracks,
    OriginalDownload,
};
use spoolframe_protocol::features::DOWNLOADS;
use spoolframe_protocol::resource::{ResourceMetadata, ResourceRequest, ResourceResponse};
use spoolframe_protocol::streams::uni;
use spoolframe_protocol::{
    Error, Event, Hello, PROTOCOL_VERSION, Reply, Request, Response, SessionId,
};
use spoolframe_types::{Container, Platform, TrackPreferences};
use tokio::io::AsyncReadExt;

use crate::downloads::write_film;
use crate::support::{ServerOptions, TestClient, TestServer, fresh_dir, movie};

pub(crate) const PASSWORD: &str = "correct horse";

/// A server with real ffmpeg and ffprobe, and the users alice and bob.
pub(crate) async fn server(test: &str, options: ServerOptions) -> TestServer {
    let server = TestServer::start_with(
        fresh_dir(test).await,
        ServerOptions {
            real_media: true,
            ..options
        },
    )
    .await;
    for name in ["alice", "bob"] {
        server.add_user(name, PASSWORD, None).await;
    }
    server
}

/// `Hello` from an app on `platform` listing `features`, saying nothing
/// of what it plays.
pub(crate) async fn hello(client: &TestClient, platform: Platform, features: &[&str]) {
    let hello = Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "test".into(),
        client_version: "0".into(),
        platform,
        device_name: None,
        capabilities: None,
        experimental: Vec::new(),
        features: features.iter().map(|n| n.to_string()).collect(),
    };
    assert!(matches!(
        client.call(None, Request::Hello(hello)).await,
        Response::Ok(Reply::ServerInfo(_))
    ));
}

/// A connection from an app on `platform` that lists downloads, signed in
/// as `user`.
async fn device(server: &TestServer, platform: Platform, user: &str) -> (TestClient, SessionId) {
    let client = server.client().await;
    hello(&client, platform, &[DOWNLOADS]).await;
    let session = client.login(user, PASSWORD).await;
    (client, session)
}

/// Write the movie: 4 s of H.264 with a DTS 5.1 track and an AAC stereo
/// one, in Matroska.
fn write_dts_film(server: &TestServer) {
    std::fs::create_dir_all(server.media.parent().unwrap()).unwrap();
    let status = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", "testsrc2=size=640x360:rate=25"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"])
        .args(["-f", "lavfi", "-i", "sine=frequency=880:sample_rate=48000"])
        .args(["-map", "0:v", "-map", "1:a", "-map", "2:a"])
        .args(["-t", "4", "-pix_fmt", "yuv420p", "-c:v", "libx264"])
        .args(["-c:a:0", "dca", "-c:a:1", "aac", "-strict", "-2"])
        .args(["-ac:a:0", "6", "-ac:a:1", "2"])
        .arg(&server.media)
        .status()
        .expect("ffmpeg runs");
    assert!(status.success(), "ffmpeg could not make the film");
}

async fn prepare(
    client: &TestClient,
    session: &SessionId,
    source: DownloadSource,
    tracks: DownloadTracks,
) -> Response {
    client
        .call(
            Some(session),
            Request::PrepareDownload {
                id: movie().id().clone(),
                source,
                tracks,
            },
        )
        .await
}

async fn prepared(
    client: &TestClient,
    session: &SessionId,
    source: DownloadSource,
    tracks: DownloadTracks,
) -> DownloadInfo {
    match prepare(client, session, source, tracks).await {
        Response::Ok(Reply::Download(info)) => info,
        other => panic!("not prepared: {other:?}"),
    }
}

pub(crate) async fn status(client: &TestClient, session: &SessionId, id: &str) -> Response {
    client
        .call(
            Some(session),
            Request::DownloadStatus {
                ids: vec![id.to_owned()],
            },
        )
        .await
}

/// Download `id` once it is ready or failed, for at most 60 s.
pub(crate) async fn settled(client: &TestClient, session: &SessionId, id: &str) -> DownloadInfo {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let info = match status(client, session, id).await {
            Response::Ok(Reply::Downloads { mut downloads, .. }) => downloads.remove(0),
            other => panic!("no status: {other:?}"),
        };
        if matches!(
            info.state,
            DownloadState::Ready { .. } | DownloadState::Failed { .. }
        ) {
            return info;
        }
        assert!(Instant::now() < deadline, "the download stays {info:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Download `id`'s size and SHA-256, once ready.
pub(crate) async fn ready(client: &TestClient, session: &SessionId, id: &str) -> (u64, String) {
    match settled(client, session, id).await.state {
        DownloadState::Ready { size, sha256 } => (size, sha256),
        other => panic!("the download failed: {other:?}"),
    }
}

/// One read of download `id`: the answer and the bytes after it.
pub(crate) async fn read(
    client: &TestClient,
    session: &SessionId,
    id: &str,
    offset: u64,
    length: u64,
) -> (ResourceResponse, Vec<u8>) {
    client
        .resource(&ResourceRequest::Download {
            session: session.clone(),
            id: id.to_owned(),
            offset,
            length,
        })
        .await
}

/// Download `id`'s file from `offset` to its end, `chunk` bytes a read.
pub(crate) async fn read_from(
    client: &TestClient,
    session: &SessionId,
    id: &str,
    mut offset: u64,
    chunk: u64,
) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let (response, body) = read(client, session, id, offset, chunk).await;
        let ResourceResponse::Ready {
            length,
            metadata: ResourceMetadata::Download { size },
        } = response
        else {
            panic!("not read: {response:?}");
        };
        assert_eq!(body.len() as u64, length);
        bytes.extend_from_slice(&body);
        offset += length;
        if offset >= size {
            return bytes;
        }
        assert!(length > 0, "a read short of the end sent nothing");
    }
}

pub(crate) fn sha256(bytes: &[u8]) -> String {
    <sha2::Sha256 as sha2::Digest>::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// What ffprobe says of `entries` in `file`, a line each.
pub(crate) fn probe(file: &Path, entries: &str) -> Vec<String> {
    let out = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", entries, "-of", "csv=p=0"])
        .arg(file)
        .output()
        .expect("ffprobe runs");
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| l.trim_end_matches(',').to_owned())
        .collect()
}

/// The SHA-256 of `file`'s video packets, as ffmpeg reads them.
fn video_hash(file: &Path) -> String {
    let out = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(file)
        .args([
            "-map",
            "0:v:0",
            "-c",
            "copy",
            "-f",
            "streamhash",
            "-hash",
            "sha256",
            "-",
        ])
        .output()
        .expect("ffmpeg runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

pub(crate) async fn events(client: &TestClient) -> RecvStream {
    let mut events = tokio::time::timeout(Duration::from_secs(5), client.conn.accept_uni())
        .await
        .expect("no events stream within 5 s")
        .unwrap();
    assert_eq!(events.read_u8().await.unwrap(), uni::EVENTS);
    events
}

/// The download events on `events` for download `id` until it is ready
/// or failed, for at most 60 s. Another download's are skipped: one the
/// test settled before may still be told after this one's first, under
/// load.
pub(crate) async fn moves_until_settled(events: &mut RecvStream, id: &str) -> Vec<DownloadState> {
    let mut moves = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let event = tokio::time::timeout_at(deadline, read_frame::<_, Event>(events))
            .await
            .unwrap_or_else(|_| panic!("not settled: {moves:?}"))
            .unwrap();
        let Event::DownloadChanged { id: of, state } = event else {
            continue;
        };
        if of != id {
            continue;
        }
        let settled = matches!(
            state,
            DownloadState::Ready { .. } | DownloadState::Failed { .. }
        );
        moves.push(state);
        if settled {
            return moves;
        }
    }
}

/// Each kind of state in `moves`, once, in order.
pub(crate) fn kinds(moves: &[DownloadState]) -> Vec<&'static str> {
    let mut kinds: Vec<&'static str> = Vec::new();
    for state in moves {
        let kind = match state {
            DownloadState::Waiting => "waiting",
            DownloadState::Converting { .. } => "converting",
            DownloadState::Writing { .. } => "writing",
            DownloadState::Ready { .. } => "ready",
            DownloadState::Failed { .. } => "failed",
            DownloadState::Unknown => "unknown",
        };
        if kinds.last() != Some(&kind) {
            kinds.push(kind);
        }
    }
    kinds
}

fn first_audio() -> DownloadTracks {
    DownloadTracks::Chosen {
        audio: vec![0],
        subtitles: Vec::new(),
    }
}

pub(crate) fn unsupported(response: &Response) -> bool {
    matches!(response, Response::Err(Error::Unsupported { .. }))
}

#[tokio::test]
async fn a_client_that_did_not_list_downloads_is_refused_them_and_told_nothing() {
    let server = server("downloads-wire-unlisted", ServerOptions::default()).await;
    write_film(&server);
    let old = server.client().await;
    hello(&old, Platform::Linux, &[]).await;
    let old_session = old.login("alice", PASSWORD).await;
    let mut old_events = events(&old).await;
    let options = old
        .call(
            Some(&old_session),
            Request::DownloadOptions {
                id: movie().id().clone(),
            },
        )
        .await;
    let refused_prepare = prepare(
        &old,
        &old_session,
        DownloadSource::Original,
        DownloadTracks::All,
    )
    .await;

    // The same user, on an app that lists them, downloads meanwhile.
    let (new, session) = device(&server, Platform::Linux, "alice").await;
    let mut new_events = events(&new).await;
    let info = prepared(&new, &session, DownloadSource::Original, first_audio()).await;
    moves_until_settled(&mut new_events, &info.id).await;
    let (read_refused, _) = read(&old, &old_session, &info.id, 0, 1 << 20).await;
    // Every event the server queued for the old app is out by now: the
    // read above went after them on the same connection.
    let told = tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            if let Event::DownloadChanged { .. } =
                read_frame::<_, Event>(&mut old_events).await.unwrap()
            {
                return;
            }
        }
    })
    .await;
    assert_eq!(
        (
            unsupported(&options),
            unsupported(&refused_prepare),
            matches!(
                read_refused,
                ResourceResponse::Error {
                    error: Error::Unsupported { .. }
                }
            ),
            told.is_ok()
        ),
        (true, true, true, false)
    );
    server.stop().await;
}

/// Review 2026-10-20, finding 6: a connection that signed in as bob and
/// out as alice is told nothing of alice's downloads. (Signed out of
/// every user, the device's connection is closed.)
#[tokio::test]
async fn a_connection_signed_out_of_a_user_is_told_nothing_of_their_downloads() {
    let server = server("downloads-wire-switched", ServerOptions::default()).await;
    write_film(&server);
    let (shared, first) = device(&server, Platform::Linux, "alice").await;
    let mut shared_events = events(&shared).await;
    // A request as alice: the connection is told of her downloads.
    status(&shared, &first, &"00".repeat(16)).await;
    let bob = shared.login("bob", PASSWORD).await;
    assert!(matches!(
        shared.call(Some(&first), Request::Logout).await,
        Response::Ok(Reply::LoggedOut)
    ));

    // Alice downloads on another device meanwhile.
    let (other, alice) = device(&server, Platform::Linux, "alice").await;
    let mut other_events = events(&other).await;
    let info = prepared(&other, &alice, DownloadSource::Original, first_audio()).await;
    moves_until_settled(&mut other_events, &info.id).await;
    // Every event queued for the shared connection is out once this is
    // answered on it.
    status(&shared, &bob, &info.id).await;
    let told = tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            if let Event::DownloadChanged { .. } =
                read_frame::<_, Event>(&mut shared_events).await.unwrap()
            {
                return;
            }
        }
    })
    .await;
    assert!(
        told.is_err(),
        "the shared connection was told of alice's download"
    );
    server.stop().await;
}

#[tokio::test]
async fn an_apple_tv_is_refused_downloads_though_it_lists_them() {
    let server = server("downloads-wire-tvos", ServerOptions::default()).await;
    write_film(&server);
    let (tv, session) = device(&server, Platform::Tvos, "alice").await;
    let refused = prepare(&tv, &session, DownloadSource::Original, DownloadTracks::All).await;
    let Response::Err(Error::Unsupported { request }) = refused else {
        panic!("not refused: {refused:?}");
    };
    assert!(request.contains("tvos"), "{request}");
    server.stop().await;
}

#[tokio::test]
async fn the_options_offer_the_viewer_s_languages_the_profiles_and_what_original_is() {
    let server = server("downloads-wire-options", ServerOptions::default()).await;
    write_film(&server);
    let (mac, session) = device(&server, Platform::Macos, "alice").await;
    let mut preferences = TrackPreferences::default();
    preferences.subtitles.languages = vec!["en".into(), "fr".into()];
    assert_eq!(
        mac.call(Some(&session), Request::SetTrackPreferences { preferences })
            .await,
        Response::Ok(Reply::Done)
    );
    let Response::Ok(Reply::DownloadOptions(DownloadOptionsInfo {
        tracks,
        preferred,
        profiles,
        original,
        left_out,
    })) = mac
        .call(
            Some(&session),
            Request::DownloadOptions {
                id: movie().id().clone(),
            },
        )
        .await
    else {
        panic!("no options");
    };
    assert_eq!(
        (
            tracks.subtitles.len(),
            preferred,
            profiles.contains(&"Mobile".to_owned()),
            original,
            left_out
        ),
        (
            3,
            DownloadTracks::Chosen {
                audio: vec![0],
                subtitles: vec![0, 1]
            },
            true,
            // Matroska for a Mac: every track remuxed into an MP4.
            OriginalDownload::Remux,
            Vec::new()
        )
    );
    server.stop().await;
}

/// A Linux app with every track of a Matroska film gets the film itself,
/// read whole a chunk at a time.
#[tokio::test]
async fn a_file_sent_as_it_is_reads_whole_in_chunks_as_the_film_s_own_bytes() {
    let server = server("downloads-wire-as-is", ServerOptions::default()).await;
    write_film(&server);
    let (linux, session) = device(&server, Platform::Linux, "alice").await;
    let info = prepared(
        &linux,
        &session,
        DownloadSource::Original,
        DownloadTracks::All,
    )
    .await;
    let (size, told) = ready(&linux, &session, &info.id).await;
    let bytes = read_from(&linux, &session, &info.id, 0, 256 << 10).await;
    let film = std::fs::read(&server.media).unwrap();
    assert_eq!(
        (
            bytes.len() as u64,
            sha256(&bytes),
            told,
            info.file.map(|f| f.container)
        ),
        (
            film.len() as u64,
            sha256(&film),
            sha256(&film),
            Some(Container::Matroska)
        )
    );
    assert_eq!(size, film.len() as u64);
    server.stop().await;
}

/// The rest of the file, from any byte: what a download broken off
/// resumes with.
#[tokio::test]
async fn a_read_from_the_middle_of_the_file_returns_the_rest_of_it() {
    let server = server("downloads-wire-resume", ServerOptions::default()).await;
    write_film(&server);
    let (linux, session) = device(&server, Platform::Linux, "alice").await;
    let info = prepared(
        &linux,
        &session,
        DownloadSource::Original,
        DownloadTracks::All,
    )
    .await;
    let (size, _) = ready(&linux, &session, &info.id).await;
    let half = size / 2 + 1;
    let rest = read_from(&linux, &session, &info.id, half, 1 << 20).await;
    let film = std::fs::read(&server.media).unwrap();
    assert_eq!(sha256(&rest), sha256(&film[half as usize..]));
    server.stop().await;
}

/// The film's DTS for an iPhone: converted to E-AC-3 in the pool, the
/// video and the AAC as they are; the next download of the DTS, another
/// file, converts nothing.
#[tokio::test]
async fn an_apple_download_of_dts_converts_the_audio_alone_and_only_once() {
    let server = server(
        "downloads-wire-dts",
        ServerOptions {
            detect_encoders: true,
            ..ServerOptions::default()
        },
    )
    .await;
    write_dts_film(&server);
    let (phone, session) = device(&server, Platform::Ios, "alice").await;
    let mut phone_events = events(&phone).await;
    let first = prepared(
        &phone,
        &session,
        DownloadSource::Original,
        DownloadTracks::All,
    )
    .await;
    let moves = moves_until_settled(&mut phone_events, &first.id).await;
    let bytes = read_from(&phone, &session, &first.id, 0, 4 << 20).await;
    let file = server.dir.as_ref().join("dts-download.mp4");
    std::fs::write(&file, &bytes).unwrap();
    let tracks = server.store.optimized_tracks().await.unwrap().len();

    // The DTS alone: another file, from the same conversion.
    let second = prepared(&phone, &session, DownloadSource::Original, first_audio()).await;
    let second_moves = moves_until_settled(&mut phone_events, &second.id).await;
    assert_eq!(
        (
            first.state,
            kinds(&moves).first().copied(),
            kinds(&moves).last().copied(),
            kinds(&moves).contains(&"converting"),
            probe(&file, "stream=codec_name"),
            video_hash(&file) == video_hash(&server.media),
            kinds(&second_moves).contains(&"converting"),
            server.store.optimized_tracks().await.unwrap().len(),
        ),
        (
            DownloadState::Waiting,
            Some("converting"),
            Some("ready"),
            true,
            ["h264", "eac3", "aac"].map(String::from).to_vec(),
            true,
            false,
            tracks,
        ),
        "{moves:?} then {second_moves:?}"
    );
    server.stop().await;
}

#[tokio::test]
async fn a_download_in_a_profile_is_made_from_its_copy() {
    let server = server("downloads-wire-mobile", ServerOptions::default()).await;
    write_film(&server);
    let (linux, session) = device(&server, Platform::Linux, "alice").await;
    let info = prepared(
        &linux,
        &session,
        DownloadSource::Profile {
            name: "Mobile (H.264)".into(),
        },
        first_audio(),
    )
    .await;
    ready(&linux, &session, &info.id).await;
    let bytes = read_from(&linux, &session, &info.id, 0, 4 << 20).await;
    let file = server.dir.as_ref().join("mobile-download.mkv");
    std::fs::write(&file, &bytes).unwrap();
    let copies = server.store.optimized_copies().await.unwrap();
    assert_eq!(
        (
            probe(&file, "stream=codec_name"),
            copies.iter().any(|c| c.target
                == spoolframe_server::store::optimized::CopyTarget::Profile(
                    "Mobile (H.264)".into()
                ))
        ),
        (vec!["h264".to_owned(), "aac".to_owned()], true)
    );
    server.stop().await;
}

/// A conversion that cannot read its film fails the download; prepared
/// again, it is made.
#[tokio::test]
async fn a_failed_conversion_fails_the_download_and_preparing_again_makes_it() {
    use std::os::unix::fs::PermissionsExt;
    let server = server("downloads-wire-failed", ServerOptions::default()).await;
    write_dts_film(&server);
    let (phone, session) = device(&server, Platform::Ios, "alice").await;
    // Probed (and the description kept) while it can be read.
    let options = phone
        .call(
            Some(&session),
            Request::DownloadOptions {
                id: movie().id().clone(),
            },
        )
        .await;
    assert!(matches!(options, Response::Ok(Reply::DownloadOptions(_))));
    let mode = |m| std::fs::set_permissions(&server.media, std::fs::Permissions::from_mode(m));
    mode(0o000).unwrap();
    let first = prepared(
        &phone,
        &session,
        DownloadSource::Original,
        DownloadTracks::All,
    )
    .await;
    let failed = settled(&phone, &session, &first.id).await.state;
    mode(0o644).unwrap();
    let again = prepared(
        &phone,
        &session,
        DownloadSource::Original,
        DownloadTracks::All,
    )
    .await;
    let then = settled(&phone, &session, &again.id).await.state;
    assert_eq!(
        (
            matches!(failed, DownloadState::Failed { .. }),
            again.id == first.id,
            matches!(then, DownloadState::Ready { .. })
        ),
        (true, true, true),
        "{failed:?} then {then:?}"
    );
    server.stop().await;
}

#[tokio::test]
async fn another_user_s_download_is_gone_to_ask_about_and_not_found_to_read_or_cancel() {
    let server = server("downloads-wire-others", ServerOptions::default()).await;
    write_film(&server);
    let (linux, alice) = device(&server, Platform::Linux, "alice").await;
    let info = prepared(
        &linux,
        &alice,
        DownloadSource::Original,
        DownloadTracks::All,
    )
    .await;
    ready(&linux, &alice, &info.id).await;
    let bob = linux.login("bob", PASSWORD).await;
    let asked = status(&linux, &bob, &info.id).await;
    let (read_by_bob, _) = read(&linux, &bob, &info.id, 0, 1 << 20).await;
    let cancelled = linux
        .call(
            Some(&bob),
            Request::CancelDownload {
                id: info.id.clone(),
            },
        )
        .await;
    let not_found = |e: &Error| matches!(e, Error::NotFound { .. });
    assert_eq!(
        (
            matches!(
                &asked,
                Response::Ok(Reply::Downloads { downloads, gone })
                    if downloads.is_empty() && *gone == [info.id.clone()]
            ),
            matches!(&read_by_bob, ResourceResponse::Error { error } if not_found(error)),
            matches!(&cancelled, Response::Err(e) if not_found(e)),
            matches!(
                settled(&linux, &alice, &info.id).await.state,
                DownloadState::Ready { .. }
            )
        ),
        (true, true, true, true)
    );
    server.stop().await;
}

/// Review 2026-10-20, finding 8: an id the user has not is answered as
/// gone, and the rest of the ask is answered as ever.
#[tokio::test]
async fn an_unknown_id_among_those_asked_about_is_gone_alone() {
    let server = server("downloads-wire-gone", ServerOptions::default()).await;
    write_film(&server);
    let (linux, alice) = device(&server, Platform::Linux, "alice").await;
    let info = prepared(&linux, &alice, DownloadSource::Original, first_audio()).await;
    let unknown = "00".repeat(16);
    let asked = linux
        .call(
            Some(&alice),
            Request::DownloadStatus {
                ids: vec![unknown.clone(), info.id.clone()],
            },
        )
        .await;
    let Response::Ok(Reply::Downloads { downloads, gone }) = asked else {
        panic!("not answered: {asked:?}");
    };
    assert_eq!(
        (
            downloads.into_iter().map(|d| d.id).collect::<Vec<_>>(),
            gone
        ),
        (vec![info.id], vec![unknown])
    );
    server.stop().await;
}

/// Two users' same download is one file: it stays while one of them has
/// it, and goes when both cancelled.
#[tokio::test]
async fn a_cancelled_download_s_file_goes_once_no_download_names_it() {
    let server = server("downloads-wire-cancel", ServerOptions::default()).await;
    write_film(&server);
    let (linux, alice) = device(&server, Platform::Linux, "alice").await;
    let bob = linux.login("bob", PASSWORD).await;
    let of_alice = prepared(&linux, &alice, DownloadSource::Original, first_audio()).await;
    ready(&linux, &alice, &of_alice.id).await;
    let of_bob = prepared(&linux, &bob, DownloadSource::Original, first_audio()).await;
    let row = server.store.download(&of_alice.id).await.unwrap().unwrap();
    let spoolframe_server::store::downloads::StoredState::Ready { file, .. } = row.state else {
        panic!("not ready: {row:?}");
    };
    let cancel = |session: SessionId, id: String| {
        let linux = linux.clone();
        async move {
            linux
                .call(Some(&session), Request::CancelDownload { id })
                .await
        }
    };
    let first = cancel(alice.clone(), of_alice.id.clone()).await;
    let kept = Path::new(&file).exists();
    let second = cancel(bob.clone(), of_bob.id.clone()).await;
    assert_eq!(
        (first, kept, second, Path::new(&file).exists()),
        (
            Response::Ok(Reply::Done),
            true,
            Response::Ok(Reply::Done),
            false
        )
    );
    server.stop().await;
}

/// A relay on loopback, and its URL.
async fn relay() -> (iroh_relay::server::Server, url::Url) {
    use iroh_relay::server::{RelayConfig, ServerConfig};
    let mut config = ServerConfig::default();
    config.relay = Some(RelayConfig::new((std::net::Ipv4Addr::LOCALHOST, 0)));
    let relay = iroh_relay::server::Server::spawn(config).await.unwrap();
    let addr = relay.http_addr().expect("served over HTTP");
    (relay, url::Url::parse(&format!("http://{addr}")).unwrap())
}

/// A client whose only way to `server` is `relay`: it has no IP socket.
async fn relayed_client(server: &TestServer, relay: &url::Url) -> TestClient {
    let endpoint = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
        .relay_mode(iroh::RelayMode::Custom(iroh::RelayMap::empty()))
        .clear_ip_transports()
        .bind()
        .await
        .unwrap();
    let addr = iroh::EndpointAddr::from_parts(
        server.server.id(),
        [iroh::TransportAddr::Relay(relay.clone().into())],
    );
    let conn = tokio::time::timeout(
        Duration::from_secs(10),
        endpoint.connect(addr, spoolframe_protocol::CLIENT_ALPN),
    )
    .await
    .expect("no connection through the relay within 10 s")
    .unwrap();
    TestClient {
        endpoint,
        conn,
        instance: Default::default(),
        controls: Default::default(),
    }
}

/// A download reads whole over a relay, the only path there is.
#[tokio::test]
async fn a_download_reads_whole_over_a_relay_alone() {
    let (relay, url) = relay().await;
    let server = server(
        "downloads-wire-relay",
        ServerOptions {
            relays: vec![spoolframe_server::network::Relay::Custom(url.clone())],
            ..ServerOptions::default()
        },
    )
    .await;
    let home = tokio::time::timeout(Duration::from_secs(10), async {
        let url = iroh::RelayUrl::from(url.clone());
        while !server.server.addr().relay_urls().any(|u| *u == url) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(home.is_ok(), "the server never homed on the relay");
    write_film(&server);
    let linux = relayed_client(&server, &url).await;
    hello(&linux, Platform::Linux, &[DOWNLOADS]).await;
    let session = linux.login("alice", PASSWORD).await;
    let info = prepared(
        &linux,
        &session,
        DownloadSource::Original,
        DownloadTracks::All,
    )
    .await;
    let (_, told) = ready(&linux, &session, &info.id).await;
    let bytes = read_from(&linux, &session, &info.id, 0, 1 << 20).await;
    let relayed = linux
        .conn
        .paths()
        .iter()
        .all(|p| matches!(p.remote_addr(), iroh::TransportAddr::Relay(_)));
    assert_eq!((sha256(&bytes), relayed), (told, true));
    linux.close().await;
    server.stop().await;
    drop(relay);
}
