use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use spoolframe_admin_api::RestartMode;
use spoolframe_catalog::{Catalog, CatalogError};
use spoolframe_media::Prober;
use spoolframe_media::bitmap::Budget;
use spoolframe_media::decode;
use spoolframe_media::gpus;
use spoolframe_media::harden;
use spoolframe_media::ocr::Ocr;
use spoolframe_media::sandbox::{self, Sandbox, Tool};
use spoolframe_plex::{Failures, LibraryPaths, Scope};
use spoolframe_server::admin::{self, Admin, CookiePolicy, SetupCode, StatusSource};
use spoolframe_server::app_passkeys::AppPasskeys;
use spoolframe_server::artwork::ArtworkCache;
use spoolframe_server::auth::Auth;
use spoolframe_server::catalog::{Composite, LiveCatalog, PlexSource};
use spoolframe_server::config::layers::{DEFAULTS, Layers, Locations};
use spoolframe_server::config::secrets::Secrets;
use spoolframe_server::config::{ServerConfig, WatchSyncConfig};
use spoolframe_server::connected::Connected;
use spoolframe_server::encode::job::Jobs;
use spoolframe_server::identity;
use spoolframe_server::logs::{LogRing, RingLayer};
use spoolframe_server::metrics::Metrics;
use spoolframe_server::network::{Server, ServerName, Services};
use spoolframe_server::oidc::Oidc;
use spoolframe_server::passkeys::Passkeys;
use spoolframe_server::playback::Playbacks;
use spoolframe_server::playback_debug::PlaybackDebug;
use spoolframe_server::plex_accounts::{PlexAccounts, configured_owner};
use spoolframe_server::plex_sync::WatchSync;
use spoolframe_server::previews::{self, PreviewSources, Previews};
use spoolframe_server::runtime::{self, Live, RuntimeSettings, reload};
use spoolframe_server::store::Store;
use spoolframe_server::subtitles::Subtitles;
use spoolframe_server::telemetry::{OtelLayer, Swap, Telemetry};
use spoolframe_server::transcode::{self, Transcoding};
use spoolframe_server::watch::Watch;
use spoolframe_types::MediaId;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

mod admin_cli;

#[global_allocator]
static ALLOC: Allocator = Allocator;

/// snmalloc, unless the process has a data limit (`RLIMIT_DATA`), as the
/// subtitle decoder this binary also is runs with (finding 45): snmalloc
/// maps more than 256 GiB of address space at its start, which any such
/// limit refuses ("Failed to initialise snmalloc"), so a limited process
/// uses the C library's allocator. Decided at the first allocation, from
/// the limit then, and never changed: every block is freed by the
/// allocator that made it.
struct Allocator;

/// Which allocator: not yet decided, snmalloc, or the system's.
static WHICH: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
const SNMALLOC: u8 = 1;
const SYSTEM: u8 = 2;

impl Allocator {
    fn which() -> u8 {
        use std::sync::atomic::Ordering;
        match WHICH.load(Ordering::Relaxed) {
            0 => {
                let mut limit = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                // SAFETY: getrlimit(2) with a valid pointer; it allocates
                // nothing.
                let limited = unsafe { libc::getrlimit(libc::RLIMIT_DATA, &mut limit) } == 0
                    && limit.rlim_cur != libc::RLIM_INFINITY;
                // Threads that race here read the same limit.
                let which = if limited { SYSTEM } else { SNMALLOC };
                WHICH.store(which, Ordering::Relaxed);
                which
            }
            which => which,
        }
    }
}

// SAFETY: each call goes to one of two allocators, the same one for the
// whole process (see `Allocator`), so a block is always freed or grown by
// the allocator that made it.
unsafe impl std::alloc::GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        unsafe {
            match Self::which() {
                SNMALLOC => snmalloc_rs::SnMalloc.alloc(layout),
                _ => std::alloc::System.alloc(layout),
            }
        }
    }

    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        unsafe {
            match Self::which() {
                SNMALLOC => snmalloc_rs::SnMalloc.alloc_zeroed(layout),
                _ => std::alloc::System.alloc_zeroed(layout),
            }
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        // SAFETY: the caller's contract, passed on.
        unsafe {
            match Self::which() {
                SNMALLOC => snmalloc_rs::SnMalloc.dealloc(ptr, layout),
                _ => std::alloc::System.dealloc(ptr, layout),
            }
        }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new: usize) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        unsafe {
            match Self::which() {
                SNMALLOC => snmalloc_rs::SnMalloc.realloc(ptr, layout, new),
                _ => std::alloc::System.realloc(ptr, layout, new),
            }
        }
    }
}

use admin_cli::{GrantCommand, InviteCommand, UserCommand};

/// How many items per library the startup check samples.
const STARTUP_SAMPLE: u32 = 5;

/// Spoolframe media server.
#[derive(Parser)]
#[command(name = "spoolframe-server")]
#[command(version)]
struct Cli {
    /// The provided config file (UCL, schema `spoolframe.server.v1`), if
    /// any; else `SPOOLFRAME_CONFIG`. Optional: the built-in defaults
    /// (`config defaults`) are a server to set up in the web UI.
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    /// Where the server keeps its key, database, caches and runtime
    /// settings; else `SPOOLFRAME_STATE_DIR`, systemd's `STATE_DIRECTORY`,
    /// `$XDG_STATE_HOME/spoolframe`, or `/var/lib/spoolframe` as root.
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,

