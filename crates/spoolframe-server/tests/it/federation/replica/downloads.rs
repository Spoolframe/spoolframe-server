//! P8-07: downloads of a paired server's items. Bob, a user of SB's,
//! downloads SA's film and SA's tracks from SB as he would SB's own: SA
//! makes and serves the file, SB keeps the record, and SB's disk holds
//! nothing of it. Against a real film and a real track, made at test time.

use spoolframe_protocol::downloads::{
    DownloadDevice, DownloadInfo, DownloadSource, DownloadState, DownloadTracks, OfflineWatch,
    OfflineWatchOutcome,
};
use spoolframe_protocol::features::{DOWNLOADS, LIKED, MUSIC_DOWNLOADS, PLAYLISTS};
use spoolframe_protocol::federation::{PlayTokenRedeemed, RedeemPlayToken, TokenScope};
use spoolframe_protocol::resource::ResourceResponse;
use spoolframe_protocol::{Hello, PROTOCOL_VERSION, Request, Response, SessionId};
use spoolframe_types::Platform;

use super::*;
use crate::downloads::write_film;
use crate::downloads_wire::{read, read_from, settled, sha256};
use crate::support::TestClient;

const FEDERATION: &str = spoolframe_protocol::experimental::FEDERATION;

/// SA with real ffmpeg and its film written, sharing Movies with SB, and
/// Bob a user of SB's.
async fn sharing_a_film(test: &str) -> Pair {
    let pair = Pair::start_both(
        test,
        ServerOptions {
            real_media: true,
            ..ServerOptions::default()
        },
        ServerOptions::default(),
    )
    .await;
    write_film(&pair.sa);
    pair.share("plex:1").await;
    pair.refresh().await;
    pair.sb.add_user("bob", "correct horse", None).await;
    pair
}

/// An app on Linux that downloads music too and names the feature, on
/// `server`, its `Hello` said: it plays what a Linux app does.
async fn app_on(server: &TestServer) -> TestClient {
    let client = server.client().await;
    say_hello(&client).await;
    client
}

/// Bob's app on SB, signed in.
async fn bob_s_app(pair: &Pair) -> (TestClient, SessionId) {
    let app = app_on(&pair.sb).await;
    let bob = app.login("bob", "correct horse").await;
    (app, bob)
}

/// `id` prepared in original quality with every track, as SB answers.
async fn prepared(app: &TestClient, bob: &SessionId, id: &MediaId) -> DownloadInfo {
    let request = Request::PrepareDownload {
        id: id.clone(),
        source: DownloadSource::Original,
        tracks: DownloadTracks::All,
        device: None,
    };
    match app.call(Some(bob), request).await {
        Response::Ok(Reply::Download(info)) => info,
        other => panic!("not prepared: {other:?}"),
    }
}

/// A download token for `download` (SB's id) of `id`, for `app`'s device.
async fn granted(
    app: &TestClient,
    bob: &SessionId,
    id: &MediaId,
    download: &str,
) -> spoolframe_protocol::federation::RemoteGranted {
    let request = Request::RemoteGrant {
        id: id.clone(),
        download: Some(download.to_owned()),
    };
    match app.call(Some(bob), request).await {
        Response::Ok(Reply::RemoteGranted(granted)) => granted,
        other => panic!("no grant: {other:?}"),
    }
}

/// `token` redeemed on SA by `device`.
async fn redeemed(device: &TestClient, token: &str) -> PlayTokenRedeemed {
    let request = Request::RedeemPlayToken(RedeemPlayToken {
        token: token.to_owned(),
    });
    match device.call(None, request).await {
        Response::Ok(Reply::PlayTokenRedeemed(redeemed)) => redeemed,
        other => panic!("not redeemed: {other:?}"),
    }
}

/// Whether SA's session `session` is gone.
async fn session_ended(pair: &Pair, session: &SessionId) -> bool {
    pair.sa
        .auth
        .session_owner(&session.0)
        .await
        .unwrap()
        .is_none()
}

