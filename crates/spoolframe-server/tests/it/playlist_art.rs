//! Playlist covers (PX-53) over a real Iroh connection: the upload on a
//! resource stream and its refusals, the pictures fetched as artwork, and
//! the feature gate. Who may and the mosaic's rules are tested in
//! `playlists::art_tests`, decoding in `covers::tests` and
//! spoolframe-media's `picture`.

use crate::support;

use std::sync::Arc;

use spoolframe_client_sdk::{ArtworkSize, Client, Device, MemoryStorage, SdkError, Session};
use spoolframe_protocol::codec::{read_frame, write_frame};
use spoolframe_protocol::features::{LIKED, PLAYLIST_ART, PLAYLISTS};
use spoolframe_protocol::playlists::{Kinds, PlaylistEdit, PlaylistShare, Visibility};
use spoolframe_protocol::resource::{ResourceMetadata, ResourceRequest, ResourceResponse};
use spoolframe_protocol::streams::bi;
use spoolframe_protocol::{Error, Reply, Request, Response, SessionId};
use spoolframe_server::test_support::{poster_color, poster_of};
use spoolframe_types::{ArtworkId, MediaId, MediaKind, Namespace};
use support::{ServerOptions, TestClient, TestServer, fresh_dir, mkv_capabilities};
use tokio::io::AsyncWriteExt;

const EVERYTHING: Kinds = Kinds {
    movies: true,
    episodes: true,
    tracks: true,
};

fn picture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/../spoolframe-media/tests/fixtures/pictures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn media(kind: MediaKind, key: &str) -> MediaId {
    MediaId::new(Namespace::Plex, kind, key).unwrap()
}

/// A server with posters on its items, and alice, bob and carol.
async fn server(test: &str, options: ServerOptions) -> TestServer {
    let server = TestServer::start_with(fresh_dir(test).await, options).await;
    server.catalog.show_posters();
    for name in ["alice", "bob", "carol"] {
        server.add_user(name, "correct horse", None).await;
    }
    server
}

/// An SDK client of `server` that lists every feature, and `name` on it.
async fn signed_in(server: &TestServer, name: &str) -> (Client, Session) {
    let client = support::sdk_client(
        Arc::new(MemoryStorage::default()),
        Device {
            name: format!("{name}'s phone"),
            client_name: "sdk-test".into(),
            client_version: "0".into(),
            platform: spoolframe_client_sdk::types::Platform::Ios,
            capabilities: mkv_capabilities(),
        },
    )
    .await
    .unwrap();
    let id = server.server.id().to_string();
    client
        .add_address(&id, server.server.addr().ip_addrs().copied().collect())
        .unwrap();
    client.connect(&id).await.unwrap();
    client.pin(&id, true).await.unwrap();
    let session = client
        .sign_in(&id, name, "correct horse", false)
        .await
        .unwrap();
    (client, session)
}

/// A raw client listing `features`.
async fn raw(server: &TestServer, features: &[&str]) -> TestClient {
    let client = server.client().await;
    client.hello_listing(features).await;
    client
}

/// `SetPlaylistArt` by hand: the request announcing `length`, then
/// `body`, whatever its length; the answer.
async fn upload(
    client: &TestClient,
    session: &SessionId,
    id: &str,
    length: u64,
    body: &[u8],
) -> ResourceResponse {
    let (mut send, mut recv) = client.conn.open_bi().await.unwrap();
    send.write_u8(bi::RESOURCE).await.unwrap();
    let request = ResourceRequest::SetPlaylistArt {
        session: session.clone(),
        id: id.into(),
        length,
    };
    write_frame(&mut send, &request).await.unwrap();
    // A refusal before the body stops the stream: the write may fail.
    let _ = send.write_all(body).await;
    let _ = send.finish();
    read_frame(&mut recv).await.unwrap()
}

fn refused(response: ResourceResponse) -> Error {
    match response {
        ResourceResponse::Error { error } => error,
        other => panic!("not refused: {other:?}"),
    }
}