    /// Increase log verbosity (-v = debug, -vv = trace).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    /// What to do; without one, run the server.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum RelayCommand {
    /// Run the relay.
    Run,
    /// Print the relay's key: what the main server's
    /// `network.own_relays` names. Creates it if there is none yet.
    Id,
}

#[derive(Subcommand)]
enum Command {
    /// Plex integration tools.
    Plex {
        #[command(subcommand)]
        command: PlexCommand,
    },
    /// A relay of your own (P6-04), for devices that cannot reach the
    /// server directly, in place of n0's public ones. `--config` names
    /// its file (`spoolframe.relay.v1`); its key is kept in the state
    /// directory, apart from the server's.
    Relay {
        #[command(subcommand)]
        command: RelayCommand,
    },
    /// Media file tools.
    Media {
        #[command(subcommand)]
        command: MediaCommand,
    },
    /// Print the server's EndpointId, the identity clients pin, and its
    /// server id, which names its app passkeys' relying party. Creates
    /// either if this is the first start.
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
    /// Make, list and revoke invites (P6-01) through the running server's
    /// admin API, logged in as an admin; and print the server's link.
    Invite {
        #[command(subcommand)]
        command: InviteCommand,
    },
    /// List and revoke the grants of portable users (P7-06) through the
    /// running server's admin API, logged in as an admin.
    Grant {
        #[command(subcommand)]
        command: GrantCommand,
    },
    /// The server's settings, and the layers they come from.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Decode one bitmap subtitle track from standard input (finding 45):
    /// the server runs its own binary with this, confined, rather than
    /// decode a track in its own process. Prints a line per cue, its start
    /// and end in milliseconds.
    #[command(name = "decode-subtitle", hide = true)]
    DecodeSubtitle {
        /// `pgs` (a `.sup`), `dvb` or `vobsub` (ffprobe's JSON of the
        /// track).
        #[arg(long)]
        format: decode::Format,
        /// Where each cue's picture is written, as `{i}.pgm`.
        #[arg(long)]
        pictures: PathBuf,
        /// Cue times count from the start of the file (an embedded track).
        #[arg(long)]
        from_file_start: bool,
    },
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Print the built-in defaults: every setting at its default, with what
    /// it does.
    Defaults,
    /// Print every setting with the layer it comes from: the defaults, the
    /// provided file, or the runtime settings. Secrets only as set or not.
    /// Read from the files, so it works with the server stopped.
    Show,
    /// Change a runtime setting through the running server, logged in as
    /// an admin. It is checked as a config file is; a live setting applies
    /// at once, a restart one at the next start.
    Set {
        /// Dotted, as `config show` names it: `transcode.max_jobs`.
        key: String,
        /// JSON (`3`, `true`, `[{"from": "/a", "to": "/b"}]`) or a plain
        /// string. Left out with `--stdin`.
        value: Option<String>,
        /// Read the value from standard input: for `plex.token`, which
        /// should not be on a command line.
        #[arg(long)]
        stdin: bool,
    },
    /// Take a setting out of the runtime settings: the config file's value
    /// or the default applies again.
    Unset { key: String },
    /// Have the running server read its config file and runtime settings
    /// again, as SIGHUP does, logged in as an admin: the live settings
    /// apply, the restart ones wait; a file that does not load changes
    /// nothing.
    Reload,
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
    /// Sync watch state with each linked user's Plex account now (PX-04,
    /// P3-12), through the running server, logged in as an admin.
    SyncWatch {
        /// Only this user; every linked user if left out.
        #[arg(long)]
        user: Option<String>,
        /// Only print what would change.
        #[arg(long)]
        dry_run: bool,
    },
}

fn main() -> Result<()> {
    // First, before any thread, for every subcommand (PX-31): not
    // dumpable, no new privileges, no core, the seccomp deny-list. The
    // admin commands hold a session's token or read a password; the
    // subtitle decoder, a confined child, loses nothing it uses.
    let hardened = harden::harden().context("cannot harden the server's process")?;
    // What the server writes is its own (PX-11 finding 35): the database
    // holds password and session hashes, the caches what users watch. Its
    // subprocesses (ffmpeg, tesseract, its own subtitle decoder) write into
    // its caches, and inherit this. The module's UMask=0077 says the same;
    // this holds without it.
    // SAFETY: umask(2) only sets the process's file mode mask.
    unsafe {
        libc::umask(0o077);
    }
    // What a restart in place runs again (`restart_in_place`), taken
    // before anything else: the program as it was started.
    let exe = std::env::current_exe();
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let cli = Cli::parse();
    // Before any runtime, thread or log: the decoder is a child the server
    // started, confined, which only reads its input and writes pictures.
    if let Some(Command::DecodeSubtitle {
        format,
        pictures,
        from_file_start,
    }) = &cli.command
    {
        decode_subtitle(*format, pictures, *from_file_start);
    }
    spoolframe_server::tls::install_provider()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("cannot start the async runtime")?;
    let restart = runtime.block_on(serve(cli, hardened));
    // The tasks still running end here, and the media tools they ran are
    // killed with them; one stuck in a blocking call is not waited for.
    runtime.shutdown_timeout(std::time::Duration::from_secs(5));
    match restart? {
        None => Ok(()),
        // Not a success: systemd's Restart=on-failure starts it again.
        Some(RestartMode::Supervisor) => std::process::exit(RESTART_EXIT),
        Some(RestartMode::InPlace) => restart_in_place(exe, &args),
    }
}

