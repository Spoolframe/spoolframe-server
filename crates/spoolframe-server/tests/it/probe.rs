//! `spoolframe-probe` (PX-11 coverage gap 3) against an in-process server:
//! every check passes against the server as it is, and a server that keeps
//! a limit loosely fails the check for that limit. The weakened servers are
//! the operator's own settings (`Admission`, a user's role), not changed
//! code.
//!
//! Every run signs in as a consumer, over QUIC and on a web listener
//! beside the server, for the signed-in checks; and, for the
//! playback-ownership check, plays the fake catalog's movie and signs in
//! as a second consumer. Its HLS part needs a file HLS takes as a copy
//! of the audio: real FLAC, in a test of its own.
//!
//! The playback-ownership check has no weakened server here: PB is not a
//! setting, and a server that let another session use a handle would
//! need its code changed (`use_playback`, `Playbacks::control`).

use crate::support;

use std::time::Duration;

use spoolframe_probe::{Account, Config, Limits, Report, Status, Web};
use spoolframe_server::admission::Admission;
use spoolframe_server::auth::hash_password;
use spoolframe_server::principal::Principal;
use spoolframe_types::Role;
use support::{ServerOptions, TestServer};

/// The stranger cap the tests' servers keep: small, so the check is quick.
const CAP: usize = 3;

/// The account every run signs in as.
const USER: &str = "probe-consumer";
const PASSWORD: &str = "a probe's password";

/// The second account, for the playback check's cross-user part.
const USER2: &str = "probe-consumer-2";
const PASSWORD2: &str = "another probe's password";

/// The `public_url` of the web listener beside the server.
const WEB: &str = "https://media.example.org";

/// The checks that sign in as [`USER`].
const SIGNED_IN: [&str; 7] = [
    "consumer-admin-refused",
    "session-bound",
    "visibility",
    "recent-sign-in",
    "signed-in-streams",
    "playback-ownership",
    "session-reread",
];

/// A server with [`USER`] as `role` and [`USER2`] a consumer, the movie's
/// file written, and the web listener beside it.
async fn server_with(test: &str, admission: Admission, role: Role) -> (TestServer, Web) {
    let server = TestServer::start_with(
        support::fresh_dir(test).await,
        ServerOptions {
            admission,
            ..ServerOptions::default()
        },
    )
    .await;
    server.write_media(1000);
    users_and_web(server, role).await
}

/// As [`server_with`], the movie's file 20 s of real FLAC that ffprobe
/// reads: it plays directly, and HLS takes it as a copy of its audio.
async fn server_with_flac(test: &str) -> (TestServer, Web) {
    let server = TestServer::start_with(
        support::fresh_dir(test).await,
        ServerOptions {
            admission: quick(),
            real_media: true,
            ..ServerOptions::default()
        },
    )
    .await;
    server.generate_flac_noise(20);
    users_and_web(server, Role::Consumer).await
}

async fn users_and_web(server: TestServer, role: Role) -> (TestServer, Web) {
    server
        .store
        .add_user(USER, None, &hash_password(PASSWORD).await.unwrap(), role)
        .await
        .unwrap();
    server
        .store
        .add_user(
            USER2,
            None,
            &hash_password(PASSWORD2).await.unwrap(),
            Role::Consumer,
        )
        .await
        .unwrap();
    let web = crate::admin::serve_beside(&server, WEB).await;
    let web = Web {
        url: format!("http://{web}"),
        origin: WEB.into(),
    };
    (server, web)
}

/// A run with every check on, expecting a cap of [`CAP`], waiting
/// `first_frame_wait` for a silent stream to be reset.
async fn probe(server: &TestServer, web: Web, first_frame_wait: Duration) -> Report {
    let config = Config {
        first_frame_wait,
        expect: Limits {
            cap: CAP as u32,
            ..Limits::default()
        },
        check_cap: true,
        check_limits: true,
        cap_pace: Duration::from_millis(10),
        account: Some(Account {
            username: USER.into(),
            password: PASSWORD.into(),
            web: Some(web),
        }),
        check_playback: true,
        second: Some(Account {
            username: USER2.into(),
            password: PASSWORD2.into(),
            web: None,
        }),
        ..Config::new(server.server.addr())
    };
    spoolframe_probe::run(&config)
        .await
        .expect("the probe runs")
}

fn status(report: &Report, id: &str) -> Status {
    report
        .check(id)
        .unwrap_or_else(|| panic!("no check {id}"))
        .status
}

