//! Playback sessions (P1-08): what a `StartPlayback` leaves behind, so
//! that range reads need no database lookups.
//!
//! A handle is bound to the session and the endpoint that started it. It
//! lives in memory; it holds no open file (each read opens the file), so an
//! idle handle costs a map entry until it expires.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use std::sync::Arc;

use password_hash::rand_core::{OsRng, RngCore};
use spoolframe_protocol::{PlaybackHandle, PlaybackPlan, Selection, SessionId};
use spoolframe_types::{Delivery, Item, MediaId, Platform, SubtitleFormat};
use tokio::sync::watch;

use crate::encode::Conversion;
use crate::metrics::Counter;
use crate::subtitles::TextState;

/// How long a handle lives without reads.
pub const IDLE: Duration = Duration::from_secs(5 * 60);
/// How long a device's playbacks outlive its last connection: long enough
/// for a reconnect (a network change, an app brought back), short of
/// holding an encode slot for [`IDLE`] for an app that was killed.
pub const DISCONNECT_GRACE: Duration = Duration::from_secs(30);
/// How often, while a handle is used, its session is checked again, so a
/// revoked session or a disabled user stops playback promptly.
pub const RECHECK: Duration = Duration::from_secs(30);
/// How far back a playback's reads are kept, for playback debug's chart
/// (P3-15).
pub const READS_KEPT: Duration = Duration::from_secs(10 * 60);
/// At most one read point per this long: reads come many a second.
const READ_POINT_EVERY_MS: i64 = 1000;
/// How long an ended playback is listed (P3-15).
pub const ENDED_KEPT: Duration = Duration::from_secs(60 * 60);
/// The most ended playbacks kept, however many ended in the hour.
const ENDED_MAX: usize = 200;

/// Unix milliseconds now.
fn unix_ms() -> i64 {
    crate::metrics::history::unix_ms()
}

/// Who plays a playback, as playback debug names them (P3-15).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Viewer {
    /// `local:<uuid>`.
    pub user_id: String,
    pub username: String,
    /// The display name, or the username.
    pub display_name: String,
    /// What the device calls itself (its `Hello`).
    pub device_name: Option<String>,
    /// Its app, e.g. `spoolframe-linux`.
    pub client_name: String,
    pub platform: Platform,
}

impl Viewer {
    /// What to call the device: its own name, else its app's.
    pub fn device(&self) -> String {
        self.device_name
            .clone()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| self.client_name.clone())
    }
}

/// What a playback plays, as a person reads it (P3-15).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Shown {
    /// `Alien`; `Severance · S2 E4`; a track's title.
    pub title: String,
    /// An episode's own title; a track's artist.
    pub subtitle: Option<String>,
    pub duration_ms: Option<u64>,
}

impl Shown {
    /// `item` as the Playback page names it.
    pub fn of(item: &Item) -> Self {
        let ms = |d: Option<Duration>| d.map(|d| d.as_millis() as u64);
        match item {
            Item::Movie(m) => Shown {
                title: m.title.clone(),
                subtitle: None,
                duration_ms: ms(m.duration),
            },
            Item::Episode(e) => Shown {
                title: match &e.show_title {
                    Some(show) => format!("{show} · S{} E{}", e.season, e.episode),
                    None => format!("S{} E{}", e.season, e.episode),
                },
                subtitle: Some(e.title.clone()),
                duration_ms: ms(e.duration),
            },
            Item::Track(t) => Shown {
                title: t.title.clone(),
                subtitle: t.artist_name.clone(),
                duration_ms: ms(t.duration),
            },
            other => Shown {
                title: other.id().to_string(),
                ..Shown::default()
            },
        }
    }
}

/// The furthest a playback has read in its encode now (its epoch): bytes
/// into a file, or an HLS segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Furthest {
    Byte(u64),
    Segment(u32),
}

/// One point of a playback's reads over time (P3-15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadPoint {
    /// Unix ms.
    pub at: i64,
    /// Where in the film the read was, ms, when that can be told.
    pub read_ms: Option<u64>,
    /// How far its conversion had got, ms; `None` for direct play.
    pub converted_ms: Option<u64>,
}

/// What a playback's reads and its client's reports have been: shared
/// by the clones a read takes, so a read records into the registry's own.
#[derive(Debug, Default)]
struct Trace {
    reads: VecDeque<ReadPoint>,
    furthest: Option<(u32, Furthest)>,
    last_read: Option<i64>,
    /// What the client last reported, and when (Unix ms).
    position: Option<(u64, i64)>,
}

impl Trace {
    fn read(&mut self, point: ReadPoint, epoch: u32, mark: Furthest) {
        self.last_read = Some(point.at);
        self.furthest = Some(match self.furthest {
            Some((at, before)) if at == epoch => (
                epoch,
                match (before, mark) {
                    (Furthest::Byte(a), Furthest::Byte(b)) => Furthest::Byte(a.max(b)),
                    (Furthest::Segment(a), Furthest::Segment(b)) => Furthest::Segment(a.max(b)),
                    (_, mark) => mark,
                },
            ),
            _ => (epoch, mark),
        });
        match self.reads.back_mut() {
            Some(last) if point.at - last.at < READ_POINT_EVERY_MS => {
                *last = ReadPoint {
                    at: last.at,
                    ..point
                }
            }
            _ => self.reads.push_back(point),
        }
        let since = point.at - READS_KEPT.as_millis() as i64;
        while self.reads.front().is_some_and(|p| p.at < since) {
            self.reads.pop_front();
        }
    }
}

