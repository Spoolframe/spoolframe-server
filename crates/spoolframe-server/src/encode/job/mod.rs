//! One ffmpeg run (P2-03): spawned for a playback, paused when it is far
//! ahead of its reader, killed and cleaned up when dropped.

mod budget;
mod encode_job;
mod process;
mod supervisor;
#[cfg(test)]
pub(crate) mod tests;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64};
use std::time::Duration;

use tokio::sync::watch;

use spoolframe_media::sandbox::{GpuUse, Tool, Vulkan};

use super::JobLimits;
use super::args::{Made, PLAYLIST};
use crate::metrics::{Counter, Histograms};

pub(crate) use self::budget::Yielding;
pub use self::budget::{BudgetUse, CardLimit, Load, Slot, Units};
use self::budget::{Place, YieldEntry};
pub use self::encode_job::{CaughtUp, EncodeJob};
use self::process::DecodeFailures;
pub(crate) use self::process::{exit_blames_gpu, jail, launch, out_time_us, signal};
#[cfg(test)]
pub(super) use self::supervisor::listed_segments;
use self::supervisor::{RunSpan, Shared, Supervisor};

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
    pub done: Option<Result<(), Failure>>,
    /// What was written, over how much of the film (PX-24): what its
    /// measured bitrate is from.
    pub output: Measured,
}

/// Bytes written over the media time they hold (PX-24). For HLS, each
/// finished segment's file size and its duration in ffmpeg's playlist,
/// counted once, as it is listed: a run from 0 lists only its last
/// segment, and a full scratch directory removes old ones. For a
/// progressive file, the file over the time encoded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Measured {
    pub bytes: u64,
    pub ms: u64,
    /// For HLS, the last segment counted.
    pub through: Option<u32>,
}

impl Measured {
    /// Bits per second, once some media time is written.
    pub fn bitrate(&self) -> Option<u64> {
        (self.ms > 0).then(|| (u128::from(self.bytes) * 8000 / u128::from(self.ms)) as u64)
    }

    /// The segments `listed` in `dir`'s ffmpeg playlist past
    /// [`Measured::through`] counted in: their files' sizes and listed durations. A segment
    /// whose file is gone already is passed over, not counted.
    fn count_segments(&mut self, dir: &Path, listed: &[(u32, u64)]) {
        for &(n, ms) in listed {
            if self.through.is_some_and(|through| n <= through) {
                continue;
            }
            if let Ok(meta) = std::fs::metadata(dir.join(format!("seg{n}.m4s"))) {
                // Durations are parsed text: a huge one saturates rather
                // than wraps (or panics in a debug build).
                self.bytes = self.bytes.saturating_add(meta.len());
                self.ms = self.ms.saturating_add(ms);
            }
            self.through = Some(n);
        }
    }
}

/// Why a job ended unfinished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// The server's words, or ffmpeg's last line.
    pub reason: String,
    /// The reason is ffmpeg's own (PX-09 finding 23).
    pub from_ffmpeg: bool,
}

impl Failure {
    fn server(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            from_ffmpeg: false,
        }
    }

    /// What the client is told: the server's words as they are, never
    /// ffmpeg's.
    pub fn for_client(&self) -> &str {
        if self.from_ffmpeg {
            FFMPEG_FAILED
        } else {
            &self.reason
        }
    }
}

/// What a client is told of an encode ffmpeg failed (PX-09 finding 23):
/// ffmpeg's own words name the server's paths, so they are only in the
/// log and on the admin's playback page.
pub const FFMPEG_FAILED: &str = "the conversion failed on the server";

#[derive(Debug, thiserror::Error)]
pub enum JobError {
    /// Over the encode budget (`max_jobs`), or the load's own cap.
    #[error("the server is converting as many files as it may")]
    Busy,
    /// Why, in words a client may see.
    #[error("cannot prepare the encode: {0}")]
    Prepare(String),
    /// ffmpeg could not be run: the reason names it, and where it is, so
    /// it is for the log only (PX-09 finding 23).
    #[error("cannot run ffmpeg: {0}")]
    Spawn(String),
}

/// What a client is told of an encode that could not be run.
pub const SPAWN_FAILED: &str = "the conversion could not be started on the server";

