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
use spoolframe_types::{Delivery, DisplayCapabilities, Item, MediaId, Platform, SubtitleFormat};
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
/// The most direct playbacks a device holds at once. A direct playback
/// holds no slot ([`DEVICE_MOST`] counts conversions only), but each is
/// a handle the server keeps for [`IDLE`]: past this, a new one ends the
/// device's least recently used that is not being read, so a client
/// starting playbacks in a loop cannot pile up handles without end.
/// Gapless audio needs two or three.
pub const DIRECT_MOST: usize = 16;
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
    /// `item` as the Playback page names it, `probed` long by the
    /// server's probe of its file, if it has one: its length is
    /// [`crate::watch::length`], the one its watch state and plays use.
    pub fn of(item: &Item, probed: Option<Duration>) -> Self {
        let duration_ms = crate::watch::length(probed, item).map(|d| d.as_millis() as u64);
        match item {
            Item::Movie(m) => Shown {
                title: m.title.clone(),
                subtitle: None,
                duration_ms,
            },
            Item::Episode(e) => Shown {
                title: match &e.show_title {
                    Some(show) => format!("{show} · S{} E{}", e.season, e.episode),
                    None => format!("S{} E{}", e.season, e.episode),
                },
                subtitle: Some(e.title.clone()),
                duration_ms,
            },
            Item::Track(t) => Shown {
                title: t.title.clone(),
                subtitle: t.artist_name.clone(),
                duration_ms,
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
    /// An earlier scan of the file, kept on disk.
    Stored,
}

impl KeyframesFrom {
    pub fn of(from: &spoolframe_media::KeyframesFrom) -> Self {
        match from {
            spoolframe_media::KeyframesFrom::Index(_) => KeyframesFrom::Index,
            spoolframe_media::KeyframesFrom::Scan { .. } => KeyframesFrom::Scan,
            spoolframe_media::KeyframesFrom::Stored => KeyframesFrom::Stored,
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
                    KeyframesFrom::Stored => "stored",
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

/// What "now + next" and conversion cleanup count a playback by
/// (P8-04).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Holder {
    /// A device: its endpoint.
    Endpoint(String),
    /// One of a paired server's users, whose plays share that server's
    /// one endpoint: their proxied session, by its hash.
    ProxiedSession(String),
}

impl Holder {
    /// A proxied session's: each of a paired server's users counts as a
    /// device of their own, so one's start never ends another's play.
    pub fn session_holder(session: &SessionId) -> Self {
        Self::ProxiedSession(crate::auth::SessionToken(session.0.clone()).hash())
    }
}

/// One playback.
#[derive(Debug, Clone)]
pub struct Playback {
    /// What playback debug (P3-15) names it by: random, and never the
    /// handle, which is a secret of the session's. Its spans' `playback.id`.
    pub id: String,
    /// What its spans link to.
    pub traced: PlaybackTrace,
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
    /// The film, or the optimized copy it plays or takes its audio from
    /// (PX-37): a copy is lent until the playback ends, so it is not
    /// evicted while played.
    pub source: Source,
    /// The quality it is planned for (P6-06), again at a subtitle change.
    pub quality: Quality,
    /// What the screen showed when it was planned (PGPU-06): a re-plan
    /// that does not say again plans for it.
    pub display: Option<DisplayCapabilities>,
    /// What the device that shows it plays, in place of its connection's
    /// Hello's (P8-04): a paired server's proxy plans for its own device.
    /// Its re-plans plan for it too.
    pub capabilities: Option<spoolframe_types::ClientCapabilities>,
    /// What "now + next" and conversion cleanup count it by (P8-04).
    holder: Holder,
    /// Where it plays from, when that is a paired server (P8-04, on the
    /// server whose user plays): its bytes come from there, and pass
    /// through.
    pub remote: Option<Arc<crate::federation::RemotePlay>>,
    /// The file's picture, width × height (PX-24): what direct play
    /// writes.
    pub video_size: Option<(u32, u32)>,
    /// How long it took to start (PX-24).
    pub start: Arc<Mutex<StartTimes>>,
    /// When its `StartPlayback` came.
    requested: Instant,
    /// A read of its media has been answered with bytes: the first one
    /// was timed, for the metrics.
    served: bool,
    /// What its device's connection carried while it was live, read every
    /// [`crate::link_stats::EVERY`]: logged when it ends, and its last
    /// interval shown in its detail.
    link: Arc<Mutex<crate::link_stats::Totals>>,
    /// The connection that started it (a number of the server's own).
    connection: u64,
    last_used: Instant,
    /// When its client last used it ([`READING`]); never at the start, so
    /// a playback prepared and not yet read is not counted as playing.
    last_read: Option<Instant>,
    /// When the connection that started it closed.
    orphaned_at: Option<Instant>,
    /// A player fetched it from the HTTP playback port (PX-49).
    over_http: bool,
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

    /// The display a re-plan of it is for (PGPU-06): the one `asked`
    /// gives, or else the one it was planned for.
    pub fn display_for(&self, asked: Option<DisplayCapabilities>) -> Option<DisplayCapabilities> {
        asked.or_else(|| self.display.clone())
    }

    /// The file a direct play of it reads: its copy's, when the copy plays
    /// in the film's place (PX-37), else the film.
    pub fn read_path(&self) -> &std::path::Path {
        self.source.read_path(&self.path)
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

/// What a playback reads (PX-37): the film, an optimized copy in its
/// place, or the film with a copy's audio. Decided once, as it is
/// planned; all else asks this. A copy is the film and its stored tracks
/// read together (the track store): direct play always reads the film.
#[derive(Debug, Clone, Default)]
pub enum Source {
    #[default]
    Film,
    /// The copy plays in the film's place, remuxed.
    WholeCopy(CopyUse),
    /// The film plays, its audio taken from the copy.
    AudioFrom(CopyUse),
}

impl Source {
    /// The file it reads, the film being at `film`: always the film, as a
    /// copy is never one file to read by range.
    pub fn read_path<'a>(&'a self, film: &'a std::path::Path) -> &'a std::path::Path {
        film
    }

    /// The stored video it plays, if the copy gives it one.
    pub fn video_from(&self) -> Option<&std::path::Path> {
        match self {
            Self::WholeCopy(copy) => copy.video.as_deref(),
            Self::Film | Self::AudioFrom(_) => None,
        }
    }

    /// The stored audio track it plays, if the copy gives it one.
    pub fn audio_from(&self) -> Option<&std::path::Path> {
        self.copy().and_then(|copy| copy.audio.as_deref())
    }

    /// The copy lent, either way.
    fn copy(&self) -> Option<&CopyUse> {
        match self {
            Self::Film => None,
            Self::WholeCopy(copy) | Self::AudioFrom(copy) => Some(copy),
        }
    }

    /// The copy's id, if a copy is lent.
    pub fn copy_id(&self) -> Option<&str> {
        self.copy().map(|copy| copy.lease.id())
    }

    /// The tracks it takes from its copy, if any: its stored ones.
    pub fn tracks(&self) -> Option<CopyTracks> {
        let copy = self.copy()?;
        Some(CopyTracks {
            video: self.video_from().is_some(),
            audio: copy.audio.is_some(),
            subtitle: false,
        })
    }

    /// Which copy it plays from and which of its tracks, as a snapshot
    /// shows it.
    pub fn used(&self) -> Option<CopyUsed> {
        Some(CopyUsed {
            id: self.copy_id()?.to_owned(),
            target: self.copy()?.target.clone(),
            tracks: self.tracks()?,
        })
    }
}

/// An optimized copy a playback plays from (PX-37), lent to it with
/// every track it is made of.
#[derive(Debug, Clone)]
pub struct CopyUse {
    lease: Arc<crate::optimized::Lease>,
    /// Its profile's name, or `ahead`.
    target: String,
    /// Its stored video, when it plays in the film's place with one.
    video: Option<std::path::PathBuf>,
    /// Its stored track for the picked audio, if that one is stored.
    audio: Option<std::path::PathBuf>,
}

impl CopyUse {
    /// `lease`d copy made for `target`, playing its stored video if
    /// `whole`, and its stored track for the film's audio stream
    /// `audio` (a stream index), if it has one.
    pub fn new(
        lease: crate::optimized::Lease,
        target: &crate::store::optimized::CopyTarget,
        whole: bool,
        audio: Option<u32>,
    ) -> Self {
        let files = lease.files();
        Self {
            video: files.video.clone().filter(|_| whole),
            audio: audio.and_then(|stream| files.audio.get(&stream).cloned()),
            target: target.label(),
            lease: Arc::new(lease),
        }
    }
}

/// Which copy a playback plays from, and which of its tracks (PX-37).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyUsed {
    pub id: String,
    /// Its profile's name, or `ahead`.
    pub target: String,
    pub tracks: CopyTracks,
}

/// The tracks a playback takes from a copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyTracks {
    pub video: bool,
    pub audio: bool,
    pub subtitle: bool,
}

/// A playback to open.
#[derive(Debug)]
pub struct Opened {
    /// Its id and its `playback` span ([`PlaybackTrace::begin`]).
    pub trace: PlaybackTrace,
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
    /// The film, or the optimized copy it plays from (PX-37).
    pub source: Source,
    /// The quality the viewer chose (P6-06).
    pub quality: Quality,
    /// What the screen shows (PGPU-06), if the start said.
    pub display: Option<DisplayCapabilities>,
    /// What the device plays, in place of Hello's (P8-04), if the start
    /// said.
    pub capabilities: Option<spoolframe_types::ClientCapabilities>,
    /// What it is counted by (P8-04): its endpoint, or a paired server's
    /// proxied session.
    pub holder: Holder,
    /// The paired server it plays from (P8-04).
    pub remote: Option<Arc<crate::federation::RemotePlay>>,
    /// The file's picture, width × height.
    pub video_size: Option<(u32, u32)>,
    /// How long it took to start (PX-24); its first segment filled in
    /// once ready.
    pub start: Arc<Mutex<StartTimes>>,
    /// When its `StartPlayback` came.
    pub requested: Instant,
    /// The connection starting it ([`Playbacks::connection_closed`]).
    pub connection: u64,
    /// What it plays and for whom, for its observers ([`Observer`]).
    pub played: Played,
}

/// What a playback plays, for whom, and from where: what its observers
/// ([`Observer::started`]) need beyond what it serves. Its endpoint, its
/// device's name and its id are [`Opened`]'s own.
#[derive(Debug, Clone)]
pub struct Played {
    pub principal: crate::principal::Principal,
    /// The item as the catalog has it: its kind, and its show, or its
    /// album and artist.
    pub item: Item,
    /// Its title's key ([`crate::watch::Watch::title_key`]): what its play
    /// is kept with.
    pub title_key: Vec<String>,
    /// Its length ([`crate::watch::length`]).
    pub duration: Option<Duration>,
    /// Where its credits that run to the end start, when known.
    pub credits: Option<Duration>,
    /// Where in it the playback starts.
    pub from: Duration,
}

/// What follows each playback through its life, as play history does
/// (PX-52): told as it starts, as its client reports where it is, and as
/// it ends, however it ends. Each call is made on the path that changes
/// the playback (a request, the sweep), and can neither fail nor hold it
/// up: an observer does what it writes in the background.
pub trait Observer: Send + Sync {
    /// `handle` is about to be registered as `opened`. Nothing can end it
    /// before this returns, so whatever ends it finds what this keeps.
    fn started(&self, handle: &str, opened: &Opened);
    /// Its client reported `position`. `over_http`: a player fetched it
    /// from the HTTP playback port (PX-49).
    fn progressed(&self, handle: &str, position: Duration, over_http: bool);
    /// It ended: its client stopped it, it expired, an admin stopped it,
    /// its session or device ended, another took its place.
    fn ended(&self, handle: &str, over_http: bool);

    /// The idle playbacks were just ended ([`Playbacks::sweep`]): one that
    /// keeps plays of its own, with no playback here (a paired server's
    /// item, P8-03), ends its idle ones on the same beat.
    fn swept(&self, _idle: Duration) {}
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
    pub source: Source,
    pub quality: Quality,
    /// What the screen shows (PGPU-06): the change's, or the playback's.
    pub display: Option<DisplayCapabilities>,
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
pub struct Use<T = Playback> {
    /// What the caller read of the playback ([`Playbacks::use_handle_with`]):
    /// all of it, by default.
    pub playback: T,
    /// What it plays, for a recheck.
    pub item: MediaId,
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
    /// Called with each handle as its playback ends, however it ends:
    /// the uplink's share is given back (PX-46), its HTTP token ends
    /// (PX-49).
    on_end: parking_lot::RwLock<Vec<OnEnd>>,
    /// Told of each playback's start, progress and end ([`Observer`]).
    observers: parking_lot::RwLock<Vec<Arc<dyn Observer>>>,
    /// Where the time to a playback's first media goes (P3-14).
    histograms: std::sync::OnceLock<Arc<crate::metrics::Histograms>>,
}

/// What [`Playbacks::on_end`] calls.
pub type EndHook = Box<dyn Fn(&str) + Send + Sync>;

struct OnEnd(EndHook);

impl std::fmt::Debug for dyn Observer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Observer")
    }
}

impl std::fmt::Debug for OnEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OnEnd")
    }
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
    /// The optimized copy it plays from (PX-37).
    pub copy: Option<CopyUsed>,
    /// The file's picture, width × height.
    pub video_size: Option<(u32, u32)>,
    pub start: StartTimes,
    /// Its device's link so far.
    pub link: crate::link_stats::Totals,
    /// A player fetched it from the HTTP playback port (PX-49).
    pub over_http: bool,
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
            copy: self.source.used(),
            video_size: self.video_size,
            start: *self.start.lock(),
            link: self.link.lock().clone(),
            over_http: self.over_http,
        }
    }
}

