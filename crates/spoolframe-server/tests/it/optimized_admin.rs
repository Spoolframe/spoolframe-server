//! Server › Optimized (PX-37) through the admin API, beside a server's
//! copy pool: the overview, profiles written and refused, outdated copies
//! deleted now, and a copy being read deleted once its reader is done.
//! The copies' rows are written to the store as made, so no ffmpeg runs.

use std::net::SocketAddr;
use std::time::Duration;

use jsonrpsee::core::ClientError;
use jsonrpsee::ws_client::WsClient;
use spoolframe_admin_api::{
    AdminApiClient, MeApiClient, NewUser, OptimizedCopyDeleted, OptimizedCopyState,
    OptimizedDeleted, OptimizedOverview, OptimizedProfile, OptimizedProfileWritten,
    OptimizedRequestPut, OptimizedScope, OptimizedScopeKind, Role, error_code,
};
use spoolframe_server::admin::{self, Admin, Origins, SetupCode, TrustedProxies};
use spoolframe_server::downloads::DeviceClass;
use spoolframe_server::store::optimized::{CopyRow, CopyTarget, CopyVideo, NewCopy, Recipe};
use spoolframe_types::VideoCodec;

use crate::admin::{CODE, admin, anonymous, code, creds, setup, with_token};
use crate::support::{TestServer, movie};

/// `Mobile` as the server ships it: H.264, 1080p, 4 Mbit/s.
const MOBILE: CopyVideo = CopyVideo {
    codec: VideoCodec::H264,
    height: 1080,
    max_bitrate: Some(4_000_000),
    tone_map: false,
    copied: false,
};

/// A server, and an admin API beside it on its pool: root's client.
async fn beside(test: &str) -> (TestServer, WsClient, SocketAddr) {
    let server = TestServer::start(test).await;
    let (addr, handle) = admin::serve(
        "127.0.0.1:0".parse().unwrap(),
        Admin {
            playback: Some(server.debug.clone()),
            optimized: Some(server.pool().clone()),
            ..admin(server.auth.clone(), Some(SetupCode::new(CODE)))
        },
        Origins::default(),
        TrustedProxies::default(),
    )
    .await
    .unwrap();
    std::mem::forget(handle);
    let root = anonymous(addr)
        .await
        .setup(setup(CODE, "root", "correct horse"))
        .await
        .unwrap();
    (server, with_token(addr, &root.token).await, addr)
}

/// The movie's file at `path`, made for `profile`: its video track alone,
/// `size` bytes long, a video of the profile's own (no two profiles'
/// copies share a track).
async fn made(server: &TestServer, path: &str, profile: &str, size: u64) -> CopyRow {
    let video = CopyVideo {
        height: 100 + profile.len() as u32,
        ..MOBILE
    };
    let (row, _) = server
        .store
        .request_optimized_copy(
            &NewCopy {
                media_id: movie().id().to_string(),
                media_path: path.to_owned(),
                source_stamp: "1000:1".into(),
                target: CopyTarget::Profile(profile.to_owned()),
                video,
            },
            &[Recipe::Video(video)],
        )
        .await
        .unwrap();
    for track in server.store.optimized_copy_tracks(&row.id).await.unwrap() {
        server
            .store
            .optimized_track_ready(&track.id, size)
            .await
            .unwrap();
    }
    server.store.optimized_copy(&row.id).await.unwrap().unwrap()
}

fn profile(name: &str, codec: &str, height: u32, max_bitrate: u64) -> OptimizedProfile {
    OptimizedProfile {
        name: name.into(),
        video_codec: codec.into(),
        height,
        max_bitrate,
    }
}

fn refusal(err: ClientError) -> (i32, String) {
    match err {
        ClientError::Call(e) => (e.code(), e.message().to_owned()),
        other => panic!("not a call error: {other}"),
    }
}

