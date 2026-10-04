use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use spoolframe_catalog::Catalog;
use spoolframe_media::Prober;
use spoolframe_media::encoders::FfmpegCommand;
use spoolframe_media::ocr::Ocr;
use spoolframe_plex::{Failures, LibraryPaths, PlexCatalog, Scope};
use spoolframe_server::admin::{self, Admin, SetupCode, StatusSource};
use spoolframe_server::artwork::ArtworkCache;
use spoolframe_server::auth::Auth;
use spoolframe_server::config::ServerConfig;
use spoolframe_server::encode::job::Jobs;
use spoolframe_server::identity;
use spoolframe_server::network::{Server, Services};
use spoolframe_server::passkeys::Passkeys;
use spoolframe_server::plex_sync::PlexSync;
use spoolframe_server::principal::Principal;
use spoolframe_server::store::Store;
use spoolframe_server::subtitles::Subtitles;
use spoolframe_server::transcode::{self, Transcoding};
use spoolframe_server::watch::Watch;
use spoolframe_types::MediaId;
use tracing_subscriber::EnvFilter;

mod admin_cli;

#[global_allocator]
static ALLOC: snmalloc_rs::SnMalloc = snmalloc_rs::SnMalloc;

use admin_cli::UserCommand;

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
    /// Print the server key's fingerprint, to compare with what a client
    /// shows before it pins this server. Works with the server stopped;
    /// creates the key if this is the first start, as `id` does.
    Fingerprint,
    /// The running server's name, key, version and encoders, as an admin
    /// sees them.
    Status,
    /// Register the first admin on a server that has no users yet. The
    /// server must be running.
    Setup {
        /// The setup code from the server's log; prompted for if left out.
        #[arg(long)]
        code: Option<String>,
        /// Prompted for if left out.
        #[arg(long)]
        username: Option<String>,
        /// Read the password from standard input instead of prompting.
        #[arg(long)]
        password_stdin: bool,
    },
    /// Manage users through the running server's admin API, logged in as an
    /// admin.
    User {
        #[command(subcommand)]
        command: UserCommand,
    },
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
    /// Sync watch state with the Plex account now (PX-04), through the
    /// running server, logged in as an admin.
    SyncWatch {
        /// Only print what would change.
        #[arg(long)]
        dry_run: bool,
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
        Some(Command::Setup {
            code,
            username,
            password_stdin,
        }) => admin_cli::setup(config.admin.listen()?, code, username, password_stdin).await,
        Some(Command::User { command }) => admin_cli::user(config.admin.listen()?, command).await,
        Some(Command::Status) => admin_cli::status(config.admin.listen()?).await,
        Some(Command::Id) => {
            println!("{}", identity::load_or_create(&config.state_dir)?.public());
            Ok(())
        }
        Some(Command::Fingerprint) => {
            println!(
                "{}",
                identity::fingerprint(&identity::load_or_create(&config.state_dir)?)
            );
            Ok(())
        }
        Some(Command::Plex {
            command: PlexCommand::CheckPaths { library },
        }) => check_paths(&plex(&config)?, library.as_deref()).await,
        Some(Command::Plex {
            command: PlexCommand::SyncWatch { dry_run },
        }) => admin_cli::plex_sync(config.admin.listen()?, dry_run).await,
        Some(Command::Media {
            command: MediaCommand::Probe { id },
        }) => probe(&plex(&config)?, &id, config.media.ffprobe()).await,
    }
}

fn plex(config: &ServerConfig) -> Result<PlexCatalog> {
    Ok(PlexCatalog::new(config.plex.url()?, config.plex.token()?)?
        .with_paths(config.plex.path_map()?))
}

async fn probe(catalog: &dyn Catalog, id: &MediaId, ffprobe: PathBuf) -> Result<()> {
    let path = catalog.media_path(id).await?;
    let prober = Prober::new(ffprobe, Prober::DEFAULT_TIMEOUT);
    let descriptor = prober.describe(&path).await?;
    println!("{}", serde_json::to_string_pretty(&*descriptor)?);
    Ok(())
}