/// What `main` did to harden the process, said once the log is up.
fn log_hardened(hardened: &harden::Filter) {
    const FLAGS: &str = "not dumpable (its /proc is root's), no new privileges, no core dumps";
    match hardened {
        harden::Filter::Loaded => tracing::debug!(
            "the server's process is hardened: {FLAGS}, and a seccomp filter refuses the \
             calls it never makes"
        ),
        harden::Filter::Kept => tracing::debug!(
            "the server's process is hardened: {FLAGS}, and its seccomp filter is kept from \
             before the restart"
        ),
        harden::Filter::Unavailable(reason) => tracing::warn!(
            reason,
            "the server's process is hardened: {FLAGS}, but there is no seccomp filter for \
             this machine"
        ),
    }
}

/// Run this binary again in this process, as it was started: the same
/// PID, arguments, environment and standard streams, so a pipe the log
/// goes to (`| tee`) keeps receiving it. Every other descriptor the server
/// opened is close-on-exec, so the exec frees its ports for the new
/// server. `exe` is the path the binary was started from, so a binary
/// rebuilt there since is the one that runs. The environment gains one
/// variable: the seccomp filter survives the exec, and the new start
/// keeps it rather than load a second copy (PX-31). Returns only to exit,
/// non-zero, when the exec fails.
fn restart_in_place(exe: std::io::Result<PathBuf>, args: &[std::ffi::OsString]) -> ! {
    use std::os::unix::process::CommandExt as _;
    reap_children();
    let failed = match exe {
        Ok(exe) => {
            tracing::warn!(
                exe = %exe.display(),
                pid = std::process::id(),
                "restarting in place"
            );
            let mut command = std::process::Command::new(&exe);
            if let Some((arg0, rest)) = args.split_first() {
                command.arg0(arg0).args(rest);
            }
            harden::keep_across_exec(&mut command);
            command.exec()
        }
        Err(e) => e,
    };
    tracing::error!(error = %failed, "cannot run the server again in place: it has stopped");
    std::process::exit(1)
}

/// Reap the media tools killed as the runtime stopped, waiting up to two
/// seconds for them to go: after the exec nothing would, and each would
/// stay a zombie.
fn reap_children() {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let mut status = 0;
        // SAFETY: waitpid(2) with a valid pointer to an int.
        match unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) } {
            reaped if reaped > 0 => {}
            0 if std::time::Instant::now() < until => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            // None left (ECHILD), or one still running after the wait.
            _ => return,
        }
    }
}

/// `decode-subtitle`: the track on standard input decoded within the
/// default budget, its pictures written, its index printed. A failure is
/// one line on stderr, which the server reports, and exit status 1.
fn decode_subtitle(format: decode::Format, pictures: &Path, from_file_start: bool) -> ! {
    let decoded = decode::decode(
        format,
        from_file_start,
        std::io::stdin().lock(),
        pictures,
        &Budget::DEFAULT,
    );
    let printed = decoded.and_then(|times| {
        use std::io::Write as _;
        let mut out = std::io::stdout().lock();
        out.write_all(decode::index(&times).as_bytes())
            .and_then(|()| out.flush())
            .map_err(|e| format!("cannot write the index: {e}"))
    });
    match printed {
        Ok(()) => std::process::exit(0),
        Err(reason) => {
            eprintln!("{reason}");
            std::process::exit(1)
        }
    }
}

/// Run what `cli` says. `Some` when the server stopped to restart, and
/// how it is to start again.
/// `relay run` or `relay id` (P6-04): a relay of the operator's own,
/// from its `config` file, as the key in `state_dir`.
async fn relay(command: RelayCommand, config: Option<PathBuf>, state_dir: &Path) -> Result<()> {
    use spoolframe_server::own_relay;
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("cannot make {}", state_dir.display()))?;
    let key = identity::load_or_create_at(state_dir, identity::RELAY_KEY_FILE)?;
    if matches!(command, RelayCommand::Id) {
        println!("{}", key.public());
        return Ok(());
    }
    let path = config.context("relay run needs --config: a spoolframe.relay.v1 file")?;
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    let settings = own_relay::RelayConfig::parse(&text)
        .with_context(|| format!("{} is not a relay config", path.display()))?;
    for warning in settings.warnings() {
        tracing::warn!("{warning}");
    }
    let server = own_relay::spawn(&settings, key.clone(), state_dir).await?;
    tracing::info!(
        key = %key.public(),
        http = ?server.http_addr(),
        https = ?server.https_addr(),
        owner = settings.access.owner,
        friends = settings.access.friends,
        everyone = settings.access.everyone,
        server = %settings.server.id.fmt_short(),
        "relay listening"
    );
    use tokio::signal::unix::{SignalKind, signal};
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        _ = interrupt.recv() => {}
        _ = terminate.recv() => {}
    }
    tracing::info!("relay stopping");
    server
        .shutdown()
        .await
        .map_err(|e| anyhow::anyhow!("the relay did not stop cleanly: {e}"))?;
    Ok(())
}

