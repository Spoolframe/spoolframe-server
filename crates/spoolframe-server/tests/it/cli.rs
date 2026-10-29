//! The binary as an operator runs it.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};

fn server() -> Command {
    let mut cmd = Command::new(crate::support::server_binary());
    for var in [
        "SPOOLFRAME_CONFIG",
        "SPOOLFRAME_STATE_DIR",
        "STATE_DIRECTORY",
        "XDG_STATE_HOME",
    ] {
        cmd.env_remove(var);
    }
    cmd
}

/// The server with the config `config_with_state` wrote, and its state
/// directory beside it.
fn server_with(config: &Path) -> Command {
    let mut cmd = server();
    cmd.arg("--config")
        .arg(config)
        .arg("--state-dir")
        .arg(config.with_file_name("state"));
    cmd
}

#[tokio::test]
async fn version_names_the_binary_and_its_version() {
    let output = server().arg("--version").output().expect("the binary runs");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("--version prints UTF-8");
    let mut lines = stdout.lines();
    // The first line alone is what the image's smoke test reads.
    assert_eq!(
        lines.next(),
        Some(format!("spoolframe-server {}", env!("CARGO_PKG_VERSION")).as_str())
    );
    let rest: Vec<&str> = lines.collect();
    assert_eq!(
        rest,
        [
            format!("client protocol: {}", spoolframe_protocol::PROTOCOL_VERSION).as_str(),
            "experimental features (each on by its setting): http-playback, federation",
        ]
    );
    // `-V` is the crate's version alone.
    let short = server().arg("-V").output().expect("the binary runs");
    assert_eq!(
        String::from_utf8(short.stdout).unwrap().trim(),
        format!("spoolframe-server {}", env!("CARGO_PKG_VERSION"))
    );
}

