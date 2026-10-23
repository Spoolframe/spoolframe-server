//! `spoolframe-probe`: checks that a Spoolframe server keeps its documented
//! limits for devices with no session, over live QUIC, and, given a
//! consumer account, what that account may and may not do signed in.
//! `spoolframe-probe federation` plays a paired server's item as a user's
//! device does (P8), and says how, a JSON object per line.

use std::io::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use iroh::{EndpointAddr, EndpointId};
use spoolframe_probe::federation::{self, Download, FederationRun, Play, Target};
use spoolframe_probe::{Account, Config, Status, Web, run, summary};

#[derive(Debug, Parser)]
#[command(
    version,
    about,
    subcommand_negates_reqs = true,
    args_conflicts_with_subcommands = true
)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    /// The server's endpoint id, in hex.
    #[arg(long, required = true)]
    endpoint: Option<EndpointId>,
    /// Where to dial it.
    #[arg(long, required_unless_present = "check_wan")]
    addr: Vec<SocketAddr>,
    /// Only check that the server answers through this public address
    /// (`network.wan`), and nothing else: run it from another network.
    #[arg(long, conflicts_with_all = ["addr", "check_cap", "check_limits", "check_pool", "username"])]
    check_wan: Option<SocketAddr>,
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

#[derive(Debug, Subcommand)]
enum Command {
    /// Sign in to a user's own server with the experimental `federation`
    /// feature, list its libraries, and play a paired server's film as an
    /// app does: from that server when it is reached, else through the
    /// user's own. Each step is printed as a JSON object on a line.
    Federation(FederationArgs),
}

#[derive(Debug, clap::Args)]
struct FederationArgs {
    /// The user's own server's endpoint id, in hex.
    #[arg(long)]
    endpoint: EndpointId,
    /// Where to dial it.
    #[arg(long, required = true)]
    addr: Vec<SocketAddr>,
    /// The user to sign in as.
    #[arg(long)]
    username: String,
    /// Their password, alone in this file (never on the command line).
    #[arg(long)]
    password_file: PathBuf,
    /// Also print each item of the paired servers' libraries.
    #[arg(long)]
    list_items: bool,
    /// Play the paired server's film of this title.
    #[arg(long, conflicts_with = "play_id")]
    play_title: Option<String>,
    /// Play this `fed:` item, listed or not.
    #[arg(long)]
    play_id: Option<String>,
    /// Play through the user's own server without trying the paired one.
    #[arg(long)]
    proxied: bool,
    /// Read at most this many bytes of the film; all of it if left out.
    #[arg(long)]
    max_bytes: Option<u64>,
    /// The most each read asks for; 16 MiB (the protocol's most) if left
    /// out.
    #[arg(long)]
    read_size: Option<u64>,
    /// Write the bytes read here.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Report this position, in milliseconds, once the film is read.
    #[arg(long)]
    progress_ms: Option<u64>,
    /// Search the films for this, and print each hit with its copies.
    #[arg(long)]
    search: Option<String>,
    /// Play the title of this id from the paired server --from names,
    /// then print the copies its details say.
    #[arg(long, requires = "from")]
    play_from: Option<String>,
    /// The paired server's server id, for --play-from.
    #[arg(long, requires = "play_from")]
    from: Option<String>,
    /// Download the paired server's film of this title, every track as it
    /// is, into --downloads-dir.
    #[arg(long, requires = "downloads_dir")]
    download_title: Option<String>,
    /// Where the downloads are kept.
    #[arg(long, requires = "download_title")]
    downloads_dir: Option<PathBuf>,
    /// How long the download may take, in seconds.
    #[arg(long, default_value_t = 120)]
    download_secs: u64,
    /// Ask the user's own server for this download, by its id there
    /// (`DownloadStatus`); repeat for several.
    #[arg(long)]
    download_status: Vec<String>,
    /// Have the user's own server prepare a download of this item, every
    /// track as it is, wait until it is made, and read none of it; then
    /// print what it says of it (with --download-status's).
    #[arg(long)]
    prepare_id: Option<String>,
}

impl FederationArgs {
    fn run(&self) -> anyhow::Result<FederationRun> {
        self.run_with_password(password(&self.password_file)?)
    }

