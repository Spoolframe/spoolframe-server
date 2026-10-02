//! The binary as an operator runs it.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

fn server() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_spoolframe-server"));
    cmd.env_remove("SPOOLFRAME_CONFIG");
    cmd
}

#[test]
fn version_names_the_binary_and_its_version() {
    let output = server().arg("--version").output().expect("the binary runs");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("--version prints UTF-8");
    assert_eq!(
        stdout.trim(),
        format!("spoolframe-server {}", env!("CARGO_PKG_VERSION"))
    );
}

/// Write `body` as a config file under the test's own tmp dir.
fn config_file(name: &str, body: &str) -> std::path::PathBuf {
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::write(&path, body).unwrap();
    path
}

/// A config for a server whose state lives in its own tmp dir.
fn config_with_state(name: &str, plex_url: &str) -> std::path::PathBuf {
    let state = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-state"));
    let _ = std::fs::remove_dir_all(&state);
    config_file(
        &format!("{name}.ucl"),
        &format!(
            r#"
            schema = "spoolframe.server.v1";
            state_dir = "{}";
            plex {{ url = "{plex_url}"; }}
            "#,
            state.display()
        ),
    )
}

/// A port that was just free: nothing is listening on it.
fn closed_port_url() -> String {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    format!("http://127.0.0.1:{port}")
}

#[test]
fn an_unreachable_plex_is_reported_and_the_server_keeps_running() {
    let url = closed_port_url();
    let config = config_with_state("unreachable-plex", &url);
    let mut child = server()
        .arg("--config")
        .arg(&config)
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary runs");

    // Read the log until the endpoint is up; the Plex warning comes first.
    let mut log = String::new();
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    for line in lines.by_ref() {
        let line = line.unwrap();
        log.push_str(&line);
        log.push('\n');
        if line.contains("listening") {
            break;
        }
    }
    assert!(log.contains("Plex catalog unavailable"), "{log}");
    assert!(log.contains(&url), "{log}");
    assert!(log.contains("listening"), "{log}");

    // SIGTERM is a clean stop, not a failure.
    let killed = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let status = child.wait().unwrap();
    for line in lines {
        log.push_str(&line.unwrap());
    }
    assert!(status.success(), "{status:?}\n{log}");
    assert!(log.contains("shutting down"), "{log}");
}

#[test]
fn id_prints_the_same_endpoint_id_every_time() {
    let config = config_with_state("id", &closed_port_url());
    let id = || {
        let output = server()
            .arg("--config")
            .arg(&config)
            .arg("id")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    let first = id();
    assert_eq!(first.len(), 64, "{first}");
    assert!(first.bytes().all(|b| b.is_ascii_hexdigit()), "{first}");
    assert_eq!(id(), first);
}

#[test]
fn a_bad_config_path_fails_and_names_the_path() {
    let output = server()
        .args(["--config", "/nonexistent/spoolframe.ucl"])
        .output()
        .expect("the binary runs");
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("/nonexistent/spoolframe.ucl"), "{stderr}");
}
