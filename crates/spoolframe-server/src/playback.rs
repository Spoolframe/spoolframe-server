//! Playback sessions (P1-08): what a `StartPlayback` leaves behind, so
//! that range reads need no database lookups.
//!
//! A handle is bound to the session and the endpoint that started it. It
//! lives in memory; it holds no open file (each read opens the file), so an
//! idle handle costs a map entry until it expires.

use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use std::sync::Arc;

use password_hash::rand_core::{OsRng, RngCore};
use spoolframe_protocol::{PlaybackHandle, PlaybackPlan, Quality, Selection, SessionId};
use spoolframe_types::{Delivery, Item, MediaId, Platform, SubtitleFormat};
use tokio::sync::watch;

use crate::encode::Conversion;
use crate::metrics::Counter;
use crate::subtitles::TextState;

/// How long a handle lives unused: no reads, and no request naming it (a
/// paused client's progress report counts). Long enough for a pause: a
/// viewer away for a few minutes plays on where they left off. A paused
/// conversion keeps its ffmpeg, stopped by the read-ahead throttle (no
/// CPU), its slot and its files for this long, unless its device starts
/// another that needs the slot: it is not played ([`READING`]), so it
/// gives way. A client that comes back later starts again at its position.
pub const IDLE: Duration = Duration::from_secs(30 * 60);
/// How long a device's playbacks outlive its last connection: long enough
/// for a reconnect (a network change, an app brought back), short of
/// holding an encode slot for [`IDLE`] for an app that was killed.
pub const DISCONNECT_GRACE: Duration = Duration::from_secs(30);
/// How recently a playback must have been used (a segment, playlist or
/// media read, or any request naming it) to count as playing: making room
/// on its device never ends it. Five 6-second HLS segments: a player
/// fetches about one per segment it plays once its buffer is full, since an
/// encode runs at most `read_ahead_secs` ahead of its reader, so a track
/// that plays is read far more often than this. One fully read (its end
/// within the read-ahead) may go quiet longer, but its player holds all of
/// it by then.
pub const READING: Duration = Duration::from_secs(30);
/// The converted playbacks a device keeps when it starts another, "now +
/// next": the one playing and the one starting, the next it prepares.
pub const NOW_AND_NEXT: usize = 2;
/// The most converted playbacks a device holds, however many it reads: a
/// third only while the track before is still read as the next is
/// prepared, so a broken client cannot run conversions without end.
pub const DEVICE_MOST: usize = 3;
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

/// Where a playback's keyframes were read from (PX-25, shown by PX-24).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyframesFrom {
    /// The file's own index.
    Index,
    /// Every packet of the file.
    Scan,
}

impl KeyframesFrom {
    pub fn of(from: &spoolframe_media::KeyframesFrom) -> Self {
        match from {
            spoolframe_media::KeyframesFrom::Index(_) => KeyframesFrom::Index,
            spoolframe_media::KeyframesFrom::Scan { .. } => KeyframesFrom::Scan,
        }
    }
}

/// When a playback's first segment was ready (PX-24).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirstSegment {
    /// Direct play has none: the file is there to read at once, and when
    /// the client first reads it is the client's doing.
    Direct,
    /// Its conversion has written none yet.
    Waiting,
    /// This long after the request.
    Ready(Duration),
    /// Its first encode ended (failed, ended, or replaced by a seek)
    /// before it wrote one.
    Never,
}

/// How long a playback took to start (PX-24), from its `StartPlayback`
/// request: what the "playback start timed" log line says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartTimes {
    /// From the request to the plan made.
    pub plan: Duration,
    /// How long its keyframes took to read, and where from; `None` when
    /// none were read (only a copied video's HLS needs them).
    pub keyframes: Option<(Duration, KeyframesFrom)>,
    /// From the request to the first segment written (or, progressive,
    /// the first bytes).
    pub first_segment: FirstSegment,
}

impl StartTimes {
    /// Log the start's breakdown: once per start, when its first segment
    /// is ready, or at once for direct play.
    pub fn log(&self, id: &MediaId) {
        let (keyframes_ms, keyframes) = match self.keyframes {
            Some((took, from)) => (
                took.as_millis().to_string(),
                match from {
                    KeyframesFrom::Index => "index",
                    KeyframesFrom::Scan => "scan",
                },
            ),
            None => ("none".to_owned(), "none"),
        };
        let first_segment_ms = match self.first_segment {
            FirstSegment::Ready(took) => took.as_millis().to_string(),
            FirstSegment::Direct => "n/a".to_owned(),
            FirstSegment::Waiting | FirstSegment::Never => "none".to_owned(),
        };
        tracing::info!(
            %id,
            plan_ms = self.plan.as_millis() as u64,
            %keyframes_ms,
            keyframes,
            %first_segment_ms,
            "playback start timed"
        );
    }
}

