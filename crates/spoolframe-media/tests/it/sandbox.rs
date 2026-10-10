//! The media tools' sandbox (PX-11 finding 34): what a confined ffmpeg,
//! ffprobe or shell can and cannot reach. Every test here needs Landlock
//! (Linux 6.2 or later, ABI 3), which the dev machines have; the other
//! suites' real ffmpeg, ffprobe and tesseract run confined through
//! [`confined`] too, which shows normal probes, encodes and OCR still work.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use spoolframe_media::Prober;
use spoolframe_media::sandbox::{GpuDevices, GpuUse, Jail, Limits, Memory, Mode, Sandbox, Tool};

/// The sandbox the suites' real tools run in, with nothing hidden.
fn shared() -> Sandbox {
    static SANDBOX: OnceLock<Sandbox> = OnceLock::new();
    SANDBOX
        .get_or_init(|| {
            Sandbox::new(Mode::Required, Vec::new(), GpuDevices::default())
                .expect("this kernel has Landlock")
        })
        .clone()
}

/// `program`, confined.
pub fn confined(program: &str) -> Tool {
    Tool::new(program, shared())
}

/// A sandbox that hides `state`, as the server hides its state directory.
fn hiding(state: &Path) -> Sandbox {
    Sandbox::new(
        Mode::Required,
        vec![state.to_path_buf()],
        GpuDevices::default(),
    )
    .unwrap()
}

/// A second of test pattern with sound, as Matroska, at `path`.
fn film(path: &Path) {
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=d=1:s=160x120",
        ])
        .args([
            "-f", "lavfi", "-i", "sine=d=1", "-c:v", "libx264", "-c:a", "aac",
        ])
        .args(["-f", "matroska"])
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success());
}

async fn scratch(test: &str) -> (spoolframe_test_dir::TestDir, PathBuf) {
    let tmp = spoolframe_test_dir::TestDir::new(&format!("spoolframe-sandbox-{test}")).await;
    let root = tmp.to_path_buf();
    (tmp, root)
}

/// `script` in a confined bash, its output.
async fn confined_bash(jail: &Jail, script: &str) -> std::process::Output {
    let mut command = confined("bash").command(jail).unwrap();
    command.args(["-c", script]).stdin(Stdio::null());
    command.output().await.unwrap()
}