fn unauthenticated(response: &Response) -> bool {
    matches!(response, Response::Err(Error::Unauthenticated))
}

/// What SB writes of downloads: no row of its own, nothing in its
/// download area, and no byte sent from it.
fn sb_holds_no_download(pair: &Pair) -> bool {
    let area = pair.sb.dir.join("optimized").join("downloads");
    let files = walk(&area);
    files.is_empty() && pair.sb.downloads.sent() == 0
}

/// Every file under `dir`.
fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut left = vec![dir.to_path_buf()];
    while let Some(at) = left.pop() {
        let Ok(entries) = std::fs::read_dir(&at) else {
            continue;
        };
        for path in entries.flatten().map(|e| e.path()) {
            if path.is_dir() {
                left.push(path);
            } else {
                found.push(path);
            }
        }
    }
    found
}

/// Bob downloads SA's film from SB: the options are SA's planner's for his
/// device, the file is SA's, read whole through SB (proxied) and straight
/// from SA with a download token (direct), the same bytes both ways. SB
/// keeps the record under its own id and writes nothing of the file. His
/// app's cancel, once it has the file, is passed to SA, which lets its
/// download go.
#[tokio::test]
async fn bob_downloads_sa_s_film_whole_through_sb_and_from_sa() {
    let pair = sharing_a_film("fed-dl-film").await;
    let (app, bob) = bob_s_app(&pair).await;
    let alien = pair.fed(movie().id());
    let options = app
        .call(
            Some(&bob),
            Request::DownloadOptions {
                id: alien.clone(),
                device: None,
            },
        )
        .await;
    let Response::Ok(Reply::DownloadOptions(options)) = options else {
        panic!("no options: {options:?}");
    };
    assert_eq!(options.tracks.audio.len(), 2, "SA's file's tracks");
    let info = prepared(&app, &bob, &alien).await;
    let on_sa = pair.sa.store.downloads().await.unwrap();
    assert_eq!(on_sa.len(), 1, "SA makes it");
    assert_ne!(info.id, on_sa[0].id, "SB keeps it under its own id");
    assert_eq!(
        on_sa[0].principal,
        format!(
            "remote:{}:{}",
            pair.sb.server_id,
            pair.sb.store.user_by_name("bob").await.unwrap().unwrap().id
        )
    );
    let settled = settled(&app, &bob, &info.id).await;
    let DownloadState::Ready { size, sha256: sum } = settled.state else {
        panic!("not ready: {settled:?}");
    };
    let file = std::fs::read(&pair.sa.media).unwrap();
    assert_eq!(size, file.len() as u64, "sent as it is");
    // Through SB, in reads smaller than the file.
    let through_sb = read_from(&app, &bob, &info.id, 0, 64 << 10).await;
    assert_eq!(sha256(&through_sb), sum);
    assert_eq!(through_sb, file);
    // Straight from SA.
    let granted = granted(&app, &bob, &alien, &info.id).await;
    assert_eq!(granted.endpoint_id, pair.sa.server.id().to_string());
    let remote = granted.download.clone().expect("SA's id");
    let device = app_from(&pair.sa, &app).await;
    let opened = redeemed(&device, &granted.token).await;
    assert_eq!(opened.scope, TokenScope::Download);
    let from_sa = read_from(&device, &opened.session, &remote, 0, 1 << 20).await;
    assert_eq!(from_sa, file);
    assert!(sb_holds_no_download(&pair), "SB wrote nothing of it");
    // Done: the app cancels, SB tells SA, and SA's download goes with the
    // device's token.
    assert!(matches!(
        app.call(
            Some(&bob),
            Request::CancelDownload {
                id: info.id.clone()
            }
        )
        .await,
        Response::Ok(Reply::Done)
    ));
    assert!(pair.sa.store.downloads().await.unwrap().is_empty());
    assert!(
        session_ended(&pair, &opened.session).await,
        "the device's token"
    );
    match app
        .call(Some(&bob), Request::DownloadStatus { ids: vec![info.id] })
        .await
    {
        Response::Ok(Reply::Downloads { downloads, gone }) => {
            assert!(downloads.is_empty() && gone.len() == 1)
        }
        other => panic!("{other:?}"),
    }
    pair.stop().await;
}

