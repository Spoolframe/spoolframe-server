//! The artwork stream (P1-05) over a real Iroh connection.

use crate::support;

use spoolframe_protocol::{ArtworkResponse, ArtworkSize, Error};
use spoolframe_types::{ArtworkId, Namespace};
use support::{TestServer, poster};

#[tokio::test]
async fn an_image_comes_with_its_reply_and_exact_length() {
    let server = TestServer::start("artwork").await;
    server.add_user("alice", "correct horse", None).await;
    let tv = server.client().await;
    tv.hello().await;
    let alice = tv.login("alice", "correct horse").await;

    let (response, bytes) = tv.artwork(&alice, poster(), ArtworkSize::Thumb).await;
    let ArtworkResponse::Ok(reply) = response else {
        panic!("no image: {response:?}");
    };
    assert_eq!(reply.content_type, "image/jpeg");
    assert_eq!(reply.length, bytes.len() as u64);
    assert_eq!(image::load_from_memory(&bytes).unwrap().width(), 160);

    // The same image again: same etag.
    let (again, _) = tv.artwork(&alice, poster(), ArtworkSize::Thumb).await;
    let ArtworkResponse::Ok(again) = again else {
        panic!("no image");
    };
    assert_eq!(again.etag, reply.etag);
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn artwork_needs_a_valid_session() {
    let server = TestServer::start("artwork-no-session").await;
    let tv = server.client().await;
    tv.hello().await;
    let made_up = spoolframe_protocol::SessionId("made-up".into());
    let (response, bytes) = tv.artwork(&made_up, poster(), ArtworkSize::Small).await;
    assert_eq!(response, ArtworkResponse::Err(Error::Unauthenticated));
    assert!(bytes.is_empty());
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn an_id_that_is_not_an_image_is_not_found() {
    let server = TestServer::start("artwork-missing").await;
    server.add_user("alice", "correct horse", None).await;
    let tv = server.client().await;
    tv.hello().await;
    let alice = tv.login("alice", "correct horse").await;
    let id = ArtworkId::new(Namespace::Plex, "/library/sections/1/all").unwrap();
    let (response, _) = tv.artwork(&alice, id, ArtworkSize::Original).await;
    assert!(
        matches!(response, ArtworkResponse::Err(Error::NotFound { .. })),
        "{response:?}"
    );
    tv.close().await;
    server.stop().await;
}