/// `username`'s sessions the store still has, of any kind.
async fn sessions_of(server: &TestServer, username: &str) -> usize {
    let user = server
        .store
        .list_users()
        .await
        .unwrap()
        .into_iter()
        .find(|u| u.username == username)
        .expect("the probe's user");
    server.store.list_sessions(user.id).await.unwrap().len()
}

/// [`USER`]'s and [`USER2`]'s sessions the store still has.
async fn sessions_left(server: &TestServer) -> usize {
    sessions_of(server, USER).await + sessions_of(server, USER2).await
}

fn quick() -> Admission {
    Admission {
        max_strangers: CAP,
        first_frame: Duration::from_millis(300),
        ..Admission::default()
    }
}

#[tokio::test]
async fn every_check_passes_against_the_server() {
    let (server, web) = server_with("probe-passes", quick(), Role::Consumer).await;
    let report = probe(&server, web, Duration::from_secs(5)).await;
    let summary = spoolframe_probe::summary(&report);
    // Shown with --nocapture: the evidence, not only the verdicts.
    eprintln!("{summary}");
    assert_eq!(report.aborted, None, "{summary}");
    // All but the anonymous-pool check, which needs more strangers than
    // [`CAP`] and has a test of its own below.
    assert_eq!(status(&report, "anonymous-pool"), Status::Skip, "{summary}");
    assert_eq!(
        report.count(Status::Pass),
        report.checks.len() - 1,
        "{summary}"
    );
    server.stop().await;
}

/// Finding 48 (the second review's M-1): strangers spamming refused
/// resumes leave room for another stranger's login. Its own server, at the
/// default stranger cap: the spammers are more strangers than [`CAP`].
/// Short, so the run stays quick; the live run spams for 45 s.
#[tokio::test]
async fn strangers_spamming_resumes_leave_room_for_a_login() {
    let (server, _web) = server_with("probe-pool", Admission::default(), Role::Consumer).await;
    let config = Config {
        check_pool: true,
        pool_seconds: 3,
        pool_drain: Duration::from_secs(10),
        ..Config::new(server.server.addr())
    };
    let report = spoolframe_probe::run(&config)
        .await
        .expect("the probe runs");
    let summary = spoolframe_probe::summary(&report);
    assert_eq!(status(&report, "anonymous-pool"), Status::Pass, "{summary}");
    server.stop().await;
}

/// G-16: the limits check's refused logins, from the run's own key, once
/// locked that key for the pool check's logins after it, which then failed
/// as `RateLimited`. In one run, both pass.
#[tokio::test]
async fn the_limits_and_pool_checks_pass_in_one_run() {
    let (server, _web) =
        server_with("probe-limits-pool", Admission::default(), Role::Consumer).await;
    let config = Config {
        check_limits: true,
        check_pool: true,
        pool_seconds: 3,
        pool_drain: Duration::from_secs(10),
        ..Config::new(server.server.addr())
    };
    let report = spoolframe_probe::run(&config)
        .await
        .expect("the probe runs");
    let summary = spoolframe_probe::summary(&report);
    assert_eq!(status(&report, "sign-in-limits"), Status::Pass, "{summary}");
    assert_eq!(status(&report, "anonymous-pool"), Status::Pass, "{summary}");
    server.stop().await;
}

#[tokio::test]
async fn the_signed_in_checks_leave_no_session_and_never_show_the_password() {
    let (server, web) = server_with("probe-clean", quick(), Role::Consumer).await;
    let report = probe(&server, web, Duration::from_secs(5)).await;
    let summary = spoolframe_probe::summary(&report);
    for id in SIGNED_IN {
        assert_eq!(status(&report, id), Status::Pass, "{id}\n{summary}");
    }
    assert_eq!(sessions_left(&server).await, 0, "{summary}");
    let json = serde_json::to_string(&report).unwrap();
    assert!(!summary.contains(PASSWORD), "{summary}");
    assert!(!json.contains(PASSWORD));
    assert!(!summary.contains(PASSWORD2), "{summary}");
    assert!(!json.contains(PASSWORD2));
    server.stop().await;
}

