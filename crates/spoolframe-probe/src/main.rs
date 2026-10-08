//! `spoolframe-probe`: checks that a Spoolframe server keeps its documented
//! limits for devices with no session, over live QUIC, and, given a
//! consumer account, what that account may and may not do signed in.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context;
use clap::Parser;
use iroh::{EndpointAddr, EndpointId};
use spoolframe_probe::{Account, Config, Status, Web, run, summary};

#[derive(Debug, Parser)]
#[command(version, about)]
struct Args {
    /// The server's endpoint id, in hex.
    #[arg(long)]
    endpoint: EndpointId,
    /// Where to dial it.
    #[arg(long, required = true)]
    addr: Vec<SocketAddr>,
    /// The most connections and streams the run opens.
    #[arg(long, default_value_t = 200)]
    budget: u32,
    /// Also check the stranger connection cap: opens cap+1 connections.
    #[arg(long)]
    check_cap: bool,
    /// Also check the sign-in limits: failed logins for a made-up username.
    #[arg(long)]
    check_limits: bool,
    /// Also check the anonymous operation pool: strangers holding it cannot
    /// refuse another stranger's login (holds it for a while).
    #[arg(long)]
    check_pool: bool,
    /// Also run the signed-in checks as this consumer account.
    #[arg(long, requires = "password_file")]
    username: Option<String>,
    /// The account's password, alone in this file (never on the command
    /// line). One trailing newline is not part of it.
    #[arg(long, requires = "username")]
    password_file: Option<PathBuf>,
    /// The server's web listener, as `http://host:port`: the admin API
    /// checks, and the account's sessions and devices after the run.
    #[arg(long, requires = "username")]
    web: Option<String>,
    /// The `Origin` the web listener takes: its `public_url`. The `--web`
    /// URL if left out.
    #[arg(long, requires = "web")]
    web_origin: Option<String>,
    /// Also check playback ownership as the account: start one direct
    /// playback, try its handle from another device signed in as the same
    /// account (and as --second-username), then stop it; then the same
    /// item as an HLS remux of its audio, if it is planned as one, with
    /// progress sent from the others too.
    #[arg(long, requires = "username")]
    check_playback: bool,
    /// A second consumer account, for the playback check's cross-user part.
    #[arg(long, requires_all = ["second_password_file", "check_playback"])]
    second_username: Option<String>,
    /// Its password, alone in this file, as for --password-file.
    #[arg(long, requires = "second_username")]
    second_password_file: Option<PathBuf>,
    /// Print the report as JSON.
    #[arg(long)]
    json: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<ExitCode> {
    let args = Args::parse();
    // aws-lc-rs, iroh's provider, for reqwest too: reqwest 0.12 falls back
    // to ring when no provider is installed (the workspace Cargo.toml).
    if rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .is_err()
    {
        anyhow::bail!("a TLS provider was installed before the probe's own");
    }
    let target = args
        .addr
        .iter()
        .fold(EndpointAddr::new(args.endpoint), |addr, ip| {
            addr.with_ip_addr(*ip)
        });
    let account = match (&args.username, &args.password_file) {
        (Some(username), Some(file)) => Some(Account {
            username: username.clone(),
            password: password(file)?,
            web: args.web.as_ref().map(|url| Web {
                url: url.trim_end_matches('/').to_owned(),
                origin: args
                    .web_origin
                    .clone()
                    .unwrap_or_else(|| url.trim_end_matches('/').to_owned()),
            }),
        }),
        _ => None,
    };
    let second = match (&args.second_username, &args.second_password_file) {
        (Some(username), Some(file)) => Some(Account {
            username: username.clone(),
            password: password(file)?,
            web: None,
        }),
        _ => None,
    };
    let config = Config {
        budget: args.budget,
        check_cap: args.check_cap,
        check_limits: args.check_limits,
        check_pool: args.check_pool,
        account,
        check_playback: args.check_playback,
        second,
        ..Config::new(target)
    };
    let report = run(&config).await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", summary(&report));
    }
    let failed = report.count(Status::Fail) > 0 || report.aborted.is_some();
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// The password in `file`, without one trailing newline. Errors name the
/// file, never its contents.
fn password(file: &Path) -> anyhow::Result<String> {
    let mut text = std::fs::read_to_string(file)
        .with_context(|| format!("cannot read the password file {}", file.display()))?;
    if text.ends_with('\n') {
        text.pop();
        if text.ends_with('\r') {
            text.pop();
        }
    }
    anyhow::ensure!(
        !text.is_empty(),
        "the password file {} is empty",
        file.display()
    );
    Ok(text)
}
