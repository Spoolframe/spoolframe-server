//! `spoolframe-server setup`, `spoolframe-server user …` and
//! `spoolframe-server invite …`: clients of the running server's admin
//! API. They never open the database.
//!
//! The admin logs in like any user. For scripts, `SPOOLFRAME_ADMIN_USER`
//! and `SPOOLFRAME_ADMIN_PASSWORD` stand in for the prompts, and
//! `--password-stdin` reads a new password from standard input.

use std::io::{BufRead, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use jsonrpsee::core::ClientError;
use jsonrpsee::ws_client::{HeaderMap, WsClient, WsClientBuilder};
use spoolframe_admin_api::{
    AdminApiClient, Credentials, InviteKind, MeApiClient, NewInvite, NewUser, Role, SetupRequest,
    error_code,
};

#[derive(Subcommand)]
pub enum UserCommand {
    /// Add a user.
    Add {
        username: String,
        /// What clients show for them; the username if left out.
        #[arg(long)]
        display_name: Option<String>,
        /// `admin` or `consumer`.
        #[arg(long, default_value = "consumer")]
        role: Role,
        /// The same as `--role admin`.
        #[arg(long, hide = true)]
        admin: bool,
        /// Read their password from standard input instead of prompting.
        #[arg(long)]
        password_stdin: bool,
    },
    /// Set a user's password.
    Passwd {
        username: String,
        #[arg(long)]
        password_stdin: bool,
    },
    /// Reset a user's password to a one-time one, printed this once: they
    /// choose a new one when they sign in with it, and it stops working
    /// after 7 days. Ends their sessions signed in with a password; those
    /// signed in with a passkey, a sign-on provider or a portable identity
    /// stay. Not your own (change it with `user passwd`).
    ResetPassword { username: String },
    /// Make a user an admin or a consumer. The last enabled admin stays
    /// one.
    Role { username: String, role: Role },
    /// Whether a user may download subtitles from OpenSubtitles (PX-02):
    /// `on` or `off`; without one, says which. Searching stays theirs.
    SubtitleDownloads {
        username: String,
        #[arg(value_parser = ["on", "off"])]
        set: Option<String>,
    },
    /// A user's profile PIN (P2-08).
    Pin {
        #[command(subcommand)]
        command: PinCommand,
    },
    /// The devices a user has signed in from, and where their PIN is asked
    /// for.
    Devices { username: String },
    /// Trust one of a user's devices, so it opens their profile without a
    /// password (`--off`: stop, which also ends their sessions there).
    /// Admins only.
    Trust {
        username: String,
        /// The device's name, as `user devices` lists it, or the start of
        /// its endpoint id.
        #[arg(long)]
        device: String,
        #[arg(long)]
        off: bool,
        /// Also open their profile there at start (turns the PIN off there,
        /// and auto-login off for the device's other users).
        #[arg(long, conflicts_with = "off")]
        auto_login: bool,
    },
    /// A user's live sessions, on every device and the web.
    Sessions { username: String },
    /// End one of a user's sessions, by the id `user sessions` shows.
    RevokeSession { username: String, id: String },
    /// List users, with their roles.
    List,
    /// Disable a user and end their sessions.
    Disable { username: String },
    /// Let a disabled user log in again.
    Enable { username: String },
}

/// Invites (P6-01): codes that let someone make their own account, and
/// the server's link without one.
#[derive(Subcommand)]
pub enum InviteCommand {
    /// Make an invite. Its code and link are printed this once: the server
    /// keeps only the code's hash.
    Create {
        /// Who it is for; only admins see it.
        #[arg(long)]
        note: Option<String>,
        /// How many accounts it can make.
        #[arg(long, default_value_t = spoolframe_admin_api::invite::DEFAULT_USES)]
        uses: u32,
        /// How long it lasts: hours or days, as `12h` or `7d`; an hour to
        /// 30 days.
        #[arg(long, default_value = "7d", value_parser = expiry)]
        expires: u64,
        /// The role its accounts get: `consumer` or `admin`; a consumer's
        /// if left out. Only a `create-user` invite takes one; a
        /// `portable-account` one only `consumer`, a `federation` one none.
        #[arg(long)]
        role: Option<Role>,
        /// `create-user` (an account here), `portable-account` (P7-05:
        /// someone with their own portable identity watches as
        /// themselves; the role is a consumer's), or `federation` (P8-01,
        /// experimental: another server pairs with this one, while
        /// `experimental.federation` is on).
        #[arg(long, default_value = "create-user", value_parser = invite_kind)]
        kind: InviteKind,
    },
    /// The invites that can still make an account, with their ids.
    List,
    /// Stop an invite's unused code, by the id `invite list` shows. The
    /// accounts it made stay.
    Revoke { id: String },
    /// The server's link without a code: it finds and saves the server,
    /// and whoever has it signs in with an account they have.
    Link,
}

/// Portable users granted access here (P7-06): people whose accounts
/// this server does not host.
#[derive(Subcommand)]
pub enum GrantCommand {
    /// The portable users granted access, with their ids.
    List,
    /// Take a portable user's grant away, by the id `grant list` shows:
    /// their sessions end at once.
    Revoke { id: String },
}

/// Playlists (PX-40), as the user who logs in sees them: their own,
/// those shared with them, and the server-wide ones.
#[derive(Subcommand)]
pub enum PlaylistCommand {
    /// The playlists you see, with their ids and whether each has a cover
    /// of its own.
    List,
    /// Give a playlist a cover (PX-53): a JPEG, PNG, WebP or HEIC of at
    /// most 20 MB and 40 megapixels, kept as its centre square, with
    /// nothing of the file but its picture. Whoever may edit the
    /// playlist; not Liked or All.
    SetArt {
        /// The playlist's id, as `playlist list` shows it.
        id: String,
        /// The picture.
        file: std::path::PathBuf,
    },
    /// Take a playlist's cover away: its mosaic of what it holds again.
    RemoveArt { id: String },
}

/// External text subtitles' sync and offset (PX-03), as an admin: the
/// same for every user who plays the item.
#[derive(Debug, Subcommand)]
pub enum SubtitleCommand {
    /// An item's subtitle files and how each is timed.
    List {
        /// The item's id, as `plex:movie:123`.
        id: String,
    },
    /// Sync a subtitle file to the item's file, in the background; `list`
    /// says when it is done.
    Sync {
        id: String,
        /// The track, as `list` numbers it.
        track: u32,
    },
    /// Serve a subtitle with its original timing again; its offset stays.
    Original { id: String, track: u32 },
    /// Move a subtitle by an offset, in milliseconds (positive: later);
    /// 0 resets it.
    Offset {
        id: String,
        track: u32,
        #[arg(allow_hyphen_values = true)]
        offset_ms: i64,
    },
    /// Search OpenSubtitles for an item's subtitles (PX-02), best first.
    Search {
        id: String,
        /// `en`, `pt-BR`; the languages set if left out.
        #[arg(long)]
        language: Option<String>,
    },
    /// Download a result of `search`: a track of the item for everyone.
    Download {
        id: String,
        /// The result's id, as `search` lists it: `opensubtitles:5338574`,
        /// `subdl:3172974-3188291`.
        result: String,
    },
    /// Remove a downloaded subtitle, for everyone.
    Remove {
        id: String,
        /// The download's id, as `list` shows it.
        download: String,
    },
}

/// `12h` or `7d` as seconds.
fn expiry(text: &str) -> Result<u64, String> {
    let (number, unit) = text.split_at(text.len().saturating_sub(1));
    let secs = match unit {
        "h" => 3600,
        "d" => 24 * 3600,
        _ => return Err(format!("{text:?}: say hours or days, as 12h or 7d")),
    };
    number
        .parse::<u64>()
        .map(|n| n * secs)
        .map_err(|_| format!("{text:?}: say hours or days, as 12h or 7d"))
}

fn invite_kind(text: &str) -> Result<InviteKind, String> {
    match text {
        "create-user" => Ok(InviteKind::CreateUser),
        "portable-account" => Ok(InviteKind::PortableAccount),
        "federation" => Ok(InviteKind::Federation),
        _ => Err(format!(
            "{text:?}: create-user, portable-account or federation"
        )),
    }
}

#[derive(Subcommand)]
pub enum PinCommand {
    /// Set or change a user's PIN, as that user: prompts for their password,
    /// to sign in as them, and the new PIN.
    Set {
        username: String,
        /// Read the password and the PIN from standard input, a line each.
        #[arg(long)]
        stdin: bool,
    },
    /// Remove a user's PIN, and where it is asked for. Admins only.
    Clear { username: String },
    /// Ask for a user's PIN on one of their devices (`--off`: stop asking).
    /// Admins only.
    Require {
        username: String,
        /// The device's name, as `user devices` lists it, or the start of
        /// its endpoint id.
        #[arg(long)]
        device: String,
        #[arg(long)]
        off: bool,
    },
}

/// The URL to reach the admin API listening on `listen`. A wildcard
/// address is reached over loopback.
fn url(listen: SocketAddr) -> String {
    let ip = match listen.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    format!("ws://{}", SocketAddr::new(ip, listen.port()))
}

async fn connect(listen: SocketAddr, token: Option<&str>) -> Result<WsClient> {
    let mut builder = WsClientBuilder::default();
    if let Some(token) = token {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Bearer {token}").parse()?);
        builder = builder.set_headers(headers);
    }
    let url = url(listen);
    builder
        .build(&url)
        .await
        .with_context(|| format!("cannot reach the admin API at {url}; is the server running?"))
}

/// A failed call as the server explained it, not as a debug dump.
fn api_error(e: ClientError) -> anyhow::Error {
    match e {
        ClientError::Call(error) => anyhow::anyhow!("{}", error.message()),
        other => other.into(),
    }
}

fn prompt_line(prompt: &str) -> Result<String> {
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_owned())
}