async fn serve(cli: Cli, hardened: harden::Filter) -> Result<Option<RestartMode>> {
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
    // And into memory for the web UI's Logs page (P3-14): the same lines,
    // under the same filter.
    let ring = Arc::new(LogRing::default());
    // And, once telemetry.otlp names a collector, to it (P3-14): a slot
    // filled and emptied as the setting changes.
    let (otlp, slot) = tracing_subscriber::reload::Layer::new(None::<OtelLayer>);
    let swap: Swap = Box::new(move |layer| {
        if let Err(e) = slot.reload(layer) {
            tracing::warn!(error = %e, "cannot change the OTLP export's layer");
        }
    });
    tracing_subscriber::registry()
        .with(otlp)
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(RingLayer(ring.clone()))
        .init();
    log_hardened(&hardened);

    if let Some(Command::Config {
        command: ConfigCommand::Defaults,
    }) = cli.command
    {
        print!("{DEFAULTS}");
        return Ok(None);
    }
    // A relay reads no server config: its own file, and its key.
    if let Some(Command::Relay { command }) = cli.command {
        let locations = Locations::from_env(cli.state_dir, None)?;
        relay(command, cli.config, &locations.state_dir).await?;
        return Ok(None);
    }
    let locations = Locations::from_env(cli.state_dir, cli.config)?;
    let layers = Layers::load(&locations)?;
    let config = layers.config()?;
    let state_dir = &locations.state_dir;
    tracing::info!(
        state_dir = %state_dir.display(),
        config = ?locations.config,
        "config loaded"
    );
    // The locked file's shadows and the deprecated keys (PGPU-05), as a
    // reload logs them again.
    reload::shadowed_warnings(&layers);

    let done = match cli.command {
        None => return run(&config, &layers, state_dir, ring, swap).await,
        Some(Command::Config {
            command: ConfigCommand::Defaults,
        }) => unreachable!("printed before the layers are read"),
        Some(Command::DecodeSubtitle { .. }) => unreachable!("decoded before the runtime starts"),
        Some(Command::Relay { .. }) => unreachable!("run before the config is read"),
        Some(Command::Config {
            command: ConfigCommand::Show,
        }) => {
            print!("{}", layers.show(&Secrets::new(state_dir)));
            Ok(())
        }
        Some(Command::Config {
            command: ConfigCommand::Set { key, value, stdin },
        }) => {
            let value = match (value, stdin) {
                (Some(value), false) => value,
                (None, true) => {
                    let mut line = String::new();
                    std::io::stdin().read_line(&mut line)?;
                    line.trim_end_matches(['\r', '\n']).to_owned()
                }
                _ => bail!("give the value, or --stdin"),
            };
            admin_cli::config_change(config.admin.listen()?, &key, Some(&value)).await
        }
        Some(Command::Config {
            command: ConfigCommand::Unset { key },
        }) => admin_cli::config_change(config.admin.listen()?, &key, None).await,
        Some(Command::Config {
            command: ConfigCommand::Reload,
        }) => admin_cli::config_reload(config.admin.listen()?).await,
        Some(Command::Setup {
            code,
            username,
            password_stdin,
        }) => admin_cli::setup(config.admin.listen()?, code, username, password_stdin).await,
        Some(Command::User { command }) => admin_cli::user(config.admin.listen()?, command).await,
        Some(Command::Invite { command }) => {
            admin_cli::invite(config.admin.listen()?, command).await
        }
        Some(Command::Grant { command }) => admin_cli::grant(config.admin.listen()?, command).await,
        Some(Command::Status) => admin_cli::status(config.admin.listen()?).await,
        Some(Command::Id) => {
            let key = identity::load_or_create(state_dir)?;
            let server_id = identity::load_or_create_server_id(state_dir)?;
            println!("endpoint id: {}", key.public());
            println!("server id:   {server_id}");
            Ok(())
        }
        Some(Command::Fingerprint) => {
            println!(
                "{}",
                identity::fingerprint(&identity::load_or_create(state_dir)?)
            );
            Ok(())
        }
        Some(Command::Plex {
            command: PlexCommand::CheckPaths { library },
        }) => check_paths(&plex(&config, &layers, state_dir)?, library.as_deref()).await,
        Some(Command::Plex {
            command: PlexCommand::SyncWatch { user, dry_run },
        }) => admin_cli::plex_sync(config.admin.listen()?, user, dry_run).await,
        Some(Command::Media {
            command: MediaCommand::Probe { id },
        }) => {
            probe(
                &plex(&config, &layers, state_dir)?,
                &id,
                Tool::new(config.media.ffprobe(), media_sandbox(&config, state_dir)?),
            )
            .await
        }
    };
    done.map(|()| None)
}

/// Every Plex server and library, for the commands that read them
/// directly.
fn plex(config: &ServerConfig, layers: &Layers, state_dir: &Path) -> Result<Composite> {
    match runtime::plex_catalog(config, layers, &Secrets::new(state_dir))? {
        Some(plex) => Ok(plex),
        None => bail!("this server is not set up: no Plex server is configured (plex.url)"),
    }
}

/// How ffmpeg, ffprobe and tesseract run (PX-11 finding 34): confined as
/// `media.sandbox` says, kept out of the state directory and systemd's
/// credentials, with no GPU's device nodes: a run on a GPU is given its
/// one card's (PGPU-05, `Transcoding`).
fn media_sandbox(config: &ServerConfig, state_dir: &Path) -> Result<Sandbox> {
    let mut hidden = vec![state_dir.to_path_buf()];
    hidden.extend(std::env::var_os("CREDENTIALS_DIRECTORY").map(PathBuf::from));
    Ok(Sandbox::new(
        config.media.sandbox()?,
        hidden,
        sandbox::GpuDevices::default(),
    )?)
}