/// A config for a server whose state lives in a directory of the test's
/// own, under a random name so runs at once never share one; the config
/// file is in it too, beside `state/` ([`server_with`]). The directory goes when the first value is dropped.
/// Its admin API is on a port of its own, so test servers never share one.
async fn config_with_state(
    name: &str,
    plex_url: &str,
) -> (spoolframe_test_dir::TestDir, std::path::PathBuf) {
    let tmp = spoolframe_test_dir::TestDir::new(&format!("spoolframe-cli-{name}")).await;
    let path = tmp.as_ref().join(format!("{name}.ucl"));
    std::fs::write(
        &path,
        format!(
            r#"
            schema = "spoolframe.server.v1";
            plex {{ url = "{plex_url}"; }}
            admin {{ listen = "127.0.0.1:{}"; }}
            "#,
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

/// A running server, stopped with SIGTERM when dropped, and everything it
/// has logged.
struct Running(Child, std::sync::Arc<parking_lot::Mutex<String>>);

impl Running {
    /// Start a server and wait until both its endpoint and admin API are
    /// up. Returns the log so far.
    fn start(config: &Path) -> (Self, String) {
        Self::start_with(config, &[])
    }

    /// [`Running::start`], with `env` set for the server.
    fn start_with(config: &Path, env: &[(&str, &str)]) -> (Self, String) {
        let mut command = server_with(config);
        command.envs(env.iter().copied());
        Self::start_command(command)
    }

    /// [`Running::start`] of `command`, a [`server_with`].
    fn start_command(mut command: Command) -> (Self, String) {
        let mut child = command
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
        // Keep reading so the server never blocks on a full pipe.
        let kept = std::sync::Arc::new(parking_lot::Mutex::new(log.clone()));
        let more = kept.clone();
        std::thread::spawn(move || {
            for line in lines.map_while(Result::ok) {
                let mut kept = more.lock();
                kept.push_str(&line);
                kept.push('\n');
            }
        });
        (Self(child, kept), log)
    }

    /// Everything logged so far.
    fn log(&self) -> String {
        self.1.lock().clone()
    }

    /// Everything logged once `needle` is, waiting up to 10 seconds for
    /// the line to be read.
    fn log_with(&self, needle: &str) -> String {
        for _ in 0..200 {
            let log = self.log();
            if log.contains(needle) {
                return log;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("never logged {needle:?}: {}", self.log());
    }

    fn stop(mut self) -> std::process::ExitStatus {
        let killed = Command::new("kill")
            .args(["-TERM", &self.0.id().to_string()])
            .status()
            .unwrap();
        assert!(killed.success());
        self.0.wait().unwrap()
    }

    /// [`Running::stop`], which must be clean (SIGTERM is not a failure).
    /// Says how it ended, and the log, when it is not.
    fn stop_cleanly(self) {
        let log = self.1.clone();
        let status = self.stop();
        if !status.success() {
            // Time for the reader thread to take the last lines.
            std::thread::sleep(std::time::Duration::from_millis(500));
            panic!("stopped with {status}:\n{}", log.lock());
        }
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
    assert!(log.contains("admin API listening"), "{log}");
    // Checked in the background once the server is up (review
    // 2026-10-11 L41).
    let log = running.log_with("Plex catalog unavailable");
    assert!(log.contains("Plex catalog unavailable"), "{log}");
    assert!(log.contains(&url), "{log}");
    // SIGTERM is a clean stop, not a failure.
    running.stop_cleanly();
}

/// `path`'s permission bits.
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .permissions()
        .mode()
        & 0o777
}

/// Started under the usual umask, the server keeps what it writes to its
/// own user (PX-11 finding 35): its umask is 077, its caches 0700 (one an
/// older server left open too), and its database 0600.
#[tokio::test]
async fn the_server_keeps_its_state_and_caches_to_its_own_user() {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;
    let (_tmp, config) = config_with_state("private-state", &closed_port_url()).await;
    let state = config.with_file_name("state");
    let old = state.join("cache").join("artwork");
    std::fs::create_dir_all(&old).unwrap();
    for dir in [&state.join("cache"), &old] {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut command = server_with(&config);
    // SAFETY: only umask(2), async-signal-safe, between fork and exec.
    unsafe {
        command.pre_exec(|| {
            libc::umask(0o022);
            Ok(())
        });
    }
    let (running, _log) = Running::start_command(command);
    let status = std::fs::read_to_string(format!("/proc/{}/status", running.0.id())).unwrap();
    assert!(status.contains("Umask:\t0077"), "{status}");
    for dir in [
        "cache",
        "cache/artwork",
        "cache/previews",
        "cache/subtitles",
        "cache/transcode",
    ] {
        assert_eq!(mode(&state.join(dir)), 0o700, "{dir}");
    }
    for file in ["spoolframe.db", "spoolframe.db-wal"] {
        assert_eq!(mode(&state.join(file)), 0o600, "{file}");
    }
    running.stop_cleanly();
}

/// `config`'s `admin { … }` with `settings` added.
fn with_admin(config: &Path, settings: &str) {
    let text = std::fs::read_to_string(config)
        .unwrap()
        .replace("admin {", &format!("admin {{ {settings}"));
    std::fs::write(config, text).unwrap();
}

/// A loopback listener with a public URL is a proxy on this host; without
/// `trusted_proxies` every caller looks like loopback, so the server does
/// not start (PX-11 finding 38, the second review's M-4).
#[tokio::test]
async fn a_same_host_proxy_without_trusted_proxies_does_not_start() {
    let (_tmp, config) = config_with_state("unnamed-proxy", &closed_port_url()).await;
    with_admin(&config, r#"public_url = "https://media.example.org";"#);
    let mut child = server_with(&config)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("the binary runs");
    // A server that starts never exits by itself: give it a while, then
    // fail rather than wait for ever.
    let started = std::time::Instant::now();
    while child.try_wait().unwrap().is_none() {
        if started.elapsed() > std::time::Duration::from_secs(30) {
            let _ = child.kill();
            panic!("the server started with an unnamed proxy in front");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("admin.trusted_proxies"), "{stderr}");
    assert!(stderr.contains("admin.not_proxied"), "{stderr}");
}

/// The same shape starts once the proxy is named.
#[tokio::test]
async fn a_same_host_proxy_named_in_trusted_proxies_starts() {
    let (_tmp, config) = config_with_state("named-proxy", &closed_port_url()).await;
    with_admin(
        &config,
        r#"public_url = "https://media.example.org"; trusted_proxies = "127.0.0.1";"#,
    );
    let (running, log) = Running::start(&config);
    assert!(log.contains("admin API listening"), "{log}");
    running.stop_cleanly();
}

/// And once the config says nothing proxies to the listener: a tunnel's
/// or this host's callers are local.
#[tokio::test]
async fn a_loopback_listener_said_not_to_be_proxied_starts() {
    let (_tmp, config) = config_with_state("not-proxied", &closed_port_url()).await;
    with_admin(
        &config,
        r#"public_url = "https://media.example.org"; not_proxied = true;"#,
    );
    let (running, log) = Running::start(&config);
    assert!(log.contains("admin API listening"), "{log}");
    assert!(log.contains("admin.not_proxied"), "{log}");
    running.stop_cleanly();
}

/// `config`'s listener moved to every IPv4 interface, on the same port.
fn on_every_interface(config: &Path) {
    let text = std::fs::read_to_string(config)
        .unwrap()
        .replace(r#"listen = "127.0.0.1:"#, r#"listen = "0.0.0.0:"#);
    std::fs::write(config, text).unwrap();
}

/// Finding 58 (the third review's G-9): a listener on every interface
/// with a public URL may have a proxy on this host in front, whose
/// callers would all look like loopback. Browsers may as well reach it
/// directly, as the dev server's do, so it starts, and says what to set.
#[tokio::test]
async fn a_public_url_on_every_interface_without_trusted_proxies_is_warned_of() {
    let (_tmp, config) = config_with_state("wildcard-proxy", &closed_port_url()).await;
    on_every_interface(&config);
    // The dev server's shape.
    with_admin(
        &config,
        r#"public_url = "http://localhost:7431"; insecure_cookies = true;"#,
    );
    let (running, log) = Running::start(&config);
    assert!(log.contains("admin API listening"), "{log}");
    let warning = log
        .lines()
        .find(|line| line.contains("WARN") && line.contains("admin.trusted_proxies"));
    assert!(
        warning.is_some_and(|line| line.contains("admin.not_proxied")),
        "{log}"
    );
    running.stop_cleanly();
}

/// The same shape says nothing once the config says nothing proxies to
/// the listener.
#[tokio::test]
async fn a_public_url_on_every_interface_said_not_to_be_proxied_is_not_warned_of() {
    let (_tmp, config) = config_with_state("wildcard-not-proxied", &closed_port_url()).await;
    on_every_interface(&config);
    with_admin(
        &config,
        r#"public_url = "https://media.example.org"; not_proxied = true;"#,
    );
    let (running, log) = Running::start(&config);
    assert!(log.contains("admin API listening"), "{log}");
    assert!(!log.contains("admin.trusted_proxies"), "{log}");
    running.stop_cleanly();
}

/// `config` with `media { … }` added.
fn with_media(config: &Path, media: &str) {
    let mut text = std::fs::read_to_string(config).unwrap();
    text.push_str(&format!("media {{ {media} }}\n"));
    std::fs::write(config, text).unwrap();
}

/// The start's last line names the wire revision the server speaks, so
/// the log says which apps it takes before any is refused.
#[tokio::test]
async fn the_listening_line_names_the_wire_revision() {
    let (_tmp, config) = config_with_state("start-protocol", &closed_port_url()).await;
    let (running, log) = Running::start(&config);
    // The fields as plain text: the log colours their names.
    let listening: String = log
        .lines()
        .find(|l| started(l))
        .unwrap()
        .split('\x1b')
        .map(|part| part.split_once('m').map_or(part, |(_, rest)| rest))
        .collect();
    assert!(
        listening.contains(&format!(
            "protocol={}",
            spoolframe_protocol::PROTOCOL_VERSION
        )),
        "{listening}"
    );
    running.stop_cleanly();
}

/// PX-11 finding 34: on a kernel with Landlock the media tools run
/// confined without being asked, and the server checks it at start: a
/// confined ffprobe is refused its state directory and a listener on
/// loopback. The check's pass is logged at DEBUG: it is the usual case.
#[tokio::test]
async fn the_media_tools_run_confined_by_default() {
    let (_tmp, config) = config_with_state("sandbox-auto", &closed_port_url()).await;
    let (running, log) = Running::start(&config);
    assert!(
        log.contains("media tools run confined: Landlock ABI"),
        "{log}"
    );
    assert!(!log.contains("the media tools are confined"), "{log}");
    running.stop_cleanly();
    let (running, log) = Running::start_with(
        &config,
        &[("SPOOLFRAME_LOG", "info,spoolframe_server=debug")],
    );
    assert!(log.contains("the media tools are confined"), "{log}");
    assert!(log.contains("refused reading"), "{log}");
    // And the subtitle decoder, this binary run again (finding 45).
    assert!(log.contains("the subtitle decoder is confined"), "{log}");
    running.stop_cleanly();
}

/// A process's line `field` of `/proc/<pid>/status`.
fn status_line(pid: u32, field: &str) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .unwrap()
        .lines()
        .find(|l| l.starts_with(&format!("{field}:")))
        .unwrap_or_else(|| panic!("no {field} in /proc/{pid}/status"))
        .to_owned()
}

/// PX-31: the server hardens its own process before anything else. Its
/// parent, this test, as the same user, cannot read its environment and
/// the secrets in it; it has no new privileges and a seccomp filter.
/// Said at DEBUG: it is the usual case.
#[tokio::test]
async fn the_server_s_process_is_hardened() {
    let (_tmp, config) = config_with_state("hardened", &closed_port_url()).await;
    let (running, log) = Running::start_with(
        &config,
        &[
            ("SF_ENV_PX31_SECRET", "s3cret"),
            ("SPOOLFRAME_LOG", "info,spoolframe_server=debug"),
        ],
    );
    let pid = running.0.id();
    let environ = std::fs::read(format!("/proc/{pid}/environ"));
    assert_eq!(
        environ.as_ref().map_err(std::io::Error::kind).err(),
        Some(std::io::ErrorKind::PermissionDenied),
        "{environ:?}"
    );
    assert_eq!(status_line(pid, "NoNewPrivs"), "NoNewPrivs:\t1");
    assert_eq!(status_line(pid, "Seccomp"), "Seccomp:\t2");
    assert!(log.contains("the server's process is hardened"), "{log}");
    running.stop_cleanly();
}

/// "required" refuses to start when the start check cannot show the
/// tools confined: here ffprobe cannot run at all.
#[tokio::test]
async fn a_required_sandbox_that_cannot_be_shown_to_confine_does_not_start() {
    let (_tmp, config) = config_with_state("sandbox-required", &closed_port_url()).await;
    with_media(
        &config,
        r#"sandbox = "required"; ffprobe = "/nonexistent/ffprobe";"#,
    );
    let output = server_with(&config).output().expect("the binary runs");
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(r#"media.sandbox is "required", but"#),
        "{stderr}"
    );
    assert!(stderr.contains("/nonexistent/ffprobe"), "{stderr}");
}

/// "auto" starts all the same, and says so at the error level.
#[tokio::test]
async fn an_auto_sandbox_that_cannot_be_shown_to_confine_starts_and_says_so() {
    let (_tmp, config) = config_with_state("sandbox-fallback", &closed_port_url()).await;
    with_media(&config, r#"ffprobe = "/nonexistent/ffprobe";"#);
    let (running, log) = Running::start(&config);
    assert!(log.contains("MEDIA TOOLS MAY NOT BE CONFINED"), "{log}");
    assert!(log.contains("ERROR"), "{log}");
    running.stop_cleanly();
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
    let mut child = server_with(config)
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

    // A reset prints a one-time password once, and the list says so. With
    // it, bob may only choose a new password: an admin call is refused.
    let out = cli(&config, &["user", "reset-password", "bob"], "", &alice);
    assert!(out.status.success(), "{out:?}");
    let printed = String::from_utf8(out.stdout).unwrap();
    let otp = printed
        .strip_prefix("one-time password for bob: ")
        .and_then(|rest| rest.lines().next())
        .unwrap_or_else(|| panic!("no one-time password in: {printed}"))
        .to_owned();
    assert_eq!(otp.len(), 19, "{otp}");
    assert!(list(&alice).contains("bob (admin, one-time password)"));
    let out = cli(
        &config,
        &["user", "list"],
        "",
        &[
            ("SPOOLFRAME_ADMIN_USER", "bob"),
            ("SPOOLFRAME_ADMIN_PASSWORD", &otp),
        ],
    );
    assert!(!out.status.success(), "{out:?}");
    // Not one's own: that is `user passwd`.
    let out = cli(&config, &["user", "reset-password", "alice"], "", &alice);
    assert!(!out.status.success(), "{out:?}");

    running.stop();
}

/// `spoolframe-server invite` (P6-01): an invite made, listed as made by
/// the server CLI, refused for the later kinds, revoked; and the server's
/// link without a code. The server is off the LAN (no mDNS) and named for
/// the test.
#[tokio::test]
async fn invites_from_the_cli() {
    let tmp = spoolframe_test_dir::TestDir::new("spoolframe-cli-invites").await;
    let config = tmp.as_ref().join("invites.ucl");
    std::fs::write(
        &config,
        format!(
            r#"
            schema = "spoolframe.server.v1";
            name = "CLI Invites";
            plex {{ url = "{}"; }}
            admin {{ listen = "127.0.0.1:{}"; }}
            network {{ mdns = false; plex_discovery_port = 0; }}
            "#,
            closed_port_url(),
            free_port()
        ),
    )
    .unwrap();
    let (running, log) = Running::start(&config);
    let code = setup_code(&log);
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
    let admin = [
        ("SPOOLFRAME_ADMIN_USER", "root"),
        ("SPOOLFRAME_ADMIN_PASSWORD", "correct horse"),
    ];
    let stdout = |out: Output| {
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    };

    let made = stdout(cli(
        &config,
        &[
            "invite",
            "create",
            "--note",
            "Mum's iPad",
            "--uses",
            "2",
            "--expires",
            "3d",
        ],
        "",
        &admin,
    ));
    let line = |label: &str| {
        made.lines()
            .find_map(|l| l.strip_prefix(label))
            .map(str::trim)
            .unwrap_or_else(|| panic!("no {label:?} in {made}"))
            .to_owned()
    };
    // PX-45: the https form, as it is sent.
    assert!(
        line("link").starts_with("https://spoolframe.app/i#"),
        "{made}"
    );
    let link = spoolframe_protocol::invite::parse_link(&line("link"))
        .unwrap_or_else(|| panic!("not a link: {made}"));
    assert_eq!(link.name.as_deref(), Some("CLI Invites"));
    assert_eq!(
        link.code,
        spoolframe_protocol::invite::normalize_code(&line("code"))
    );
    assert!(link.code.is_some(), "{made}");
    assert_eq!(link.role, Some(spoolframe_types::Role::Consumer));

    let listed = stdout(cli(&config, &["invite", "list"], "", &admin));
    let row = listed
        .lines()
        .find(|l| l.contains("Mum's iPad"))
        .unwrap_or_else(|| panic!("not listed: {listed}"));
    for part in ["consumer", "2 of 2 left", "by server CLI"] {
        assert!(row.contains(part), "{part:?} in {row}");
    }
    assert!(
        !listed.contains(&line("code")),
        "the code was listed: {listed}"
    );

    let refused = cli(
        &config,
        &["invite", "create", "--kind", "federation"],
        "",
        &admin,
    );
    assert!(!refused.status.success(), "{refused:?}");
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("experimental.federation"), "{stderr}");

    let id = row.split_whitespace().next().unwrap();
    stdout(cli(&config, &["invite", "revoke", id], "", &admin));
    let listed = stdout(cli(&config, &["invite", "list"], "", &admin));
    assert!(!listed.contains("Mum's iPad"), "{listed}");

    let printed = stdout(cli(&config, &["invite", "link"], "", &admin));
    assert!(
        printed.trim().starts_with("https://spoolframe.app/i#"),
        "{printed}"
    );
    let server =
        spoolframe_protocol::invite::parse_link(printed.trim()).expect("the server's link");
    assert_eq!((server.server, server.code), (link.server, None));

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
    let output = server_with(config).arg(command).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[tokio::test]
async fn the_fingerprint_is_the_key_s_and_the_server_logs_it() {
    let (_tmp, config) = config_with_state("fingerprint", &closed_port_url()).await;
    // With the server stopped.
    let fingerprint = printed(&config, "fingerprint");
    let (id, _) = ids(&config);
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
    running.stop_cleanly();
}

/// What `spoolframe-server id` prints: the endpoint id, then the server
/// id, each on its own line after its label.
fn ids(config: &Path) -> (String, String) {
    let printed = printed(config, "id");
    let label = |line: Option<&str>, label: &str| {
        line.and_then(|l| l.strip_prefix(label))
            .map(|id| id.trim().to_owned())
            .unwrap_or_else(|| panic!("no {label:?} line in {printed:?}"))
    };
    let mut lines = printed.lines();
    let endpoint_id = label(lines.next(), "endpoint id:");
    let server_id = label(lines.next(), "server id:");
    assert_eq!(lines.next(), None, "{printed}");
    (endpoint_id, server_id)
}

#[tokio::test]
async fn id_prints_the_same_endpoint_id_every_time() {
    let (_tmp, config) = config_with_state("id", &closed_port_url()).await;
    let (first, _) = ids(&config);
    assert_eq!(first.len(), 64, "{first}");
    assert!(first.bytes().all(|b| b.is_ascii_hexdigit()), "{first}");
    assert_eq!(ids(&config).0, first);
}

/// The server id is made at the first start, beside the key, and the
/// server logs it.
#[tokio::test]
async fn id_prints_the_same_server_id_every_time_and_the_server_logs_it() {
    let (_tmp, config) = config_with_state("server-id", &closed_port_url()).await;
    let (_, first) = ids(&config);
    assert!(
        spoolframe_protocol::ServerId::parse(&first).is_ok(),
        "{first}"
    );
    assert_eq!(ids(&config).1, first);
    let (running, log) = Running::start(&config);
    assert!(
        log.lines()
            .any(|l| l.contains("server identity") && l.contains(&first)),
        "{log}"
    );
    running.stop_cleanly();
}

#[test]
fn a_bad_config_path_fails_and_names_the_path() {
    let output = server()
        .args([
            "--config",
            "/nonexistent/spoolframe.ucl",
            "--state-dir",
            "/nonexistent/state",
        ])
        .output()
        .expect("the binary runs");
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("/nonexistent/spoolframe.ucl"), "{stderr}");
}

#[test]
fn config_defaults_prints_every_setting_at_its_default() {
    // No config and no state directory needed.
    let output = server()
        .args(["config", "defaults"])
        .env_remove("HOME")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.contains("schema = \"spoolframe.server.v1\";"),
        "{text}"
    );
    assert!(text.contains("max_jobs = 2;"), "{text}");
    assert!(text.contains("copy_segment_secs = 2;"), "{text}");
}

/// Whose state directory `id` used: which of `dirs` got the key.
fn key_written_in(dirs: &[&Path]) -> Vec<bool> {
    dirs.iter()
        .map(|d| d.join(spoolframe_server::identity::KEY_FILE).exists())
        .collect()
}

#[tokio::test]
async fn the_state_dir_flag_wins_over_each_variable_in_turn() {
    let tmp = spoolframe_test_dir::TestDir::new("spoolframe-cli-precedence").await;
    let [flag, ours, systemd] = ["flag", "ours", "systemd"].map(|d| tmp.as_ref().join(d));
    let id = |args: &[&str], env: &[(&str, &Path)]| {
        let output = server()
            .args(args)
            .arg("id")
            .envs(env.iter().map(|(k, v)| (*k, v.as_os_str())))
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    };
    let both = [
        ("SPOOLFRAME_STATE_DIR", ours.as_path()),
        ("STATE_DIRECTORY", systemd.as_path()),
    ];
    id(&["--state-dir", flag.to_str().unwrap()], &both);
    assert_eq!(
        key_written_in(&[&flag, &ours, &systemd]),
        [true, false, false]
    );
    id(&[], &both);
    assert_eq!(
        key_written_in(&[&flag, &ours, &systemd]),
        [true, true, false]
    );
    id(&[], &both[1..]);
    assert_eq!(
        key_written_in(&[&flag, &ours, &systemd]),
        [true, true, true]
    );
}

#[tokio::test]
async fn the_config_flag_wins_over_the_variable() {
    let tmp = spoolframe_test_dir::TestDir::new("spoolframe-cli-config-flag").await;
    let flag = tmp.as_ref().join("flag.ucl");
    let env = tmp.as_ref().join("env.ucl");
    std::fs::write(&flag, "name = \"From the flag\";").unwrap();
    std::fs::write(&env, "name = \"From the variable\";").unwrap();
    let show = |args: &[&str]| {
        let output = server()
            .args(args)
            .args(["--state-dir", tmp.as_ref().join("state").to_str().unwrap()])
            .args(["config", "show"])
            .env("SPOOLFRAME_CONFIG", &env)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    };
    assert!(show(&["--config", flag.to_str().unwrap()]).contains("\"From the flag\""));
    assert!(show(&[]).contains("\"From the variable\""));
}

#[tokio::test]
async fn config_show_names_each_value_s_layer_and_never_a_secret() {
    let tmp = spoolframe_test_dir::TestDir::new("spoolframe-cli-show").await;
    let config = tmp.as_ref().join("server.ucl");
    let state = tmp.as_ref().join("state");
    std::fs::write(
        &config,
        r#"name = "Nimble"; plex { url = "http://nas:32400"; } transcode { max_jobs = 4; }"#,
    )
    .unwrap();
    std::fs::create_dir_all(state.join("secrets")).unwrap();
    std::fs::write(state.join("runtime.ucl"), r#"name = "Den";"#).unwrap();
    std::fs::write(state.join("secrets/plex-token"), "s3cr3t-t0ken").unwrap();
    let output = server()
        .args(["--config", config.to_str().unwrap()])
        .args(["--state-dir", state.to_str().unwrap()])
        .args(["config", "show"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let shown = String::from_utf8(output.stdout).unwrap();
    for line in [
        r#"name = "Den";  # runtime, over the config file's "Nimble""#,
        r#"plex.url = "http://nas:32400";  # config file"#,
        "transcode.max_jobs = 4;  # config file",
        "transcode.read_ahead_secs = 60;  # default",
    ] {
        assert!(shown.lines().any(|l| l == line), "no {line:?} in {shown}");
    }
    assert!(shown.contains("plex.token = (set);"), "{shown}");
    let everything = format!("{shown}{}", String::from_utf8_lossy(&output.stderr));
    assert!(!everything.contains("s3cr3t"), "{everything}");
}

#[tokio::test]
async fn a_file_that_still_names_its_state_dir_is_refused_saying_what_to_do() {
    let tmp = spoolframe_test_dir::TestDir::new("spoolframe-cli-old-state-dir").await;
    let config = tmp.as_ref().join("old.ucl");
    std::fs::write(
        &config,
        r#"schema = "spoolframe.server.v1"; state_dir = "/srv/spoolframe"; plex { url = "http://nas:32400"; }"#,
    )
    .unwrap();
    let output = server()
        .args(["--config", config.to_str().unwrap()])
        .args(["--state-dir", tmp.as_ref().join("state").to_str().unwrap()])
        .arg("id")
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&format!(
            "{}: state_dir is no longer read from the config file: remove it, and pass \
             --state-dir /srv/spoolframe (or set SPOOLFRAME_STATE_DIR=/srv/spoolframe) instead",
            config.display()
        )),
        "{stderr}"
    );
}

#[tokio::test]
async fn without_plex_the_server_starts_not_set_up_and_says_where_to_set_it_up() {
    let tmp = spoolframe_test_dir::TestDir::new("spoolframe-cli-not-set-up").await;
    // Only where to listen, so test servers never share a port; no Plex.
    let port = free_port();
    let config = tmp.as_ref().join("server.ucl");
    std::fs::write(
        &config,
        format!(r#"admin {{ listen = "127.0.0.1:{port}"; }} network {{ mdns = false; }}"#),
    )
    .unwrap();
    let (running, log) = Running::start(&config);
    assert!(log.contains("not set up"), "{log}");
    let url = format!("http://127.0.0.1:{port}/");
    let setup_line = log
        .lines()
        .find(|l| l.contains("the setup code is "))
        .unwrap_or_else(|| panic!("no setup code in: {log}"));
    assert!(setup_line.contains(&url), "{setup_line}");
    // The web UI's first page is served: setup is open.
    let open: serde_json::Value = reqwest::get(format!("{url}auth/setup"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(open["open"], true, "{open}");
    running.stop_cleanly();
}

#[tokio::test]
async fn config_set_and_unset_change_the_running_server_through_its_admin_api() {
    let (_tmp, config) = config_with_state("config-set", &closed_port_url()).await;
    let (running, log) = Running::start(&config);
    let code = setup_code(&log);
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
    let admin = [
        ("SPOOLFRAME_ADMIN_USER", "root"),
        ("SPOOLFRAME_ADMIN_PASSWORD", "correct horse"),
    ];
    let set = |args: &[&str], stdin: &str| {
        let out = cli(&config, args, stdin, &admin);
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    };

    // Live: the running server is called so at once.
    assert_eq!(
        set(&["config", "set", "name", "Den"], ""),
        "name = \"Den\"\n"
    );
    let status = set(&["status"], "");
    assert!(status.contains("name: Den"), "{status}");
    // The server id and the passkey domain it names (PX-14).
    let (_, server_id) = ids(&config);
    assert!(
        status
            .lines()
            .any(|l| l == format!("server id: {server_id}")),
        "{status}"
    );
    assert!(
        status
            .lines()
            .any(|l| l == format!("passkey domain: {server_id}.spoolframe.app")),
        "{status}"
    );
    // Restart: saved, and said so.
    let port = set(&["config", "set", "network.port", "7440"], "");
    assert_eq!(port, "network.port = 7440\napplies after a restart\n");

    // A refused value says why, as a file with it would.
    let out = cli(
        &config,
        &["config", "set", "transcode.max_jobs", "0"],
        "",
        &admin,
    );
    assert!(!out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("invalid config: transcode.max_jobs: 0 would refuse every conversion"),
        "{stderr}"
    );

    // The token: kept, and never shown or logged.
    let token = set(
        &["config", "set", "plex.token", "--stdin"],
        "s3cr3t-t0ken\n",
    );
    assert_eq!(token, "plex.token = (set)\n");
    let shown = printed_args(&config, &["config", "show"]);
    assert!(shown.contains("plex.token = (set);"), "{shown}");
    assert!(shown.contains(r#"name = "Den";  # runtime"#), "{shown}");
    let state = config.with_file_name("state");
    let runtime = std::fs::read_to_string(state.join("runtime.ucl")).unwrap();
    assert!(runtime.contains("Den"), "{runtime}");
    for (what, text) in [
        ("show", &shown),
        ("runtime.ucl", &runtime),
        ("log", &running.log()),
    ] {
        assert!(
            !text.contains("s3cr3t"),
            "the token is in the {what}: {text}"
        );
    }
    assert!(
        running
            .log()
            .lines()
            .any(|l| l.contains("secret setting changed") && l.contains("root")),
        "{}",
        running.log()
    );

    assert_eq!(
        set(&["config", "unset", "name"], ""),
        "name = \"Spoolframe\"\n"
    );
    running.stop_cleanly();
}

#[tokio::test]
async fn config_set_is_refused_for_what_a_locked_file_sets() {
    let tmp = spoolframe_test_dir::TestDir::new("spoolframe-cli-locked").await;
    let config = tmp.as_ref().join("server.ucl");
    std::fs::write(
        &config,
        format!(
            r#"locked = true; name = "Nimble"; admin {{ listen = "127.0.0.1:{}"; }} network {{ mdns = false; }}"#,
            free_port()
        ),
    )
    .unwrap();
    let (running, log) = Running::start(&config);
    let code = setup_code(&log);
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
        &["config", "set", "name", "Den"],
        "",
        &[
            ("SPOOLFRAME_ADMIN_USER", "root"),
            ("SPOOLFRAME_ADMIN_PASSWORD", "correct horse"),
        ],
    );
    assert!(!out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("name is set by the server's config file") && stderr.contains("locked"),
        "{stderr}"
    );
    running.stop_cleanly();
}

/// What `spoolframe-server <args>` prints.
fn printed_args(config: &Path, args: &[&str]) -> String {
    let output = server_with(config).args(args).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap()
}

/// The admin API's address `config_with_state` gave the server.
fn admin_addr(config: &Path) -> std::net::SocketAddr {
    let text = std::fs::read_to_string(config).unwrap();
    let start = text.find("listen = \"").unwrap() + "listen = \"".len();
    let end = start + text[start..].find('"').unwrap();
    text[start..end].parse().unwrap()
}

/// Linking a Plex account (P3-12), against a stand-in plex.tv, logs that
/// it happened and whose it is, and never the token, even at debug level.
#[tokio::test]
async fn a_plex_link_is_logged_without_its_token() {
    use spoolframe_admin_api::{MeApiClient, PlexLinkState};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const TOKEN: &str = "tok-never-logged-4b1d";
    let tv = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/pins"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "id": 5, "code": "LOGS", "expiresIn": 900, "authToken": null
        })))
        .mount(&tv)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v2/pins/5"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 5, "code": "LOGS", "authToken": TOKEN
        })))
        .mount(&tv)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v2/user"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "username": "rooty" })),
        )
        .mount(&tv)
        .await;
    let (_tmp, config) = config_with_state("plex-link-log", &closed_port_url()).await;
    let tv_url = tv.uri();
    let (running, log) = Running::start_with(
        &config,
        &[
            ("SPOOLFRAME_PLEX_TV_URL", tv_url.as_str()),
            ("SPOOLFRAME_PLEX_APP_URL", "https://app.example"),
            ("SPOOLFRAME_LOG", "debug"),
        ],
    );
    let code = setup_code(&log);
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
    let addr = admin_addr(&config);
    let token = crate::admin::anonymous(addr)
        .await
        .login(crate::admin::creds("root", "correct horse"))
        .await
        .unwrap()
        .token;
    let root = crate::admin::with_token(addr, &token).await;
    let pin = root.plex_link_begin().await.unwrap();
    let state = root.plex_link_check(pin.id).await.unwrap();
    assert!(matches!(state, PlexLinkState::Linked { .. }), "{state:?}");
    root.plex_unlink().await.unwrap();

    let log = running.log_with("Plex account unlinked");
    assert!(log.contains("Plex account linked"), "{log}");
    assert!(log.contains("rooty"), "{log}");
    assert!(!log.contains(TOKEN), "{log}");
}

/// `plex sync-watch` runs every linked user's sync through the running
/// server: here the config's owner, whose Plex server is down, which the
/// run says rather than fails on.
#[tokio::test]
async fn plex_sync_watch_reports_each_linked_user() {
    let (_tmp, config) = config_with_state("sync-watch", &closed_port_url()).await;
    let text = std::fs::read_to_string(&config)
        .unwrap()
        .replace("plex {", "plex { watch_sync { user = \"root\"; }");
    std::fs::write(&config, text).unwrap();
    let (running, log) = Running::start(&config);
    let code = setup_code(&log);
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
    // The owner is found as the server starts.
    running.stop_cleanly();
    let (running, _) = Running::start(&config);
    let admin = [
        ("SPOOLFRAME_ADMIN_USER", "root"),
        ("SPOOLFRAME_ADMIN_PASSWORD", "correct horse"),
    ];
    let out = cli(&config, &["plex", "sync-watch", "--dry-run"], "", &admin);
    assert!(out.status.success(), "{out:?}");
    let printed = String::from_utf8(out.stdout).unwrap();
    assert!(
        printed.contains("root: 0 pulled, 0 pushed, 0 skipped, 0 failed"),
        "{printed}"
    );
    assert!(printed.contains("can't be reached"), "{printed}");
    drop(running);
}

/// A UDP port that was just free, for the endpoint to bind again after a
/// restart.
fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("0.0.0.0:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A server with no Plex, no mDNS and Plex's discovery on any port, its
/// admin API and endpoint on ports of its own: the same ports after a
/// restart. Its admin API's address too.
async fn restart_config(name: &str) -> (spoolframe_test_dir::TestDir, std::path::PathBuf) {
    let tmp = spoolframe_test_dir::TestDir::new(&format!("spoolframe-cli-{name}")).await;
    let config = tmp.as_ref().join("server.ucl");
    std::fs::write(
        &config,
        format!(
            r#"name = "Restart test";
            admin {{ listen = "127.0.0.1:{}"; }}
            network {{ mdns = false; plex_discovery_port = 0; port = {}; }}"#,
            free_port(),
            free_udp_port(),
        ),
    )
    .unwrap();
    (tmp, config)
}

/// The first admin, set up with the code `log` printed, signed in now.
async fn first_admin(config: &Path, log: &str) -> (jsonrpsee::ws_client::WsClient, String) {
    use spoolframe_admin_api::AdminApiClient;
    let addr = admin_addr(config);
    let token = crate::admin::anonymous(addr)
        .await
        .setup(crate::admin::setup(
            &setup_code(log),
            "root",
            "correct horse",
        ))
        .await
        .unwrap()
        .token;
    (crate::admin::with_token(addr, &token).await, token)
}

/// The endpoint's "listening" line: the last of a start.
fn started(line: &str) -> bool {
    line.contains("listening") && line.contains("endpoint_id")
}

/// `line` without its colours.
fn plain(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // CSI: up to and with the final letter.
            chars.by_ref().find(char::is_ascii_alphabetic);
        } else {
            out.push(c);
        }
    }
    out
}

impl Running {
    /// The lines logged once `times` of them are `wanted`, waiting up to
    /// 30 seconds.
    fn lines_once(&self, wanted: fn(&str) -> bool, times: usize) -> Vec<String> {
        self.lines_within(wanted, times, 600)
    }

    /// [`Running::lines_once`], waiting up to `polls` times 50 ms.
    fn lines_within(&self, wanted: fn(&str) -> bool, times: usize, polls: u32) -> Vec<String> {
        for _ in 0..polls {
            let lines: Vec<String> = self.log().lines().map(plain).collect();
            if lines.iter().filter(|l| wanted(l)).count() >= times {
                return lines;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("never logged it {times} times: {}", self.log());
    }
}

/// Started by hand, with no supervisor, the server restarts in place: the
/// same process, logging to the same pipe, starts again and answers on
/// the same port, its sessions kept.
#[tokio::test]
async fn a_restart_without_a_supervisor_runs_the_server_again_in_place() {
    use spoolframe_admin_api::{AdminApiClient, RestartMode};
    let (_tmp, config) = restart_config("restart-in-place").await;
    let (mut running, log) = Running::start(&config);
    let pid = running.0.id();
    let (root, token) = first_admin(&config, &log).await;
    assert_eq!(
        root.settings().await.unwrap().restart_mode,
        RestartMode::InPlace
    );
    let filters = status_line(pid, "Seccomp_filters");
    root.restart().await.unwrap();

    // A fresh start, logged to the same pipe, by the same process.
    let lines = running.lines_once(started, 2);
    let log = lines.join("\n");
    let starts: Vec<&String> = lines.iter().filter(|l| started(l)).collect();
    assert_eq!(starts.len(), 2, "{log}");
    for line in starts {
        let field = format!("pid={pid}");
        assert!(line.split_whitespace().any(|w| w == field), "{line}");
    }
    let config_loaded = lines.iter().filter(|l| l.contains("config loaded")).count();
    assert_eq!(config_loaded, 2, "{log}");
    assert!(log.contains("restarting in place"), "{log}");
    assert!(running.0.try_wait().unwrap().is_none(), "{log}");
    // Hardened again (exec made it dumpable), its seccomp filter kept
    // rather than loaded a second time (PX-31).
    let environ = std::fs::read(format!("/proc/{pid}/environ"));
    assert!(environ.is_err(), "{environ:?}");
    assert_eq!(status_line(pid, "Seccomp_filters"), filters, "{log}");

    // The same port, and the session kept in the store.
    let again = crate::admin::with_token(admin_addr(&config), &token).await;
    assert_eq!(
        again.settings().await.unwrap().restart_mode,
        RestartMode::InPlace
    );
    running.stop_cleanly();
}

/// Under a supervisor, a restart is an exit with 75 (EX_TEMPFAIL), which
/// systemd's Restart=on-failure starts again.
#[tokio::test]
async fn a_restart_under_a_supervisor_exits_for_it_to_start_the_server_again() {
    use spoolframe_admin_api::{AdminApiClient, RestartMode};
    let (_tmp, config) = restart_config("restart-supervised").await;
    let (mut running, log) = Running::start_with(&config, &[("SPOOLFRAME_SUPERVISED", "test")]);
    let (root, _) = first_admin(&config, &log).await;
    assert_eq!(
        root.settings().await.unwrap().restart_mode,
        RestartMode::Supervisor
    );
    root.restart().await.unwrap();
    let mut status = None;
    for _ in 0..600 {
        status = running.0.try_wait().unwrap();
        if status.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let status = status.unwrap_or_else(|| panic!("still running: {}", running.log()));
    assert_eq!(status.code(), Some(75), "{}", running.log());
}

/// PX-33: SIGHUP to a running server reads its config file again. A live
/// setting applies, a restart one waits with the old value running, a
/// file that does not load changes nothing and says why, and a reload
/// with nothing new says so. `config reload` asks for the same through
/// the admin API.
#[tokio::test]
async fn sighup_reloads_the_config_file() {
    use spoolframe_admin_api::AdminApiClient;
    let (_tmp, config) = restart_config("sighup-reload").await;
    let original = std::fs::read_to_string(&config).unwrap();
    let (mut running, log) = Running::start(&config);
    let pid = running.0.id().to_string();
    let (root, _) = first_admin(&config, &log).await;
    let hangup = || {
        let sent = Command::new("kill").args(["-HUP", &pid]).status().unwrap();
        assert!(sent.success());
    };
    let reloads = |running: &Running, times: usize| {
        running
            .lines_once(
                |l| l.contains("config reloaded") || l.contains("not reloaded"),
                times,
            )
            .into_iter()
            .filter(|l| l.contains("config reloaded") || l.contains("not reloaded"))
            .nth(times - 1)
            .unwrap()
    };

    // A live and a restart setting.
    std::fs::write(
        &config,
        original
            .replace("Restart test", "Reloaded")
            .replace("mdns = false", "mdns = true"),
    )
    .unwrap();
    hangup();
    let line = reloads(&running, 1);
    assert!(line.contains("INFO"), "{line}");
    assert!(
        line.contains(
            "SIGHUP: config reloaded: applied name; after a restart: network.mdns; unchanged \
             otherwise"
        ),
        "{line}"
    );
    let status = root.server_status().await.unwrap();
    assert_eq!(status.name, "Reloaded");
    let settings = root.settings().await.unwrap();
    let mdns = settings
        .entries
        .iter()
        .find(|e| e.key == "network.mdns")
        .unwrap();
    assert!(mdns.restart_pending, "{mdns:?}");
    assert_eq!(mdns.running, Some(serde_json::json!(false)));

    // A file that does not load: nothing changes, one ERROR says where.
    let good = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, good.replace("Reloaded", "Broken") + "nmae = 1;\n").unwrap();
    hangup();
    let line = reloads(&running, 2);
    assert!(line.contains("ERROR"), "{line}");
    assert!(
        line.contains("the server keeps the one it had")
            && line.contains(&config.display().to_string()),
        "{line}"
    );
    assert_eq!(root.server_status().await.unwrap().name, "Reloaded");
    assert!(running.0.try_wait().unwrap().is_none(), "{}", running.log());

    // Back, and asked for through the admin API from the CLI: the file
    // is as the server already has it.
    std::fs::write(&config, &good).unwrap();
    let out = cli(
        &config,
        &["config", "reload"],
        "",
        &[
            ("SPOOLFRAME_ADMIN_USER", "root"),
            ("SPOOLFRAME_ADMIN_PASSWORD", "correct horse"),
        ],
    );
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "config reloaded: nothing changed\n"
    );
    let line = reloads(&running, 3);
    assert!(
        line.contains("configReload by root: config reloaded: nothing changed"),
        "{line}"
    );
    hangup();
    let line = reloads(&running, 4);
    assert!(
        line.contains("SIGHUP: config reloaded: nothing changed"),
        "{line}"
    );
    // Encoders detected again, as SIGHUP always did: in the background,
    // one detection at a time, each half a minute on a busy host.
    running.lines_within(
        |l| l.contains("transcoding") && l.contains("encoders"),
        2,
        3600,
    );
    running.stop_cleanly();
}

/// PX-02: a runtime layer from before OpenSubtitles' key was built in
/// names `opensubtitles.api_key` as a variable the server no longer has.
/// The server starts all the same, says once that the key is no longer
/// read, and never shows it.
#[tokio::test]
async fn an_old_opensubtitles_key_in_the_runtime_layer_does_not_stop_the_server() {
    let (_tmp, config) = config_with_state("old-opensubtitles-key", &closed_port_url()).await;
    let state = config.with_file_name("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        state.join("runtime.ucl"),
        r#"schema = "spoolframe.server.v1";
           opensubtitles { api_key = "${SF_ENV_PX02_NEVER_SET}"; languages = ["de"]; }"#,
    )
    .unwrap();
    let (running, log) = Running::start(&config);
    assert!(log.contains("admin API listening"), "{log}");
    assert_eq!(
        log.matches("opensubtitles.api_key is no longer read; the built-in key is used")
            .count(),
        1,
        "{log}"
    );
    running.stop_cleanly();
    let shown = String::from_utf8(
        server_with(&config)
            .args(["config", "show"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert!(!shown.contains("opensubtitles.api_key"), "{shown}");
    assert!(
        shown.contains(r#"opensubtitles.languages = ["de"];"#),
        "{shown}"
    );
}