/// A new password: from standard input, or typed twice.
fn new_password(from_stdin: bool) -> Result<String> {
    if from_stdin {
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        return Ok(line.trim_end_matches(['\r', '\n']).to_owned());
    }
    let first = rpassword::prompt_password("New password: ")?;
    let again = rpassword::prompt_password("Repeat it: ")?;
    if first != again {
        bail!("the passwords do not match");
    }
    Ok(first)
}

/// The admin's own credentials: from the environment, or prompted.
fn admin_credentials() -> Result<Credentials> {
    let username = match std::env::var("SPOOLFRAME_ADMIN_USER") {
        Ok(name) => name,
        Err(_) => prompt_line("Admin username: ")?,
    };
    let password = match std::env::var("SPOOLFRAME_ADMIN_PASSWORD") {
        Ok(password) => password,
        Err(_) => rpassword::prompt_password(format!("Password for {username}: "))?,
    };
    Ok(Credentials { username, password })
}

/// Register the first admin.
pub async fn setup(
    listen: SocketAddr,
    code: Option<String>,
    username: Option<String>,
    password_stdin: bool,
) -> Result<()> {
    let code = match code {
        Some(code) => code,
        None => prompt_line("Setup code (from the server's log): ")?,
    };
    let username = match username {
        Some(name) => name,
        None => prompt_line("Admin username: ")?,
    };
    let password = new_password(password_stdin)?;
    let done = connect(listen, None)
        .await?
        .setup(SetupRequest {
            code,
            username,
            password,
        })
        .await
        .map_err(api_error)?;
    // The CLI does not keep sessions around.
    connect(listen, Some(&done.token))
        .await?
        .logout()
        .await
        .map_err(api_error)?;
    println!("created admin {}", done.username);
    Ok(())
}