/// Wait for the first segment of a conversion's first run, whose progress
/// is `progress`, and say when it was ready in `times`, then log the
/// start. HLS waits for a finished segment; a progressive file for its
/// first bytes. `requested` is when the request came.
pub async fn time_first_segment(
    times: Arc<Mutex<StartTimes>>,
    requested: Instant,
    mut progress: watch::Receiver<crate::encode::job::Progress>,
    hls: bool,
    id: MediaId,
) {
    let ready = |p: &crate::encode::job::Progress| {
        if hls {
            p.last_segment.is_some()
        } else {
            p.bytes > 0
        }
    };
    // Ready, or ended without one; or the run was stopped (a seek, the
    // playback ended), and what it wrote by then is all there is.
    let wrote = match progress.wait_for(|p| ready(p) || p.done.is_some()).await {
        Ok(p) => ready(&p),
        Err(_) => false,
    } || ready(&progress.borrow());
    let first = if wrote {
        FirstSegment::Ready(requested.elapsed())
    } else {
        FirstSegment::Never
    };
    let now = {
        let mut times = times.lock();
        times.first_segment = first;
        *times
    };
    now.log(&id);
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
    /// Its place among every playback opened since the server started:
    /// which of a device's is the newest, when two start in one ms.
    order: u64,
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
    /// The quality it is planned for (P6-06), again at a subtitle change.
    pub quality: Quality,
    /// The file's picture, width × height (PX-24): what direct play
    /// writes.
    pub video_size: Option<(u32, u32)>,
    /// How long it took to start (PX-24).
    pub start: Arc<Mutex<StartTimes>>,
    /// The connection that started it (a number of the server's own).
    connection: u64,
    last_used: Instant,
    /// When its client last used it ([`READING`]); never at the start, so
    /// a playback prepared and not yet read is not counted as playing.
    last_read: Option<Instant>,
    /// When the connection that started it closed.
    orphaned_at: Option<Instant>,
    last_checked: Instant,
}

impl Playback {
    /// Its client used it in the last [`READING`].
    fn reading(&self, now: Instant) -> bool {
        self.last_read
            .is_some_and(|at| now.saturating_duration_since(at) < READING)
    }

    /// The connection that started it closed, and no other has used it
    /// since: a killed app's.
    fn orphaned(&self) -> bool {
        self.orphaned_at
            .is_some_and(|closed| self.last_read.is_none_or(|read| read <= closed))
    }

    /// A killed app's, as a start from `starting` sees it: its connection
    /// closed ([`Playback::orphaned`]), or a newer connection of its
    /// device is starting and it has not been used since that one opened.
    /// A killed app sends no close, so its connection stays open until
    /// it idles out, after the app is open again; and an app runs one
    /// connection at a time, so a newer one means the older is left. A
    /// reconnect that reads it on (a resume) takes it back.
    fn left_behind(&self, starting: Option<(u64, Instant)>) -> bool {
        self.orphaned()
            || starting.is_some_and(|(connection, opened)| {
                self.connection < connection && self.last_read.is_none_or(|read| read < opened)
            })
    }

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
    /// The quality the viewer chose (P6-06).
    pub quality: Quality,
    /// The file's picture, width × height.
    pub video_size: Option<(u32, u32)>,
    /// How long it took to start (PX-24); its first segment filled in
    /// once ready.
    pub start: Arc<Mutex<StartTimes>>,
    /// The connection starting it ([`Playbacks::connection_closed`]).
    pub connection: u64,
}

/// What a playback becomes when its subtitle or its quality changes.
#[derive(Debug)]
pub struct Changed {
    pub planned: PlaybackPlan,
    pub selection: Selection,
    pub size: u64,
    pub file: Option<FileId>,
    pub subtitle: Option<Sidecar>,
    pub conversion: Option<Arc<Conversion>>,
    pub quality: Quality,
}

/// A device holds [`DEVICE_MOST`] converted playbacks, all playing: it
/// may start no other until one is let go of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceFull;

/// Why a handle cannot be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Unknown, or idle past [`IDLE`].
    Expired,
    /// From another session or another endpoint.
    NotYours,
    /// Stopped by an admin (P3-15): over, not to be started again.
    Stopped,
    /// Its session ended (B-10): the client must sign in again.
    SignedOut,
}

