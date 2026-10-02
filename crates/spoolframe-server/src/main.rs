use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use spoolframe_catalog::Catalog;
use spoolframe_media::Prober;
use spoolframe_plex::{Failures, LibraryPaths, PlexCatalog, Scope};
use spoolframe_server::config::ServerConfig;
use spoolframe_server::identity;
use spoolframe_server::network::Server;
use spoolframe_types::MediaId;
use tracing_subscriber::EnvFilter;

/// How many items per library the startup check samples.
const STARTUP_SAMPLE: u32 = 5;

/// Spoolframe media server.
#[derive(Parser)]
#[command(name = "spoolframe-server")]
#[command(version)]
struct Cli {
    /// Path to the server config (UCL, schema `spoolframe.server.v1`).
    #[arg(short, long, env = "SPOOLFRAME_CONFIG", global = true)]
    config: Option<PathBuf>,

    /// Increase log verbosity (-v = debug, -vv = trace).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    /// What to do; without one, run the server.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Plex integration tools.
    Plex {
        #[command(subcommand)]
        command: PlexCommand,
    },
    /// Media file tools.
    Media {
        #[command(subcommand)]
        command: MediaCommand,
    },
    /// Print the server's EndpointId, the identity clients pin. Creates the
    /// server key if this is the first start.
    Id,
}

#[derive(Subcommand)]
enum MediaCommand {
    /// Resolve an item to its local file, run ffprobe on it, and print the
    /// descriptor as JSON: what the server will plan playback from.
    Probe {
        /// The item, e.g. `plex:movie:12345`.
        id: MediaId,
    },
}

#[derive(Subcommand)]
enum PlexCommand {
    /// Map every media file in Plex and report, per library, which ones
    /// this host can read. Exits non-zero if any cannot be read.
    CheckPaths {
        /// Check only the library with this title.
        #[arg(long)]
        library: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let level = match cli.verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    // SPOOLFRAME_LOG overrides the level with a full filter
    // expression, e.g. `SPOOLFRAME_LOG=spoolframe_server=trace`.
    let filter = std::env::var("SPOOLFRAME_LOG")
        .map(EnvFilter::new)
        .unwrap_or_else(|_| EnvFilter::new(level));
    // Logs go to stderr: stdout is the output of `id`, `media probe` and
    // `plex check-paths`, and must stay parseable.
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    let Some(config_path) = cli.config else {
        bail!("no config: pass --config <file> or set SPOOLFRAME_CONFIG");
    };
    let config = ServerConfig::load(&config_path)?;
    tracing::info!(state_dir = %config.state_dir.display(), "config loaded");

    match cli.command {
        None => run(&config).await,
        Some(Command::Id) => {
            println!("{}", identity::load_or_create(&config.state_dir)?.public());
            Ok(())
        }
        Some(Command::Plex {
            command: PlexCommand::CheckPaths { library },
        }) => check_paths(&plex(&config)?, library.as_deref()).await,
        Some(Command::Media {
            command: MediaCommand::Probe { id },
        }) => probe(&plex(&config)?, &id).await,
    }
}

fn plex(config: &ServerConfig) -> Result<PlexCatalog> {
    Ok(PlexCatalog::new(config.plex.url()?, config.plex.token()?)?
        .with_paths(config.plex.path_map()?))
}

async fn probe(catalog: &dyn Catalog, id: &MediaId) -> Result<()> {
    let path = catalog.media_path(id).await?;
    let prober = Prober::new("ffprobe", Prober::DEFAULT_TIMEOUT);
    let descriptor = prober.describe(&path).await?;
    println!("{}", serde_json::to_string_pretty(&*descriptor)?);
    Ok(())
}

/// Run the server until SIGINT or SIGTERM.
async fn run(config: &ServerConfig) -> Result<()> {
    // The key first: if it cannot be loaded nothing else should start.
    let key = identity::load_or_create(&config.state_dir)?;
    let plex = Arc::new(plex(config)?);
    let catalog: Arc<dyn Catalog> = plex.clone();
    check_catalog(&plex, catalog.as_ref()).await;

    let server = Server::start(key, &config.network.endpoint_options()?).await?;
    let addrs: Vec<_> = server.addr().ip_addrs().map(ToString::to_string).collect();
    tracing::info!(endpoint_id = %server.id(), ?addrs, "listening");

    shutdown_signal().await?;
    tracing::info!("shutting down");
    server.shutdown().await;
    Ok(())
}

async fn shutdown_signal() -> Result<()> {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        r = tokio::signal::ctrl_c() => r?,
        _ = term.recv() => {}
    }
    Ok(())
}