/// `app`'s device (its key) on `server`: what it dials SA with.
async fn app_from(server: &TestServer, app: &TestClient) -> TestClient {
    let device = server.client_from(&app.endpoint).await;
    let hello = Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "test".into(),
        client_version: "0".into(),
        platform: Platform::Linux,
        device_name: None,
        capabilities: None,
        experimental: vec![FEDERATION.into()],
        features: vec![DOWNLOADS.into()],
    };
    assert!(matches!(
        device.call(None, Request::Hello(hello)).await,
        Response::Ok(Reply::ServerInfo(_))
    ));
    device
}

/// A device's download token reads its one download and nothing else: not
/// another user's download of the same film, not one made up, and it
/// neither plays the film, nor prepares, nor reaches the libraries. A
/// play token reads no download.
#[tokio::test]
async fn a_download_token_reads_its_download_alone_and_plays_nothing() {
    let pair = sharing_a_film("fed-dl-scope").await;
    let (app, bob) = bob_s_app(&pair).await;
    let alien = pair.fed(movie().id());
    let info = prepared(&app, &bob, &alien).await;
    settled(&app, &bob, &info.id).await;
    let granted = granted(&app, &bob, &alien, &info.id).await;
    let remote = granted.download.clone().unwrap();
    let device = app_from(&pair.sa, &app).await;
    let session = redeemed(&device, &granted.token).await.session;
    // Alice's download of the same film on SA, her own.
    pair.sa.add_user("alice", "correct horse", None).await;
    let alice_s = app_on(&pair.sa).await;
    let alice = alice_s.login("alice", "correct horse").await;
    let hers = prepared(&alice_s, &alice, movie().id()).await;
    for other in [hers.id.as_str(), "ab".repeat(16).as_str()] {
        let (response, _) = read(&device, &session, other, 0, 4096).await;
        assert!(
            matches!(
                response,
                ResourceResponse::Error {
                    error: Error::NotFound { .. }
                }
            ),
            "{other}: {response:?}"
        );
    }
    let (mine, _) = read(&device, &session, &remote, 0, 4096).await;
    assert!(matches!(mine, ResourceResponse::Ready { .. }), "{mine:?}");
    for request in [
        Request::StartPlayback {
            id: movie().id().clone(),
            selection: None,
            start_ms: None,
            bitmap: Default::default(),
            quality: Default::default(),
            display: None,
            capabilities: None,
            onward_share: None,
        },
        Request::PrepareDownload {
            id: movie().id().clone(),
            source: DownloadSource::Original,
            tracks: DownloadTracks::All,
            device: None,
        },
        Request::Libraries,
        Request::Item {
            id: movie().id().clone(),
        },
    ] {
        let name = format!("{request:?}");
        let answer = device.call(Some(&session), request).await;
        assert!(unauthenticated(&answer), "{name}: {answer:?}");
    }
    // Her download is not his to ask about either.
    match device
        .call(
            Some(&session),
            Request::DownloadStatus {
                ids: vec![hers.id.clone(), remote.clone()],
            },
        )
        .await
    {
        Response::Ok(Reply::Downloads { downloads, gone }) => {
            assert_eq!(downloads.len(), 1);
            assert_eq!(downloads[0].id, remote);
            assert_eq!(gone, std::slice::from_ref(&hers.id));
        }
        other => panic!("{other:?}"),
    }
    // A play token's session reads no download.
    let bob_id = pair.sb.store.user_by_name("bob").await.unwrap().unwrap().id;
    let player = app_from(&pair.sa, &app).await;
    let play = federation(&pair.sb)
        .assert_play(bob_id, &alien, player.endpoint.id())
        .await
        .unwrap();
    let playing = redeemed(&player, &play.token).await.session;
    let (refused, _) = read(&player, &playing, &remote, 0, 4096).await;
    assert!(
        matches!(
            refused,
            ResourceResponse::Error {
                error: Error::Unauthenticated
            }
        ),
        "{refused:?}"
    );
    pair.stop().await;
}