/// A selected subtitle the client fetches whole.
#[derive(Debug, Clone)]
pub enum Sidecar {
    /// A file next to the media file (P1-08).
    File {
        path: PathBuf,
        format: SubtitleFormat,
    },
    /// Text the server makes from a track (P2-07): a bitmap track read as
    /// text, or a text track converted; there once it is made.
    Made {
        state: watch::Receiver<TextState>,
        format: SubtitleFormat,
    },
}

/// The file a direct playback reads, as it was when the playback started:
/// a read opens the path again, and is refused if the path names another
/// file by then (PX-09 B-1), such as a symlink swapped in by someone who
/// can write to the library folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileId {
    dev: u64,
    ino: u64,
}

impl FileId {
    pub fn of(metadata: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }
}

/// One playback.
#[derive(Debug, Clone)]
pub struct Playback {
    /// What playback debug (P3-15) names it by: random, and never the
    /// handle, which is a secret of the session's.
    pub id: String,
    pub viewer: Viewer,
    pub shown: Shown,
    /// The plan it plays by, as the client got it.
    pub planned: PlaybackPlan,
    /// Unix ms.
    pub started_at: i64,
    trace: Arc<Mutex<Trace>>,
    pub revision: u64,
    pub progress_sequence: u64,
    progress_superseded: bool,
    pub session: SessionId,
    /// Hex `EndpointId`.
    pub endpoint: String,
    /// What is played, and with which tracks: a subtitle change re-plans
    /// it (P2-07).
    pub item: MediaId,
    pub selection: Selection,
    pub path: PathBuf,
    pub size: u64,
    /// For direct play, the file `path` named at the start.
    pub file: Option<FileId>,
    pub subtitle: Option<Sidecar>,
    /// For a converted playback, its encode (P2-03). Ending the handle
    /// drops it, which stops ffmpeg and removes its files.
    pub conversion: Option<Arc<Conversion>>,
    last_used: Instant,
    last_checked: Instant,
}

impl Playback {
    /// How it is served.
    pub fn plan(&self) -> Plan {
        match &self.conversion {
            None => Plan::Direct,
            Some(c) if c.encodes() => Plan::Transcode,
            Some(_) => Plan::Remux,
        }
    }
}

/// A playback to open.
#[derive(Debug)]
pub struct Opened {
    pub viewer: Viewer,
    pub shown: Shown,
    pub planned: PlaybackPlan,
    pub session: SessionId,
    pub endpoint: String,
    pub item: MediaId,
    pub selection: Selection,
    pub path: PathBuf,
    pub size: u64,
    pub file: Option<FileId>,
    pub subtitle: Option<Sidecar>,
    pub conversion: Option<Arc<Conversion>>,
}

/// What a playback becomes when its subtitle changes.
#[derive(Debug)]
pub struct Changed {
    pub planned: PlaybackPlan,
    pub selection: Selection,
    pub size: u64,
    pub file: Option<FileId>,
    pub subtitle: Option<Sidecar>,
    pub conversion: Option<Arc<Conversion>>,
}

/// Why a handle cannot be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Unknown, or idle past [`IDLE`].
    Expired,
    /// From another session or another endpoint.
    NotYours,
    /// Stopped by an admin (P3-15): over, not to be started again.
    Stopped,
}

/// What a use of a handle needs to do.
#[derive(Debug, Clone)]
pub struct Use {
    pub playback: Playback,
    /// The session must be checked again before serving.
    pub recheck: bool,
}

#[derive(Debug)]
pub struct Playbacks {
    idle: Duration,
    recheck: Duration,
    disconnect_grace: Duration,
    handles: Mutex<HashMap<String, Playback>>,
    controls: crate::keyed_locks::KeyedLocks,
    progress_controls: crate::keyed_locks::KeyedLocks,
    /// Opened since the server started (P3-14).
    started: Counter,
    /// Ended since the server started, however they ended.
    ended: Counter,
    /// The last hour's ended playbacks, newest last (P3-15).
    gone: Mutex<VecDeque<EndedPlayback>>,
    /// Handles an admin stopped, and when: a client still using one is
    /// told so, not that it expired (P3-15). Kept as long as an idle
    /// handle would have lived.
    stopped: Mutex<HashMap<String, Instant>>,
    /// What idling out and rechecks are timed by (PX-09).
    clock: crate::clock::Clock,
}

/// How a playback ended (P3-15).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndReason {
    /// Its client stopped it.
    Finished,
    /// An admin stopped it.
    StoppedByAdmin,
    /// No reads for [`IDLE`].
    Idle,
    /// Its device started another conversion.
    Replaced,
    /// Its device was gone longer than the grace.
    DeviceGone,
    /// Its session ended, or its user was disabled.
    SignedOut,
    /// Its conversion failed, with ffmpeg's reason.
    Failed(String),
}