pub async fn user(listen: SocketAddr, command: UserCommand) -> Result<()> {
    // Set as the user, not an admin: an admin cannot set anyone's PIN.
    if let UserCommand::Pin {
        command: PinCommand::Set { username, stdin },
    } = command
    {
        return pin_set(listen, &username, stdin).await;
    }
    // Read a new password before logging in, so both prompts do not race
    // for standard input.
    let new = match &command {
        UserCommand::Add { password_stdin, .. } | UserCommand::Passwd { password_stdin, .. } => {
            Some(new_password(*password_stdin)?)
        }
        _ => None,
    };
    let login = connect(listen, None)
        .await?
        .login(admin_credentials()?)
        .await
        .map_err(api_error)?;
    let api = connect(listen, Some(&login.token)).await?;
    let result = run(&api, command, new).await;
    // Log out even if the command failed.
    if let Err(e) = api.logout().await {
        tracing::debug!(error = %e, "logout");
    }
    result
}

/// Set `username`'s PIN, signed in as them with their password: a PIN is
/// set from a session a real sign-in made, never by an admin.
async fn pin_set(listen: SocketAddr, username: &str, from_stdin: bool) -> Result<()> {
    let (password, pin) = if from_stdin {
        let mut lines = std::io::stdin().lock().lines();
        let mut next = || -> Result<String> {
            Ok(lines
                .next()
                .context("expected the password, then the PIN, a line each")??
                .trim_end_matches('\r')
                .to_owned())
        };
        (next()?, next()?)
    } else {
        let password = rpassword::prompt_password(format!("Password for {username}: "))?;
        let pin = rpassword::prompt_password("New PIN (4 to 8 digits): ")?;
        if rpassword::prompt_password("Repeat it: ")? != pin {
            bail!("the PINs do not match");
        }
        (password, pin)
    };
    let login = connect(listen, None)
        .await?
        .login(Credentials {
            username: username.to_owned(),
            password,
        })
        .await
        .map_err(api_error)?;
    let api = connect(listen, Some(&login.token)).await?;
    let result = api.pin_set(pin).await.map_err(api_error);
    if let Err(e) = api.logout().await {
        tracing::debug!(error = %e, "logout");
    }
    result?;
    println!("PIN set for {username}");
    Ok(())
}

/// One line of `user devices`: name, endpoint id's start, last use (as
/// `when` says it), then what applies.
fn device_line(d: &spoolframe_admin_api::DeviceEntry, when: &str) -> String {
    let mut line = format!(
        "{}  {}  last used {when}",
        d.name.as_deref().unwrap_or("(unnamed)"),
        &d.endpoint_id[..d.endpoint_id.len().min(10)],
    );
    if d.trusted {
        line.push_str("  trusted");
    }
    if d.auto_login {
        line.push_str("  opens at start");
    }
    if d.pin_required {
        line.push_str("  PIN required");
    }
    line
}

/// The endpoint of `username`'s device named `device` (or whose endpoint
/// id starts with it).
async fn find_device(api: &WsClient, username: &str, device: &str) -> Result<String> {
    let devices = api
        .user_devices(username.to_owned())
        .await
        .map_err(api_error)?;
    let matching: Vec<_> = devices
        .iter()
        .filter(|d| d.name.as_deref() == Some(device) || d.endpoint_id.starts_with(device))
        .collect();
    match matching.as_slice() {
        [one] => Ok(one.endpoint_id.clone()),
        [] => bail!("{username} has no device {device:?}; see `user devices {username}`"),
        _ => bail!("{device:?} names more than one of {username}'s devices; give its endpoint id"),
    }
}

/// Sync `user`'s watch state with their Plex account now, or every linked
/// user's, or with `dry_run` print what would change, logged in as an
/// admin.
pub async fn plex_sync(listen: SocketAddr, user: Option<String>, dry_run: bool) -> Result<()> {
    let login = connect(listen, None)
        .await?
        .login(admin_credentials()?)
        .await
        .map_err(api_error)?;
    let api = connect(listen, Some(&login.token)).await?;
    let result = api.plex_watch_sync(user, dry_run).await.map_err(api_error);
    if let Err(e) = api.logout().await {
        tracing::debug!(error = %e, "logout");
    }
    let reports = result?;
    if reports.is_empty() {
        println!("nobody has linked a Plex account");
    }
    let (pull, push) = if dry_run {
        ("would pull", "would push")
    } else {
        ("pulled", "pushed")
    };
    for report in &reports {
        for line in &report.pulled {
            println!("{pull}  {line}");
        }
        for line in &report.pushed {
            println!("{push}  {line}");
        }
        for line in &report.failed {
            println!("failed  {line}");
        }
        for note in &report.notes {
            println!("note  {note}");
        }
        println!(
            "{}: {} pulled, {} pushed, {} skipped, {} failed",
            report.username,
            report.pulled.len(),
            report.pushed.len(),
            report.skipped,
            report.failed.len()
        );
    }
    Ok(())
}

