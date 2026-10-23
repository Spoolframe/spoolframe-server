//! `GET /metrics` on the admin listener (P3-14): off by default, and when
//! on, only for the addresses `admin.metrics_from` names.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use reqwest::StatusCode;
use spoolframe_admin_api::{AdminApiClient, MeApiClient, NewUser, Role, error_code};
use spoolframe_admin_api::{LogLevel, LogQuery};
use spoolframe_protocol::{
    Error, MediaReadResponse, PlaybackHandle, Reply, Request, Response, SessionId,
};
use spoolframe_server::admin::{self, Admin, Origins, SetupCode, TrustedProxies};
use spoolframe_server::auth::Auth;
use spoolframe_server::logs::{LogRing, RingLayer};
use spoolframe_server::metrics::Metrics;
use spoolframe_server::store::Store;

use crate::admin::{CODE, admin, anonymous, code, creds, setup, state_dir, with_token};
use crate::support::{
    ServerOptions, TestClient, TestServer, fresh_dir, mkv_capabilities, movie,
    progressive_capabilities,
};

/// An admin listener with `metrics`, believing a proxy on loopback about
/// whom it carries when `proxied`.
async fn serve(
    test: &str,
    metrics: Option<Arc<Metrics>>,
    proxied: bool,
) -> (SocketAddr, spoolframe_test_dir::TestDir) {
    serve_with(test, metrics, None, proxied).await
}

async fn serve_with(
    test: &str,
    metrics: Option<Arc<Metrics>>,
    logs: Option<Arc<LogRing>>,
    proxied: bool,
) -> (SocketAddr, spoolframe_test_dir::TestDir) {
    let dir = state_dir(test).await;
    let auth = Auth::new(Store::open(dir.as_ref()).await.unwrap());
    let proxies = if proxied {
        TrustedProxies::new(&["127.0.0.1"]).unwrap()
    } else {
        TrustedProxies::default()
    };
    let (addr, handle) = admin::serve(
        "127.0.0.1:0".parse().unwrap(),
        Admin {
            metrics,
            logs,
            ..admin(auth, Some(SetupCode::new(CODE)))
        },
        Origins::default(),
        proxies,
    )
    .await
    .unwrap();
    std::mem::forget(handle);
    (addr, dir)
}

/// Metrics on for `from`'s ranges, written "a, b" for short.
fn on(from: &str) -> Arc<Metrics> {
    let metrics = Arc::new(Metrics::default());
    metrics.access().set(
        true,
        admin::AddressRanges::new(&from.split(", ").collect::<Vec<_>>()).unwrap(),
    );
    metrics
}

async fn get(addr: SocketAddr, forwarded_for: Option<&str>) -> reqwest::Response {
    let mut request = reqwest::Client::new().get(format!("http://{addr}/metrics"));
    if let Some(ip) = forwarded_for {
        request = request.header("x-forwarded-for", ip);
    }
    request.send().await.unwrap()
}