async fn ids(root: &WsClient) -> Vec<String> {
    let overview: OptimizedOverview = root.optimized_overview().await.unwrap();
    overview.copies.into_iter().map(|c| c.id).collect()
}

#[tokio::test]
async fn the_overview_shows_the_budget_the_profiles_and_each_copy() {
    let (server, root, _) = beside("opt-admin-overview").await;
    let copy = made(&server, "/media/alien.mkv", "Mobile", 5_000_000).await;
    let overview = root.optimized_overview().await.unwrap();
    assert_eq!(
        (overview.used_bytes, overview.max_size_bytes),
        (5_000_000, 1 << 30)
    );
    assert_eq!(
        overview
            .profiles
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>(),
        ["Mobile", "Mobile (H.264)", "TV", "Compact 4K"]
    );
    let shown = &overview.copies[0];
    assert_eq!(
        (
            shown.id.as_str(),
            shown.title.as_deref(),
            shown.path.as_str(),
            shown.target.as_str(),
            shown.state,
            shown.size_bytes,
            shown.outdated,
            shown.reading,
        ),
        (
            copy.id.as_str(),
            Some("Alien"),
            "/media/alien.mkv",
            "Mobile",
            OptimizedCopyState::Ready,
            Some(5_000_000),
            false,
            false,
        )
    );
    server.stop().await;
}