/// The directory under `transcode.scratch_dir` that jobs write in. The
/// server makes it, and it is the only thing there the server removes.
pub const JOBS_DIR: &str = "spoolframe-jobs";

/// The jobs of this server: the limit, and where they write.
#[derive(Debug)]
pub struct Jobs {
    /// Live (P3-10) but for the scratch directory: a job reads them when
    /// it starts.
    limits: parking_lot::RwLock<JobLimits>,
    /// [`JOBS_DIR`] in the scratch directory.
    dir: PathBuf,
    ffmpeg: Tool,
    budget: Arc<parking_lot::Mutex<BudgetUse>>,
    /// Told as a place in the budget is given back, or the limits change:
    /// the copy pool (PX-37), its one waiter, tries for a place again.
    released: Arc<tokio::sync::Notify>,
    /// Held by the job that looks at a full scratch directory and makes
    /// room in it, so the next job to look sees what the first one freed.
    scratch_check: Arc<tokio::sync::Mutex<()>>,
    /// What the running jobs have written to the scratch directory, each
    /// job adding what it measured since it last did: what decides whether
    /// one walks the whole directory to make room (review 2026-10-11 H2).
    scratch_used: Arc<AtomicU64>,
    /// How many jobs run now, and playbacks being prepared: frame
    /// previews made ahead wait while any is (P3-13, PX-25).
    encoding: Encoding,
    /// Every job started and not yet dropped, to count (P3-14).
    live: parking_lot::Mutex<Vec<std::sync::Weak<EncodeJob>>>,
    /// Yielding holds (PX-37), newest last: the places live admission may
    /// take when it finds no other.
    yielding: parking_lot::Mutex<Vec<std::sync::Weak<YieldEntry>>>,
    counters: Arc<JobCounters>,
    /// Films whose GPU decode failed: they decode in software from then on.
    decode_failures: Arc<DecodeFailures>,
    /// What [`Jobs::first_note`] was told.
    notes: parking_lot::Mutex<HashSet<&'static str>>,
    /// The server's own subtitles (`state_dir/subtitles`: PX-03's served
    /// files, PX-02's downloads), in the hidden state directory: a job
    /// takes a copy of one, as no run may open it as an input.
    own_subtitles: Option<PathBuf>,
}

/// What the jobs did since the server started (P3-14).
#[derive(Debug, Default)]
struct JobCounters {
    /// Starts refused for want of a free place: there is no queue.
    refused: Counter,
    /// Yielding holds (PX-37) whose place live admission took.
    yielded: Counter,
    /// ffmpeg runs ended for making no progress.
    stalled: Counter,
    /// Media time encoded, in ms, by every job.
    output_ms: Counter,
    /// Runs started to replace another, by [`RESTART_REASONS`].
    restarts: [Counter; 4],
    /// Where each run's speed and each read's wait go.
    histograms: std::sync::OnceLock<Arc<Histograms>>,
}

/// Why a run replaced another: a seek, a switch to another rung, a
/// restart at a segment far from the run, a change of subtitle or quality.
pub const RESTART_REASONS: [&str; 4] = ["seek", "switch", "restart", "change"];

/// The jobs now (P3-14).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JobCounts {
    /// Jobs whose ffmpeg runs (paused or not).
    pub running: u64,
    /// Of those, how many of each load, in [`Load::ALL`]'s order.
    pub running_by_load: [u64; 5],
    /// Of those, the ones held back for their reader.
    pub paused: u64,
    pub refused: u64,
    /// Yielding holds (PX-37) whose place live playback took.
    pub yielded: u64,
    pub stalled: u64,
    /// Seconds of media encoded since the server started.
    pub output_seconds: f64,
    /// Runs started to replace another, in [`RESTART_REASONS`]' order.
    pub restarts: [u64; 4],
}

/// How many conversions run now, an encode or a remux, and playbacks
/// being prepared (their keyframes read, PX-25), and a way to wait for
/// that to change: frame previews made ahead wait while any is (P3-13).
///
/// A remux counts as much as an encode: it reads the film as fast as the
/// disk gives it until it is a read-ahead past its reader, and previews
/// made ahead read whole films from the same disks. A beta tester's first
/// remuxes of 4K films took 20–30 s to start beside eight preview workers.
#[derive(Debug, Clone)]
pub struct Encoding(Arc<watch::Sender<usize>>);