/// Check Plex and a sample of media paths, and warn about what is wrong.
/// Plex being down must not stop the server (§32): catalog calls report
/// their own errors until it is back.
async fn check_catalog(plex: &PlexCatalog, catalog: &dyn Catalog) {
    match catalog.libraries().await {
        Ok(libraries) => tracing::info!(count = libraries.len(), "Plex catalog ready"),
        Err(e) => {
            tracing::warn!(error = %e, "Plex catalog unavailable");
            return;
        }
    }
    // A few files per library, so a bad path mapping shows in the log at
    // startup rather than when somebody presses play.
    match plex.check_paths(None, Scope::Sample(STARTUP_SAMPLE)).await {
        Ok(reports) => {
            for r in reports.iter().filter(|r| !r.is_ok()) {
                tracing::warn!(
                    library = %r.library,
                    no_mapping = r.no_mapping.count,
                    unreadable = r.unreadable.count,
                    example = r.no_mapping.examples.iter().chain(&r.unreadable.examples).next().map(String::as_str).unwrap_or_default(),
                    "media files in this library cannot be read; run `spoolframe-server plex check-paths`"
                );
            }
        }
        Err(e) => tracing::warn!(error = %e, "cannot check Plex media paths"),
    }
}

async fn check_paths(plex: &PlexCatalog, library: Option<&str>) -> Result<()> {
    let reports = plex.check_paths(library, Scope::All).await?;
    print!("{}", report(&reports));
    let failing = reports.iter().filter(|r| !r.is_ok()).count();
    if failing > 0 {
        bail!(
            "{failing} of {} libraries have media files this host cannot read",
            reports.len()
        );
    }
    Ok(())
}

/// The `check-paths` report: a line of counts per library, then examples
/// of each failure.
fn report(reports: &[LibraryPaths]) -> String {
    let mut out = String::new();
    for r in reports {
        let _ = writeln!(
            out,
            "{}: {} ok, {} with no matching mapping, {} missing or unreadable",
            r.library, r.ok, r.no_mapping.count, r.unreadable.count
        );
        examples(&mut out, "no matching mapping", &r.no_mapping);
        examples(&mut out, "missing or unreadable", &r.unreadable);
    }
    out
}

fn examples(out: &mut String, what: &str, failures: &Failures) {
    if failures.examples.is_empty() {
        return;
    }
    let _ = writeln!(out, "  {what}, e.g.:");
    for e in &failures.examples {
        let _ = writeln!(out, "    {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_counts_each_library_and_shows_examples() {
        let reports = vec![
            LibraryPaths {
                library: "Movies".into(),
                ok: 770,
                ..LibraryPaths::default()
            },
            LibraryPaths {
                library: "Music".into(),
                ok: 1,
                no_mapping: Failures::default(),
                unreadable: Failures {
                    count: 2,
                    examples: vec!["/data/a.flac -> /nas/a.flac: No such file".into()],
                },
            },
        ];
        assert_eq!(
            report(&reports),
            "Movies: 770 ok, 0 with no matching mapping, 0 missing or unreadable\n\
             Music: 1 ok, 0 with no matching mapping, 2 missing or unreadable\n\
             \x20 missing or unreadable, e.g.:\n\
             \x20   /data/a.flac -> /nas/a.flac: No such file\n"
        );
    }
}