/// A library file that is a symlink into the state directory (finding 1's
/// swap, done before the probe): what ffprobe would read is the state
/// directory's, so it is not read at all. The "database" here is a real
/// film, so an unconfined ffprobe would describe it.
#[tokio::test]
async fn a_confined_ffprobe_cannot_read_the_state_directory_through_a_symlink() {
    let (_tmp, root) = scratch("state-symlink").await;
    let (state, media) = (root.join("state"), root.join("media"));
    std::fs::create_dir_all(&state).unwrap();
    std::fs::create_dir_all(&media).unwrap();
    film(&state.join("spoolframe.db"));
    std::os::unix::fs::symlink(state.join("spoolframe.db"), media.join("Film.mkv")).unwrap();

    let prober = Prober::new(
        Tool::new("ffprobe", hiding(&state)),
        Duration::from_secs(30),
    );
    let err = prober.describe(&media.join("Film.mkv")).await.unwrap_err();
    assert!(err.to_string().contains("may not read"), "{err}");
    let err = prober
        .describe(&state.join("spoolframe.db"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("may not read"), "{err}");
}

/// A file the run was not given is out of its reach, wherever it is: here
/// the state directory's "database" (a real film, which an unconfined
/// ffprobe describes), named on ffprobe's command line but not an input.
#[tokio::test]
async fn a_confined_ffprobe_cannot_read_a_file_it_was_not_given() {
    let (_tmp, root) = scratch("not-given").await;
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let db = state.join("spoolframe.db");
    film(&db);
    let probe = |tool: Tool| {
        let db = db.clone();
        async move {
            let mut command = tool.command(&Jail::new(Limits::PROBE)).unwrap();
            command
                .args(["-v", "error", "-show_format"])
                .arg(&db)
                .stdin(Stdio::null());
            command.output().await.unwrap()
        }
    };
    assert!(probe(Tool::unconfined("ffprobe")).await.status.success());
    let output = probe(confined("ffprobe")).await;
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Permission denied"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The same probe of a film in the library works.
#[tokio::test]
async fn a_confined_ffprobe_describes_a_film_in_the_library() {
    let (_tmp, root) = scratch("probe").await;
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();
    film(&root.join("Film.mkv"));
    let prober = Prober::new(
        Tool::new("ffprobe", hiding(&state)),
        Duration::from_secs(30),
    );
    let d = prober.describe(&root.join("Film.mkv")).await.unwrap();
    assert!(d.video.is_some());
    assert_eq!(d.audio.len(), 1);
}

/// ffmpeg reads URLs too: a confined one cannot reach a listener on
/// loopback.
#[tokio::test]
async fn a_confined_ffmpeg_cannot_reach_a_listener() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/film.mkv", listener.local_addr().unwrap());
    let mut command = confined("ffmpeg")
        .command(&Jail::new(Limits::PASS))
        .unwrap();
    command
        .args(["-v", "error", "-i", &url, "-f", "null", "-"])
        .stdin(Stdio::null())
        .kill_on_drop(true);
    // One that connected would wait for an answer that never comes.
    let output = tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .expect("ffmpeg connected, and waits for the answer")
        .unwrap();
    assert!(!output.status.success());
    // A connection, had ffmpeg made one, waits to be accepted.
    listener.set_nonblocking(true).unwrap();
    let accepted = listener.accept();
    assert!(
        accepted
            .as_ref()
            .is_err_and(|e| e.kind() == std::io::ErrorKind::WouldBlock),
        "ffmpeg connected: {accepted:?}; it said {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Nor a Unix socket: without a mount namespace the host's (D-Bus, the
/// journal) are in plain sight, so connecting is refused outright.
#[tokio::test]
async fn a_confined_ffmpeg_cannot_connect_to_a_unix_socket() {
    let (_tmp, root) = scratch("unix").await;
    let path = root.join("listener.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("unix:{}", path.display());
    let mut command = confined("ffmpeg")
        .command(&Jail::new(Limits::PASS))
        .unwrap();
    command
        .args(["-v", "error", "-f", "mpegts", "-i", &url, "-f", "null", "-"])
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .expect("ffmpeg connected, and waits for data")
        .unwrap();
    assert!(!output.status.success());
    let accepted = listener.accept();
    assert!(
        accepted
            .as_ref()
            .is_err_and(|e| e.kind() == std::io::ErrorKind::WouldBlock),
        "ffmpeg connected: {accepted:?}; it said {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The job's directory is the one place a run writes.
#[tokio::test]
async fn a_confined_ffmpeg_writes_only_in_its_directory() {
    let (_tmp, root) = scratch("writes").await;
    let (job, outside) = (root.join("job"), root.join("outside"));
    std::fs::create_dir_all(&job).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let tile = |out: PathBuf| {
        let mut command = confined("ffmpeg")
            .command(&Jail::new(Limits::TILE).writes(&job))
            .unwrap();
        command
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=d=1"])
            .args(["-frames:v", "1"])
            .arg(out)
            .stdin(Stdio::null());
        command
    };
    assert!(tile(job.join("a.png")).status().await.unwrap().success());
    assert!(job.join("a.png").exists());
    assert!(
        !tile(outside.join("b.png"))
            .status()
            .await
            .unwrap()
            .success()
    );
    assert!(!outside.join("b.png").exists());
}

/// Nor does it change a file outside it that it may otherwise not read:
/// opening one to write or truncate it is refused.
#[tokio::test]
async fn a_confined_program_cannot_overwrite_a_file_outside_its_directory() {
    let (_tmp, root) = scratch("overwrite").await;
    let (job, outside) = (root.join("job"), root.join("outside"));
    std::fs::create_dir_all(&job).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let db = outside.join("spoolframe.db");
    std::fs::write(&db, "the database").unwrap();
    let output = confined_bash(
        &Jail::new(Limits::PASS).writes(&job),
        &format!("printf x > '{}'", db.display()),
    )
    .await;
    assert!(!output.status.success());
    assert_eq!(std::fs::read_to_string(&db).unwrap(), "the database");
}

/// These programs only start threads: a confined shell cannot start a
/// process. (ffmpeg's threads are what the other suites show working.)
#[tokio::test]
async fn a_confined_program_starts_no_process() {
    let output = confined_bash(&Jail::new(Limits::PASS), "true & wait $!").await;
    assert!(
        !output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("fork"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// What a run writes it cannot run: its directory is not executable.
#[tokio::test]
async fn a_confined_program_cannot_run_what_it_wrote() {
    let (_tmp, root) = scratch("exec").await;
    let script = root.join("payload");
    std::fs::write(&script, "#!/bin/sh\necho ran\n").unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let output = confined_bash(
        &Jail::new(Limits::PASS).writes(&root),
        &format!("exec '{}'", script.display()),
    )
    .await;
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("ran"));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Permission denied"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// No PID namespace hides the server, so Landlock does (ABI 6, Linux
/// 6.12): a run cannot signal it, nor read its /proc entries.
#[tokio::test]
async fn a_confined_program_can_neither_signal_nor_read_the_server() {
    let jail = Jail::new(Limits::PASS);
    let server = std::process::id();
    let unconfined = {
        let mut command = Tool::unconfined("bash").command(&jail).unwrap();
        command
            .args(["-c", &format!("kill -0 {server}")])
            .stdin(Stdio::null());
        command.output().await.unwrap()
    };
    assert!(unconfined.status.success());
    let abi = shared().landlock_abi().expect("confined");
    if abi >= 6 {
        let output = confined_bash(&jail, &format!("kill -0 {server}")).await;
        assert!(
            !output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    } else {
        eprintln!("Landlock ABI {abi}: signals are not scoped before ABI 6");
    }
    let output = confined_bash(
        &jail,
        &format!("read -r -d '' environ < /proc/{server}/environ; echo \"${{#environ}}\""),
    )
    .await;
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Permission denied"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// What a shell tells of each of `paths`: `opened <path>` or `refused
/// <path>`, opened to read (`<`) or to write and truncate (`>`), as
/// `mode` says. No subshell: a confined shell cannot fork.
fn open_each(mode: &str, paths: &[&str]) -> String {
    let mut script = String::new();
    for path in paths {
        script.push_str(&format!(
            "if : {mode} \"{path}\"; then echo \"opened {path}\"; \
             else echo \"refused {path}\"; fi\n"
        ));
    }
    script
}

/// The lines of a run's output.
fn said(output: &std::process::Output) -> Vec<String> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

/// CUDA's cuInit names a thread through its own
/// `/proc/self/task/<tid>/comm`, opened to write and truncate, and reads
/// its own `/proc/self/maps`: an NVIDIA run may, as CUDA does it.
#[tokio::test]
async fn an_nvidia_run_may_name_its_threads_and_read_its_own_maps() {
    let jail = Jail::new(Limits::PASS).gpu(GpuUse::Nvidia);
    let output = confined_bash(
        &jail,
        "printf cuda-worker > /proc/self/task/$$/comm && read -r name < /proc/self/task/$$/comm \
         && read -r mapping < /proc/self/maps && echo \"$name ${#mapping}\"",
    )
    .await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let said = said(&output);
    let (name, length) = said[0].split_once(' ').unwrap();
    assert_eq!(name, "cuda-worker");
    assert!(length.parse::<usize>().unwrap() > 0);
}

/// The exception is the run's own entries: not the server's (its parent's)
/// `environ`, `maps` or thread names, not its own `/proc/self/environ` (only
/// `task` and `maps` were named), not init's.
#[tokio::test]
async fn an_nvidia_run_may_not_read_another_process_s_proc() {
    let server = std::process::id();
    let environ = format!("/proc/{server}/environ");
    let maps = format!("/proc/{server}/maps");
    let comm = format!("/proc/{server}/task/{server}/comm");
    let others = [
        environ.as_str(),
        maps.as_str(),
        comm.as_str(),
        "/proc/self/environ",
        "/proc/self/status",
        "/proc/1/maps",
        "/proc/1/environ",
        "/proc/cpuinfo",
    ];
    // Unconfined, the server's entries open: they are there to refuse.
    let mut unconfined = Tool::unconfined("bash")
        .command(&Jail::new(Limits::PASS))
        .unwrap();
    unconfined
        .args(["-c", &open_each("<", &others[..5])])
        .stdin(Stdio::null());
    let output = unconfined.output().await.unwrap();
    assert!(
        said(&output).iter().all(|line| line.starts_with("opened")),
        "{:?}",
        said(&output)
    );

    let jail = Jail::new(Limits::PASS).gpu(GpuUse::Nvidia);
    let output = confined_bash(&jail, &open_each("<", &others)).await;
    let refused: Vec<String> = others.iter().map(|p| format!("refused {p}")).collect();
    assert_eq!(said(&output), refused);
    let output = confined_bash(&jail, &open_each(">", &[comm.as_str()])).await;
    assert_eq!(said(&output), [format!("refused {comm}")]);
}

/// A VA-API run, or a run on no GPU, has no /proc at all: not even its
/// own threads' names or maps.
#[tokio::test]
async fn a_run_not_on_nvidia_has_no_proc() {
    let own = ["/proc/self/task/$$/comm", "/proc/self/maps"];
    for gpu in [GpuUse::None, GpuUse::Vaapi] {
        let jail = Jail::new(Limits::PASS).gpu(gpu);
        let output = confined_bash(&jail, &open_each("<", &own)).await;
        let lines = said(&output);
        assert_eq!(lines.len(), 2, "{gpu:?}: {lines:?}");
        assert!(
            lines
                .iter()
                .all(|line| line.starts_with("refused /proc/self/")),
            "{gpu:?}: {lines:?}"
        );
        let output = confined_bash(&jail, &open_each(">", &own[..1])).await;
        let lines = said(&output);
        assert!(
            lines[0].starts_with("refused /proc/self/task/"),
            "{gpu:?}: {lines:?}"
        );
    }
}

/// What the server checks at start: a confined ffprobe is refused the
/// state directory and loopback; an unconfined one is not.
#[tokio::test]
async fn the_start_check_tells_a_confined_ffprobe_from_one_that_is_not() {
    let (_tmp, root) = scratch("check").await;
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let shown = Tool::new("ffprobe", hiding(&state))
        .check_confined(&state)
        .await
        .unwrap();
    assert!(shown.contains("refused reading"), "{shown}");
    assert!(
        Tool::unconfined("ffprobe")
            .check_confined(&state)
            .await
            .is_err()
    );
}

/// Confined or not, a run gets only the few variables programs need from
/// the server's environment (cargo's are not among them), and its home is
/// its own directory.
#[tokio::test]
async fn a_run_sees_none_of_the_server_s_environment() {
    let (_tmp, root) = scratch("env").await;
    for tool in [Tool::unconfined("bash"), confined("bash")] {
        let mut command = tool
            .command(&Jail::new(Limits::PASS).writes(&root))
            .unwrap();
        command
            .args(["-c", "echo \"${CARGO_MANIFEST_DIR-none} $HOME\""])
            .stdin(Stdio::null());
        let output = command.output().await.unwrap();
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            format!("none {}", root.display())
        );
    }
}

/// Confined or not, a run is held to its limits, and cannot raise them.
#[tokio::test]
async fn a_run_is_held_to_its_limits() {
    let limits = Limits {
        memory: Memory::fixed(512 << 20),
        cpu_secs: 60,
        file_size: 1 << 20,
    };
    for tool in [Tool::unconfined("bash"), confined("bash")] {
        let mut command = tool.command(&Jail::new(limits)).unwrap();
        // bash counts -d in KiB and -f in 1 KiB blocks.
        command
            .args(["-c", "ulimit -Hd; ulimit -Ht; ulimit -Hf; ulimit -Hc"])
            .stdin(Stdio::null());
        let output = command.output().await.unwrap();
        assert_eq!(
            String::from_utf8_lossy(&output.stdout)
                .split_whitespace()
                .collect::<Vec<_>>(),
            ["524288", "60", "1024", "0"]
        );
    }
}