/// Shows the media tools confined here, in this service's own sandbox
/// (PX-11 finding 34): a confined ffprobe must be refused the state
/// directory and loopback. `required` does not start without that shown;
/// `auto` says so at the error level.
async fn check_media_sandbox(
    config: &ServerConfig,
    ffprobe: &Tool,
    state_dir: &Path,
) -> Result<()> {
    if !ffprobe.sandbox().confines() {
        return Ok(());
    }
    match ffprobe.check_confined(state_dir).await {
        // A pass is what is expected: only a failure is worth a line at
        // INFO or above.
        Ok(shown) => {
            tracing::debug!("{shown}");
            Ok(())
        }
        Err(reason) if config.media.sandbox()? == sandbox::Mode::Required => bail!(
            "media.sandbox is \"required\", but the media tools cannot be shown to be confined: \
             {reason}"
        ),
        Err(reason) => {
            tracing::error!(
                reason,
                "MEDIA TOOLS MAY NOT BE CONFINED: a confined ffprobe was not refused the state \
                 directory and loopback, as it should be. Set media.sandbox = \"required\" to \
                 refuse to start then"
            );
            Ok(())
        }
    }
}

/// Shows the subtitle decoder (this binary, run again) works here, and,
/// confined, that it may not write the state directory (finding 45), as
/// [`check_media_sandbox`] shows the tools. `required` does not start
/// without that shown; otherwise it is logged at the error level.
async fn check_decoder(
    config: &ServerConfig,
    decoder: &Tool,
    scratch: &Path,
    state_dir: &Path,
) -> Result<()> {
    match decode::check(decoder, scratch, state_dir).await {
        // A pass is what is expected: only a failure is worth a line at
        // INFO or above.
        Ok(shown) => {
            tracing::debug!("{shown}");
            Ok(())
        }
        Err(reason) if config.media.sandbox()? == sandbox::Mode::Required => bail!(
            "media.sandbox is \"required\", but the subtitle decoder cannot be shown to run \
             confined: {reason}"
        ),
        Err(reason) => {
            tracing::error!(
                reason,
                "SUBTITLE DECODER MAY NOT WORK OR NOT BE CONFINED: picture subtitles may fail to \
                 read as text"
            );
            Ok(())
        }
    }
}

async fn probe(catalog: &dyn Catalog, id: &MediaId, ffprobe: Tool) -> Result<()> {
    let path = catalog.media_path(id).await?;
    let prober = Prober::new(ffprobe, Prober::DEFAULT_TIMEOUT);
    let descriptor = prober.describe(&path).await?;
    println!("{}", serde_json::to_string_pretty(&*descriptor)?);
    Ok(())
}

