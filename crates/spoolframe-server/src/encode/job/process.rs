//! ffmpeg as a process (P2-03): launched in its jail, signalled, its
//! files removed, its progress lines read, and the GPU blamed or not
//! for how it ended.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{BufReader, Lines};
use tokio::process::ChildStderr;
use tokio::sync::watch;

use spoolframe_media::sandbox::{Jail, Limits, Tool};

/// Films whose GPU decode failed, for as long as the server runs: few,
/// and a restart (a new driver, say) tries again.
#[derive(Debug, Default)]
pub(super) struct DecodeFailures(parking_lot::Mutex<HashSet<PathBuf>>);

impl DecodeFailures {
    /// Remember `file`: true the first time.
    pub(super) fn remember(&self, file: &Path) -> bool {
        self.0.lock().insert(file.to_owned())
    }

    pub(super) fn contains(&self, file: &Path) -> bool {
        self.0.lock().contains(file)
    }
}

/// ffmpeg with `args`, in `jail`, its output piped to the supervisor.
pub(crate) fn launch(
    ffmpeg: &Tool,
    jail: &Jail,
    args: &[String],
) -> std::io::Result<tokio::process::Child> {
    ffmpeg
        .command(jail)?
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
}

/// The jail a job's ffmpeg runs in: it writes in `writes`, reads the
/// server's own `reads` and the `inputs` someone else may have put
/// there, and no file of the job's is larger than all it may write,
/// `scratch_limit` bytes.
pub(crate) fn jail(
    scratch_limit: u64,
    writes: &Path,
    reads: &[PathBuf],
    inputs: &[PathBuf],
) -> Jail {
    let mut jail = Jail::new(Limits::encode(scratch_limit));
    for dir in reads {
        jail = jail.reads(dir.clone());
    }
    jail = jail.writes(writes);
    for input in inputs {
        jail = jail.input(input.clone());
    }
    jail
}

/// A run's files removed; one already gone is fine.
pub(super) fn remove_run_dir(dir: &Path) {
    if let Err(e) = std::fs::remove_dir_all(dir)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(dir = %dir.display(), error = %e, "cannot remove an encode's files");
    }
}

/// `sig` to the run's process group, which `pid` (ffmpeg) leads.
pub(crate) fn signal(pid: u32, sig: libc::c_int) {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return;
    };
    if pid > 0 {
        // SAFETY: kill(2) on our own child's process group; an error (it
        // has exited) is harmless.
        unsafe {
            libc::kill(-pid, sig);
        }
    }
}

/// How many of ffmpeg's last stderr lines a failure keeps.
pub(crate) const STDERR_LINES: usize = 5;

/// `line` kept as ffmpeg's last, with at most [`STDERR_LINES`] before it.
pub(crate) fn keep(last: &mut VecDeque<String>, line: String) {
    if last.len() == STDERR_LINES {
        last.pop_front();
    }
    last.push_back(line);
}

/// The rest of what an ended ffmpeg said on `err`, briefly: kept as
/// [`keep`] does, for at most a second.
pub(crate) async fn drain_stderr(
    err: &mut Lines<BufReader<ChildStderr>>,
    last: &mut VecDeque<String>,
) {
    let rest = async {
        while let Ok(Some(line)) = err.next_line().await {
            keep(last, line);
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(1), rest).await;
}

/// Until `stop` says to stop (or its sender is gone). The guard
/// `wait_for` gives back is not `Send`: it is dropped here, not held
/// across the caller's awaits.
pub(crate) async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|stop| *stop).await;
}

/// `link` made to lead to `target` (made absolute).
pub(crate) fn link(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(std::path::absolute(target)?, link)
}

/// `link` made to lead to `target`, whatever it led to before.
pub(crate) fn relink(target: &Path, at: &Path) -> std::io::Result<()> {
    remove_file(at);
    link(target, at)
}

/// `path` removed; one already gone is fine.
pub(crate) fn remove_file(path: &Path) {
    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(path = %path.display(), error = %e, "cannot remove a file");
    }
}

/// The encoded media time in a line of ffmpeg's `-progress`, µs: its
/// `out_time_us` (`out_time_ms` is µs too). `None` for any other line.
pub(crate) fn out_time_us(line: &str) -> Option<u64> {
    line.strip_prefix("out_time_us=")?.trim().parse().ok()
}

/// Whether a run that exited with `status` may have failed for decoding
/// on the GPU: it exited non-zero. One ended by a signal (the kernel's OOM
/// killer, an operator, or the server stopping it) says nothing about the
/// GPU, nor does one that could not be waited for.
pub(crate) fn exit_blames_gpu(status: &std::io::Result<std::process::ExitStatus>) -> bool {
    use std::os::unix::process::ExitStatusExt;
    status
        .as_ref()
        .is_ok_and(|status| !status.success() && status.signal().is_none())
}