#[tokio::test]
async fn metrics_are_not_found_while_off() {
    let (addr, _dir) = serve("metrics-off", Some(Arc::new(Metrics::default())), false).await;
    assert_eq!(get(addr, None).await.status(), StatusCode::NOT_FOUND);
    let (addr, _dir) = serve("metrics-none", None, false).await;
    assert_eq!(get(addr, None).await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn loopback_reads_prometheus_text() {
    let (addr, _dir) = serve("metrics-loopback", Some(on("127.0.0.0/8, ::1")), false).await;
    let response = get(addr, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"],
        "text/plain; version=0.0.4; charset=utf-8"
    );
    let text = response.text().await.unwrap();
    assert!(
        text.contains("# TYPE spoolframe_process_uptime_seconds gauge\n"),
        "{text}"
    );
}

#[tokio::test]
async fn an_address_not_listed_is_refused() {
    let (addr, _dir) = serve("metrics-elsewhere", Some(on("10.0.0.0/8")), false).await;
    assert_eq!(get(addr, None).await.status(), StatusCode::FORBIDDEN);
}

/// A proxy on this machine carries the Internet: what it forwards for is
/// the caller, not loopback.
#[tokio::test]
async fn behind_a_trusted_proxy_the_forwarded_address_is_the_caller() {
    let (addr, _dir) = serve(
        "metrics-proxied",
        Some(on("127.0.0.0/8, ::1, 198.51.100.0/24")),
        true,
    )
    .await;
    assert_eq!(
        get(addr, Some("192.0.2.7")).await.status(),
        StatusCode::FORBIDDEN
    );
    // Nobody named: nothing says who it is.
    assert_eq!(get(addr, None).await.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        get(addr, Some("198.51.100.9")).await.status(),
        StatusCode::OK
    );
}

/// The number in family `name` with exactly `labels`.
fn value(metrics: &Metrics, name: &str, labels: &[(&str, &str)]) -> f64 {
    let families = metrics.families();
    let family = families
        .iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("no {name}"));
    family
        .value(labels)
        .unwrap_or_else(|| panic!("no {name}{labels:?} in {:?}", family.samples))
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let within = Duration::from_secs(20);
    tokio::time::timeout(within, async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("not within {within:?}: {what}"));
}

async fn start(client: &TestClient, session: &SessionId) -> PlaybackHandle {
    let response = client
        .call(
            Some(session),
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
    match response {
        Response::Ok(Reply::PlaybackStarted { handle, .. }) => handle,
        other => panic!("playback did not start: {other:?}"),
    }
}

#[tokio::test]
async fn a_direct_playback_its_reads_and_its_end_move_the_counts() {
    let server = TestServer::start("direct").await;
    server.write_media(1 << 20);
    server.add_user("alice", "correct horse", None).await;
    let client = server.client().await;
    client.hello_with(None, Some(mkv_capabilities())).await;
    let session = client.login("alice", "correct horse").await;
    let metrics = &server.metrics;
    assert_eq!(
        value(metrics, "spoolframe_connections_open", &[("kind", "app")]),
        1.0
    );
    let path = |path| value(metrics, "spoolframe_connection_paths", &[("path", path)]);
    assert_eq!((path("direct"), path("relay")), (1.0, 0.0));

    let handle = start(&client, &session).await;
    let active = |plan| value(metrics, "spoolframe_playbacks_active", &[("plan", plan)]);
    assert_eq!((active("direct"), active("transcode")), (1.0, 0.0));
    assert_eq!(
        value(metrics, "spoolframe_playbacks_started_total", &[]),
        1.0
    );
    let (response, bytes) = client.read(&session, &handle, 0, 4096).await;
    assert!(matches!(response, MediaReadResponse::Ok(_)));
    assert_eq!(bytes.len(), 4096);
    assert_eq!(value(metrics, "spoolframe_media_reads_total", &[]), 1.0);
    assert_eq!(
        value(metrics, "spoolframe_media_read_bytes_total", &[]),
        4096.0
    );

    let stopped = client
        .call(
            Some(&session),
            Request::StopPlayback {
                handle: handle.clone(),
            },
        )
        .await;
    assert!(matches!(stopped, Response::Ok(_)), "{stopped:?}");
    assert_eq!(active("direct"), 0.0);
    assert_eq!(value(metrics, "spoolframe_playbacks_ended_total", &[]), 1.0);
    // Read on after the end: the client is told, and that is counted.
    let (response, _) = client.read(&session, &handle, 0, 4096).await;
    assert!(
        matches!(response, MediaReadResponse::Err(Error::PlaybackExpired)),
        "{response:?}"
    );
    let errors = |error| {
        value(
            metrics,
            "spoolframe_playback_errors_total",
            &[("error", error)],
        )
    };
    assert_eq!((errors("expired"), errors("failed")), (1.0, 0.0));

    // Sessions are read from the store at a refresh, never at a scrape.
    metrics.refresh().await;
    let sessions = metrics.sessions();
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    // By kind only: a label per user would grow with the users.
    assert_eq!(
        value(metrics, "spoolframe_sessions", &[("kind", "app")]),
        1.0
    );
    client.close().await;
    server.stop().await;
}

/// An H.264 + AC-3 file for a device that plays AAC: its audio is
/// encoded, so the playback is a transcode, and its encode runs.
#[tokio::test]
async fn a_conversion_moves_the_transcode_and_encode_counts() {
    let server = TestServer::start_with(
        fresh_dir("conversion").await,
        ServerOptions {
            real_media: true,
            ..ServerOptions::default()
        },
    )
    .await;
    server.generate_media(4, "libx264", "ac3");
    server.add_user("alice", "correct horse", None).await;
    let client = server.client().await;
    client
        .hello_with(None, Some(progressive_capabilities()))
        .await;
    let session = client.login("alice", "correct horse").await;
    let metrics = server.metrics.clone();
    start(&client, &session).await;
    assert_eq!(
        value(
            &metrics,
            "spoolframe_playbacks_active",
            &[("plan", "transcode")]
        ),
        1.0
    );
    until("ffmpeg reports encoded time", || {
        value(&metrics, "spoolframe_conversion_output_seconds_total", &[]) > 0.0
    })
    .await;
    client.close().await;
    server.stop().await;
}

/// The same audio-only conversion takes a quarter of the encode budget:
/// the metrics, the admin status (and so the Server page and `spoolframe
/// status`) say so, and count the running encode by its kind.
#[tokio::test]
async fn the_encode_budget_s_use_is_in_the_metrics_and_the_status() {
    let server = TestServer::start_with(
        fresh_dir("budget").await,
        ServerOptions {
            real_media: true,
            ..ServerOptions::default()
        },
    )
    .await;
    // Longer than the read-ahead: the encode is held back, still running.
    server.generate_media(30, "libx264", "ac3");
    server.add_user("alice", "correct horse", None).await;
    let client = server.client().await;
    client
        .hello_with(None, Some(progressive_capabilities()))
        .await;
    let session = client.login("alice", "correct horse").await;
    let metrics = server.metrics.clone();
    start(&client, &session).await;
    assert_eq!(value(&metrics, "spoolframe_encode_budget_used", &[]), 0.25);
    assert_eq!(value(&metrics, "spoolframe_encode_budget_max", &[]), 2.0);
    let running = |kind| {
        value(
            &metrics,
            "spoolframe_conversions_running",
            &[("kind", kind)],
        )
    };
    until("the audio encode runs", || running("audio") == 1.0).await;
    assert_eq!(
        (running("remux"), running("gpu"), running("software")),
        (0.0, 0.0, 0.0)
    );

    let dir = state_dir("budget-status").await;
    let auth = Auth::new(Store::open(dir.as_ref()).await.unwrap());
    let status = admin::StatusSource::new(
        server.name.clone(),
        server.server_id.clone(),
        crate::support::transcoding(),
    )
    .with_jobs(server.jobs.clone());
    let (addr, handle) = admin::serve(
        "127.0.0.1:0".parse().unwrap(),
        Admin {
            status: Arc::new(status),
            ..admin(auth, Some(SetupCode::new(CODE)))
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
    let budget = with_token(addr, &root.token)
        .await
        .server_status()
        .await
        .unwrap()
        .transcoding
        .budget
        .expect("the budget, with jobs");
    assert_eq!(
        budget,
        spoolframe_admin_api::EncodeBudget {
            used_quarters: 1,
            max_quarters: 8,
            remuxes: 0,
            max_remux: 32,
            gpu_jobs: 0,
            max_gpu_jobs: 3,
        }
    );
    assert_eq!(budget.used_of_max(), "0.25 of 2");
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn admin_sockets_are_counted_while_open() {
    let metrics = Arc::new(Metrics::default());
    let (addr, _dir) = serve("metrics-sockets", Some(metrics.clone()), false).await;
    let web = || value(&metrics, "spoolframe_connections_open", &[("kind", "web")]);
    assert_eq!(web(), 0.0);
    let client = anonymous(addr).await;
    // A call makes sure the socket is up on the server's side.
    let _ = client.who_am_i().await;
    assert_eq!(web(), 1.0);
    drop(client);
    until("the socket is closed", || web() == 0.0).await;
}

/// The admin listener's root admin, and alice, a consumer.
async fn root_and_consumer(
    addr: SocketAddr,
) -> (
    jsonrpsee::ws_client::WsClient,
    jsonrpsee::ws_client::WsClient,
) {
    let root = anonymous(addr)
        .await
        .setup(setup(CODE, "root", "correct horse"))
        .await
        .unwrap();
    let root = with_token(addr, &root.token).await;
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
    (root, with_token(addr, &alice.token).await)
}

#[tokio::test]
async fn the_activity_is_the_last_samples_and_now_for_admins_only() {
    let metrics = Arc::new(Metrics::default());
    let (addr, _dir) = serve("metrics-activity", Some(metrics.clone()), false).await;
    let (root, alice) = root_and_consumer(addr).await;
    assert_eq!(
        code(alice.activity().await.unwrap_err()),
        error_code::FORBIDDEN
    );
    metrics.sample().await;
    let activity = root.activity().await.unwrap();
    assert_eq!(activity.sample_secs, 10);
    // One sample is only what the next one's rates are from.
    assert!(activity.history.is_empty(), "{:?}", activity.history);
    metrics.sample().await;
    let activity = root.activity().await.unwrap();
    assert_eq!(activity.history.len(), 1);
    // root's and alice's sockets at least (the ones they signed in on may
    // not be closed yet), and their sessions, as last sampled.
    assert!(activity.now.connections_web >= 2, "{:?}", activity.now);
    let mut sessions: Vec<_> = activity
        .sessions
        .iter()
        .map(|s| (s.username.as_str(), s.app, s.web))
        .collect();
    sessions.sort();
    assert_eq!(sessions, [("alice", 0, 1), ("root", 0, 1)]);
    assert!(
        activity
            .sessions
            .iter()
            .all(|s| s.user_id.starts_with("local:"))
    );
}

/// The Logs page (P3-14): a WARN raised here is there, the page filters
/// by level and text and follows new lines, and only admins see it.
#[tokio::test]
async fn the_last_log_lines_are_for_admins_filtered_and_followed() {
    use tracing_subscriber::layer::SubscriberExt as _;
    let ring = Arc::new(LogRing::new(3));
    let (addr, _dir) = serve_with("metrics-logs", None, Some(ring.clone()), false).await;
    let (root, alice) = root_and_consumer(addr).await;
    assert_eq!(
        code(alice.logs(LogQuery::default()).await.unwrap_err()),
        error_code::FORBIDDEN
    );
    // Only this thread's events: the server's own go elsewhere.
    let here = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(RingLayer(ring.clone())),
    );
    tracing::info!("the library is ready");
    tracing::warn!(server = "plex", "Plex server unavailable");
    let warnings = root
        .logs(LogQuery {
            level: Some(LogLevel::Warn),
            ..LogQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(warnings.lines.len(), 1, "{warnings:?}");
    assert_eq!(warnings.lines[0].level, LogLevel::Warn);
    assert_eq!(
        warnings.lines[0].text,
        "Plex server unavailable server=\"plex\""
    );
    let library = root
        .logs(LogQuery {
            text: Some("LIBRARY".into()),
            ..LogQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(library.lines.len(), 1);
    // Follow: only what came after.
    let all = root.logs(LogQuery::default()).await.unwrap();
    for i in 0..5 {
        tracing::info!("line {i}");
    }
    drop(here);
    let next = root
        .logs(LogQuery {
            after: Some(all.next),
            ..LogQuery::default()
        })
        .await
        .unwrap();
    let texts: Vec<_> = next.lines.iter().map(|l| l.text.as_str()).collect();
    // The buffer stays at its size: three lines, the two before them gone.
    assert_eq!(texts, ["line 2", "line 3", "line 4"]);
    assert!(next.missed);
    assert_eq!(next.capacity, 3);
}

/// The Settings › Telemetry tab's status (P3-14): admins only, refused
/// scrapes by address, and what an export would carry while it is off.
#[tokio::test]
async fn the_telemetry_status_says_who_was_refused_and_what_an_export_carries() {
    let metrics = on("10.0.0.0/8");
    let dir = state_dir("metrics-telemetry-status").await;
    let auth = Auth::new(Store::open(dir.as_ref()).await.unwrap());
    let telemetry = Arc::new(spoolframe_server::telemetry::Telemetry::new(
        Box::new(|_| {}),
        Default::default(),
        metrics.clone(),
        spoolframe_server::network::ServerName::new("Den"),
        "f00dfeed".into(),
    ));
    let (addr, handle) = admin::serve(
        "127.0.0.1:0".parse().unwrap(),
        Admin {
            metrics: Some(metrics.clone()),
            telemetry: Some(telemetry),
            ..admin(auth, Some(SetupCode::new(CODE)))
        },
        Origins::default(),
        TrustedProxies::default(),
    )
    .await
    .unwrap();
    std::mem::forget(handle);
    let (root, alice) = root_and_consumer(addr).await;
    assert_eq!(
        code(alice.telemetry_status().await.unwrap_err()),
        error_code::FORBIDDEN
    );
    // Loopback is not in 10.0.0.0/8: refused, and counted.
    assert_eq!(get(addr, None).await.status(), StatusCode::FORBIDDEN);
    assert_eq!(get(addr, None).await.status(), StatusCode::FORBIDDEN);
    let status = root.telemetry_status().await.unwrap();
    assert!(status.metrics.enabled);
    assert_eq!(status.metrics.refused.len(), 1, "{:?}", status.metrics);
    assert_eq!(
        (
            status.metrics.refused[0].address.as_str(),
            status.metrics.refused[0].count
        ),
        ("127.0.0.1", 2)
    );
    assert_eq!(status.otlp, None);
    assert!(
        status
            .resource
            .iter()
            .any(|a| a.key == "spoolframe.server.name" && a.value == "Den"),
        "{:?}",
        status.resource
    );
}

/// The `# TYPE` line of `name` in the Prometheus text, and its lines.
fn exposed(metrics: &Metrics, name: &str) -> (String, Vec<String>) {
    let text = spoolframe_server::metrics::render(&metrics.families());
    let kind = text
        .lines()
        .find_map(|l| l.strip_prefix(&format!("# TYPE {name} ")))
        .unwrap_or_else(|| panic!("no {name} in {text}"))
        .to_owned();
    let lines = text
        .lines()
        .filter(|l| !l.starts_with('#') && l.starts_with(name))
        .map(str::to_owned)
        .collect();
    (kind, lines)
}

/// Each client request is counted by its type and how it went, and its
/// time is in a histogram by type. A type this build does not know is
/// `unknown`, not what the client said: the client chooses it.
#[tokio::test]
async fn client_requests_are_counted_by_type_and_outcome_and_timed() {
    #[derive(serde::Serialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum Newer {
        JoinFederation { code: String },
    }
    #[derive(serde::Serialize)]
    struct Envelope {
        session: Option<String>,
        request: Newer,
    }

    let server = TestServer::start("requests").await;
    server.add_user("alice", "correct horse", None).await;
    let client = server.client().await;
    client.hello().await;
    let refused = client
        .call(
            None,
            Request::LoginPassword(spoolframe_protocol::LoginPassword {
                username: "alice".into(),
                password: "wrong".into(),
                remember: false,
            }),
        )
        .await;
    let Response::Err(error) = &refused else {
        panic!("{refused:?}");
    };
    let refused_as = serde_json::to_value(error).unwrap()["type"]
        .as_str()
        .unwrap()
        .to_owned();
    client.login("alice", "correct horse").await;
    client
        .send(&Envelope {
            session: None,
            request: Newer::JoinFederation { code: "abc".into() },
        })
        .await;
    let metrics = &server.metrics;
    let requests = |request, outcome| {
        value(
            metrics,
            "spoolframe_client_requests_total",
            &[("request", request), ("outcome", outcome)],
        )
    };
    assert_eq!(requests("hello", "ok"), 1.0);
    assert_eq!(requests("login_password", "ok"), 1.0);
    assert_eq!(requests("login_password", &refused_as), 1.0);
    assert_eq!(requests("unknown", "unsupported"), 1.0);
    let (kind, lines) = exposed(metrics, "spoolframe_client_requests_total");
    assert_eq!(kind, "counter");
    assert!(
        lines.iter().all(|l| !l.contains("join_federation")),
        "{lines:?}"
    );

    let (kind, lines) = exposed(metrics, "spoolframe_client_request_duration_seconds");
    assert_eq!(kind, "histogram");
    assert!(
        lines.contains(
            &"spoolframe_client_request_duration_seconds_count{request=\"login_password\"} 2"
                .to_owned()
        ),
        "{lines:?}"
    );
    assert!(
        lines.contains(
            &"spoolframe_client_request_duration_seconds_bucket{request=\"hello\",le=\"+Inf\"} 1"
                .to_owned()
        ),
        "{lines:?}"
    );
    client.close().await;
    server.stop().await;
}

/// The `_count` of histogram `name`'s series with `labels` (written as
/// Prometheus writes them, `a="x"`), or 0 when it has none.
fn histogram_count(metrics: &Metrics, name: &str, labels: &str) -> u64 {
    let (_, lines) = exposed(metrics, name);
    let series = if labels.is_empty() {
        format!("{name}_count ")
    } else {
        format!("{name}_count{{{labels}}} ")
    };
    lines
        .iter()
        .find_map(|l| l.strip_prefix(&series))
        .map_or(0, |n| n.parse().unwrap())
}

/// A direct playback's first read with bytes is its time to first media,
/// once: the reads after it are not.
#[tokio::test]
async fn a_playback_s_first_read_is_its_time_to_first_media() {
    let server = TestServer::start("first-media").await;
    server.write_media(1 << 20);
    server.add_user("alice", "correct horse", None).await;
    let client = server.client().await;
    client.hello_with(None, Some(mkv_capabilities())).await;
    let session = client.login("alice", "correct horse").await;
    let metrics = &server.metrics;
    let name = "spoolframe_playback_first_segment_seconds";
    let (kind, _) = exposed(metrics, name);
    assert_eq!(kind, "histogram");
    let handle = start(&client, &session).await;
    assert_eq!(histogram_count(metrics, name, "plan=\"direct\""), 0);
    for offset in [0, 4096] {
        let (response, _) = client.read(&session, &handle, offset, 4096).await;
        assert!(matches!(response, MediaReadResponse::Ok(_)));
    }
    assert_eq!(histogram_count(metrics, name, "plan=\"direct\""), 1);
    // A direct playback's reads never wait for an encode.
    assert_eq!(
        histogram_count(metrics, "spoolframe_segment_read_wait_seconds", ""),
        0
    );
    client.close().await;
    server.stop().await;
}

/// An HLS conversion's reads say how long they waited for the encode, a
/// read the encode is past 0; its first segment is its time to first
/// media; a seek's run is a restart. Real ffmpeg.
///
/// The first read waits however late it comes: with no read-ahead the
/// throttle holds the encode before its first segment until a reader asks
/// for it. ffmpeg reads in real time, reporting often, so the throttle
/// holds it well before the 2 s segment is cut; at full speed it could
/// write the segment between two looks.
#[tokio::test]
async fn an_hls_conversion_s_waits_first_segment_and_seek_are_measured() {
    let dir = fresh_dir("hls-quality").await;
    std::fs::create_dir_all(&dir).unwrap();
    let ffmpeg = dir.join("real-time-ffmpeg");
    crate::support::write_script(
        &ffmpeg,
        "#!/bin/sh\nexec ffmpeg -stats_period 0.1 -readrate 1 \"$@\"\n",
    );
    let server = TestServer::start_with(
        dir,
        ServerOptions {
            real_media: true,
            ffmpeg,
            read_ahead: Duration::ZERO,
            ..ServerOptions::default()
        },
    )
    .await;
    server.generate_media(60, "libx264", "ac3");
    server.add_user("alice", "correct horse", None).await;
    let client = server.client().await;
    client
        .hello_with(None, Some(crate::support::hls_capabilities()))
        .await;
    let session = client.login("alice", "correct horse").await;
    let metrics = &server.metrics;
    let handle = start(&client, &session).await;
    let segment = |n| spoolframe_protocol::MediaRead {
        session: session.clone(),
        handle: handle.clone(),
        offset: 0,
        length: spoolframe_protocol::MAX_READ,
        epoch: 0,
        segment: Some(n),
        variant: 0,
    };
    // Held for its reader before its first segment, so the first read
    // waits for the encode; the same again never does.
    until("the encode is held for its reader", || {
        let open = server.debug.list().open;
        let conversion = open[0].conversion.as_ref().expect("a conversion");
        assert_eq!(conversion.last_segment, None, "held only after segment 1");
        conversion.paused
    })
    .await;
    for _ in 0..2 {
        let (read, bytes) = client.media(segment(1)).await;
        assert!(matches!(read, MediaReadResponse::Ok(_)), "{read:?}");
        assert!(!bytes.is_empty());
    }
    let wait = "spoolframe_segment_read_wait_seconds";
    let (kind, lines) = exposed(metrics, wait);
    assert_eq!(kind, "histogram");
    assert_eq!(histogram_count(metrics, wait, ""), 2, "{lines:?}");
    assert!(
        lines.contains(&format!("{wait}_bucket{{le=\"0\"}} 1")),
        "{lines:?}"
    );
    assert_eq!(
        histogram_count(
            metrics,
            "spoolframe_playback_first_segment_seconds",
            "plan=\"transcode\""
        ),
        1
    );

    let restarts = |reason| {
        value(
            metrics,
            "spoolframe_conversion_restarts_total",
            &[("reason", reason)],
        )
    };
    assert_eq!(restarts("seek"), 0.0);
    let seeked = client
        .call(
            Some(&session),
            Request::SeekPlayback {
                handle: handle.clone(),
                position_ms: 30_000,
            },
        )
        .await;
    assert!(
        matches!(seeked, Response::Ok(Reply::PlaybackSeeked { .. })),
        "{seeked:?}"
    );
    assert_eq!(restarts("seek"), 1.0);
    assert_eq!(restarts("switch"), 0.0);
    let (kind, _) = exposed(metrics, "spoolframe_conversion_restarts_total");
    assert_eq!(kind, "counter");
    client.close().await;
    server.stop().await;
}

/// What a client connection carried is counted by the path it went by,
/// the rest of it as it closes.
#[tokio::test]
async fn a_connection_s_bytes_are_counted_by_its_path() {
    let server = TestServer::start("link-bytes").await;
    let metrics = server.metrics.clone();
    let (kind, lines) = exposed(&metrics, "spoolframe_connection_bytes_total");
    assert_eq!(kind, "counter");
    for path in ["lan", "direct", "relay", "unknown"] {
        for direction in ["sent", "received"] {
            assert!(
                lines.iter().any(|l| l.starts_with(&format!(
                    "spoolframe_connection_bytes_total{{path=\"{path}\",direction=\"{direction}\"}}"
                ))),
                "{lines:?}"
            );
        }
    }
    let bytes = |direction| {
        ["lan", "direct", "relay", "unknown"]
            .iter()
            .map(|path| {
                value(
                    &metrics,
                    "spoolframe_connection_bytes_total",
                    &[("path", path), ("direction", direction)],
                )
            })
            .sum::<f64>()
    };
    let client = server.client().await;
    client.hello().await;
    for _ in 0..3 {
        client.call(None, Request::Ping).await;
    }
    client.close().await;
    until("the closed connection's bytes are counted", || {
        bytes("sent") > 0.0 && bytes("received") > 0.0
    })
    .await;
    // Not a relay: the test's client dials the server directly.
    assert_eq!(
        value(
            &metrics,
            "spoolframe_connection_bytes_total",
            &[("path", "relay"), ("direction", "sent")]
        ),
        0.0
    );
    // A connection is read every 10 s, its round-trip time with it: the
    // first reading is at once.
    let (kind, lines) = exposed(&metrics, "spoolframe_connection_rtt_seconds");
    assert_eq!(kind, "histogram");
    assert!(
        lines.iter().any(|l| {
            l.starts_with("spoolframe_connection_rtt_seconds_count{path=\"")
                && !l.starts_with("spoolframe_connection_rtt_seconds_count{path=\"relay\"")
        }),
        "{lines:?}"
    );
    server.stop().await;
}

/// Refused requests are counted by kind and channel, every one, never
/// by who or from where.
#[tokio::test]
async fn refused_requests_are_counted_by_kind_and_channel() {
    let server = TestServer::start("refused").await;
    server.add_user("alice", "correct horse", None).await;
    let client = server.client().await;
    client.hello().await;
    let metrics = &server.metrics;
    let refused = |kind| {
        value(
            metrics,
            "spoolframe_requests_refused_total",
            &[("kind", kind), ("channel", "quic")],
        )
    };
    assert_eq!(refused("sign_in_failed"), 0.0);
    for _ in 0..2 {
        let response = client
            .call(
                None,
                Request::LoginPassword(spoolframe_protocol::LoginPassword {
                    username: "alice".into(),
                    password: "wrong".into(),
                    remember: false,
                }),
            )
            .await;
        assert!(matches!(response, Response::Err(_)), "{response:?}");
    }
    assert_eq!(refused("sign_in_failed"), 2.0);
    let (kind, lines) = exposed(metrics, "spoolframe_requests_refused_total");
    assert_eq!(kind, "counter");
    assert!(
        lines
            .iter()
            .all(|l| !l.contains("alice") && !l.contains("source")),
        "{lines:?}"
    );
    assert!(
        lines.contains(
            &"spoolframe_requests_refused_total{kind=\"rate_limited\",channel=\"admin_api\"} 0"
                .to_owned()
        ),
        "{lines:?}"
    );
    client.close().await;
    server.stop().await;
}