/// Print the server's status, logged in as an admin.
pub async fn status(listen: SocketAddr) -> Result<()> {
    let login = connect(listen, None)
        .await?
        .login(admin_credentials()?)
        .await
        .map_err(api_error)?;
    let api = connect(listen, Some(&login.token)).await?;
    let result = async {
        let status = api.server_status().await.map_err(api_error)?;
        // A server that is not set up has no Plex to ask about.
        let plex = match api.plex_status().await {
            Ok(plex) => Some(plex),
            Err(ClientError::Call(e)) if e.code() == error_code::NOT_CONFIGURED => None,
            Err(e) => return Err(api_error(e)),
        };
        Ok::<_, anyhow::Error>((status, plex))
    }
    .await;
    if let Err(e) = api.logout().await {
        tracing::debug!(error = %e, "logout");
    }
    let (status, plex) = result?;
    let list = |v: &[String]| {
        if v.is_empty() {
            "none".to_owned()
        } else {
            v.join(", ")
        }
    };
    println!("name: {}", status.name);
    match &status.revision {
        Some(revision) => println!("version: {} ({revision})", status.version),
        None => println!("version: {}", status.version),
    }
    println!(
        "endpoint: {}",
        status.endpoint_id.as_deref().unwrap_or("starting")
    );
    println!(
        "fingerprint: {}",
        status.fingerprint.as_deref().unwrap_or("starting")
    );
    println!("server id: {}", status.server_id);
    println!(
        "passkey domain: {}.{}",
        status.server_id,
        spoolframe_admin_api::APP_RP_DOMAIN
    );
    println!(
        "video encoders: {}",
        list(&status.transcoding.video_encoders)
    );
    println!(
        "audio encoders: {}",
        list(&status.transcoding.audio_encoders)
    );
    println!(
        "hardware decoders: {}",
        list(&status.transcoding.hardware_decoders)
    );
    println!("GPU filters: {}", gpu_filters(&status.transcoding));
    println!("GPU tone maps: {}", gpu_tone_maps(&status.transcoding));
    if let Some(budget) = &status.transcoding.budget {
        println!("encode budget used: {}", budget.used_of_max());
        println!("remuxes: {} of {}", budget.remuxes, budget.max_remux);
        println!("GPU jobs: {} of {}", budget.gpu_jobs, budget.max_gpu_jobs);
    }
    println!("mDNS: {}", if status.mdns { "on" } else { "off" });
    println!("addresses: {}", list(&status.addresses));
    println!("told to devices: {}", list(&status.told_addresses));
    if status.wan {
        let public: Vec<String> = status
            .public_addresses
            .iter()
            .map(|a| format!("{} ({})", a.address, a.source.label()))
            .collect();
        println!("public addresses: {}", list(&public));
        for note in &status.public_notes {
            println!("  {note}");
        }
    }
    let Some(plex) = plex else {
        println!("Plex: not set up");
        return Ok(());
    };
    match plex.error {
        None => println!(
            "Plex: {}",
            plex.server_name.as_deref().unwrap_or("reachable")
        ),
        Some(e) => println!("Plex: unreachable ({e})"),
    }
    for library in plex.libraries {
        println!(
            "  {} ({:?}): {} items",
            library.title, library.kind, library.items
        );
    }
    Ok(())
}

async fn run(api: &WsClient, command: UserCommand, new_password: Option<String>) -> Result<()> {
    let new_password = || new_password.clone().unwrap_or_default();
    match command {
        UserCommand::Add {
            username,
            display_name,
            role,
            admin,
            ..
        } => {
            let user = api
                .user_add(NewUser {
                    username,
                    display_name,
                    password: new_password(),
                    role: if admin { Role::Admin } else { role },
                })
                .await
                .map_err(api_error)?;
            println!("added {} ({})", user.username, user.role);
        }
        UserCommand::SubtitleDownloads { username, set } => {
            let allowed = match set.as_deref() {
                Some(set) => {
                    let allowed = set == "on";
                    api.user_set_subtitle_downloads(username.clone(), allowed)
                        .await
                        .map_err(api_error)?;
                    allowed
                }
                None => api
                    .user_subtitle_downloads(username.clone())
                    .await
                    .map_err(api_error)?,
            };
            let state = if allowed { "on" } else { "off" };
            println!("subtitle downloads are {state} for {username}");
        }
        UserCommand::Role { username, role } => {
            let user = api.user_set_role(username, role).await.map_err(api_error)?;
            println!("{} is now {}", user.username, user.role);
        }
        UserCommand::Passwd { username, .. } => {
            api.user_set_password(Credentials {
                username: username.clone(),
                password: new_password(),
            })
            .await
            .map_err(api_error)?;
            println!("password set for {username}");
        }
        UserCommand::ResetPassword { username } => {
            let reset = api
                .user_reset_password(username.clone())
                .await
                .map_err(api_error)?;
            println!(
                "one-time password for {username}: {}\n\
                 it works until {}; they choose a new one when they sign in with it\n\
                 sessions ended: {}, devices no longer trusted: {}",
                reset.password,
                when(reset.expires_at),
                reset.revoked_sessions,
                reset.untrusted_devices,
            );
        }
        UserCommand::List => {
            for user in api.user_list().await.map_err(api_error)? {
                let mut flags = vec![user.role.as_str().to_owned()];
                if user.left_at.is_some() {
                    flags.push("left (enable restores)".into());
                } else if user.disabled {
                    flags.push("disabled".into());
                }
                if let Some(secs) = user.rate_limited_secs {
                    flags.push(format!("rate limited for {} min", secs.div_ceil(60)));
                }
                if !user.has_password {
                    flags.push("no password".into());
                }
                if user.password_must_change {
                    flags.push("one-time password".into());
                }
                let mut line = user.username;
                if let Some(name) = user.display_name {
                    line.push_str(&format!(" \"{name}\""));
                }
                line.push_str(&format!(" ({})", flags.join(", ")));
                match user.last_seen_at {
                    Some(at) => line.push_str(&format!("  last seen {}", when(at))),
                    None => line.push_str("  never signed in"),
                }
                println!("{line}");
            }
        }
        UserCommand::Pin { command } => match command {
            PinCommand::Set { .. } => unreachable!("set signs in as the user, before this"),
            PinCommand::Clear { username } => {
                api.user_clear_pin(username.clone())
                    .await
                    .map_err(api_error)?;
                println!("PIN cleared for {username}; it is asked for nowhere");
            }
            PinCommand::Require {
                username,
                device,
                off,
            } => {
                let endpoint = find_device(api, &username, &device).await?;
                api.user_set_pin_required(username.clone(), endpoint, !off)
                    .await
                    .map_err(api_error)?;
                let state = if off {
                    "no longer asked for"
                } else {
                    "asked for"
                };
                println!("{username}'s PIN is {state} on {device}");
            }
        },
        UserCommand::Devices { username } => {
            for d in api.user_devices(username).await.map_err(api_error)? {
                println!("{}", device_line(&d, &when(d.last_seen_at)));
            }
        }
        UserCommand::Trust {
            username,
            device,
            off,
            auto_login,
        } => {
            let endpoint = find_device(api, &username, &device).await?;
            api.user_set_device_trust(username.clone(), endpoint, !off, auto_login)
                .await
                .map_err(api_error)?;
            let state = match (off, auto_login) {
                (true, _) => "no longer trusted; their sessions there ended",
                (false, true) => "trusted, and opens their profile at start",
                (false, false) => "trusted",
            };
            println!("{device} is {state} for {username}");
        }
        UserCommand::Sessions { username } => {
            for s in api.user_sessions(username).await.map_err(api_error)? {
                let what = match (&s.endpoint_id, &s.device_name) {
                    (Some(_), Some(name)) => name.clone(),
                    (Some(id), None) => format!("device {}", &id[..id.len().min(10)]),
                    (None, _) => match s.platform {
                        Some(platform) => platform.label().to_lowercase(),
                        None => "web or CLI".to_owned(),
                    },
                };
                println!(
                    "{}  {what}  since {}  last used {}",
                    s.id,
                    when(s.created_at),
                    when(s.last_used_at)
                );
            }
        }
        UserCommand::RevokeSession { username, id } => {
            api.user_revoke_session(username.clone(), id)
                .await
                .map_err(api_error)?;
            println!("ended that session of {username}");
        }
        UserCommand::Disable { username } => {
            let revoked = api
                .user_disable(username.clone())
                .await
                .map_err(api_error)?;
            println!("disabled {username}; ended {revoked} session(s)");
        }
        UserCommand::Enable { username } => {
            api.user_enable(username.clone()).await.map_err(api_error)?;
            println!("enabled {username}");
        }
    }
    Ok(())
}