/// The movie is played on device A; B (the same user, another key) and C
/// (the second user) are refused every read and control of its handle,
/// and A stops it: no playback is left open.
#[tokio::test]
async fn another_device_and_another_user_are_refused_the_playback() {
    let (server, web) = server_with("probe-playback", quick(), Role::Consumer).await;
    let report = probe(&server, web, Duration::from_secs(5)).await;
    let summary = spoolframe_probe::summary(&report);
    let check = report.check("playback-ownership").unwrap();
    assert_eq!(check.status, Status::Pass, "{summary}");
    let refused = |who: &str| {
        check
            .evidence
            .iter()
            .filter(|line| line.starts_with(who) && line.contains("Unauthenticated"))
            .count()
    };
    // Five RPC requests, three resource reads and the stop; B also sends
    // A's own session.
    assert_eq!(refused("B ("), 9, "{summary}");
    assert_eq!(refused("B, with A's session"), 1, "{summary}");
    assert_eq!(refused("C ("), 9, "{summary}");
    assert!(
        check
            .evidence
            .iter()
            .any(|l| l.starts_with("A: StopPlayback: Ok(Done)")),
        "{summary}"
    );
    let counts = server.debug.playbacks.counts();
    assert_eq!(
        (counts.started, counts.ended, counts.direct),
        (1, 1, 0),
        "{summary}"
    );
    assert_eq!(sessions_left(&server).await, 0, "{summary}");
    // The fake movie has a picture: HLS would encode it, so it is not
    // started.
    assert!(
        check
            .evidence
            .iter()
            .any(|l| l.starts_with("HLS part not checked: A: PlaybackPlan")),
        "{summary}"
    );
    server.stop().await;
}

/// The FLAC movie is played directly, then as an HLS copy of its audio.
/// B and C are refused A's playlists, segments, seek, stop and progress;
/// A's resume point is as it was, and the server logs every refusal.
#[tokio::test]
async fn another_device_and_another_user_are_refused_an_hls_playback_and_its_progress() {
    const TEST: &str =
        "probe::another_device_and_another_user_are_refused_an_hls_playback_and_its_progress";
    if !support::alone(TEST) {
        return;
    }
    let (server, web) = server_with_flac("probe-playback-hls").await;
    let (capture, _guard) = support::LogCapture::start();
    let report = probe(&server, web, Duration::from_secs(5)).await;
    let summary = spoolframe_probe::summary(&report);
    // Shown with --nocapture: the evidence, not only the verdicts.
    eprintln!("{summary}");
    let check = report.check("playback-ownership").unwrap();
    assert_eq!(check.status, Status::Pass, "{summary}");
    let refused = |who: &str, hls: bool| {
        check
            .evidence
            .iter()
            .filter(|line| {
                line.starts_with(who)
                    && line.contains(", HLS: ") == hls
                    && line.contains("Unauthenticated")
            })
            .count()
    };
    // Direct: five RPC requests, three resource reads and the stop.
    assert_eq!(refused("B (", false), 9, "{summary}");
    assert_eq!(refused("C (", false), 9, "{summary}");
    // HLS: two playlists, progress and its sequence, a seek, three
    // resource reads and the stop.
    assert_eq!(refused("B (", true), 9, "{summary}");
    assert_eq!(refused("C (", true), 9, "{summary}");
    for line in [
        "B (\"probe-consumer\" on another device), HLS: UpdatePlaybackProgress: Err(Unauthenticated)",
        "C (\"probe-consumer-2\" on another device), HLS: UpdatePlaybackProgress: Err(Unauthenticated)",
        "HLS: A: PlaybackProgressSequence (none accepted): Ok(PlaybackProgressSequence { sequence: 0 })",
        "HLS: A: StopPlayback: Ok(Done)",
    ] {
        assert!(
            check.evidence.iter().any(|l| l == line),
            "{line}\n{summary}"
        );
    }
    assert!(
        !check.evidence.iter().any(|l| l.contains("not checked")),
        "{summary}"
    );

    // Both playbacks were started and ended, and nothing is playing.
    let counts = server.debug.playbacks.counts();
    assert_eq!(
        (
            counts.started,
            counts.ended,
            counts.direct + counts.remux + counts.transcode
        ),
        (2, 2, 0),
        "{summary}"
    );
    // No progress was written for the probe's user.
    let user = server
        .store
        .list_users()
        .await
        .unwrap()
        .into_iter()
        .find(|u| u.username == USER)
        .unwrap();
    let id = support::movie().id().to_string();
    let written = server
        .store
        .watch_states(Principal::LocalUser(user.id), std::slice::from_ref(&id))
        .await
        .unwrap();
    assert!(written.is_empty(), "{written:?}");
    assert_eq!(sessions_left(&server).await, 0, "{summary}");

    // Each refusal of the playback is a line of its own: 9 a part from
    // each of B and C, under the log's 10 a minute for one key.
    let logged = capture
        .refused()
        .into_iter()
        .filter(|l| l.field("reason") == Some("playback is another session's"))
        .count();
    assert_eq!(logged, 36, "{summary}");
    server.stop().await;
}

