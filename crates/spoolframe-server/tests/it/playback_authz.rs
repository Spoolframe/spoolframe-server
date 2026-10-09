//! Playback controls belong to the session and endpoint that started the
//! playback (PX-10, the PX-09 authorization matrix's "Gaps"): check PB.
//!
//! Media reads were already tested against another session's handle
//! (`playback::a_handle_is_only_for_its_session_and_endpoint`). Here every
//! other control is: `StopPlayback`, `SeekPlayback`, `ChangeSubtitle`,
//! `GetManifest`, and the subtitle reads (`ReadSubtitle` and
//! `ReadSubtitleSegment`, which are resource requests on the wire).
//!
//! Each request is sent as an attacker who has learnt the handle would
//! send it: straight onto the wire, with the playback's real revision and
//! a fresh operation id. The test helper `TestClient::call` first asks for
//! the revision with the caller's own session, which is itself refused, so
//! through it only `PlaybackRevision` would be tested.
//!
//! Every refusal is `Unauthenticated`, as for media reads: the server does
//! not say whether a handle exists to anyone it does not belong to
//! (`playback_refusal`, `Refusal::NotYours`). After all of them, the owner's
//! playback is as it was.

use crate::support;

use spoolframe_protocol::resource::{ResourceRequest, ResourceResponse};
use spoolframe_protocol::rpc::RpcRequest;
use spoolframe_protocol::{
    Error, ManifestKind, PlaybackHandle, PlaybackPlan, Reply, Request, RequestEnvelope, Response,
    Selection, SessionId,
};
use spoolframe_types::{ClientCapabilities, SubtitleFormat};
use support::{
    ServerOptions, TestClient, TestServer, fresh_dir, hls_capabilities, mkv_capabilities, movie,
};

/// Someone other than the playback's owner, holding its handle.
struct Intruder {
    /// Who it is, for the failure message.
    who: &'static str,
    client: TestClient,
    session: SessionId,
}

/// Everyone but the owner (alice on the TV): another user on the same
/// device and on another one, alice herself on another device, and alice's
/// own TV session presented from another device.
async fn intruders(server: &TestServer, tv: &TestClient, alice: &SessionId) -> Vec<Intruder> {
    server.add_user("bob", "battery staple", None).await;
    let bob_on_tv = tv.login("bob", "battery staple").await;
    let laptop = server.client().await;
    laptop.hello_with(None, Some(mkv_capabilities())).await;
    let bob_on_laptop = laptop.login("bob", "battery staple").await;
    let alice_on_laptop = laptop.login("alice", "correct horse").await;
    vec![
        Intruder {
            who: "another user on the same device",
            client: tv.clone(),
            session: bob_on_tv,
        },
        Intruder {
            who: "another user on another device",
            client: laptop.clone(),
            session: bob_on_laptop,
        },
        Intruder {
            who: "the same user on another device",
            client: laptop.clone(),
            session: alice_on_laptop,
        },
        Intruder {
            who: "the owner's session from another device",
            client: laptop,
            session: alice.clone(),
        },
    ]
}

/// `request` as `intruder`, on the wire, claiming `revision`.
async fn control(intruder: &Intruder, request: Request, revision: u64) -> Response {
    let wire = RpcRequest {
        envelope: RequestEnvelope {
            session: Some(intruder.session.clone()),
            request,
        },
        operation: Some(intruder.client.operation()),
        expected_revision: Some(revision),
    };
    intruder.client.wire(&wire).await.reply.response
}

