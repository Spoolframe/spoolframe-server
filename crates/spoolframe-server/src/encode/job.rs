//! One ffmpeg run (P2-03): spawned for a playback, paused when it is far
//! ahead of its reader, killed and cleaned up when dropped.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use spoolframe_media::sandbox::{GpuUse, Jail, Limits, Tool, Vulkan};

use super::JobLimits;
use super::args::{Made, PLAYLIST};
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

    /// The segments of `dir`'s ffmpeg `playlist` past [`Measured::through`]
    /// counted in: their files' sizes and listed durations. A segment
    /// whose file is gone already is passed over, not counted.
    fn count_segments(&mut self, dir: &Path, playlist: &str) {
        for (n, ms) in listed_segments(playlist) {
            if self.through.is_some_and(|through| n <= through) {
                continue;
            }
            if let Ok(meta) = std::fs::metadata(dir.join(format!("seg{n}.m4s"))) {
                self.bytes += meta.len();
                self.ms += ms;
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

/// A share of the encode budget (`transcode.max_jobs`), in quarters of
/// one software video encode: whole numbers, so admission never compares
/// floats.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Units(u32);

impl Units {
    /// What one software video encode weighs: `transcode.max_jobs = 1`.
    pub const PER_JOB: u32 = 4;

    /// `n` software video encodes' worth.
    pub const fn jobs(n: u32) -> Self {
        Self(n.saturating_mul(Self::PER_JOB))
    }

    /// `n` quarters of one.
    pub const fn quarters(n: u32) -> Self {
        Self(n)
    }

    /// In quarters.
    pub const fn get(self) -> u32 {
        self.0
    }

    /// In software video encodes: what the metrics report.
    pub fn as_jobs(self) -> f64 {
        f64::from(self.0) / f64::from(Self::PER_JOB)
    }
}

/// As `max_jobs` is written: `1.25`, `2`.
impl std::fmt::Display for Units {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const { assert!(Units::PER_JOB == 4, "quarters, as the admin API shows them") };
        f.write_str(&spoolframe_admin_api::quarters(self.0))
    }
}

/// What a conversion asks of the machine, which says what it weighs
/// against the encode budget and which cap of its own it counts against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Load {
    /// Every stream copied into another container: disk and a file
    /// descriptor, next to no CPU. Bounded by `transcode.max_remux`.
    Remux,
    /// The video copied (or none), the audio encoded.
    Audio,
    /// The video encoded on a GPU (VA-API, NVENC), with its decode or a
    /// filter (tone mapping, burn-in) on the CPU. Bounded by
    /// `transcode.max_gpu_jobs` too.
    Gpu,
    /// The video encoded in software (libx264, libx265).
    Software,
    /// The video decoded, scaled and encoded on a GPU (VA-API), with no
    /// filter on the CPU. Bounded by `transcode.max_gpu_jobs` too.
    GpuOnly,
}

impl Load {
    pub const ALL: [Load; 5] = [
        Load::Remux,
        Load::Audio,
        Load::Gpu,
        Load::Software,
        Load::GpuOnly,
    ];

    /// What it weighs against `transcode.max_jobs`. Measured on a 32-core
    /// machine, for 60 s of output, a remux took under 1% of the CPU time
    /// of a libx264 encode of the same source and an audio-only encode
    /// about 0.5%. A VA-API encode took 10 to 45% decoding in software,
    /// 1.5 to 26% decoding on the GPU with its frames back in memory, and
    /// 0.4 to 1.7% all on the GPU; one tone-mapping 4K HDR on the CPU
    /// took 70%. An audio encode, and one all on the GPU, weigh more than
    /// they cost, as the least a conversion that encodes can weigh.
    pub fn weight(self) -> Units {
        match self {
            Load::Remux => Units::quarters(0),
            Load::Audio | Load::GpuOnly => Units::quarters(1),
            Load::Gpu => Units::quarters(2),
            Load::Software => Units::jobs(1),
        }
    }

    /// Its name in the metrics' `kind` label.
    pub fn as_str(self) -> &'static str {
        match self {
            Load::Remux => "remux",
            Load::Audio => "audio",
            Load::Gpu => "gpu",
            Load::Software => "software",
            Load::GpuOnly => "gpu_only",
        }
    }

    /// It decodes and encodes a stream again, rather than only copying.
    pub fn encodes(self) -> bool {
        self != Load::Remux
    }
}

/// A conversion's place in the budget. A job that replaces another (a
/// seek, a subtitle change) with the same load shares the place of the
/// one it replaces, so nobody else can take it between the two; it is
/// free once both have let go of it.
#[derive(Debug, Clone)]
pub struct Slot {
    place: Arc<Place>,
}

impl Slot {
    pub fn load(&self) -> Load {
        self.place.load
    }

    /// The GPU card it counts against, for a GPU video encode.
    pub fn card(&self) -> Option<&CardLimit> {
        self.place.card.as_ref()
    }
}

/// A GPU card a GPU video encode counts against (PGPU-05): each card has
/// its own count, and its own limit or `transcode.max_gpu_jobs`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CardLimit {
    /// Which card: `gpus::Card::key`.
    pub key: String,
    /// Its entry's `max_jobs`; `None` for `transcode.max_gpu_jobs`.
    pub max_jobs: Option<usize>,
}

/// The budget's use now, and its limits.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BudgetUse {
    /// Taken by the conversions running now.
    pub used: Units,
    /// `transcode.max_jobs`.
    pub max: Units,
    /// Remuxes running, and `transcode.max_remux`.
    pub remuxes: usize,
    pub max_remux: usize,
    /// GPU video encodes running on every card together, and
    /// `transcode.max_gpu_jobs`: each card's limit unless its entry sets
    /// one (PGPU-05).
    pub gpu_jobs: usize,
    pub max_gpu_jobs: usize,
    /// GPU video encodes running on each card, by `CardLimit::key`. A GPU
    /// encode with no card (none was detected) counts under `""`.
    pub gpu_jobs_by_card: HashMap<String, usize>,
}

/// A card's key in [`BudgetUse::gpu_jobs_by_card`].
fn card_key(card: Option<&CardLimit>) -> &str {
    card.map_or("", |c| c.key.as_str())
}

impl BudgetUse {
    /// Whether a conversion of `load` on `card` may start now.
    /// `replacing`: the load and card of the conversion it replaces, which
    /// still runs but ends once this one has started, so what it holds
    /// counts as free.
    fn admits(
        &self,
        load: Load,
        card: Option<&CardLimit>,
        replacing: Option<(Load, Option<&CardLimit>)>,
    ) -> bool {
        let mut now = self.clone();
        if let Some((old, old_card)) = replacing {
            now.give_back(old, old_card);
        }
        let fits = now.used.0 + load.weight().0 <= now.max.0;
        match load {
            Load::Remux => now.remuxes < now.max_remux,
            Load::Gpu | Load::GpuOnly => fits && now.card_has_room(card),
            Load::Audio | Load::Software => fits,
        }
    }

