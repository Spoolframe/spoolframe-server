//! The binary as an operator runs it.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};

fn server() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_spoolframe-server"));
    cmd.env_remove("SPOOLFRAME_CONFIG");
    cmd
}

#[tokio::test]
async fn version_names_the_binary_and_its_version() {
    let output = server().arg("--version").output().expect("the binary runs");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("--version prints UTF-8");
    assert_eq!(
        stdout.trim(),
        format!("spoolframe-server {}", env!("CARGO_PKG_VERSION"))
    );
}

/// A config for a server whose state lives in a directory of the test's
/// own, under a random name so runs at once never share one; the config
/// file is in it too. The directory goes when the first value is dropped.
/// Its admin API is on a port of its own, so test servers never share one.
async fn config_with_state(
    name: &str,
    plex_url: &str,
) -> (spoolframe_test_dir::TestDir, std::path::PathBuf) {
    let tmp = spoolframe_test_dir::TestDir::new(&format!("spoolframe-cli-{name}")).await;
    let state = tmp.as_ref().join("state");
    let path = tmp.as_ref().join(format!("{name}.ucl"));
    std::fs::write(
        &path,
        format!(
            r#"
            schema = "spoolframe.server.v1";
            state_dir = "{}";
            plex {{ url = "{plex_url}"; }}
            admin {{ listen = "127.0.0.1:{}"; }}
            "#,
            state.display(),
            free_port()
        ),
    )
    .unwrap();
    (tmp, path)
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A port that was just free: nothing is listening on it.
fn closed_port_url() -> String {
    format!("http://127.0.0.1:{}", free_port())
}

/// A running server, stopped with SIGTERM when dropped.
struct Running(Child);

impl Running {
    /// Start a server and wait until both its endpoint and admin API are
    /// up. Returns the log so far.
    fn start(config: &Path) -> (Self, String) {
        let mut child = server()
            .arg("--config")
            .arg(config)
            .stderr(Stdio::piped())
            .spawn()
            .expect("the binary runs");
        let stderr = child.stderr.take().unwrap();
        let mut log = String::new();
        let mut lines = BufReader::new(stderr).lines();
        for line in lines.by_ref() {
            let line = line.unwrap();
            log.push_str(&line);
            log.push('\n');
            if line.contains("listening") && line.contains("endpoint_id") {
                break;
            }
        }
        // Keep draining so the server never blocks on a full pipe.
        std::thread::spawn(move || lines.for_each(drop));
        (Self(child), log)
    }

    fn stop(mut self) -> std::process::ExitStatus {
        let killed = Command::new("kill")
            .args(["-TERM", &self.0.id().to_string()])
            .status()
            .unwrap();
        assert!(killed.success());
        self.0.wait().unwrap()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

#[tokio::test]
async fn an_unreachable_plex_is_reported_and_the_server_keeps_running() {
    let url = closed_port_url();
    let (_tmp, config) = config_with_state("unreachable-plex", &url).await;
    let (running, log) = Running::start(&config);
    assert!(log.contains("Plex catalog unavailable"), "{log}");
    assert!(log.contains(&url), "{log}");
    assert!(log.contains("admin API listening"), "{log}");
    // SIGTERM is a clean stop, not a failure.
    assert!(running.stop().success());
}

/// The setup code a fresh server printed in its log.
fn setup_code(log: &str) -> String {
    let marker = "the setup code is ";
    let start = log
        .find(marker)
        .unwrap_or_else(|| panic!("no setup code in: {log}"))
        + marker.len();
    log[start..start + 14].to_owned()
}

/// Run a CLI command against `config`, with `stdin` and environment.
fn cli(config: &Path, args: &[&str], stdin: &str, env: &[(&str, &str)]) -> Output {
    let mut child = server()
        .arg("--config")
        .arg(config)
        .args(args)
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[tokio::test]
async fn setup_then_manage_users_from_the_cli() {
    let (_tmp, config) = config_with_state("cli-users", &closed_port_url()).await;
    let (running, log) = Running::start(&config);
    let code = setup_code(&log);
    let admin = [
        ("SPOOLFRAME_ADMIN_USER", "root"),
        ("SPOOLFRAME_ADMIN_PASSWORD", "correct horse"),
    ];

    let out = cli(
        &config,
        &[
            "setup",
            "--code",
            &code,
            "--username",
            "root",
            "--password-stdin",
        ],
        "correct horse\n",
        &[],
    );
    assert!(out.status.success(), "{out:?}");

    let out = cli(
        &config,
        &["user", "add", "alice", "--password-stdin"],
        "battery staple\n",
        &admin,
    );
    assert!(out.status.success(), "{out:?}");

    // Each line without when the user was last seen, which is a clock.
    let list = |admin: &[(&str, &str)]| {
        let out = cli(&config, &["user", "list"], "", admin);
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(|l| l.split("  ").next().unwrap().to_owned() + "\n")
            .collect::<String>()
    };
    assert_eq!(list(&admin), "alice (consumer)\nroot (admin)\n");
    // Alice has never signed in; root just did.
    let out = cli(&config, &["user", "list"], "", &admin);
    let full = String::from_utf8(out.stdout).unwrap();
    assert!(full.contains("alice (consumer)  never signed in"), "{full}");
    assert!(full.contains("root (admin)  last seen "), "{full}");

    // The last admin cannot step down; once alice is one, root can.
    let out = cli(&config, &["user", "role", "root", "consumer"], "", &admin);
    assert!(!out.status.success(), "{out:?}");
    let out = cli(&config, &["user", "role", "alice", "admin"], "", &admin);
    assert!(out.status.success(), "{out:?}");
    let out = cli(&config, &["user", "role", "root", "consumer"], "", &admin);
    assert!(out.status.success(), "{out:?}");
    // root is a consumer now: the CLI's admin calls are refused for them.
    let out = cli(&config, &["user", "list"], "", &admin);
    assert!(!out.status.success(), "{out:?}");
    let alice = [
        ("SPOOLFRAME_ADMIN_USER", "alice"),
        ("SPOOLFRAME_ADMIN_PASSWORD", "battery staple"),
    ];
    assert_eq!(list(&alice), "alice (admin)\nroot (consumer)\n");

    // Alice's PIN, set as her; cleared by an admin.
    let out = cli(
        &config,
        &["user", "pin", "set", "root", "--stdin"],
        "correct horse\n4321\n",
        &[],
    );
    assert!(out.status.success(), "{out:?}");
    let out = cli(&config, &["user", "devices", "root"], "", &alice);
    assert!(out.status.success(), "{out:?}");
    // No client has signed in as root: no device to require it on.
    let out = cli(
        &config,
        &["user", "pin", "require", "root", "--device", "TV"],
        "",
        &alice,
    );
    assert!(!out.status.success(), "{out:?}");
    let out = cli(&config, &["user", "pin", "clear", "root"], "", &alice);
    assert!(out.status.success(), "{out:?}");

    // The hidden `--admin` still means `--role admin`.
    let out = cli(
        &config,
        &["user", "add", "bob", "--admin", "--password-stdin"],
        "bob's password\n",
        &alice,
    );
    assert!(out.status.success(), "{out:?}");
    assert!(list(&alice).contains("bob (admin)"));

    let out = cli(
        &config,
        &["user", "list"],
        "",
        &[
            ("SPOOLFRAME_ADMIN_USER", "root"),
            ("SPOOLFRAME_ADMIN_PASSWORD", "wrong"),
        ],
    );
    assert!(!out.status.success(), "{out:?}");

    running.stop();
}

#[tokio::test]
async fn the_cli_says_when_the_server_is_not_running() {
    let (_tmp, config) = config_with_state("cli-no-server", &closed_port_url()).await;
    let out = cli(
        &config,
        &["user", "list"],
        "",
        &[
            ("SPOOLFRAME_ADMIN_USER", "root"),
            ("SPOOLFRAME_ADMIN_PASSWORD", "x"),
        ],
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("is the server running?"), "{stderr}");
}

/// What `spoolframe-server <command>` prints, trimmed.
fn printed(config: &Path, command: &str) -> String {
    let output = server()
        .arg("--config")
        .arg(config)
        .arg(command)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[tokio::test]
async fn the_fingerprint_is_the_key_s_and_the_server_logs_it() {
    let (_tmp, config) = config_with_state("fingerprint", &closed_port_url()).await;
    // With the server stopped.
    let fingerprint = printed(&config, "fingerprint");
    let id = printed(&config, "id");
    assert_eq!(
        spoolframe_protocol::fingerprint::of_hex(&id).as_deref(),
        Some(fingerprint.as_str())
    );
    let (running, log) = Running::start(&config);
    assert!(
        log.lines()
            .any(|l| l.contains("server identity") && l.contains(&fingerprint)),
        "{log}"
    );
    assert!(running.stop().success());
}

#[tokio::test]
async fn id_prints_the_same_endpoint_id_every_time() {
    let (_tmp, config) = config_with_state("id", &closed_port_url()).await;
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