/// Run the server until SIGINT or SIGTERM, or until an admin asks for a
/// restart: then `Some`, and how it is to start again.
async fn run(
    config: &ServerConfig,
    layers: &Layers,
    state_dir: &Path,
    logs: Arc<LogRing>,
    swap: Swap,
) -> Result<Option<RestartMode>> {
    logs.set_capacity(config.logs.buffer_lines()?);
    // Before anything is logged: whatever waits for "listening" (a service
    // manager, the CLI tests) may send SIGTERM the moment it sees it, and a
    // signal with no handler yet kills the process instead of stopping it.
    let shutdown = shutdown_signal()?;
    // SIGHUP too (PX-33): unhandled, it would end the process.
    let hangup = reload::listen();
    // The key first: if it cannot be loaded nothing else should start.
    let key = identity::load_or_create(state_dir)?;
    let server_id = identity::load_or_create_server_id(state_dir)?;
    let endpoint_id = key.public().to_string();
    // What a person compares with a client's screen before pinning (P1-21).
    tracing::info!(
        server = config.server_name(),
        fingerprint = identity::fingerprint(&key),
        %server_id,
        "server identity"
    );
    let plex = runtime::plex_catalog(config, layers, &Secrets::new(state_dir))?.map(Arc::new);
    let live = Arc::new(LiveCatalog::new(plex.clone()));
    let catalog: Arc<dyn Catalog> = live.clone();
    let catalog_changes = live.changes();
    match &plex {
        Some(plex) => check_catalog(plex).await,
        None => tracing::warn!(
            "not set up: no Plex server is configured, so clients are told the server is not \
             set up yet; set it up in the web UI"
        ),
    }
    // The caches under the state directory, whatever an older server
    // left: each cache closes its own as well.
    spoolframe_server::files::private_dir(&state_dir.join("cache"))
        .with_context(|| format!("cannot prepare the caches in {}", state_dir.display()))?;
    let sandbox = media_sandbox(config, state_dir)?;
    let ffmpeg = Tool::new(config.media.ffmpeg(), sandbox.clone());
    let ffprobe = Tool::new(config.media.ffprobe(), sandbox.clone());
    check_media_sandbox(config, &ffprobe, state_dir).await?;
    // Bitmap subtitles are decoded by this binary run again, confined as
    // the tools are (finding 45). /proc/self/exe now: the program running,
    // whatever PATH or a later upgrade says.
    let decoder = Tool::new(
        std::env::current_exe().context("cannot find the server's own binary")?,
        sandbox.clone(),
    );
    let prober = Arc::new(Prober::new(ffprobe.clone(), Prober::DEFAULT_TIMEOUT));
    reload::config_warnings(config);
    let transcoding = Arc::new(
        Transcoding::new(
            ffmpeg.clone(),
            config.transcode.gpus()?,
            state_dir.join("cache").join("encoder-test"),
        )
        .with_settings(config.transcode.settings(&config.experimental)?)
        .on_host(
            gpus::host_cards(&gpus::HostRoots::default()),
            Path::new("/dev"),
        ),
    );
    transcode::spawn_detection(transcoding.clone());
    let limits = config.transcode.limits(state_dir)?;
    let jobs = Arc::new(Jobs::new(limits.clone(), ffmpeg.clone()).with_context(|| {
        format!(
            "cannot prepare the scratch directory {}",
            limits.scratch_dir.display()
        )
    })?);
    let ocr_settings = config.subtitles.ocr_settings(state_dir)?;
    let ocr = if ocr_settings.enabled {
        Ocr::detect(&Tool::new(config.media.tesseract(), sandbox.clone())).await
    } else {
        Ocr::disabled()
    };
    let subtitles = Arc::new(
        Subtitles::new(ocr, ffmpeg.clone(), ffprobe, decoder.clone(), &ocr_settings).with_context(
            || {
                format!(
                    "cannot prepare the subtitle cache {}",
                    ocr_settings.cache_dir.display()
                )
            },
        )?,
    );
    if ocr_settings.enabled {
        check_decoder(
            config,
            &decoder,
            &ocr_settings.cache_dir.join("decoder-check"),
            state_dir,
        )
        .await?;
    }
    let previews_dir = state_dir.join("cache").join("previews");
    let previews = Previews::start(
        &previews_dir,
        config.previews.settings()?,
        PreviewSources {
            catalog: catalog.clone(),
            prober: prober.clone(),
            transcoding: transcoding.clone(),
            ffmpeg,
            encoding: jobs.encoding(),
        },
    )
    .with_context(|| format!("cannot open the previews cache {}", previews_dir.display()))?;
    let name = ServerName::new(config.server_name());
    let status = Arc::new(
        StatusSource::new(name.clone(), server_id.clone(), transcoding.clone())
            .with_jobs(jobs.clone())
            .with_mdns(config.network.endpoint_options()?.mdns)
            .with_ocr_languages(subtitles.ocr().languages().iter().cloned().collect()),
    );
    let artwork_dir = state_dir.join("cache").join("artwork");
    let artwork = Arc::new(
        ArtworkCache::open(&artwork_dir, config.artwork.cache_bytes(), catalog.clone())
            .with_context(|| format!("cannot open the artwork cache {}", artwork_dir.display()))?,
    );

    let store = Store::open(state_dir).await?;
    // Closed last, as the server stops.
    let database = store.clone();
    // Previews made ahead (P3-13) start with what is being watched.
    previews::ahead::spawn(&previews, catalog.clone(), store.clone());
    let auth = Auth::new(store).with_trust_idle(config.auth.trust_idle()?);
    let watch = Arc::new(Watch::new(
        auth.store().clone(),
        catalog.clone(),
        prober.clone(),
        config.watch.rules()?,
    ));
    let wizard = wizard()?;
    let accounts = Arc::new(PlexAccounts::new(
        Secrets::new(state_dir),
        auth.store().clone(),
        wizard.plex_site(),
        configured_owner(config, auth.store()).await?,
    ));
    // Always: a server set up in the wizard, or a user who links, syncs
    // without a restart.
    let plex_sync = Arc::new(WatchSync::new(
        live.clone(),
        accounts.clone(),
        watch.rules().started_after,
        config
            .plex
            .as_ref()
            .map_or(WatchSyncConfig::DEFAULT_INTERVAL, |p| p.sync_interval()),
    ));
    plex_sync.start(watch.changes());
    let admin_listen = config.admin.listen()?;
    let admin_origins = config.admin.origins()?;
    let admin_proxies = config.admin.trusted_proxies()?;
    let public_url = admin_origins.public_url().map(str::to_owned);
    let cookies = CookiePolicy::new(config.admin.insecure_cookies());
    if cookies.insecure() {
        tracing::warn!(
            "admin.insecure_cookies is on: the session cookie is sent without Secure, so it \
             crosses the network in clear"
        );
    } else if !admin_listen.ip().is_loopback() && public_url.is_none() {
        tracing::warn!(
            listen = %admin_listen,
            "admin.public_url is not set: browsers keep the session cookie only over HTTPS or at \
             localhost, so signing in to the web UI at this address will appear to do nothing"
        );
    }
    // Without admin.not_proxied the config would not have loaded (PX-11
    // finding 38).
    if config.admin.proxy_unnamed()? {
        tracing::info!(
            listen = %admin_listen,
            "admin.not_proxied: nothing proxies to this loopback listener, so its callers are \
             taken as local and share one failure limit"
        );
    }
    // Finding 58: on every interface, a proxy on this host is as likely
    // as browsers coming straight in, so this is said, not refused.
    if config.network.own_relays_untold()? {
        tracing::warn!(
            "network.own_relays names relays of your own, but network.relays is empty: the \
             server answers them, yet tells no device about a relay, so only devices that \
             know a relay's URL another way use it. Add each relay's URL to network.relays"
        );
    }
    if config.admin.proxy_maybe_unnamed()? {
        tracing::warn!(
            listen = %admin_listen,
            "admin.trusted_proxies is empty, but admin.public_url is set: if a proxy on this \
             host is in front, every caller through it looks like loopback, so failed logins \
             are not limited per caller, and /metrics, when on, answers whoever the proxy lets \
             in. Set admin.trusted_proxies to the proxy's address (\"127.0.0.1\" for one on \
             this host), better with admin.listen on loopback; or, if nothing proxies to this \
             listener, set admin.not_proxied = true"
        );
    }
    // Without a public URL, a browser's page is the listener's own port
    // (PX-09 finding 28).
    let passkeys = Arc::new(Passkeys::at(
        &admin_origins,
        &server_id,
        Some(admin_listen.port()).filter(|port| *port != 0),
    ));
    match (passkeys.rp_id(), passkeys.unavailable()) {
        (Some(rp_id), _) => tracing::info!(rp_id, "passkeys are bound to this host"),
        (None, why) => tracing::warn!(why, "no passkeys in the web UI"),
    }
    tracing::info!(
        rp_id = server_id.rp_id(),
        "the apps' passkeys are bound to this server's relying party"
    );
    passkeys.log_orphans(auth.store()).await?;
    // Single sign-on (P3-04): its providers are a live setting.
    let oidc = Arc::new(Oidc::new(
        public_url.as_deref(),
        Some(Secrets::new(state_dir)),
    ));
    oidc.set_providers(&config.auth.oidc);
    if !config.auth.oidc.is_empty() {
        tracing::info!(providers = oidc.names().join(", "), "single sign-on");
    }
    // Said by whatever starts the server again when it exits for a restart
    // (the NixOS module's unit does); without it, the server runs itself
    // again in place. Not systemd's INVOCATION_ID: every process started
    // from a service inherits it, a shell and what it runs included.
    let restart_mode = if std::env::var_os("SPOOLFRAME_SUPERVISED").is_some_and(|v| !v.is_empty()) {
        RestartMode::Supervisor
    } else {
        RestartMode::InPlace
    };
    // GET /metrics (P3-14): its settings are live.
    let metrics = Arc::new(Metrics::default());
    let telemetry = Arc::new(Telemetry::new(
        swap,
        metrics.clone(),
        name.clone(),
        endpoint_id.clone(),
    ));
    telemetry.apply(config.telemetry.otlp.settings()?);
    metrics
        .access()
        .set(config.admin.metrics(), config.admin.metrics_from()?);
    metrics.observe_catalog(live.clone());
    metrics.spawn_sampler();
    // The WAN uplink budget (PX-46): a live setting, read by the client
    // protocol's plans and the admin API's measurement.
    let uplink = Arc::new(spoolframe_server::uplink::Uplink::new(
        config.network.uplink()?,
    ));
    let settings = Arc::new(RuntimeSettings::new(
        layers.clone(),
        state_dir,
        Live {
            uplink: Some(uplink.clone()),
            name: Some(name.clone()),
            catalog: live.clone(),
            cookies: cookies.clone(),
            watch: Some(watch.clone()),
            jobs: Some(jobs.clone()),
            previews: Some(previews.clone()),
            oidc: Some(oidc.clone()),
            metrics: Some(metrics.clone()),
            logs: Some(logs.clone()),
            telemetry: Some(telemetry.clone()),
            transcoding: Some(transcoding.clone()),
        },
        restart_mode,
    ));
    if let Some(hangup) = hangup {
        reload::on_hangup(hangup, settings.clone());
    }
    // Shared by the client protocol, which opens and plays them, and the
    // admin API's playback debug (P3-15), which looks at them.
    let playbacks: Arc<Playbacks> = Arc::default();
    let connected: Arc<Connected> = Arc::default();
    // Only a server with no users offers setup, and only to someone who
    // can read this log.
    let setup_code = (auth.store().user_count().await? == 0).then(SetupCode::generate);
    let (admin_addr, admin) = admin::serve(
        admin_listen,
        Admin {
            auth: auth.clone(),
            setup_code: setup_code.clone(),
            status: status.clone(),
            plex_sync: Some(plex_sync.clone()),
            accounts: Some(accounts.clone()),
            plex: live,
            passkeys,
            cookies,
            settings: Some(settings.clone()),
            wizard,
            oidc,
            watch: Some(watch.clone()),
            artwork: Some(artwork.clone()),
            metrics: Some(metrics.clone()),
            logs: Some(logs),
            telemetry: Some(telemetry.clone()),
            playback: Some(Arc::new(PlaybackDebug {
                playbacks: playbacks.clone(),
                connected: connected.clone(),
                catalog: catalog.clone(),
                prober: prober.clone(),
                transcoding: transcoding.clone(),
                subtitles: subtitles.clone(),
                previews: previews.clone(),
                jobs: Some(jobs.clone()),
            })),
        },
        admin_origins,
        admin_proxies,
    )
    .await
    .with_context(|| format!("cannot start the admin API on {admin_listen}"))?;
    let urls = admin::web_urls(admin_addr);
    status.set_web_urls(urls.clone());
    tracing::info!(addr = %admin_addr, web = urls.join(" "), "admin API listening");
    if let Some(code) = setup_code {
        admin::log_setup_code(&urls, &code);
    } else if !auth.store().an_admin_has_a_password().await? {
        // Set up before the rule (PX-22's follow-up): said, not refused.
        tracing::warn!(
            "no enabled admin has a password: if their passkeys are lost, only a shell gets \
             in; give one a password in My profile, or with `spoolframe-server user passwd <name>`"
        );
    }
    let server = Server::start(
        key,
        &config.network.endpoint_options()?,
        &name,
        Services {
            public_url,
            plex_accounts: Some(accounts),
            plex_sync: Some(plex_sync),
            auth,
            catalog,
            catalog_changes: Some(catalog_changes),
            prober,
            artwork,
            playbacks: playbacks.clone(),
            connected: connected.clone(),
            app_passkeys: Arc::new(AppPasskeys::new(server_id)),
            watch,
            transcoding,
            jobs,
            subtitles,
            previews,
            metrics,
            wan: spoolframe_server::wan::Finders::system(),
            uplink: uplink.clone(),
        },
    )
    .await?;
    status.set_endpoint_id(server.id().to_string());
    let addrs: Vec<_> = server.addr().ip_addrs().map(ToString::to_string).collect();
    status.set_addresses(addrs.clone());
    let told = server.told();
    status.set_told(told.clone());
    let told: Vec<_> = tokio::task::spawn_blocking(move || told.addrs())
        .await
        .unwrap_or_default()
        .iter()
        .map(ToString::to_string)
        .collect();
    // The PID: a restart in place keeps it. The wire revision, so which
    // apps it takes is in the log, not only in a refused Hello.
    tracing::info!(
        endpoint_id = %server.id(),
        ?addrs,
        ?told,
        pid = std::process::id(),
        protocol = spoolframe_protocol::PROTOCOL_VERSION,
        version = env!("CARGO_PKG_VERSION"),
        "listening"
    );

    let restart = tokio::select! {
        _ = shutdown => None,
        _ = settings.restart_requested() => Some(settings.restart_mode()),
    };
    if restart.is_some() {
        // admin.restart's answer is on its way to the browser that asked:
        // time for it to get there before the connection closes.
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    tracing::info!(restart = ?restart, "shutting down");
    server.shutdown().await;
    let _ = admin.stop();
    admin.stopped().await;
    // What is queued for the collector goes before the process does.
    telemetry.shutdown().await;
    if tokio::time::timeout(std::time::Duration::from_secs(5), database.close())
        .await
        .is_err()
    {
        tracing::warn!("the database did not close within 5 seconds");
    }
    Ok(restart)
}

/// What the setup wizard reaches beyond the server: plex.tv, and Plex's
/// discovery on the LAN, unless `SPOOLFRAME_PLEX_TV_URL`,
/// `SPOOLFRAME_PLEX_APP_URL` and `SPOOLFRAME_GDM_TARGETS` (addresses,
/// comma-separated) name stand-ins: for trying the wizard without the real
/// plex.tv, or a real PMS on the LAN.
fn wizard() -> Result<admin::Wizard> {
    let mut wizard = admin::Wizard::default();
    if let Ok(list) = std::env::var("SPOOLFRAME_GDM_TARGETS") {
        wizard.gdm = list
            .split(',')
            .map(|a| a.trim().parse())
            .collect::<Result<_, _>>()
            .context("SPOOLFRAME_GDM_TARGETS")?;
    }
    if let Ok(url) = std::env::var("SPOOLFRAME_PLEX_TV_URL") {
        wizard.plex_tv = url.parse().context("SPOOLFRAME_PLEX_TV_URL")?;
    }
    if let Ok(url) = std::env::var("SPOOLFRAME_PLEX_APP_URL") {
        wizard.plex_app = url.parse().context("SPOOLFRAME_PLEX_APP_URL")?;
    }
    Ok(wizard)
}

/// The exit status of a stop for a restart (EX_TEMPFAIL): a failure to a
/// supervisor, which starts the server again.
const RESTART_EXIT: i32 = 75;

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

/// Check each Plex server and a sample of media paths, and warn about
/// what is wrong. Plex being down must not stop the server (§32): catalog
/// calls report their own errors until it is back, and a server that is
/// down makes only its libraries unavailable (P3-11).
async fn check_catalog(plex: &Composite) {
    match plex.libraries().await {
        Ok(libraries) => tracing::info!(count = libraries.len(), "Plex catalog ready"),
        Err(e) => {
            tracing::warn!(error = %e, "Plex catalog unavailable");
            return;
        }
    }
    for server in plex.servers() {
        check_server(server).await;
    }
}

async fn check_server(server: &PlexSource) {
    if let Err(e) = server.catalog.identity().await {
        tracing::warn!(server = server.name, error = %e, "Plex server unavailable");
        return;
    }
    // A few files per library, so a bad path mapping shows in the log at
    // startup rather than when somebody presses play.
    match server
        .catalog
        .check_paths(None, Scope::Sample(STARTUP_SAMPLE))
        .await
    {
        Ok(reports) => {
            for r in reports.iter().filter(|r| !r.is_ok()) {
                tracing::warn!(
                    server = server.name,
                    library = %r.library,
                    no_mapping = r.no_mapping.count,
                    unreadable = r.unreadable.count,
                    example = r.no_mapping.examples.iter().chain(&r.unreadable.examples).next().map(String::as_str).unwrap_or_default(),
                    "media files in this library cannot be read; run `spoolframe-server plex check-paths`"
                );
            }
        }
        Err(e) => tracing::warn!(server = server.name, error = %e, "cannot check Plex media paths"),
    }
}

/// `plex check-paths`: every served library of every Plex server, or the
/// one titled `library` in Plex.
async fn check_paths(plex: &Composite, library: Option<&str>) -> Result<()> {
    let mut reports = Vec::new();
    for server in plex.servers() {
        match server.catalog.check_paths(library, Scope::All).await {
            Ok(found) => reports.extend(found.into_iter().map(|mut r| {
                if plex.servers().len() > 1 {
                    r.library = format!("{} · {}", server.name, r.library);
                }
                r
            })),
            // Asked for one library: it is on one of the servers.
            Err(CatalogError::NotFound(_)) if library.is_some() => {}
            Err(e) => return Err(e).with_context(|| format!("Plex server {}", server.name)),
        }
    }
    if let Some(wanted) = library
        && reports.is_empty()
    {
        bail!("no Plex library {wanted:?} is served");
    }
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
                ..LibraryPaths::default()
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