/// The RPC controls on `handle`.
fn rpc_controls(handle: &PlaybackHandle) -> Vec<(&'static str, Request)> {
    vec![
        (
            "SeekPlayback",
            Request::SeekPlayback {
                handle: handle.clone(),
                position_ms: 4_000,
            },
        ),
        (
            "ChangeSubtitle",
            Request::ChangeSubtitle {
                handle: handle.clone(),
                subtitle: None,
                bitmap: Default::default(),
                position_ms: 0,
            },
        ),
        (
            "GetManifest (multivariant)",
            Request::GetManifest {
                handle: handle.clone(),
                kind: ManifestKind::Multivariant,
            },
        ),
        (
            "GetManifest (media)",
            Request::GetManifest {
                handle: handle.clone(),
                kind: ManifestKind::Media,
            },
        ),
        (
            "GetManifest (subtitles)",
            Request::GetManifest {
                handle: handle.clone(),
                kind: ManifestKind::Subtitles,
            },
        ),
        (
            "StopPlayback",
            Request::StopPlayback {
                handle: handle.clone(),
            },
        ),
    ]
}

/// The subtitle reads on `handle`, as `session` would send them.
fn subtitle_reads(
    session: &SessionId,
    handle: &PlaybackHandle,
    revision: u64,
) -> Vec<(&'static str, ResourceRequest)> {
    vec![
        (
            "ReadSubtitle",
            ResourceRequest::Subtitle {
                session: session.clone(),
                handle: handle.clone(),
                revision,
                offset: 0,
                version: None,
            },
        ),
        (
            "ReadSubtitleSegment",
            ResourceRequest::SubtitleSegment {
                session: session.clone(),
                handle: handle.clone(),
                revision,
                segment: 1,
            },
        ),
    ]
}

/// Every control and subtitle read on `handle`, by each of `intruders`,
/// is refused, with no bytes.
async fn all_refused(intruders: &[Intruder], handle: &PlaybackHandle, revision: u64) {
    for intruder in intruders {
        for (what, request) in rpc_controls(handle) {
            assert_eq!(
                control(intruder, request, revision).await,
                Response::Err(Error::Unauthenticated),
                "{what} by {}",
                intruder.who
            );
        }
        for (what, request) in subtitle_reads(&intruder.session, handle, revision) {
            let (response, bytes) = intruder.client.resource(&request).await;
            assert_eq!(
                response,
                ResourceResponse::Error {
                    error: Error::Unauthenticated
                },
                "{what} by {}",
                intruder.who
            );
            assert!(bytes.is_empty(), "{what} by {} got bytes", intruder.who);
        }
    }
}

/// A direct playback with the subtitle file beside the movie selected, so
/// that `ReadSubtitle` has something to give its owner.
#[tokio::test]
async fn no_one_else_controls_or_reads_a_direct_playback() {
    let server = TestServer::start("authz-direct").await;
    server.write_media(1000);
    let sidecar = server.write_sidecar();
    server.add_user("alice", "correct horse", None).await;
    let tv = server.client().await;
    tv.hello_with(None, Some(mkv_capabilities())).await;
    let alice = tv.login("alice", "correct horse").await;
    let Response::Ok(Reply::Tracks(tracks)) = tv
        .call(
            Some(&alice),
            Request::Tracks {
                id: movie().id().clone(),
            },
        )
        .await
    else {
        panic!("no tracks");
    };
    let external = tracks.subtitles.iter().position(|s| s.external).unwrap();
    let Response::Ok(Reply::PlaybackStarted { handle, plan, .. }) = tv
        .call(
            Some(&alice),
            Request::StartPlayback {
                id: movie().id().clone(),
                selection: Some(Selection {
                    audio: None,
                    subtitle: Some(external as u32),
                }),
                start_ms: None,
                bitmap: Default::default(),
                link_bitrate: None,
            },
        )
        .await
    else {
        panic!("did not start");
    };
    assert!(matches!(plan, PlaybackPlan::Direct { .. }), "{plan:?}");
    let revision = tv.revision(&alice, &handle).await.unwrap();

    let intruders = intruders(&server, &tv, &alice).await;
    all_refused(&intruders, &handle, revision).await;

    // Nothing changed: the same revision, and the owner still reads the
    // file and its subtitle.
    assert_eq!(tv.revision(&alice, &handle).await, Ok(revision));
    let (response, bytes) = tv.read(&alice, &handle, 0, 10).await;
    assert!(
        matches!(response, spoolframe_protocol::MediaReadResponse::Ok(_)),
        "{response:?}"
    );
    assert_eq!(bytes.len(), 10);
    let (response, bytes) = tv
        .resource(&subtitle_reads(&alice, &handle, revision)[0].1)
        .await;
    assert!(
        matches!(response, ResourceResponse::Ready { .. }),
        "{response:?}"
    );
    assert_eq!(bytes, sidecar);
    // A control the owner makes still applies: the stops above did not.
    let stop = Request::StopPlayback {
        handle: handle.clone(),
    };
    assert_eq!(tv.call(Some(&alice), stop).await, Response::Ok(Reply::Done));
    // The laptop; the first intruder is on the TV.
    intruders[1].client.clone().close().await;
    tv.close().await;
    server.stop().await;
}

