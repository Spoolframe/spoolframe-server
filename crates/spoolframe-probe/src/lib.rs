//! Checks that a Spoolframe server keeps the limits it documents for
//! devices with no session on `spoolframe/client/1`, over live QUIC
//! (PX-11's coverage gap 3): a conformance check for one's own server.
//!
//! Every client it acts as is a stranger: a fresh endpoint key with no
//! session. It speaks raw iroh connections, so it can hold streams open
//! or stay silent on one as the SDK never would, but every frame it sends
//! is a real request, encoded by `spoolframe-protocol`. Malformed frames
//! are left to the fuzz targets (`protocol_frame`, `protocol_cbor`).
//!
//! Bounded by design:
//! - every connection and stream it opens counts against a budget
//!   ([`Config::budget`]); when it is spent, the remaining checks are
//!   skipped;
//! - [`wire::BURST`] unanswered or `Internal` exchanges in a row stop the
//!   run;
//! - the stranger-cap check opens at most cap+1 connections, one at a
//!   time, and only when asked ([`Config::check_cap`]); the sign-in check
//!   only when asked ([`Config::check_limits`]), only for a username it
//!   makes up, and from a key of its own, so the source it locks is not
//!   the run's.
//! - the anonymous-pool check holds the pool's attention for
//!   [`Config::pool_seconds`] with [`Config::pool_spammers`] strangers,
//!   and only when asked ([`Config::check_pool`]). It is last: it holds
//!   strangers' slots for a while (before finding 48's fix it also blocked
//!   every anonymous sign-in).
//!
//! Given a consumer account ([`Config::account`]), it also signs in as
//! that account, once over QUIC and once on the web listener, and checks
//! what a signed-in consumer may and may not do (`account`): only reads,
//! and refusals that would be harmless were they not refused. Every
//! session it makes is signed out at the end, and the report says what
//! the account still lists.
//!
//! Only when asked ([`Config::check_playback`]), it starts one direct
//! playback as the account and tries its handle from another device of
//! the same account, and of [`Config::second`] if given (`playback`);
//! then one HLS remux of the same item's audio, with progress sent from
//! the others too. Nothing it sends there changes more than those
//! playbacks, unless a refusal fails and progress is taken.

//!
//! `--check-wan <addr>` ([`check_wan`], P6-02) is a mode of its own: it
//! only dials the server at one public address and says whether it
//! answered, for an operator checking a port forward from another
//! network. So is `federation` ([`federation`], P8): a user's device
//! playing a paired server's item, through the SDK.

mod account;
mod checks;
pub mod federation;
mod playback;
pub mod wan;
pub mod web;
pub mod wire;

use std::time::{Duration, Instant};

use iroh::EndpointAddr;
use serde::Serialize;

use account::AccountState;
pub use checks::Check;
pub use wan::{WanCheck, check_wan, wan_summary};
use wire::{Probe, Problem};

/// What to probe and how hard.
#[derive(Debug, Clone)]
pub struct Config {
    pub target: EndpointAddr,
    /// The most connections and streams the run opens.
    pub budget: u32,
    /// How long any one answer may take.
    pub reply_timeout: Duration,
    /// How long the first-frame check waits for its stream to be reset:
    /// past the server's `network.first_frame_timeout_secs`.
    pub first_frame_wait: Duration,
    /// The stranger limits the server is expected to keep.
    pub expect: Limits,
    /// Run the stranger-cap check (cap+1 connections).
    pub check_cap: bool,
    /// How long the stranger-cap check waits between its connections.
    pub cap_pace: Duration,
    /// Run the sign-in limits check (failed logins for a made-up user).
    pub check_limits: bool,
    /// Run the anonymous-pool check (strangers holding the pool).
    pub check_pool: bool,
    /// Strangers the anonymous-pool check holds shares with.
    pub pool_spammers: usize,
    /// How long the anonymous-pool check spams for.
    pub pool_seconds: u64,
    /// How long the anonymous-pool check waits for the pool to drain.
    pub pool_drain: Duration,
    /// Sign in as this account for the signed-in checks.
    pub account: Option<Account>,
    /// Run the playback-ownership check: start one direct playback as
    /// [`Config::account`], and try its handle from other devices; then
    /// one HLS remux of the same item's audio, if it is planned as one.
    pub check_playback: bool,
    /// A second consumer account, for the playback-ownership check's
    /// cross-user part. Its `web` is not used.
    pub second: Option<Account>,
}

/// A consumer account the run signs in as. The password is never shown:
/// not in `Debug`, the report or any line of evidence.
#[derive(Clone)]
pub struct Account {
    pub username: String,
    pub password: String,
    /// The server's web listener, for the admin API checks and the list of
    /// what the account still has signed in after the run.
    pub web: Option<Web>,
}

impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account")
            .field("username", &self.username)
            .field("password", &"<hidden>")
            .field("web", &self.web)
            .finish()
    }
}

/// Where the web listener is, and the `Origin` its own pages send: the
/// server's `public_url`, or the listener's address without one.
#[derive(Debug, Clone)]
pub struct Web {
    pub url: String,
    pub origin: String,
}