async fn create(client: &TestClient, session: &SessionId, visibility: Visibility) -> String {
    match client
        .call(
            Some(session),
            Request::CreatePlaylist {
                name: "Road trip".into(),
                kinds: EVERYTHING,
                visibility,
            },
        )
        .await
    {
        Response::Ok(Reply::PlaylistInfo(made)) => made.id,
        other => panic!("{other:?}"),
    }
}

/// The SDK uploads a HEIC; every call that lists the playlist says so,
/// the cover is fetched as artwork with nothing of the file but its
/// picture, and removing it brings back the mosaic, fetched too.
#[tokio::test]
async fn a_cover_round_trips_through_the_sdk() {
    let server = server("art-sdk", ServerOptions::default()).await;
    let (client, alice) = signed_in(&server, "alice").await;
    let made = alice
        .create_playlist("Road trip", EVERYTHING, Visibility::Personal)
        .await
        .unwrap();
    let edits = ["1", "2"]
        .iter()
        .map(|k| media(MediaKind::Movie, k))
        .chain([
            media(MediaKind::Episode, "12"),
            media(MediaKind::Track, "32"),
        ])
        .map(|id| PlaylistEdit::Add { id, before: None })
        .collect();
    alice
        .edit_playlist(&made.id, made.revision, edits)
        .await
        .unwrap();
    let mosaic = alice.playlist(&made.id, 0, 0).await.unwrap().playlist;
    assert!(!mosaic.custom_art);
    let mosaic = mosaic.artwork.unwrap();
    assert_eq!(mosaic.namespace(), Namespace::Image);

    let cover = alice
        .set_playlist_art(&made.id, picture("photo.heic"))
        .await
        .unwrap();
    let listed = alice.playlists().await.unwrap().playlists;
    assert_eq!(listed[0].artwork.as_ref(), Some(&cover));
    assert!(listed[0].custom_art);
    let bytes = alice.artwork(&cover, ArtworkSize::Original).await.unwrap();
    let kept = image::load_from_memory(&bytes).unwrap();
    assert_eq!((kept.width(), kept.height()), (64, 64));
    assert!(!bytes.windows(4).any(|w| w == b"Exif"));

    let after = alice.remove_playlist_art(&made.id).await.unwrap();
    assert!(!after.custom_art);
    assert_eq!(after.artwork.as_ref(), Some(&mosaic));
    // The mosaic: the four posters, each in its quarter.
    let bytes = alice.artwork(&mosaic, ArtworkSize::Original).await.unwrap();
    let grid = image::load_from_memory(&bytes).unwrap().to_rgb8();
    assert_eq!(grid.dimensions(), (1200, 1200));
    for (n, key) in ["1", "2", "11", "31"].iter().enumerate() {
        let want = poster_color(&poster_of(key)).unwrap();
        let (x, y) = (300 + (n as u32 % 2) * 600, 300 + (n as u32 / 2) * 600);
        let got = grid.get_pixel(x, y).0;
        assert!(
            got.iter().zip(want).all(|(a, b)| a.abs_diff(b) < 12),
            "quarter {n}: {got:?}, not {want:?}"
        );
    }
    // The sizes are made from it as from any artwork.
    let small = alice.artwork(&mosaic, ArtworkSize::Small).await.unwrap();
    assert_eq!(image::load_from_memory(&small).unwrap().width(), 320);
    client.close().await;
    server.stop().await;
}