/// One playback as playback debug shows it (P3-15): read from the
/// registry, its conversion and its reads at one moment.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub id: String,
    pub viewer: Viewer,
    pub item: MediaId,
    /// The media file it reads.
    pub path: PathBuf,
    pub shown: Shown,
    pub planned: PlaybackPlan,
    pub kind: Plan,
    pub selection: Selection,
    pub revision: u64,
    /// The encode's epoch; 0 for direct play.
    pub epoch: u32,
    /// Unix ms.
    pub started_at: i64,
    /// What the client last reported, ms.
    pub position_ms: Option<u64>,
    /// Where in the film the last read was, ms.
    pub read_to_ms: Option<u64>,
    pub furthest: Option<Furthest>,
    /// Unix ms.
    pub last_read_at: Option<i64>,
    /// The last [`READS_KEPT`], oldest first.
    pub reads: Vec<ReadPoint>,
    pub conversion: Option<ConversionSnapshot>,
}

/// A playback's conversion at one moment (P3-15).
#[derive(Debug, Clone)]
pub struct ConversionSnapshot {
    pub delivery: Delivery,
    /// How far in the film the encode has got, ms.
    pub converted_ms: u64,
    pub bytes: u64,
    pub last_segment: Option<u32>,
    /// Held back by the throttle for its reader.
    pub paused: bool,
    /// ffmpeg runs (paused or not).
    pub running: bool,
    /// Ended by an admin.
    pub ended: bool,
    /// ffmpeg's failure, when it failed.
    pub failed: Option<String>,
    /// The command line, with every token hidden ([`crate::redact`]).
    pub command: String,
    /// ffmpeg's last stderr lines.
    pub stderr: Vec<String>,
    /// It encodes a stream, rather than only copying.
    pub encodes: bool,
}

/// A playback that ended in the last hour (P3-15), as it was at the end.
#[derive(Debug, Clone)]
pub struct EndedPlayback {
    /// Unix ms.
    pub at: i64,
    pub why: EndReason,
    pub last: Snapshot,
}

/// A playback an admin asks for (P3-15).
#[derive(Debug, Clone)]
pub enum Found {
    Open(Snapshot),
    Ended(EndedPlayback),
}

/// Why an admin's End conversion did nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndConversionError {
    /// No open playback has that id.
    NotFound,
    /// It plays the file directly.
    NoConversion,
}

impl Playback {
    /// Playback debug's view of it now. `handle` is its own, hidden in the
    /// command line shown.
    fn snapshot(&self, handle: &str) -> Snapshot {
        let trace = self.trace.lock().expect("never held across a panic");
        let conversion = self.conversion.as_ref().map(|c| {
            let job = c.job();
            let progress = job.progress().borrow().clone();
            ConversionSnapshot {
                delivery: c.delivery,
                converted_ms: progress.encoded_ms,
                bytes: progress.bytes,
                last_segment: progress.last_segment,
                paused: job.paused(),
                running: progress.done.is_none() && !c.ended(),
                ended: c.ended(),
                failed: progress.done.and_then(Result::err).map(|f| f.reason),
                command: crate::redact::command_line(&job.command(), &[handle]),
                stderr: job
                    .stderr()
                    .iter()
                    .map(|l| crate::redact::hide(l, &[handle]))
                    .collect(),
                encodes: c.encodes(),
            }
        });
        Snapshot {
            id: self.id.clone(),
            viewer: self.viewer.clone(),
            item: self.item.clone(),
            path: self.path.clone(),
            shown: self.shown.clone(),
            planned: self.planned.clone(),
            kind: self.plan(),
            selection: self.selection,
            revision: self.revision,
            epoch: self.conversion.as_ref().map_or(0, |c| c.job().epoch),
            started_at: self.started_at,
            position_ms: trace.position.map(|(ms, _)| ms),
            read_to_ms: trace.reads.back().and_then(|p| p.read_ms),
            furthest: trace.furthest.map(|(_, f)| f),
            last_read_at: trace.last_read,
            reads: trace.reads.iter().copied().collect(),
            conversion,
        }
    }
}

/// A random id for playback debug: 8 hex digits.
fn debug_id() -> String {
    format!("{:08x}", OsRng.next_u32())
}

/// How a playback is served (P3-14): the file as it is, a conversion that
/// only copies its streams into another container, or one that encodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Plan {
    Direct,
    Remux,
    Transcode,
}

impl Plan {
    pub const ALL: [Plan; 3] = [Plan::Direct, Plan::Remux, Plan::Transcode];

    pub fn as_str(self) -> &'static str {
        match self {
            Plan::Direct => "direct",
            Plan::Remux => "remux",
            Plan::Transcode => "transcode",
        }
    }
}

/// The playbacks open now, by plan, and how many opened and ended since
/// the server started.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlaybackCounts {
    pub direct: u64,
    pub remux: u64,
    pub transcode: u64,
    pub started: u64,
    pub ended: u64,
}

impl PlaybackCounts {
    pub fn active(&self, plan: Plan) -> u64 {
        match plan {
            Plan::Direct => self.direct,
            Plan::Remux => self.remux,
            Plan::Transcode => self.transcode,
        }
    }
}

impl Default for Playbacks {
    fn default() -> Self {
        Self::new(IDLE, RECHECK)
    }
}