/// Make, list or revoke invites, or print the server's link, logged in
/// as an admin. The CLI sends no page origin, so the server records its
/// invites as made by "server CLI".
pub async fn invite(listen: SocketAddr, command: InviteCommand) -> Result<()> {
    let login = connect(listen, None)
        .await?
        .login(admin_credentials()?)
        .await
        .map_err(api_error)?;
    let api = connect(listen, Some(&login.token)).await?;
    let result = run_invite(&api, command).await;
    if let Err(e) = api.logout().await {
        tracing::debug!(error = %e, "logout");
    }
    result
}

async fn run_invite(api: &WsClient, command: InviteCommand) -> Result<()> {
    match command {
        InviteCommand::Create {
            note,
            uses,
            expires,
            role,
            kind,
        } => {
            let made = api
                .invite_create(NewInvite {
                    kind,
                    note,
                    uses: Some(uses),
                    expires: Some(expires),
                    role,
                })
                .await
                .map_err(api_error)?;
            println!("{}", invite_line(&made.invite));
            println!("code  {}", made.code);
            // The https form (PX-45): text is sent, not scanned.
            println!("link  {}", made.share_link);
            eprintln!(
                "The code is shown only this once: the server keeps only its hash. \
                 Lost it? Revoke the invite and make another."
            );
        }
        InviteCommand::List => {
            let invites = api.invite_list().await.map_err(api_error)?;
            if invites.is_empty() {
                println!("no invites can make an account now");
            }
            for invite in &invites {
                println!("{}", invite_line(invite));
            }
        }
        InviteCommand::Revoke { id } => {
            api.invite_revoke(id.clone()).await.map_err(api_error)?;
            println!("revoked invite {id}; the accounts it made stay");
        }
        InviteCommand::Link => {
            println!("{}", api.invite_link().await.map_err(api_error)?.share_link);
        }
    }
    Ok(())
}

/// List playlists, or set or remove one's cover (PX-53), as the user who
/// logs in.
pub async fn playlist(listen: SocketAddr, command: PlaylistCommand) -> Result<()> {
    let login = connect(listen, None)
        .await?
        .login(admin_credentials()?)
        .await
        .map_err(api_error)?;
    let api = connect(listen, Some(&login.token)).await?;
    let result = run_playlist(&api, listen, &login.token, command).await;
    if let Err(e) = api.logout().await {
        tracing::debug!(error = %e, "logout");
    }
    result
}

async fn run_playlist(
    api: &WsClient,
    listen: SocketAddr,
    token: &str,
    command: PlaylistCommand,
) -> Result<()> {
    match command {
        PlaylistCommand::List => {
            let playlists = api.playlists().await.map_err(api_error)?;
            if playlists.is_empty() {
                println!("you see no playlists");
            }
            for p in &playlists {
                println!(
                    "{}  {}{}",
                    p.id,
                    p.name,
                    if p.custom_art { "  (own cover)" } else { "" }
                );
            }
        }
        PlaylistCommand::SetArt { id, file } => {
            let picture =
                std::fs::read(&file).with_context(|| format!("cannot read {}", file.display()))?;
            // The picture goes as an HTTP body, as the web UI sends it: a
            // JSON-RPC message is no place for 20 MB.
            let url = format!(
                "{}/playlists/art?id={}",
                url(listen).replacen("ws://", "http://", 1),
                url::form_urlencoded::byte_serialize(id.as_bytes()).collect::<String>()
            );
            let response = reqwest::Client::new()
                .post(&url)
                .bearer_auth(token)
                .body(picture)
                .send()
                .await
                .with_context(|| format!("cannot reach the admin API at {url}"))?;
            if !response.status().is_success() {
                let said: serde_json::Value = response.json().await.unwrap_or_default();
                bail!(
                    "{}",
                    said["message"]
                        .as_str()
                        .unwrap_or("the server refused the cover")
                );
            }
            let after: spoolframe_admin_api::PlaylistInfo = response.json().await?;
            println!("{} has its cover", after.name);
        }
        PlaylistCommand::RemoveArt { id } => {
            let after = api.playlist_remove_art(id).await.map_err(api_error)?;
            println!("{} shows its mosaic again", after.name);
        }
    }
    Ok(())
}

/// See or change subtitles' timing (PX-03), logged in as an admin.
pub async fn subtitle(listen: SocketAddr, command: SubtitleCommand) -> Result<()> {
    let login = connect(listen, None)
        .await?
        .login(admin_credentials()?)
        .await
        .map_err(api_error)?;
    let api = connect(listen, Some(&login.token)).await?;
    let result = run_subtitle(&api, command).await;
    if let Err(e) = api.logout().await {
        tracing::debug!(error = %e, "logout");
    }
    result
}