/// What a server is expected to give a device with no session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Bidirectional streams open at once (the server's
    /// `STRANGER_BI_STREAMS`).
    pub bi_streams: u32,
    /// The longest request frame (`STRANGER_MAX_FRAME`).
    pub max_frame: u32,
    /// Stranger connections held at once
    /// (`network.max_stranger_connections`).
    pub cap: u32,
    /// Failed logins per username before `RateLimited` (`MAX_FAILURES`).
    pub failures: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            bi_streams: 4,
            max_frame: 64 << 10,
            cap: 64,
            failures: 5,
        }
    }
}

impl Config {
    /// The defaults, against `target`.
    pub fn new(target: EndpointAddr) -> Self {
        Self {
            target,
            budget: 200,
            reply_timeout: Duration::from_secs(5),
            first_frame_wait: Duration::from_secs(15),
            expect: Limits::default(),
            check_cap: false,
            cap_pace: Duration::from_millis(100),
            check_limits: false,
            check_pool: false,
            pool_spammers: 8,
            pool_seconds: 45,
            pool_drain: Duration::from_secs(40),
            account: None,
            check_playback: false,
            second: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Status {
    Pass,
    Fail,
    Skip,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Status::Pass => "PASS",
            Status::Fail => "FAIL",
            Status::Skip => "SKIP",
        })
    }
}

/// One check's result.
#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    pub id: &'static str,
    pub title: &'static str,
    pub status: Status,
    /// What was sent and what came back, one line each.
    pub evidence: Vec<String>,
    pub elapsed_ms: u64,
}

/// Who answered the probe's first `Hello`.
#[derive(Debug, Clone, Serialize)]
pub struct ServerSummary {
    pub name: String,
    pub server_version: String,
    pub protocol_version: u32,
}

/// A run.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub endpoint: String,
    pub addrs: Vec<String>,
    pub server: Option<ServerSummary>,
    pub budget: u32,
    pub spent: u32,
    /// Why the run stopped early, if it did.
    pub aborted: Option<String>,
    pub checks: Vec<CheckResult>,
    /// What the run did as the account, its signing out, and what the
    /// account still listed before the web session ended.
    pub account: Vec<String>,
}

impl Report {
    pub fn check(&self, id: &str) -> Option<&CheckResult> {
        self.checks.iter().find(|c| c.id == id)
    }

    pub fn count(&self, status: Status) -> usize {
        self.checks.iter().filter(|c| c.status == status).count()
    }
}

/// Run every check against `config.target`, in order.
pub async fn run(config: &Config) -> anyhow::Result<Report> {
    let probe = Probe::new(config.target.clone(), config.budget, config.reply_timeout).await?;
    let mut report = Report {
        endpoint: config.target.id.to_string(),
        addrs: config.target.ip_addrs().map(ToString::to_string).collect(),
        server: None,
        budget: config.budget,
        spent: 0,
        aborted: None,
        checks: Vec::new(),
        account: Vec::new(),
    };
    let state = AccountState::new();
    // A server that does not answer a good Hello is not probed further.
    match checks::preflight(&probe).await {
        Ok(info) => {
            report.server = Some(ServerSummary {
                name: info.name,
                server_version: info.server_version,
                protocol_version: info.protocol_version,
            })
        }
        Err(Problem::Stop(stop)) => report.aborted = Some(stop.to_string()),
        Err(Problem::Failed(why)) => {
            report.aborted = Some(format!("the server did not answer a good Hello: {why}"))
        }
    }
    for check in Check::ALL {
        let began = Instant::now();
        let (status, evidence) = match &report.aborted {
            Some(why) => (Status::Skip, vec![format!("not run: {why}")]),
            None => match check.run(&probe, config, &state).await {
                Ok(outcome) => (outcome.status, outcome.evidence),
                Err(Problem::Failed(why)) => (Status::Fail, vec![why]),
                Err(Problem::Stop(stop)) => {
                    report.aborted = Some(stop.to_string());
                    (Status::Skip, vec![format!("stopped: {stop}")])
                }
            },
        };
        report.checks.push(CheckResult {
            id: check.id(),
            title: check.title(),
            status,
            evidence,
            elapsed_ms: began.elapsed().as_millis() as u64,
        });
    }
    // Whatever happened above: nothing the run signed in stays signed in.
    report.account = state.finish(&probe, config).await;
    report.spent = probe.spent();
    probe.close().await;
    Ok(report)
}

/// The report as a person reads it.
pub fn summary(report: &Report) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "spoolframe-probe: {} at {}",
        report.endpoint,
        report.addrs.join(", ")
    );
    match &report.server {
        Some(s) => {
            let _ = writeln!(
                out,
                "server: {:?}, version {}, protocol {}",
                s.name, s.server_version, s.protocol_version
            );
        }
        None => out.push_str("server: no answer\n"),
    }
    for check in &report.checks {
        let _ = writeln!(
            out,
            "\n{}  {:<18} {} ({} ms)",
            check.status, check.id, check.title, check.elapsed_ms
        );
        for line in &check.evidence {
            let _ = writeln!(out, "      - {line}");
        }
    }
    if !report.account.is_empty() {
        out.push_str("\naccount:\n");
        for line in &report.account {
            let _ = writeln!(out, "      - {line}");
        }
    }
    let _ = writeln!(out, "\nbudget: {} of {} used", report.spent, report.budget);
    if let Some(why) = &report.aborted {
        let _ = writeln!(out, "stopped early: {why}");
    }
    let _ = writeln!(
        out,
        "result: {} PASS, {} FAIL, {} SKIP",
        report.count(Status::Pass),
        report.count(Status::Fail),
        report.count(Status::Skip)
    );
    out
}