/// SA stops sharing the library once Bob's download is made: SA cancels
/// its download of the film and the device's token reads no more, and SB
/// keeps its record, still `Ready` (an app deletes a failed download's
/// file) but no longer available, which reads nothing through SB either.
/// Shared again, the film is downloaded anew.
#[tokio::test]
async fn a_revoked_grant_ends_the_download_and_sb_marks_it_unavailable() {
    let pair = sharing_a_film("fed-dl-revoke").await;
    let (app, bob) = bob_s_app(&pair).await;
    let alien = pair.fed(movie().id());
    let info = prepared(&app, &bob, &alien).await;
    settled(&app, &bob, &info.id).await;
    let granted = granted(&app, &bob, &alien, &info.id).await;
    let remote = granted.download.clone().unwrap();
    let device = app_from(&pair.sa, &app).await;
    let session = redeemed(&device, &granted.token).await.session;
    federation(&pair.sa)
        .unshare(&pair.sb_on_sa, "plex:1", "alice")
        .await
        .unwrap();
    assert!(
        pair.sa.store.downloads().await.unwrap().is_empty(),
        "SA's download is cancelled"
    );
    assert!(session_ended(&pair, &session).await, "the device's token");
    drop((device, remote));
    // SB drops the library at its next sync.
    pair.refresh().await;
    match app
        .call(
            Some(&bob),
            Request::DownloadStatus {
                ids: vec![info.id.clone()],
            },
        )
        .await
    {
        Response::Ok(Reply::Downloads { downloads, .. }) => {
            assert!(
                matches!(downloads[0].state, DownloadState::Ready { .. })
                    && downloads[0].no_longer_available,
                "{downloads:?}"
            );
        }
        other => panic!("{other:?}"),
    }
    let (proxied, _) = read(&app, &bob, &info.id, 0, 4096).await;
    assert!(
        matches!(proxied, ResourceResponse::Error { .. }),
        "{proxied:?}"
    );
    pair.share("plex:1").await;
    pair.refresh().await;
    let again = prepared(&app, &bob, &alien).await;
    assert_ne!(again.id, info.id, "SA made a new one");
    assert!(matches!(
        settled(&app, &bob, &again.id).await.state,
        DownloadState::Ready { .. }
    ));
    pair.stop().await;
}