#[tokio::test]
async fn a_consumer_cannot_see_the_pool() {
    let (server, root, addr) = beside("opt-admin-consumer").await;
    root.user_add(NewUser {
        username: "alice".into(),
        display_name: None,
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
    let alice = with_token(addr, &alice.token).await;
    let err = alice.optimized_overview().await.unwrap_err();
    assert_eq!(code(err), error_code::FORBIDDEN);
    server.stop().await;
}

#[tokio::test]
async fn a_server_without_the_pool_says_so() {
    let (addr, _dir) = crate::admin::server("opt-admin-none").await;
    let root = anonymous(addr)
        .await
        .setup(setup(CODE, "root", "correct horse"))
        .await
        .unwrap();
    let root = with_token(addr, &root.token).await;
    let err = root.optimized_overview().await.unwrap_err();
    assert_eq!(code(err), error_code::NOT_CONFIGURED);
}

/// A server with binge-ahead off, as by default, still runs the
/// pool for downloads (PX-38), and the page lists their copies.
#[tokio::test]
async fn with_binge_ahead_off_the_overview_lists_a_download_s_copy() {
    let server = TestServer::start_with(
        crate::support::fresh_dir("opt-admin-off").await,
        crate::support::ServerOptions {
            binge_ahead: false,
            ..Default::default()
        },
    )
    .await;
    let (addr, handle) = admin::serve(
        "127.0.0.1:0".parse().unwrap(),
        Admin {
            // As the server wires it: the pool, always.
            optimized: Some(server.pool().clone()),
            ..admin(server.auth.clone(), Some(SetupCode::new(CODE)))
        },
        Origins::default(),
        TrustedProxies::default(),
    )
    .await
    .unwrap();
    std::mem::forget(handle);
    let root = anonymous(addr)
        .await
        .setup(setup(CODE, "root", "correct horse"))
        .await
        .unwrap();
    let root = with_token(addr, &root.token).await;
    server
        .store
        .request_optimized_copy(
            &NewCopy {
                media_id: movie().id().to_string(),
                media_path: "/media/alien.mkv".into(),
                source_stamp: "1000:1".into(),
                target: CopyTarget::Original {
                    class: DeviceClass::Apple,
                },
                video: CopyVideo {
                    copied: true,
                    max_bitrate: None,
                    ..MOBILE
                },
            },
            &[],
        )
        .await
        .unwrap();
    let overview = root.optimized_overview().await.unwrap();
    let targets: Vec<_> = overview.copies.into_iter().map(|c| c.target).collect();
    assert_eq!(targets, ["original (Apple)"]);
    server.stop().await;
}

#[tokio::test]
async fn a_profile_out_of_range_is_refused_saying_why() {
    let (server, root, _) = beside("opt-admin-refused").await;
    let err = root
        .optimized_profile_put(profile("Tablet", "h264", 100, 3_000_000))
        .await
        .unwrap_err();
    assert_eq!(
        refusal(err),
        (
            error_code::INVALID_INPUT,
            "a height of 100 lines: use 144 to 4320".to_owned()
        )
    );
    server.stop().await;
}

#[tokio::test]
async fn a_codec_no_copy_is_made_in_is_refused() {
    let (server, root, _) = beside("opt-admin-codec").await;
    let err = root
        .optimized_profile_put(profile("Tablet", "mpeg2video", 720, 3_000_000))
        .await
        .unwrap_err();
    assert_eq!(
        refusal(err),
        (
            error_code::INVALID_INPUT,
            "mpeg2video is not a codec a copy is made in: use h264, hevc or av1".to_owned()
        )
    );
    server.stop().await;
}

#[tokio::test]
async fn a_new_profile_is_added_last() {
    let (server, root, _) = beside("opt-admin-new").await;
    let written = root
        .optimized_profile_put(profile("Tablet", "av1", 1440, 6_000_000))
        .await
        .unwrap();
    assert_eq!(
        written,
        OptimizedProfileWritten {
            created: true,
            outdated: 0,
            requeued: 0
        }
    );
    let overview = root.optimized_overview().await.unwrap();
    assert_eq!(
        overview.profiles.last(),
        Some(&profile("Tablet", "av1", 1440, 6_000_000))
    );
    server.stop().await;
}

#[tokio::test]
async fn editing_a_profile_s_video_outdates_its_copies() {
    let (server, root, _) = beside("opt-admin-edit").await;
    let copy = made(&server, "/media/alien.mkv", "Mobile", 1000).await;
    made(&server, "/media/alien.mkv", "TV", 1000).await;
    let written = root
        .optimized_profile_put(profile("Mobile", "h264", 1080, 6_000_000))
        .await
        .unwrap();
    assert_eq!(
        written,
        OptimizedProfileWritten {
            created: false,
            outdated: 1,
            requeued: 0
        }
    );
    let overview = root.optimized_overview().await.unwrap();
    let outdated: Vec<_> = overview
        .copies
        .iter()
        .filter(|c| c.outdated)
        .map(|c| c.id.clone())
        .collect();
    assert_eq!(outdated, [copy.id]);
    server.stop().await;
}

#[tokio::test]
async fn removing_a_profile_outdates_its_copies() {
    let (server, root, _) = beside("opt-admin-remove").await;
    made(&server, "/media/alien.mkv", "TV", 1000).await;
    assert_eq!(root.optimized_profile_delete("TV".into()).await.unwrap(), 1);
    let overview = root.optimized_overview().await.unwrap();
    assert!(overview.copies[0].outdated);
    server.stop().await;
}

#[tokio::test]
async fn delete_them_now_removes_only_that_profile_s_outdated_copies() {
    let (server, root, _) = beside("opt-admin-outdated").await;
    made(&server, "/media/alien.mkv", "Mobile", 1000).await;
    made(&server, "/media/aliens.mkv", "Mobile", 1000).await;
    let tv = made(&server, "/media/alien.mkv", "TV", 1000).await;
    root.optimized_profile_put(profile("Mobile", "h264", 720, 4_000_000))
        .await
        .unwrap();
    root.optimized_profile_put(profile("TV", "hevc", 720, 12_000_000))
        .await
        .unwrap();
    let deleted = root
        .optimized_outdated_delete(Some("Mobile".into()))
        .await
        .unwrap();
    assert_eq!(
        (deleted, ids(&root).await),
        (
            OptimizedDeleted {
                now: 2,
                deferred: 0
            },
            vec![tv.id]
        )
    );
    server.stop().await;
}

#[tokio::test]
async fn a_copy_being_read_is_deleted_once_its_reader_is_done() {
    let (server, root, _) = beside("opt-admin-leased").await;
    let copy = made(&server, "/media/alien.mkv", "Mobile", 1000).await;
    let lease = server.pool().lease(&copy.id).await.unwrap().unwrap();
    assert!(root.optimized_overview().await.unwrap().copies[0].reading);
    assert_eq!(
        root.optimized_copy_delete(copy.id.clone()).await.unwrap(),
        OptimizedCopyDeleted::Deferred
    );
    assert_eq!(ids(&root).await, std::slice::from_ref(&copy.id));
    drop(lease);
    tokio::time::timeout(Duration::from_secs(10), async {
        while !ids(&root).await.is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the copy is removed once its lease ends");
    server.stop().await;
}

#[tokio::test]
async fn deleting_a_copy_not_read_removes_it_at_once() {
    let (server, root, _) = beside("opt-admin-delete").await;
    let copy = made(&server, "/media/alien.mkv", "Mobile", 1000).await;
    assert_eq!(
        root.optimized_copy_delete(copy.id.clone()).await.unwrap(),
        OptimizedCopyDeleted::Now
    );
    assert_eq!(
        (
            ids(&root).await,
            root.optimized_copy_delete(copy.id).await.unwrap()
        ),
        (Vec::<String>::new(), OptimizedCopyDeleted::NotFound)
    );
    server.stop().await;
}

#[tokio::test]
async fn delete_them_now_leaves_a_copy_being_read_to_its_reader() {
    let (server, root, _) = beside("opt-admin-outdated-read").await;
    let copy = made(&server, "/media/alien.mkv", "Mobile", 1000).await;
    let _lease = server.pool().lease(&copy.id).await.unwrap().unwrap();
    root.optimized_profile_put(profile("Mobile", "h264", 720, 4_000_000))
        .await
        .unwrap();
    let deleted = root.optimized_outdated_delete(None).await.unwrap();
    assert_eq!(
        (deleted, ids(&root).await),
        (
            OptimizedDeleted {
                now: 0,
                deferred: 1
            },
            vec![copy.id]
        )
    );
    server.stop().await;
}

/// An on-demand request for the fake catalog's film in `profile`.
fn for_the_film(profile: &str, pinned: bool) -> OptimizedRequestPut {
    OptimizedRequestPut {
        scope: OptimizedScope {
            kind: OptimizedScopeKind::Item,
            id: movie().id().to_string(),
        },
        profile: profile.into(),
        pinned,
    }
}

/// `Tiny`: about 97 MB for the film's 117 minutes, which a 1 GiB pool
/// holds pinned.
fn tiny() -> OptimizedProfile {
    profile("Tiny", "h264", 144, 100_000)
}

#[tokio::test]
async fn an_on_demand_request_reads_back_named_with_its_item_and_admin() {
    let (server, root, _) = beside("opt-admin-request").await;
    let put = root
        .optimized_request_put(for_the_film("Mobile", false))
        .await
        .unwrap();
    assert_eq!(
        (
            put.scope.clone(),
            put.title.as_deref(),
            put.profile.as_str(),
            put.pinned,
            put.created_by.as_deref(),
            put.items
        ),
        (
            for_the_film("Mobile", false).scope,
            Some("Alien"),
            "Mobile",
            false,
            Some("root"),
            1
        )
    );
    let listed = root.optimized_requests().await.unwrap();
    let overview = root.optimized_overview().await.unwrap();
    assert_eq!(
        (
            listed.iter().map(|r| &r.id).collect::<Vec<_>>(),
            overview.requests.iter().map(|r| &r.id).collect::<Vec<_>>()
        ),
        (vec![&put.id], vec![&put.id])
    );
    server.stop().await;
}

#[tokio::test]
async fn the_same_scope_and_profile_again_is_the_same_request() {
    let (server, root, _) = beside("opt-admin-request-again").await;
    root.optimized_profile_put(tiny()).await.unwrap();
    let first = root
        .optimized_request_put(for_the_film("Tiny", false))
        .await
        .unwrap();
    let again = root
        .optimized_request_put(for_the_film("Tiny", true))
        .await
        .unwrap();
    assert_eq!(
        (
            again.id == first.id,
            again.pinned,
            root.optimized_requests().await.unwrap().len()
        ),
        (true, true, 1)
    );
    server.stop().await;
}

/// `Mobile`'s film is about 3.9 GB: the server's 1 GiB pool cannot hold
/// it pinned, and says so.
#[tokio::test]
async fn a_pin_the_budget_cannot_hold_is_refused_saying_why() {
    let (server, root, _) = beside("opt-admin-request-budget").await;
    let (code, message) = refusal(
        root.optimized_request_put(for_the_film("Mobile", true))
            .await
            .unwrap_err(),
    );
    assert!(
        code == error_code::INVALID_INPUT
            && message.contains("pinned copies would not fit the budget")
            && message.contains("optimized.max_size_mb"),
        "{code} {message}"
    );
    server.stop().await;
}

#[tokio::test]
async fn a_request_is_pinned_and_unpinned() {
    let (server, root, _) = beside("opt-admin-request-pin").await;
    root.optimized_profile_put(tiny()).await.unwrap();
    let put = root
        .optimized_request_put(for_the_film("Tiny", false))
        .await
        .unwrap();
    let pinned = root
        .optimized_request_pin(put.id.clone(), true)
        .await
        .unwrap();
    let unpinned = root.optimized_request_pin(put.id, false).await.unwrap();
    assert_eq!((pinned.pinned, unpinned.pinned), (true, false));
    server.stop().await;
}

#[tokio::test]
async fn a_deleted_request_is_gone_and_a_second_delete_says_so() {
    let (server, root, _) = beside("opt-admin-request-delete").await;
    let put = root
        .optimized_request_put(for_the_film("Mobile", false))
        .await
        .unwrap();
    let first = root.optimized_request_delete(put.id.clone()).await.unwrap();
    let second = root.optimized_request_delete(put.id).await.unwrap();
    assert_eq!(
        (
            first,
            second,
            root.optimized_requests().await.unwrap().len()
        ),
        (true, false, 0)
    );
    server.stop().await;
}

#[tokio::test]
async fn a_scope_of_the_wrong_kind_is_refused() {
    let (server, root, _) = beside("opt-admin-request-kind").await;
    let request = OptimizedRequestPut {
        scope: OptimizedScope {
            kind: OptimizedScopeKind::Season,
            id: movie().id().to_string(),
        },
        ..for_the_film("Mobile", false)
    };
    let (code, message) = refusal(root.optimized_request_put(request).await.unwrap_err());
    assert_eq!(
        (code, message),
        (
            error_code::INVALID_INPUT,
            format!("{} is not a season", movie().id())
        )
    );
    server.stop().await;
}

#[tokio::test]
async fn a_request_in_no_such_profile_is_refused() {
    let (server, root, _) = beside("opt-admin-request-profile").await;
    let (code, message) = refusal(
        root.optimized_request_put(for_the_film("Nope", false))
            .await
            .unwrap_err(),
    );
    assert_eq!(
        (code, message.as_str()),
        (error_code::INVALID_INPUT, "there is no profile Nope")
    );
    server.stop().await;
}

/// A change to a profile's video asks for its requests' items again.
#[tokio::test]
async fn editing_a_profile_s_video_requeues_its_requests_items() {
    let (server, root, _) = beside("opt-admin-request-remake").await;
    root.optimized_request_put(for_the_film("Mobile", false))
        .await
        .unwrap();
    let written = root
        .optimized_profile_put(profile("Mobile", "h264", 1080, 6_000_000))
        .await
        .unwrap();
    assert_eq!(written.requeued, 1);
    server.stop().await;
}