/// How a client uses a handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Using {
    /// A read, or a control (a seek, a subtitle change, a stop): it is
    /// played ([`READING`]).
    Plays,
    /// A progress report, which a paused client sends too: the playback
    /// stays alive ([`IDLE`]), but is not played for it, so its device's
    /// next start that needs its slot may still end it.
    Reports,
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
    /// Handles ended for a reason their client is told, when, and the
    /// refusal it gets: an admin stopped it (P3-15), or its session ended
    /// (B-10), rather than that it expired. Kept as long as an idle handle
    /// would have lived.
    let_go: Mutex<HashMap<String, (Instant, Refusal)>>,
    /// What idling out and rechecks are timed by (PX-09).
    clock: crate::clock::Clock,
    /// The open connections, by number, and when each opened.
    connections: Mutex<HashMap<u64, Instant>>,
    /// The last connection number given out.
    numbered: std::sync::atomic::AtomicU64,
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
    /// Its device started another and needed room: it keeps the one
    /// playing now and the next ("now + next"), and this one was neither
    /// (not used in the last [`READING`]).
    Replaced,
    /// Its device was gone longer than the grace.
    DeviceGone,
    /// Its session ended, or its user was disabled.
    SignedOut,
    /// What it plays is in a library its user can no longer see (B-10):
    /// removed, or on a Plex server no longer served.
    AccessRemoved,
    /// Its conversion failed, with ffmpeg's reason.
    Failed(String),
}

