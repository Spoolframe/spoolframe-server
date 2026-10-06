//! One ffmpeg run (P2-03): spawned for a playback, paused when it is far
//! ahead of its reader, killed and cleaned up when dropped.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::JobLimits;
use super::args::PLAYLIST;
use crate::metrics::Counter;

/// How often a job looks at its output.
const POLL: Duration = Duration::from_millis(200);
/// How long an encode that is not held back may make no progress before
/// it is ended as stalled. Long enough for an input on a disk that has to
/// spin up or a network mount that has to answer, and for a slow software
/// encoder to fill its lookahead before its first frame comes out; a
/// viewer has given up long before.
pub const STALLED_AFTER: Duration = Duration::from_secs(120);
/// How many of ffmpeg's last stderr lines a failure reports.
const STDERR_LINES: usize = 20;
/// How many of ffmpeg's last stderr lines a job keeps, for playback debug
/// (P3-15) to show while it runs and after it ends.
pub const STDERR_KEPT: usize = 50;

/// What a job writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Written {
    Progressive {
        file: PathBuf,
    },
    /// `init.mp4` and `seg<n>.m4s` in the job's directory.
    Hls,
}

/// How far a job has got.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Progress {
    /// Bytes written: the file, or every segment.
    pub bytes: u64,
    /// The media time encoded, as ffmpeg reports it.
    pub encoded_ms: u64,
    /// For HLS, the highest segment ffmpeg has finished: the last one its
    /// own playlist lists.
    pub last_segment: Option<u32>,
    /// `None` while running; then how it ended.
    pub done: Option<Result<(), String>>,
}

#[derive(Debug, thiserror::Error)]
pub enum JobError {
    /// Over `max_jobs`.
    #[error("the server is converting as many files as it may")]
    Busy,
    #[error("cannot prepare the encode: {0}")]
    Prepare(String),
}

/// One of the `max_jobs` places. A job that replaces another (a seek, a
/// subtitle change) shares the place of the one it replaces, so nobody
/// else can take it between the two; it is free once both have let go of
/// it.
#[derive(Debug, Clone)]
pub struct Slot {
    _place: Arc<Place>,
}

/// How many of the places are taken, and how many there are. A counter
/// rather than a semaphore: `max_jobs` is a live setting (P3-10), and a
/// lower limit takes effect as jobs end, without stopping any.
#[derive(Debug, Default)]
struct Places {
    taken: usize,
    max: usize,
}

/// A taken place, given back when dropped.
#[derive(Debug)]
struct Place(Arc<std::sync::Mutex<Places>>);

impl Place {
    fn take(places: &Arc<std::sync::Mutex<Places>>) -> Option<Self> {
        let mut now = places.lock().expect("never held across a panic");
        if now.taken >= now.max {
            return None;
        }
        now.taken += 1;
        Some(Self(places.clone()))
    }
}

impl Drop for Place {
    fn drop(&mut self) {
        self.0.lock().expect("never held across a panic").taken -= 1;
    }
}

/// The directory under `transcode.scratch_dir` that jobs write in. The
/// server makes it, and it is the only thing there the server removes.
pub const JOBS_DIR: &str = "spoolframe-jobs";

/// The jobs of this server: the limit, and where they write.
#[derive(Debug)]
pub struct Jobs {
    /// Live (P3-10) but for the scratch directory: a job reads them when
    /// it starts.
    limits: std::sync::RwLock<JobLimits>,
    /// [`JOBS_DIR`] in the scratch directory.
    dir: PathBuf,
    ffmpeg: PathBuf,
    places: Arc<std::sync::Mutex<Places>>,
    /// Held by the job that looks at a full scratch directory and makes
    /// room in it, so the next job to look sees what the first one freed.
    scratch_check: Arc<tokio::sync::Mutex<()>>,
    /// How many jobs that encode a stream run now: frame previews made
    /// ahead wait while any does (P3-13).
    encoding: Encoding,
    /// Every job started and not yet dropped, to count (P3-14).
    live: std::sync::Mutex<Vec<std::sync::Weak<EncodeJob>>>,
    counters: Arc<JobCounters>,
}

/// What the jobs did since the server started (P3-14).
#[derive(Debug, Default)]
struct JobCounters {
    /// Starts refused for want of a free place: there is no queue.
    refused: Counter,
    /// ffmpeg runs ended for making no progress.
    stalled: Counter,
    /// Media time encoded, in ms, by every job.
    output_ms: Counter,
}

/// The jobs now (P3-14).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JobCounts {
    /// Jobs whose ffmpeg runs (paused or not).
    pub running: u64,
    /// Of those, the ones held back for their reader.
    pub paused: u64,
    pub refused: u64,
    pub stalled: u64,
    /// Seconds of media encoded since the server started.
    pub output_seconds: f64,
}

/// How many encodes (a stream decoded and encoded again, not only copied
/// into another container) run now, and a way to wait for that to change.
#[derive(Debug, Clone)]
pub struct Encoding(Arc<watch::Sender<usize>>);

impl Default for Encoding {
    fn default() -> Self {
        Self(Arc::new(watch::Sender::new(0)))
    }
}

impl Encoding {
    /// One more encode runs until the guard is dropped.
    pub fn begin(&self) -> EncodingGuard {
        self.0.send_modify(|n| *n += 1);
        EncodingGuard(self.0.clone())
    }

    /// How many run now.
    pub fn running(&self) -> usize {
        *self.0.borrow()
    }

    /// The count, to wait on.
    pub fn watch(&self) -> watch::Receiver<usize> {
        self.0.subscribe()
    }
}

/// An encode counted in [`Encoding`] while held.
#[derive(Debug)]
pub struct EncodingGuard(Arc<watch::Sender<usize>>);

impl Drop for EncodingGuard {
    fn drop(&mut self) {
        self.0.send_modify(|n| *n -= 1);
    }
}