    /// Whether `card` runs fewer GPU encodes than its limit.
    pub fn card_has_room(&self, card: Option<&CardLimit>) -> bool {
        let running = self
            .gpu_jobs_by_card
            .get(card_key(card))
            .copied()
            .unwrap_or(0);
        let max = card.and_then(|c| c.max_jobs).unwrap_or(self.max_gpu_jobs);
        running < max
    }

    fn take(&mut self, load: Load, card: Option<&CardLimit>) {
        self.used.0 += load.weight().0;
        match load {
            Load::Remux => self.remuxes += 1,
            Load::Gpu | Load::GpuOnly => {
                self.gpu_jobs += 1;
                *self
                    .gpu_jobs_by_card
                    .entry(card_key(card).to_owned())
                    .or_default() += 1;
            }
            Load::Audio | Load::Software => {}
        }
    }

    fn give_back(&mut self, load: Load, card: Option<&CardLimit>) {
        self.used.0 -= load.weight().0;
        match load {
            Load::Remux => self.remuxes -= 1,
            Load::Gpu | Load::GpuOnly => {
                self.gpu_jobs -= 1;
                let key = card_key(card);
                if let Some(running) = self.gpu_jobs_by_card.get_mut(key) {
                    *running -= 1;
                    if *running == 0 {
                        self.gpu_jobs_by_card.remove(key);
                    }
                }
            }
            Load::Audio | Load::Software => {}
        }
    }

    fn set_limits(&mut self, limits: &JobLimits) {
        self.max = limits.budget;
        self.max_remux = limits.max_remux;
        self.max_gpu_jobs = limits.max_gpu_jobs;
    }
}

/// A taken place, given back when dropped. A counter rather than a
/// semaphore: the limits are live settings (P3-10), and a lower one takes
/// effect as jobs end, without stopping any.
#[derive(Debug)]
struct Place {
    budget: Arc<parking_lot::Mutex<BudgetUse>>,
    load: Load,
    /// The card a GPU encode counts against.
    card: Option<CardLimit>,
}

impl Place {
    fn take(
        budget: &Arc<parking_lot::Mutex<BudgetUse>>,
        load: Load,
        card: Option<CardLimit>,
        replacing: Option<&Slot>,
    ) -> Option<Self> {
        // Only a GPU encode counts against a card.
        let card = card.filter(|_| matches!(load, Load::Gpu | Load::GpuOnly));
        let mut now = budget.lock();
        if !now.admits(load, card.as_ref(), replacing.map(|r| (r.load(), r.card()))) {
            return None;
        }
        now.take(load, card.as_ref());
        Some(Self {
            budget: budget.clone(),
            load,
            card,
        })
    }
}

impl Drop for Place {
    fn drop(&mut self) {
        self.budget.lock().give_back(self.load, self.card.as_ref());
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
    limits: parking_lot::RwLock<JobLimits>,
    /// [`JOBS_DIR`] in the scratch directory.
    dir: PathBuf,
    ffmpeg: Tool,
    budget: Arc<parking_lot::Mutex<BudgetUse>>,
    /// Held by the job that looks at a full scratch directory and makes
    /// room in it, so the next job to look sees what the first one freed.
    scratch_check: Arc<tokio::sync::Mutex<()>>,
    /// How many jobs run now, and playbacks being prepared: frame
    /// previews made ahead wait while any is (P3-13, PX-25).
    encoding: Encoding,
    /// Every job started and not yet dropped, to count (P3-14).
    live: parking_lot::Mutex<Vec<std::sync::Weak<EncodeJob>>>,
    counters: Arc<JobCounters>,
    /// Films whose GPU decode failed: they decode in software from then on.
    decode_failures: Arc<DecodeFailures>,
    /// What [`Jobs::first_note`] was told.
    notes: parking_lot::Mutex<HashSet<&'static str>>,
}

/// Films whose GPU decode failed, for as long as the server runs: few,
/// and a restart (a new driver, say) tries again.
#[derive(Debug, Default)]
struct DecodeFailures(parking_lot::Mutex<HashSet<PathBuf>>);

impl DecodeFailures {
    /// Remember `file`: true the first time.
    fn remember(&self, file: &Path) -> bool {
        self.0.lock().insert(file.to_owned())
    }

    fn contains(&self, file: &Path) -> bool {
        self.0.lock().contains(file)
    }
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
    /// Of those, how many of each load, in [`Load::ALL`]'s order.
    pub running_by_load: [u64; 5],
    /// Of those, the ones held back for their reader.
    pub paused: u64,
    pub refused: u64,
    pub stalled: u64,
    /// Seconds of media encoded since the server started.
    pub output_seconds: f64,
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
            scratch_check: Arc::default(),
            encoding: Encoding::default(),
            live: parking_lot::Mutex::default(),
            counters: Arc::default(),
            decode_failures: Arc::default(),
            notes: parking_lot::Mutex::default(),
            limits: parking_lot::RwLock::new(limits),
            dir,
            ffmpeg,
        })
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

    /// Run new jobs under `limits` from now on. Jobs running go on as they
    /// started; with less budget than is taken, none starts until enough
    /// jobs end. The scratch directory stays the one the jobs started in:
    /// it changes only with a restart.
    pub fn set_limits(&self, limits: JobLimits) {
        self.budget.lock().set_limits(&limits);
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
            stalled: self.counters.stalled.get(),
            output_seconds: self.counters.output_ms.get() as f64 / 1000.0,
        }
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
        let slot = match slot {
            Some(slot) if slot.load() == load && slot.card() == card.as_ref() => slot,
            replacing => match Place::take(&self.budget, load, card, replacing.as_ref()) {
                Some(place) => Slot {
                    place: Arc::new(place),
                },
                None => {
                    self.counters.refused.inc();
                    return Err(JobError::Busy);
                }
            },
        };
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
        } = run;
        let ffmpeg = ffmpeg.unwrap_or_else(|| self.ffmpeg.clone());
        let limits = self.limits();
        let cannot_run =
            |e: std::io::Error| JobError::Spawn(format!("{}: {e}", ffmpeg.program().display()));
        // No file of the job's is larger than all it may write.
        let mut jail = Jail::new(Limits::encode(limits.scratch_limit_bytes));
        for dir in reads {
            jail = jail.reads(dir);
        }
        jail = jail.writes(&dir);
        for input in inputs {
            jail = jail.input(input);
        }
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
            paused: AtomicBool::new(false),
            stderr: parking_lot::Mutex::default(),
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
        });
        let mut live = self.live.lock();
        live.retain(|job| job.strong_count() > 0);
        live.push(Arc::downgrade(&job));
        Ok(job)
    }
}

