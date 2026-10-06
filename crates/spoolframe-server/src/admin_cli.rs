//! `spoolframe-server setup` and `spoolframe-server user …`: clients of the
//! running server's admin API. They never open the database.
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
    AdminApiClient, Credentials, MeApiClient, NewUser, Role, SetupRequest, error_code,
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
    /// Make a user an admin or a consumer. The last enabled admin stays
    /// one.
    Role { username: String, role: Role },
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
    println!("mDNS: {}", if status.mdns { "on" } else { "off" });
    println!("addresses: {}", list(&status.addresses));
    println!("told to devices: {}", list(&status.told_addresses));
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
        UserCommand::List => {
            for user in api.user_list().await.map_err(api_error)? {
                let mut flags = vec![user.role.as_str().to_owned()];
                if user.disabled {
                    flags.push("disabled".into());
                }
                if let Some(secs) = user.rate_limited_secs {
                    flags.push(format!("rate limited for {} min", secs.div_ceil(60)));
                }
                if !user.has_password {
                    flags.push("no password".into());
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

#[cfg(test)]
mod tests {
    use super::*;

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