impl Jobs {
    /// Jobs with `limits`, run with `ffmpeg`. The jobs' own directory in
    /// the scratch directory is emptied: whatever is there is a previous
    /// run's. Nothing else in the scratch directory is touched: it may be
    /// a directory other things are kept in.
    pub fn new(limits: JobLimits, ffmpeg: PathBuf) -> std::io::Result<Self> {
        let dir = limits.scratch_dir.join(JOBS_DIR);
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => {
                tracing::info!(dir = %dir.display(), "removed leftover encodes")
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            places: Arc::new(std::sync::Mutex::new(Places {
                taken: 0,
                max: limits.max_jobs,
            })),
            scratch_check: Arc::default(),
            encoding: Encoding::default(),
            live: std::sync::Mutex::default(),
            counters: Arc::default(),
            limits: std::sync::RwLock::new(limits),
            dir,
            ffmpeg,
        })
    }

    /// The limits now.
    pub fn limits(&self) -> JobLimits {
        self.limits
            .read()
            .expect("never held across a panic")
            .clone()
    }

    /// Run new jobs under `limits` from now on. Jobs running go on as they
    /// started; with fewer places than are taken, none is free until
    /// enough jobs end. The scratch directory stays the one the jobs
    /// started in: it changes only with a restart.
    pub fn set_limits(&self, limits: JobLimits) {
        self.places.lock().expect("never held across a panic").max = limits.max_jobs;
        let mut now = self.limits.write().expect("never held across a panic");
        *now = JobLimits {
            scratch_dir: now.scratch_dir.clone(),
            ..limits
        };
    }

    /// Where jobs write: a directory per playback handle in it.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// How many jobs that encode run now.
    pub fn encoding(&self) -> Encoding {
        self.encoding.clone()
    }

    /// The jobs now, and what they did (P3-14).
    pub fn counts(&self) -> JobCounts {
        let jobs: Vec<Arc<EncodeJob>> = {
            let mut live = self.live.lock().expect("never held across a panic");
            live.retain(|job| job.strong_count() > 0);
            live.iter().filter_map(std::sync::Weak::upgrade).collect()
        };
        // Dropped outside the lock: the last hold of a job removes its files.
        let running: Vec<_> = jobs
            .iter()
            .filter(|job| job.progress.borrow().done.is_none() && !job.task.is_finished())
            .collect();
        JobCounts {
            running: running.len() as u64,
            paused: running.iter().filter(|job| job.paused()).count() as u64,
            refused: self.counters.refused.get(),
            stalled: self.counters.stalled.get(),
            output_seconds: self.counters.output_ms.get() as f64 / 1000.0,
        }
    }

    /// Run ffmpeg with `args` into `dir` (created), which the job owns and
    /// removes when dropped. It runs in `slot`, the place of the job it
    /// replaces, or takes a free one. One that `encodes` counts in
    /// [`Jobs::encoding`] while ffmpeg runs.
    #[expect(
        clippy::too_many_arguments,
        reason = "one caller, which names each part"
    )]
    pub fn start(
        &self,
        dir: PathBuf,
        args: Vec<String>,
        written: Written,
        encodes: bool,
        epoch: u32,
        first_segment: u32,
        start_ms: u64,
        slot: Option<Slot>,
    ) -> Result<Arc<EncodeJob>, JobError> {
        let slot = match slot {
            Some(slot) => slot,
            None => match Place::take(&self.places) {
                Some(place) => Slot {
                    _place: Arc::new(place),
                },
                None => {
                    self.counters.refused.inc();
                    return Err(JobError::Busy);
                }
            },
        };
        let slot = Arc::new(std::sync::Mutex::new(Some(slot)));
        std::fs::create_dir_all(&dir).map_err(|e| JobError::Prepare(e.to_string()))?;
        let mut child = tokio::process::Command::new(&self.ffmpeg)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| JobError::Prepare(format!("cannot run {}: {e}", self.ffmpeg.display())))?;
        let pid = child.id().unwrap_or(0);
        let limits = self.limits();
        let (progress_tx, progress) = watch::channel(Progress::default());
        let progress_tx = Arc::new(progress_tx);
        let shared = Arc::new(Shared {
            furthest_bytes: AtomicU64::new(0),
            reader_ms: AtomicU64::new(0),
            reader_segment: AtomicU32::new(0),
            first_kept: AtomicU32::new(first_segment),
            paused: AtomicBool::new(false),
            stderr: std::sync::Mutex::default(),
        });
        let supervisor = Supervisor {
            pid,
            start_ms,
            dir: dir.clone(),
            written: written.clone(),
            scratch: self.dir.clone(),
            scratch_limit: limits.scratch_limit_bytes,
            scratch_check: self.scratch_check.clone(),
            read_ahead: limits.read_ahead,
            stalled_after: limits.stalled_after,
            shared: shared.clone(),
            slot: slot.clone(),
            progress: progress_tx.clone(),
            counters: self.counters.clone(),
            _encoding: encodes.then(|| self.encoding.begin()),
        };
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let task = tokio::spawn(supervisor.run(child, stdout, stderr));
        tracing::debug!(pid, dir = %dir.display(), epoch, "encode started");
        let mut command = vec![self.ffmpeg.display().to_string()];
        command.extend(args);
        let job = Arc::new(EncodeJob {
            epoch,
            first_segment,
            pid,
            dir,
            written,
            start_ms,
            command,
            progress,
            // Only the supervisor holds it otherwise: once it is gone, a
            // reader waiting for more hears so at once.
            sender: Arc::downgrade(&progress_tx),
            shared,
            task,
            slot,
        });
        let mut live = self.live.lock().expect("never held across a panic");
        live.retain(|job| job.strong_count() > 0);
        live.push(Arc::downgrade(&job));
        Ok(job)
    }
}

/// What the reader side tells the supervisor.
#[derive(Debug)]
struct Shared {
    furthest_bytes: AtomicU64,
    /// For HLS: the media time of the segment asked for last.
    reader_ms: AtomicU64,
    /// For HLS: the number of the segment asked for last. Not the furthest:
    /// a reader that seeks back to a segment still on disk reads on from
    /// there.
    reader_segment: AtomicU32,
    /// For HLS: the first segment still on disk. Those before it were
    /// never written by this run, or removed to make room.
    first_kept: AtomicU32,
    paused: AtomicBool,
    /// ffmpeg's last [`STDERR_KEPT`] lines of stderr.
    stderr: std::sync::Mutex<VecDeque<String>>,
}

impl Shared {
    fn stderr_line(&self, line: String) {
        let mut kept = self.stderr.lock().expect("never poisoned");
        if kept.len() == STDERR_KEPT {
            kept.pop_front();
        }
        kept.push_back(line);
    }

    /// The last [`STDERR_LINES`], for a failure's report.
    fn stderr_tail(&self) -> Vec<String> {
        let kept = self.stderr.lock().expect("never poisoned");
        kept.iter()
            .skip(kept.len().saturating_sub(STDERR_LINES))
            .cloned()
            .collect()
    }
}

/// A running (or finished) ffmpeg.
#[derive(Debug)]
pub struct EncodeJob {
    pub epoch: u32,
    /// For HLS, the number of the first segment this run writes.
    pub first_segment: u32,
    pid: u32,
    dir: PathBuf,
    written: Written,
    /// Where the run started in the film, ms.
    start_ms: u64,
    /// The program and its arguments, as run.
    command: Vec<String>,
    progress: watch::Receiver<Progress>,
    /// The supervisor's, while it runs: what [`EncodeJob::end`] says the
    /// job ended with.
    sender: std::sync::Weak<watch::Sender<Progress>>,
    shared: Arc<Shared>,
    task: JoinHandle<()>,
    /// Held while ffmpeg may run; released by [`EncodeJob::stop`], or by
    /// the job itself when it ends a stalled ffmpeg.
    slot: Arc<std::sync::Mutex<Option<Slot>>>,
}

impl EncodeJob {
    /// Kill ffmpeg now and let go of its slot, though readers may still
    /// hold the job: one replaced at a seek must not encode on.
    pub fn stop(&self) {
        self.task.abort();
        if self.progress.borrow().done.is_none() {
            if self.shared.paused.load(Ordering::Relaxed) {
                signal(self.pid, libc::SIGCONT);
            }
            signal(self.pid, libc::SIGKILL);
        }
        self.slot.lock().expect("never poisoned").take();
    }

    /// End the encode where it is (P3-15's End conversion): ffmpeg is
    /// killed and its slot let go of, and what it wrote is all there is,
    /// so a reader gets that and then the end, rather than waiting for
    /// more. Its files stay until the job is dropped. One that has ended
    /// already keeps how it ended.
    pub fn end(&self) {
        // Held across the abort, so the channel is still open to say it.
        let sender = self.sender.upgrade();
        self.stop();
        if let Some(sender) = sender {
            let (bytes, last_segment) = measure(&self.written, &self.dir);
            sender.send_if_modified(|p| {
                if p.done.is_some() {
                    return false;
                }
                p.bytes = p.bytes.max(bytes);
                p.last_segment = p.last_segment.max(last_segment);
                p.done = Some(Ok(()));
                true
            });
        }
        self.shared.paused.store(false, Ordering::Relaxed);
    }