/// A random id for playback debug: 8 hex digits.
fn debug_id() -> String {
    format!("{:08x}", OsRng.next_u32())
}

/// What ties a playback's spans together: its id (playback debug's), and
/// the context of its `playback` span, which every later span about it
/// links to. Not one trace for the whole playback: that needs a span open
/// as long as the playback, and Tempo gets a span only when it ends.
#[derive(Debug, Clone)]
pub struct PlaybackTrace {
    pub id: String,
    context: opentelemetry::trace::SpanContext,
}

impl PlaybackTrace {
    /// A playback starting now: its trace, and its `playback` span, a root
    /// that ends as the start does. Its plan and how the start went are
    /// recorded on it then.
    pub fn begin() -> (Self, tracing::Span) {
        use opentelemetry::trace::TraceContextExt as _;
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;
        let id = debug_id();
        let span = tracing::info_span!(
            parent: None,
            "playback",
            playback.id = %id,
            playback.plan = tracing::field::Empty,
            playback.delivery = tracing::field::Empty,
            playback.start_ms = tracing::field::Empty,
            outcome = tracing::field::Empty,
        );
        let context = span.context().span().span_context().clone();
        (Self { id, context }, span)
    }

    /// A playback no span is about: a test's.
    pub fn untraced() -> Self {
        Self {
            id: debug_id(),
            context: opentelemetry::trace::SpanContext::empty_context(),
        }
    }

