//! `spoolframe-probe`: checks that a Spoolframe server keeps its documented
//! limits for devices with no session, over live QUIC.

use std::net::SocketAddr;
use std::process::ExitCode;

use clap::Parser;
use iroh::{EndpointAddr, EndpointId};
use spoolframe_probe::{Config, Status, run, summary};

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
    /// Print the report as JSON.
    #[arg(long)]
    json: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<ExitCode> {
    let args = Args::parse();
    let target = args
        .addr
        .iter()
        .fold(EndpointAddr::new(args.endpoint), |addr, ip| {
            addr.with_ip_addr(*ip)
        });
    let config = Config {
        budget: args.budget,
        check_cap: args.check_cap,
        check_limits: args.check_limits,
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