impl Default for Encoding {
    fn default() -> Self {
        Self(Arc::new(watch::Sender::new(0)))
    }
}

impl Encoding {
    /// One more conversion runs, or playback is prepared, until the guard
    /// is dropped.
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

/// A conversion or a playback being prepared, counted in [`Encoding`]
/// while held.
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
    pub fn new(limits: JobLimits, ffmpeg: Tool) -> std::io::Result<Self> {
        let dir = limits.scratch_dir.join(JOBS_DIR);
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => {
                tracing::info!(dir = %dir.display(), "removed leftover encodes")
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        // The scratch directory may be shared (`/tmp`): only the jobs'
        // own is closed to others.
        crate::files::private_dir(&dir)?;
        let mut budget = BudgetUse::default();
        budget.set_limits(&limits);
        Ok(Self {
            budget: Arc::new(parking_lot::Mutex::new(budget)),
            released: Arc::default(),
            scratch_check: Arc::default(),
            scratch_used: Arc::default(),
            encoding: Encoding::default(),
            live: parking_lot::Mutex::default(),
            yielding: parking_lot::Mutex::default(),
            counters: Arc::default(),
            decode_failures: Arc::default(),
            notes: parking_lot::Mutex::default(),
            limits: parking_lot::RwLock::new(limits),
            dir,
            ffmpeg,
            own_subtitles: None,
        })
    }

    /// The server's own subtitles are under `dir` (PX-02).
    pub fn with_own_subtitles(mut self, dir: PathBuf) -> Self {
        self.own_subtitles = Some(dir);
        self
    }

    /// Whether `path` is one of the server's own subtitles.
    pub(crate) fn is_own_subtitle(&self, path: &Path) -> bool {
        self.own_subtitles
            .as_deref()
            .is_some_and(|dir| path.starts_with(dir))
    }

    /// Whether decoding (or filtering) `file` on the GPU failed since the
    /// server started.
    pub fn hardware_decode_failed(&self, file: &Path) -> bool {
        self.decode_failures.contains(file)
    }

    /// True the first time it is told `note`: a reason GPU filters were
    /// not used, logged once rather than at every start and seek.
    pub fn first_note(&self, note: &'static str) -> bool {
        self.notes.lock().insert(note)
    }

    /// The limits now.
    pub fn limits(&self) -> JobLimits {
        self.limits.read().clone()
    }

    /// `transcode.gpu_filters` and `transcode.tone_map` now, read together
    /// so a spec never mixes two configs.
    pub fn filters(&self) -> (bool, spoolframe_media::encoders::ToneMapSetting) {
        let limits = self.limits.read();
        (limits.gpu_filters, limits.tone_map)
    }

    /// Run new jobs under `limits` from now on. Jobs running go on as they
    /// started; with less budget than is taken, none starts until enough
    /// jobs end. The scratch directory stays the one the jobs started in:
    /// it changes only with a restart.
    pub fn set_limits(&self, limits: JobLimits) {
        self.budget.lock().set_limits(&limits);
        // A higher limit may have room now.
        self.released.notify_one();
        let mut now = self.limits.write();
        *now = JobLimits {
            scratch_dir: now.scratch_dir.clone(),
            ..limits
        };
    }

    /// Where jobs write: a directory per playback handle in it.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The encode budget's use now, and its limits.
    pub fn budget(&self) -> BudgetUse {
        self.budget.lock().clone()
    }

    /// Whether `card` runs fewer GPU encodes than its limit now: a
    /// conversion goes to the next card when not (PGPU-05).
    pub fn card_has_room(&self, card: &CardLimit) -> bool {
        self.budget.lock().card_has_room(Some(card))
    }

    /// How many jobs that encode run now.
    pub fn encoding(&self) -> Encoding {
        self.encoding.clone()
    }

    /// The jobs now, and what they did (P3-14).
    pub fn counts(&self) -> JobCounts {
        let jobs: Vec<Arc<EncodeJob>> = {
            let mut live = self.live.lock();
            live.retain(|job| job.strong_count() > 0);
            live.iter().filter_map(std::sync::Weak::upgrade).collect()
        };
        // Dropped outside the lock: the last hold of a job removes its files.
        let running: Vec<_> = jobs
            .iter()
            .filter(|job| job.progress.borrow().done.is_none() && !job.task.is_finished())
            .collect();
        let mut running_by_load = [0; 5];
        for job in &running {
            let at = Load::ALL
                .iter()
                .position(|&l| l == job.load)
                .expect("in ALL");
            running_by_load[at] += 1;
        }
        JobCounts {
            running: running.len() as u64,
            running_by_load,
            paused: running.iter().filter(|job| job.paused()).count() as u64,
            refused: self.counters.refused.get(),
            yielded: self.counters.yielded.get(),
            stalled: self.counters.stalled.get(),
            output_seconds: self.counters.output_ms.get() as f64 / 1000.0,
            restarts: self.counters.restarts.each_ref().map(Counter::get),
        }
    }

    /// Observe each run's speed and each read's wait into `histograms`
    /// (P3-14).
    pub fn record_into(&self, histograms: Arc<Histograms>) {
        let _ = self.counters.histograms.set(histograms);
    }

    /// A run started for `reason`: counted if it replaced another, as
    /// one of [`RESTART_REASONS`] (`start` does not).
    pub fn started_for(&self, reason: &str) {
        if let Some(at) = RESTART_REASONS.iter().position(|r| *r == reason) {
            self.counters.restarts[at].inc();
        }
    }

    /// Kill every job's ffmpeg: the server is stopping. Each run's span
    /// ends now, while the export can still send it.
    pub fn stop_all(&self) {
        let live: Vec<_> = self
            .live
            .lock()
            .iter()
            .filter_map(std::sync::Weak::upgrade)
            .collect();
        for job in live {
            job.stop_because("server_stopping");
        }
    }

    /// The ffmpeg jobs run with, unless a run brings its own (a GPU
    /// card's, confined to its nodes).
    pub(crate) fn ffmpeg(&self) -> &Tool {
        &self.ffmpeg
    }

    /// Remember that decoding (or filtering) `file` on the GPU failed: it
    /// decodes in software from now on. True the first time.
    pub(crate) fn remember_decode_failure(&self, file: &Path) -> bool {
        self.decode_failures.remember(file)
    }

    /// A place in the budget for live work of `load` on `card`: `slot`,
    /// the place of the job it replaces, when that has the same load and
    /// card; otherwise a place of its own, counting the replaced one's as
    /// free; with none free, the place of a yielding hold
    /// ([`Jobs::take_yielded`]). `Busy` when there is none of those; the
    /// caller counts a refusal.
    fn admit(
        &self,
        load: Load,
        card: Option<CardLimit>,
        slot: Option<Slot>,
    ) -> Result<Slot, JobError> {
        match slot {
            Some(slot) if slot.load() == load && slot.card() == card.as_ref() => Ok(slot),
            replacing => Place::take(
                &self.budget,
                &self.released,
                load,
                card.clone(),
                replacing.as_ref(),
            )
            .or_else(|| self.take_yielded(load, card))
            .map(|place| Slot {
                place: Arc::new(place),
            })
            .ok_or(JobError::Busy),
        }
    }

    /// Live playback wins (PX-37): a place for `load` on `card` taken from
    /// the newest yielding hold whose place, counted as free, makes room
    /// for it, as a seek's run takes the place of the run it replaces. The
    /// hold is told ([`Yielding::yielded`]) and is no longer one to take;
    /// the budget counts both places until the holder drops its hold.
    /// `None`, and no hold told, when none makes room.
    fn take_yielded(&self, load: Load, card: Option<CardLimit>) -> Option<Place> {
        let mut yielding = self.yielding.lock();
        yielding.retain(|entry| entry.strong_count() > 0);
        let (at, entry, place) = yielding.iter().enumerate().rev().find_map(|(at, entry)| {
            let entry = entry.upgrade()?;
            let place = Place::take(
                &self.budget,
                &self.released,
                load,
                card.clone(),
                Some(&entry.slot),
            )?;
            Some((at, entry, place))
        })?;
        yielding.remove(at);
        drop(yielding);
        entry.preempted.send_replace(true);
        self.counters.yielded.inc();
        Some(place)
    }

    /// Until a place in the budget is given back or the limits change,
    /// since the last time this resolved: for the copy pool (PX-37), its
    /// one waiter, whose copy found no place.
    pub(crate) async fn place_released(&self) {
        self.released.notified().await;
    }

    /// A place in the budget for work of `load` on `card` that gives it up
    /// to live playback (PX-37): taken only where one is free, never from
    /// a running job or another yielding hold; `Busy` when none is. Not
    /// counted as a refused start.
    pub(crate) fn hold_yielding(
        &self,
        load: Load,
        card: Option<CardLimit>,
    ) -> Result<Yielding, JobError> {
        let place =
            Place::take(&self.budget, &self.released, load, card, None).ok_or(JobError::Busy)?;
        let entry = Arc::new(YieldEntry {
            slot: Slot {
                place: Arc::new(place),
            },
            preempted: tokio::sync::watch::Sender::new(false),
        });
        let mut yielding = self.yielding.lock();
        yielding.retain(|entry| entry.strong_count() > 0);
        yielding.push(Arc::downgrade(&entry));
        Ok(Yielding { entry })
    }

    /// A place in the budget for live work of `load` on `card` that is not
    /// a job's run, held until the slot is dropped: taken as a start takes
    /// one, from a yielding hold if none is free; `Busy` when there is
    /// none. Not counted as a refused start.
    #[cfg_attr(not(test), expect(dead_code, reason = "the copy pool (PX-37) uses it"))]
    pub(crate) fn hold(&self, load: Load, card: Option<CardLimit>) -> Result<Slot, JobError> {
        self.admit(load, card, None)
    }

    /// Run ffmpeg with `args` into `dir` (created), which the job owns and
    /// removes when dropped. It runs in `slot`, the place of the job it
    /// replaces, when that has the same `load`; otherwise it takes a place
    /// of its own, counting the replaced one's as free. One whose load
    /// encodes counts in [`Jobs::encoding`] while ffmpeg runs.
    #[expect(
        clippy::too_many_arguments,
        reason = "one caller, which names each part"
    )]
    pub fn start(
        &self,
        dir: PathBuf,
        run: impl Into<Run>,
        written: Written,
        load: Load,
        epoch: u32,
        first_segment: u32,
        start_ms: u64,
        slot: Option<Slot>,
    ) -> Result<Arc<EncodeJob>, JobError> {
        let run: Run = run.into();
        let card = run.card.clone();
        let slot = self.admit(load, card, slot).inspect_err(|_| {
            self.counters.refused.inc();
        })?;
        let slot = Arc::new(parking_lot::Mutex::new(Some(slot)));
        std::fs::create_dir_all(&dir).map_err(|e| JobError::Prepare(e.to_string()))?;
        let Run {
            args,
            inputs,
            reads,
            gpu,
            vulkan,
            fallback,
            made,
            card: _,
            ffmpeg,
            aligned_until,
            span,
        } = run;
        let ffmpeg = ffmpeg.unwrap_or_else(|| self.ffmpeg.clone());
        let limits = self.limits();
        let cannot_run =
            |e: std::io::Error| JobError::Spawn(format!("{}: {e}", ffmpeg.program().display()));
        let jail = jail(limits.scratch_limit_bytes, &dir, &reads, &inputs);
        // The fallback's run is `jail` with its own GPU: no Vulkan.
        let mut child =
            launch(&ffmpeg, &jail.clone().gpu(gpu).vulkan(vulkan), &args).map_err(cannot_run)?;
        // The run's process group, which ffmpeg leads.
        let pid = child.id().unwrap_or(0);
        let (progress_tx, progress) = watch::channel(Progress::default());
        let progress_tx = Arc::new(progress_tx);
        let mut command = vec![ffmpeg.program().display().to_string()];
        command.extend(args);
        let shared = Arc::new(Shared {
            pid: AtomicU32::new(pid),
            command: parking_lot::Mutex::new(command),
            hardware_decode: AtomicBool::new(fallback.is_some()),
            made: parking_lot::Mutex::new(made),
            furthest_bytes: AtomicU64::new(0),
            reader_ms: AtomicU64::new(0),
            reader_segment: AtomicU32::new(0),
            first_kept: AtomicU32::new(first_segment),
            paused: watch::Sender::new(false),
            counted: AtomicU64::new(0),
            stderr: parking_lot::Mutex::default(),
            span: RunSpan(parking_lot::Mutex::new(span)),
            began: std::time::Instant::now(),
            paused_for: parking_lot::Mutex::default(),
            first_ready: AtomicBool::new(false),
            load,
            histograms: self.counters.histograms.get().cloned(),
            ended: AtomicBool::new(false),
        });
        let supervisor = Supervisor {
            ffmpeg: ffmpeg.clone(),
            jail,
            fallback,
            decode_failures: self.decode_failures.clone(),
            start_ms,
            dir: dir.clone(),
            written: written.clone(),
            scratch: self.dir.clone(),
            scratch_limit: limits.scratch_limit_bytes,
            scratch_check: self.scratch_check.clone(),
            scratch_used: self.scratch_used.clone(),
            read_ahead: limits.read_ahead,
            stalled_after: limits.stalled_after,
            shared: shared.clone(),
            slot: slot.clone(),
            progress: progress_tx.clone(),
            counters: self.counters.clone(),
            _encoding: self.encoding.begin(),
        };
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let task = tokio::spawn(supervisor.run(child, stdout, stderr));
        tracing::debug!(pid, dir = %dir.display(), epoch, "encode started");
        let job = Arc::new(EncodeJob {
            epoch,
            first_segment,
            aligned_until,
            load,
            dir,
            written,
            start_ms,
            progress,
            // Only the supervisor holds it otherwise: once it is gone, a
            // reader waiting for more hears so at once.
            sender: Arc::downgrade(&progress_tx),
            shared,
            task,
            slot,
            scratch_used: self.scratch_used.clone(),
        });
        let mut live = self.live.lock();
        live.retain(|job| job.strong_count() > 0);
        live.push(Arc::downgrade(&job));
        Ok(job)
    }
}

