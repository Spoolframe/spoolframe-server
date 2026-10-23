//! The HTTP playback port (PX-49): a playback over plain HTTP, under a
//! token a signed-in client asks for with the `http-playback`
//! experimental feature.

use crate::support;

use std::time::Duration;

use spoolframe_protocol::experimental::{HTTP_PLAYBACK, HttpPlaybackRefusal};
use spoolframe_protocol::{PlaybackHandle, Reply, Request, Response, Selection, SessionId};
use spoolframe_types::ClientCapabilities;
use support::{
    ServerOptions, TestClient, TestServer, fresh_dir, hls_capabilities, mkv_capabilities, movie,
};

/// A server with the port on whose devices' playbacks end `grace` after
/// they leave, and alice.
async fn graced(test: &str, grace: Duration) -> TestServer {
    let options = ServerOptions {
        disconnect_grace: grace,
        http_playback: spoolframe_server::http_playback::Settings {
            listen: Some("0.0.0.0:0".parse().unwrap()),
            ..Default::default()
        },
        ..ServerOptions::default()
    };
    let server = TestServer::start_with(fresh_dir(test).await, options).await;
    server.add_user("alice", "correct horse", None).await;
    server
}

/// The open playbacks' debug ids.
fn open(server: &TestServer) -> Vec<String> {
    server
        .debug
        .playbacks
        .snapshots()
        .into_iter()
        .map(|s| s.id)
        .collect()
}

/// A server with the port on, or off, and alice. On every interface, as
/// it is meant to run: the URL names whichever of this host's addresses
/// the client's path reached (a docker bridge's, even, from a loopback
/// client), and a port on loopback alone would not answer there.
async fn server(test: &str, on: bool, real_media: bool) -> TestServer {
    let options = ServerOptions {
        real_media,
        http_playback: spoolframe_server::http_playback::Settings {
            listen: on.then(|| "0.0.0.0:0".parse().unwrap()),
            ..Default::default()
        },
        ..ServerOptions::default()
    };
    let server = TestServer::start_with(fresh_dir(test).await, options).await;
    server.add_user("alice", "correct horse", None).await;
    server.add_user("bob", "battery staple", None).await;
    server
}

/// A client on loopback, the LAN as far as the server can tell, that
/// named `experimental` in its Hello and signed in as `user`.
async fn client(
    server: &TestServer,
    caps: ClientCapabilities,
    experimental: &[&str],
    user: (&str, &str),
) -> (TestClient, SessionId, Vec<String>) {
    let client = server.loopback_client().await;
    let info = client.hello_naming(caps, experimental).await;
    let session = client.login(user.0, user.1).await;
    (client, session, info.experimental)
}

async fn alice(server: &TestServer, caps: ClientCapabilities) -> (TestClient, SessionId) {
    let (client, session, on) =
        client(server, caps, &[HTTP_PLAYBACK], ("alice", "correct horse")).await;
    assert_eq!(on, vec![HTTP_PLAYBACK.to_owned()]);
    (client, session)
}

async fn start(client: &TestClient, session: &SessionId) -> PlaybackHandle {
    let started = client
        .call(
            Some(session),
            Request::StartPlayback {
                id: movie().id().clone(),
                selection: Some(Selection::default()),
                start_ms: None,
                bitmap: Default::default(),
                quality: Default::default(),
                display: None,
                capabilities: None,
                onward_share: None,
            },
        )
        .await;
    match started {
        Response::Ok(Reply::PlaybackStarted { handle, .. }) => handle,
        other => panic!("no playback: {other:?}"),
    }
}

/// What `HttpPlaybackUrl` answers for `handle`: the URL, or why not.
async fn url(
    client: &TestClient,
    session: &SessionId,
    handle: &PlaybackHandle,
) -> Result<String, HttpPlaybackRefusal> {
    let answer = client
        .call(
            Some(session),
            Request::HttpPlaybackUrl {
                handle: handle.clone(),
            },
        )
        .await;
    match answer {
        Response::Ok(Reply::HttpPlaybackUrl { url }) => Ok(url),
        Response::Ok(Reply::HttpPlaybackRefused { reason }) => Err(reason),
        other => panic!("not an answer to HttpPlaybackUrl: {other:?}"),
    }
}