    /// Where its reader is in the film, ms, as the throttle reckons it.
    pub fn reader_ms(&self) -> u64 {
        let p = self.progress.borrow();
        reader_ms(
            &self.written,
            &self.shared,
            self.start_ms,
            p.encoded_ms,
            p.bytes,
            p.last_segment,
        )
    }

    /// The program and its arguments, as run (paths and all).
    pub fn command(&self) -> Vec<String> {
        self.command.clone()
    }

    /// ffmpeg's last lines of stderr, oldest first, at most
    /// [`STDERR_KEPT`].
    pub fn stderr(&self) -> Vec<String> {
        self.shared
            .stderr
            .lock()
            .expect("never poisoned")
            .iter()
            .cloned()
            .collect()
    }

    /// The job's slot, for the job that replaces it; none once stopped.
    pub fn slot(&self) -> Option<Slot> {
        self.slot.lock().expect("never poisoned").clone()
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn progress(&self) -> watch::Receiver<Progress> {
        self.progress.clone()
    }

    /// Whether the job is held back for its reader to catch up.
    pub fn paused(&self) -> bool {
        self.shared.paused.load(Ordering::Relaxed)
    }

    /// A reader got as far as `bytes` into the progressive output.
    pub fn read_to(&self, bytes: u64) {
        self.shared
            .furthest_bytes
            .fetch_max(bytes, Ordering::Relaxed);
    }

    /// A reader asked for HLS segment `n`, which ends at `ms` (media
    /// time): that is where the reader is now.
    pub fn reader_at(&self, n: u32, ms: u64) {
        self.shared.reader_segment.store(n, Ordering::Relaxed);
        self.shared.reader_ms.store(ms, Ordering::Relaxed);
    }

    /// A reader waits for HLS segment `n`, which ends at `ms`, to be
    /// finished, without reading it (the init segment is ready with the
    /// run's first): the reader is there if it was not further already.
    pub fn reader_at_least(&self, n: u32, ms: u64) {
        self.shared.reader_segment.fetch_max(n, Ordering::Relaxed);
        self.shared.reader_ms.fetch_max(ms, Ordering::Relaxed);
    }

    /// For HLS, the first segment this run still has: the one it started
    /// at, or a later one once a full scratch directory made it remove
    /// those behind its reader.
    pub fn first_kept(&self) -> u32 {
        self.shared.first_kept.load(Ordering::Relaxed)
    }
}

impl Drop for EncodeJob {
    fn drop(&mut self) {
        // The task owns the child, which is killed when dropped; the signal
        // makes it now rather than when the runtime gets to the task.
        self.stop();
        if let Err(e) = std::fs::remove_dir_all(&self.dir)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(dir = %self.dir.display(), error = %e, "cannot remove an encode's files");
        }
        tracing::debug!(pid = self.pid, "encode ended");
    }
}

fn signal(pid: u32, sig: libc::c_int) {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return;
    };
    if pid > 0 {
        // SAFETY: kill(2) on our own child's pid; an error (it has exited)
        // is harmless.
        unsafe {
            libc::kill(pid, sig);
        }
    }
}

struct Supervisor {
    pid: u32,
    /// Where the run started in the film, ms.
    start_ms: u64,
    dir: PathBuf,
    written: Written,
    /// The jobs' directory: what all jobs have written.
    scratch: PathBuf,
    scratch_limit: u64,
    scratch_check: Arc<tokio::sync::Mutex<()>>,
    read_ahead: Duration,
    stalled_after: Duration,
    shared: Arc<Shared>,
    /// The job's slot, let go of when a stalled ffmpeg is ended.
    slot: Arc<std::sync::Mutex<Option<Slot>>>,
    progress: Arc<watch::Sender<Progress>>,
    counters: Arc<JobCounters>,
    /// Counted as encoding for as long as the supervisor runs: until
    /// ffmpeg exits, is ended as stalled, or the job is stopped (which
    /// aborts the supervisor).
    _encoding: Option<EncodingGuard>,
}

impl Supervisor {
    async fn run(
        self,
        mut child: tokio::process::Child,
        stdout: tokio::process::ChildStdout,
        stderr: tokio::process::ChildStderr,
    ) {
        let mut out = BufReader::new(stdout).lines();
        let mut err = BufReader::new(stderr).lines();
        let mut encoded_ms = 0u64;
        // What the job had written when it last looked.
        let mut seen = 0u64;
        // What it had written and encoded when it last got further, and
        // when that was.
        let mut reached = (0u64, 0u64);
        let mut got_further = tokio::time::Instant::now();
        // With `-copyts` a run started at a seek reports the film's own
        // time, from where it started.
        let base_ms = self.start_ms;
        // The furthest media time counted as output (P3-14): from where the
        // run started, never twice.
        let mut counted_ms = base_ms;
        let mut tick = tokio::time::interval(POLL);
        let mut out_open = true;
        let mut err_open = true;
        let status = loop {
            tokio::select! {
                line = out.next_line(), if out_open => match line {
                    Ok(Some(line)) => {
                        // `-progress` writes key=value lines; out_time_us is
                        // the encoded media time (out_time_ms is µs too).
                        if let Some(us) = line.strip_prefix("out_time_us=")
                            && let Ok(us) = us.trim().parse::<u64>()
                        {
                            encoded_ms = us / 1000;
                            if encoded_ms > counted_ms {
                                self.counters.output_ms.add(encoded_ms - counted_ms);
                                counted_ms = encoded_ms;
                            }
                        }
                    }
                    _ => out_open = false,
                },
                line = err.next_line(), if err_open => match line {
                    Ok(Some(line)) => self.shared.stderr_line(line),
                    _ => err_open = false,
                },
                status = child.wait() => break status,
                _ = tick.tick() => {
                    let (bytes, last_segment) = self.measure();
                    self.throttle(encoded_ms, base_ms, bytes, last_segment);
                    self.progress.send_modify(|p| {
                        p.bytes = bytes;
                        p.encoded_ms = encoded_ms;
                        p.last_segment = last_segment;
                    });
                    // ffmpeg can wait for ever with nothing wrong that it
                    // knows of. One held back for its reader is not
                    // waiting: its time starts again when it goes on.
                    if reached != (bytes, encoded_ms) || self.shared.paused.load(Ordering::Relaxed) {
                        reached = (bytes, encoded_ms);
                        got_further = tokio::time::Instant::now();
                    } else if got_further.elapsed() >= self.stalled_after {
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        tracing::warn!(
                            pid = self.pid,
                            after = ?self.stalled_after,
                            bytes,
                            encoded_ms,
                            stderr = ?self.shared.stderr_tail(),
                            "an encode stalled"
                        );
                        self.slot.lock().expect("never poisoned").take();
                        self.counters.stalled.inc();
                        self.finish(Err(format!(
                            "the conversion stalled: ffmpeg made no progress for {:?}",
                            self.stalled_after
                        )));
                        return;
                    }
                    // Only a job that wrote since it last looked can be the
                    // one that fills the scratch directory: a paused one
                    // is left alone, whoever fills it.
                    if bytes > seen {
                        let _one_job_at_a_time = self.scratch_check.lock().await;
                        // A job just held back for its reader writes no
                        // more: what it cannot free now it can once the
                        // reader has moved on and it writes again.
                        if !self.make_room() && !self.shared.paused.load(Ordering::Relaxed) {
                            let _ = child.start_kill();
                            let _ = child.wait().await;
                            // Of no use now, and in the way of the other
                            // jobs for as long as the playback lives.
                            if let Err(e) = std::fs::remove_dir_all(&self.dir) {
                                tracing::warn!(dir = %self.dir.display(), error = %e, "cannot remove an encode's files");
                            }
                            self.finish(Err("the server's scratch space for conversions is full".into()));
                            return;
                        }
                        seen = self.measure().0;
                    }
                }
            }
        };
        // Drain what is left of stderr for the report.
        while let Ok(Some(line)) = err.next_line().await {
            self.shared.stderr_line(line);
        }
        let (bytes, last_segment) = self.measure();
        self.progress.send_modify(|p| {
            p.bytes = bytes;
            p.last_segment = last_segment;
            p.encoded_ms = p.encoded_ms.max(encoded_ms);
        });
        match status {
            Ok(status) if status.success() => self.finish(Ok(())),
            Ok(status) => {
                let tail = self.shared.stderr_tail();
                let reason = tail
                    .iter()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .cloned()
                    .unwrap_or_else(|| format!("ffmpeg exited with {status}"));
                tracing::warn!(pid = self.pid, %status, stderr = ?tail, "an encode failed");
                self.finish(Err(reason));
            }
            Err(e) => self.finish(Err(format!("cannot wait for ffmpeg: {e}"))),
        }
    }