    /// Mark `span` as about this playback: its id, and a link to its
    /// `playback` span.
    pub fn mark(&self, span: &tracing::Span) {
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;
        span.set_attribute("playback.id", self.id.clone());
        if self.context.is_valid() {
            span.add_link(self.context.clone());
        }
    }
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
            on_end: parking_lot::RwLock::default(),
            observers: parking_lot::RwLock::default(),
            histograms: std::sync::OnceLock::new(),
        }
    }

    /// Observe the time to each playback's first media into `histograms`
    /// (P3-14).
    pub fn record_into(&self, histograms: Arc<crate::metrics::Histograms>) {
        let _ = self.histograms.set(histograms);
    }

    /// Call `hook` with each playback's handle as it ends, after the
    /// hooks added before it.
    pub fn on_end(&self, hook: EndHook) {
        self.on_end.write().push(OnEnd(hook));
    }

    /// Tell `observer` of every playback's start, progress and end from
    /// now, after the observers added before it.
    pub fn observe(&self, observer: Arc<dyn Observer>) {
        self.observers.write().push(observer);
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
    /// What a client using `handle`, which has ended, is told.
    pub fn gone_error(&self, handle: &str) -> spoolframe_protocol::Error {
        match self.gone(handle) {
            Refusal::Stopped => spoolframe_protocol::Error::PlaybackStopped,
            Refusal::SignedOut => spoolframe_protocol::Error::Unauthenticated,
            _ => spoolframe_protocol::Error::PlaybackExpired,
        }
    }

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
    ) -> Result<crate::keyed_locks::Guard, Refusal> {
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
        // Before it is registered: nothing can end it until it is.
        for observer in self.observers.read().iter() {
            observer.started(&handle.0, &opened);
        }
        let now = self.clock.now();
        let mut handles = self.lock();
        // Forget idle handles here rather than on a timer.
        let idle = take_where(&mut handles, |p| {
            now.duration_since(p.last_used) >= self.idle
        });
        self.started.inc();
        // Under the lock, so no two playbacks share one.
        let order = self.started.get();
        for previous in handles.values_mut() {
            if previous.session == opened.session && previous.item == opened.item {
                previous.progress_superseded = true;
            }
        }
        // A direct playback past the device's DIRECT_MOST ends its least
        // recently used one that is not being read (review 2026-10-11 M11).
        let mut surplus: Vec<String> = Vec::new();
        if opened.conversion.is_none() {
            let mut direct: Vec<(&String, &Playback)> = handles
                .iter()
                .filter(|(_, p)| p.holder == opened.holder && p.conversion.is_none())
                .collect();
            let over = (direct.len() + 1).saturating_sub(DIRECT_MOST);
            direct.retain(|(_, p)| !p.reading(now));
            direct.sort_by_key(|(_, p)| (p.last_read, p.last_used, p.order));
            surplus = direct
                .iter()
                .take(over)
                .map(|(h, _)| (*h).clone())
                .collect();
        }
        let surplus: Vec<(String, Playback)> = surplus
            .into_iter()
            .filter_map(|h| handles.remove_entry(&h))
            .collect();
        handles.insert(
            handle.0.clone(),
            Playback {
                id: opened.trace.id.clone(),
                traced: opened.trace,
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
                source: opened.source,
                quality: opened.quality,
                display: opened.display,
                capabilities: opened.capabilities,
                holder: opened.holder.clone(),
                remote: opened.remote,
                video_size: opened.video_size,
                start: opened.start,
                requested: opened.requested,
                served: false,
                link: Arc::default(),
                connection: opened.connection,
                last_used: now,
                last_read: None,
                orphaned_at: None,
                over_http: false,
                last_checked: now,
            },
        );
        drop(handles);
        self.ended_all(idle, EndReason::Idle);
        self.ended_all(surplus, EndReason::Replaced);
        handle
    }

    /// Play `handle` as `changed` from now (a subtitle change, P2-07). The
    /// conversion it replaces is ended here and returned, to be dropped
    /// outside the lock: dropping its last clone removes its files.
    pub fn change(&self, handle: &PlaybackHandle, changed: Changed) -> Option<Arc<Conversion>> {
        let replaced = self.swap(handle, changed);
        // Its ffmpeg stops and its slot is let go of now, as
        // `ended_all` does: a read still waiting holds the conversion, and
        // would keep both until it gave up, beside the new one already
        // running. Outside the lock, which `end` does not need.
        if let Some(conversion) = &replaced {
            conversion.end();
        }
        replaced
    }

    fn swap(&self, handle: &PlaybackHandle, changed: Changed) -> Option<Arc<Conversion>> {
        let mut handles = self.lock();
        let playback = handles.get_mut(&handle.0)?;
        playback.revision += 1;
        playback.planned = changed.planned;
        playback.selection = changed.selection;
        playback.size = changed.size;
        playback.file = changed.file;
        playback.subtitle = changed.subtitle;
        playback.quality = changed.quality;
        playback.display = changed.display;
        // The copy it played is let go of once the new conversion runs:
        // the old one, which may read it, is ended by then.
        playback.source = changed.source;
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
        self.use_handle_with(handle, session, endpoint, using, Playback::clone)
    }

    /// [`Playbacks::use_handle`], reading only what `read` takes of the
    /// playback, under the lock: a range read needs its revision and its
    /// file, not a copy of everything a playback holds, several times a
    /// second per client (review 2026-10-11 M10).
    pub fn use_handle_with<T>(
        &self,
        handle: &PlaybackHandle,
        session: &SessionId,
        endpoint: &str,
        using: Using,
        read: impl FnOnce(&Playback) -> T,
    ) -> Result<Use<T>, Refusal> {
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
            playback: read(playback),
            item: playback.item.clone(),
            recheck,
        })
    }

    /// What `handle`'s spans link to, if it is open. Not a use of it.
    pub fn traced(&self, handle: &PlaybackHandle) -> Option<PlaybackTrace> {
        self.lock().get(&handle.0).map(|p| p.traced.clone())
    }

    /// What `handle` plays, if it is open: for the log, which never names
    /// the handle itself. Not a use of it.
    pub fn item(&self, handle: &PlaybackHandle) -> Option<MediaId> {
        self.lock().get(&handle.0).map(|p| p.item.clone())
    }

    /// What binge-ahead (PX-37) looks at of `handle`, if it is open. Not a
    /// use of it.
    pub fn binge_view(&self, handle: &PlaybackHandle) -> Option<crate::optimized::ahead::Seen> {
        self.lock()
            .get(&handle.0)
            .map(|p| crate::optimized::ahead::Seen {
                plan: p.planned.clone(),
                caught_up: p.conversion.as_ref().is_some_and(|c| c.caught_up()),
                signal: p.conversion.as_ref().map(|c| c.caught_up_signal()),
                copy: match &p.source {
                    Source::WholeCopy(copy) => Some(copy.lease.id().to_owned()),
                    Source::Film | Source::AudioFrom(_) => None,
                },
            })
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
    /// conversion had got, go on its chart. The first such read with
    /// bytes, but for the init segment, is its time to first media
    /// (P3-14).
    pub fn read(&self, handle: &PlaybackHandle, offset: u64, sent: u64, segment: Option<u32>) {
        // What the chart needs, not the whole playback (M10).
        let Some((trace, conversion, duration_ms, size, first)) =
            self.lock().get_mut(&handle.0).map(|p| {
                let first = (!p.served && sent > 0 && segment != Some(0)).then(|| {
                    p.served = true;
                    (p.plan(), p.requested.elapsed())
                });
                (
                    p.trace.clone(),
                    p.conversion.clone(),
                    p.shown.duration_ms,
                    p.size,
                    first,
                )
            })
        else {
            return;
        };
        if let (Some((plan, took)), Some(histograms)) = (first, self.histograms.get()) {
            histograms.observe(
                &crate::metrics::histogram::PLAYBACK_FIRST_SEGMENT,
                took.as_secs_f64(),
                &[("plan", plan.as_str())],
            );
        }
        let (epoch, read_ms, converted_ms, mark) = match &conversion {
            None => {
                // Bounded today by every caller, but not by this type.
                let to = offset.saturating_add(sent);
                let read_ms = duration_ms
                    .filter(|_| size > 0)
                    .map(|d| (u128::from(d) * u128::from(to) / u128::from(size)) as u64);
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
                        Furthest::Byte(offset.saturating_add(sent)),
                    ),
                }
            }
        };
        let point = ReadPoint {
            at: unix_ms(),
            read_ms,
            converted_ms,
        };
        trace.lock().read(point, epoch, mark);
    }

    /// Keep `quality` and `display` for `handle`'s next re-plan, its plan
    /// unchanged: not a change of configuration, so the revision stays.
    pub fn set_quality(
        &self,
        handle: &PlaybackHandle,
        quality: Quality,
        display: Option<DisplayCapabilities>,
    ) {
        if let Some(playback) = self.lock().get_mut(&handle.0) {
            playback.quality = quality;
            playback.display = display;
        }
    }

    /// A player fetched `handle`'s playback from the HTTP playback port
    /// (PX-49): its activity row says so from now.
    pub fn served_over_http(&self, handle: &PlaybackHandle) {
        if let Some(playback) = self.lock().get_mut(&handle.0) {
            playback.over_http = true;
        }
    }

    /// `handle`'s revision now, if it is open.
    pub fn revision_of(&self, handle: &PlaybackHandle) -> Option<u64> {
        self.lock().get(&handle.0).map(|playback| playback.revision)
    }

    /// A playback played from a paired server through this one (P8-04),
    /// changed as that server answered a re-plan: `change` sets what it
    /// told.
    pub fn relayed(&self, handle: &PlaybackHandle, change: impl FnOnce(&mut Playback)) {
        if let Some(playback) = self.lock().get_mut(&handle.0) {
            change(playback);
        }
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
    ) -> crate::keyed_locks::Guard {
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
            return Err(self.gone_error(&handle.0));
        };
        if playback.progress_superseded || sequence <= playback.progress_sequence {
            return Err(spoolframe_protocol::Error::StaleProgress);
        }
        Ok(())
    }

    /// The client reported `position`: its observers are told. Before its
    /// watch state, which a catalog that is down can fail
    /// ([`Self::record_progress`] after it).
    pub(crate) fn progressed(&self, handle: &PlaybackHandle, position: Duration) {
        let Some(over_http) = self.lock().get(&handle.0).map(|p| p.over_http) else {
            return;
        };
        for observer in self.observers.read().iter() {
            observer.progressed(&handle.0, position, over_http);
        }
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

    /// End `handle` as stopped, as an admin's Stop does: its client is
    /// told `PlaybackStopped` on its next use, so it does not start it
    /// again. A paired server's playback that one stopped (P8-04).
    pub fn end_stopped(&self, handle: &PlaybackHandle, why: EndReason) {
        let mut handles = self.lock();
        let ended = handles.remove_entry(&handle.0);
        if ended.is_some() {
            self.let_go
                .lock()
                .insert(handle.0.clone(), (self.clock.now(), Refusal::Stopped));
        }
        drop(handles);
        self.ended_all(ended, why);
    }

    /// End `holder`'s (its endpoint, or a paired server's proxied
    /// session, P8-04) converted playbacks that are not playing: it is
    /// starting another from connection `starting`, and no slot is free
    /// for it, so its own idle encodes give way (one device's two
    /// playbacks with `max_jobs = 1`). A killed app's always does
    /// (`Playback::left_behind`); one its client used in the last
    /// [`READING`] is playing and stays, and the new start is then refused
    /// as busy: a track that stops mid-play is worse than a next track
    /// that waits. Returns how many.
    pub fn end_conversions_on(&self, holder: &Holder, starting: u64) -> usize {
        self.end_conversions_on_where(holder, starting, |p| p.conversion.is_some())
    }

    /// [`Playbacks::end_conversions_on`], counting the playbacks `holds`
    /// picks.
    fn end_conversions_on_where(
        &self,
        holder: &Holder,
        starting: u64,
        holds: impl Fn(&Playback) -> bool,
    ) -> usize {
        let now = self.clock.now();
        let starting = self.opened(starting);
        self.end_where(
            |p| &p.holder == holder && holds(p) && (p.left_behind(starting) || !p.reading(now)),
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

    /// Make room on `holder` (its endpoint, or a paired server's proxied
    /// session, P8-04) for a playback that connection `starting`
    /// is starting: a device keeps "now + next" ([`NOW_AND_NEXT`]), the
    /// one playing now and the next it prepares (an iOS app opens the next
    /// track while this one plays, for gapless audio). A killed app's
    /// playbacks all end first, however lately they were read
    /// (`Playback::left_behind`): the app opened again can use none of
    /// them. Of the others, enough end to keep one besides the new start,
    /// those used least recently first, never used first, the oldest first
    /// among equals. One used in the last [`READING`] is playing and never
    /// ends; the one starting is not open yet, so it cannot be chosen.
    /// With too few to end the device holds more than two, up to
    /// [`DEVICE_MOST`]: past that the start is refused ([`DeviceFull`]).
    /// Direct play holds no slot, and is left alone. Returns how many
    /// ended.
    pub fn make_room_for_next(&self, holder: &Holder, starting: u64) -> Result<usize, DeviceFull> {
        self.make_room_for_next_where(holder, starting, |p| p.conversion.is_some())
    }

    /// [`Playbacks::make_room_for_next`], counting the playbacks `holds`
    /// picks.
    fn make_room_for_next_where(
        &self,
        holder: &Holder,
        starting: u64,
        holds: impl Fn(&Playback) -> bool,
    ) -> Result<usize, DeviceFull> {
        let now = self.clock.now();
        let starting = self.opened(starting);
        let mut handles = self.lock();
        let (left_behind, held): (Vec<(&String, &Playback)>, Vec<_>) = handles
            .iter()
            .filter(|(_, p)| &p.holder == holder && holds(p))
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
        self.end_device_but(endpoint, |_| false)
    }

    /// [`Playbacks::end_device`], but for the playbacks whose handle
    /// `kept` keeps: something besides the device still plays them (an
    /// AirPlay receiver over the HTTP playback port, PX-49). Those end
    /// as any playback does: stopped, signed out, or idle. `kept` is asked
    /// with no lock held.
    pub fn end_device_but(&self, endpoint: &str, kept: impl Fn(&str) -> bool) -> usize {
        let of_device: Vec<String> = self
            .lock()
            .iter()
            .filter(|(_, p)| p.endpoint == endpoint)
            .map(|(h, _)| h.clone())
            .collect();
        let ending: Vec<String> = of_device.into_iter().filter(|h| !kept(h)).collect();
        let mut handles = self.lock();
        let ended: Vec<(String, Playback)> = ending
            .iter()
            .filter_map(|h| handles.remove_entry(h))
            .collect();
        drop(handles);
        let count = ended.len();
        self.ended_all(ended, EndReason::DeviceGone);
        count
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

    /// Whether `user_id` plays `item` here now: a paired server's item
    /// played through this server (P8-04) is such a play, its play kept
    /// by its playback.
    pub fn playing(&self, user_id: &str, item: &MediaId) -> bool {
        self.lock()
            .values()
            .any(|p| p.viewer.user_id == user_id && p.item == *item)
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
        let ended = take_where(&mut handles, |p| &p.session == session);
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
        let ended = take_where(&mut handles, ends);
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
        for observer in self.observers.read().iter() {
            observer.swept(self.idle);
        }
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
            log_link(&playback.id, &last.link);
            for OnEnd(hook) in self.on_end.read().iter() {
                hook(&handle);
            }
            for observer in self.observers.read().iter() {
                observer.ended(&handle, playback.over_http);
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

    /// Whether `endpoint` (hex) has a playback open: its connection's link
    /// is then looked at every [`crate::link_stats::EVERY`].
    pub fn live_on(&self, endpoint: &str) -> bool {
        self.lock().values().any(|p| p.endpoint == endpoint)
    }

    /// Add one interval of `endpoint`'s connection to each of its open
    /// playbacks' link totals.
    pub fn record_link(
        &self,
        endpoint: &str,
        interval: &crate::link_stats::Interval,
        path: &crate::link_stats::PathNow,
    ) {
        let links: Vec<_> = self
            .lock()
            .values()
            .filter(|p| p.endpoint == endpoint)
            .map(|p| p.link.clone())
            .collect();
        for link in links {
            link.lock().add(interval, path);
        }
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
        // Searched where it is kept, newest first as `ended` lists it: one
        // cloned, not every ended playback.
        let mut gone = self.gone_lock();
        Self::prune(&mut gone, unix_ms());
        gone.iter()
            .rev()
            .find(|e| e.last.id == id)
            .cloned()
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

/// The line that sums up a playback's link when it ends, next to "a
/// playback ended": every end, a client's own included, since a stalled
/// one is often stopped by its viewer. Nothing when no interval was read
/// (it ended within the first [`crate::link_stats::EVERY`]).
fn log_link(playback: &str, link: &crate::link_stats::Totals) {
    if link.intervals == 0 {
        return;
    }
    tracing::info!(
        target: "spoolframe_server::link",
        playback,
        secs = %format_args!("{:.0}", link.secs),
        send_mbps = %format_args!("{:.2}", link.send_mbps()),
        max_rtt_ms = link.max_rtt.as_millis() as u64,
        lost_pct = %format_args!("{:.2}", link.loss_percent()),
        lost_packets = link.lost_packets,
        congestion = link.congestion_events,
        switches = link.path_switches,
        lan_secs = %format_args!("{:.0}", link.lan_secs),
        direct_secs = %format_args!("{:.0}", link.direct_secs),
        relay_secs = %format_args!("{:.0}", link.relay_secs),
        "playback link"
    );
}

/// Remove and return every playback `picks` picks.
fn take_where(
    handles: &mut HashMap<String, Playback>,
    picks: impl Fn(&Playback) -> bool,
) -> Vec<(String, Playback)> {
    let keys: Vec<String> = handles
        .iter()
        .filter(|(_, p)| picks(p))
        .map(|(h, _)| h.clone())
        .collect();
    keys.into_iter()
        .filter_map(|h| handles.remove_entry(&h))
        .collect()
}
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::clock::Clock;

    fn start(p: &Playbacks) -> PlaybackHandle {
        start_on(p, "e")
    }

    /// A device starting direct playbacks in a loop keeps at most
    /// DIRECT_MOST, the newest; another device's are not touched (review
    /// 2026-10-11 M11).
    #[test]
    fn a_device_holds_at_most_direct_most_direct_playbacks() {
        let p = Playbacks::default();
        let other = start_on(&p, "f");
        let handles: Vec<PlaybackHandle> = (0..DIRECT_MOST + 10).map(|_| start(&p)).collect();
        let open = |endpoint: &str| p.lock().values().filter(|x| x.endpoint == endpoint).count();
        assert_eq!(open("e"), DIRECT_MOST);
        assert_eq!(open("f"), 1);
        assert!(p.lock().contains_key(&handles.last().unwrap().0));
        assert!(!p.lock().contains_key(&handles[0].0));
        assert!(p.lock().contains_key(&other.0));
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
    /// The holder a device with `endpoint` is.
    fn device(endpoint: &str) -> Holder {
        Holder::Endpoint(endpoint.into())
    }

    pub(crate) fn opened_on(endpoint: &str) -> Opened {
        Opened {
            trace: PlaybackTrace::untraced(),
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
            source: Source::Film,
            quality: Quality::default(),
            video_size: None,
            start: Arc::new(Mutex::new(StartTimes {
                plan: Duration::ZERO,
                keyframes: None,
                first_segment: FirstSegment::Direct,
            })),
            requested: Instant::now(),
            connection: 0,
            display: None,
            capabilities: None,
            holder: Holder::Endpoint(endpoint.into()),
            remote: None,
            played: played(),
        }
    }

    /// The movie, from its start, for Alice.
    pub(crate) fn played() -> Played {
        Played {
            principal: crate::principal::Principal::LocalUser(
                crate::store::users::LocalUserId::from_uuid(uuid::Uuid::nil()),
            ),
            item: crate::test_support::movie(),
            title_key: Vec::new(),
            duration: Some(Duration::from_secs(10)),
            credits: None,
            from: Duration::ZERO,
        }
    }

    /// A change ends the conversion it replaces, though a read still
    /// waiting holds it: its ffmpeg and its slot go with the change, not
    /// when that read gives up. Review 2026-10-11 M7.
    #[tokio::test]
    async fn a_change_ends_the_conversion_it_replaces() {
        use spoolframe_types::VideoCodec;
        let (old, _a) = crate::encode::tests::resumed_ladder_of(
            "spoolframe-change-ends-old",
            VideoCodec::H264,
            "h264_vaapi",
        )
        .await;
        let (new, _b) = crate::encode::tests::resumed_ladder_of(
            "spoolframe-change-ends-new",
            VideoCodec::H264,
            "h264_vaapi",
        )
        .await;
        let p = Playbacks::default();
        let h = p.start(
            Playbacks::new_handle(),
            Opened {
                conversion: Some(old.clone()),
                ..opened_on("e")
            },
        );
        let waiting = old.clone();
        let replaced = p.change(
            &h,
            Changed {
                planned: PlaybackPlan::Unknown,
                selection: Selection::default(),
                size: 0,
                file: None,
                subtitle: None,
                conversion: Some(new.clone()),
                source: Source::Film,
                quality: Quality::default(),
                display: None,
            },
        );
        drop(replaced);
        assert!(waiting.ended());
        assert!(!new.ended());
    }

    /// A use can read only what it needs of the playback, under the lock,
    /// and still counts as a use: a range read takes its revision, not a
    /// copy of the whole playback. Review 2026-10-11 M10.
    #[test]
    fn a_use_reads_only_what_it_asks_for() {
        let p = Playbacks::default();
        let h = start(&p);
        let used = p
            .use_handle_with(&h, &SessionId("s".into()), "e", Using::Plays, |p| {
                (p.revision, p.size)
            })
            .unwrap();
        assert_eq!(used.playback, (0, 10));
        assert_eq!(used.item.to_string(), "plex:movie:1");
        assert!(
            p.use_handle_with(&h, &SessionId("other".into()), "e", Using::Plays, |p| p
                .revision)
                .is_err()
        );
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

    /// A re-plan keeps the display the playback started with unless it
    /// gives another, which it then keeps (PGPU-06); a change of quality
    /// that plans the same keeps it too.
    #[test]
    fn a_playback_keeps_its_display_until_a_re_plan_gives_another() {
        let p = Playbacks::default();
        let sdr = DisplayCapabilities {
            bit_depth: Some(8),
            hdr: Some(std::collections::BTreeSet::new()),
            video_codecs: None,
        };
        let hdr = DisplayCapabilities {
            bit_depth: Some(10),
            hdr: Some([spoolframe_types::HdrFormat::Hdr10].into()),
            video_codecs: None,
        };
        let h = p.start(
            Playbacks::new_handle(),
            Opened {
                display: Some(sdr.clone()),
                ..opened_on("e")
            },
        );
        let using = |p: &Playbacks| {
            p.use_handle(&h, &SessionId("s".into()), "e", Using::Plays)
                .unwrap()
                .playback
        };
        assert_eq!(using(&p).display_for(None), Some(sdr.clone()));
        assert_eq!(using(&p).display_for(Some(hdr.clone())), Some(hdr.clone()));
        p.set_quality(&h, Quality::default(), Some(hdr.clone()));
        assert_eq!(using(&p).display_for(None), Some(hdr.clone()));
        p.change(
            &h,
            Changed {
                planned: PlaybackPlan::Unknown,
                selection: Selection::default(),
                size: 10,
                file: None,
                subtitle: None,
                conversion: None,
                source: Source::Film,
                quality: Quality::default(),
                display: Some(sdr.clone()),
            },
        );
        assert_eq!(using(&p).display_for(None), Some(sdr));
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
        assert_eq!(p.end_conversions_on(&device("tv"), 0), 0);
        assert_eq!(p.make_room_for_next(&device("tv"), 0), Ok(0));
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
        p.make_room_for_next_where(&device(endpoint), connection, |_| true)
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
        p.make_room_for_next_where(&device("phone"), 0, |_| true)
            .unwrap();
        p.end_conversions_on_where(&device("phone"), 0, |_| true);
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

    /// A paired server plays its users' items through its one endpoint
    /// (P8-04), each play on a connection of its own and in a session of
    /// its own: each session is its own "now + next", so one user's start
    /// does not take another's playback for a killed app's and end it,
    /// nor does it end their conversions for room.
    #[test]
    fn a_paired_server_s_users_do_not_end_each_other_s_plays() {
        let p = Playbacks::default();
        let s = SessionId("s".into());
        let start = |session: &str| {
            let holder = Holder::session_holder(&SessionId(session.into()));
            let connection = p.connection_opened();
            p.make_room_for_next_where(&holder, connection, |_| true)
                .expect("room on the session");
            p.end_conversions_on_where(&holder, connection, |_| true);
            p.start(
                Playbacks::new_handle(),
                Opened {
                    connection,
                    holder,
                    ..opened_on("sb")
                },
            )
        };
        let bob = start("bob");
        let carol = start("carol");
        let dave = start("dave");
        for handle in [&bob, &carol, &dave] {
            assert!(p.use_handle(handle, &s, "sb", Using::Plays).is_ok());
        }
        assert!(p.ended().is_empty());
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
            p.make_room_for_next_where(&device("phone"), relaunched, |_| true),
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
        assert_eq!(
            p.end_conversions_on_where(&device("phone"), relaunched, |_| true),
            1
        );
        // The older connection's own start leaves its playbacks alone.
        let mine = start_next_from(&p, "phone", killed);
        p.use_handle(&mine, &s, "phone", Using::Plays).unwrap();
        assert_eq!(
            p.end_conversions_on_where(&device("phone"), killed, |_| true),
            0
        );
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
            p.make_room_for_next_where(&device("phone"), 0, |_| true),
            Err(DeviceFull)
        );
        for h in &held {
            assert!(p.use_handle(h, &s, "phone", Using::Plays).is_ok());
        }
        // Once one is no longer read, it makes the room.
        clock.advance(READING);
        p.use_handle(&held[1], &s, "phone", Using::Plays).unwrap();
        p.use_handle(&held[2], &s, "phone", Using::Plays).unwrap();
        assert_eq!(
            p.make_room_for_next_where(&device("phone"), 0, |_| true),
            Ok(1)
        );
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
        assert_eq!(p.end_conversions_on_where(&device("tv"), 0, |_| true), 1);
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

    /// A read's end is saturated, not wrapped: no caller sends one past
    /// the end of a u64, but nothing here relies on that.
    #[test]
    fn a_read_at_the_end_of_a_u64_saturates() {
        let p = Playbacks::default();
        let h = start_on(&p, "tv");
        p.read(&h, u64::MAX, 5, None);
        let snap = p.snapshots().remove(0);
        assert_eq!(snap.furthest, Some(Furthest::Byte(u64::MAX)));
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
            cast: Vec::new(),
            artwork: Vec::new(),
            playback: Default::default(),
        });
        let shown = Shown::of(&item, None);
        assert_eq!(shown.title, "Severance · S2 E4");
        assert_eq!(shown.subtitle.as_deref(), Some("Woe's Hollow"));
        assert_eq!(shown.duration_ms, Some(3_000_000));
    }

    /// The Playback page shows the length watch state and plays judge
    /// the item by: the server's probe, over the catalog's.
    #[test]
    fn a_probed_length_is_shown_over_the_catalog_s() {
        let item = crate::test_support::track("32", "31", 1);
        let probed = Some(Duration::from_millis(1_309_472));
        assert_eq!(Shown::of(&item, probed).duration_ms, Some(1_309_472));
        assert_eq!(
            Shown::of(&item, None).duration_ms,
            crate::watch::length(None, &item).map(|d| d.as_millis() as u64)
        );
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