/// Bob follows a playlist on SB holding SA's track: what it holds names
/// SA's track, which his app downloads from SB as it would SB's own, with
/// a download token of its own on SA.
#[tokio::test]
async fn a_followed_playlist_with_sa_s_track_downloads_it() {
    use spoolframe_protocol::downloads::{Followed, FollowedTracks};
    use spoolframe_protocol::playlists::{Kinds, PlaylistEdit, Visibility};
    let pair = Pair::start_both(
        "fed-dl-music",
        ServerOptions {
            real_media: true,
            ..ServerOptions::default()
        },
        ServerOptions::default(),
    )
    .await;
    pair.sa.catalog.serve_music();
    crate::music_downloads::write_flac(&pair.sa, "32");
    pair.share("plex:2").await;
    pair.refresh().await;
    pair.sb.add_user("bob", "correct horse", None).await;
    let (app, bob) = bob_s_app(&pair).await;
    let song = pair.fed(&crate::music_downloads::track_id("32"));
    let made = match app
        .call(
            Some(&bob),
            Request::CreatePlaylist {
                name: "Road".into(),
                kinds: Kinds {
                    movies: false,
                    episodes: false,
                    tracks: true,
                },
                visibility: Visibility::Personal,
            },
        )
        .await
    {
        Response::Ok(Reply::PlaylistInfo(made)) => made,
        other => panic!("{other:?}"),
    };
    let added = app
        .call(
            Some(&bob),
            Request::EditPlaylist {
                id: made.id.clone(),
                revision: 0,
                edits: vec![PlaylistEdit::Add {
                    id: song.clone(),
                    before: None,
                }],
            },
        )
        .await;
    assert!(
        matches!(added, Response::Ok(Reply::PlaylistEdited { .. })),
        "{added:?}"
    );
    let changes = match app
        .call(
            Some(&bob),
            Request::MusicDownloadChanges {
                artists: Vec::new(),
                playlists: vec![made.id.clone()],
                liked: false,
                since: None,
            },
        )
        .await
    {
        Response::Ok(Reply::MusicDownloadChanges { changes, .. }) => changes,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        changes[0].followed,
        Followed::Playlist {
            id: made.id.clone()
        }
    );
    let FollowedTracks::Tracks { tracks } = &changes[0].tracks else {
        panic!("{changes:?}");
    };
    assert_eq!(tracks, std::slice::from_ref(&song));
    let info = prepared(&app, &bob, &song).await;
    let DownloadState::Ready { sha256: sum, .. } = settled(&app, &bob, &info.id).await.state else {
        panic!("not ready");
    };
    let bytes = read_from(&app, &bob, &info.id, 0, 1 << 20).await;
    assert_eq!(sha256(&bytes), sum);
    assert_eq!(
        bytes,
        std::fs::read(crate::music_downloads::track_file(&pair.sa, "32")).unwrap()
    );
    assert!(sb_holds_no_download(&pair));
    pair.stop().await;
}

/// More of SA's tracks than SA lets Bob hold download tokens for at once
/// download whole one after another, and none is cancelled: SB ends its
/// session on SA for each once SA says it is made, so finished downloads
/// hold no place under the cap (here 2, as 200 would be for a playlist of
/// hundreds of tracks).
#[tokio::test]
async fn finished_downloads_hold_no_place_under_the_token_cap() {
    let pair = Pair::start_both(
        "fed-dl-cap",
        ServerOptions {
            real_media: true,
            ..ServerOptions::default()
        },
        ServerOptions::default(),
    )
    .await;
    pair.sa.catalog.serve_music();
    let keys = ["32", "33", "35"];
    for key in keys {
        crate::music_downloads::write_flac(&pair.sa, key);
    }
    let sa = federation(&pair.sa);
    sa.set_limits(spoolframe_server::federation::Limits {
        download_tokens: spoolframe_server::store::federation_tokens::TokenLimits {
            per_user: 2,
            per_peer: 50,
        },
        ..sa.limits()
    });
    pair.share("plex:2").await;
    pair.refresh().await;
    pair.sb.add_user("bob", "correct horse", None).await;
    let (app, bob) = bob_s_app(&pair).await;
    for key in keys {
        let song = pair.fed(&crate::music_downloads::track_id(key));
        let info = prepared(&app, &bob, &song).await;
        assert!(
            matches!(
                settled(&app, &bob, &info.id).await.state,
                DownloadState::Ready { .. }
            ),
            "track {key}"
        );
    }
    pair.stop().await;
}

/// What Bob watched of SA's downloaded film while offline reaches SB, his
/// home server, as any progress on SA's items does; SA keeps nothing.
#[tokio::test]
async fn offline_watch_of_sa_s_film_reaches_sb_alone() {
    let pair = sharing_a_film("fed-dl-offline").await;
    let (app, bob) = bob_s_app(&pair).await;
    let alien = pair.fed(movie().id());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let synced = app
        .call(
            Some(&bob),
            Request::SyncOfflineWatch {
                device_now: now,
                entries: vec![OfflineWatch {
                    id: alien.clone(),
                    position_ms: Some(1_200_000),
                    watched: None,
                    at: now - 60,
                }],
            },
        )
        .await;
    let Response::Ok(Reply::OfflineWatchSynced { outcomes }) = synced else {
        panic!("{synced:?}");
    };
    assert_eq!(outcomes, [OfflineWatchOutcome::Applied]);
    let bob_id = pair.sb.store.user_by_name("bob").await.unwrap().unwrap().id;
    let states = pair
        .sb
        .store
        .watch_states(
            spoolframe_server::principal::Principal::LocalUser(bob_id),
            &[alien.to_string()],
        )
        .await
        .unwrap();
    assert_eq!(states.len(), 1);
    let remote = format!("remote:{}:{bob_id}", pair.sb.server_id)
        .parse()
        .unwrap();
    assert!(
        pair.sa
            .store
            .watch_states(remote, &[movie().id().to_string()])
            .await
            .unwrap()
            .is_empty()
    );
    pair.stop().await;
}