impl Playbacks {
    pub fn new(idle: Duration, recheck: Duration) -> Self {
        Self {
            idle,
            recheck,
            disconnect_grace: DISCONNECT_GRACE,
            handles: Mutex::default(),
            controls: Default::default(),
            progress_controls: Default::default(),
            started: Counter::default(),
            ended: Counter::default(),
            gone: Mutex::default(),
            stopped: Mutex::default(),
            clock: crate::clock::Clock::default(),
        }
    }

    /// Time idling and rechecks by `clock` (tests move it instead of
    /// sleeping).
    pub fn with_clock(mut self, clock: crate::clock::Clock) -> Self {
        self.clock = clock;
        self
    }

    /// What is open now, and what opened and ended (P3-14). A playback
    /// idle past the limit counts as open until the next sweep ends it.
    pub fn counts(&self) -> PlaybackCounts {
        let mut counts = PlaybackCounts {
            started: self.started.get(),
            ended: self.ended.get(),
            ..PlaybackCounts::default()
        };
        for playback in self.lock().values() {
            match playback.plan() {
                Plan::Direct => counts.direct += 1,
                Plan::Remux => counts.remux += 1,
                Plan::Transcode => counts.transcode += 1,
            }
        }
        counts
    }

    /// Playbacks end `grace` after their device's last connection closes
    /// ([`DISCONNECT_GRACE`]; tests shorten it).
    pub fn with_disconnect_grace(mut self, grace: Duration) -> Self {
        self.disconnect_grace = grace;
        self
    }

    pub fn disconnect_grace(&self) -> Duration {
        self.disconnect_grace
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Playback>> {
        self.handles.lock().expect("never held across a panic")
    }

    /// Why `handle`, which is not open, cannot be used: an admin stopped
    /// it, or it is unknown or idled out.
    fn gone(&self, handle: &str) -> Refusal {
        let mut stopped = self.stopped.lock().expect("never held across a panic");
        let now = self.clock.now();
        stopped.retain(|_, at| now.saturating_duration_since(*at) < self.idle);
        if stopped.contains_key(handle) {
            Refusal::Stopped
        } else {
            Refusal::Expired
        }
    }

    /// Serialize changes to this handle. The caller must look up the
    /// playback again after acquiring this guard: a preceding operation
    /// may have replaced its conversion or stopped it entirely.
    pub(crate) async fn control(
        &self,
        handle: &PlaybackHandle,
        session: &SessionId,
        endpoint: &str,
    ) -> Result<crate::keyed_locks::Guard<'_>, Refusal> {
        {
            let handles = self.lock();
            let Some(playback) = handles.get(&handle.0) else {
                drop(handles);
                return Err(self.gone(&handle.0));
            };
            if &playback.session != session || playback.endpoint != endpoint {
                return Err(Refusal::NotYours);
            }
        }
        Ok(self.controls.lock(&handle.0).await)
    }

    /// A new, unguessable handle, before its playback is opened: a
    /// conversion names its files after it.
    pub fn new_handle() -> PlaybackHandle {
        let mut bytes = [0u8; 16];
        OsRng.fill_bytes(&mut bytes);
        PlaybackHandle(bytes.iter().map(|b| format!("{b:02x}")).collect())
    }

    /// Open `opened` as `handle`; the session was checked by the caller
    /// just now.
    pub fn start(&self, handle: PlaybackHandle, opened: Opened) -> PlaybackHandle {
        let now = self.clock.now();
        let mut handles = self.lock();
        // Forget idle handles here rather than on a timer.
        let idle: Vec<String> = handles
            .iter()
            .filter(|(_, p)| now.duration_since(p.last_used) >= self.idle)
            .map(|(h, _)| h.clone())
            .collect();
        let idle: Vec<(String, Playback)> = idle
            .into_iter()
            .filter_map(|h| handles.remove_entry(&h))
            .collect();
        self.started.inc();
        for previous in handles.values_mut() {
            if previous.session == opened.session && previous.item == opened.item {
                previous.progress_superseded = true;
            }
        }
        handles.insert(
            handle.0.clone(),
            Playback {
                id: debug_id(),
                viewer: opened.viewer,
                shown: opened.shown,
                planned: opened.planned,
                started_at: unix_ms(),
                trace: Arc::default(),
                revision: 0,
                progress_sequence: 0,
                progress_superseded: false,
                session: opened.session,
                endpoint: opened.endpoint,
                item: opened.item,
                selection: opened.selection,
                path: opened.path,
                size: opened.size,
                file: opened.file,
                subtitle: opened.subtitle,
                conversion: opened.conversion,
                last_used: now,
                last_checked: now,
            },
        );
        drop(handles);
        self.ended_all(idle, EndReason::Idle);
        handle
    }

    /// Play `handle` as `changed` from now (a subtitle change, P2-07). The
    /// conversion it replaces is returned, to be dropped outside the lock:
    /// dropping it stops its encode and removes its files.
    pub fn change(&self, handle: &PlaybackHandle, changed: Changed) -> Option<Arc<Conversion>> {
        let mut handles = self.lock();
        let playback = handles.get_mut(&handle.0)?;
        playback.revision += 1;
        playback.planned = changed.planned;
        playback.selection = changed.selection;
        playback.size = changed.size;
        playback.file = changed.file;
        playback.subtitle = changed.subtitle;
        std::mem::replace(&mut playback.conversion, changed.conversion)
    }

