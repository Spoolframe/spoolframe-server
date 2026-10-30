//! The artwork stream (P1-05) over a real Iroh connection.

use crate::support;

use std::time::Duration;

use spoolframe_protocol::experimental::SMART_CROP;
use spoolframe_protocol::{ArtworkFace, ArtworkResponse, ArtworkSize, Error, SessionId};
use spoolframe_server::test_support::BACKDROP;
use spoolframe_types::{ArtworkId, Namespace};
use support::{ServerOptions, TestClient, TestServer, mkv_capabilities, poster};

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

fn backdrop() -> ArtworkId {
    ArtworkId::new(Namespace::Plex, BACKDROP).unwrap()
}

async fn faces_of(tv: &TestClient, session: &SessionId, id: ArtworkId) -> Option<Vec<ArtworkFace>> {
    let (response, _) = tv.artwork(session, id, ArtworkSize::Medium).await;
    let ArtworkResponse::Ok(reply) = response else {
        panic!("no image: {response:?}");
    };
    reply.faces
}

/// A server with `experimental.smart_crop` as `smart_crop`, and alice.
async fn smart_crop_server(test: &str, smart_crop: bool) -> TestServer {
    let server = TestServer::start_with(
        support::fresh_dir(test).await,
        ServerOptions {
            smart_crop,
            ..Default::default()
        },
    )
    .await;
    server.add_user("alice", "correct horse", None).await;
    server
}

/// PX-64: the image is served first, the scan comes after it, and later
/// replies say what it found.
#[tokio::test]
async fn a_backdrop_s_faces_come_with_a_later_reply_for_who_named_the_feature() {
    let server = smart_crop_server("smart-crop", true).await;
    let tv = server.client().await;
    let info = tv.hello_naming(mkv_capabilities(), &[SMART_CROP]).await;
    assert_eq!(info.experimental, [SMART_CROP]);
    let alice = tv.login("alice", "correct horse").await;

    let mut faces = None;
    for _ in 0..200 {
        faces = faces_of(&tv, &alice, backdrop()).await;
        if faces.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let faces = faces.expect("the scan finished");
    assert_eq!(faces.len(), 1, "{faces:?}");
    assert!(faces[0].top < faces[0].eyes && faces[0].eyes < faces[0].bottom);

    // A poster has none to give, not an empty list: it is never scanned.
    assert_eq!(faces_of(&tv, &alice, poster()).await, None);
    tv.close().await;
    server.stop().await;
}

/// PX-64's "a peer without smart-crop sees no change on the wire".
#[tokio::test]
async fn a_client_that_did_not_name_the_feature_gets_no_faces() {
    let server = smart_crop_server("smart-crop-unnamed", true).await;
    let tv = server.client().await;
    let info = tv.hello_naming(mkv_capabilities(), &[]).await;
    assert!(info.experimental.is_empty());
    let alice = tv.login("alice", "correct horse").await;
    // The scan is for whoever else asks, so it runs; this client's reply
    // stays as it was.
    faces_of(&tv, &alice, backdrop()).await;
    let faces = server.artwork.faces().unwrap();
    for _ in 0..200 {
        if faces.known(&backdrop()).await.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(faces.known(&backdrop()).await.is_some());
    assert_eq!(faces_of(&tv, &alice, backdrop()).await, None);
    tv.close().await;
    server.stop().await;
}

/// Through the SDK, as an app: the hero's crop follows the face.
#[tokio::test]
async fn an_app_crops_a_backdrop_by_its_faces() {
    use spoolframe_client_sdk::{Experimental, backdrop_crop};
    let server = smart_crop_server("smart-crop-sdk", true).await;
    let asked = Experimental {
        smart_crop: true,
        ..Experimental::default()
    };
    let (_client, session) = crate::federation::app::signed_in(&server, "alice", asked).await;
    assert_eq!(session.experimental(), asked);
    let mut faces = None;
    for _ in 0..200 {
        faces = session
            .backdrop(&backdrop(), ArtworkSize::Medium)
            .await
            .unwrap()
            .faces;
        if faces.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let faces = faces.expect("the scan finished");
    let crop = backdrop_crop(&faces, 3.0);
    assert!(crop.top < backdrop_crop(&[], 3.0).top, "{crop:?}");
    assert!(crop.top <= f64::from(faces[0].top), "{crop:?}");
    server.stop().await;
}

#[tokio::test]
async fn a_server_with_the_feature_off_turns_it_on_for_no_one() {
    let server = smart_crop_server("smart-crop-off", false).await;
    let tv = server.client().await;
    let info = tv.hello_naming(mkv_capabilities(), &[SMART_CROP]).await;
    assert!(info.experimental.is_empty());
    let alice = tv.login("alice", "correct horse").await;
    assert_eq!(faces_of(&tv, &alice, backdrop()).await, None);
    assert!(server.artwork.faces().is_none());
    tv.close().await;
    server.stop().await;
}