/// SB asks SA's planner for its device: SA honours a device named only
/// from a download token's session, which reaches no other device's plan.
#[tokio::test]
async fn sa_plans_the_download_for_the_device_sb_names() {
    let pair = sharing_a_film("fed-dl-device").await;
    let (app, bob) = bob_s_app(&pair).await;
    let alien = pair.fed(movie().id());
    // An Apple device's file is MP4.
    let iphone = pair.sb.client().await;
    let hello = Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "test".into(),
        client_version: "0".into(),
        platform: Platform::Ios,
        device_name: None,
        capabilities: None,
        experimental: vec![FEDERATION.into()],
        features: vec![DOWNLOADS.into()],
    };
    assert!(matches!(
        iphone.call(None, Request::Hello(hello)).await,
        Response::Ok(Reply::ServerInfo(_))
    ));
    let bob_on_iphone = iphone.login("bob", "correct horse").await;
    let on_iphone = prepared(&iphone, &bob_on_iphone, &alien).await;
    assert_eq!(
        on_iphone.file.map(|f| f.container),
        Some(spoolframe_types::Container::Mp4)
    );
    let on_linux = prepared(&app, &bob, &alien).await;
    assert_eq!(
        on_linux.file.map(|f| f.container),
        Some(spoolframe_types::Container::Matroska)
    );
    // A device named by an app's own session is not read.
    pair.sa.add_user("carol", "correct horse", None).await;
    let carol_s = app_on(&pair.sa).await;
    let carol = carol_s.login("carol", "correct horse").await;
    let request = Request::PrepareDownload {
        id: movie().id().clone(),
        source: DownloadSource::Original,
        tracks: DownloadTracks::All,
        device: Some(DownloadDevice {
            platform: Platform::Ios,
            capabilities: None,
        }),
    };
    match carol_s.call(Some(&carol), request).await {
        Response::Ok(Reply::Download(info)) => assert_eq!(
            info.file.map(|f| f.container),
            Some(spoolframe_types::Container::Matroska)
        ),
        other => panic!("{other:?}"),
    }
    pair.stop().await;
}

/// `device`'s endpoint, which is bound to loopback, connected to `server`
/// over loopback too: the same device on another server.
async fn loopback_from(server: &TestServer, device: &TestClient) -> TestClient {
    let addr = server.server.addr();
    let port = addr
        .ip_addrs()
        .find(|a| a.is_ipv4())
        .expect("an IPv4 address")
        .port();
    let at = iroh::EndpointAddr::new(addr.id).with_ip_addr(std::net::SocketAddr::from((
        std::net::Ipv4Addr::LOCALHOST,
        port,
    )));
    let conn = device
        .endpoint
        .connect(at, spoolframe_protocol::CLIENT_ALPN)
        .await
        .unwrap();
    TestClient {
        endpoint: device.endpoint.clone(),
        conn,
        instance: Default::default(),
        controls: Default::default(),
    }
}