/// Run the server until SIGINT or SIGTERM.
async fn run(config: &ServerConfig) -> Result<()> {
    // Before anything is logged: whatever waits for "listening" (a service
    // manager, the CLI tests) may send SIGTERM the moment it sees it, and a
    // signal with no handler yet kills the process instead of stopping it.
    let shutdown = shutdown_signal()?;
    // The key first: if it cannot be loaded nothing else should start.
    let key = identity::load_or_create(&config.state_dir)?;
    // What a person compares with a client's screen before pinning (P1-21).
    tracing::info!(
        server = config.server_name(),
        fingerprint = identity::fingerprint(&key),
        "server identity"
    );
    let plex = Arc::new(plex(config)?);
    let catalog: Arc<dyn Catalog> = plex.clone();
    check_catalog(&plex, catalog.as_ref()).await;
    let prober = Arc::new(Prober::new(config.media.ffprobe(), Prober::DEFAULT_TIMEOUT));
    let transcoding = Arc::new(Transcoding::new(
        Box::new(FfmpegCommand::new(config.media.ffmpeg())),
        config.transcode.settings()?,
        config.state_dir.join("cache").join("encoder-test"),
    ));
    transcode::spawn_detection(transcoding.clone());
    let limits = config.transcode.limits(&config.state_dir)?;
    let jobs = Arc::new(
        Jobs::new(limits.clone(), config.media.ffmpeg()).with_context(|| {
            format!(
                "cannot prepare the scratch directory {}",
                limits.scratch_dir.display()
            )
        })?,
    );
    let ocr_settings = config.subtitles.ocr_settings(&config.state_dir)?;
    let ocr = if ocr_settings.enabled {
        Ocr::detect(&config.media.tesseract()).await
    } else {
        Ocr::disabled()
    };
    let subtitles = Arc::new(
        Subtitles::new(ocr, config.media.ffmpeg(), &ocr_settings).with_context(|| {
            format!(
                "cannot prepare the subtitle cache {}",
                ocr_settings.cache_dir.display()
            )
        })?,
    );
    let status = Arc::new(
        StatusSource::new(config.server_name(), transcoding.clone())
            .with_mdns(config.network.endpoint_options()?.mdns),
    );
    let artwork_dir = config.state_dir.join("cache").join("artwork");
    let artwork = Arc::new(
        ArtworkCache::open(&artwork_dir, config.artwork.cache_bytes(), catalog.clone())
            .with_context(|| format!("cannot open the artwork cache {}", artwork_dir.display()))?,
    );

    let store = Store::open(&config.state_dir).await?;
    let auth = Auth::new(store);
    let watch = Arc::new(Watch::new(
        auth.store().clone(),
        catalog.clone(),
        prober.clone(),
        config.watch.rules()?,
    ));
    let plex_sync = plex_sync(config, &plex, auth.store(), &watch).await?;
    let admin_listen = config.admin.listen()?;
    let admin_origins = config.admin.origins()?;
    let admin_proxies = config.admin.trusted_proxies()?;
    let public_url = admin_origins.public_url().map(str::to_owned);
    if !admin_listen.ip().is_loopback() && public_url.is_none() {
        tracing::warn!(
            listen = %admin_listen,
            "admin.public_url is not set: browsers keep the session cookie only over HTTPS or at \
             localhost, so signing in to the web UI at this address will appear to do nothing"
        );
    }
    let passkeys = Arc::new(Passkeys::new(&admin_origins));
    match (passkeys.rp_id(), passkeys.unavailable()) {
        (Some(rp_id), _) => tracing::info!(rp_id, "passkeys are bound to this host"),
        (None, why) => tracing::warn!(why, "no passkeys"),
    }
    passkeys.log_orphans(auth.store()).await?;
    // Only a server with no users offers setup, and only to someone who
    // can read this log.
    let setup_code = if auth.store().user_count().await? == 0 {
        let code = SetupCode::generate();
        tracing::warn!(
            "no users yet: register the first admin with `spoolframe-server setup`; the setup code is {code}"
        );
        Some(code)
    } else {
        None
    };
    let (admin_addr, admin) = admin::serve(
        admin_listen,
        Admin {
            auth: auth.clone(),
            setup_code,
            status: status.clone(),
            plex_sync,
            plex: Some(plex.clone()),
            passkeys,
        },
        admin_origins,
        admin_proxies,
    )
    .await
    .with_context(|| format!("cannot start the admin API on {admin_listen}"))?;
    tracing::info!(addr = %admin_addr, "admin API listening");
    let server = Server::start(
        key,
        &config.network.endpoint_options()?,
        config.server_name(),
        Services {
            public_url,
            auth,
            catalog,
            prober,
            artwork,
            playbacks: Arc::default(),
            watch,
            transcoding,
            jobs,
            subtitles,
        },
    )
    .await?;
    status.set_endpoint_id(server.id().to_string());
    let addrs: Vec<_> = server.addr().ip_addrs().map(ToString::to_string).collect();
    status.set_addresses(addrs.clone());
    tracing::info!(endpoint_id = %server.id(), ?addrs, "listening");

    shutdown.await;
    tracing::info!("shutting down");
    server.shutdown().await;
    let _ = admin.stop();
    admin.stopped().await;
    Ok(())
}

/// The Plex watch sync, started, if the config asks for one and its user
/// exists.
async fn plex_sync(
    config: &ServerConfig,
    plex: &Arc<PlexCatalog>,
    store: &Store,
    watch: &Arc<Watch>,
) -> Result<Option<Arc<PlexSync>>> {
    let Some(sync) = &config.plex.watch_sync else {
        return Ok(None);
    };
    let Some(user) = store.password_login(&sync.user).await? else {
        tracing::warn!(
            user = sync.user,
            "plex.watch_sync.user does not exist; not syncing watch state with Plex"
        );
        return Ok(None);
    };
    let started = config.watch.rules()?.started_after;
    let syncer = Arc::new(PlexSync::new(
        plex.clone(),
        store.clone(),
        Principal::LocalUser(user.id),
        started,
        sync.interval(),
    ));
    syncer.clone().spawn(watch.changes());
    tracing::info!(user = sync.user, "syncing watch state with Plex");
    Ok(Some(syncer))
}

/// SIGINT or SIGTERM, handled from now on. `ctrl_c()` would only start
/// handling SIGINT once polled.
fn shutdown_signal() -> Result<impl Future<Output = ()>> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    Ok(async move {
        tokio::select! {
            _ = interrupt.recv() => {}
            _ = terminate.recv() => {}
        }
    })
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
