//! The media tools' sandbox (PX-11 finding 34): what a confined ffmpeg,
//! ffprobe or shell can and cannot reach. Every test here needs
//! bubblewrap, which the dev shell has; the other suites' real ffmpeg,
//! ffprobe and tesseract run confined through [`confined`] too, which shows
//! normal probes, encodes and OCR still work.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use spoolframe_media::Prober;
use spoolframe_media::sandbox::{Jail, Limits, Memory, Mode, Sandbox, Tool};

/// The sandbox the suites' real tools run in, with nothing hidden.
fn shared() -> Sandbox {
    static SANDBOX: OnceLock<Sandbox> = OnceLock::new();
    SANDBOX
        .get_or_init(|| {
            Sandbox::new(Mode::Required, Path::new("bwrap"), Vec::new(), Vec::new())
                .expect("the dev shell has bubblewrap")
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
        Path::new("bwrap"),
        vec![state.to_path_buf()],
        Vec::new(),
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

/// ffmpeg reads URLs too: a confined one has no network to reach a
/// listener on loopback with.
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

/// These programs only start threads: a confined shell cannot start a
/// process. (ffmpeg's threads are what the other suites show working.)
#[tokio::test]
async fn a_confined_program_starts_no_process() {
    let mut command = confined("bash").command(&Jail::new(Limits::PASS)).unwrap();
    command.args(["-c", "true & wait $!"]).stdin(Stdio::null());
    let output = command.output().await.unwrap();
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