/// Off SA's LAN, reading SA's file takes a place among SA's download
/// reads, apart from its plays' WAN streams: with SA's one WAN stream
/// taken by Bob's play, the read goes, and the play keeps its place.
#[tokio::test]
async fn reading_sa_s_file_off_its_lan_takes_no_play_s_wan_stream() {
    let loopback: Vec<spoolframe_server::announce::Network> = ["127.0.0.0/8", "::1/128"]
        .iter()
        .map(|n| n.parse().unwrap())
        .collect();
    let pair = Pair::start_both(
        "fed-dl-wan",
        ServerOptions {
            real_media: true,
            uplink: spoolframe_server::uplink::UplinkSettings {
                max_streams: Some(1),
                wan_prefixes: loopback,
                ..Default::default()
            },
            ..ServerOptions::default()
        },
        ServerOptions::default(),
    )
    .await;
    write_film(&pair.sa);
    pair.share("plex:1").await;
    pair.refresh().await;
    pair.sb.add_user("bob", "correct horse", None).await;
    let alien = pair.fed(movie().id());
    // The device reaches both servers over loopback alone.
    let on_sa = pair.sa.loopback_client().await;
    let app = loopback_from(&pair.sb, &on_sa).await;
    say_hello(&app).await;
    let bob = app.login("bob", "correct horse").await;
    let info = prepared(&app, &bob, &alien).await;
    settled(&app, &bob, &info.id).await;
    let granted = granted(&app, &bob, &alien, &info.id).await;
    let remote = granted.download.clone().unwrap();
    say_hello(&on_sa).await;
    let session = redeemed(&on_sa, &granted.token).await.session;
    // Bob plays the film on another device, off the LAN too.
    let player = pair.sa.loopback_client().await;
    player
        .hello_naming(crate::support::hls_capabilities(), &[FEDERATION])
        .await;
    let bob_id = pair.sb.store.user_by_name("bob").await.unwrap().unwrap().id;
    let play = federation(&pair.sb)
        .assert_play(bob_id, &alien, player.endpoint.id())
        .await
        .unwrap();
    let playing = redeemed(&player, &play.token).await.session;
    let started = player
        .call(
            Some(&playing),
            Request::StartPlayback {
                id: movie().id().clone(),
                selection: None,
                start_ms: None,
                bitmap: Default::default(),
                quality: Default::default(),
                display: None,
                capabilities: None,
                onward_share: None,
            },
        )
        .await;
    let Response::Ok(Reply::PlaybackStarted { handle, .. }) = started else {
        panic!("{started:?}");
    };
    let (read_now, _) = read(&on_sa, &session, &remote, 0, 4096).await;
    assert!(
        matches!(read_now, ResourceResponse::Ready { .. }),
        "{read_now:?}"
    );
    assert!(matches!(
        player
            .call(Some(&playing), Request::StopPlayback { handle })
            .await,
        Response::Ok(_)
    ));
    pair.stop().await;
}

/// `client`'s `Hello`, as [`app_on`] says it.
async fn say_hello(client: &TestClient) {
    let hello = Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "test".into(),
        client_version: "0".into(),
        platform: Platform::Linux,
        device_name: None,
        capabilities: None,
        experimental: vec![FEDERATION.into()],
        features: [DOWNLOADS, MUSIC_DOWNLOADS, PLAYLISTS, LIKED]
            .map(String::from)
            .to_vec(),
    };
    assert!(matches!(
        client.call(None, Request::Hello(hello)).await,
        Response::Ok(Reply::ServerInfo(_))
    ));
}

/// Unpaired, SB keeps Bob's record of SA's film, made and no longer
/// available, and SA keeps none of its tokens or its download for him.
#[tokio::test]
async fn after_unpairing_the_download_is_no_longer_available() {
    let pair = sharing_a_film("fed-dl-unpair").await;
    let (app, bob) = bob_s_app(&pair).await;
    let alien = pair.fed(movie().id());
    let info = prepared(&app, &bob, &alien).await;
    settled(&app, &bob, &info.id).await;
    granted(&app, &bob, &alien, &info.id).await;
    federation(&pair.sb)
        .unpair(&pair.sa_on_sb, "bob")
        .await
        .unwrap()
        .unwrap();
    until(|| async { pair.sa.store.downloads().await.unwrap().is_empty() }).await;
    match app
        .call(
            Some(&bob),
            Request::DownloadStatus {
                ids: vec![info.id.clone()],
            },
        )
        .await
    {
        Response::Ok(Reply::Downloads { downloads, .. }) => assert!(
            matches!(&downloads[0].state, DownloadState::Ready { .. })
                && downloads[0].no_longer_available,
            "{downloads:?}"
        ),
        other => panic!("{other:?}"),
    }
    pair.stop().await;
}

