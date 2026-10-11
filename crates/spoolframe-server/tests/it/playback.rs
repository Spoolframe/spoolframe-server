//! Direct play over Iroh (P1-08): playback sessions and range reads.

use crate::support;

use std::time::Duration;

use spoolframe_protocol::codec::MAX_FRAME;
use spoolframe_protocol::{
    Error, MAX_READ, MediaReadReply, MediaReadResponse, PlaybackHandle, PlaybackPlan, Reply,
    Request, Response, SUBTITLE_CHUNK, Selection, SessionId,
};
use support::{PLAYBACK_IDLE, PLAYBACK_RECHECK, TestClient, TestServer, mkv_capabilities, movie};

/// A server with alice, and a client logged in as her that can play the
/// stand-in MKV.
async fn ready(test: &str) -> (TestServer, TestClient, SessionId) {
    let server = TestServer::start(test).await;
    server.add_user("alice", "correct horse", None).await;
    let client = server.client().await;
    client.hello_with(None, Some(mkv_capabilities())).await;
    let session = client.login("alice", "correct horse").await;
    (server, client, session)
}

async fn start(client: &TestClient, session: &SessionId, selection: Selection) -> Response {
    client
        .call(
            Some(session),
            Request::StartPlayback {
                id: movie().id().clone(),
                selection: Some(selection),
                start_ms: None,
                bitmap: Default::default(),
                quality: Default::default(),
                display: None,
            },
        )
        .await
}

async fn started(client: &TestClient, session: &SessionId) -> (PlaybackHandle, u64) {
    match start(client, session, Selection::default()).await {
        Response::Ok(Reply::PlaybackStarted { handle, size, .. }) => (handle, size),
        other => panic!("playback did not start: {other:?}"),
    }
}

fn ok(response: MediaReadResponse) -> MediaReadReply {
    match response {
        MediaReadResponse::Ok(reply) => reply,
        MediaReadResponse::Err(e) => panic!("read failed: {e:?}"),
    }
}

fn err(response: MediaReadResponse) -> Error {
    match response {
        MediaReadResponse::Err(e) => e,
        MediaReadResponse::Ok(reply) => panic!("read succeeded: {reply:?}"),
    }
}

