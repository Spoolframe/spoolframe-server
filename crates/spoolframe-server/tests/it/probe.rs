//! `spoolframe-probe` (PX-11 coverage gap 3) against an in-process server:
//! every check passes against the server as it is, and a server that keeps
//! a limit loosely fails the check for that limit. The weakened servers are
//! the operator's own settings (`Admission`), not changed code.

use crate::support;

use std::time::Duration;

use spoolframe_probe::{Config, Limits, Report, Status};
use spoolframe_server::admission::Admission;
use support::{ServerOptions, TestServer};

/// The stranger cap the tests' servers keep: small, so the check is quick.
const CAP: usize = 3;

async fn server_with(test: &str, admission: Admission) -> TestServer {
    TestServer::start_with(
        support::fresh_dir(test).await,
        ServerOptions {
            admission,
            ..ServerOptions::default()
        },
    )
    .await
}

/// A run with every check on, expecting a cap of [`CAP`], waiting
/// `first_frame_wait` for a silent stream to be reset.
async fn probe(server: &TestServer, first_frame_wait: Duration) -> Report {
    let config = Config {
        first_frame_wait,
        expect: Limits {
            cap: CAP as u32,
            ..Limits::default()
        },
        check_cap: true,
        check_limits: true,
        cap_pace: Duration::from_millis(10),
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

#[tokio::test]
async fn every_check_passes_against_the_server() {
    let server = server_with(
        "probe-passes",
        Admission {
            max_strangers: CAP,
            first_frame: Duration::from_millis(300),
            ..Admission::default()
        },
    )
    .await;
    let report = probe(&server, Duration::from_secs(5)).await;
    let summary = spoolframe_probe::summary(&report);
    // Shown with --nocapture: the evidence, not only the verdicts.
    eprintln!("{summary}");
    assert_eq!(report.aborted, None, "{summary}");
    assert_eq!(report.count(Status::Pass), report.checks.len(), "{summary}");
    server.stop().await;
}

#[tokio::test]
async fn a_server_that_leaves_a_silent_stream_open_fails_the_first_frame_check() {
    let server = server_with(
        "probe-first-frame",
        Admission {
            max_strangers: CAP,
            first_frame: Duration::from_secs(60),
            ..Admission::default()
        },
    )
    .await;
    let report = probe(&server, Duration::from_secs(1)).await;
    let summary = spoolframe_probe::summary(&report);
    assert_eq!(status(&report, "first-frame"), Status::Fail, "{summary}");
    assert_eq!(report.count(Status::Fail), 1, "{summary}");
    server.stop().await;
}

#[tokio::test]
async fn a_server_that_holds_more_strangers_fails_the_cap_check() {
    let server = server_with(
        "probe-cap",
        Admission {
            max_strangers: CAP + 2,
            first_frame: Duration::from_millis(300),
            ..Admission::default()
        },
    )
    .await;
    let report = probe(&server, Duration::from_secs(5)).await;
    let summary = spoolframe_probe::summary(&report);
    assert_eq!(status(&report, "stranger-cap"), Status::Fail, "{summary}");
    assert_eq!(report.count(Status::Fail), 1, "{summary}");
    server.stop().await;
}