impl EndReason {
    /// What the log calls it.
    pub fn as_str(&self) -> &'static str {
        match self {
            EndReason::Finished => "finished",
            EndReason::StoppedByAdmin => "stopped_by_admin",
            EndReason::Idle => "idle",
            EndReason::Replaced => "replaced",
            EndReason::DeviceGone => "device_gone",
            EndReason::SignedOut => "signed_out",
            EndReason::AccessRemoved => "access_removed",
            EndReason::Failed(_) => "failed",
        }
    }
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
    /// The file's picture, width × height.
    pub video_size: Option<(u32, u32)>,
    pub start: StartTimes,
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
    /// What it weighs against the encode budget.
    pub load: crate::encode::Load,
    /// How its running encode makes the video (PX-24).
    pub made: Option<crate::encode::Made>,
    /// What that encode has written, over how much of the film.
    pub output: crate::encode::Measured,
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
        let trace = self.trace.lock();
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
                load: c.load(),
                made: job.made(),
                output: progress.output,
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
            video_size: self.video_size,
            start: *self.start.lock(),
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
            let_go: Mutex::default(),
            clock: crate::clock::Clock::default(),
            connections: Mutex::default(),
            numbered: Default::default(),
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

    fn lock(&self) -> parking_lot::MutexGuard<'_, HashMap<String, Playback>> {
        self.handles.lock()
    }

    /// Why `handle`, which is not open, cannot be used: an admin stopped
    /// it, its session ended, or it is unknown or idled out.
    fn gone(&self, handle: &str) -> Refusal {
        let mut let_go = self.let_go.lock();
        let now = self.clock.now();
        let_go.retain(|_, (at, _)| now.saturating_duration_since(*at) < self.idle);
        let_go
            .get(handle)
            .map_or(Refusal::Expired, |(_, refusal)| *refusal)
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
        // Under the lock, so no two playbacks share one.
        let order = self.started.get();
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
                order,
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
                quality: opened.quality,
                video_size: opened.video_size,
                start: opened.start,
                connection: opened.connection,
                last_used: now,
                last_read: None,
                orphaned_at: None,
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
        playback.quality = changed.quality;
        std::mem::replace(&mut playback.conversion, changed.conversion)
    }

    /// Use `handle` from `session` on `endpoint`: refused if it is not
    /// theirs or has expired, and otherwise marked used, and played if
    /// `using` plays it.
    pub fn use_handle(
        &self,
        handle: &PlaybackHandle,
        session: &SessionId,
        endpoint: &str,
        using: Using,
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
        if using == Using::Plays {
            playback.last_read = Some(now);
        }
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
    /// Not a use by the client ([`READING`]): the server waits on, even
    /// for a killed app whose connection it has not yet seen go.
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
        playback.trace.lock().read(point, epoch, mark);
    }

    /// Keep `quality` for `handle`'s next re-plan, its plan unchanged:
    /// not a change of configuration, so the revision stays.
    pub fn set_quality(&self, handle: &PlaybackHandle, quality: Quality) {
        if let Some(playback) = self.lock().get_mut(&handle.0) {
            playback.quality = quality;
        }
    }

    /// `handle`'s revision now, if it is open.
    pub fn revision_of(&self, handle: &PlaybackHandle) -> Option<u64> {
        self.lock().get(&handle.0).map(|playback| playback.revision)
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
                Refusal::SignedOut => spoolframe_protocol::Error::Unauthenticated,
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
            playback.trace.lock().position = Some((position_ms, unix_ms()));
        }
    }

    /// End a playback, `why`: its client stopped it, or its session failed
    /// its re-check.
    pub fn end(&self, handle: &PlaybackHandle, why: EndReason) {
        let ended = self.lock().remove_entry(&handle.0);
        // Dropped outside the lock: ending a conversion removes files.
        self.ended_all(ended, why);
    }

    /// End `endpoint`'s converted playbacks that are not playing: it is
    /// starting another from connection `starting`, and no slot is free
    /// for it, so its own idle encodes give way (one device's two
    /// playbacks with `max_jobs = 1`). A killed app's always does
    /// ([`Playback::left_behind`]); one its client used in the last
    /// [`READING`] is playing and stays, and the new start is then refused
    /// as busy: a track that stops mid-play is worse than a next track
    /// that waits. Returns how many.
    pub fn end_conversions_on(&self, endpoint: &str, starting: u64) -> usize {
        self.end_conversions_on_where(endpoint, starting, |p| p.conversion.is_some())
    }

    /// [`Playbacks::end_conversions_on`], counting the playbacks `holds`
    /// picks.
    fn end_conversions_on_where(
        &self,
        endpoint: &str,
        starting: u64,
        holds: impl Fn(&Playback) -> bool,
    ) -> usize {
        let now = self.clock.now();
        let starting = self.opened(starting);
        self.end_where(
            |p| p.endpoint == endpoint && holds(p) && (p.left_behind(starting) || !p.reading(now)),
            EndReason::Replaced,
        )
    }

    /// Connection `connection` and when it opened, if it is open.
    fn opened(&self, connection: u64) -> Option<(u64, Instant)> {
        self.connections
            .lock()
            .get(&connection)
            .map(|at| (connection, *at))
    }

    /// Make room on `endpoint` for a playback that connection `starting`
    /// is starting: a device keeps "now + next" ([`NOW_AND_NEXT`]), the
    /// one playing now and the next it prepares (an iOS app opens the next
    /// track while this one plays, for gapless audio). A killed app's
    /// playbacks all end first, however lately they were read
    /// ([`Playback::left_behind`]): the app opened again can use none of
    /// them. Of the others, enough end to keep one besides the new start,
    /// those used least recently first, never used first, the oldest first
    /// among equals. One used in the last [`READING`] is playing and never
    /// ends; the one starting is not open yet, so it cannot be chosen.
    /// With too few to end the device holds more than two, up to
    /// [`DEVICE_MOST`]: past that the start is refused ([`DeviceFull`]).
    /// Direct play holds no slot, and is left alone. Returns how many
    /// ended.
    pub fn make_room_for_next(&self, endpoint: &str, starting: u64) -> Result<usize, DeviceFull> {
        self.make_room_for_next_where(endpoint, starting, |p| p.conversion.is_some())
    }

    /// [`Playbacks::make_room_for_next`], counting the playbacks `holds`
    /// picks.
    fn make_room_for_next_where(
        &self,
        endpoint: &str,
        starting: u64,
        holds: impl Fn(&Playback) -> bool,
    ) -> Result<usize, DeviceFull> {
        let now = self.clock.now();
        let starting = self.opened(starting);
        let mut handles = self.lock();
        let (left_behind, held): (Vec<(&String, &Playback)>, Vec<_>) = handles
            .iter()
            .filter(|(_, p)| p.endpoint == endpoint && holds(p))
            .partition(|(_, p)| p.left_behind(starting));
        // One stays besides the one starting: "now", once that is "next".
        let over = held.len().saturating_sub(NOW_AND_NEXT - 1);
        let mut idle: Vec<&(&String, &Playback)> =
            held.iter().filter(|(_, p)| !p.reading(now)).collect();
        idle.sort_by_key(|(_, p)| (p.last_read, p.order));
        let idle = idle.into_iter().take(over);
        let ending: Vec<String> = left_behind
            .iter()
            .chain(idle)
            .map(|(h, _)| (*h).clone())
            .collect();
        let left = held.len() + left_behind.len() - ending.len();
        let ended: Vec<(String, Playback)> = ending
            .into_iter()
            .filter_map(|h| handles.remove_entry(&h))
            .collect();
        drop(handles);
        let count = ended.len();
        self.ended_all(ended, EndReason::Replaced);
        if left >= DEVICE_MOST {
            return Err(DeviceFull);
        }
        Ok(count)
    }

    /// A connection opened: its number, never another's, which its
    /// playbacks are started with ([`Opened::connection`]).
    pub fn connection_opened(&self) -> u64 {
        let number = self
            .numbered
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        self.connections.lock().insert(number, self.clock.now());
        number
    }

    /// The connection numbered `connection` closed: its playbacks are a
    /// killed app's, unless another connection of the device uses them
    /// again (a reconnect).
    pub fn connection_closed(&self, connection: u64) {
        let now = self.clock.now();
        self.connections.lock().remove(&connection);
        for p in self.lock().values_mut() {
            if p.connection == connection {
                p.orphaned_at = Some(now);
            }
        }
    }

    /// End every playback of `endpoint`: it has been gone longer than the
    /// grace. Returns how many.
    pub fn end_device(&self, endpoint: &str) -> usize {
        self.end_where(|p| p.endpoint == endpoint, EndReason::DeviceGone)
    }

    /// The items open playbacks play, each once.
    pub fn items(&self) -> Vec<MediaId> {
        let mut items: Vec<MediaId> = Vec::new();
        for playback in self.lock().values() {
            if !items.contains(&playback.item) {
                items.push(playback.item.clone());
            }
        }
        items
    }

    /// End every playback of `item`, `why`. Returns how many.
    pub fn end_item(&self, item: &MediaId, why: EndReason) -> usize {
        self.end_where(|p| &p.item == item, why)
    }

    /// The sessions with a playback open, each once.
    pub fn sessions(&self) -> Vec<SessionId> {
        let mut sessions: Vec<SessionId> =
            self.lock().values().map(|p| p.session.clone()).collect();
        sessions.sort_by(|a, b| a.0.cmp(&b.0));
        sessions.dedup();
        sessions
    }

    /// End every playback of `session`, which has ended (B-10): revoked,
    /// signed out, or its user disabled. At once, not after the
    /// disconnect grace, which is for a device coming back to a session
    /// still live: their encodes stop now, and a client still using one
    /// is told to sign in again. Returns how many.
    pub fn end_session(&self, session: &SessionId) -> usize {
        let mut handles = self.lock();
        let ending: Vec<String> = handles
            .iter()
            .filter(|(_, p)| &p.session == session)
            .map(|(h, _)| h.clone())
            .collect();
        let ended: Vec<(String, Playback)> = ending
            .into_iter()
            .filter_map(|h| handles.remove_entry(&h))
            .collect();
        let now = self.clock.now();
        self.let_go.lock().extend(
            ended
                .iter()
                .map(|(h, _)| (h.clone(), (now, Refusal::SignedOut))),
        );
        drop(handles);
        let count = ended.len();
        self.ended_all(ended, EndReason::SignedOut);
        count
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
    /// Each one the server ends, rather than its client, is logged here
    /// and nowhere else, once.
    fn ended_all(&self, ended: impl IntoIterator<Item = (String, Playback)>, why: EndReason) {
        let at = unix_ms();
        let now = self.clock.now();
        // A client that stops its own playback knows why; every other end
        // is something its user may ask about later.
        let logged = why != EndReason::Finished;
        let mut kept = Vec::new();
        for (handle, playback) in ended {
            self.ended.inc();
            let last = playback.snapshot(&handle);
            // Its ffmpeg stops and its slot is let go of now: a read still
            // waiting (a killed app's) holds the conversion, which would
            // keep both until that read gave up.
            if let Some(conversion) = &playback.conversion {
                conversion.end();
            }
            // A failed conversion says more than how its playback was let go.
            let why = match (
                &why,
                last.conversion.as_ref().and_then(|c| c.failed.clone()),
            ) {
                (EndReason::StoppedByAdmin, _) | (_, None) => why.clone(),
                (_, Some(reason)) => EndReason::Failed(reason),
            };
            if logged {
                let error = match &why {
                    EndReason::Failed(reason) => Some(reason.as_str()),
                    _ => None,
                };
                tracing::info!(
                    playback = %playback.id,
                    item = %playback.item,
                    user = %playback.viewer.username,
                    endpoint = %playback.endpoint,
                    plan = playback.plan().as_str(),
                    why = why.as_str(),
                    idle_secs = now.saturating_duration_since(playback.last_used).as_secs(),
                    error,
                    "a playback ended"
                );
            }
            kept.push(EndedPlayback { at, why, last });
        }
        if kept.is_empty() {
            return;
        }
        let mut gone = self.gone_lock();
        gone.extend(kept);
        Self::prune(&mut gone, at);
    }

    fn gone_lock(&self) -> parking_lot::MutexGuard<'_, VecDeque<EndedPlayback>> {
        self.gone.lock()
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
        self.let_go
            .lock()
            .insert(handle, (self.clock.now(), Refusal::Stopped));
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
        start_from(p, endpoint, 0)
    }

    /// A playback `connection` starts on `endpoint`.
    fn start_from(p: &Playbacks, endpoint: &str, connection: u64) -> PlaybackHandle {
        p.start(
            Playbacks::new_handle(),
            Opened {
                connection,
                ..opened_on(endpoint)
            },
        )
    }

    /// Session `s`'s playback of the movie on `endpoint`.
    fn opened_on(endpoint: &str) -> Opened {
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
            quality: Quality::default(),
            video_size: None,
            start: Arc::new(Mutex::new(StartTimes {
                plan: Duration::ZERO,
                keyframes: None,
                first_segment: FirstSegment::Direct,
            })),
            connection: 0,
        }
    }

    #[test]
    fn a_handle_is_usable_by_its_session_on_its_endpoint_only() {
        let p = Playbacks::default();
        let h = start(&p);
        assert!(
            p.use_handle(&h, &SessionId("s".into()), "e", Using::Plays)
                .is_ok()
        );
        assert_eq!(
            p.use_handle(&h, &SessionId("other".into()), "e", Using::Plays)
                .unwrap_err(),
            Refusal::NotYours
        );
        assert_eq!(
            p.use_handle(&h, &SessionId("s".into()), "x", Using::Plays)
                .unwrap_err(),
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
        assert!(
            p.use_handle(&idle, &SessionId("s".into()), "e", Using::Plays)
                .is_err()
        );
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
            p.use_handle(
                &PlaybackHandle("nope".into()),
                &SessionId("s".into()),
                "e",
                Using::Plays
            )
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
        assert!(p.use_handle(&h, &s, "e", Using::Plays).is_ok());
        // Used a moment ago: idle from then.
        clock.advance(IDLE);
        assert_eq!(
            p.use_handle(&h, &s, "e", Using::Plays).unwrap_err(),
            Refusal::Expired
        );
    }

    #[test]
    fn use_asks_for_a_recheck_once_per_interval() {
        let clock = Clock::default();
        let p = Playbacks::default().with_clock(clock.clone());
        let h = start(&p);
        let s = SessionId("s".into());
        assert!(!p.use_handle(&h, &s, "e", Using::Plays).unwrap().recheck);
        clock.advance(RECHECK - Duration::from_secs(1));
        assert!(!p.use_handle(&h, &s, "e", Using::Plays).unwrap().recheck);
        clock.advance(Duration::from_secs(1));
        assert!(p.use_handle(&h, &s, "e", Using::Plays).unwrap().recheck);
        assert!(!p.use_handle(&h, &s, "e", Using::Plays).unwrap().recheck);
    }

    #[test]
    fn a_gone_device_s_playbacks_end_and_no_one_else_s() {
        let p = Playbacks::default();
        let tv = start_on(&p, "tv");
        let phone = start_on(&p, "phone");
        assert_eq!(p.end_device("tv"), 1);
        let s = SessionId("s".into());
        assert_eq!(
            p.use_handle(&tv, &s, "tv", Using::Plays).unwrap_err(),
            Refusal::Expired
        );
        assert!(p.use_handle(&phone, &s, "phone", Using::Plays).is_ok());
    }

    #[test]
    fn a_new_playback_ends_only_converted_ones() {
        let p = Playbacks::default();
        let direct = start_on(&p, "tv");
        // Direct play holds no slot: it stays.
        assert_eq!(p.end_conversions_on("tv", 0), 0);
        assert_eq!(p.make_room_for_next("tv", 0), Ok(0));
        assert!(
            p.use_handle(&direct, &SessionId("s".into()), "tv", Using::Plays)
                .is_ok()
        );
    }

    /// A start as StartPlayback makes it: room first, then the playback.
    /// Every playback here stands in for a converted one, since a real
    /// conversion needs ffmpeg; tests/it/encode.rs has those.
    fn start_next(p: &Playbacks, endpoint: &str) -> PlaybackHandle {
        start_next_from(p, endpoint, 0)
    }

    fn start_next_from(p: &Playbacks, endpoint: &str, connection: u64) -> PlaybackHandle {
        p.make_room_for_next_where(endpoint, connection, |_| true)
            .expect("room on the device");
        start_from(p, endpoint, connection)
    }

    /// An app prepares the next track while this one plays (iOS gapless
    /// audio): starting the next leaves the one playing alone.
    #[test]
    fn a_device_s_next_playback_leaves_the_one_playing() {
        let p = Playbacks::default();
        let s = SessionId("s".into());
        let now = start_next(&p, "phone");
        let next = start_next(&p, "phone");
        assert!(p.use_handle(&now, &s, "phone", Using::Plays).is_ok());
        assert!(p.use_handle(&next, &s, "phone", Using::Plays).is_ok());
    }

    /// A third start ends the oldest: its encode was abandoned, and its
    /// slot is for the new one.
    #[test]
    fn a_device_s_third_playback_ends_the_oldest_and_keeps_the_next() {
        let p = Playbacks::default();
        let s = SessionId("s".into());
        let oldest = start_next(&p, "phone");
        let next = start_next(&p, "phone");
        let third = start_next(&p, "phone");
        assert_eq!(
            p.use_handle(&oldest, &s, "phone", Using::Plays)
                .unwrap_err(),
            Refusal::Expired
        );
        assert!(p.use_handle(&next, &s, "phone", Using::Plays).is_ok());
        assert!(p.use_handle(&third, &s, "phone", Using::Plays).is_ok());
        let why: Vec<EndReason> = p.ended().into_iter().map(|e| e.why).collect();
        assert_eq!(why, [EndReason::Replaced]);
    }

    /// A killed app's conversion is left over on its device, its encode
    /// still holding a slot. The app, opened again, plays a track and
    /// prepares the next: the leftover is what ends, and the track being
    /// read plays on, even when no slot is free and the device must give
    /// way to its new start.
    #[test]
    fn a_leftover_conversion_ends_and_the_track_being_read_plays_on() {
        let clock = Clock::default();
        let p = Playbacks::default().with_clock(clock.clone());
        let s = SessionId("s".into());
        let leftover = start_next(&p, "phone");
        clock.advance(Duration::from_secs(60));
        let now = start_next(&p, "phone");
        p.use_handle(&now, &s, "phone", Using::Plays).unwrap();
        clock.advance(Duration::from_secs(1));
        // The next, as StartPlayback starts it with no slot free.
        p.make_room_for_next_where("phone", 0, |_| true).unwrap();
        p.end_conversions_on_where("phone", 0, |_| true);
        let next = start_on(&p, "phone");
        assert!(
            p.use_handle(&now, &s, "phone", Using::Plays).is_ok(),
            "the track played"
        );
        assert_eq!(
            p.use_handle(&leftover, &s, "phone", Using::Plays)
                .unwrap_err(),
            Refusal::Expired
        );
        assert!(p.use_handle(&next, &s, "phone", Using::Plays).is_ok());
    }

    /// An app that starts more than the next while one track is read: the
    /// ones far ahead give way to each other, and the track being read
    /// plays on.
    #[test]
    fn starts_past_the_next_end_the_ones_far_ahead_not_the_one_being_read() {
        let clock = Clock::default();
        let p = Playbacks::default().with_clock(clock.clone());
        let s = SessionId("s".into());
        let now = start_next(&p, "phone");
        p.use_handle(&now, &s, "phone", Using::Plays).unwrap();
        clock.advance(Duration::from_secs(1));
        let second = start_next(&p, "phone");
        let third = start_next(&p, "phone");
        let fourth = start_next(&p, "phone");
        assert!(
            p.use_handle(&now, &s, "phone", Using::Plays).is_ok(),
            "the track played"
        );
        for far in [&second, &third] {
            assert_eq!(
                p.use_handle(far, &s, "phone", Using::Plays).unwrap_err(),
                Refusal::Expired
            );
        }
        assert!(p.use_handle(&fourth, &s, "phone", Using::Plays).is_ok());
    }

    /// An app killed mid-track and opened again at once, its connection
    /// seen closed: its playbacks, read a moment ago, all end at the new
    /// connection's first start, which needs no room for them.
    #[test]
    fn a_closed_connection_s_playbacks_end_though_read_just_now() {
        let p = Playbacks::default();
        let s = SessionId("s".into());
        let killed = p.connection_opened();
        let now = start_next_from(&p, "phone", killed);
        let next = start_next_from(&p, "phone", killed);
        p.use_handle(&now, &s, "phone", Using::Plays).unwrap();
        p.use_handle(&next, &s, "phone", Using::Plays).unwrap();
        p.connection_closed(killed);
        let relaunched = p.connection_opened();
        assert_eq!(
            p.make_room_for_next_where("phone", relaunched, |_| true),
            Ok(2)
        );
    }

    /// A killed app sends no close: its connection is still open when the
    /// app, opened again, starts on a newer one. What the older one read
    /// and the newer has not ends, read a moment ago or not, and gives
    /// way to the new start when no slot is free.
    #[test]
    fn an_older_connection_s_playbacks_end_at_a_newer_one_s_start() {
        let clock = Clock::default();
        let p = Playbacks::default().with_clock(clock.clone());
        let s = SessionId("s".into());
        let killed = p.connection_opened();
        let now = start_next_from(&p, "phone", killed);
        p.use_handle(&now, &s, "phone", Using::Plays).unwrap();
        clock.advance(Duration::from_millis(1));
        let relaunched = p.connection_opened();
        assert_eq!(p.end_conversions_on_where("phone", relaunched, |_| true), 1);
        // The older connection's own start leaves its playbacks alone.
        let mine = start_next_from(&p, "phone", killed);
        p.use_handle(&mine, &s, "phone", Using::Plays).unwrap();
        assert_eq!(p.end_conversions_on_where("phone", killed, |_| true), 0);
    }

    /// A device that reconnects and reads its playback on keeps it: read
    /// after its connection closed and the new one opened, it is not a
    /// killed app's.
    #[test]
    fn a_playback_read_after_a_reconnect_is_not_a_killed_app_s() {
        let clock = Clock::default();
        let p = Playbacks::default().with_clock(clock.clone());
        let s = SessionId("s".into());
        let before = p.connection_opened();
        let resumed = start_next_from(&p, "phone", before);
        p.connection_closed(before);
        clock.advance(Duration::from_secs(1));
        let back = p.connection_opened();
        clock.advance(Duration::from_secs(1));
        p.use_handle(&resumed, &s, "phone", Using::Plays).unwrap();
        let next = start_next_from(&p, "phone", back);
        assert!(p.use_handle(&resumed, &s, "phone", Using::Plays).is_ok());
        // Not read for a while (paused), it is still not a killed app's:
        // the next start ends the one never read instead.
        clock.advance(READING);
        let unread = start_next_from(&p, "phone", back);
        assert!(p.use_handle(&resumed, &s, "phone", Using::Plays).is_ok());
        assert_eq!(
            p.use_handle(&next, &s, "phone", Using::Plays).unwrap_err(),
            Refusal::Expired
        );
        assert!(p.use_handle(&unread, &s, "phone", Using::Plays).is_ok());
    }

    /// While the track before is still read as the next is prepared, a
    /// device holds a third, and no more: a fourth start, all three being
    /// read, is refused rather than ending one that plays.
    #[test]
    fn a_device_reading_all_it_holds_gets_a_third_and_no_fourth() {
        let clock = Clock::default();
        let p = Playbacks::default().with_clock(clock.clone());
        let s = SessionId("s".into());
        let mut held = Vec::new();
        for _ in 0..DEVICE_MOST {
            let h = start_next(&p, "phone");
            p.use_handle(&h, &s, "phone", Using::Plays).unwrap();
            held.push(h);
        }
        assert_eq!(
            p.make_room_for_next_where("phone", 0, |_| true),
            Err(DeviceFull)
        );
        for h in &held {
            assert!(p.use_handle(h, &s, "phone", Using::Plays).is_ok());
        }
        // Once one is no longer read, it makes the room.
        clock.advance(READING);
        p.use_handle(&held[1], &s, "phone", Using::Plays).unwrap();
        p.use_handle(&held[2], &s, "phone", Using::Plays).unwrap();
        assert_eq!(p.make_room_for_next_where("phone", 0, |_| true), Ok(1));
    }

    /// A playback paused for most of the idle limit is alive, and is not
    /// playing: with no slot free, the device's next start ends it rather
    /// than being refused as busy.
    #[test]
    fn a_long_paused_playback_lives_and_gives_way_to_its_device_s_next_start() {
        let clock = Clock::default();
        let p = Playbacks::default().with_clock(clock.clone());
        let s = SessionId("s".into());
        let paused = start_next(&p, "tv");
        p.use_handle(&paused, &s, "tv", Using::Plays).unwrap();
        clock.advance(Duration::from_secs(29 * 60));
        p.sweep();
        assert_eq!(p.counts().ended, 0, "ended while paused");
        assert_eq!(p.end_conversions_on_where("tv", 0, |_| true), 1);
        let why: Vec<EndReason> = p.ended().into_iter().map(|e| e.why).collect();
        assert_eq!(why, [EndReason::Replaced]);
    }

    /// Another device's playbacks are not this device's to make room in.
    #[test]
    fn making_room_on_one_device_leaves_another_s() {
        let p = Playbacks::default();
        let tv = start_next(&p, "tv");
        start_next(&p, "phone");
        start_next(&p, "phone");
        start_next(&p, "phone");
        assert!(
            p.use_handle(&tv, &SessionId("s".into()), "tv", Using::Plays)
                .is_ok()
        );
    }

    fn id_of(p: &Playbacks, endpoint: &str) -> String {
        p.lock()
            .values()
            .find(|pb| pb.endpoint == endpoint)
            .unwrap()
            .id
            .clone()
    }

    /// B-10: an ended session's playbacks end at once, its own only, and
    /// its client is told to sign in again rather than that they expired.
    #[test]
    fn an_ended_session_s_playbacks_end_and_are_refused_as_signed_out() {
        let p = Playbacks::default();
        let h = start_on(&p, "tv");
        let s = SessionId("s".into());
        let other = p.start(
            Playbacks::new_handle(),
            Opened {
                session: SessionId("other".into()),
                ..opened_on("laptop")
            },
        );
        assert_eq!(p.end_session(&s), 1);
        assert_eq!(
            p.use_handle(&h, &s, "tv", Using::Plays).unwrap_err(),
            Refusal::SignedOut
        );
        assert_eq!(
            p.check_progress(&h, 1).unwrap_err(),
            spoolframe_protocol::Error::Unauthenticated
        );
        assert!(
            p.use_handle(&other, &SessionId("other".into()), "laptop", Using::Plays)
                .is_ok()
        );
        assert_eq!(p.ended()[0].why, EndReason::SignedOut);
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
        assert_eq!(
            p.use_handle(&h, &s, "tv", Using::Plays).unwrap_err(),
            Refusal::Stopped
        );
        assert_eq!(
            p.check_progress(&h, 1).unwrap_err(),
            spoolframe_protocol::Error::PlaybackStopped
        );
        // Any other gone handle is still merely expired.
        assert_eq!(
            p.use_handle(&PlaybackHandle("nope".into()), &s, "tv", Using::Plays)
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
            year: None,
            air_date: None,
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