#[tokio::test]
async fn ranges_read_in_any_order_match_the_file() {
    let (server, client, session) = ready("random-ranges").await;
    let file = server.write_media(3 << 20);
    let (handle, size) = started(&client, &session).await;
    assert_eq!(size, file.len() as u64);

    // Fixed, shuffled ranges covering the file, some overlapping.
    let ranges = [
        (2_000_000u64, 500_000u64),
        (0, 4096),
        (3_000_000, 145_728),
        (700_000, 1_300_000),
        (4096, 695_904),
        (2_500_000, 500_000),
        (1_999_999, 1),
    ];
    for (offset, length) in ranges {
        let (response, bytes) = client.read(&session, &handle, offset, length).await;
        let reply = ok(response);
        assert_eq!(reply.size, size);
        assert_eq!(reply.sent, bytes.len() as u64);
        let end = (offset + reply.sent) as usize;
        assert_eq!(bytes, file[offset as usize..end], "range {offset}+{length}");
    }
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn several_reads_run_at_once() {
    let (server, client, session) = ready("parallel-reads").await;
    let file = server.write_media(4 << 20);
    let (handle, _) = started(&client, &session).await;
    let reads: Vec<_> = (0..4u64)
        .map(|i| {
            let (client, session, handle) = (client.clone(), session.clone(), handle.clone());
            tokio::spawn(async move { client.read(&session, &handle, i << 20, 1 << 20).await })
        })
        .collect();
    for (i, read) in reads.into_iter().enumerate() {
        let (response, bytes) = read.await.unwrap();
        ok(response);
        assert_eq!(bytes, file[i << 20..(i + 1) << 20]);
    }
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_long_read_is_capped_and_the_next_one_continues() {
    let (server, client, session) = ready("capped").await;
    let file = server.write_media(20 << 20);
    let (handle, size) = started(&client, &session).await;

    let (response, first) = client.read(&session, &handle, 0, 32 << 20).await;
    let reply = ok(response);
    assert_eq!(reply.sent, MAX_READ);
    assert_eq!(reply.size, size);
    let (response, rest) = client.read(&session, &handle, reply.sent, 32 << 20).await;
    let reply = ok(response);
    assert_eq!(reply.sent, size - MAX_READ);
    assert_eq!([first, rest].concat(), file);
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_read_at_the_end_sends_nothing() {
    let (server, client, session) = ready("eof").await;
    server.write_media(1000);
    let (handle, size) = started(&client, &session).await;
    for offset in [size, size + 10] {
        let (response, bytes) = client.read(&session, &handle, offset, 100).await;
        assert_eq!(ok(response).sent, 0);
        assert!(bytes.is_empty());
    }
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_handle_is_only_for_its_session_and_endpoint() {
    let (server, tv, alice) = ready("not-yours").await;
    server.write_media(1000);
    server.add_user("bob", "battery staple", None).await;
    let (handle, _) = started(&tv, &alice).await;

    // Bob on the same connection.
    let bob = tv.login("bob", "battery staple").await;
    let (response, _) = tv.read(&bob, &handle, 0, 10).await;
    assert_eq!(err(response), Error::Unauthenticated);

    // Alice's session and handle from another endpoint.
    let laptop = server.client().await;
    laptop.hello().await;
    let (response, _) = laptop.read(&alice, &handle, 0, 10).await;
    assert_eq!(err(response), Error::Unauthenticated);
    tv.close().await;
    laptop.close().await;
    server.stop().await;
}

#[tokio::test]
async fn an_idle_handle_expires_and_a_new_start_reads_on() {
    let (server, client, session) = ready("expiry").await;
    let file = server.write_media(1000);
    let (handle, _) = started(&client, &session).await;
    server.clock.advance(PLAYBACK_IDLE);
    let (response, _) = client.read(&session, &handle, 500, 100).await;
    assert_eq!(err(response), Error::PlaybackExpired);

    let (handle, _) = started(&client, &session).await;
    let (response, bytes) = client.read(&session, &handle, 500, 100).await;
    ok(response);
    assert_eq!(bytes, file[500..600]);
    client.close().await;
    server.stop().await;
}

/// A paused client reports its position once a minute (PROTOCOL.md).
/// Each report keeps the playback alive, past the idle limit, and leaves
/// its watch state as it was. Without them it idles out at the limit.
#[tokio::test]
async fn progress_reports_while_paused_keep_a_playback_past_the_idle_limit() {
    let (server, client, session) = ready("paused-reports").await;
    let file = server.write_media(1000);
    let (handle, _) = started(&client, &session).await;
    ok(client.read(&session, &handle, 0, 100).await.0);
    let paused_at = Duration::from_secs(600);
    let minutes = PLAYBACK_IDLE.as_secs() / 60 + 5;
    for sequence in 1..=minutes {
        server.clock.advance(Duration::from_secs(60));
        let response = client
            .call(
                Some(&session),
                Request::UpdatePlaybackProgress {
                    handle: handle.clone(),
                    sequence,
                    position_ms: paused_at.as_millis() as u64,
                },
            )
            .await;
        assert_eq!(response, Response::Ok(Reply::Done), "report {sequence}");
    }
    server.debug.playbacks.sweep();
    let (response, bytes) = client.read(&session, &handle, 100, 100).await;
    ok(response);
    assert_eq!(bytes, file[100..200]);
    let item = client
        .call(
            Some(&session),
            Request::Item {
                id: movie().id().clone(),
            },
        )
        .await;
    let Response::Ok(Reply::Item {
        item: spoolframe_types::Item::Movie(movie),
        ..
    }) = item
    else {
        panic!("no movie: {item:?}");
    };
    assert_eq!(
        movie.playback,
        spoolframe_types::PlaybackState {
            position: Some(paused_at),
            watched: false,
        }
    );
    server.clock.advance(PLAYBACK_IDLE);
    server.debug.playbacks.sweep();
    let (response, _) = client.read(&session, &handle, 200, 100).await;
    assert_eq!(err(response), Error::PlaybackExpired);
    client.close().await;
    server.stop().await;
}

/// A playback the server ends, rather than its client, is logged once at
/// INFO: how it ended, whose it was, and how long it had been idle. An
/// Apple TV paused past the limit was told its playback had expired, and
/// the log had said nothing of it.
#[tokio::test]
async fn a_playback_the_server_ends_is_logged_once_and_one_its_client_stops_is_not() {
    const TEST: &str =
        "playback::a_playback_the_server_ends_is_logged_once_and_one_its_client_stops_is_not";
    if !support::alone(TEST) {
        return;
    }
    let (server, client, session) = ready("ended-logged").await;
    server.write_media(1000);
    let (stopped, _) = started(&client, &session).await;
    let (idle, _) = started(&client, &session).await;
    ok(client.read(&session, &idle, 0, 10).await.0);
    let (capture, _guard) = support::LogCapture::start();
    let response = client
        .call(Some(&session), Request::StopPlayback { handle: stopped })
        .await;
    assert_eq!(response, Response::Ok(Reply::Done));
    let after = PLAYBACK_IDLE + Duration::from_secs(1);
    server.clock.advance(after);
    server.debug.playbacks.sweep();
    let ended = capture.saying("a playback ended");
    assert_eq!(ended.len(), 1, "{ended:?}");
    let line = &ended[0];
    assert_eq!(line.level, tracing::Level::INFO);
    let id = &server.debug.playbacks.ended()[0].last.id;
    let endpoint = client.endpoint.id().to_string();
    for (field, value) in [
        ("why", "idle"),
        ("user", "alice"),
        ("playback", id.as_str()),
        ("endpoint", endpoint.as_str()),
        ("plan", "direct"),
    ] {
        assert_eq!(line.field(field), Some(value), "{field} in {line:?}");
    }
    let idle_secs: u64 = line.field("idle_secs").unwrap().parse().unwrap();
    assert!(idle_secs >= after.as_secs(), "{line:?}");
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn revoking_the_session_stops_reads_within_the_recheck_interval() {
    let (server, client, session) = ready("revoked").await;
    server.write_media(1000);
    let (handle, _) = started(&client, &session).await;
    ok(client.read(&session, &handle, 0, 10).await.0);
    server.store.disable_user("alice").await.unwrap();
    // The device's connection is closed (PX-10); it comes back.
    client.conn.closed().await;
    let client = client.reconnect(&server).await;
    server.clock.advance(PLAYBACK_RECHECK);
    let (response, _) = client.read(&session, &handle, 0, 10).await;
    assert_eq!(err(response), Error::Unauthenticated);
    client.close().await;
    server.stop().await;
}

/// B-10's backstop: an item no longer served, though the catalog was not
/// replaced (Plex moved it to a section not served here), ends at the
/// next recheck of a read.
#[tokio::test]
async fn an_item_no_longer_served_ends_at_the_recheck() {
    let (server, client, session) = ready("unserved-recheck").await;
    server.write_media(1000);
    let (handle, _) = started(&client, &session).await;
    ok(client.read(&session, &handle, 0, 10).await.0);
    server.catalog.set_old_key(
        movie().id(),
        spoolframe_server::test_support::OldKey::Foreign,
    );
    server.clock.advance(PLAYBACK_RECHECK);
    // The read that rechecks is served: the item is looked up beside it.
    client.read(&session, &handle, 0, 10).await;
    let playbacks = &server.debug.playbacks;
    tokio::time::timeout(Duration::from_secs(5), async {
        while playbacks.ended().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the playback ends");
    assert_eq!(
        playbacks.ended()[0].why,
        spoolframe_server::playback::EndReason::AccessRemoved
    );
    assert_eq!(
        err(client.read(&session, &handle, 0, 10).await.0),
        Error::PlaybackExpired
    );
    client.close().await;
    server.stop().await;
}

/// A Plex that cannot be reached at the recheck says nothing of whether
/// the item is served: the playback plays on.
#[tokio::test]
async fn an_item_that_cannot_be_looked_up_at_the_recheck_plays_on() {
    let (server, client, session) = ready("unreachable-recheck").await;
    server.write_media(1000);
    let (handle, _) = started(&client, &session).await;
    server.catalog.set_old_key(
        movie().id(),
        spoolframe_server::test_support::OldKey::Unreachable,
    );
    server.clock.advance(PLAYBACK_RECHECK);
    ok(client.read(&session, &handle, 0, 10).await.0);
    // Once the lookup beside that read has surely been answered.
    tokio::time::sleep(Duration::from_millis(300)).await;
    ok(client.read(&session, &handle, 0, 10).await.0);
    assert!(server.debug.playbacks.ended().is_empty());
    client.close().await;
    server.stop().await;
}

/// The external subtitles of the movie, as indexes for a `Selection`.
async fn sidecars(client: &TestClient, session: &SessionId) -> Vec<u32> {
    let Response::Ok(Reply::Tracks(tracks)) = client
        .call(
            Some(session),
            Request::Tracks {
                id: movie().id().clone(),
            },
        )
        .await
    else {
        panic!("no tracks");
    };
    let external = tracks
        .subtitles
        .iter()
        .enumerate()
        .filter(|(_, s)| s.external);
    external.map(|(index, _)| index as u32).collect()
}

/// A playback of the movie with the subtitle file beside it selected.
async fn started_with_sidecar(client: &TestClient, session: &SessionId) -> PlaybackHandle {
    let selection = Selection {
        audio: None,
        subtitle: Some(sidecars(client, session).await[0]),
    };
    match start(client, session, selection).await {
        Response::Ok(Reply::PlaybackStarted { handle, .. }) => handle,
        other => panic!("playback did not start: {other:?}"),
    }
}

/// `ReadSubtitle` from `offset`: with `version`, of that file only.
async fn read_subtitle(
    client: &TestClient,
    session: &SessionId,
    handle: &PlaybackHandle,
    offset: u64,
    version: Option<u64>,
) -> Response {
    let request = Request::ReadSubtitle {
        handle: handle.clone(),
        offset,
        version,
    };
    client.call(Some(session), request).await
}

/// One chunk of the playback's subtitle from `offset`, whatever file it
/// is now: its bytes, and the file's length and version as the reply
/// gives them.
async fn subtitle_chunk(
    client: &TestClient,
    session: &SessionId,
    handle: &PlaybackHandle,
    offset: u64,
) -> (Vec<u8>, u64, u64) {
    match read_subtitle(client, session, handle, offset, None).await {
        Response::Ok(Reply::SubtitleChunk {
            bytes,
            total,
            version,
            ..
        }) => (bytes, total, version),
        other => panic!("no subtitle: {other:?}"),
    }
}

#[tokio::test]
async fn a_selected_external_subtitle_is_read() {
    let (server, client, session) = ready("subtitle").await;
    server.write_media(1000);
    let sidecar = server.write_sidecar();
    let selection = Selection {
        audio: None,
        subtitle: Some(sidecars(&client, &session).await[0]),
    };
    let Response::Ok(Reply::PlaybackStarted { handle, plan, .. }) =
        start(&client, &session, selection).await
    else {
        panic!("did not start");
    };
    assert!(matches!(
        plan,
        PlaybackPlan::Direct {
            subtitle: Some(_),
            ..
        }
    ));
    let (bytes, total, _) = subtitle_chunk(&client, &session, &handle, 0).await;
    assert_eq!(total, sidecar.len() as u64);
    assert_eq!(bytes, sidecar);

    // Without an external subtitle selected there is nothing to read.
    let (handle, _) = started(&client, &session).await;
    assert!(matches!(
        read_subtitle(&client, &session, &handle, 0, None).await,
        Response::Err(Error::NotFound { .. })
    ));
    client.close().await;
    server.stop().await;
}

/// Large collections span bounded reply chunks; the connection carries on.
#[tokio::test]
async fn a_reply_larger_than_one_frame_is_reassembled() {
    let (server, client, session) = ready("reply-too-large").await;
    let response = client
        .call(
            Some(&session),
            Request::Search(support::search(support::A_TITLE_OVER_A_FRAME)),
        )
        .await;
    assert!(
        matches!(response, Response::Ok(Reply::Search(_))),
        "{response:?}"
    );
    assert!(spoolframe_protocol::codec::encode(&response).unwrap().len() > MAX_FRAME as usize);
    assert_eq!(
        client.call(None, Request::Ping).await,
        Response::Ok(Reply::Pong)
    );
    client.close().await;
    server.stop().await;
}

/// A subtitle too large for one frame is read in chunks, each from the
/// byte the one before ended at and of the file the first one was of
/// (P2-11).
#[tokio::test]
async fn a_subtitle_too_large_for_a_frame_is_read_in_chunks() {
    let (server, client, session) = ready("subtitle-chunks").await;
    server.write_media(1000);
    // No two chunks alike: 251 does not divide the chunk size.
    let sidecar: Vec<u8> = (0..MAX_FRAME as usize + 1)
        .map(|i| (i % 251) as u8)
        .collect();
    std::fs::write(server.media.with_extension("en.srt"), &sidecar).unwrap();
    let handle = started_with_sidecar(&client, &session).await;
    let total = sidecar.len() as u64;

    let (first, said, version) = subtitle_chunk(&client, &session, &handle, 0).await;
    assert_eq!(said, total);
    assert_eq!(first.len() as u64, SUBTITLE_CHUNK);
    let mut whole = first;
    while (whole.len() as u64) < total {
        let offset = whole.len() as u64;
        let response = read_subtitle(&client, &session, &handle, offset, Some(version)).await;
        let Response::Ok(Reply::SubtitleChunk {
            bytes,
            total: said,
            version: same,
            ..
        }) = response
        else {
            panic!("no chunk at {offset}");
        };
        assert_eq!((said, same), (total, version));
        assert!(!bytes.is_empty(), "nothing at {offset}");
        whole.extend(bytes);
    }
    assert!(whole == sidecar, "the chunks are not the file");
    client.close().await;
    server.stop().await;
}

/// A client's loop ends there: no bytes, and the length it has reached.
#[tokio::test]
async fn a_subtitle_read_at_or_past_the_end_answers_no_bytes() {
    let (server, client, session) = ready("subtitle-past-end").await;
    server.write_media(1000);
    let total = server.write_sidecar().len() as u64;
    let handle = started_with_sidecar(&client, &session).await;
    for offset in [total, total + 1, u64::MAX] {
        let (bytes, said, _) = subtitle_chunk(&client, &session, &handle, offset).await;
        assert_eq!((bytes, said), (Vec::new(), total), "at {offset}");
    }
    client.close().await;
    server.stop().await;
}

fn modified(path: &std::path::Path) -> std::time::SystemTime {
    std::fs::metadata(path).unwrap().modified().unwrap()
}

/// Sets the file's modification time. No test waits for the clock to give
/// a file another one.
fn set_modified(path: &std::path::Path, to: std::time::SystemTime) {
    let file = std::fs::File::options().write(true).open(path).unwrap();
    file.set_modified(to).unwrap();
}

/// A started playback of the movie with the subtitle `sidecar` beside it,
/// the file's path, and the version its first chunk named.
async fn first_chunk_read(
    server: &TestServer,
    client: &TestClient,
    session: &SessionId,
    sidecar: &[u8],
) -> (PlaybackHandle, std::path::PathBuf, u64) {
    server.write_media(1000);
    let path = server.media.with_extension("en.srt");
    std::fs::write(&path, sidecar).unwrap();
    let handle = started_with_sidecar(client, session).await;
    let (bytes, _, version) = subtitle_chunk(client, session, &handle, 0).await;
    assert_eq!(bytes, sidecar);
    (handle, path, version)
}

/// Another file of the same length and the same modification time is
/// moved over the subtitle between two chunks: only its inode tells it
/// from the first. The second chunk is refused, not answered from the new
/// file (P2-11).
#[tokio::test]
async fn a_subtitle_replaced_by_one_of_the_same_length_between_chunks_is_refused() {
    let (server, client, session) = ready("subtitle-replaced").await;
    let (handle, path, version) = first_chunk_read(&server, &client, &session, b"aaaaaaaa").await;

    let replacement = path.with_extension("replacement");
    std::fs::write(&replacement, b"bbbbbbbb").unwrap();
    set_modified(&replacement, modified(&path));
    std::fs::rename(&replacement, &path).unwrap();

    assert_eq!(
        read_subtitle(&client, &session, &handle, 4, Some(version)).await,
        Response::Err(Error::SubtitleChanged)
    );
    // From the start, it is the new file, under another version.
    let (bytes, _, new_version) = subtitle_chunk(&client, &session, &handle, 0).await;
    assert_eq!(bytes, b"bbbbbbbb");
    assert_ne!(new_version, version);
    assert!(matches!(
        read_subtitle(&client, &session, &handle, 4, Some(new_version)).await,
        Response::Ok(Reply::SubtitleChunk { .. })
    ));
    client.close().await;
    server.stop().await;
}

/// The same bytes written into the same file again, a second later: the
/// length and the inode are the same, the modification time is not.
#[tokio::test]
async fn a_subtitle_written_again_in_place_between_chunks_is_refused() {
    let (server, client, session) = ready("subtitle-rewritten").await;
    let (handle, path, version) = first_chunk_read(&server, &client, &session, b"aaaaaaaa").await;

    let before = modified(&path);
    std::fs::write(&path, b"aaaaaaaa").unwrap();
    set_modified(&path, before + Duration::from_secs(1));

    assert_eq!(
        read_subtitle(&client, &session, &handle, 4, Some(version)).await,
        Response::Err(Error::SubtitleChanged)
    );
    client.close().await;
    server.stop().await;
}

/// More bytes at the end of the same file, and the modification time put
/// back: only the length is another.
#[tokio::test]
async fn a_subtitle_that_grew_between_chunks_is_refused() {
    use std::io::Write;
    let (server, client, session) = ready("subtitle-grew").await;
    let (handle, path, version) = first_chunk_read(&server, &client, &session, b"aaaaaaaa").await;

    let before = modified(&path);
    let mut file = std::fs::File::options().append(true).open(&path).unwrap();
    file.write_all(b"bb").unwrap();
    drop(file);
    set_modified(&path, before);

    assert_eq!(
        read_subtitle(&client, &session, &handle, 4, Some(version)).await,
        Response::Err(Error::SubtitleChanged)
    );
    client.close().await;
    server.stop().await;
}

/// `ChangeSubtitle` to another file of the same length and format between
/// two chunks: the rest is not read from the other subtitle.
#[tokio::test]
async fn a_subtitle_changed_for_another_between_chunks_is_refused() {
    let (server, client, session) = ready("subtitle-changed-track").await;
    let other = server.media.with_extension("zz.srt");
    std::fs::create_dir_all(other.parent().unwrap()).unwrap();
    std::fs::write(&other, b"bbbbbbbb").unwrap();
    let (handle, path, version) = first_chunk_read(&server, &client, &session, b"aaaaaaaa").await;
    set_modified(&other, modified(&path));

    let sidecars = sidecars(&client, &session).await;
    assert_eq!(sidecars.len(), 2);
    let response = client
        .call(
            Some(&session),
            Request::ChangeSubtitle {
                handle: handle.clone(),
                subtitle: Some(sidecars[1]),
                bitmap: Default::default(),
                position_ms: 0,
                display: None,
            },
        )
        .await;
    assert!(
        matches!(response, Response::Ok(Reply::PlaybackChanged { .. })),
        "{response:?}"
    );

    assert_eq!(
        read_subtitle(&client, &session, &handle, 4, Some(version)).await,
        Response::Err(Error::SubtitleChanged)
    );
    let (bytes, _, _) = subtitle_chunk(&client, &session, &handle, 0).await;
    assert_eq!(bytes, b"bbbbbbbb");
    client.close().await;
    server.stop().await;
}

/// Swap `path` for a symlink to `target`, as a consumer who can write to
/// the library folder could once a playback has started (PX-09 B-1).
fn swap_for_symlink(path: &std::path::Path, target: &std::path::Path) {
    std::fs::remove_file(path).unwrap();
    std::os::unix::fs::symlink(target, path).unwrap();
}

/// The sidecar becomes a symlink into the state directory after
/// `StartPlayback`: the server key is not read through it.
#[tokio::test]
async fn a_sidecar_swapped_for_a_symlink_after_start_is_not_read() {
    let (server, client, session) = ready("subtitle-symlink-swap").await;
    server.write_media(1000);
    server.write_sidecar();
    let handle = started_with_sidecar(&client, &session).await;
    swap_for_symlink(
        &server.media.with_extension("en.srt"),
        &server.dir.join("endpoint.key"),
    );

    let response = read_subtitle(&client, &session, &handle, 0, None).await;
    assert!(
        matches!(response, Response::Err(Error::Unavailable { .. })),
        "{response:?}"
    );
    client.close().await;
    server.stop().await;
}

/// The media file becomes a symlink to the database after
/// `StartPlayback`: a range read does not answer from it.
#[tokio::test]
async fn a_media_file_swapped_for_a_symlink_after_start_is_not_read() {
    let (server, client, session) = ready("media-symlink-swap").await;
    server.write_media(1000);
    let (handle, _) = started(&client, &session).await;
    ok(client.read(&session, &handle, 0, 10).await.0);
    swap_for_symlink(&server.media, &server.dir.join("spoolframe.db"));

    let (response, bytes) = client.read(&session, &handle, 0, 100).await;
    assert!(bytes.is_empty(), "read {} bytes", bytes.len());
    assert!(
        matches!(err(response), Error::Unavailable { .. }),
        "the read was not refused"
    );
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_plan_that_is_not_direct_starts_nothing() {
    let server = TestServer::start("cannot-play").await;
    server.write_media(1000);
    server.add_user("alice", "correct horse", None).await;
    let client = server.client().await;
    // No capabilities: nothing can be promised to play.
    client.hello().await;
    let session = client.login("alice", "correct horse").await;
    assert!(matches!(
        start(&client, &session, Selection::default()).await,
        Response::Err(Error::CannotPlay { .. })
    ));
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn stop_playback_ends_the_handle() {
    let (server, client, session) = ready("stop").await;
    server.write_media(1000);
    let (handle, _) = started(&client, &session).await;
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
    let (response, _) = client.read(&session, &handle, 0, 10).await;
    assert_eq!(err(response), Error::PlaybackExpired);
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn reading_needs_a_valid_session() {
    let (server, client, session) = ready("read-no-session").await;
    server.write_media(1000);
    let (handle, _) = started(&client, &session).await;
    let (response, _) = client
        .read(&SessionId("made-up".into()), &handle, 0, 10)
        .await;
    assert_eq!(err(response), Error::Unauthenticated);
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn mutation_ids_survive_reconnect_and_reject_changed_payloads() {
    use spoolframe_protocol::resource::{ResourceRequest, ResourceResponse};
    use spoolframe_protocol::rpc::RpcRequest;
    use spoolframe_protocol::{MediaRead, RequestEnvelope};
    let (server, client, session) = ready("wire-replay").await;
    server.write_media(1000);
    let mut wire = RpcRequest {
        envelope: RequestEnvelope {
            session: Some(session.clone()),
            request: Request::StartPlayback {
                id: movie().id().clone(),
                selection: Some(Selection::default()),
                start_ms: None,
                bitmap: Default::default(),
                quality: Default::default(),
                display: None,
            },
        },
        operation: Some(client.operation()),
        expected_revision: None,
    };
    let (first, duplicate) = tokio::join!(client.wire(&wire), client.wire(&wire));
    assert_eq!(first.reply, duplicate.reply);
    let Response::Ok(Reply::PlaybackStarted { handle, .. }) = &first.reply.response else {
        panic!("{:?}", first.reply)
    };
    let reconnected = client
        .reconnect_with(&server, Some(mkv_capabilities()))
        .await;
    assert_eq!(reconnected.wire(&wire).await.reply, first.reply);
    wire.envelope.request = Request::StopPlayback {
        handle: handle.clone(),
    };
    assert_eq!(
        reconnected.wire(&wire).await.reply.response,
        Response::Err(Error::OperationConflict)
    );
    wire.operation = Some(reconnected.operation());
    // Stale control and stale bytes are rejected independently of encode epoch.
    wire.expected_revision = Some(1);
    assert_eq!(
        reconnected.wire(&wire).await.reply.response,
        Response::Err(Error::RevisionConflict { revision: 0 })
    );
    let (response, bytes) = reconnected
        .resource(&ResourceRequest::Media {
            request: MediaRead {
                session: session.clone(),
                handle: handle.clone(),
                offset: 0,
                length: 10,
                epoch: 0,
                segment: None,
                variant: 0,
            },
            revision: 1,
        })
        .await;
    assert_eq!(
        response,
        ResourceResponse::Error {
            error: Error::RevisionConflict { revision: 0 }
        }
    );
    assert!(bytes.is_empty());
    wire.operation = Some(reconnected.operation());
    wire.expected_revision = Some(0);
    assert_eq!(
        reconnected.wire(&wire).await.reply.response,
        Response::Ok(Reply::Done)
    );
    // The stopped handle is gone, but its successful stop remains replayable.
    assert_eq!(
        reconnected.wire(&wire).await.reply.response,
        Response::Ok(Reply::Done)
    );
    reconnected.close().await;
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn two_controls_with_the_same_revision_cannot_both_apply() {
    use spoolframe_protocol::RequestEnvelope;
    use spoolframe_protocol::rpc::RpcRequest;
    let (server, client, session) = ready("wire-cas").await;
    server.write_media(1000);
    let (handle, _) = started(&client, &session).await;
    let change = |operation| RpcRequest {
        envelope: RequestEnvelope {
            session: Some(session.clone()),
            request: Request::ChangeSubtitle {
                handle: handle.clone(),
                subtitle: None,
                bitmap: Default::default(),
                position_ms: 0,
                display: None,
            },
        },
        operation: Some(operation),
        expected_revision: Some(0),
    };
    let one = change(client.operation());
    let two = change(client.operation());
    let (one, two) = tokio::join!(client.wire(&one), client.wire(&two));
    let responses = [one.reply.response, two.reply.response];
    assert_eq!(
        responses
            .iter()
            .filter(|r| matches!(r, Response::Ok(Reply::PlaybackChanged { .. })))
            .count(),
        1
    );
    assert_eq!(
        responses
            .iter()
            .filter(|r| **r == Response::Err(Error::RevisionConflict { revision: 1 }))
            .count(),
        1
    );
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn progress_rejects_old_sequences_superseded_and_stopped_handles() {
    let (server, client, session) = ready("progress-order").await;
    server.write_media(1000);
    let (first, _) = started(&client, &session).await;
    let report = |handle, sequence, position_ms| Request::UpdatePlaybackProgress {
        handle,
        sequence,
        position_ms,
    };
    assert_eq!(
        client
            .call(Some(&session), report(first.clone(), 2, 30_000))
            .await,
        Response::Ok(Reply::Done)
    );
    assert_eq!(
        client
            .call(Some(&session), report(first.clone(), 1, 600_000))
            .await,
        Response::Err(Error::StaleProgress)
    );
    let (second, _) = started(&client, &session).await;
    assert_eq!(
        client
            .call(Some(&session), report(first.clone(), 3, 600_000))
            .await,
        Response::Err(Error::StaleProgress)
    );
    assert_eq!(
        client
            .call(Some(&session), report(second.clone(), 1, 600_000))
            .await,
        Response::Ok(Reply::Done)
    );
    assert_eq!(
        client
            .call(
                Some(&session),
                Request::StopPlayback {
                    handle: second.clone()
                }
            )
            .await,
        Response::Ok(Reply::Done)
    );
    assert_eq!(
        client
            .call(Some(&session), report(second, 2, 800_000))
            .await,
        Response::Err(Error::PlaybackExpired)
    );
    // Stopping the newer playback does not let an older one overwrite it.
    assert_eq!(
        client.call(Some(&session), report(first, 4, 900_000)).await,
        Response::Err(Error::StaleProgress)
    );
    client.close().await;
    server.stop().await;
}

/// PX-09 finding 14 (C-4): a read whose reader took nothing kept its
/// file open, and what was waiting to be sent, for as long as its
/// connection lived, so a client could pin as many as it could open
/// streams. One whose reader takes nothing for `network.stalled_read_secs`
/// is reset.
#[tokio::test]
async fn a_read_nobody_takes_is_reset() {
    use spoolframe_protocol::codec::{read_frame, write_frame};
    use spoolframe_protocol::resource::{ResourceRequest, ResourceResponse};
    use spoolframe_protocol::{MediaRead, streams::bi};
    use tokio::io::AsyncWriteExt as _;

    let server = TestServer::start_with(
        support::fresh_dir("read-unread").await,
        support::ServerOptions {
            admission: spoolframe_server::admission::Admission {
                stalled_read: Duration::from_secs(1),
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .await;
    server.add_user("alice", "correct horse", None).await;
    let client = server.client().await;
    client.hello_with(None, Some(mkv_capabilities())).await;
    let session = client.login("alice", "correct horse").await;
    server.write_media(16 << 20);
    let (handle, size) = started(&client, &session).await;
    let revision = client.revision(&session, &handle).await.unwrap();
    let (mut send, mut recv) = client.conn.open_bi().await.unwrap();
    send.write_u8(bi::RESOURCE).await.unwrap();
    let request = ResourceRequest::Media {
        request: MediaRead {
            session: session.clone(),
            handle,
            offset: 0,
            length: size,
            epoch: 0,
            segment: None,
            variant: 0,
        },
        revision,
    };
    write_frame(&mut send, &request).await.unwrap();
    send.finish().unwrap();
    let ready: ResourceResponse = read_frame(&mut recv).await.unwrap();
    assert!(matches!(ready, ResourceResponse::Ready { .. }), "{ready:?}");
    // Read nothing for a while, then all of it.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let rest = recv.read_to_end(32 << 20).await;
    assert!(rest.is_err(), "read whole: {} bytes", rest.unwrap().len());
    client.close().await;
    server.stop().await;
}