/// A converted (HLS) playback, made by real ffmpeg, with the subtitle file
/// beside the movie as its WebVTT rendition: there is a manifest and a
/// subtitle segment for its owner, and none for anyone else. A seek by
/// someone else restarts nothing.
#[tokio::test]
async fn no_one_else_controls_or_reads_a_converted_playback() {
    let server = TestServer::start_with(
        fresh_dir("authz-hls").await,
        ServerOptions {
            real_media: true,
            ..ServerOptions::default()
        },
    )
    .await;
    server.generate_media(10, "libx264", "ac3");
    server.write_sidecar();
    server.add_user("alice", "correct horse", None).await;
    let tv = server.client().await;
    let caps = ClientCapabilities {
        subtitles: [SubtitleFormat::WebVtt].into(),
        ..hls_capabilities()
    };
    tv.hello_with(None, Some(caps)).await;
    let alice = tv.login("alice", "correct horse").await;
    let Response::Ok(Reply::Tracks(tracks)) = tv
        .call(
            Some(&alice),
            Request::Tracks {
                id: movie().id().clone(),
            },
        )
        .await
    else {
        panic!("no tracks");
    };
    let external = tracks.subtitles.iter().position(|s| s.external).unwrap();
    let Response::Ok(Reply::PlaybackStarted {
        handle,
        plan: PlaybackPlan::Converted { .. },
        epoch: 0,
        ..
    }) = tv
        .call(
            Some(&alice),
            Request::StartPlayback {
                id: movie().id().clone(),
                selection: Some(Selection {
                    audio: None,
                    subtitle: Some(external as u32),
                }),
                start_ms: None,
                bitmap: Default::default(),
                link_bitrate: None,
            },
        )
        .await
    else {
        panic!("no converted playback");
    };
    let revision = tv.revision(&alice, &handle).await.unwrap();

    let intruders = intruders(&server, &tv, &alice).await;
    all_refused(&intruders, &handle, revision).await;

    // The owner's playback is as it was: the same revision, a manifest,
    // and its WebVTT segment.
    assert_eq!(tv.revision(&alice, &handle).await, Ok(revision));
    let manifest = Request::GetManifest {
        handle: handle.clone(),
        kind: ManifestKind::Media,
    };
    assert!(
        matches!(
            tv.call(Some(&alice), manifest).await,
            Response::Ok(Reply::Manifest { .. })
        ),
        "the owner has no manifest"
    );
    let segment = loop {
        let read = Request::ReadSubtitleSegment {
            handle: handle.clone(),
            n: 1,
        };
        match tv.call(Some(&alice), read).await {
            Response::Err(Error::SubtitleNotReady { .. }) => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            other => break other,
        }
    };
    assert!(
        matches!(segment, Response::Ok(Reply::Subtitle { .. })),
        "{segment:?}"
    );
    intruders[1].client.clone().close().await;
    tv.close().await;
    server.stop().await;
}