#[tokio::test]
async fn without_an_account_the_signed_in_checks_are_skipped() {
    let (server, _web) = server_with("probe-no-account", quick(), Role::Consumer).await;
    let config = Config {
        first_frame_wait: Duration::from_secs(5),
        ..Config::new(server.server.addr())
    };
    let report = spoolframe_probe::run(&config).await.unwrap();
    for id in SIGNED_IN {
        assert_eq!(status(&report, id), Status::Skip, "{id}");
    }
    assert!(report.account.is_empty());
    server.stop().await;
}

/// The account the operator called a consumer is an admin on this server:
/// the admin reads are answered, and that check fails, alone.
#[tokio::test]
async fn an_account_with_admin_rights_fails_the_consumer_check() {
    let (server, web) = server_with("probe-admin", quick(), Role::Admin).await;
    let report = probe(&server, web, Duration::from_secs(5)).await;
    let summary = spoolframe_probe::summary(&report);
    assert_eq!(
        status(&report, "consumer-admin-refused"),
        Status::Fail,
        "{summary}"
    );
    assert_eq!(report.count(Status::Fail), 1, "{summary}");
    assert_eq!(sessions_left(&server).await, 0, "{summary}");
    server.stop().await;
}

#[tokio::test]
async fn a_server_that_leaves_a_silent_stream_open_fails_the_first_frame_check() {
    let (server, web) = server_with(
        "probe-first-frame",
        Admission {
            max_strangers: CAP,
            first_frame: Duration::from_secs(60),
            ..Admission::default()
        },
        Role::Consumer,
    )
    .await;
    let report = probe(&server, web, Duration::from_secs(1)).await;
    let summary = spoolframe_probe::summary(&report);
    assert_eq!(status(&report, "first-frame"), Status::Fail, "{summary}");
    assert_eq!(report.count(Status::Fail), 1, "{summary}");
    server.stop().await;
}

#[tokio::test]
async fn a_server_that_holds_more_strangers_fails_the_cap_check() {
    let (server, web) = server_with(
        "probe-cap",
        Admission {
            max_strangers: CAP + 2,
            first_frame: Duration::from_millis(300),
            ..Admission::default()
        },
        Role::Consumer,
    )
    .await;
    let report = probe(&server, web, Duration::from_secs(5)).await;
    let summary = spoolframe_probe::summary(&report);
    assert_eq!(status(&report, "stranger-cap"), Status::Fail, "{summary}");
    assert_eq!(report.count(Status::Fail), 1, "{summary}");
    server.stop().await;
}

/// `--check-wan` (P6-02), in-process: through an address that reaches the
/// server it says so, with the server's name. Run for real from another
/// network, against the advertised public address.
#[tokio::test]
async fn check_wan_reaches_the_server_through_the_address_given() {
    let server = TestServer::start_with(
        support::fresh_dir("probe-check-wan").await,
        ServerOptions::default(),
    )
    .await;
    let addr = *server
        .server
        .addr()
        .ip_addrs()
        .find(|a| a.is_ipv4())
        .expect("an IPv4 address");
    let addr = std::net::SocketAddr::new([127, 0, 0, 1].into(), addr.port());
    let check = spoolframe_probe::check_wan(server.server.id(), addr, Duration::from_secs(5))
        .await
        .unwrap();
    assert!(check.reachable, "{check:?}");
    assert_eq!(check.server.unwrap().name, "Home Media");
    assert_eq!(check.addr, addr.to_string());
    server.stop().await;
}

/// Through an address nothing answers at, or that another key answers
/// at, the check fails and says why.
#[tokio::test]
async fn check_wan_fails_where_the_server_is_not() {
    let server = TestServer::start_with(
        support::fresh_dir("probe-check-wan-not").await,
        ServerOptions::default(),
    )
    .await;
    let silent = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let nowhere = silent.local_addr().unwrap();
    let check =
        spoolframe_probe::check_wan(server.server.id(), nowhere, Duration::from_millis(500))
            .await
            .unwrap();
    assert!(!check.reachable, "{check:?}");
    assert!(check.error.is_some(), "{check:?}");
    assert!(
        spoolframe_probe::wan_summary(&check).starts_with("check-wan: FAIL"),
        "{}",
        spoolframe_probe::wan_summary(&check)
    );

    let port = server
        .server
        .addr()
        .ip_addrs()
        .find(|a| a.is_ipv4())
        .expect("an IPv4 address")
        .port();
    let other = iroh::SecretKey::generate().public();
    let check = spoolframe_probe::check_wan(
        other,
        std::net::SocketAddr::new([127, 0, 0, 1].into(), port),
        Duration::from_millis(500),
    )
    .await
    .unwrap();
    assert!(!check.reachable, "another key answers there: {check:?}");
    drop(silent);
    server.stop().await;
}