    /// Use `handle` from `session` on `endpoint`: refused if it is not
    /// theirs or has expired, and otherwise marked used.
    pub fn use_handle(
        &self,
        handle: &PlaybackHandle,
        session: &SessionId,
        endpoint: &str,
    ) -> Result<Use, Refusal> {
        let now = self.clock.now();
        let mut handles = self.lock();
        let Some(playback) = handles.get_mut(&handle.0) else {
            drop(handles);
            return Err(self.gone(&handle.0));
        };
        if &playback.session != session || playback.endpoint != endpoint {
            return Err(Refusal::NotYours);
        }
        if now.duration_since(playback.last_used) >= self.idle {
            let ended = handles.remove_entry(&handle.0);
            drop(handles);
            self.ended_all(ended, EndReason::Idle);
            return Err(Refusal::Expired);
        }
        playback.last_used = now;
        let recheck = now.duration_since(playback.last_checked) >= self.recheck;
        if recheck {
            playback.last_checked = now;
        }
        Ok(Use {
            playback: playback.clone(),
            recheck,
        })
    }

    /// What `handle` plays, if it is open: for the log, which never names
    /// the handle itself. Not a use of it.
    pub fn item(&self, handle: &PlaybackHandle) -> Option<MediaId> {
        self.lock().get(&handle.0).map(|p| p.item.clone())
    }

    /// Mark `handle` used now, as a read that is still waiting for an
    /// encode does, so the playback does not idle out under it.
    pub fn touch(&self, handle: &PlaybackHandle) {
        if let Some(p) = self.lock().get_mut(&handle.0) {
            p.last_used = self.clock.now();
        }
    }

    /// `handle` read `sent` bytes at `offset`, of HLS segment `segment` if
    /// it names one (P3-15): where that is in the film, and how far its
    /// conversion had got, go on its chart.
    pub fn read(&self, handle: &PlaybackHandle, offset: u64, sent: u64, segment: Option<u32>) {
        let Some(playback) = self.lock().get(&handle.0).cloned() else {
            return;
        };
        let (epoch, read_ms, converted_ms, mark) = match &playback.conversion {
            None => {
                let to = offset + sent;
                let read_ms = playback
                    .shown
                    .duration_ms
                    .filter(|_| playback.size > 0)
                    .map(|d| (u128::from(d) * u128::from(to) / u128::from(playback.size)) as u64);
                (0, read_ms, None, Furthest::Byte(to))
            }
            Some(conversion) => {
                let job = conversion.job();
                let converted = job.progress().borrow().encoded_ms;
                match segment {
                    Some(n) => (
                        job.epoch,
                        conversion.segment_span(n).map(|(_, end)| end),
                        Some(converted),
                        Furthest::Segment(n),
                    ),
                    None => (
                        job.epoch,
                        Some(job.reader_ms()),
                        Some(converted),
                        Furthest::Byte(offset + sent),
                    ),
                }
            }
        };
        let point = ReadPoint {
            at: unix_ms(),
            read_ms,
            converted_ms,
        };
        playback
            .trace
            .lock()
            .expect("never held across a panic")
            .read(point, epoch, mark);
    }

    pub fn revised(&self, handle: &PlaybackHandle) {
        if let Some(playback) = self.lock().get_mut(&handle.0) {
            playback.revision += 1;
        }
    }