async fn run_subtitle(api: &WsClient, command: SubtitleCommand) -> Result<()> {
    let timings = match command {
        SubtitleCommand::Search { id, language } => {
            let found = api.subtitle_search(id, language).await.map_err(api_error)?;
            if found.results.is_empty() {
                println!("nothing found in {}", found.languages.join(", "));
            }
            for result in &found.results {
                println!("{}", result_line(result));
            }
            for status in &found.sources {
                if let Some(line) = source_line(status) {
                    eprintln!("{line}");
                }
            }
            return Ok(());
        }
        SubtitleCommand::Download { id, result } => {
            let got = api.subtitle_download(id, result).await.map_err(api_error)?;
            let mut line = format!("downloaded as subtitle {}", got.track);
            if let Some(left) = got.account_remaining {
                line.push_str(&format!("; the account has {left} download(s) left today"));
            }
            if let Some(left) = got.user_remaining {
                line.push_str(&format!("; you have {left} left today"));
            }
            println!("{line}");
            return Ok(());
        }
        SubtitleCommand::Remove { id, download } => {
            api.subtitle_remove(id, download.clone())
                .await
                .map_err(api_error)?;
            println!("removed {download}");
            return Ok(());
        }
        SubtitleCommand::List { id } => {
            let timings = api.subtitle_timings(id).await.map_err(api_error)?;
            if timings.is_empty() {
                println!("the item has no subtitle file to sync");
            }
            timings
        }
        SubtitleCommand::Sync { id, track } => {
            vec![api.subtitle_sync(id, track).await.map_err(api_error)?]
        }
        SubtitleCommand::Original { id, track } => vec![
            api.subtitle_use_original(id, track)
                .await
                .map_err(api_error)?,
        ],
        SubtitleCommand::Offset {
            id,
            track,
            offset_ms,
        } => vec![
            api.subtitle_set_offset(id, track, offset_ms)
                .await
                .map_err(api_error)?,
        ],
    };
    for timing in &timings {
        println!("{}", timing_line(timing));
    }
    Ok(())
}

/// `0  Film.en.srt  synced (-3200 ms, x1.0427, 1 split, score 87)  offset +1500 ms by Alice`.
/// One search result as `subtitle search` lists it (PX-02).
fn result_line(r: &spoolframe_types::SubtitleResult) -> String {
    use spoolframe_types::SubtitleOrigin;
    let mut marks = Vec::new();
    if r.hash_match {
        marks.push("hash match");
    }
    if r.hearing_impaired {
        marks.push("SDH");
    }
    if r.forced {
        marks.push("forced");
    }
    match r.origin {
        SubtitleOrigin::MachineTranslated => marks.push("machine-translated"),
        SubtitleOrigin::Ai => marks.push("AI-made"),
        SubtitleOrigin::Human => {}
    }
    if r.downloaded {
        marks.push("downloaded");
    }
    if r.rejected {
        marks.push("removed before");
    }
    let rating = r.rating.map_or_else(
        || "unrated".to_owned(),
        |r| format!("{}.{}/10", r / 10, r % 10),
    );
    let marks = if marks.is_empty() {
        String::new()
    } else {
        format!("  [{}]", marks.join(", "))
    };
    format!(
        "{}  {}  {}  {rating}, {} downloads{marks}",
        r.id, r.language, r.release, r.downloads
    )
}

/// What a site's part of a search says when it found nothing it could:
/// `subdl: out of downloads`. `None` for one that answered.
fn source_line(status: &spoolframe_types::SubtitleSourceStatus) -> Option<String> {
    use spoolframe_types::SubtitleSourceState as S;
    let why = match &status.state {
        S::Found { .. } => return None,
        S::QuotaExhausted { .. } => "its account has no downloads or searches left today".into(),
        S::RateLimited { retry_after_ms } => {
            format!("asks to wait {} s", retry_after_ms.div_ceil(1000))
        }
        S::Failed { message } => message.clone(),
        S::Unknown => "did not answer as this build knows".into(),
    };
    Some(format!("{}: {why}", status.source))
}

fn timing_line(t: &spoolframe_admin_api::SubtitleTimingInfo) -> String {
    use spoolframe_admin_api::SubtitleSyncState as S;
    let sync = match &t.sync {
        S::Original => "original timing".to_owned(),
        S::Syncing => "syncing".to_owned(),
        S::Synced {
            offset_ms,
            factor,
            splits,
            score,
        } => format!(
            "synced ({offset_ms:+} ms, x{factor:.4}, {splits} split{}, score {score})",
            if *splits == 1 { "" } else { "s" }
        ),
        S::Uncertain { score } => format!("sync uncertain (score {score}); original timing"),
        S::Failed { reason } => format!("sync failed: {reason}"),
    };
    let offset = match (t.offset_ms, &t.offset_by) {
        (0, _) => String::new(),
        (ms, Some(by)) => format!("  offset {ms:+} ms by {by}"),
        (ms, None) => format!("  offset {ms:+} ms"),
    };
    let download = match &t.download {
        Some(d) => format!("  downloaded by {} (id {})", d.by, d.id),
        None => String::new(),
    };
    format!("{}  {}  {sync}{offset}{download}", t.track, t.label)
}

/// List or revoke portable users' grants (P7-06), logged in as an admin.
pub async fn grant(listen: SocketAddr, command: GrantCommand) -> Result<()> {
    let login = connect(listen, None)
        .await?
        .login(admin_credentials()?)
        .await
        .map_err(api_error)?;
    let api = connect(listen, Some(&login.token)).await?;
    let result = run_grant(&api, command).await;
    if let Err(e) = api.logout().await {
        tracing::debug!(error = %e, "logout");
    }
    result
}

async fn run_grant(api: &WsClient, command: GrantCommand) -> Result<()> {
    match command {
        GrantCommand::List => {
            let grants = api.grant_list().await.map_err(api_error)?;
            if grants.is_empty() {
                println!("no portable users are granted access");
            }
            for grant in &grants {
                println!(
                    "{}  {}  {}  granted {}  {} live sessions",
                    grant.id,
                    grant.fingerprint,
                    grant.display_name.as_deref().unwrap_or("(no name)"),
                    when(grant.granted_at),
                    grant.live_sessions,
                );
            }
        }
        GrantCommand::Revoke { id } => {
            api.grant_revoke(id.clone()).await.map_err(api_error)?;
            println!("revoked the grant to {id}; their sessions have ended");
        }
    }
    Ok(())
}

