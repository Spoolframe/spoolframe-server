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
use spoolframe_admin_api::{AdminApiClient, Credentials, NewUser, SetupRequest};

#[derive(Subcommand)]
pub enum UserCommand {
    /// Add a user.
    Add {
        username: String,
        /// What clients show for them; the username if left out.
        #[arg(long)]
        display_name: Option<String>,
        /// Make them an admin.
        #[arg(long)]
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
    /// List users.
    List,
    /// Disable a user and end their sessions.
    Disable { username: String },
    /// Let a disabled user log in again.
    Enable { username: String },
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

/// Print the server's status, logged in as an admin.
pub async fn status(listen: SocketAddr) -> Result<()> {
    let login = connect(listen, None)
        .await?
        .login(admin_credentials()?)
        .await
        .map_err(api_error)?;
    let api = connect(listen, Some(&login.token)).await?;
    let result = api.server_status().await.map_err(api_error);
    if let Err(e) = api.logout().await {
        tracing::debug!(error = %e, "logout");
    }
    let status = result?;
    let list = |v: &[String]| {
        if v.is_empty() {
            "none".to_owned()
        } else {
            v.join(", ")
        }
    };
    println!("name: {}", status.name);
    println!("version: {}", status.version);
    println!(
        "endpoint: {}",
        status.endpoint_id.as_deref().unwrap_or("starting")
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
    Ok(())
}

async fn run(api: &WsClient, command: UserCommand, new_password: Option<String>) -> Result<()> {
    let new_password = || new_password.clone().unwrap_or_default();
    match command {
        UserCommand::Add {
            username,
            display_name,
            admin,
            ..
        } => {
            let user = api
                .user_add(NewUser {
                    username,
                    display_name,
                    password: new_password(),
                    admin,
                })
                .await
                .map_err(api_error)?;
            println!(
                "added {}{}",
                user.username,
                if user.is_admin { " (admin)" } else { "" }
            );
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
                let mut flags = Vec::new();
                if user.is_admin {
                    flags.push("admin");
                }
                if user.disabled {
                    flags.push("disabled");
                }
                if !user.has_password {
                    flags.push("no password");
                }
                let mut line = user.username;
                if let Some(name) = user.display_name {
                    line.push_str(&format!(" \"{name}\""));
                }
                if !flags.is_empty() {
                    line.push_str(&format!(" ({})", flags.join(", ")));
                }
                println!("{line}");
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wildcard_listen_address_is_reached_over_loopback() {
        assert_eq!(url("0.0.0.0:7421".parse().unwrap()), "ws://127.0.0.1:7421");
        assert_eq!(url("[::]:7421".parse().unwrap()), "ws://[::1]:7421");
        assert_eq!(url("127.0.0.1:9".parse().unwrap()), "ws://127.0.0.1:9");
    }
}
