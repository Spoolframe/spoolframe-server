//! `spoolframe-probe` (PX-11 coverage gap 3) against an in-process server:
//! every check passes against the server as it is, and a server that keeps
//! a limit loosely fails the check for that limit. The weakened servers are
//! the operator's own settings (`Admission`, a user's role), not changed
//! code.
//!
//! Every run signs in as a consumer, over QUIC and on a web listener
//! beside the server, for the signed-in checks.

use crate::support;

use std::time::Duration;

use spoolframe_probe::{Account, Config, Limits, Report, Status, Web};
use spoolframe_server::admission::Admission;
use spoolframe_server::auth::hash_password;
use spoolframe_types::Role;
use support::{ServerOptions, TestServer};

/// The stranger cap the tests' servers keep: small, so the check is quick.
const CAP: usize = 3;

/// The account every run signs in as.
const USER: &str = "probe-consumer";
const PASSWORD: &str = "a probe's password";

/// The `public_url` of the web listener beside the server.
const WEB: &str = "https://media.example.org";

/// The checks that sign in as [`USER`].
const SIGNED_IN: [&str; 6] = [
    "consumer-admin-refused",
    "session-bound",
    "visibility",
    "recent-sign-in",
    "signed-in-streams",
    "session-reread",
];

/// A server with [`USER`] as `role`, and the web listener beside it.
async fn server_with(test: &str, admission: Admission, role: Role) -> (TestServer, Web) {
    let server = TestServer::start_with(
        support::fresh_dir(test).await,
        ServerOptions {
            admission,
            ..ServerOptions::default()
        },
    )
    .await;
    server
        .store
        .add_user(USER, None, &hash_password(PASSWORD).await.unwrap(), role)
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

/// [`USER`]'s sessions the store still has, of any kind.
async fn sessions_left(server: &TestServer) -> usize {
    let user = server
        .store
        .list_users()
        .await
        .unwrap()
        .into_iter()
        .find(|u| u.username == USER)
        .expect("the probe's user");
    server.store.list_sessions(user.id).await.unwrap().len()
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