/// One invite as `invite list` prints it: its id first, then who it is
/// for, the role, the uses left, when it expires and who made it.
fn invite_line(invite: &spoolframe_admin_api::InviteEntry) -> String {
    let what = match invite.kind {
        InviteKind::CreateUser => invite.role.to_string(),
        InviteKind::PortableAccount => "portable guest".to_owned(),
        InviteKind::Federation => "federation".to_owned(),
    };
    format!(
        "{}  {}  {}  {} of {} left  expires {}  by {}",
        invite.id,
        invite.note.as_deref().unwrap_or("(no note)"),
        what,
        invite.uses_left,
        invite.uses,
        when(invite.expires_at),
        invite.created_by,
    )
}

/// Unix seconds as a UTC date and time, to the minute.
fn when(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rest = secs.rem_euclid(86_400);
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        rest / 3600,
        rest / 60 % 60
    )
}

/// `config set` and `config unset`: a runtime setting, through the
/// running server, which checks it as a config file is checked and
/// applies it. `value` is JSON (`3`, `true`, `[{"from": "/a", "to": "/b"}]`)
/// or else a string; `None` unsets.
pub async fn config_change(listen: SocketAddr, key: &str, value: Option<&str>) -> Result<()> {
    let login = connect(listen, None)
        .await?
        .login(admin_credentials()?)
        .await
        .map_err(api_error)?;
    let api = connect(listen, Some(&login.token)).await?;
    let result = match value {
        Some(text) => {
            // A token is a string whatever it looks like.
            let value = match serde_json::from_str(text) {
                Ok(value) if key != "plex.token" => value,
                _ => serde_json::Value::String(text.to_owned()),
            };
            api.setting_set(key.to_owned(), value).await
        }
        None => api.setting_unset(key.to_owned()).await,
    };
    if let Err(e) = api.logout().await {
        tracing::debug!(error = %e, "logout");
    }
    let entry = result.map_err(api_error)?;
    let value = match (&entry.value, entry.secret) {
        (_, true) if entry.set => "(set)".to_owned(),
        (_, true) => "(not set)".to_owned(),
        (Some(value), false) => value.to_string(),
        (None, false) => "(not set)".to_owned(),
    };
    println!("{} = {value}", entry.key);
    if entry.restart_pending {
        println!("applies after a restart");
    }
    Ok(())
}

/// `config reload` (PX-33): the running server reads its config again,
/// and what changed is printed as its log says it.
pub async fn config_reload(listen: SocketAddr) -> Result<()> {
    let login = connect(listen, None)
        .await?
        .login(admin_credentials()?)
        .await
        .map_err(api_error)?;
    let api = connect(listen, Some(&login.token)).await?;
    let result = api.config_reload().await;
    if let Err(e) = api.logout().await {
        tracing::debug!(error = %e, "logout");
    }
    let reloaded = result.map_err(api_error)?;
    if reloaded.applied.is_empty() && reloaded.after_restart.is_empty() {
        println!("config reloaded: nothing changed");
        return Ok(());
    }
    println!("config reloaded");
    if !reloaded.applied.is_empty() {
        println!("applied: {}", reloaded.applied.join(", "));
    }
    if !reloaded.after_restart.is_empty() {
        println!("after a restart: {}", reloaded.after_restart.join(", "));
    }
    Ok(())
}

/// `transcode.gpu_filters`, and where each filter runs with it on: on
/// the CPU where this GPU cannot.
fn gpu_filters(t: &spoolframe_admin_api::TranscodingStatus) -> String {
    if !t.gpu_filters {
        return "off".to_owned();
    }
    let place = |gpu: bool| {
        if gpu {
            "on the GPU"
        } else {
            "on the CPU, this GPU cannot"
        }
    };
    format!(
        "on; tone map {}; picture subtitles {}; text subtitles on the CPU",
        place(t.gpu_tonemap),
        place(t.gpu_overlay)
    )
}