/// Who may not, and what is too large or not a picture, is refused
/// before the picture is read: these announce 25 MB, or a picture they
/// do not send, and are answered at once.
#[tokio::test]
async fn refusals_come_before_the_picture_is_read() {
    let server = server("art-refused", ServerOptions::default()).await;
    server.catalog.serve_music();
    let tv = raw(&server, &[PLAYLISTS, LIKED, PLAYLIST_ART]).await;
    let alice = tv.login("alice", "correct horse").await;
    let bob = tv.login("bob", "correct horse").await;
    let carol = tv.login("carol", "correct horse").await;
    let bob_id = match tv.call(Some(&bob), Request::WhoAmI).await {
        Response::Ok(Reply::WhoAmI(who)) => who.user.user_id,
        other => panic!("{other:?}"),
    };
    let shared = Visibility::Shared {
        with: vec![PlaylistShare::new(bob_id, false)],
    };
    let playlist = create(&tv, &alice, shared).await;

    let too_large = refused(upload(&tv, &alice, &playlist, 25 << 20, &[]).await);
    assert!(
        matches!(too_large, Error::ImageTooLarge { max_bytes, .. } if max_bytes == 20 << 20),
        "{too_large:?}"
    );
    // A read-only share, and someone who does not see it.
    assert_eq!(
        refused(upload(&tv, &bob, &playlist, 1000, &[]).await),
        Error::NotAllowed
    );
    assert!(matches!(
        refused(upload(&tv, &carol, &playlist, 1000, &[]).await),
        Error::NotFound { .. }
    ));
    // Liked and All.
    let Response::Ok(Reply::Playlists { playlists, .. }) =
        tv.call(Some(&alice), Request::ListPlaylists).await
    else {
        panic!()
    };
    for builtin in playlists.iter().filter(|p| p.builtin.is_some()) {
        assert_eq!(
            refused(upload(&tv, &alice, &builtin.id, 1000, &[]).await),
            Error::NotAllowed
        );
        assert_eq!(
            tv.call(
                Some(&alice),
                Request::RemovePlaylistArt {
                    id: builtin.id.clone()
                }
            )
            .await,
            Response::Err(Error::NotAllowed)
        );
    }
    // 50 megapixels by the header, which is all there is.
    let mut fifty = picture("tall.png")[..16].to_vec();
    fifty.extend(10_000u32.to_be_bytes());
    fifty.extend(5_000u32.to_be_bytes());
    fifty.extend([8, 6, 0, 0, 0, 0, 0, 0, 0]);
    assert!(matches!(
        refused(upload(&tv, &alice, &playlist, fifty.len() as u64, &fifty).await),
        Error::ImageTooLarge { .. }
    ));
    let text = b"%PDF-1.7 not a picture";
    assert_eq!(
        refused(upload(&tv, &alice, &playlist, text.len() as u64, text).await),
        Error::ImageUnreadable
    );
    server.stop().await;
}

/// A bomb within the pixel limit runs into the decode's memory limit:
/// refused as unreadable, and the server goes on answering.
#[tokio::test]
async fn a_decompression_bomb_is_stopped_and_the_server_keeps_serving() {
    let options = ServerOptions {
        cover_limits: spoolframe_media::picture::DecodeLimits {
            memory: 512 << 20,
            ..spoolframe_media::picture::DecodeLimits::DEFAULT
        },
        ..ServerOptions::default()
    };
    let server = server("art-bomb", options).await;
    let tv = raw(&server, &[PLAYLISTS, PLAYLIST_ART]).await;
    let alice = tv.login("alice", "correct horse").await;
    let playlist = create(&tv, &alice, Visibility::Personal).await;
    let dir = fresh_dir("art-bomb-file").await;
    let bomb = dir.join("bomb.png");
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=6000x6000",
        ])
        .args(["-frames:v", "1", "-pix_fmt", "rgba64be"])
        .arg(&bomb)
        .status()
        .unwrap();
    assert!(status.success());
    let bytes = std::fs::read(&bomb).unwrap();
    assert_eq!(
        refused(upload(&tv, &alice, &playlist, bytes.len() as u64, &bytes).await),
        Error::ImageUnreadable
    );
    assert!(matches!(
        tv.call(Some(&alice), Request::ListPlaylists).await,
        Response::Ok(Reply::Playlists { .. })
    ));
    // And a photo after it is taken.
    let photo = picture("photo.heic");
    assert!(matches!(
        upload(&tv, &alice, &playlist, photo.len() as u64, &photo).await,
        ResourceResponse::Ready {
            metadata: ResourceMetadata::PlaylistArt { .. },
            ..
        }
    ));
    server.stop().await;
}

