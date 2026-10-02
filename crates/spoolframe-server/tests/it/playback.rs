//! Direct play over Iroh (P1-08): playback sessions and range reads.

use crate::support;

use std::time::Duration;

use spoolframe_protocol::{
    Error, MAX_READ, MediaReadReply, MediaReadResponse, PlaybackHandle, PlaybackPlan, Reply,
    Request, Response, Selection, SessionId,
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
    tokio::time::sleep(PLAYBACK_IDLE + Duration::from_millis(200)).await;
    let (response, _) = client.read(&session, &handle, 500, 100).await;
    assert_eq!(err(response), Error::PlaybackExpired);

    let (handle, _) = started(&client, &session).await;
    let (response, bytes) = client.read(&session, &handle, 500, 100).await;
    ok(response);
    assert_eq!(bytes, file[500..600]);
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
    tokio::time::sleep(PLAYBACK_RECHECK + Duration::from_millis(100)).await;
    let (response, _) = client.read(&session, &handle, 0, 10).await;
    assert_eq!(err(response), Error::Unauthenticated);
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_selected_external_subtitle_is_read_whole() {
    let (server, client, session) = ready("subtitle").await;
    server.write_media(1000);
    let sidecar = server.write_sidecar();
    let Response::Ok(Reply::Tracks(tracks)) = client
        .call(
            Some(&session),
            Request::Tracks {
                id: movie().id().clone(),
            },
        )
        .await
    else {
        panic!("no tracks");
    };
    let external = tracks.subtitles.iter().position(|s| s.external).unwrap() as u32;

    let selection = Selection {
        audio: None,
        subtitle: Some(external),
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
    let response = client
        .call(Some(&session), Request::ReadSubtitle { handle })
        .await;
    let Response::Ok(Reply::Subtitle { bytes, .. }) = response else {
        panic!("no subtitle: {response:?}");
    };
    assert_eq!(bytes, sidecar);

    // Without an external subtitle selected there is nothing to read.
    let (handle, _) = started(&client, &session).await;
    assert!(matches!(
        client
            .call(Some(&session), Request::ReadSubtitle { handle })
            .await,
        Response::Err(Error::NotFound { .. })
    ));
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