    /// The run, signing in with `password`.
    fn run_with_password(&self, password: String) -> anyhow::Result<FederationRun> {
        let target = match (&self.play_title, &self.play_id) {
            (Some(title), _) => Some(Target::Title(title.clone())),
            (None, Some(id)) => Some(Target::Id(
                id.parse()
                    .map_err(|e| anyhow::anyhow!("--play-id {id:?}: {e}"))?,
            )),
            (None, None) => None,
        };
        Ok(FederationRun {
            server: self.endpoint.to_string(),
            addrs: self.addr.clone(),
            username: self.username.clone(),
            password,
            list_items: self.list_items,
            play: target.map(|target| Play {
                target,
                proxied: self.proxied,
                max_bytes: self.max_bytes,
                read_size: self.read_size,
                out: self.out.clone(),
                progress: self.progress_ms.map(Duration::from_millis),
            }),
            search: self.search.clone(),
            play_from: match (&self.play_from, &self.from) {
                (Some(id), Some(from)) => Some((
                    id.parse()
                        .map_err(|e| anyhow::anyhow!("--play-from {id:?}: {e}"))?,
                    from.clone(),
                )),
                _ => None,
            },
            download: match (&self.download_title, &self.downloads_dir) {
                (Some(title), Some(dir)) => Some(Download {
                    title: title.clone(),
                    dir: dir.clone(),
                    within: Duration::from_secs(self.download_secs),
                }),
                _ => None,
            },
            download_status: self.download_status.clone(),
            prepare: self
                .prepare_id
                .as_ref()
                .map(|id| {
                    id.parse()
                        .map_err(|e| anyhow::anyhow!("--prepare-id {id:?}: {e}"))
                })
                .transpose()?,
        })
    }
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
    if let Some(Command::Federation(fed)) = &args.command {
        let ok = federation::run_federation(&fed.run()?, |event| {
            // A line each, flushed, for a script reading as it goes.
            let mut out = std::io::stdout().lock();
            let _ = serde_json::to_writer(&mut out, &event);
            let _ = writeln!(out);
            let _ = out.flush();
        })
        .await;
        return Ok(if ok {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        });
    }
    let endpoint = args
        .endpoint
        .context("--endpoint is required without a subcommand")?;
    if let Some(addr) = args.check_wan {
        let check = spoolframe_probe::check_wan(endpoint, addr, Duration::from_secs(10)).await?;
        if args.json {
            println!("{}", serde_json::to_string_pretty(&check)?);
        } else {
            print!("{}", spoolframe_probe::wan_summary(&check));
        }
        return Ok(if check.reachable {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        });
    }
    let target = args
        .addr
        .iter()
        .fold(EndpointAddr::new(endpoint), |addr, ip| {
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

#[cfg(test)]
mod tests {
    use super::*;

    const ENDPOINT: &str = "ae58ff8833241ac82d6ff7611046ed67b5072d142c588d0063e942d9a75502b6";

    #[test]
    fn check_wan_needs_only_the_endpoint_and_the_address() {
        let args = Args::try_parse_from([
            "probe",
            "--endpoint",
            ENDPOINT,
            "--check-wan",
            "203.0.113.7:7420",
        ])
        .unwrap();
        assert_eq!(args.check_wan, Some("203.0.113.7:7420".parse().unwrap()));
        assert!(args.addr.is_empty());
    }

    #[test]
    fn check_wan_runs_alone() {
        for other in [
            &["--addr", "192.168.1.5:7420"][..],
            &["--check-cap"],
            &["--check-limits"],
        ] {
            let mut argv = vec![
                "probe",
                "--endpoint",
                ENDPOINT,
                "--check-wan",
                "203.0.113.7:7420",
            ];
            argv.extend_from_slice(other);
            assert!(Args::try_parse_from(argv).is_err(), "{other:?}");
        }
        assert!(Args::try_parse_from(["probe", "--endpoint", ENDPOINT]).is_err());
    }

    #[test]
    fn the_checks_still_need_the_endpoint() {
        assert!(Args::try_parse_from(["probe", "--addr", "192.168.1.5:7420"]).is_err());
    }

    /// The federation run's own arguments, with `extra`.
    fn federation_args(extra: &[&'static str]) -> Result<Args, clap::Error> {
        let mut argv = vec![
            "probe",
            "federation",
            "--endpoint",
            ENDPOINT,
            "--addr",
            "192.168.3.4:7420",
            "--username",
            "bob",
            "--password-file",
            "/run/pw",
        ];
        argv.extend_from_slice(extra);
        Args::try_parse_from(argv)
    }

    #[test]
    fn federation_runs_without_the_checks_arguments() {
        let args = federation_args(&["--play-title", "A Film", "--progress-ms", "3000"]).unwrap();
        assert!(args.endpoint.is_none());
        let Some(Command::Federation(fed)) = args.command else {
            panic!("not the federation run");
        };
        assert_eq!(fed.play_title.as_deref(), Some("A Film"));
        assert_eq!(fed.progress_ms, Some(3000));
    }

    #[test]
    fn a_download_needs_somewhere_to_go_and_play_from_a_server() {
        assert!(federation_args(&["--download-title", "A"]).is_err());
        assert!(federation_args(&["--download-title", "A", "--downloads-dir", "/d"]).is_ok());
        assert!(federation_args(&["--play-from", "plex:movie:1"]).is_err());
        let args = federation_args(&["--play-from", "plex:movie:1", "--from", "srv"]).unwrap();
        let Some(Command::Federation(fed)) = args.command else {
            panic!("not the federation run");
        };
        let run = fed.run_with_password(String::new()).unwrap();
        assert_eq!(
            run.play_from,
            Some(("plex:movie:1".parse().unwrap(), "srv".to_owned()))
        );
    }

    #[test]
    fn federation_plays_by_title_or_by_id_not_both() {
        assert!(federation_args(&["--play-title", "A", "--play-id", "fed:x"]).is_err());
        assert!(federation_args(&["--play-id", "fed:x"]).is_ok());
    }

    #[test]
    fn federation_and_the_checks_do_not_mix() {
        let argv = [
            "probe",
            "--check-cap",
            "federation",
            "--endpoint",
            ENDPOINT,
            "--addr",
            "192.168.3.4:7420",
            "--username",
            "bob",
            "--password-file",
            "/run/pw",
        ];
        assert!(Args::try_parse_from(argv).is_err());
    }
}