/// ffmpeg with `args`, in `jail`, its output piped to the supervisor.
fn launch(ffmpeg: &Tool, jail: &Jail, args: &[String]) -> std::io::Result<tokio::process::Child> {
    ffmpeg
        .command(jail)?
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
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

/// What the reader side and the supervisor tell each other.
#[derive(Debug)]
struct Shared {
    /// ffmpeg's: its process group's. Another once it runs again
    /// decoding in software.
    pid: AtomicU32,
    /// The program and its arguments, as run.
    command: parking_lot::Mutex<Vec<String>>,
    /// ffmpeg decodes on the GPU.
    hardware_decode: AtomicBool,
    /// How the ffmpeg running now makes its video (PX-24): the fallback's
    /// once it runs.
    made: parking_lot::Mutex<Option<Made>>,
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
    stderr: parking_lot::Mutex<VecDeque<String>>,
}

impl Shared {
    fn stderr_line(&self, line: String) {
        let mut kept = self.stderr.lock();
        if kept.len() == STDERR_KEPT {
            kept.pop_front();
        }
        kept.push_back(line);
    }

    /// The last [`STDERR_LINES`], for a failure's report.
    fn stderr_tail(&self) -> Vec<String> {
        let kept = self.stderr.lock();
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
    /// What it weighs against the encode budget.
    pub load: Load,
    dir: PathBuf,
    written: Written,
    /// Where the run started in the film, ms.
    start_ms: u64,
    progress: watch::Receiver<Progress>,
    /// The supervisor's, while it runs: what [`EncodeJob::end`] says the
    /// job ended with.
    sender: std::sync::Weak<watch::Sender<Progress>>,
    shared: Arc<Shared>,
    task: JoinHandle<()>,
    /// Held while ffmpeg may run; released by [`EncodeJob::stop`], or by
    /// the job itself when it ends a stalled ffmpeg.
    slot: Arc<parking_lot::Mutex<Option<Slot>>>,
}

impl EncodeJob {
    /// Kill ffmpeg now and let go of its slot, though readers may still
    /// hold the job: one replaced at a seek must not encode on.
    pub fn stop(&self) {
        self.task.abort();
        if self.progress.borrow().done.is_none() {
            let pid = self.pid();
            if self.shared.paused.load(Ordering::Relaxed) {
                signal(pid, libc::SIGCONT);
            }
            signal(pid, libc::SIGKILL);
        }
        self.slot.lock().take();
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
        self.shared.command.lock().clone()
    }

    /// ffmpeg's last lines of stderr, oldest first, at most
    /// [`STDERR_KEPT`].
    pub fn stderr(&self) -> Vec<String> {
        self.shared.stderr.lock().iter().cloned().collect()
    }

    /// The job's slot, for the job that replaces it; none once stopped.
    pub fn slot(&self) -> Option<Slot> {
        self.slot.lock().clone()
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn pid(&self) -> u32 {
        self.shared.pid.load(Ordering::Relaxed)
    }

    pub fn progress(&self) -> watch::Receiver<Progress> {
        self.progress.clone()
    }

    /// How its ffmpeg makes the video (PX-24): the run decoding in
    /// software once it fell back to it.
    pub fn made(&self) -> Option<Made> {
        self.shared.made.lock().clone()
    }

    /// Whether its ffmpeg decodes on the GPU: false once it fell back to
    /// software.
    pub fn decodes_in_hardware(&self) -> bool {
        self.shared.hardware_decode.load(Ordering::Relaxed)
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

    /// For HLS, the segment its reader asked for last (0: none yet).
    pub fn reader_segment(&self) -> u32 {
        self.shared.reader_segment.load(Ordering::Relaxed)
    }

    /// For HLS, the last segment it has written, if any.
    pub fn written(&self) -> Option<u32> {
        self.progress.borrow().last_segment
    }

    /// For HLS, whether it has segment `n` on disk now: one it wrote and
    /// has not removed for the scratch space.
    pub fn has_segment(&self, n: u32) -> bool {
        n >= self.first_segment.max(self.first_kept()) && self.written().is_some_and(|w| n <= w)
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
        tracing::debug!(pid = self.pid(), "encode ended");
    }
}

/// `sig` to the run's process group, which `pid` (ffmpeg) leads.
fn signal(pid: u32, sig: libc::c_int) {
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

struct Supervisor {
    /// What ran ffmpeg, and its jail but for the GPU: for the run in
    /// software.
    ffmpeg: Tool,
    jail: Jail,
    /// The run decoding in software, while it has not run.
    fallback: Option<Fallback>,
    decode_failures: Arc<DecodeFailures>,
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
    slot: Arc<parking_lot::Mutex<Option<Slot>>>,
    progress: Arc<watch::Sender<Progress>>,
    counters: Arc<JobCounters>,
    /// Counted in [`Encoding`] for as long as the supervisor runs: until
    /// ffmpeg exits, is ended as stalled, or the job is stopped (which
    /// aborts the supervisor).
    _encoding: EncodingGuard,
}

/// How one ffmpeg ended.
enum Ended {
    Exited(std::io::Result<std::process::ExitStatus>),
    /// No progress for `stalled_after`: killed.
    Stalled,
    /// The scratch directory is full: killed.
    ScratchFull,
    /// ffmpeg could not be run again, decoding in software.
    NotRelaunched,
}

impl Supervisor {
    async fn run(
        mut self,
        child: tokio::process::Child,
        stdout: tokio::process::ChildStdout,
        stderr: tokio::process::ChildStderr,
    ) {
        // The furthest media time counted as output (P3-14): from where the
        // run started, never twice, whichever ffmpeg encoded it.
        let mut counted_ms = self.start_ms;
        let mut ended = self.attempt(child, stdout, stderr, &mut counted_ms).await;
        let decode_failed = matches!(ended, Ended::Exited(Ok(status)) if !status.success())
            || matches!(ended, Ended::Stalled);
        if decode_failed && self.shared.hardware_decode.load(Ordering::Relaxed) {
            if let Some(fallback) = &self.fallback
                && self.decode_failures.remember(&fallback.file)
            {
                tracing::warn!(
                    file = %fallback.file.display(),
                    stderr = ?self.shared.stderr_tail(),
                    "decoding or filtering on the GPU failed; this file is decoded and \
                     filtered on the CPU from now on"
                );
            }
            // Before the first output nothing a reader has changes: the job
            // runs again from the start, decoding in software.
            if self.nothing_written()
                && let Some(fallback) = self.fallback.take()
            {
                match self.relaunch(&fallback) {
                    Ok((child, stdout, stderr)) => {
                        ended = self.attempt(child, stdout, stderr, &mut counted_ms).await;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "cannot run ffmpeg again, decoding in software");
                        ended = Ended::NotRelaunched;
                    }
                }
            }
        }
        self.conclude(ended);
    }

    /// Run ffmpeg again with `fallback`'s arguments, in its place.
    fn relaunch(
        &self,
        fallback: &Fallback,
    ) -> std::io::Result<(
        tokio::process::Child,
        tokio::process::ChildStdout,
        tokio::process::ChildStderr,
    )> {
        let mut child = launch(
            &self.ffmpeg,
            &self.jail.clone().gpu(fallback.gpu),
            &fallback.args,
        )?;
        let pid = child.id().unwrap_or(0);
        self.shared.pid.store(pid, Ordering::Relaxed);
        self.shared.hardware_decode.store(false, Ordering::Relaxed);
        *self.shared.made.lock() = fallback.made.clone();
        self.shared.paused.store(false, Ordering::Relaxed);
        let mut command = vec![self.ffmpeg.program().display().to_string()];
        command.extend(fallback.args.iter().cloned());
        *self.shared.command.lock() = command;
        self.shared.stderr_line(
            "(the GPU failed: running again, decoding and filtering on the CPU)".into(),
        );
        tracing::debug!(pid, dir = %self.dir.display(), "encode running again, decoding in software");
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        Ok((child, stdout, stderr))
    }

    /// Whether the run has written nothing a reader could have: no
    /// finished segment, or not a byte of the file.
    fn nothing_written(&self) -> bool {
        match self.measure() {
            (_, Some(_)) => false,
            (bytes, None) => self.written == Written::Hls || bytes == 0,
        }
    }

    /// How the job ended, said once ffmpeg will not run again.
    fn conclude(&self, ended: Ended) {
        match ended {
            Ended::Exited(Ok(status)) if status.success() => self.finish(Ok(())),
            Ended::Exited(Ok(status)) => {
                let tail = self.shared.stderr_tail();
                let reason = tail
                    .iter()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .cloned()
                    .unwrap_or_else(|| format!("ffmpeg exited with {status}"));
                tracing::warn!(pid = self.pid(), %status, stderr = ?tail, "an encode failed");
                self.finish(Err(Failure {
                    reason,
                    from_ffmpeg: true,
                }));
            }
            Ended::Exited(Err(e)) => {
                self.finish(Err(Failure::server(format!("cannot wait for ffmpeg: {e}"))))
            }
            Ended::NotRelaunched => {
                self.slot.lock().take();
                self.finish(Err(Failure::server(SPAWN_FAILED)));
            }
            Ended::Stalled => {
                self.slot.lock().take();
                self.finish(Err(Failure::server(format!(
                    "the conversion stalled: ffmpeg made no progress for {:?}",
                    self.stalled_after
                ))));
            }
            Ended::ScratchFull => {
                // Of no use now, and in the way of the other jobs for as
                // long as the playback lives.
                if let Err(e) = std::fs::remove_dir_all(&self.dir) {
                    tracing::warn!(dir = %self.dir.display(), error = %e, "cannot remove an encode's files");
                }
                self.finish(Err(Failure::server(
                    "the server's scratch space for conversions is full",
                )));
            }
        }
    }

    fn pid(&self) -> u32 {
        self.shared.pid.load(Ordering::Relaxed)
    }

    /// Watch one ffmpeg until it ends, or is ended.
    async fn attempt(
        &self,
        mut child: tokio::process::Child,
        stdout: tokio::process::ChildStdout,
        stderr: tokio::process::ChildStderr,
        counted_ms: &mut u64,
    ) -> Ended {
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
        let mut tick = tokio::time::interval(POLL);
        let mut out_open = true;
        let mut err_open = true;
        let status = loop {
            tokio::select! {
                line = out.next_line(), if out_open => match line {
                    Ok(Some(line)) => self.progress_line(&line, &mut encoded_ms, counted_ms),
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
                    let output = self.output(bytes, encoded_ms, last_segment);
                    self.progress.send_modify(|p| {
                        p.bytes = bytes;
                        p.encoded_ms = encoded_ms;
                        p.last_segment = last_segment;
                        p.output = output;
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
                            pid = self.pid(),
                            after = ?self.stalled_after,
                            bytes,
                            encoded_ms,
                            stderr = ?self.shared.stderr_tail(),
                            "an encode stalled"
                        );
                        self.counters.stalled.inc();
                        return Ended::Stalled;
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
                            return Ended::ScratchFull;
                        }
                        seen = self.measure().0;
                    }
                }
            }
        };
        // What is left of its progress (a run that ends at once may have
        // written all of it unread), and of stderr for the report.
        while out_open && let Ok(Some(line)) = out.next_line().await {
            self.progress_line(&line, &mut encoded_ms, counted_ms);
        }
        while let Ok(Some(line)) = err.next_line().await {
            self.shared.stderr_line(line);
        }
        let (bytes, last_segment) = self.measure();
        let encoded_ms = self.progress.borrow().encoded_ms.max(encoded_ms);
        let output = self.output(bytes, encoded_ms, last_segment);
        self.progress.send_modify(|p| {
            p.bytes = bytes;
            p.last_segment = last_segment;
            p.encoded_ms = encoded_ms;
            p.output = output;
        });
        Ended::Exited(status)
    }

    /// A line of ffmpeg's `-progress`: key=value; out_time_us is the
    /// encoded media time (out_time_ms is µs too), counted as output past
    /// `counted_ms` (P3-14).
    fn progress_line(&self, line: &str, encoded_ms: &mut u64, counted_ms: &mut u64) {
        if let Some(us) = line.strip_prefix("out_time_us=")
            && let Ok(us) = us.trim().parse::<u64>()
        {
            *encoded_ms = us / 1000;
            if *encoded_ms > *counted_ms {
                self.counters.output_ms.add(*encoded_ms - *counted_ms);
                *counted_ms = *encoded_ms;
            }
        }
    }

    fn finish(&self, done: Result<(), Failure>) {
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
                pid = self.pid(),
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

    /// [`Progress::output`] now, `bytes` and `last_segment` just measured
    /// and `encoded_ms` reported: for HLS, the segments finished since it
    /// last counted added in.
    fn output(&self, bytes: u64, encoded_ms: u64, last_segment: Option<u32>) -> Measured {
        let mut output = self.progress.borrow().output;
        match self.written {
            Written::Progressive { .. } => {
                output.bytes = bytes;
                output.ms = encoded_ms.saturating_sub(self.start_ms);
            }
            Written::Hls => {
                if last_segment > output.through
                    && let Ok(playlist) = std::fs::read_to_string(self.dir.join(PLAYLIST))
                {
                    output.count_segments(&self.dir, &playlist);
                }
            }
        }
        output
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
            signal(self.pid(), libc::SIGSTOP);
            self.shared.paused.store(true, Ordering::Relaxed);
            tracing::debug!(pid = self.pid(), ?ahead, "encode paused for its reader");
        } else if paused && ahead <= self.read_ahead / 2 {
            signal(self.pid(), libc::SIGCONT);
            self.shared.paused.store(false, Ordering::Relaxed);
            tracing::debug!(pid = self.pid(), ?ahead, "encode resumed");
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

/// Each `seg<n>.m4s` an ffmpeg HLS playlist lists, with the duration its
/// `#EXTINF` gives it, ms.
fn listed_segments(playlist: &str) -> Vec<(u32, u64)> {
    let mut listed = Vec::new();
    let mut duration = None;
    for line in playlist.lines() {
        if let Some(info) = line.strip_prefix("#EXTINF:") {
            duration = info
                .split(',')
                .next()
                .and_then(|secs| secs.trim().parse::<f64>().ok())
                .map(|secs| (secs * 1000.0).round() as u64);
        } else if let Some(n) = line
            .strip_prefix("seg")
            .and_then(|rest| rest.strip_suffix(".m4s"))
            .and_then(|n| n.parse().ok())
        {
            listed.push((n, duration.take().unwrap_or(0)));
        }
    }
    listed
}

/// The highest `seg<n>.m4s` an ffmpeg HLS playlist lists.
fn last_listed(playlist: &str) -> Option<u32> {
    playlist
        .lines()
        .filter_map(|line| line.strip_prefix("seg")?.strip_suffix(".m4s")?.parse().ok())
        .max()
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// A budget of `jobs` software video encodes, with the default caps.
    fn budget(jobs: u32) -> Arc<parking_lot::Mutex<BudgetUse>> {
        budget_with(jobs, 32, 3)
    }

    fn budget_with(
        jobs: u32,
        max_remux: usize,
        max_gpu_jobs: usize,
    ) -> Arc<parking_lot::Mutex<BudgetUse>> {
        Arc::new(parking_lot::Mutex::new(BudgetUse {
            max: Units::jobs(jobs),
            max_remux,
            max_gpu_jobs,
            ..BudgetUse::default()
        }))
    }

    fn take(budget: &Arc<parking_lot::Mutex<BudgetUse>>, load: Load) -> Option<Place> {
        Place::take(budget, load, None, None)
    }

    fn card(key: &str, max_jobs: Option<usize>) -> CardLimit {
        CardLimit {
            key: key.into(),
            max_jobs,
        }
    }

    fn take_on(
        budget: &Arc<parking_lot::Mutex<BudgetUse>>,
        load: Load,
        card: &CardLimit,
    ) -> Option<Place> {
        Place::take(budget, load, Some(card.clone()), None)
    }

    /// PGPU-05: `max_gpu_jobs` is each card's, not all of theirs: a full
    /// card leaves the other's room, and an entry's `max_jobs` is its
    /// cards' own limit.
    #[test]
    fn each_card_has_its_own_gpu_cap() {
        let budget = budget_with(8, 32, 2);
        let (xtx, igpu) = (
            card("vaapi:0000:03:00.0", None),
            card("vaapi:0000:6d:00.0", Some(1)),
        );
        let _a = take_on(&budget, Load::Gpu, &xtx).unwrap();
        let _b = take_on(&budget, Load::GpuOnly, &xtx).unwrap();
        assert!(
            take_on(&budget, Load::Gpu, &xtx).is_none(),
            "a third on a card of 2"
        );
        assert!(!budget.lock().card_has_room(Some(&xtx)));
        assert!(budget.lock().card_has_room(Some(&igpu)));
        let c = take_on(&budget, Load::Gpu, &igpu).unwrap();
        assert!(
            take_on(&budget, Load::Gpu, &igpu).is_none(),
            "its entry says 1"
        );
        assert_eq!(budget.lock().gpu_jobs, 3);
        drop(c);
        assert!(budget.lock().card_has_room(Some(&igpu)));
        assert_eq!(
            budget.lock().gpu_jobs_by_card.get("vaapi:0000:6d:00.0"),
            None
        );
        // A software encode counts against no card.
        let _software = take_on(&budget, Load::Software, &xtx).unwrap();
        assert_eq!(budget.lock().gpu_jobs_by_card["vaapi:0000:03:00.0"], 2);
    }

    /// A full card leaves a spilled software encode (`pick::spill`) the
    /// CPU's budget: it starts while `max_jobs` has room, and only then
    /// is a conversion refused as busy.
    #[test]
    fn a_spill_from_a_full_card_runs_on_the_cpu_budget() {
        let budget = budget_with(2, 32, 1);
        let xtx = card("vaapi:0000:03:00.0", None);
        let _gpu = take_on(&budget, Load::Gpu, &xtx).unwrap();
        assert!(
            take_on(&budget, Load::Gpu, &xtx).is_none(),
            "the card is full"
        );
        let _spilled = take(&budget, Load::Software).expect("the CPU has room");
        assert!(
            take(&budget, Load::Software).is_none(),
            "half a job and one more fill 2: busy"
        );
    }

    /// A job that replaces another counts the replaced one's place as
    /// free, on the card it was on.
    #[test]
    fn a_replaced_job_s_card_counts_as_free() {
        let budget = budget_with(8, 32, 1);
        let one = card("nvenc:0000:01:00.0", None);
        let old = Slot {
            place: Arc::new(take_on(&budget, Load::Gpu, &one).unwrap()),
        };
        assert!(take_on(&budget, Load::GpuOnly, &one).is_none());
        let new = Place::take(&budget, Load::GpuOnly, Some(one.clone()), Some(&old));
        assert!(new.is_some(), "the old one's place on the card is free");
    }

    #[test]
    fn a_lower_limit_takes_effect_as_jobs_end_and_a_higher_one_at_once() {
        let budget = budget(2);
        let first = take(&budget, Load::Software).unwrap();
        let second = take(&budget, Load::Software).unwrap();
        assert!(take(&budget, Load::Software).is_none());
        budget.lock().max = Units::jobs(1);
        drop(first);
        // Still one taken, and one is all there is.
        assert!(take(&budget, Load::Software).is_none());
        drop(second);
        let third = take(&budget, Load::Software).unwrap();
        budget.lock().max = Units::jobs(3);
        let _fourth = take(&budget, Load::Software).unwrap();
        drop(third);
    }

    #[test]
    fn four_audio_encodes_fill_a_budget_of_one() {
        let budget = budget(1);
        let four: Vec<Place> = (0..4)
            .map(|n| take(&budget, Load::Audio).unwrap_or_else(|| panic!("audio encode {n}")))
            .collect();
        assert!(take(&budget, Load::Audio).is_none(), "a fifth fits");
        assert_eq!(budget.lock().used, Units::jobs(1));
        drop(four);
        assert_eq!(budget.lock().used, Units::default());
    }

    #[test]
    fn remuxes_weigh_nothing_and_run_up_to_their_own_cap() {
        let budget = budget_with(1, 5, 3);
        let _video = take(&budget, Load::Software).unwrap();
        let remuxes: Vec<Place> = (0..5)
            .map(|n| take(&budget, Load::Remux).unwrap_or_else(|| panic!("remux {n}")))
            .collect();
        assert!(take(&budget, Load::Remux).is_none(), "a sixth remux fits");
        let used = budget.lock().clone();
        assert_eq!((used.used, used.remuxes), (Units::jobs(1), 5));
        drop(remuxes);
        assert_eq!(budget.lock().remuxes, 0);
    }

    #[test]
    fn a_video_encode_needs_a_whole_unit_where_an_audio_one_needs_a_quarter() {
        let budget = budget(1);
        let _three: Vec<Place> = (0..3)
            .map(|_| take(&budget, Load::Audio).unwrap())
            .collect();
        assert_eq!(budget.lock().used, Units::quarters(3));
        assert!(take(&budget, Load::Software).is_none(), "1.75 of 1");
        assert!(take(&budget, Load::Gpu).is_none(), "1.25 of 1");
        assert!(take(&budget, Load::Audio).is_some());
    }

    #[test]
    fn gpu_encodes_weigh_half_and_stop_at_their_own_cap() {
        let budget = budget_with(4, 32, 2);
        let first = take(&budget, Load::Gpu).unwrap();
        let _second = take(&budget, Load::Gpu).unwrap();
        assert_eq!(budget.lock().used, Units::jobs(1));
        // 3 of the budget is free, but the GPU cap is reached.
        assert!(take(&budget, Load::Gpu).is_none());
        let _software = take(&budget, Load::Software).unwrap();
        drop(first);
        assert!(take(&budget, Load::Gpu).is_some());
    }

    /// Decoded, scaled and encoded on the GPU, a conversion takes 1 to 2%
    /// of a software encode's CPU: the least an encode weighs. It is still
    /// a GPU job.
    #[test]
    fn a_conversion_all_on_the_gpu_weighs_a_quarter_under_the_gpu_cap() {
        let budget = budget_with(1, 32, 3);
        let first = take(&budget, Load::GpuOnly).unwrap();
        let _second = take(&budget, Load::GpuOnly).unwrap();
        assert_eq!(budget.lock().used, Units::quarters(2));
        assert_eq!(budget.lock().gpu_jobs, 2);
        // The cap counts both kinds of GPU job.
        let _third = take(&budget, Load::Gpu).unwrap();
        assert_eq!(budget.lock().used, Units::jobs(1));
        drop(first);
        let budget = budget_with(4, 32, 2);
        let _gpu = take(&budget, Load::Gpu).unwrap();
        let _only = take(&budget, Load::GpuOnly).unwrap();
        assert!(take(&budget, Load::GpuOnly).is_none(), "over the GPU cap");
        assert_eq!(budget.lock().gpu_jobs, 2);
    }

    #[test]
    fn a_replacement_of_another_load_counts_what_it_replaces_as_free() {
        let budget = budget(1);
        let audio = Slot {
            place: Arc::new(take(&budget, Load::Audio).unwrap()),
        };
        let _others: Vec<Place> = (0..3)
            .map(|_| take(&budget, Load::Audio).unwrap())
            .collect();
        // A subtitle burned in where an audio encode ran: a software video
        // encode, which fits only once that audio encode's quarter is free.
        assert!(take(&budget, Load::Software).is_none());
        drop(_others);
        let video = Place::take(&budget, Load::Software, None, Some(&audio)).unwrap();
        // Both run for a moment, until the old one stops.
        assert_eq!(budget.lock().used, Units::quarters(5));
        drop(audio);
        assert_eq!(budget.lock().used, Units::jobs(1));
        drop(video);
    }

    #[test]
    fn units_read_as_max_jobs_is_written() {
        assert_eq!(Units::jobs(2).to_string(), "2");
        assert_eq!(Units::quarters(5).to_string(), "1.25");
        assert_eq!(Units::quarters(2).to_string(), "0.5");
        assert_eq!(Units::quarters(7).to_string(), "1.75");
        assert_eq!(Units::quarters(5).as_jobs(), 1.25);
    }

    #[tokio::test]
    async fn new_limits_apply_to_the_next_job_but_not_the_scratch_directory() {
        let tmp = spoolframe_test_dir::TestDir::new("spoolframe-job-limits").await;
        let limits = JobLimits {
            budget: Units::jobs(2),
            max_remux: 32,
            max_gpu_jobs: 3,
            scratch_dir: tmp.as_ref().join("scratch"),
            scratch_limit_bytes: 1 << 30,
            read_ahead: Duration::from_secs(60),
            stalled_after: STALLED_AFTER,
            gpu_filters: false,
            tone_map: spoolframe_media::encoders::ToneMapSetting::Auto,
            video_codecs: crate::config::DEFAULT_VIDEO_CODECS.to_vec(),
        };
        let jobs = Jobs::new(limits.clone(), Tool::unconfined("ffmpeg")).unwrap();
        jobs.set_limits(JobLimits {
            budget: Units::jobs(1),
            max_remux: 1,
            max_gpu_jobs: 1,
            scratch_dir: tmp.as_ref().join("elsewhere"),
            read_ahead: Duration::from_secs(5),
            ..limits.clone()
        });
        let now = jobs.limits();
        assert_eq!(now.budget, Units::jobs(1));
        assert_eq!(now.read_ahead, Duration::from_secs(5));
        assert_eq!(now.scratch_dir, limits.scratch_dir);
        let budget = jobs.budget();
        assert_eq!(
            (budget.max, budget.max_remux, budget.max_gpu_jobs),
            (Units::jobs(1), 1, 1)
        );
        let _taken = take(&jobs.budget, Load::Software).unwrap();
        assert!(take(&jobs.budget, Load::Software).is_none());
        let _remux = take(&jobs.budget, Load::Remux).unwrap();
        assert!(take(&jobs.budget, Load::Remux).is_none());
    }

    #[tokio::test]
    async fn a_note_is_new_once() {
        let (jobs, _dir) = jobs("notes", "exit 0", 1, Duration::from_secs(60)).await;
        assert!(jobs.first_note("the GPU's driver does not tone map"));
        assert!(!jobs.first_note("the GPU's driver does not tone map"));
        assert!(jobs.first_note("text subtitles are burned in on the CPU"));
    }

    #[test]
    fn a_segment_is_finished_once_ffmpeg_lists_it() {
        let playlist = "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-MAP:URI=\"init.mp4\"\n\
                        #EXTINF:6.000000,\nseg4.m4s\n#EXTINF:6.000000,\nseg5.m4s\n";
        assert_eq!(last_listed(playlist), Some(5));
        assert_eq!(last_listed("#EXTM3U\n#EXT-X-VERSION:7\n"), None);
    }

    /// PX-24: the measured bitrate is the finished segments' bytes over
    /// their listed durations, each counted once as the playlist lists it,
    /// though a run from 0 lists only its last.
    #[tokio::test]
    async fn the_measured_bitrate_is_segment_bytes_over_their_durations() {
        let dir = scratch("measured").await;
        let dir = dir.as_ref();
        // Segment 1: 2 s, 500 kB; 2: 2 s, 500 kB; 3: 4 s, 1 MB.
        for (n, bytes) in [(1, 500_000), (2, 500_000), (3, 1_000_000)] {
            std::fs::write(dir.join(format!("seg{n}.m4s")), vec![0u8; bytes]).unwrap();
        }
        let listing = |n: u32, secs: &str| {
            format!(
                "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-MAP:URI=\"init.mp4\"\n\
                 #EXTINF:{secs},\nseg{n}.m4s\n"
            )
        };
        let mut output = Measured::default();
        assert_eq!(output.bitrate(), None, "nothing written yet");
        output.count_segments(dir, &listing(1, "2.000000"));
        output.count_segments(dir, &listing(1, "2.000000"));
        assert_eq!((output.bytes, output.ms), (500_000, 2000), "counted once");
        assert_eq!(output.bitrate(), Some(2_000_000));
        output.count_segments(dir, &listing(2, "2.000000"));
        output.count_segments(dir, &listing(3, "4.000000"));
        // 2 MB over 8 s.
        assert_eq!(output.through, Some(3));
        assert_eq!(output.bitrate(), Some(2_000_000));
        // One with every segment listed counts only the ones after.
        std::fs::write(dir.join("seg4.m4s"), vec![0u8; 3_000_000]).unwrap();
        let all = format!(
            "{}#EXTINF:2.000000,\nseg2.m4s\n#EXTINF:4.000000,\nseg3.m4s\n\
             #EXTINF:4.000000,\nseg4.m4s\n",
            listing(1, "2.000000")
        );
        output.count_segments(dir, &all);
        // 5 MB over 12 s.
        assert_eq!((output.bytes, output.ms), (5_000_000, 12_000));
        assert_eq!(output.bitrate(), Some(3_333_333));
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
    pub(in crate::encode) fn ffmpeg(dir: &Path, body: &str) -> PathBuf {
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
            budget: Units::jobs(max_jobs as u32),
            max_remux: 32,
            max_gpu_jobs: 3,
            scratch_dir: root.join("scratch"),
            scratch_limit_bytes: 1 << 30,
            read_ahead,
            stalled_after,
            gpu_filters: false,
            tone_map: spoolframe_media::encoders::ToneMapSetting::Auto,
            video_codecs: crate::config::DEFAULT_VIDEO_CODECS.to_vec(),
        };
        (Jobs::new(limits, Tool::unconfined(bin)).unwrap(), tmp)
    }

    async fn done(job: &EncodeJob) -> Result<(), String> {
        let mut progress = job.progress();
        let ended = progress
            .wait_for(|p| p.done.is_some())
            .await
            .unwrap()
            .clone();
        ended.done.unwrap().map_err(|f| f.reason)
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
            .start(dir, Vec::new(), Written::Hls, Load::Remux, 0, 0, 0, None)
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
                Load::Remux,
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
                Load::Remux,
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
                Load::Software,
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
                Load::Software,
                0,
                0,
                0,
                None
            ),
            Err(JobError::Busy)
        ));
        let counts = jobs.counts();
        assert_eq!((counts.running, counts.refused), (1, 1));
        assert_eq!(counts.running_by_load, [0, 0, 0, 1, 0], "{counts:?}");
        first.stop();
        jobs.start(
            scratch.join("b/0"),
            Vec::new(),
            Written::Hls,
            Load::Software,
            0,
            0,
            0,
            None,
        )
        .unwrap();
    }

    #[tokio::test]
    /// PX-25: a remux counts as an encode does.
    async fn a_conversion_counts_while_ffmpeg_runs_a_remux_too() {
        let (some, _dir) = jobs("encoding", "exec sleep 30", 3, Duration::from_secs(60)).await;
        let encoding = some.encoding();
        let scratch = some.dir().to_path_buf();
        let copy = some
            .start(
                scratch.join("c/0"),
                Vec::new(),
                Written::Hls,
                Load::Remux,
                0,
                0,
                0,
                None,
            )
            .unwrap();
        assert_eq!(encoding.running(), 1);
        let stopped = some
            .start(
                scratch.join("a/0"),
                Vec::new(),
                Written::Hls,
                Load::Software,
                0,
                0,
                0,
                None,
            )
            .unwrap();
        assert_eq!(encoding.running(), 2);
        stopped.stop();
        copy.stop();
        none_encoding(&encoding).await;
        // One whose ffmpeg exits by itself stops counting then, though
        // the job (its playback) lives on.
        let (quick, _dir) = jobs("encoding-exits", "exit 0", 1, Duration::from_secs(60)).await;
        let encoding = quick.encoding();
        let exited = quick
            .start(
                quick.dir().join("e/0"),
                Vec::new(),
                Written::Hls,
                Load::Software,
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
    pub(crate) async fn eventually(ready: impl Fn() -> bool) {
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
                Load::Remux,
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

    /// A run decoding on the GPU, for `film`, as `jobs` gets it: `hw` as
    /// its last argument, which the stand-in ffmpeg looks for, and no
    /// `hw` in its software fallback.
    fn gpu_run(file: &Path, film: &Path) -> Run {
        let file = file.display().to_string();
        Run {
            args: vec![file.clone(), "hw".into()],
            gpu: GpuUse::Vaapi,
            fallback: Some(Fallback {
                args: vec![file],
                gpu: GpuUse::None,
                file: film.to_owned(),
                made: Some(made_by(crate::encode::Decoder::Software)),
            }),
            made: Some(made_by(crate::encode::Decoder::Hardware(
                spoolframe_media::encoders::Hardware::Vaapi,
            ))),
            ..Run::default()
        }
    }

    /// A 1080p encode decoded by `decoder`, tone-mapped where it decodes.
    fn made_by(decoder: crate::encode::Decoder) -> Made {
        use crate::encode::{Decoder, FilterOn};
        Made {
            decoder,
            encoder: None,
            tone_map: match decoder {
                Decoder::Hardware(_) => FilterOn::Gpu,
                _ => FilterOn::Cpu,
            },
            tone_mapper: None,
            burn_in: FilterOn::None,
            size: Some((1920, 1080)),
            target_bitrate: None,
        }
    }

    /// The stand-in: `on_gpu` when the last argument is `hw`, else writes
    /// `abc` to the first and reports a second encoded.
    fn decoding(on_gpu: &str) -> String {
        format!(
            "for a; do last=$a; done\n\
             if [ \"$last\" = hw ]; then {on_gpu}; fi\n\
             printf abc > \"$1\"; echo out_time_us=1000000"
        )
    }

    /// A GPU decode the driver refuses (10-bit H.264, an AV1 profile) fails
    /// before ffmpeg writes anything: the same job runs again decoding in
    /// software, and the film is remembered, so its next run decodes in
    /// software from the start.
    #[tokio::test]
    async fn a_gpu_decode_that_fails_before_any_output_runs_again_in_software() {
        let (jobs, tmp) = jobs(
            "gpu-decode-fails",
            &decoding(
                "echo 'Impossible to convert between the formats supported by the filter' >&2; \
                 exit 218",
            ),
            1,
            Duration::from_secs(60),
        )
        .await;
        let film = tmp.as_ref().join("film.mkv");
        let dir = jobs.dir().join("h/0");
        let file = dir.join("out.mkv");
        let job = jobs
            .start(
                dir,
                gpu_run(&file, &film),
                Written::Progressive { file: file.clone() },
                Load::Gpu,
                0,
                0,
                0,
                None,
            )
            .unwrap();
        assert_eq!(done(&job).await, Ok(()));
        assert_eq!(job.progress().borrow().bytes, 3);
        assert!(jobs.hardware_decode_failed(&film));
        assert!(!jobs.hardware_decode_failed(&tmp.as_ref().join("other.mkv")));
        assert!(!job.decodes_in_hardware());
        // PX-24: what playback debug says is made now is the run in
        // software's.
        assert_eq!(job.made(), Some(made_by(crate::encode::Decoder::Software)));
        // What ran last is what playback debug shows; what the GPU run
        // said is kept above it.
        assert_eq!(
            job.command(),
            [
                jobs.ffmpeg.program().display().to_string(),
                file.display().to_string()
            ]
        );
        assert!(
            job.stderr()
                .iter()
                .any(|l| l.starts_with("Impossible to convert")),
            "{:?}",
            job.stderr()
        );
    }

    /// One that fails after it wrote is not run again from the start: what
    /// its reader has would change under it. It fails, and the film is
    /// remembered, so the playback's next run (a seek, the next segment)
    /// decodes in software.
    #[tokio::test]
    async fn a_gpu_decode_that_fails_after_its_first_output_fails_and_is_remembered() {
        let (jobs, tmp) = jobs(
            "gpu-decode-fails-late",
            &decoding("printf abc > \"$1\"; echo 'hardware decoding failed' >&2; exit 1"),
            1,
            Duration::from_secs(60),
        )
        .await;
        let film = tmp.as_ref().join("film.mkv");
        let dir = jobs.dir().join("h/0");
        let file = dir.join("out.mkv");
        let job = jobs
            .start(
                dir,
                gpu_run(&file, &film),
                Written::Progressive { file: file.clone() },
                Load::Gpu,
                0,
                0,
                0,
                None,
            )
            .unwrap();
        assert_eq!(done(&job).await.unwrap_err(), "hardware decoding failed");
        assert!(jobs.hardware_decode_failed(&film));
        assert!(job.decodes_in_hardware());
    }

    /// A GPU decode that hangs before its first output is ended as stalled
    /// and run again in software, rather than holding the playback.
    #[tokio::test]
    async fn a_gpu_decode_that_stalls_before_any_output_runs_again_in_software() {
        let (jobs, tmp) = jobs_stalled_after(
            "gpu-decode-stalls",
            &decoding("exec sleep 30"),
            1,
            Duration::from_secs(60),
            Duration::from_millis(600),
        )
        .await;
        let film = tmp.as_ref().join("film.mkv");
        let dir = jobs.dir().join("h/0");
        let file = dir.join("out.mkv");
        let job = jobs
            .start(
                dir,
                gpu_run(&file, &film),
                Written::Progressive { file: file.clone() },
                Load::Gpu,
                0,
                0,
                0,
                None,
            )
            .unwrap();
        let hung = job.pid();
        assert_eq!(done(&job).await, Ok(()));
        assert_ne!(job.pid(), hung);
        assert!(!alive(hung), "the hung ffmpeg {hung} still runs");
        assert!(jobs.hardware_decode_failed(&film));
        // The hung run is counted as a stall all the same.
        assert_eq!(jobs.counts().stalled, 1);
    }

    /// The software run is the job's ffmpeg in every way: held back for its
    /// reader, and killed with the job.
    #[tokio::test]
    async fn the_software_run_is_throttled_and_killed_as_the_first_would_be() {
        let (jobs, tmp) = jobs(
            "gpu-decode-fallback-supervised",
            "for a; do last=$a; done\n\
             if [ \"$last\" = hw ]; then exit 218; fi\n\
             head -c 100 /dev/zero > \"$1\"; echo out_time_us=60000000; exec sleep 30",
            1,
            Duration::from_secs(10),
        )
        .await;
        let film = tmp.as_ref().join("film.mkv");
        let dir = jobs.dir().join("h/0");
        let file = dir.join("out.mkv");
        let job = jobs
            .start(
                dir,
                gpu_run(&file, &film),
                Written::Progressive { file },
                Load::Gpu,
                0,
                0,
                0,
                None,
            )
            .unwrap();
        let first = job.pid();
        eventually(|| job.pid() != first && stopped(job.pid())).await;
        assert!(job.paused());
        let pid = job.pid();
        drop(job);
        eventually(|| !alive(pid)).await;
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
                Load::Remux,
                0,
                0,
                0,
                None,
            )
            .unwrap();
        let mut expected = vec![jobs.ffmpeg.program().display().to_string()];
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
                Load::Software,
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
            Load::Remux,
            0,
            0,
            0,
            None,
        );
        assert!(other.is_ok(), "the ended job still holds its place");
        // The ended job no longer counts for previews; the new remux does.
        eventually(|| jobs.encoding().running() == 1).await;
    }

    #[tokio::test]
    async fn dropping_a_job_kills_ffmpeg_and_removes_its_files() {
        let (jobs, _dir) = jobs("drop", "exec sleep 30", 2, Duration::from_secs(60)).await;
        let dir = jobs.dir().join("h/0");
        let job = jobs
            .start(
                dir.clone(),
                Vec::new(),
                Written::Hls,
                Load::Remux,
                0,
                0,
                0,
                None,
            )
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
                Load::Remux,
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
                Load::Remux,
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
            .start(dir, args, Written::Hls, Load::Remux, 0, 1, 0, None)
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
                Load::Remux,
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
                Load::Software,
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
            Load::Software,
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
                Load::Remux,
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
            .start(dir, args, Written::Hls, Load::Remux, 0, 0, 0, None)
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
            budget: Units::jobs(2),
            max_remux: 32,
            max_gpu_jobs: 3,
            scratch_dir: root.join("scratch"),
            scratch_limit_bytes: scratch_limit,
            read_ahead: Duration::from_secs(60),
            stalled_after: STALLED_AFTER,
            gpu_filters: false,
            tone_map: spoolframe_media::encoders::ToneMapSetting::Auto,
            video_codecs: crate::config::DEFAULT_VIDEO_CODECS.to_vec(),
        };
        (Jobs::new(limits, Tool::unconfined(bin)).unwrap(), tmp)
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
                jobs.start(dir, args, written, Load::Remux, 0, 0, 0, None)
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
            .start(
                dir.clone(),
                Vec::new(),
                Written::Hls,
                Load::Remux,
                0,
                1,
                0,
                None,
            )
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
            .start(dir.clone(), args, Written::Hls, Load::Remux, 0, 1, 0, None)
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
            budget: Units::jobs(1),
            max_remux: 32,
            max_gpu_jobs: 3,
            scratch_dir: root.join("scratch"),
            scratch_limit_bytes: 1 << 30,
            read_ahead: Duration::from_secs(60),
            stalled_after: STALLED_AFTER,
            gpu_filters: false,
            tone_map: spoolframe_media::encoders::ToneMapSetting::Auto,
            video_codecs: crate::config::DEFAULT_VIDEO_CODECS.to_vec(),
        };
        let jobs = Jobs::new(limits, Tool::unconfined("ffmpeg")).unwrap();
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
            budget: Units::jobs(1),
            max_remux: 32,
            max_gpu_jobs: 3,
            scratch_dir: root.join("state"),
            scratch_limit_bytes: 1 << 30,
            read_ahead: Duration::from_secs(60),
            stalled_after: STALLED_AFTER,
            gpu_filters: false,
            tone_map: spoolframe_media::encoders::ToneMapSetting::Auto,
            video_codecs: crate::config::DEFAULT_VIDEO_CODECS.to_vec(),
        };
        Jobs::new(limits, Tool::unconfined("ffmpeg")).unwrap();
        assert_eq!(std::fs::read(&key).unwrap(), b"secret");
        assert_eq!(std::fs::read(&text).unwrap(), b"WEBVTT");
    }
}