    fn finish(&self, done: Result<(), String>) {
        self.shared.paused.store(false, Ordering::Relaxed);
        self.progress.send_modify(|p| p.done = Some(done));
    }

    /// Whether the scratch directory is within its limit. When it is not,
    /// an HLS job first removes its own segments behind its reader, the
    /// oldest first, until it is (P2-03). The segment before the one
    /// asked for last stays: a reader may still be fetching it.
    fn make_room(&self) -> bool {
        let mut total = dir_size(&self.scratch);
        if total > self.scratch_limit && self.written == Written::Hls {
            let behind = self
                .shared
                .reader_segment
                .load(Ordering::Relaxed)
                .saturating_sub(1);
            let mut n = self.shared.first_kept.load(Ordering::Relaxed);
            while total > self.scratch_limit && n < behind {
                // Said first, so no reader takes the missing file for an
                // empty segment.
                self.shared.first_kept.store(n + 1, Ordering::Relaxed);
                let file = self.dir.join(format!("seg{n}.m4s"));
                let size = std::fs::metadata(&file).map_or(0, |m| m.len());
                if std::fs::remove_file(&file).is_ok() {
                    total = total.saturating_sub(size);
                }
                n += 1;
            }
            tracing::debug!(
                pid = self.pid,
                first_kept = n,
                "segments behind the reader removed for room"
            );
        }
        total <= self.scratch_limit
    }

    /// Bytes written, and for HLS the highest segment on disk.
    fn measure(&self) -> (u64, Option<u32>) {
        measure(&self.written, &self.dir)
    }

    /// Pause ffmpeg once it is `read_ahead` past its reader; resume it at
    /// half that. Never while the reader has nothing it can read: a held
    /// encode goes on only when its reader moves on, and a reader that
    /// waits for the encode cannot.
    fn throttle(&self, encoded_ms: u64, base_ms: u64, bytes: u64, last_segment: Option<u32>) {
        let reader_ms = reader_ms(
            &self.written,
            &self.shared,
            base_ms,
            encoded_ms,
            bytes,
            last_segment,
        );
        let ahead = Duration::from_millis(encoded_ms.saturating_sub(reader_ms));
        let paused = self.shared.paused.load(Ordering::Relaxed);
        if !paused && ahead > self.read_ahead {
            signal(self.pid, libc::SIGSTOP);
            self.shared.paused.store(true, Ordering::Relaxed);
            tracing::debug!(pid = self.pid, ?ahead, "encode paused for its reader");
        } else if paused && ahead <= self.read_ahead / 2 {
            signal(self.pid, libc::SIGCONT);
            self.shared.paused.store(false, Ordering::Relaxed);
            tracing::debug!(pid = self.pid, ?ahead, "encode resumed");
        }
    }
}

/// Where a job's reader is in the film, ms: what the throttle holds the
/// encode back against, and what playback debug (P3-15) shows as read.
fn reader_ms(
    written: &Written,
    shared: &Shared,
    base_ms: u64,
    encoded_ms: u64,
    bytes: u64,
    last_segment: Option<u32>,
) -> u64 {
    match written {
        Written::Progressive { .. } => {
            let read = shared.furthest_bytes.load(Ordering::Relaxed);
            if read >= bytes {
                // It has all that is written, which is nothing until ffmpeg
                // has its first cluster whole: it is where the encode is.
                encoded_ms
            } else {
                // A byte offset as a share of what this run encoded: close
                // enough for a stream of roughly even bitrate.
                let run = encoded_ms.saturating_sub(base_ms);
                base_ms + (u128::from(run) * u128::from(read) / u128::from(bytes)) as u64
            }
        }
        Written::Hls => {
            let asked = shared.reader_segment.load(Ordering::Relaxed);
            if asked > 0 && last_segment.is_none_or(|last| last < asked) {
                // It waits for a segment ffmpeg has not finished, whatever
                // time ffmpeg reports.
                encoded_ms
            } else {
                shared.reader_ms.load(Ordering::Relaxed).max(base_ms)
            }
        }
    }
}

/// Bytes `written` in `dir`, and for HLS the highest segment finished.
fn measure(written: &Written, dir: &Path) -> (u64, Option<u32>) {
    match written {
        Written::Progressive { file } => (std::fs::metadata(file).map_or(0, |m| m.len()), None),
        Written::Hls => {
            let bytes = std::fs::read_dir(dir).map_or(0, |entries| {
                entries
                    .flatten()
                    .map(|e| e.metadata().map_or(0, |m| m.len()))
                    .sum()
            });
            // Not the segment files: in fMP4 mode ffmpeg writes a segment's
            // file only as it closes it, so a file that exists may still be
            // half written, and the one being encoded does not exist yet.
            // ffmpeg's playlist is replaced (written aside, then renamed)
            // right after each segment is closed.
            let last = std::fs::read_to_string(dir.join(PLAYLIST))
                .ok()
                .and_then(|text| last_listed(&text));
            (bytes, last)
        }
    }
}

/// The bytes under `dir`, one level of subdirectories deep per handle and
/// epoch: `<jobs>/<handle>/<epoch>/…`.
fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => dir_size(&e.path()),
            Ok(m) if m.is_file() => m.len(),
            _ => 0,
        })
        .sum()
}