/// A session that did not list `spoolframe:playlist-art` is sent no
/// picture, and may neither upload nor remove one; one that did is sent
/// both fields. An uploaded cover is fetched only by who sees a playlist
/// that has it.
#[tokio::test]
async fn the_feature_gates_the_fields_and_the_requests() {
    let server = server("art-gate", ServerOptions::default()).await;
    let with = raw(&server, &[PLAYLISTS, PLAYLIST_ART]).await;
    let without = raw(&server, &[PLAYLISTS]).await;
    let alice = with.login("alice", "correct horse").await;
    let alice_old = without.login("alice", "correct horse").await;
    let playlist = create(&with, &alice, Visibility::Personal).await;
    let photo = picture("tall.png");
    let ResourceResponse::Ready {
        metadata: ResourceMetadata::PlaylistArt { artwork },
        length: 0,
    } = upload(&with, &alice, &playlist, photo.len() as u64, &photo).await
    else {
        panic!("not uploaded")
    };

    let summary = |reply: Response| match reply {
        Response::Ok(Reply::Playlists { mut playlists, .. }) => playlists.remove(0),
        other => panic!("{other:?}"),
    };
    let new = summary(with.call(Some(&alice), Request::ListPlaylists).await);
    assert_eq!(new.artwork.as_ref(), Some(&artwork));
    assert!(new.custom_art);
    let old = summary(without.call(Some(&alice_old), Request::ListPlaylists).await);
    assert_eq!(old.artwork, None);
    assert!(!old.custom_art);
    assert!(matches!(
        refused(upload(&without, &alice_old, &playlist, photo.len() as u64, &photo).await),
        Error::Unsupported { .. }
    ));
    assert!(matches!(
        without
            .call(
                Some(&alice_old),
                Request::RemovePlaylistArt {
                    id: playlist.clone()
                }
            )
            .await,
        Response::Err(Error::Unsupported { .. })
    ));

    // Alice fetches her cover; carol, who does not see the playlist, may
    // not, though she has its id.
    let (fetched, _) = with
        .artwork(&alice, artwork.clone(), ArtworkSize::Thumb)
        .await;
    assert!(
        matches!(fetched, spoolframe_protocol::ArtworkResponse::Ok(_)),
        "{fetched:?}"
    );
    let carol = with.login("carol", "correct horse").await;
    let (denied, _) = with.artwork(&carol, artwork, ArtworkSize::Thumb).await;
    assert!(
        matches!(
            denied,
            spoolframe_protocol::ArtworkResponse::Err(Error::NotFound { .. })
        ),
        "{denied:?}"
    );
    // Not a cover anyone has.
    let made_up: ArtworkId = "image:playlist/0123456789abcdef0123456789abcdef"
        .parse()
        .unwrap();
    let (none, _) = with.artwork(&alice, made_up, ArtworkSize::Thumb).await;
    assert!(matches!(
        none,
        spoolframe_protocol::ArtworkResponse::Err(Error::NotFound { .. })
    ));
    server.stop().await;
}

/// The SDK's errors for the refusals are typed.
#[tokio::test]
async fn the_sdk_says_each_refusal() {
    let server = server("art-sdk-errors", ServerOptions::default()).await;
    let (client, alice) = signed_in(&server, "alice").await;
    let made = alice
        .create_playlist("Road trip", EVERYTHING, Visibility::Personal)
        .await
        .unwrap();
    let mut big = picture("tall.png");
    big.resize(25 << 20, 0);
    assert!(matches!(
        alice.set_playlist_art(&made.id, big).await,
        Err(SdkError::ImageTooLarge { .. })
    ));
    assert_eq!(
        alice
            .set_playlist_art(&made.id, b"not a picture".to_vec())
            .await,
        Err(SdkError::ImageUnreadable)
    );
    client.close().await;
    server.stop().await;
}