/// One ffmpeg run: its arguments, and what it reads besides the directory
/// it writes.
#[derive(Debug, Clone, Default)]
pub struct Run {
    pub args: Vec<String>,
    /// Files someone else may have put there: the film, a subtitle file
    /// beside it.
    pub inputs: Vec<PathBuf>,
    /// Directories of the server's own it reads: the conversion's, with its
    /// links to the inputs.
    pub reads: Vec<PathBuf>,
    /// The GPU it decodes or encodes on, if any.
    pub gpu: GpuUse,
    /// How it uses Vulkan, if it does (libplacebo, PGPU-04).
    pub vulkan: Option<Vulkan>,
    /// For a run that decodes or tone maps on the GPU: the same run
    /// decoding in software, its tone map on the CPU, for when that fails
    /// before the first output.
    pub fallback: Option<Fallback>,
    /// How it makes its video (PX-24).
    pub made: Option<Made>,
    /// For a GPU video encode, the card it counts against (PGPU-05).
    pub card: Option<CardLimit>,
    /// ffmpeg confined to the GPU card it runs on, in place of the jobs'
    /// own (which is given no GPU's nodes).
    pub ffmpeg: Option<Tool>,
    /// For HLS cut at given keyframes, the first segment it does not cut
    /// where the playlist says: its list of keyframes to force stopped
    /// short (`args::forced_through`). Worked out once, when it starts.
    pub aligned_until: Option<u32>,
    /// Its `conversion` span, ended when ffmpeg exits or is killed.
    pub span: Option<tracing::Span>,
}

/// A run decoding in software, in place of one decoding on the GPU.
#[derive(Debug, Clone)]
pub struct Fallback {
    pub args: Vec<String>,
    /// The GPU it still encodes on, if any.
    pub gpu: GpuUse,
    /// The film, as remembered: later runs of it decode in software.
    pub file: PathBuf,
    /// How it makes its video (PX-24).
    pub made: Option<Made>,
}

impl From<Vec<String>> for Run {
    fn from(args: Vec<String>) -> Self {
        Self {
            args,
            ..Self::default()
        }
    }
}