    pub(crate) async fn progress_control(
        &self,
        session: &SessionId,
        item: &MediaId,
    ) -> crate::keyed_locks::Guard<'_> {
        self.progress_controls
            .lock(&format!("{}:{item}", session.0))
            .await
    }

    pub(crate) fn check_progress(
        &self,
        handle: &PlaybackHandle,
        sequence: u64,
    ) -> Result<(), spoolframe_protocol::Error> {
        let handles = self.lock();
        let Some(playback) = handles.get(&handle.0) else {
            drop(handles);
            return Err(match self.gone(&handle.0) {
                Refusal::Stopped => spoolframe_protocol::Error::PlaybackStopped,
                _ => spoolframe_protocol::Error::PlaybackExpired,
            });
        };
        if playback.progress_superseded || sequence <= playback.progress_sequence {
            return Err(spoolframe_protocol::Error::StaleProgress);
        }
        Ok(())
    }

    /// The client reported `position_ms` as `sequence`.
    pub(crate) fn record_progress(&self, handle: &PlaybackHandle, sequence: u64, position_ms: u64) {
        if let Some(playback) = self.lock().get_mut(&handle.0) {
            playback.progress_sequence = sequence;
            playback
                .trace
                .lock()
                .expect("never held across a panic")
                .position = Some((position_ms, unix_ms()));
        }
    }

    /// End a playback, `why`: its client stopped it, or its session failed
    /// its re-check.
    pub fn end(&self, handle: &PlaybackHandle, why: EndReason) {
        let ended = self.lock().remove_entry(&handle.0);
        // Dropped outside the lock: ending a conversion removes files.
        self.ended_all(ended, why);
    }

    /// End `endpoint`'s converted playbacks: it is starting another, and a
    /// device plays one thing at a time, so their encodes would only hold
    /// slots (an app killed mid-film and opened again). Returns how many.
    pub fn end_conversions_on(&self, endpoint: &str) -> usize {
        self.end_where(
            |p| p.endpoint == endpoint && p.conversion.is_some(),
            EndReason::Replaced,
        )
    }

    /// End every playback of `endpoint`: it has been gone longer than the
    /// grace. Returns how many.
    pub fn end_device(&self, endpoint: &str) -> usize {
        self.end_where(|p| p.endpoint == endpoint, EndReason::DeviceGone)
    }

    fn end_where(&self, ends: impl Fn(&Playback) -> bool, why: EndReason) -> usize {
        let mut handles = self.lock();
        let ending: Vec<String> = handles
            .iter()
            .filter(|(_, p)| ends(p))
            .map(|(h, _)| h.clone())
            .collect();
        let ended: Vec<(String, Playback)> = ending
            .into_iter()
            .filter_map(|h| handles.remove_entry(&h))
            .collect();
        drop(handles);
        let count = ended.len();
        self.ended_all(ended, why);
        count
    }

    /// End every playback idle past the limit. Run now and then, so a
    /// conversion does not outlive its handle by long.
    pub fn sweep(&self) {
        let now = self.clock.now();
        self.end_where(
            |p| now.duration_since(p.last_used) >= self.idle,
            EndReason::Idle,
        );
    }

    /// How often [`Playbacks::sweep`] should run.
    pub fn sweep_every(&self) -> Duration {
        (self.idle / 2).min(Duration::from_secs(30))
    }

    /// Count `ended` playbacks and keep them for the last hour's list, as
    /// they were (P3-15). Called outside the lock: their conversions are
    /// dropped here, which stops their encodes and removes their files.
    fn ended_all(&self, ended: impl IntoIterator<Item = (String, Playback)>, why: EndReason) {
        let at = unix_ms();
        let mut kept = Vec::new();
        for (handle, playback) in ended {
            self.ended.inc();
            let last = playback.snapshot(&handle);
            // A failed conversion says more than how its playback was let go.
            let why = match (
                &why,
                last.conversion.as_ref().and_then(|c| c.failed.clone()),
            ) {
                (EndReason::StoppedByAdmin, _) | (_, None) => why.clone(),
                (_, Some(reason)) => EndReason::Failed(reason),
            };
            kept.push(EndedPlayback { at, why, last });
        }
        if kept.is_empty() {
            return;
        }
        let mut gone = self.gone_lock();
        gone.extend(kept);
        Self::prune(&mut gone, at);
    }

    fn gone_lock(&self) -> std::sync::MutexGuard<'_, VecDeque<EndedPlayback>> {
        self.gone.lock().expect("never held across a panic")
    }

    fn prune(gone: &mut VecDeque<EndedPlayback>, now: i64) {
        let since = now - ENDED_KEPT.as_millis() as i64;
        while gone
            .front()
            .is_some_and(|e| e.at < since || gone.len() > ENDED_MAX)
        {
            gone.pop_front();
        }
    }

    /// Every open playback as playback debug shows it, newest first
    /// (P3-15). One idle past the limit is still listed until a sweep.
    pub fn snapshots(&self) -> Vec<Snapshot> {
        let open: Vec<(String, Playback)> = self
            .lock()
            .iter()
            .map(|(h, p)| (h.clone(), p.clone()))
            .collect();
        let mut snapshots: Vec<Snapshot> =
            open.iter().map(|(handle, p)| p.snapshot(handle)).collect();
        snapshots.sort_by(|a, b| b.started_at.cmp(&a.started_at).then(a.id.cmp(&b.id)));
        snapshots
    }

    /// The playbacks that ended in the last hour, newest first.
    pub fn ended(&self) -> Vec<EndedPlayback> {
        let mut gone = self.gone_lock();
        Self::prune(&mut gone, unix_ms());
        gone.iter().rev().cloned().collect()
    }

    /// The playback debug calls `id`: open, or ended in the last hour.
    pub fn find(&self, id: &str) -> Option<Found> {
        let open = self
            .lock()
            .iter()
            .find(|(_, p)| p.id == id)
            .map(|(h, p)| (h.clone(), p.clone()));
        if let Some((handle, playback)) = open {
            return Some(Found::Open(playback.snapshot(&handle)));
        }
        self.ended()
            .into_iter()
            .find(|e| e.last.id == id)
            .map(Found::Ended)
    }

    /// Stop playback `id` (an admin's Stop, P3-15): it ends now, its
    /// conversion with it, and its client is told `PlaybackStopped` on
    /// its next use, so it does not start it again. Whether it was open.
    pub fn stop(&self, id: &str) -> bool {
        let mut handles = self.lock();
        let Some(handle) = handles
            .iter()
            .find(|(_, p)| p.id == id)
            .map(|(h, _)| h.clone())
        else {
            return false;
        };
        let ended = handles.remove_entry(&handle);
        self.stopped
            .lock()
            .expect("never held across a panic")
            .insert(handle, self.clock.now());
        drop(handles);
        self.ended_all(ended, EndReason::StoppedByAdmin);
        true
    }

    /// End playback `id`'s conversion (an admin's End conversion, P3-15):
    /// its ffmpeg stops and the playback goes on with what was written.
    pub fn end_conversion(&self, id: &str) -> Result<(), EndConversionError> {
        let conversion = self
            .lock()
            .values()
            .find(|p| p.id == id)
            .ok_or(EndConversionError::NotFound)?
            .conversion
            .clone()
            .ok_or(EndConversionError::NoConversion)?;
        conversion.end();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::Clock;

    fn start(p: &Playbacks) -> PlaybackHandle {
        start_on(p, "e")
    }

    fn viewer() -> Viewer {
        Viewer {
            user_id: "local:1".into(),
            username: "alice".into(),
            display_name: "Alice".into(),
            device_name: Some("Den".into()),
            client_name: "spoolframe-linux".into(),
            platform: Platform::Linux,
        }
    }

    fn start_on(p: &Playbacks, endpoint: &str) -> PlaybackHandle {
        p.start(
            Playbacks::new_handle(),
            Opened {
                viewer: viewer(),
                shown: Shown {
                    title: "Alien".into(),
                    subtitle: None,
                    duration_ms: Some(10_000),
                },
                planned: PlaybackPlan::Unknown,
                session: SessionId("s".into()),
                endpoint: endpoint.into(),
                item: "plex:movie:1".parse().unwrap(),
                selection: Selection::default(),
                path: "/media/a.mkv".into(),
                size: 10,
                file: None,
                subtitle: None,
                conversion: None,
            },
        )
    }

    #[test]
    fn a_handle_is_usable_by_its_session_on_its_endpoint_only() {
        let p = Playbacks::default();
        let h = start(&p);
        assert!(p.use_handle(&h, &SessionId("s".into()), "e").is_ok());
        assert_eq!(
            p.use_handle(&h, &SessionId("other".into()), "e")
                .unwrap_err(),
            Refusal::NotYours
        );
        assert_eq!(
            p.use_handle(&h, &SessionId("s".into()), "x").unwrap_err(),
            Refusal::NotYours
        );
    }

    /// Each playback ends once in the counts, however it ends (P3-14).
    #[test]
    fn playbacks_are_counted_open_and_ended_once_each() {
        let clock = Clock::default();
        let p = Playbacks::default().with_clock(clock.clone());
        let stopped = start(&p);
        let idle = start(&p);
        let _device = start_on(&p, "tv");
        let counts = p.counts();
        assert_eq!((counts.direct, counts.started, counts.ended), (3, 3, 0));
        p.end(&stopped, EndReason::Finished);
        p.end(&stopped, EndReason::Finished);
        assert_eq!(p.end_device("tv"), 1);
        clock.advance(IDLE);
        // Found idle when used, then forgotten.
        assert!(p.use_handle(&idle, &SessionId("s".into()), "e").is_err());
        p.sweep();
        let counts = p.counts();
        assert_eq!(
            (counts.active(Plan::Direct), counts.started, counts.ended),
            (0, 3, 3)
        );
    }

    #[test]
    fn an_unknown_handle_is_expired() {
        let p = Playbacks::default();
        assert_eq!(
            p.use_handle(&PlaybackHandle("nope".into()), &SessionId("s".into()), "e")
                .unwrap_err(),
            Refusal::Expired
        );
    }

    #[test]
    fn an_idle_handle_expires() {
        let clock = Clock::default();
        let p = Playbacks::default().with_clock(clock.clone());
        let h = start(&p);
        let s = SessionId("s".into());
        clock.advance(IDLE - Duration::from_secs(1));
        assert!(p.use_handle(&h, &s, "e").is_ok());
        // Used a moment ago: idle from then.
        clock.advance(IDLE);
        assert_eq!(p.use_handle(&h, &s, "e").unwrap_err(), Refusal::Expired);
    }

    #[test]
    fn use_asks_for_a_recheck_once_per_interval() {
        let clock = Clock::default();
        let p = Playbacks::default().with_clock(clock.clone());
        let h = start(&p);
        let s = SessionId("s".into());
        assert!(!p.use_handle(&h, &s, "e").unwrap().recheck);
        clock.advance(RECHECK - Duration::from_secs(1));
        assert!(!p.use_handle(&h, &s, "e").unwrap().recheck);
        clock.advance(Duration::from_secs(1));
        assert!(p.use_handle(&h, &s, "e").unwrap().recheck);
        assert!(!p.use_handle(&h, &s, "e").unwrap().recheck);
    }

    #[test]
    fn a_gone_device_s_playbacks_end_and_no_one_else_s() {
        let p = Playbacks::default();
        let tv = start_on(&p, "tv");
        let phone = start_on(&p, "phone");
        assert_eq!(p.end_device("tv"), 1);
        let s = SessionId("s".into());
        assert_eq!(p.use_handle(&tv, &s, "tv").unwrap_err(), Refusal::Expired);
        assert!(p.use_handle(&phone, &s, "phone").is_ok());
    }

    #[test]
    fn a_new_playback_ends_only_converted_ones() {
        let p = Playbacks::default();
        let direct = start_on(&p, "tv");
        // Direct play holds no slot: it stays.
        assert_eq!(p.end_conversions_on("tv"), 0);
        assert!(p.use_handle(&direct, &SessionId("s".into()), "tv").is_ok());
    }

    fn id_of(p: &Playbacks, endpoint: &str) -> String {
        p.lock()
            .values()
            .find(|pb| pb.endpoint == endpoint)
            .unwrap()
            .id
            .clone()
    }

    /// An admin's Stop (P3-15) ends the playback, and its client is told
    /// so, not that it expired: an expired one it would start again.
    #[test]
    fn a_stopped_playback_ends_and_its_client_is_told_it_was_stopped() {
        let p = Playbacks::default();
        let h = start_on(&p, "tv");
        let id = id_of(&p, "tv");
        assert!(p.stop(&id));
        assert!(!p.stop(&id), "stopped twice");
        let s = SessionId("s".into());
        assert_eq!(p.use_handle(&h, &s, "tv").unwrap_err(), Refusal::Stopped);
        assert_eq!(
            p.check_progress(&h, 1).unwrap_err(),
            spoolframe_protocol::Error::PlaybackStopped
        );
        // Any other gone handle is still merely expired.
        assert_eq!(
            p.use_handle(&PlaybackHandle("nope".into()), &s, "tv")
                .unwrap_err(),
            Refusal::Expired
        );
        let ended = p.ended();
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].why, EndReason::StoppedByAdmin);
        assert!(matches!(p.find(&id), Some(Found::Ended(_))));
        assert_eq!(p.counts().ended, 1);
    }

    /// The last hour's list says how each one ended.
    #[test]
    fn an_ended_playback_is_listed_with_how_it_ended_newest_first() {
        let clock = Clock::default();
        let p = Playbacks::default().with_clock(clock.clone());
        let finished = start_on(&p, "a");
        start_on(&p, "b");
        p.end(&finished, EndReason::Finished);
        assert_eq!(p.end_device("b"), 1);
        start_on(&p, "c");
        clock.advance(IDLE);
        p.sweep();
        let why: Vec<EndReason> = p.ended().into_iter().map(|e| e.why).collect();
        assert_eq!(
            why,
            [EndReason::Idle, EndReason::DeviceGone, EndReason::Finished]
        );
    }

    #[test]
    fn end_conversion_needs_an_open_converted_playback() {
        let p = Playbacks::default();
        start_on(&p, "tv");
        let id = id_of(&p, "tv");
        assert_eq!(p.end_conversion(&id), Err(EndConversionError::NoConversion));
        assert_eq!(
            p.end_conversion("00000000"),
            Err(EndConversionError::NotFound)
        );
    }

    /// Direct play's reads are placed in the film by their share of the
    /// file; the chart keeps one point a second, for ten minutes.
    #[test]
    fn reads_are_charted_one_point_a_second_for_ten_minutes() {
        let p = Playbacks::default();
        let h = start_on(&p, "tv");
        p.read(&h, 0, 5, None);
        p.read(&h, 5, 3, None);
        let snap = p.snapshots().remove(0);
        assert_eq!(snap.reads.len(), 1, "two reads in one second");
        assert_eq!(snap.read_to_ms, Some(8_000), "8 of 10 bytes of 10 s");
        assert_eq!(snap.furthest, Some(Furthest::Byte(8)));
        assert!(snap.last_read_at.is_some());

        let mut trace = Trace::default();
        let minute = 60_000;
        for i in 0..=20 {
            let point = ReadPoint {
                at: i * minute,
                read_ms: Some(i as u64),
                converted_ms: None,
            };
            trace.read(point, 0, Furthest::Byte(i as u64));
        }
        let kept: Vec<i64> = trace.reads.iter().map(|r| r.at / minute).collect();
        assert_eq!(kept, (10..=20).collect::<Vec<_>>());
    }

    /// The furthest read is in the encode now: a seek's new epoch starts
    /// it again.
    #[test]
    fn the_furthest_read_starts_again_with_a_new_epoch() {
        let mut trace = Trace::default();
        let point = |at| ReadPoint {
            at,
            read_ms: None,
            converted_ms: None,
        };
        trace.read(point(0), 0, Furthest::Segment(9));
        trace.read(point(2000), 0, Furthest::Segment(4));
        assert_eq!(trace.furthest, Some((0, Furthest::Segment(9))));
        trace.read(point(4000), 1, Furthest::Segment(2));
        assert_eq!(trace.furthest, Some((1, Furthest::Segment(2))));
    }

    #[test]
    fn an_episode_is_shown_by_its_show_season_and_number() {
        let item = Item::Episode(spoolframe_types::Episode {
            id: "plex:episode:7".parse().unwrap(),
            show_id: "plex:show:1".parse().unwrap(),
            show_title: Some("Severance".into()),
            season_id: "plex:season:2".parse().unwrap(),
            season: 2,
            episode: 4,
            title: "Woe's Hollow".into(),
            summary: None,
            content_rating: None,
            duration: Some(Duration::from_secs(3000)),
            artwork: Vec::new(),
            playback: Default::default(),
        });
        let shown = Shown::of(&item);
        assert_eq!(shown.title, "Severance · S2 E4");
        assert_eq!(shown.subtitle.as_deref(), Some("Woe's Hollow"));
        assert_eq!(shown.duration_ms, Some(3_000_000));
    }

    #[test]
    fn handles_are_unguessable_and_distinct() {
        let p = Playbacks::default();
        let a = start(&p);
        let b = start(&p);
        assert_ne!(a, b);
        assert_eq!(a.0.len(), 32);
    }
}