/// Bob's app, through the SDK, on SB, keeping its downloads in `dir`.
async fn sdk_app(
    pair: &Pair,
    dir: &std::path::Path,
) -> (
    spoolframe_client_sdk::Client,
    spoolframe_client_sdk::Session,
) {
    use spoolframe_client_sdk::{Client, ClientOptions, Device, Features, MemoryStorage};
    let device = Device {
        name: "Den PC".into(),
        client_name: "sdk-test".into(),
        client_version: "0".into(),
        platform: spoolframe_client_sdk::types::Platform::Linux,
        capabilities: Default::default(),
    };
    let options = ClientOptions {
        lan_discovery: false,
        relays: false,
        experimental: super::super::app::FEDERATION,
        features: Features {
            downloads: true,
            ..Features::default()
        },
        downloads_dir: Some(dir.to_path_buf()),
        ..ClientOptions::default()
    };
    let client = Client::open_with_options(
        std::sync::Arc::new(MemoryStorage::default()),
        device,
        options,
    )
    .await
    .unwrap();
    let sb = pair.sb.server.id().to_string();
    client
        .add_address(&sb, pair.sb.server.addr().ip_addrs().copied().collect())
        .unwrap();
    client.connect(&sb).await.unwrap();
    client.pin(&sb, true).await.unwrap();
    let session = client
        .sign_in(&sb, "bob", "correct horse", false)
        .await
        .unwrap();
    (client, session)
}

/// The SDK downloads SA's film from SB's listing whole, its SHA-256
/// checked: straight from SA when the device reaches it, with a download
/// token of its own there, and through SB when SB's record of SA names no
/// way there.
#[tokio::test]
async fn the_sdk_downloads_sa_s_film_from_sa_or_through_sb() {
    use spoolframe_client_sdk::{DownloadPhase, DownloadSource, DownloadTracks};
    for direct in [true, false] {
        let pair = sharing_a_film(&format!("fed-dl-sdk-{direct}")).await;
        if !direct {
            let pool = sqlx::SqlitePool::connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(pair.sb.dir.join(spoolframe_server::store::DB_FILE)),
            )
            .await
            .unwrap();
            sqlx::query("UPDATE federation_peers SET addrs = '[]', relays = '[]'")
                .execute(&pool)
                .await
                .unwrap();
        }
        let dir =
            spoolframe_test_dir::TestDir::new(&format!("spoolframe-fed-dl-sdk-{direct}")).await;
        let (client, bob) = sdk_app(&pair, &dir).await;
        let alien = pair.fed(movie().id());
        let started = client
            .downloads()
            .start(&bob, &alien, DownloadSource::Original, DownloadTracks::All)
            .await
            .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(120),
            client.downloads().run_until_done(),
        )
        .await
        .expect("the download never settled")
        .unwrap();
        let done = client.downloads().get(&started.key).unwrap();
        // Done is whole, its SHA-256 checked against SA's.
        assert_eq!(done.phase, DownloadPhase::Done, "direct {direct}: {done:?}");
        let kept = std::fs::metadata(done.file_path.unwrap()).unwrap().len();
        assert_eq!(Some(kept), done.size);
        // A token for the device's own reads was redeemed on SA only when
        // it reached SA.
        let tokens = pair
            .sa
            .store
            .federation_tokens_of(&client.id())
            .await
            .unwrap();
        assert_eq!(!tokens.is_empty(), direct, "direct {direct}: {tokens:?}");
        assert!(sb_holds_no_download(&pair));
        client.close().await;
        pair.stop().await;
    }
}