/// The tone maps each GPU runs (PGPU-04): `vaapi: VA-API, libplacebo
/// (own); nvidia: libplacebo (shared)`, or `none`.
fn gpu_tone_maps(t: &spoolframe_admin_api::TranscodingStatus) -> String {
    let mut per_gpu: Vec<(spoolframe_admin_api::GpuApi, Vec<&str>)> = Vec::new();
    for found in &t.gpu_tone_maps {
        match per_gpu.iter_mut().find(|(gpu, _)| *gpu == found.gpu) {
            Some((_, labels)) => labels.push(found.mapper.label()),
            None => per_gpu.push((found.gpu, vec![found.mapper.label()])),
        }
    }
    if per_gpu.is_empty() {
        return "none".to_owned();
    }
    per_gpu
        .into_iter()
        .map(|(gpu, labels)| {
            let name = match gpu {
                spoolframe_admin_api::GpuApi::Vaapi => "vaapi",
                spoolframe_admin_api::GpuApi::Nvidia => "nvidia",
                spoolframe_admin_api::GpuApi::Qsv => "qsv",
                spoolframe_admin_api::GpuApi::Vulkan => "vulkan",
            };
            format!("{name}: {}", labels.join(", "))
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PX-03: a subtitle's line says its sync and who offset it.
    #[test]
    fn a_subtitle_line_says_its_sync_and_offset() {
        use spoolframe_admin_api::{SubtitleSyncState, SubtitleTimingInfo};
        let synced = SubtitleTimingInfo {
            track: 2,
            label: "Film.en.srt".into(),
            sync: SubtitleSyncState::Synced {
                offset_ms: -3_200,
                factor: 25.0 / 23.976,
                splits: 1,
                score: 87,
            },
            offset_ms: 1_500,
            offset_by: Some("Alice".into()),
            offset_at: Some(1),
            download: None,
        };
        assert_eq!(
            timing_line(&synced),
            "2  Film.en.srt  synced (-3200 ms, x1.0427, 1 split, score 87)  offset +1500 ms by Alice"
        );
        let uncertain = SubtitleTimingInfo {
            sync: SubtitleSyncState::Uncertain { score: 9 },
            offset_ms: 0,
            ..synced
        };
        assert_eq!(
            timing_line(&uncertain),
            "2  Film.en.srt  sync uncertain (score 9); original timing"
        );
    }

    /// PX-02: a result says what it is made for and what marks it.
    #[test]
    fn a_search_result_line_says_its_marks() {
        let result = spoolframe_types::SubtitleResult {
            id: "opensubtitles:101".into(),
            source: "opensubtitles".into(),
            release: "Film.2019.1080p".into(),
            language: "en".into(),
            format: Some("srt".into()),
            fps_milli: None,
            downloads: 120,
            rating: Some(87),
            hearing_impaired: false,
            forced: true,
            origin: spoolframe_types::SubtitleOrigin::MachineTranslated,
            hash_match: true,
            rejected: true,
            downloaded: false,
        };
        assert_eq!(
            result_line(&result),
            "opensubtitles:101  en  Film.2019.1080p  8.7/10, 120 downloads  [hash match, forced, machine-translated, removed before]"
        );
    }

    /// A site that failed says so; one that answered says nothing.
    #[test]
    fn a_failed_site_s_part_of_a_search_is_said() {
        use spoolframe_types::{SubtitleSourceState as S, SubtitleSourceStatus};
        let status = |state| SubtitleSourceStatus {
            source: "subdl".into(),
            state,
        };
        assert_eq!(source_line(&status(S::Found { found: 3 })), None);
        assert_eq!(
            source_line(&status(S::RateLimited {
                retry_after_ms: 1_500
            }))
            .as_deref(),
            Some("subdl: asks to wait 2 s")
        );
        assert_eq!(
            source_line(&status(S::Failed {
                message: "SubDL cannot be reached: no connection".into()
            }))
            .as_deref(),
            Some("subdl: SubDL cannot be reached: no connection")
        );
    }

    #[test]
    fn the_status_says_which_tone_maps_each_gpu_runs() {
        use spoolframe_admin_api::{GpuApi, GpuToneMap, ToneMapper};
        let mut t = spoolframe_admin_api::TranscodingStatus {
            video_encoders: Vec::new(),
            audio_encoders: Vec::new(),
            hardware_decoders: Vec::new(),
            gpu_filters: false,
            gpu_tonemap: false,
            gpu_overlay: false,
            gpu_tone_maps: Vec::new(),
            budget: None,
            gpus: Vec::new(),
            other_gpus: Vec::new(),
            gpu_notes: Vec::new(),
            routes: Vec::new(),
            picks: Vec::new(),
        };
        assert_eq!(gpu_tone_maps(&t), "none");
        t.gpu_tone_maps = vec![
            GpuToneMap {
                gpu: GpuApi::Vaapi,
                mapper: ToneMapper::Vaapi,
            },
            GpuToneMap {
                gpu: GpuApi::Nvidia,
                mapper: ToneMapper::LibplaceboShared,
            },
            GpuToneMap {
                gpu: GpuApi::Vaapi,
                mapper: ToneMapper::LibplaceboOwn,
            },
        ];
        assert_eq!(
            gpu_tone_maps(&t),
            "vaapi: VA-API, libplacebo (own); nvidia: libplacebo (shared)"
        );
    }

    #[test]
    fn the_status_says_where_gpu_filters_run() {
        let mut t = spoolframe_admin_api::TranscodingStatus {
            video_encoders: Vec::new(),
            audio_encoders: Vec::new(),
            hardware_decoders: Vec::new(),
            gpu_filters: false,
            gpu_tonemap: true,
            gpu_overlay: false,
            gpu_tone_maps: Vec::new(),
            budget: None,
            gpus: Vec::new(),
            other_gpus: Vec::new(),
            gpu_notes: Vec::new(),
            routes: Vec::new(),
            picks: Vec::new(),
        };
        assert_eq!(gpu_filters(&t), "off");
        t.gpu_filters = true;
        assert_eq!(
            gpu_filters(&t),
            "on; tone map on the GPU; picture subtitles on the CPU, this GPU cannot; \
             text subtitles on the CPU"
        );
    }

    #[test]
    fn times_read_as_utc_dates() {
        assert_eq!(when(0), "1970-01-01 00:00 UTC");
        // A leap day, and a date this century.
        assert_eq!(
            when(951_782_400 + 13 * 3600 + 7 * 60),
            "2000-02-29 13:07 UTC"
        );
        assert_eq!(when(1_790_000_000), "2026-09-21 14:13 UTC");
    }

    #[test]
    fn the_devices_list_says_what_applies_on_each() {
        let tv = spoolframe_admin_api::DeviceEntry {
            endpoint_id: "ab".repeat(32),
            name: Some("TV".into()),
            platform: None,
            last_seen_at: 0,
            pin_required: false,
            trusted: true,
            auto_login: true,
        };
        assert_eq!(
            device_line(&tv, "now"),
            "TV  ababababab  last used now  trusted  opens at start"
        );
        let plain = spoolframe_admin_api::DeviceEntry {
            name: None,
            pin_required: true,
            trusted: false,
            auto_login: false,
            ..tv
        };
        assert_eq!(
            device_line(&plain, "now"),
            "(unnamed)  ababababab  last used now  PIN required"
        );
    }

    #[test]
    fn a_wildcard_listen_address_is_reached_over_loopback() {
        assert_eq!(url("0.0.0.0:7421".parse().unwrap()), "ws://127.0.0.1:7421");
        assert_eq!(url("[::]:7421".parse().unwrap()), "ws://[::1]:7421");
        assert_eq!(url("127.0.0.1:9".parse().unwrap()), "ws://127.0.0.1:9");
    }
}