/// A GET of `url` over real HTTP, with `range` if given: the status and the
/// body.
async fn get(url: &str, range: Option<&str>) -> (u16, Vec<u8>, Option<String>) {
    let mut request = reqwest::Client::new().get(url);
    if let Some(range) = range {
        request = request.header("range", range);
    }
    let response = request.send().await.expect("the port answers");
    let status = response.status().as_u16();
    let kind = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = response.bytes().await.expect("a whole body").to_vec();
    (status, body, kind)
}

/// `url` with its token's last character changed.
fn another_token(url: &str) -> String {
    let rest = url.strip_prefix("http://").unwrap();
    let (host, path) = rest.split_once('/').unwrap();
    let (token, name) = path.split_once('/').unwrap();
    let mut token = token.to_owned();
    let last = token.pop().unwrap();
    token.push(if last == '0' { '1' } else { '0' });
    format!("http://{host}/{token}/{name}")
}

#[tokio::test]
async fn with_the_port_off_a_url_is_refused_as_off_and_nothing_listens() {
    let server = server("http-off", false, false).await;
    server.write_media(4096);
    let (client, session, on) = client(
        &server,
        mkv_capabilities(),
        &[HTTP_PLAYBACK],
        ("alice", "correct horse"),
    )
    .await;
    assert!(on.is_empty(), "the answer names nothing: {on:?}");
    assert!(server.server.http_playback().is_none());
    let handle = start(&client, &session).await;
    assert_eq!(
        url(&client, &session, &handle).await,
        Err(HttpPlaybackRefusal::Off)
    );
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_client_that_did_not_name_it_is_refused_and_unknown_names_are_ignored() {
    let server = server("http-unnamed", true, false).await;
    server.write_media(4096);
    let (quiet, session, on) =
        client(&server, mkv_capabilities(), &[], ("alice", "correct horse")).await;
    assert!(on.is_empty(), "{on:?}");
    let handle = start(&quiet, &session).await;
    assert_eq!(
        url(&quiet, &session, &handle).await,
        Err(HttpPlaybackRefusal::Off)
    );
    let (curious, _, on) = client(
        &server,
        mkv_capabilities(),
        &["teleport", HTTP_PLAYBACK],
        ("bob", "battery staple"),
    )
    .await;
    assert_eq!(on, vec![HTTP_PLAYBACK.to_owned()]);
    quiet.close().await;
    curious.close().await;
    server.stop().await;
}

#[tokio::test]
async fn direct_play_is_fetched_whole_and_in_ranges_over_http() {
    let server = server("http-direct", true, false).await;
    let file = server.write_media(1 << 20);
    let (client, session) = alice(&server, mkv_capabilities()).await;
    let handle = start(&client, &session).await;
    let url = url(&client, &session, &handle).await.expect("a URL");
    let port = server.server.http_playback().unwrap().addr().port();
    let host = url
        .strip_prefix("http://")
        .and_then(|rest| rest.split_once(&format!(":{port}/")))
        .map(|(host, _)| host)
        .expect("the port's own");
    assert!(
        host.parse::<std::net::Ipv4Addr>().is_ok() && url.ends_with("/file"),
        "{url}"
    );
    let over_http = || server.debug.playbacks.snapshots()[0].over_http;
    assert!(!over_http(), "a URL given is not a playback played");
    let (status, body, kind) = get(&url, None).await;
    assert_eq!(status, 200);
    assert_eq!(body, file);
    assert_eq!(kind.as_deref(), Some("video/x-matroska"));
    assert!(over_http(), "the activity view says so");
    let (status, body, _) = get(&url, Some("bytes=1000-1999")).await;
    assert_eq!(status, 206);
    assert_eq!(body, file[1000..2000]);
    // Asked again: the same live token.
    assert_eq!(
        self::url(&client, &session, &handle).await.as_deref(),
        Ok(url.as_str())
    );
    client.close().await;
    server.stop().await;
}

/// A phone that reached the server over IPv6, and a port on `0.0.0.0`: the
/// URL names an IPv4 address of the same interface, one the port listens
/// on, not the IPv6 one the phone came in on, which nothing answers at.
#[tokio::test]
async fn a_client_over_ipv6_is_named_an_ipv4_address_the_port_listens_on() {
    // In a namespace of its own, so ::1 is there even where the host or
    // CI's container has no IPv6.
    if !support::private_netns(
        module_path!(),
        "a_client_over_ipv6_is_named_an_ipv4_address_the_port_listens_on",
    ) {
        return;
    }
    let server = server("http-over-v6", true, false).await;
    let file = server.write_media(4096);
    let client = server
        .loopback_client_at(std::net::Ipv6Addr::LOCALHOST.into())
        .await;
    let info = client
        .hello_naming(mkv_capabilities(), &[HTTP_PLAYBACK])
        .await;
    assert_eq!(info.experimental, vec![HTTP_PLAYBACK.to_owned()]);
    let session = client.login("alice", "correct horse").await;
    let handle = start(&client, &session).await;
    let url = url(&client, &session, &handle).await.expect("a URL");
    let host = url
        .strip_prefix("http://")
        .and_then(|rest| rest.split_once(':'))
        .map(|(host, _)| host)
        .expect("a host");
    assert!(host.parse::<std::net::Ipv4Addr>().is_ok(), "{url}");
    let (status, body, _) = get(&url, None).await;
    assert_eq!(status, 200);
    assert_eq!(body, file);
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn another_users_playback_or_none_is_no_such_playback() {
    let server = server("http-not-yours", true, false).await;
    server.write_media(4096);
    let (alice, alice_session) = alice(&server, mkv_capabilities()).await;
    let handle = start(&alice, &alice_session).await;
    let (bob, bob_session, _) = client(
        &server,
        mkv_capabilities(),
        &[HTTP_PLAYBACK],
        ("bob", "battery staple"),
    )
    .await;
    assert_eq!(
        url(&bob, &bob_session, &handle).await,
        Err(HttpPlaybackRefusal::NoSuchPlayback)
    );
    let none = PlaybackHandle("00".repeat(16));
    assert_eq!(
        url(&alice, &alice_session, &none).await,
        Err(HttpPlaybackRefusal::NoSuchPlayback)
    );
    alice.close().await;
    bob.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_changed_token_or_another_path_is_not_found() {
    let server = server("http-wrong-token", true, false).await;
    server.write_media(4096);
    let (client, session) = alice(&server, mkv_capabilities()).await;
    let handle = start(&client, &session).await;
    let url = url(&client, &session, &handle).await.expect("a URL");
    assert_eq!(get(&url, None).await.0, 200);
    assert_eq!(get(&another_token(&url), None).await.0, 404);
    let root = url.split('/').take(3).collect::<Vec<_>>().join("/");
    assert_eq!(get(&format!("{root}/"), None).await.0, 404);
    assert_eq!(
        get(&url.replace("/file", "/master.m3u8"), None).await.0,
        404
    );
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_stopped_playback_s_url_is_not_found() {
    let server = server("http-stopped", true, false).await;
    server.write_media(4096);
    let (client, session) = alice(&server, mkv_capabilities()).await;
    // Its client stops it.
    let handle = start(&client, &session).await;
    let url = url(&client, &session, &handle).await.expect("a URL");
    assert!(matches!(
        client
            .call(Some(&session), Request::StopPlayback { handle })
            .await,
        Response::Ok(Reply::Done)
    ));
    assert_eq!(get(&url, None).await.0, 404);
    // An admin stops it (P3-15).
    let handle = start(&client, &session).await;
    let url = self::url(&client, &session, &handle).await.expect("a URL");
    assert_eq!(get(&url, None).await.0, 200);
    let id = server.debug.playbacks.snapshots()[0].id.clone();
    assert!(server.debug.playbacks.stop(&id));
    assert_eq!(get(&url, None).await.0, 404);
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_token_nothing_asks_for_ends_and_its_playback_plays_on() {
    let server = server("http-idle", true, false).await;
    let file = server.write_media(4096);
    let (client, session) = alice(&server, mkv_capabilities()).await;
    let handle = start(&client, &session).await;
    let url = url(&client, &session, &handle).await.expect("a URL");
    let port = server.server.http_playback().unwrap();
    // Asked for just now: kept.
    server
        .clock
        .advance(spoolframe_server::http_playback::IDLE / 2);
    assert_eq!(get(&url, None).await.0, 200);
    server
        .clock
        .advance(spoolframe_server::http_playback::IDLE / 2);
    port.sweep();
    assert_eq!(get(&url, None).await.0, 200, "asked for within the limit");
    // Its own client goes on reading it; nothing asks for the URL.
    server
        .clock
        .advance(spoolframe_server::http_playback::IDLE / 2);
    client.read(&session, &handle, 0, 16).await;
    server
        .clock
        .advance(spoolframe_server::http_playback::IDLE / 2 + Duration::from_secs(1));
    port.sweep();
    assert_eq!(get(&url, None).await.0, 404);
    // The playback is its client's still, and a new URL serves it.
    let (_, bytes) = client.read(&session, &handle, 0, 16).await;
    assert_eq!(bytes, file[..16]);
    let again = self::url(&client, &session, &handle).await.expect("a URL");
    assert_ne!(again, url);
    assert_eq!(get(&again, None).await.1, file);
    client.close().await;
    server.stop().await;
}

/// The lines of `playlist` that name a resource.
fn names(playlist: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for line in playlist.lines() {
        if let Some(at) = line.find("URI=\"") {
            let rest = &line[at + 5..];
            names.push(rest[..rest.find('"').unwrap()].to_owned());
        } else if !line.starts_with('#') && !line.trim().is_empty() {
            names.push(line.trim().to_owned());
        }
    }
    names
}

#[tokio::test]
async fn hls_is_fetched_over_http_from_the_master_playlist_down() {
    let server = server("http-hls", true, true).await;
    server.generate_media(12, "libx264", "ac3");
    let (client, session) = alice(&server, hls_capabilities()).await;
    let handle = start(&client, &session).await;
    let url = url(&client, &session, &handle).await.expect("a URL");
    assert!(url.ends_with("/master.m3u8"), "{url}");
    let base = url.trim_end_matches("master.m3u8");
    let (status, master, kind) = get(&url, None).await;
    assert_eq!(status, 200);
    assert_eq!(kind.as_deref(), Some("application/vnd.apple.mpegurl"));
    let master = String::from_utf8(master).unwrap();
    assert!(master.starts_with("#EXTM3U"), "{master}");
    let media = names(&master)
        .into_iter()
        .find(|n| n.ends_with("media.m3u8"))
        .expect("a variant");
    let (status, playlist, _) = get(&format!("{base}{media}"), None).await;
    assert_eq!(status, 200);
    let playlist = String::from_utf8(playlist).unwrap();
    let prefix = media.trim_end_matches("media.m3u8");
    let resources = names(&playlist);
    let init = resources
        .iter()
        .find(|n| n.ends_with("init.mp4"))
        .cloned()
        .unwrap_or_else(|| format!("{prefix}init.mp4"));
    let first = resources
        .iter()
        .find(|n| n.ends_with(".m4s"))
        .expect("a segment")
        .clone();
    let (status, init, _) = get(&format!("{base}{init}"), None).await;
    assert_eq!(status, 200);
    assert!(!init.is_empty());
    let (status, segment, kind) = get(&format!("{base}{first}"), None).await;
    assert_eq!(status, 200);
    assert_eq!(kind.as_deref(), Some("video/iso.segment"));
    assert_eq!(
        segment,
        std::fs::read(server.job_dir(&handle, 0).join(&first)).unwrap(),
        "the segment the encode wrote"
    );
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_conversion_read_as_one_growing_file_is_not_served() {
    let server = server("http-progressive", true, true).await;
    server.generate_media(6, "libx264", "ac3");
    let (client, session) = alice(&server, support::progressive_capabilities()).await;
    let handle = start(&client, &session).await;
    assert_eq!(
        url(&client, &session, &handle).await,
        Err(HttpPlaybackRefusal::Unsupported)
    );
    client.close().await;
    server.stop().await;
}

/// The SDK's side of it (PX-49): the app names the feature in its
/// options, learns whether the server turned it on, and asks for a URL.
mod sdk {
    use super::*;

    use std::sync::Arc;

    use spoolframe_client_sdk::{
        Client, ClientOptions, Device, Experimental, HttpPlayback, MemoryStorage, PlaybackOptions,
        Session,
    };

    /// An app on loopback that asks for `experimental`, signed in as alice.
    async fn signed_in(server: &TestServer, experimental: Experimental) -> (Client, Session) {
        let options = ClientOptions {
            lan_discovery: false,
            experimental,
            ..ClientOptions::default()
        };
        let device = Device {
            name: "phone".into(),
            client_name: "sdk-test".into(),
            client_version: "0".into(),
            platform: spoolframe_client_sdk::types::Platform::Ios,
            capabilities: mkv_capabilities(),
        };
        let client = Client::open_with_options(Arc::new(MemoryStorage::default()), device, options)
            .await
            .unwrap();
        let port = server
            .server
            .addr()
            .ip_addrs()
            .find(|addr| addr.is_ipv4())
            .expect("an IPv4 address")
            .port();
        let id = server.server.id().to_string();
        client
            .add_address(&id, vec![([127, 0, 0, 1], port).into()])
            .unwrap();
        client.connect(&id).await.unwrap();
        client.pin(&id, true).await.unwrap();
        let session = client
            .sign_in(&id, "alice", "correct horse", false)
            .await
            .unwrap();
        (client, session)
    }

    const ASKING: Experimental = Experimental {
        http_playback: true,
        federation: false,
    };

    #[tokio::test]
    async fn an_app_that_asks_gets_a_url_that_fetches() {
        let server = server("http-sdk", true, false).await;
        let file = server.write_media(64 << 10);
        let (_client, session) = signed_in(&server, ASKING).await;
        assert_eq!(session.experimental(), ASKING);
        let playback = session
            .open_playback(movie().id(), None, PlaybackOptions::default())
            .await
            .unwrap();
        let HttpPlayback::Url(url) = playback.http_playback_url().await.unwrap() else {
            panic!("no URL");
        };
        assert_eq!(get(&url, None).await.1, file);
        // The session asked directly: the same live token.
        let handle = playback.snapshot().handle;
        assert_eq!(
            session.http_playback_url(&handle).await.unwrap(),
            HttpPlayback::Url(url)
        );
        playback.close().await.unwrap();
        server.stop().await;
    }

    #[tokio::test]
    async fn an_app_that_does_not_ask_is_told_off() {
        let server = server("http-sdk-quiet", true, false).await;
        server.write_media(4096);
        let (_client, session) = signed_in(&server, Experimental::default()).await;
        assert_eq!(session.experimental(), Experimental::default());
        let playback = session
            .open_playback(movie().id(), None, PlaybackOptions::default())
            .await
            .unwrap();
        assert_eq!(
            playback.http_playback_url().await.unwrap(),
            HttpPlayback::Refused(HttpPlaybackRefusal::Off)
        );
        playback.close().await.unwrap();
        server.stop().await;
    }

    #[tokio::test]
    async fn with_the_port_off_an_app_that_asks_is_told_off() {
        let server = server("http-sdk-off", false, false).await;
        server.write_media(4096);
        let (_client, session) = signed_in(&server, ASKING).await;
        assert_eq!(session.experimental(), Experimental::default());
        let playback = session
            .open_playback(movie().id(), None, PlaybackOptions::default())
            .await
            .unwrap();
        assert_eq!(
            playback.http_playback_url().await.unwrap(),
            HttpPlayback::Refused(HttpPlaybackRefusal::Off)
        );
        playback.close().await.unwrap();
        server.stop().await;
    }
}

/// A phone that AirPlays and then locks: its connection goes, and the
/// playback the Apple TV reads over HTTP stays past the grace. One it
/// holds no URL for ends after the grace as before.
#[tokio::test]
async fn a_device_that_leaves_keeps_the_playback_served_over_http() {
    let grace = Duration::from_millis(300);
    let server = graced("http-device-gone", grace).await;
    let file = server.write_media(4096);
    let (phone, session) = alice(&server, mkv_capabilities()).await;
    let airplayed = start(&phone, &session).await;
    let url = url(&phone, &session, &airplayed).await.expect("a URL");
    start(&phone, &session).await;
    assert_eq!(open(&server).len(), 2);
    phone.close().await;
    tokio::time::sleep(grace * 4).await;
    assert_eq!(open(&server).len(), 1, "the one with no URL ended");
    assert_eq!(get(&url, None).await.1, file, "still served");
    tokio::time::sleep(grace * 2).await;
    assert_eq!(open(&server).len(), 1, "and kept");
    server.stop().await;
}

/// Kept past its device's leaving, the playback still ends by idling out
/// with its URL.
#[tokio::test]
async fn a_left_device_s_playback_ends_once_its_url_idles_out() {
    let grace = Duration::from_millis(300);
    let server = graced("http-device-gone-idle", grace).await;
    server.write_media(4096);
    let (phone, session) = alice(&server, mkv_capabilities()).await;
    let handle = start(&phone, &session).await;
    let url = url(&phone, &session, &handle).await.expect("a URL");
    phone.close().await;
    tokio::time::sleep(grace * 4).await;
    assert_eq!(open(&server).len(), 1);
    server
        .clock
        .advance(spoolframe_server::playback::IDLE + Duration::from_secs(1));
    server.server.http_playback().unwrap().sweep();
    server.debug.playbacks.sweep();
    assert_eq!(get(&url, None).await.0, 404);
    assert!(open(&server).is_empty());
    server.stop().await;
}

/// Or by an admin stopping it (P3-15).
#[tokio::test]
async fn a_left_device_s_playback_ends_when_an_admin_stops_it() {
    let grace = Duration::from_millis(300);
    let server = graced("http-device-gone-stop", grace).await;
    server.write_media(4096);
    let (phone, session) = alice(&server, mkv_capabilities()).await;
    let handle = start(&phone, &session).await;
    let url = url(&phone, &session, &handle).await.expect("a URL");
    phone.close().await;
    tokio::time::sleep(grace * 4).await;
    let kept = open(&server);
    assert_eq!(kept.len(), 1);
    assert!(server.debug.playbacks.stop(&kept[0]));
    assert_eq!(get(&url, None).await.0, 404);
    assert!(open(&server).is_empty());
    server.stop().await;
}

/// A TV paused for longer than the old 10 minutes keeps its URL: the
/// token lives as long as an idle playback does.
#[tokio::test]
async fn a_url_lives_as_long_as_an_idle_playback() {
    assert_eq!(
        spoolframe_server::http_playback::IDLE,
        spoolframe_server::playback::IDLE
    );
    let server = server("http-long-pause", true, false).await;
    server.write_media(4096);
    let (client, session) = alice(&server, mkv_capabilities()).await;
    let handle = start(&client, &session).await;
    let url = url(&client, &session, &handle).await.expect("a URL");
    server.clock.advance(Duration::from_secs(15 * 60));
    server.server.http_playback().unwrap().sweep();
    assert_eq!(get(&url, None).await.0, 200);
    client.close().await;
    server.stop().await;
}

/// A phone that comes back after the grace (unlocked) finds its playback
/// under the same handle, and the URL it gave the TV still works: a
/// playback is its session's and device's, not its connection's.
#[tokio::test]
async fn a_device_back_after_the_grace_finds_the_same_handle_and_url() {
    let grace = Duration::from_millis(300);
    let server = graced("http-device-back", grace).await;
    let file = server.write_media(4096);
    let (phone, session) = alice(&server, mkv_capabilities()).await;
    let handle = start(&phone, &session).await;
    let url = url(&phone, &session, &handle).await.expect("a URL");
    phone.conn.close(0u32.into(), b"locked");
    tokio::time::sleep(grace * 4).await;
    let back = phone.reconnect(&server).await;
    let (_, bytes) = back.read(&session, &handle, 0, 16).await;
    assert_eq!(bytes, file[..16]);
    assert_eq!(get(&url, None).await.1, file);
    back.close().await;
    server.stop().await;
}