/// The highest `seg<n>.m4s` an ffmpeg HLS playlist lists.
fn last_listed(playlist: &str) -> Option<u32> {
    playlist
        .lines()
        .filter_map(|line| line.strip_prefix("seg")?.strip_suffix(".m4s")?.parse().ok())
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lower_limit_takes_effect_as_jobs_end_and_a_higher_one_at_once() {
        let places = Arc::new(std::sync::Mutex::new(Places { taken: 0, max: 2 }));
        let first = Place::take(&places).unwrap();
        let second = Place::take(&places).unwrap();
        assert!(Place::take(&places).is_none());
        places.lock().unwrap().max = 1;
        drop(first);
        // Still one taken, and one is all there is.
        assert!(Place::take(&places).is_none());
        drop(second);
        let third = Place::take(&places).unwrap();
        places.lock().unwrap().max = 3;
        let _fourth = Place::take(&places).unwrap();
        drop(third);
    }

    #[tokio::test]
    async fn new_limits_apply_to_the_next_job_but_not_the_scratch_directory() {
        let tmp = spoolframe_test_dir::TestDir::new("spoolframe-job-limits").await;
        let limits = JobLimits {
            max_jobs: 2,
            scratch_dir: tmp.as_ref().join("scratch"),
            scratch_limit_bytes: 1 << 30,
            read_ahead: Duration::from_secs(60),
            stalled_after: STALLED_AFTER,
        };
        let jobs = Jobs::new(limits.clone(), "ffmpeg".into()).unwrap();
        jobs.set_limits(JobLimits {
            max_jobs: 1,
            scratch_dir: tmp.as_ref().join("elsewhere"),
            read_ahead: Duration::from_secs(5),
            ..limits.clone()
        });
        let now = jobs.limits();
        assert_eq!(now.max_jobs, 1);
        assert_eq!(now.read_ahead, Duration::from_secs(5));
        assert_eq!(now.scratch_dir, limits.scratch_dir);
        let _taken = Place::take(&jobs.places).unwrap();
        assert!(Place::take(&jobs.places).is_none());
    }

    #[test]
    fn a_segment_is_finished_once_ffmpeg_lists_it() {
        let playlist = "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-MAP:URI=\"init.mp4\"\n\
                        #EXTINF:6.000000,\nseg4.m4s\n#EXTINF:6.000000,\nseg5.m4s\n";
        assert_eq!(last_listed(playlist), Some(5));
        assert_eq!(last_listed("#EXTM3U\n#EXT-X-VERSION:7\n"), None);
    }

    /// A directory of the test's own, removed when dropped.
    async fn scratch(test: &str) -> spoolframe_test_dir::TestDir {
        spoolframe_test_dir::TestDir::new(&format!("spoolframe-jobs-{test}")).await
    }

    /// Writes an executable script. The text goes to a plain file first and
    /// `install` makes the script from it, so this process never holds the
    /// script open for writing: a child another test forks at that moment
    /// would inherit the handle, and running the script would then fail with
    /// "Text file busy" (ETXTBSY).
    fn write_script(path: &Path, text: &str) {
        let source = path.with_extension("txt");
        std::fs::write(&source, text).unwrap();
        let status = std::process::Command::new("install")
            .args(["-m", "755"])
            .arg(&source)
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success(), "install {} failed", path.display());
    }

    /// A stand-in for ffmpeg: a shell script.
    fn ffmpeg(dir: &Path, body: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("ffmpeg");
        write_script(&path, &format!("#!/bin/sh\n{body}\n"));
        path
    }

    /// Jobs running `body` as ffmpeg, in a directory kept for the test.
    async fn jobs(
        test: &str,
        body: &str,
        max_jobs: usize,
        read_ahead: Duration,
    ) -> (Jobs, spoolframe_test_dir::TestDir) {
        jobs_stalled_after(test, body, max_jobs, read_ahead, STALLED_AFTER).await
    }

    /// The same, with an encode ended as stalled after `stalled_after`
    /// without progress.
    async fn jobs_stalled_after(
        test: &str,
        body: &str,
        max_jobs: usize,
        read_ahead: Duration,
        stalled_after: Duration,
    ) -> (Jobs, spoolframe_test_dir::TestDir) {
        let tmp = scratch(test).await;
        let root = tmp.as_ref().to_path_buf();
        let bin = ffmpeg(&root.join("bin"), body);
        let limits = JobLimits {
            max_jobs,
            scratch_dir: root.join("scratch"),
            scratch_limit_bytes: 1 << 30,
            read_ahead,
            stalled_after,
        };
        (Jobs::new(limits, bin).unwrap(), tmp)
    }

    async fn done(job: &EncodeJob) -> Result<(), String> {
        let mut progress = job.progress();
        let ended = progress
            .wait_for(|p| p.done.is_some())
            .await
            .unwrap()
            .clone();
        ended.done.unwrap()
    }

    fn alive(pid: u32) -> bool {
        Path::new(&format!("/proc/{pid}")).exists()
            && std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .is_ok_and(|stat| !stat.contains(") Z "))
    }

    fn stopped(pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| stat.contains(") T "))
    }

    #[tokio::test]
    async fn a_failed_encode_reports_ffmpeg_s_last_words() {
        let (jobs, _dir) = jobs(
            "fail",
            "echo 'out.mkv: Invalid data found when processing input' >&2; exit 1",
            2,
            Duration::from_secs(60),
        )
        .await;
        let dir = jobs.dir().join("h/0");
        let job = jobs
            .start(dir, Vec::new(), Written::Hls, false, 0, 0, 0, None)
            .unwrap();
        assert_eq!(
            done(&job).await.unwrap_err(),
            "out.mkv: Invalid data found when processing input"
        );
    }

    #[tokio::test]
    async fn what_a_job_writes_is_measured() {
        let (jobs, _dir) = jobs(
            "write",
            "printf 'abcdefghij' > \"$1\"; echo out_time_us=4000000",
            2,
            Duration::from_secs(60),
        )
        .await;
        let dir = jobs.dir().join("h/0");
        let file = dir.join("out.mkv");
        let job = jobs
            .start(
                dir,
                vec![file.display().to_string()],
                Written::Progressive { file },
                false,
                0,
                0,
                0,
                None,
            )
            .unwrap();
        done(&job).await.unwrap();
        let progress = job.progress().borrow().clone();
        assert_eq!(progress.bytes, 10);
        assert_eq!(progress.encoded_ms, 4000);
        let counts = jobs.counts();
        assert_eq!(counts.output_seconds, 4.0);
        assert_eq!(counts.running, 0);
    }

    /// A run started at a seek reports the film's own time: only what it
    /// encoded from there is output (P3-14).
    #[tokio::test]
    async fn output_is_counted_from_where_the_run_started() {
        let (jobs, _dir) = jobs(
            "output-from-seek",
            "echo out_time_us=61000000; echo out_time_us=63500000",
            2,
            Duration::from_secs(60),
        )
        .await;
        let job = jobs
            .start(
                jobs.dir().join("h/0"),
                Vec::new(),
                Written::Hls,
                false,
                0,
                0,
                60_000,
                None,
            )
            .unwrap();
        done(&job).await.unwrap();
        assert_eq!(jobs.counts().output_seconds, 3.5);
    }

    #[tokio::test]
    async fn one_job_too_many_is_busy_until_one_stops() {
        let (jobs, _dir) = jobs("busy", "exec sleep 30", 1, Duration::from_secs(60)).await;
        let scratch = jobs.dir().to_path_buf();
        let first = jobs
            .start(
                scratch.join("a/0"),
                Vec::new(),
                Written::Hls,
                false,
                0,
                0,
                0,
                None,
            )
            .unwrap();
        assert!(matches!(
            jobs.start(
                scratch.join("b/0"),
                Vec::new(),
                Written::Hls,
                false,
                0,
                0,
                0,
                None
            ),
            Err(JobError::Busy)
        ));
        let counts = jobs.counts();
        assert_eq!((counts.running, counts.refused), (1, 1));
        first.stop();
        jobs.start(
            scratch.join("b/0"),
            Vec::new(),
            Written::Hls,
            false,
            0,
            0,
            0,
            None,
        )
        .unwrap();
    }

    #[tokio::test]
    async fn an_encode_counts_as_encoding_while_ffmpeg_runs_and_a_copy_never_does() {
        let (some, _dir) = jobs("encoding", "exec sleep 30", 3, Duration::from_secs(60)).await;
        let encoding = some.encoding();
        let scratch = some.dir().to_path_buf();
        let copy = some
            .start(
                scratch.join("c/0"),
                Vec::new(),
                Written::Hls,
                false,
                0,
                0,
                0,
                None,
            )
            .unwrap();
        assert_eq!(encoding.running(), 0);
        let stopped = some
            .start(
                scratch.join("a/0"),
                Vec::new(),
                Written::Hls,
                true,
                0,
                0,
                0,
                None,
            )
            .unwrap();
        assert_eq!(encoding.running(), 1);
        stopped.stop();
        none_encoding(&encoding).await;
        drop(copy);
        // One whose ffmpeg exits by itself stops counting then, though
        // the job (its playback) lives on.
        let (quick, _dir) = jobs("encoding-exits", "exit 0", 1, Duration::from_secs(60)).await;
        let encoding = quick.encoding();
        let exited = quick
            .start(
                quick.dir().join("e/0"),
                Vec::new(),
                Written::Hls,
                true,
                0,
                0,
                0,
                None,
            )
            .unwrap();
        done(&exited).await.unwrap();
        none_encoding(&encoding).await;
    }

    async fn none_encoding(encoding: &Encoding) {
        tokio::time::timeout(
            Duration::from_secs(10),
            encoding.watch().wait_for(|n| *n == 0),
        )
        .await
        .expect("an encode still counts")
        .unwrap();
    }

    /// Waits up to 5 s for `ready`, checking every 20 ms.
    async fn eventually(ready: impl Fn() -> bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !ready() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("never happened");
    }

    /// Playback debug (P3-15) shows what ffmpeg said last while it still
    /// runs, not only once it has failed; only the last lines are kept.
    #[tokio::test]
    async fn ffmpeg_s_last_stderr_lines_are_kept_while_it_runs() {
        let (jobs, _dir) = jobs(
            "stderr-ring",
            "i=0; while [ $i -lt 100 ]; do echo \"line $i\" >&2; i=$((i+1)); done; exec sleep 30",
            2,
            Duration::from_secs(60),
        )
        .await;
        let job = jobs
            .start(
                jobs.dir().join("h/0"),
                Vec::new(),
                Written::Hls,
                false,
                0,
                0,
                0,
                None,
            )
            .unwrap();
        eventually(|| job.stderr().last().is_some_and(|l| l == "line 99")).await;
        let kept = job.stderr();
        assert_eq!(kept.len(), STDERR_KEPT);
        assert_eq!(kept[0], format!("line {}", 100 - STDERR_KEPT));
    }

    /// What the job runs, as it ran it: the program and its arguments.
    #[tokio::test]
    async fn a_job_knows_its_command_line() {
        let (jobs, _dir) = jobs("command", "exec sleep 30", 2, Duration::from_secs(60)).await;
        let args = vec!["-i".to_owned(), "/media/a b.mkv".to_owned()];
        let job = jobs
            .start(
                jobs.dir().join("h/0"),
                args.clone(),
                Written::Hls,
                false,
                0,
                0,
                0,
                None,
            )
            .unwrap();
        let mut expected = vec![jobs.ffmpeg.display().to_string()];
        expected.extend(args);
        assert_eq!(job.command(), expected);
    }

    /// End conversion (P3-15): ffmpeg stops, the place is free at once, and
    /// what it wrote stays readable as all there is, rather than a reader
    /// waiting for more that never comes.
    #[tokio::test]
    async fn an_ended_encode_stops_and_what_it_wrote_is_the_whole() {
        let (jobs, _dir) = jobs(
            "end",
            "printf 'abcdefghij' > \"$1\"; echo out_time_us=4000000; exec sleep 30",
            1,
            Duration::from_secs(60),
        )
        .await;
        let dir = jobs.dir().join("h/0");
        let file = dir.join("out.mkv");
        let job = jobs
            .start(
                dir.clone(),
                vec![file.display().to_string()],
                Written::Progressive { file: file.clone() },
                true,
                0,
                0,
                0,
                None,
            )
            .unwrap();
        let mut progress = job.progress();
        progress.wait_for(|p| p.bytes == 10).await.unwrap();
        let pid = job.pid();
        job.end();
        let now = job.progress().borrow().clone();
        assert_eq!(now.done, Some(Ok(())));
        assert_eq!(now.bytes, 10);
        eventually(|| !alive(pid)).await;
        // Its files stay for the reader until the job is dropped.
        assert!(file.exists());
        // Its place is free: another job starts at the limit of one.
        let other = jobs.start(
            jobs.dir().join("i/0"),
            Vec::new(),
            Written::Hls,
            false,
            0,
            0,
            0,
            None,
        );
        assert!(other.is_ok(), "the ended job still holds its place");
        eventually(|| jobs.encoding().running() == 0).await;
    }

    #[tokio::test]
    async fn dropping_a_job_kills_ffmpeg_and_removes_its_files() {
        let (jobs, _dir) = jobs("drop", "exec sleep 30", 2, Duration::from_secs(60)).await;
        let dir = jobs.dir().join("h/0");
        let job = jobs
            .start(dir.clone(), Vec::new(), Written::Hls, false, 0, 0, 0, None)
            .unwrap();
        let pid = job.pid();
        assert!(alive(pid));
        drop(job);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!alive(pid), "ffmpeg {pid} still runs");
        assert!(!dir.exists());
    }

    #[tokio::test]
    async fn an_encode_far_ahead_of_its_reader_is_paused_and_resumed() {
        // Reports a minute of encoded time at once, writes 100 bytes, then
        // waits.
        let (jobs, _dir) = jobs(
            "throttle",
            "head -c 100 /dev/zero > \"$1\"; echo out_time_us=60000000; exec sleep 30",
            2,
            Duration::from_secs(10),
        )
        .await;
        let dir = jobs.dir().join("h/0");
        let file = dir.join("out.mkv");
        let job = jobs
            .start(
                dir,
                vec![file.display().to_string()],
                Written::Progressive { file },
                false,
                0,
                0,
                0,
                None,
            )
            .unwrap();
        let pid = job.pid();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !stopped(pid) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("not paused");
        assert!(job.paused());
        let counts = jobs.counts();
        assert_eq!((counts.running, counts.paused), (1, 1));
        // The reader is at 95 of 100 bytes: 57 s of 60, 3 s behind.
        job.read_to(95);
        tokio::time::timeout(Duration::from_secs(5), async {
            while stopped(pid) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("not resumed");
        assert!(!job.paused());
    }

    /// ffmpeg keeps a Matroska cluster to itself until it is whole, so it
    /// reports encoded time before it has written a byte. A job held back
    /// then stayed so for ever: its reader had nothing to read, and only a
    /// read lets a held job go on.
    #[tokio::test]
    async fn an_encode_that_has_written_nothing_is_not_held_back() {
        // Reports two seconds encoded, and writes them a second later.
        let (jobs, _dir) = jobs(
            "nothing-written",
            "echo out_time_us=2000000; sleep 1; head -c 100 /dev/zero > \"$1\"; exec sleep 30",
            2,
            Duration::from_millis(500),
        )
        .await;
        let dir = jobs.dir().join("h/0");
        let file = dir.join("out.mkv");
        let job = jobs
            .start(
                dir,
                vec![file.display().to_string()],
                Written::Progressive { file },
                false,
                0,
                0,
                0,
                None,
            )
            .unwrap();
        let mut progress = job.progress();
        tokio::time::timeout(
            Duration::from_secs(5),
            progress.wait_for(|p| p.bytes == 100),
        )
        .await
        .expect("the encode writes on")
        .unwrap();
    }

    /// The same for HLS: the reader waits for a segment ffmpeg has not
    /// finished, though it reports time past the segment's end.
    #[tokio::test]
    async fn an_encode_is_not_held_back_before_the_segment_its_reader_waits_for() {
        // Reports twenty seconds encoded, and lists segment 1 a second later.
        let (jobs, _dir) = jobs(
            "segment-awaited",
            &format!(
                "echo out_time_us=20000000; sleep 1; echo seg1.m4s > \"$1/{PLAYLIST}\"; exec sleep 30"
            ),
            2,
            Duration::from_millis(500),
        )
        .await;
        let dir = jobs.dir().join("h/0");
        let args = vec![dir.display().to_string()];
        let job = jobs
            .start(dir, args, Written::Hls, false, 0, 1, 0, None)
            .unwrap();
        job.reader_at(1, 6_000);
        let mut progress = job.progress();
        let listed = progress.wait_for(|p| p.last_segment == Some(1));
        tokio::time::timeout(Duration::from_secs(5), listed)
            .await
            .expect("the encode finishes the segment")
            .unwrap();
    }

    /// `read_ahead_secs = 0` holds the encode whenever its reader is
    /// behind. It once never let it go on: with the reader caught up the
    /// encode was no longer ahead, but not under half of nothing either.
    #[tokio::test]
    async fn an_encode_with_no_read_ahead_goes_on_once_its_reader_has_caught_up() {
        let (jobs, _dir) = jobs(
            "no-read-ahead",
            "head -c 100 /dev/zero > \"$1\"; echo out_time_us=1000000; exec sleep 30",
            2,
            Duration::ZERO,
        )
        .await;
        let dir = jobs.dir().join("h/0");
        let file = dir.join("out.mkv");
        let job = jobs
            .start(
                dir,
                vec![file.display().to_string()],
                Written::Progressive { file },
                false,
                0,
                0,
                0,
                None,
            )
            .unwrap();
        let pid = job.pid();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !stopped(pid) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("not paused");
        job.read_to(100);
        tokio::time::timeout(Duration::from_secs(5), async {
            while stopped(pid) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("not resumed");
        assert!(!job.paused());
    }

    /// Short, for the tests of a stalled encode, and over two looks at the
    /// output long.
    const STALLED_SOON: Duration = Duration::from_millis(500);

    fn stalled(done: Result<(), String>) -> bool {
        done.is_err_and(|reason| reason.starts_with("the conversion stalled"))
    }

    /// ffmpeg can wait for ever with nothing wrong that it knows of (it
    /// does when it lays a picture subtitle file of one cue over the
    /// video and loses the file's end). The job once waited with it, in
    /// its slot, and its reads waited too.
    #[tokio::test]
    async fn an_encode_that_makes_no_progress_is_ended_and_gives_up_its_slot() {
        let (jobs, _dir) = jobs_stalled_after(
            "stalled",
            "exec sleep 30",
            1,
            Duration::from_secs(60),
            STALLED_SOON,
        )
        .await;
        let scratch = jobs.dir().to_path_buf();
        let job = jobs
            .start(
                scratch.join("a/0"),
                Vec::new(),
                Written::Hls,
                false,
                0,
                0,
                0,
                None,
            )
            .unwrap();
        let pid = job.pid();
        let ended = tokio::time::timeout(Duration::from_secs(5), done(&job))
            .await
            .expect("the stalled encode is ended");
        assert_eq!(
            ended.unwrap_err(),
            "the conversion stalled: ffmpeg made no progress for 500ms"
        );
        assert!(!alive(pid), "ffmpeg {pid} still runs");
        let counts = jobs.counts();
        assert_eq!((counts.running, counts.stalled), (0, 1));
        // The job is still held, as by a playback not yet stopped: its
        // slot, the only one, is free.
        jobs.start(
            scratch.join("b/0"),
            Vec::new(),
            Written::Hls,
            false,
            0,
            0,
            0,
            None,
        )
        .unwrap();
    }

    /// An encode held back for its reader makes no progress on purpose,
    /// for as long as the reader takes. The time it is held does not count:
    /// let go on, it has the whole time again.
    #[tokio::test]
    async fn an_encode_held_back_for_its_reader_is_not_stalled() {
        // Reports a minute of encoded time, writes 100 bytes, then hangs.
        let (jobs, _dir) = jobs_stalled_after(
            "stalled-held",
            "head -c 100 /dev/zero > \"$1\"; echo out_time_us=60000000; exec sleep 30",
            2,
            Duration::from_secs(10),
            STALLED_SOON,
        )
        .await;
        let dir = jobs.dir().join("h/0");
        let file = dir.join("out.mkv");
        let job = jobs
            .start(
                dir,
                vec![file.display().to_string()],
                Written::Progressive { file },
                false,
                0,
                0,
                0,
                None,
            )
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !job.paused() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("not paused");
        // Held for six looks, 1.2 s: over twice the time a stall takes.
        looked(&job, 6).await;
        assert_eq!(job.progress().borrow().done, None);
        assert!(job.paused());
        // The reader catches up: the encode goes on, and now it hangs.
        job.read_to(100);
        let ended = tokio::time::timeout(Duration::from_secs(5), done(&job))
            .await
            .expect("the stalled encode is ended");
        assert!(stalled(ended));
    }

    /// A slow encode is not a stalled one: while ffmpeg reports more time
    /// encoded, however little, the job lets it run.
    #[tokio::test]
    async fn an_encode_that_gets_on_slowly_is_not_stalled() {
        // Reports a tenth of a second more every tenth of a second, for
        // 1.5 s (three times the time a stall takes), says so, then hangs.
        let (jobs, _dir) = jobs_stalled_after(
            "stalled-slow",
            "i=0; while [ $i -lt 15 ]; do i=$((i+1)); echo out_time_us=${i}00000; sleep 0.1; done\n\
             touch \"$1\"; exec sleep 30",
            2,
            Duration::from_secs(60),
            STALLED_SOON,
        )
        .await;
        let dir = jobs.dir().join("h/0");
        let slow_part_over = jobs.dir().join("over");
        let args = vec![slow_part_over.display().to_string()];
        let job = jobs
            .start(dir, args, Written::Hls, false, 0, 0, 0, None)
            .unwrap();
        let ended = tokio::time::timeout(Duration::from_secs(10), done(&job))
            .await
            .expect("the stalled encode is ended");
        assert!(stalled(ended));
        assert!(
            slow_part_over.exists(),
            "ended while it still reported progress"
        );
    }

    /// Jobs running `body` as ffmpeg, which may write `scratch_limit`
    /// bytes together.
    async fn jobs_within(
        test: &str,
        body: &str,
        scratch_limit: u64,
    ) -> (Jobs, spoolframe_test_dir::TestDir) {
        let tmp = scratch(test).await;
        let root = tmp.as_ref().to_path_buf();
        let bin = ffmpeg(&root.join("bin"), body);
        let limits = JobLimits {
            max_jobs: 2,
            scratch_dir: root.join("scratch"),
            scratch_limit_bytes: scratch_limit,
            read_ahead: Duration::from_secs(60),
            stalled_after: STALLED_AFTER,
        };
        (Jobs::new(limits, bin).unwrap(), tmp)
    }

    const FULL: &str = "the server's scratch space for conversions is full";

    /// Waits until `job` has looked at its output `times` more times. A
    /// job tells its progress at every look, changed or not, and only
    /// then looks at the scratch space: with the next look told, the one
    /// before is over.
    async fn looked(job: &EncodeJob, times: usize) {
        let mut progress = job.progress();
        progress.borrow_and_update();
        for _ in 0..times {
            tokio::time::timeout(Duration::from_secs(5), progress.changed())
                .await
                .expect("the job looks at its output")
                .unwrap();
        }
    }

    #[tokio::test]
    async fn the_job_that_fills_the_scratch_space_fails_and_no_other() {
        // `quiet` writes 100 bytes and waits; the other writes on and on.
        let (jobs, _dir) = jobs_within(
            "full",
            "if [ \"$2\" = quiet ]; then head -c 100 /dev/zero > \"$1\"; exec sleep 30; fi\n\
             while :; do head -c 1000 /dev/zero >> \"$1\"; sleep 0.01; done",
            20_000,
        )
        .await;
        let start = |name: &str, mode: &str| {
            let dir = jobs.dir().join(name).join("0");
            let file = dir.join("out.mkv");
            let args = vec![file.display().to_string(), mode.to_owned()];
            let written = Written::Progressive { file: file.clone() };
            (
                jobs.start(dir, args, written, false, 0, 0, 0, None)
                    .unwrap(),
                file,
            )
        };
        let (quiet, kept) = start("a", "quiet");
        let mut progress = quiet.progress();
        progress.wait_for(|p| p.bytes == 100).await.unwrap();
        // It has seen its 100 bytes, in a space with room.
        looked(&quiet, 1).await;
        // Something else fills the space, and it stays full: the quiet job
        // looks at its output twice more, so once at least with the space
        // full and that look over.
        let other = jobs.dir().join("c/0");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("out.mkv"), vec![0; 30_000]).unwrap();
        looked(&quiet, 2).await;
        assert_eq!(quiet.progress().borrow().done, None);
        // The job that writes into the full space is the one that fails.
        let (greedy, removed) = start("b", "greedy");
        assert_eq!(done(&greedy).await.unwrap_err(), FULL);
        // What it wrote is gone.
        assert!(!removed.exists());
        assert_eq!(quiet.progress().borrow().done, None);
        assert!(alive(quiet.pid()));
        assert!(kept.exists());
    }

    /// Writes segments 1 to 8 of 1000 bytes into `dir`, as ffmpeg would,
    /// and waits until `job` has made room for them and that look at the
    /// scratch space is over (or the job has ended). The job sees the
    /// eight files at once: its task runs on the test's thread, only while
    /// the test waits. It has finished the look that removed a segment
    /// when it counts a byte written after.
    async fn eight_segments(job: &EncodeJob, dir: &Path) {
        for n in 1..=8 {
            std::fs::write(dir.join(format!("seg{n}.m4s")), [0; 1000]).unwrap();
        }
        let mut progress = job.progress();
        tokio::time::timeout(Duration::from_secs(5), async {
            while job.first_kept() == 1 {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            std::fs::write(dir.join("mark"), [0]).unwrap();
            let counted = |p: &Progress| p.bytes % 1000 == 1 || p.done.is_some();
            progress.wait_for(counted).await.unwrap();
        })
        .await
        .expect("no segment removed");
    }

    /// Which of segments 1 to 8 are in `dir`.
    fn segments_left(dir: &Path) -> Vec<bool> {
        (1..=8)
            .map(|n| dir.join(format!("seg{n}.m4s")).exists())
            .collect()
    }

    #[tokio::test]
    async fn a_full_scratch_space_takes_the_segments_behind_the_reader_first() {
        let (jobs, _dir) = jobs_within("trim", "exec sleep 30", 5_500).await;
        let dir = jobs.dir().join("h/0");
        let job = jobs
            .start(dir.clone(), Vec::new(), Written::Hls, false, 0, 1, 0, None)
            .unwrap();
        // The reader asked for segment 7: 6 stays too, 1 to 5 may go.
        job.reader_at(7, 42_000);
        eight_segments(&job, &dir).await;
        // 8000 bytes, 5500 allowed: the three oldest went, and no more.
        assert_eq!(job.first_kept(), 4);
        assert_eq!(
            segments_left(&dir),
            [false, false, false, true, true, true, true, true]
        );
        assert_eq!(job.progress().borrow().done, None);
    }

    /// A player that seeks back to a segment still on disk reads there
    /// again. The job once kept only the furthest segment ever asked for,
    /// so it took the segments the player was now reading for ones behind
    /// it, and removed them for room.
    #[tokio::test]
    async fn a_reader_that_went_back_keeps_the_segments_it_now_reads() {
        // Says it has encoded two minutes and lists segments 1 to 8, in a
        // playlist as large as a segment, then waits.
        let script = format!(
            "printf 'seg%s.m4s\\n' 1 2 3 4 5 6 7 8 > \"$1/{PLAYLIST}\"; \
             truncate -s 1000 \"$1/{PLAYLIST}\"; echo out_time_us=120000000; exec sleep 30"
        );
        let (jobs, _dir) = jobs_within("back", &script, 5_500).await;
        let dir = jobs.dir().join("h/0");
        let args = vec![dir.display().to_string()];
        let job = jobs
            .start(dir.clone(), args, Written::Hls, false, 0, 1, 0, None)
            .unwrap();
        // The reader got to segment 7, then went back to 4.
        job.reader_at(7, 42_000);
        job.reader_at(4, 24_000);
        // Far behind what is encoded: the job is held back for it.
        tokio::time::timeout(Duration::from_secs(5), async {
            while !job.paused() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("not paused");
        eight_segments(&job, &dir).await;
        // 9000 bytes with the playlist, 5500 allowed. Segments 1 and 2 are
        // behind the reader and went; 3 is the one before the reader's and
        // stays, with all after it, though that leaves 7000.
        assert_eq!(job.first_kept(), 3);
        assert_eq!(
            segments_left(&dir),
            [false, false, true, true, true, true, true, true]
        );
        // The job is held back and writes no more: it is not failed for
        // the space it cannot free yet.
        assert_eq!(job.progress().borrow().done, None);
    }

    #[tokio::test]
    async fn leftovers_from_a_previous_run_are_removed() {
        let tmp = scratch("leftovers").await;
        let root = tmp.as_ref();
        let old = root.join("scratch").join(JOBS_DIR).join("oldhandle/0");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("out.mkv"), b"stale").unwrap();
        let limits = JobLimits {
            max_jobs: 1,
            scratch_dir: root.join("scratch"),
            scratch_limit_bytes: 1 << 30,
            read_ahead: Duration::from_secs(60),
            stalled_after: STALLED_AFTER,
        };
        let jobs = Jobs::new(limits, "ffmpeg".into()).unwrap();
        assert!(!jobs.dir().join("oldhandle").exists());
        assert!(jobs.dir().is_dir());
    }

    /// `transcode.scratch_dir` may name a directory other things are in:
    /// the state directory, the parent of the subtitle cache, a mount
    /// other programs use. Once, all of it was removed at every start.
    #[tokio::test]
    async fn what_else_is_in_the_scratch_directory_is_left_alone() {
        let tmp = scratch("not-ours").await;
        let root = tmp.as_ref();
        let key = root.join("state/endpoint.key");
        let text = root.join("state/subtitles/film.vtt");
        std::fs::create_dir_all(text.parent().unwrap()).unwrap();
        std::fs::write(&key, b"secret").unwrap();
        std::fs::write(&text, b"WEBVTT").unwrap();
        let limits = JobLimits {
            max_jobs: 1,
            scratch_dir: root.join("state"),
            scratch_limit_bytes: 1 << 30,
            read_ahead: Duration::from_secs(60),
            stalled_after: STALLED_AFTER,
        };
        Jobs::new(limits, "ffmpeg".into()).unwrap();
        assert_eq!(std::fs::read(&key).unwrap(), b"secret");
        assert_eq!(std::fs::read(&text).unwrap(), b"WEBVTT");
    }
}
