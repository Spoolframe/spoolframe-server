//! Playback ownership independent of any UI or media engine.
//!
//! Clones share a handle. Recovery replaces it for readers, progress and controls
//! together. Call `finish`/`close` before shutting down the runtime: Drop cleanup
//! is best effort and cannot run after runtime shutdown.
//!
//! # Resuming a playback the server ended
//!
//! The server ends a playback it has not heard from for a while (a long
//! pause), when it restarts, and when it lets a device's playbacks go. The
//! next use of the old handle is [`SdkError::PlaybackExpired`]. A
//! [`ManagedPlayback`] then starts the item again with the tracks and
//! options it was opened with, at the last known position, swaps the new
//! handle in behind every clone, and tries the operation once more:
//!
//! - direct reads, HLS playlists, HLS segments read from offset 0,
//!   progress reports, seeks, subtitle changes and `finish` go on with no
//!   error. An HLS resume starts at the segment asked for: segments are
//!   numbered on the item's own time, so the playlist does not change;
//! - a progressive read ([`ManagedPlayback::read_chunk`]) and the rest of
//!   an HLS segment read part-way get [`SdkError::PlaybackResumed`]: a new
//!   encode's bytes do not continue the old one's.
//!
//! It does not resume a playback the app closed ([`SdkError::PlaybackClosed`]),
//! one whose start is refused for good (signed out, no longer served,
//! cannot play, stopped by an admin: that error, every time after), one
//! whose new plan plays differently ([`SdkError::PlaybackChanged`]), or
//! more than [`ManagedPlayback::MAX_RESUMES`] times in
//! [`ManagedPlayback::RESUME_WINDOW`] ([`SdkError::PlaybackExpired`]).
use crate::{
    Chunk, ChunkStream, Playback, PlaybackChange, PlaybackOptions, PlaybackQuality, SdkError,
    Session, Subtitle,
};
use async_trait::async_trait;
use parking_lot::Mutex;
use spoolframe_protocol::{BitmapSubtitles, ManifestKind, PlaybackHandle, PlaybackPlan, Selection};
use spoolframe_types::MediaId;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Notify, broadcast, oneshot, watch};
use tokio::time::Instant;

mod cadence;
mod choice;
mod events;
mod next;
mod player_session;
mod revival;
mod routed;
mod stall;
mod subtitle_file;
pub use cadence::PlayerState;
pub use events::PlaybackEvent;
pub use player_session::PlayerSession;
pub use revival::{PlaybackStopped, RestartCause};
pub use subtitle_file::SubtitleFile;

/// Small transport port for media adapters and deterministic client tests.
#[async_trait]
pub trait PlaybackBackend: Send + Sync + 'static {
    async fn start(
        &self,
        item: &MediaId,
        selection: Option<Selection>,
        options: PlaybackOptions,
    ) -> Result<Playback, SdkError>;
    async fn read(
        &self,
        handle: &PlaybackHandle,
        offset: u64,
        length: u64,
    ) -> Result<Vec<u8>, SdkError>;
    async fn read_chunk(
        &self,
        handle: &PlaybackHandle,
        epoch: u32,
        offset: u64,
        length: u64,
    ) -> Result<Chunk, SdkError>;
    async fn seek(&self, handle: &PlaybackHandle, position: Duration) -> Result<u32, SdkError>;
    async fn subtitle(
        &self,
        handle: &PlaybackHandle,
        selected: Option<u32>,
        bitmap: BitmapSubtitles,
        position: Duration,
    ) -> Result<PlaybackChange, SdkError>;
    async fn progress(&self, handle: &PlaybackHandle, position: Duration) -> Result<(), SdkError>;
    async fn stop(&self, handle: &PlaybackHandle) -> Result<(), SdkError>;
    /// Plan the playback again for the quality the viewer chose
    /// ([`Session::change_quality`]). A backend that cannot keeps the
    /// default, which refuses.
    async fn change_quality(
        &self,
        handle: &PlaybackHandle,
        quality: PlaybackQuality,
        bitmap: BitmapSubtitles,
        position: Duration,
    ) -> Result<PlaybackChange, SdkError> {
        let _ = (handle, quality, bitmap, position);
        Err(SdkError::InvalidInput(
            "this playback backend cannot change a playback's quality".into(),
        ))
    }
    /// Plan the playback again for a link found slow
    /// ([`Session::limit_playback`]). A backend with no link keeps the
    /// default, which refuses.
    async fn limit(
        &self,
        handle: &PlaybackHandle,
        link_bitrate: u64,
        bitmap: BitmapSubtitles,
        position: Duration,
    ) -> Result<PlaybackChange, SdkError> {
        let _ = (handle, link_bitrate, bitmap, position);
        Err(SdkError::InvalidInput(
            "this playback backend cannot plan for its link".into(),
        ))
    }
    /// Experimental: the playback's HTTP URL
    /// ([`Session::http_playback_url`]). A backend with no server keeps
    /// the default: off.
    async fn http_playback_url(
        &self,
        handle: &PlaybackHandle,
    ) -> Result<crate::HttpPlayback, SdkError> {
        let _ = handle;
        Ok(crate::HttpPlayback::Refused(
            crate::HttpPlaybackRefusal::Off,
        ))
    }
    /// An HLS playlist ([`Session::manifest`]). A backend with no HLS
    /// keeps the default, which refuses.
    async fn manifest(
        &self,
        handle: &PlaybackHandle,
        kind: ManifestKind,
        variant: u32,
    ) -> Result<String, SdkError> {
        let _ = (handle, kind, variant);
        Err(no_hls())
    }
    /// Part of HLS segment `n` of rung `variant` ([`Session::read_segment`]).
    async fn read_segment(
        &self,
        handle: &PlaybackHandle,
        epoch: u32,
        variant: u32,
        n: u32,
        offset: u64,
        length: u64,
    ) -> Result<Chunk, SdkError> {
        let _ = (handle, epoch, variant, n, offset, length);
        Err(no_hls())
    }
    /// [`Self::read_segment`], its bytes handed on as they arrive
    /// ([`Session::read_segment_stream`]). A backend that reads
    /// whole keeps the default: its chunk, as one piece.
    async fn open_segment(
        &self,
        handle: &PlaybackHandle,
        epoch: u32,
        variant: u32,
        n: u32,
        offset: u64,
        length: u64,
    ) -> Result<ChunkStream, SdkError> {
        self.read_segment(handle, epoch, variant, n, offset, length)
            .await
            .map(ChunkStream::from)
    }
    /// [`Self::read_chunk`], its bytes handed on as they arrive
    /// ([`Session::read_chunk_stream`]). A backend that reads whole
    /// keeps the default: its chunk, as one piece.
    async fn open_chunk(
        &self,
        handle: &PlaybackHandle,
        epoch: u32,
        offset: u64,
        length: u64,
    ) -> Result<ChunkStream, SdkError> {
        self.read_chunk(handle, epoch, offset, length)
            .await
            .map(ChunkStream::from)
    }
    /// How fast the link to the server carries media, for the
    /// direct reader to feed. A backend with no link keeps the default.
    fn link_meter(&self) -> Option<Arc<crate::LinkMeter>> {
        None
    }
    /// Playbacks whose share of the server's uplink changed: a
    /// managed playback in Auto plans itself again on its own. A backend
    /// with no server keeps the default.
    fn shares(&self) -> Option<broadcast::Receiver<PlaybackShare>> {
        None
    }
    /// Keep `selection` for the item's series or movie
    /// ([`Session::set_track_override`]). A backend with no server keeps
    /// the default, which refuses.
    async fn set_track_override(
        &self,
        item: &MediaId,
        selection: Selection,
    ) -> Result<(), SdkError> {
        let _ = (item, selection);
        Err(SdkError::InvalidInput(
            "this playback backend cannot keep a track choice".into(),
        ))
    }
    /// The playback's subtitle file, whole ([`Session::read_subtitle`]).
    /// A backend with no files keeps the default, which refuses.
    async fn read_subtitle(&self, handle: &PlaybackHandle) -> Result<Subtitle, SdkError> {
        let _ = handle;
        Err(SdkError::InvalidInput(
            "this playback backend has no subtitle files".into(),
        ))
    }
    /// The server's subtitle timing changes, and the server's key they
    /// name: a managed playback follows its own track's. A backend with
    /// no server keeps the default.
    fn subtitle_timings(
        &self,
    ) -> Option<(String, broadcast::Receiver<crate::SubtitleTimingEvent>)> {
        None
    }
    /// How a paired server's item plays now
    /// ([`ManagedPlayback::route`]); `None` for any other backend.
    fn route(&self) -> Option<crate::RemoteMode> {
        None
    }
    /// HLS subtitle segment `n` ([`Session::read_subtitle_segment`]).
    async fn subtitle_segment(
        &self,
        handle: &PlaybackHandle,
        n: u32,
    ) -> Result<Subtitle, SdkError> {
        let _ = (handle, n);
        Err(no_hls())
    }
}

fn no_hls() -> SdkError {
    SdkError::InvalidInput("this playback backend has no HLS".into())
}

#[async_trait]
impl PlaybackBackend for Session {
    async fn start(
        &self,
        item: &MediaId,
        selection: Option<Selection>,
        options: PlaybackOptions,
    ) -> Result<Playback, SdkError> {
        self.start_playback_with(item, selection, options).await
    }
    async fn read(&self, h: &PlaybackHandle, o: u64, n: u64) -> Result<Vec<u8>, SdkError> {
        self.read(h, o, n).await
    }
    async fn read_chunk(
        &self,
        h: &PlaybackHandle,
        e: u32,
        o: u64,
        n: u64,
    ) -> Result<Chunk, SdkError> {
        self.read_chunk(h, e, o, n).await
    }
    async fn seek(&self, h: &PlaybackHandle, p: Duration) -> Result<u32, SdkError> {
        self.seek_playback(h, p).await
    }
    async fn subtitle(
        &self,
        h: &PlaybackHandle,
        s: Option<u32>,
        b: BitmapSubtitles,
        p: Duration,
    ) -> Result<PlaybackChange, SdkError> {
        self.change_subtitle(h, s, b, p).await
    }
    async fn progress(&self, h: &PlaybackHandle, p: Duration) -> Result<(), SdkError> {
        self.update_progress(h, p).await
    }
    async fn stop(&self, h: &PlaybackHandle) -> Result<(), SdkError> {
        self.stop_playback(h).await
    }
    async fn limit(
        &self,
        h: &PlaybackHandle,
        l: u64,
        b: BitmapSubtitles,
        p: Duration,
    ) -> Result<PlaybackChange, SdkError> {
        self.limit_playback(h, l, b, p).await
    }
    async fn change_quality(
        &self,
        h: &PlaybackHandle,
        q: PlaybackQuality,
        b: BitmapSubtitles,
        p: Duration,
    ) -> Result<PlaybackChange, SdkError> {
        Session::change_quality(self, h, q, b, p).await
    }
    async fn manifest(
        &self,
        h: &PlaybackHandle,
        k: ManifestKind,
        v: u32,
    ) -> Result<String, SdkError> {
        Session::manifest(self, h, k, v).await
    }
    async fn http_playback_url(&self, h: &PlaybackHandle) -> Result<crate::HttpPlayback, SdkError> {
        Session::http_playback_url(self, h).await
    }
    async fn read_segment(
        &self,
        h: &PlaybackHandle,
        e: u32,
        v: u32,
        n: u32,
        o: u64,
        l: u64,
    ) -> Result<Chunk, SdkError> {
        Session::read_segment(self, h, e, v, n, o, l).await
    }
    async fn subtitle_segment(&self, h: &PlaybackHandle, n: u32) -> Result<Subtitle, SdkError> {
        self.read_subtitle_segment(h, n).await
    }
    async fn read_subtitle(&self, h: &PlaybackHandle) -> Result<Subtitle, SdkError> {
        Session::read_subtitle(self, h).await
    }
    async fn set_track_override(&self, item: &MediaId, s: Selection) -> Result<(), SdkError> {
        Session::set_track_override(self, item, s).await
    }
    async fn open_segment(
        &self,
        h: &PlaybackHandle,
        e: u32,
        v: u32,
        n: u32,
        o: u64,
        l: u64,
    ) -> Result<ChunkStream, SdkError> {
        Session::read_segment_stream(self, h, e, v, n, o, l).await
    }
    async fn open_chunk(
        &self,
        h: &PlaybackHandle,
        e: u32,
        o: u64,
        l: u64,
    ) -> Result<ChunkStream, SdkError> {
        Session::read_chunk_stream(self, h, e, o, l).await
    }
    fn link_meter(&self) -> Option<Arc<crate::LinkMeter>> {
        Some(self.client.link(self.server).meter())
    }
    fn shares(&self) -> Option<broadcast::Receiver<PlaybackShare>> {
        Some(self.client.inner.shares.subscribe())
    }
    fn subtitle_timings(
        &self,
    ) -> Option<(String, broadcast::Receiver<crate::SubtitleTimingEvent>)> {
        Some((
            self.server.to_string(),
            self.client.subtitle_timing_events(),
        ))
    }
}

struct State {
    playback: Playback,
    selection: Option<Selection>,
    options: PlaybackOptions,
    pending: Option<Duration>,
    /// Where the viewer last was: reported, sought to, or where a subtitle
    /// change restarted the encode. A resume starts there.
    position: Option<Duration>,
    /// Where each HLS segment starts (segment `n` at `[n - 1]`), from the
    /// media playlist last read: a segment read says where the player is.
    segments: Vec<Duration>,
    /// One more each time the encode is replaced: with the handle (a
    /// resume), or under it (a seek, a subtitle or quality change, a
    /// re-plan for the link), as the server serves segments by number
    /// from its encode now.
    generation: u64,
    /// What each HLS segment (by rung and number) was last opened from
    /// offset 0 under, for a read of its rest that names no
    /// reader ([`ManagedPlayback::read_segment`] and
    /// [`ManagedPlayback::segment_stream`] from past 0): the rest must
    /// come from the same encode. One entry per segment, so with two
    /// readers of it in different encodes it holds the last to open; a
    /// [`SegmentStream`] reads on with [`SegmentStream::reopen`], which
    /// checks its own.
    segment_reads: HashMap<(u32, u32), Opened>,
    /// Where a resume (not a seek) started the current handle: a reader of
    /// an older handle is told so.
    resumed_at: Option<Duration>,
    /// When the resumes of the last [`ManagedPlayback::RESUME_WINDOW`]
    /// were asked for, oldest first.
    resumes: VecDeque<Instant>,
    /// Why the playback is over for good: a resume refused for a reason
    /// that starting again would not change.
    ended: Option<SdkError>,
    finishing: bool,
    closed: bool,
    /// The last few whole HLS segments read: their bytes and their time,
    /// what the stream needs of the link.
    recent_segments: VecDeque<(u64, Duration)>,
    /// When a slow link last made this ask to be planned again for it.
    limit_asked: Option<std::time::Instant>,
    /// The share of the server's uplink it was last told.
    share: Option<PlaybackShare>,
    /// What the app's player last said it does ([`ManagedPlayback::set_state`]);
    /// `None` until it says.
    player: Option<PlayerState>,
    /// When the server last heard from this playback: a read or a report.
    contact: Instant,
    /// When bytes were last read of it.
    read_at: Instant,
    /// Since when it has played without a pause or a restart: the
    /// restart budget is whole again after [`ManagedPlayback::REFILL_AFTER`].
    playing_since: Option<Instant>,
    /// Whether an idle playback must be used to stay alive. A server that
    /// keeps paused playbacks needs no keep-alive.
    keepalive_needed: bool,
    /// A stall the app reported made this ask to be planned for the link.
    stall_limited: bool,
    /// Each subtitle track's timing, as the server last said.
    timings: HashMap<u32, spoolframe_types::SubtitleTiming>,
}

/// What a segment read was opened under: the handle it went to and the
/// playback's generation then, taken together before it was awaited. Its
/// bytes are that encode's, whatever replaced the handle, or the encode
/// under it, while they were on the link.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Opened {
    handle: PlaybackHandle,
    generation: u64,
}

impl Opened {
    fn now(state: &State) -> Self {
        Self {
            handle: state.playback.handle.clone(),
            generation: state.generation,
        }
    }

    /// The playback still runs the encode this was opened in.
    fn current(&self, state: &State) -> bool {
        self.generation == state.generation && self.handle == state.playback.handle
    }
}

struct Inner {
    backend: Arc<dyn PlaybackBackend>,
    item: MediaId,
    state: Mutex<State>,
    control: tokio::sync::Mutex<()>,
    changed: watch::Sender<Playback>,
    errors: broadcast::Sender<SdkError>,
    events: broadcast::Sender<PlaybackEvent>,
    notify: Arc<Notify>,
    runtime: tokio::runtime::Handle,
    /// Never sent: dropped with the playback, which ends the task that
    /// follows the server's share events (B2). That task holds only a
    /// weak reference, so nothing else would wake it.
    gone: watch::Sender<()>,
    /// What keeps a track's bytes as they are read.
    keeper: std::sync::OnceLock<Arc<crate::downloads::Keeper>>,
    /// The read ahead of a kept track's file was started.
    reading_ahead: std::sync::atomic::AtomicBool,
    /// True once its player read a byte, or it ended or was closed: what
    /// opening the next one waits for ([`ManagedPlayback::open_after`]).
    settled: watch::Sender<bool>,
}

/// A live playback with shared recovery, bounded progress reporting and cleanup.
#[derive(Clone)]
pub struct ManagedPlayback(Arc<Inner>);
impl std::fmt::Debug for ManagedPlayback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ManagedPlayback")
            .field(&self.snapshot())
            .finish()
    }
}

// Owns an admitted start until its caller installs it. A dropped start future
// cannot cancel cleanup: admission and ownership transfer run in a runtime task.
struct Admission {
    backend: Arc<dyn PlaybackBackend>,
    playback: Option<Playback>,
    runtime: tokio::runtime::Handle,
}
impl Drop for Admission {
    fn drop(&mut self) {
        if let Some(p) = self.playback.take() {
            let backend = self.backend.clone();
            self.runtime.spawn(async move {
                let _ = backend.stop(&p.handle).await;
            });
        }
    }
}
/// Start the playback on `runtime`: the caller's when it opens one, the
/// playback's own when it starts one again (B5), whatever thread asks.
async fn admitted(
    runtime: tokio::runtime::Handle,
    backend: Arc<dyn PlaybackBackend>,
    item: MediaId,
    selection: Option<Selection>,
    options: PlaybackOptions,
) -> Result<Admission, SdkError> {
    let (send, receive) = oneshot::channel();
    runtime.clone().spawn(async move {
        let answer = backend
            .start(&item, selection, options)
            .await
            .map(|p| Admission {
                backend,
                playback: Some(p),
                runtime,
            });
        let _ = send.send(answer);
    });
    receive
        .await
        .map_err(|e| SdkError::Protocol(format!("playback startup task ended: {e}")))?
}

/// What a restart is for.
#[derive(Debug, Clone, Copy)]
enum Restart {
    /// The server ended the playback: go on as it was, from `at` or the
    /// last known position. The new playback must play the same way.
    Resume { at: Option<Duration> },
    /// A seek or subtitle change at `at` met an ended playback: start it
    /// there, as asked. Its plan may differ; the app builds a new reader
    /// from the result either way.
    Control { at: Duration },
    /// The viewer picked other tracks, which the server serves only with
    /// a new playback: start one with them at `at`, and stop the old one,
    /// which is still live. A choice, not a revival: it spends no budget,
    /// and a refusal leaves the old playback playing.
    Reselect { at: Duration, selection: Selection },
}

/// Whether a restart brings back a playback that died, as against one
/// the viewer asked for.
fn revival(restart: Restart) -> bool {
    !matches!(restart, Restart::Reselect { .. })
}

/// A playback's share of its server's uplink changed (the server's
/// `PlaybackShare` event): `max_bitrate` is what it may take now,
/// bits/s, `None` for no limit. From [`crate::Client::playback_shares`],
/// and as a managed playback's own [`ManagedPlayback::share`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlaybackShare {
    pub handle: PlaybackHandle,
    pub max_bitrate: Option<u64>,
}

/// A refusal of a start that starting again cannot change: the session
/// ended, the item is no longer served to this user, it cannot play
/// here, or an admin stopped it.
fn ends_for_good(error: &SdkError) -> bool {
    matches!(
        error,
        SdkError::SignInRequired
            | SdkError::NotFound(_)
            | SdkError::CannotPlay { .. }
            | SdkError::PlaybackStopped
    )
}

/// Whether `next` goes on where `before` was, with no seam: a direct
/// file of the same size, or the same conversion. HLS segments are
/// numbered on the item's own time, whatever an encode started from
/// (`encode::hls` on the server), so the same plan has the same playlist.
fn continues(before: &Playback, next: &Playback) -> bool {
    match before.plan {
        PlaybackPlan::Direct { .. } => {
            matches!(next.plan, PlaybackPlan::Direct { .. }) && next.size == before.size
        }
        _ => next.plan == before.plan,
    }
}

/// Where each segment of an HLS media playlist starts, from its
/// `#EXTINF` durations.
/// Whether a link carrying `link` bit/s is too slow for a stream whose
/// recent segments held `segments` (bytes, time): under
/// [`spoolframe_protocol::playback::LINK_SHARE`] of it is what a plan for
/// the link takes, so it must be at least what the segments need.
pub(crate) fn too_slow(link: f64, segments: &[(u64, Duration)]) -> bool {
    let bytes: u64 = segments.iter().map(|(b, _)| b).sum();
    let secs: f64 = segments.iter().map(|(_, t)| t.as_secs_f64()).sum();
    if secs <= 0.0 {
        return false;
    }
    let needs = bytes as f64 * 8.0 / secs;
    link * spoolframe_protocol::playback::LINK_SHARE < needs
}

fn segment_starts(playlist: &str) -> Vec<Duration> {
    let mut starts = Vec::new();
    let mut at = Duration::ZERO;
    for line in playlist.lines() {
        let Some(info) = line.strip_prefix("#EXTINF:") else {
            continue;
        };
        starts.push(at);
        let secs: f64 = info
            .split(',')
            .next()
            .and_then(|s| s.trim().parse().ok())
            .filter(|s: &f64| s.is_finite() && *s >= 0.0)
            .unwrap_or(0.0);
        at += Duration::from_millis((secs * 1000.0).round() as u64);
    }
    starts
}

impl ManagedPlayback {
    /// The most restarts in [`Self::RESUME_WINDOW`]: resumes behind the
    /// app's back and [`Self::restart`]s alike. A playback the server
    /// keeps ending is not started again and again: past this the
    /// operation fails with [`SdkError::PlaybackExpired`]. The budget is
    /// whole again once it has played [`Self::REFILL_AFTER`] on end
    /// ([`Self::set_state`]). An app keeps no budget of its own.
    pub const MAX_RESUMES: usize = 3;
    pub const RESUME_WINDOW: Duration = Duration::from_secs(5 * 60);

    pub async fn open(
        backend: Arc<dyn PlaybackBackend>,
        item: MediaId,
        selection: Option<Selection>,
        options: PlaybackOptions,
    ) -> Result<Self, SdkError> {
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|e| SdkError::InvalidInput(e.to_string()))?;
        let mut admission = admitted(
            runtime,
            backend.clone(),
            item.clone(),
            selection,
            options.clone(),
        )
        .await?;
        let playback = admission.playback.take().expect("admitted playback");
        Ok(Self::adopt(backend, item, selection, options, playback))
    }

    /// Transfer an already-started playback into this owner. Do not keep a second
    /// owner that independently stops or restarts the same handle.
    ///
    /// # Panics
    /// Must be called inside an entered Tokio runtime. For normal startup use
    /// [`Self::open`], which also cleans up when its future is cancelled.
    pub fn adopt(
        backend: Arc<dyn PlaybackBackend>,
        item: MediaId,
        selection: Option<Selection>,
        options: PlaybackOptions,
        playback: Playback,
    ) -> Self {
        let selection = playback.chosen.map(|c| c.selection).or(selection);
        let (changed, _) = watch::channel(playback.clone());
        let (errors, _) = broadcast::channel(16);
        let notify = Arc::new(Notify::new());
        let inner = Arc::new(Inner {
            backend,
            item,
            state: Mutex::new(State {
                playback,
                selection,
                options,
                pending: None,
                position: None,
                segments: Vec::new(),
                generation: 0,
                segment_reads: HashMap::new(),
                resumed_at: None,
                resumes: VecDeque::new(),
                ended: None,
                finishing: false,
                closed: false,
                recent_segments: VecDeque::new(),
                limit_asked: None,
                share: None,
                player: None,
                contact: Instant::now(),
                read_at: Instant::now(),
                playing_since: None,
                keepalive_needed: true,
                stall_limited: false,
                timings: HashMap::new(),
            }),
            control: tokio::sync::Mutex::new(()),
            changed,
            errors,
            events: broadcast::Sender::new(16),
            notify: notify.clone(),
            runtime: tokio::runtime::Handle::current(),
            gone: watch::Sender::new(()),
            keeper: std::sync::OnceLock::new(),
            reading_ahead: std::sync::atomic::AtomicBool::new(false),
            settled: watch::Sender::new(false),
        });
        // A share of the server's uplink that moved: a playback in
        // Auto plans itself again where the viewer is, as the viewer's own
        // quality change would.
        if let Some(mut shares) = inner.backend.shares() {
            let weak = Arc::downgrade(&inner);
            let mut gone = inner.gone.subscribe();
            inner.runtime.spawn(async move {
                loop {
                    let received = tokio::select! {
                        received = shares.recv() => received,
                        // Only ever an error: the playback was dropped.
                        _ = gone.changed() => return,
                    };
                    let share = match received {
                        Ok(share) => share,
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => return,
                    };
                    let Some(inner) = weak.upgrade() else { return };
                    let this = ManagedPlayback(inner);
                    let mine = {
                        let mut state = this.0.state.lock();
                        let ours = state.playback.handle == share.handle && !state.closed;
                        if ours {
                            state.share = Some(share.clone());
                        }
                        ours && state.options.quality == PlaybackQuality::Auto
                    };
                    // Checked again under the control lock: a pick of the
                    // viewer's on the server now must not be undone.
                    if mine && let Err(error) = this.follow_share(&share.handle).await {
                        let _ = this.0.errors.send(error);
                    }
                }
            });
        }
        let weak = Arc::downgrade(&inner);
        inner.runtime.spawn(async move {
            loop {
                notify.notified().await;
                tokio::time::sleep(Duration::from_millis(250)).await;
                let Some(inner) = weak.upgrade() else { break };
                if let Err(error) = ManagedPlayback(inner.clone()).flush().await {
                    let _ = inner.errors.send(error);
                }
            }
        });
        cadence::follow(&inner);
        events::follow(&inner);
        Self(inner)
    }
    pub fn snapshot(&self) -> Playback {
        self.0.state.lock().playback.clone()
    }
    /// Whether it is planned as `plan` now: a replan's test, with no
    /// copy of the playback.
    pub fn is_planned(&self, plan: &PlaybackPlan) -> bool {
        self.0.state.lock().playback.plan == *plan
    }
    /// Whether the server converts it now.
    pub fn is_converted(&self) -> bool {
        matches!(
            self.0.state.lock().playback.plan,
            PlaybackPlan::Converted { .. }
        )
    }
    /// Whether `handle` is the one it reads now: a share or event for an
    /// earlier start of it is not its own.
    pub fn is_handle(&self, handle: &PlaybackHandle) -> bool {
        self.0.state.lock().playback.handle == *handle
    }
    /// The link meter of the server this plays from, if it has one.
    #[cfg(feature = "media-readers")]
    pub(crate) fn link_meter(&self) -> Option<Arc<crate::LinkMeter>> {
        self.0.backend.link_meter()
    }
    /// The playback after each change: a seek, a subtitle change, or a
    /// resume behind the app's back.
    pub fn changes(&self) -> watch::Receiver<Playback> {
        self.0.changed.subscribe()
    }
    /// The share of the server's uplink this playback was last told:
    /// `max_bitrate` is what it may take, bits/s, `None` for no
    /// limit. `None` before the server said, and after the playback
    /// started again under another handle, until it says again.
    pub fn share(&self) -> Option<PlaybackShare> {
        let state = self.0.state.lock();
        state
            .share
            .clone()
            .filter(|share| share.handle == state.playback.handle)
    }
    /// Background reporting failures; explicit `flush` and `finish` return errors.
    pub fn errors(&self) -> broadcast::Receiver<SdkError> {
        self.0.errors.subscribe()
    }
    /// Where the player is. Reports are coalesced and sent in the
    /// background; a resume starts the new playback here.
    pub fn report_position(&self, position: Duration) -> Result<(), SdkError> {
        let mut state = self.0.state.lock();
        if state.finishing || state.closed {
            return Err(SdkError::PlaybackClosed);
        }
        if let Some(ended) = &state.ended {
            return Err(ended.clone());
        }
        state.pending = Some(position);
        state.position = Some(position);
        self.0.notify.notify_one();
        Ok(())
    }
    pub async fn flush(&self) -> Result<(), SdkError> {
        let this = self.clone();
        self.0
            .runtime
            .spawn(async move { this.flush_inner().await })
            .await
            .map_err(|e| SdkError::Protocol(format!("playback control task ended: {e}")))?
    }
    async fn flush_inner(&self) -> Result<(), SdkError> {
        let _control = self.0.control.lock().await;
        self.flush_resuming_locked().await
    }
    /// Sends the pending position. A playback the server ended is resumed
    /// there, and the position sent to the new one.
    async fn flush_resuming_locked(&self) -> Result<(), SdkError> {
        match self.flush_locked().await {
            Err(SdkError::PlaybackExpired) => {
                let old = self.snapshot().handle;
                self.restart_locked(&old, Restart::Resume { at: None })
                    .await?;
                self.flush_locked().await
            }
            answer => answer,
        }
    }
    async fn flush_locked(&self) -> Result<(), SdkError> {
        let report = {
            let mut state = self.0.state.lock();
            state
                .pending
                .take()
                .map(|p| (state.playback.handle.clone(), p))
        };
        let Some((handle, position)) = report else {
            return Ok(());
        };
        if let Err(error) = self.0.backend.progress(&handle, position).await {
            let mut state = self.0.state.lock();
            if !state.closed && state.pending.is_none() {
                state.pending = Some(position);
            }
            return Err(error);
        }
        let mut state = self.0.state.lock();
        state.contact = Instant::now();
        Ok(())
    }
    fn live(&self) -> Result<Playback, SdkError> {
        self.live_opened().map(|(playback, _)| playback)
    }
    /// The live playback, and what a read of it now is opened under, taken
    /// together.
    fn live_opened(&self) -> Result<(Playback, Opened), SdkError> {
        let state = self.0.state.lock();
        if state.finishing || state.closed {
            Err(SdkError::PlaybackClosed)
        } else if let Some(ended) = &state.ended {
            Err(ended.clone())
        } else {
            Ok((state.playback.clone(), Opened::now(&state)))
        }
    }
    /// `op` on the live handle; if the server had ended the playback, it
    /// is resumed (from `at`, or the last known position) and `op` tried
    /// once more on the new handle.
    async fn resuming<T, F, Fut>(&self, at: Option<Duration>, op: F) -> Result<T, SdkError>
    where
        F: Fn(Playback) -> Fut,
        Fut: Future<Output = Result<T, SdkError>>,
    {
        self.resuming_opened(at, op).await.map(|(answer, _)| answer)
    }
    /// [`Self::resuming`], with what the answer was opened under: the
    /// handle `op` went to and the generation then, taken before it was
    /// awaited. A resume while it was on the link does not relabel it.
    async fn resuming_opened<T, F, Fut>(
        &self,
        at: Option<Duration>,
        op: F,
    ) -> Result<(T, Opened), SdkError>
    where
        F: Fn(Playback) -> Fut,
        Fut: Future<Output = Result<T, SdkError>>,
    {
        let (playback, opened) = self.live_opened()?;
        match op(playback.clone()).await {
            Err(SdkError::PlaybackExpired) => {
                self.replace(&playback.handle, Restart::Resume { at })
                    .await?;
                let (playback, opened) = self.live_opened()?;
                op(playback).await.map(|answer| (answer, opened))
            }
            answer => answer.map(|answer| (answer, opened)),
        }
    }
    /// Up to `length` bytes of a direct playback's file from `offset`. A
    /// playback the server ended is resumed and the read goes on.
    ///
    /// A track kept as it plays is read ahead, whole, from its
    /// first read: the reads come from what is here once it is, and the
    /// track is in the cache before it has played to its end.
    pub async fn read(&self, offset: u64, length: u64) -> Result<Vec<u8>, SdkError> {
        let Some(keeper) = self.0.keeper.get() else {
            return self.read_remote(offset, length).await;
        };
        let size = self.snapshot().size;
        if let Some(bytes) = keeper.local(size, offset, length).await {
            return Ok(bytes);
        }
        let bytes = self.read_remote(offset, length).await?;
        let p = self.snapshot();
        keeper.direct(&p.plan, p.size, offset, &bytes).await;
        self.read_ahead();
        Ok(bytes)
    }

    async fn read_remote(&self, offset: u64, length: u64) -> Result<Vec<u8>, SdkError> {
        let backend = &self.0.backend;
        let bytes = self
            .resuming(None, move |p| async move {
                backend.read(&p.handle, offset, length).await
            })
            .await?;
        self.read_now();
        Ok(bytes)
    }

    /// Bytes were read: the server heard from the playback.
    fn read_now(&self) {
        let mut state = self.0.state.lock();
        let now = Instant::now();
        state.read_at = now;
        state.contact = now;
        drop(state);
        self.0.settled.send_replace(true);
    }

    /// Where the viewer last was: reported, sought to, or set.
    pub(crate) fn last_position(&self) -> Option<Duration> {
        self.0.state.lock().position
    }

    /// Wait `time` on the playback's runtime, from any thread or
    /// executor: the timer is the runtime's.
    pub(crate) async fn delay(&self, time: Duration) {
        let _ = self.0.runtime.spawn(tokio::time::sleep(time)).await;
    }

    /// Fetch what the keeper is missing of the file, a piece at a time,
    /// once: until it is all here, the keeper gives up, a read fails, or
    /// the playback goes. The task holds the playback only for a read.
    fn read_ahead(&self) {
        if self
            .0
            .reading_ahead
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        let weak = Arc::downgrade(&self.0);
        self.0.runtime.spawn(async move {
            loop {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                let playback = ManagedPlayback(inner);
                let Some(keeper) = playback.0.keeper.get().cloned() else {
                    return;
                };
                let Some((offset, length)) = keeper.gap().await else {
                    return;
                };
                match playback.read_remote(offset, length).await {
                    Ok(bytes) if !bytes.is_empty() => {
                        let p = playback.snapshot();
                        keeper.direct(&p.plan, p.size, offset, &bytes).await;
                    }
                    Ok(_) => return,
                    Err(e) => {
                        tracing::debug!(error = %e, "a track's read ahead stopped");
                        return;
                    }
                }
            }
        });
    }
    /// A reader remains pinned to its handle and epoch snapshot. On a seek
    /// ([`SdkError::PlaybackRestarted`]) or a resume
    /// ([`SdkError::PlaybackResumed`]) the media engine must retire that
    /// reader and create a replacement from byte zero of [`Self::snapshot`].
    pub async fn read_chunk(
        &self,
        source: &Playback,
        offset: u64,
        length: u64,
    ) -> Result<Chunk, SdkError> {
        let p = self.live()?;
        if p.handle != source.handle || p.epoch != source.epoch {
            return Err(self.moved_on(source));
        }
        match self
            .0
            .backend
            .read_chunk(&p.handle, source.epoch, offset, length)
            .await
        {
            Err(SdkError::PlaybackExpired) => {
                self.replace(&p.handle, Restart::Resume { at: None })
                    .await?;
                Err(self.moved_on(source))
            }
            Ok(chunk) => {
                self.read_now();
                if let Some(keeper) = self.0.keeper.get() {
                    keeper.chunk(source, offset, &chunk).await;
                }
                Ok(chunk)
            }
            answer => answer,
        }
    }
    /// What a reader of `source` is told now that the playback is not it.
    fn moved_on(&self, source: &Playback) -> SdkError {
        let state = self.0.state.lock();
        match state.resumed_at {
            Some(at) if state.playback.handle != source.handle => SdkError::PlaybackResumed { at },
            _ => SdkError::PlaybackRestarted {
                epoch: state.playback.epoch,
            },
        }
    }
    /// Experimental (`http-playback`): where a player with no Iroh
    /// (an AirPlay receiver) fetches this playback over plain HTTP, or why
    /// not ([`Session::http_playback_url`]). The URL is bound to the
    /// playback as it is now: one the SDK starts again behind the app (a
    /// resume) has a new handle, and the old URL stops working; ask again.
    pub async fn http_playback_url(&self) -> Result<crate::HttpPlayback, SdkError> {
        let backend = &self.0.backend;
        self.resuming(None, move |p| async move {
            backend.http_playback_url(&p.handle).await
        })
        .await
    }
    /// An HLS playlist; a media playlist of rung `variant` (0 for
    /// one encode, and for the other kinds). One the server ended is
    /// resumed: the playlist of the same plan is the same, so the
    /// player's copy stays good.
    pub async fn manifest(&self, kind: ManifestKind, variant: u32) -> Result<String, SdkError> {
        let backend = &self.0.backend;
        let text = self
            .resuming(None, move |p| async move {
                backend.manifest(&p.handle, kind, variant).await
            })
            .await?;
        if matches!(kind, ManifestKind::Media) {
            self.0.state.lock().segments = segment_starts(&text);
        }
        Ok(text)
    }
    /// Up to `length` bytes of HLS segment `n` (0 is the init segment) from
    /// `offset`. Segments are numbered on the item's own time, so one read
    /// from offset 0 goes on through a resume with no error: the new
    /// playback starts at segment `n`. The rest of a segment read part-way
    /// cannot come from another encode: that read is
    /// [`SdkError::PlaybackResumed`] (or [`SdkError::PlaybackRestarted`],
    /// for an encode replaced under the same handle), and the segment is
    /// read again from 0.
    /// `variant` is a ladder's rung, as `v{n}/` in its playlist; 0
    /// for one encode. The rungs share the grid and the segment numbers.
    pub async fn read_segment(
        &self,
        variant: u32,
        n: u32,
        offset: u64,
        length: u64,
    ) -> Result<Chunk, SdkError> {
        let at = self.segment_start(n);
        let backend = &self.0.backend;
        if offset == 0 {
            let (chunk, opened) = self
                .resuming_opened(at, move |p| async move {
                    backend
                        .read_segment(&p.handle, p.epoch, variant, n, 0, length)
                        .await
                })
                .await?;
            self.opened_segment(variant, n, &opened);
            self.read_now();
            self.segment_read(n, 0, chunk.bytes.len() as u64, chunk.size, chunk.complete);
            return Ok(chunk);
        }
        let (p, _) = self.rest_of(variant, n, at)?;
        match backend
            .read_segment(&p.handle, p.epoch, variant, n, offset, length)
            .await
        {
            Err(SdkError::PlaybackExpired) => {
                self.replace(&p.handle, Restart::Resume { at }).await?;
                Err(self.resumed_error(at))
            }
            Ok(chunk) => {
                self.read_now();
                let read = chunk.bytes.len() as u64;
                self.segment_read(n, offset, read, chunk.size, chunk.complete);
                Ok(chunk)
            }
            answer => answer,
        }
    }

    /// [`Self::read_segment`], its bytes handed on as they arrive:
    /// the answer first (its length, the segment's size and whether it is
    /// whole), so a bridge can promise a player the length and start the
    /// body at once, then the body piece by piece. The same rules as a
    /// read: from offset 0 it goes on through a resume; the rest of a
    /// segment begun in an encode since replaced is
    /// [`SdkError::PlaybackResumed`]. A resume while it is arriving ends it
    /// with that error too: read the segment again from 0. The link meter
    /// and the step-down count it as they count a read.
    ///
    /// The stream knows the encode it was opened in: to read on after an
    /// error part-way, [`SegmentStream::reopen`] checks that encode, where
    /// a call here from past 0 can only check the last reader of the
    /// segment to open it.
    pub async fn segment_stream(
        &self,
        variant: u32,
        n: u32,
        offset: u64,
        length: u64,
    ) -> Result<SegmentStream, SdkError> {
        let at = self.segment_start(n);
        let backend = &self.0.backend;
        let (body, opened) = if offset == 0 {
            let (body, opened) = self
                .resuming_opened(at, move |p| async move {
                    backend
                        .open_segment(&p.handle, p.epoch, variant, n, 0, length)
                        .await
                })
                .await?;
            self.opened_segment(variant, n, &opened);
            (body, opened)
        } else {
            let (p, opened) = self.rest_of(variant, n, at)?;
            let body = self.open_rest(&p, variant, n, offset, length, at).await?;
            (body, opened)
        };
        self.read_now();
        Ok(SegmentStream {
            playback: self.clone(),
            body,
            variant,
            n,
            offset,
            at,
            opened,
            counted: false,
        })
    }

    /// Segment `n` of rung `variant` was opened from 0 under `opened`.
    /// Opens can finish out of order: one made before a seek or a resume
    /// may answer after one made since. The newest encode is kept (B3).
    /// The generation goes up with every new encode, under the handle or
    /// with a new one, so it alone orders them.
    fn opened_segment(&self, variant: u32, n: u32, opened: &Opened) {
        let mut state = self.0.state.lock();
        let entry = state
            .segment_reads
            .entry((variant, n))
            .or_insert_with(|| opened.clone());
        if opened.generation > entry.generation {
            *entry = opened.clone();
        }
    }

    /// The live playback for a read of the rest of segment `n` that names
    /// no reader: refused ([`Self::segment_moved_on`]) if the segment was
    /// last opened from 0 in an encode since replaced.
    fn rest_of(
        &self,
        variant: u32,
        n: u32,
        at: Option<Duration>,
    ) -> Result<(Playback, Opened), SdkError> {
        let (p, opened) = self.live_opened()?;
        let begun = self
            .0
            .state
            .lock()
            .segment_reads
            .get(&(variant, n))
            .filter(|begun| **begun != opened)
            .cloned();
        if let Some(begun) = begun {
            return Err(self.segment_moved_on(&begun, at));
        }
        Ok((p, opened))
    }

    /// What a reader of a segment opened under `opened` is told now that
    /// the encode is another: [`SdkError::PlaybackRestarted`] with the
    /// epoch now if it was replaced under the same handle (a seek, a
    /// change, a re-plan), as a progressive reader is told; else
    /// [`SdkError::PlaybackResumed`]. Either way, read the segment again
    /// from 0.
    fn segment_moved_on(&self, opened: &Opened, at: Option<Duration>) -> SdkError {
        {
            let state = self.0.state.lock();
            if state.playback.handle == opened.handle {
                return SdkError::PlaybackRestarted {
                    epoch: state.playback.epoch,
                };
            }
        }
        self.resumed_error(at)
    }

    /// The rest of segment `n` from `offset`, from `p`. A playback the
    /// server ended is resumed, and the read is
    /// [`SdkError::PlaybackResumed`]: the rest cannot come from another
    /// encode.
    async fn open_rest(
        &self,
        p: &Playback,
        variant: u32,
        n: u32,
        offset: u64,
        length: u64,
        at: Option<Duration>,
    ) -> Result<ChunkStream, SdkError> {
        match self
            .0
            .backend
            .open_segment(&p.handle, p.epoch, variant, n, offset, length)
            .await
        {
            Err(SdkError::PlaybackExpired) => {
                self.replace(&p.handle, Restart::Resume { at }).await?;
                Err(self.resumed_error(at))
            }
            answer => answer,
        }
    }

    /// [`Self::read_chunk`], its bytes handed on as they arrive.
    pub async fn chunk_stream(
        &self,
        source: &Playback,
        offset: u64,
        length: u64,
    ) -> Result<ChunkStream, SdkError> {
        let p = self.live()?;
        if p.handle != source.handle || p.epoch != source.epoch {
            return Err(self.moved_on(source));
        }
        match self
            .0
            .backend
            .open_chunk(&p.handle, source.epoch, offset, length)
            .await
        {
            Err(SdkError::PlaybackExpired) => {
                self.replace(&p.handle, Restart::Resume { at: None })
                    .await?;
                Err(self.moved_on(source))
            }
            answer => {
                if answer.is_ok() {
                    self.read_now();
                }
                answer
            }
        }
    }

    /// The most whole segments a stream's need is worked out over.
    const RECENT_SEGMENTS: usize = 4;
    /// The least time between two asks to be planned for the link.
    const LIMIT_EVERY: Duration = Duration::from_secs(30);

    /// HLS segment `n` read from `offset`: once it is whole, what
    /// it held is noted against its time, and an Auto playback whose
    /// link carries less than its segments need asks to be planned again
    /// for the link, as the direct reader does when it stalls. A
    /// ladder is left to the player, which moves between its rungs.
    fn segment_read(&self, n: u32, offset: u64, read: u64, size: u64, complete: bool) {
        if n == 0 || !complete || offset + read != size {
            return;
        }
        let link = self
            .0
            .backend
            .link_meter()
            .and_then(|meter| meter.rate(std::time::Instant::now()));
        let mut state = self.0.state.lock();
        let index = n as usize - 1;
        let (Some(start), Some(end)) = (state.segments.get(index), state.segments.get(index + 1))
        else {
            return;
        };
        let time = end.saturating_sub(*start);
        state.recent_segments.push_back((size, time));
        while state.recent_segments.len() > Self::RECENT_SEGMENTS {
            state.recent_segments.pop_front();
        }
        let ladder = matches!(
            &state.playback.plan,
            PlaybackPlan::Converted { ladder, .. } if !ladder.is_empty()
        );
        let Some(link) = link.map(|bytes| bytes * 8.0) else {
            return;
        };
        if ladder
            || state.options.quality != PlaybackQuality::Auto
            || state.recent_segments.len() < 2
            || state
                .limit_asked
                .is_some_and(|at| at.elapsed() < Self::LIMIT_EVERY)
            || !too_slow(link, state.recent_segments.make_contiguous())
        {
            return;
        }
        state.limit_asked = Some(std::time::Instant::now());
        drop(state);
        let this = self.clone();
        self.0.runtime.spawn(async move {
            if let Err(error) = this.limit(link as u64).await {
                let _ = this.0.errors.send(error);
            }
        });
    }
    /// HLS subtitle segment `n` (from 1), resumed through as
    /// [`Self::read_segment`] from offset 0.
    pub async fn read_subtitle_segment(&self, n: u32) -> Result<Subtitle, SdkError> {
        let backend = &self.0.backend;
        self.resuming(self.segment_start(n), move |p| async move {
            backend.subtitle_segment(&p.handle, n).await
        })
        .await
    }
    /// Where media segment `n` starts, once the media playlist was read.
    fn segment_start(&self, n: u32) -> Option<Duration> {
        let index = n.checked_sub(1)? as usize;
        self.0.state.lock().segments.get(index).copied()
    }
    fn resumed_error(&self, at: Option<Duration>) -> SdkError {
        let at = at.or(self.0.state.lock().resumed_at).unwrap_or_default();
        SdkError::PlaybackResumed { at }
    }
    async fn replace(&self, old: &PlaybackHandle, restart: Restart) -> Result<(), SdkError> {
        let _control = self.0.control.lock().await;
        self.restart_locked(old, restart).await
    }
    /// Start the playback again in place of `old`, with the tracks and
    /// options it was started with, and swap the new handle in for every
    /// clone. Concurrent callers share one replacement: one that finds
    /// `old` already replaced does nothing.
    async fn restart_locked(&self, old: &PlaybackHandle, restart: Restart) -> Result<(), SdkError> {
        let (before, selection, options) = {
            let mut state = self.0.state.lock();
            if state.closed {
                return Err(SdkError::PlaybackClosed);
            }
            if let Some(ended) = &state.ended {
                return Err(ended.clone());
            }
            if &state.playback.handle != old {
                return Ok(());
            }
            let now = Instant::now();
            let counted = revival(restart);
            while counted
                && state
                    .resumes
                    .front()
                    .is_some_and(|t| now.duration_since(*t) >= Self::RESUME_WINDOW)
            {
                state.resumes.pop_front();
            }
            if counted && state.resumes.len() >= Self::MAX_RESUMES {
                tracing::warn!(
                    handle = %old.0,
                    "the playback expired again; not resuming it more than {} times in {} s",
                    Self::MAX_RESUMES,
                    Self::RESUME_WINDOW.as_secs()
                );
                return Err(SdkError::PlaybackExpired);
            }
            if counted {
                state.resumes.push_back(now);
            }
            let start = match restart {
                Restart::Resume { at } => at.or(state.position),
                Restart::Control { at } | Restart::Reselect { at, .. } => Some(at),
            }
            .or(state.options.start);
            let options = PlaybackOptions {
                start,
                ..state.options.clone()
            };
            let selection = match restart {
                Restart::Reselect { selection, .. } => Some(selection),
                _ => state.selection,
            };
            (state.playback.clone(), selection, options)
        };
        let admission = admitted(
            self.0.runtime.clone(),
            self.0.backend.clone(),
            self.0.item.clone(),
            selection,
            options.clone(),
        )
        .await;
        let mut admission = match admission {
            Ok(admission) => admission,
            Err(error) => {
                if revival(restart) && ends_for_good(&error) {
                    tracing::info!(handle = %old.0, %error, "the playback cannot be resumed");
                    self.0.state.lock().ended = Some(error.clone());
                    self.0.settled.send_replace(true);
                }
                return Err(error);
            }
        };
        let next = admission.playback.as_ref().expect("admitted playback");
        if matches!(restart, Restart::Resume { .. }) && !continues(&before, next) {
            // Dropping the admission stops the new playback.
            tracing::info!(handle = %old.0, "the resumed playback plays differently; not swapped in");
            self.0.state.lock().ended = Some(SdkError::PlaybackChanged);
            self.0.settled.send_replace(true);
            return Err(SdkError::PlaybackChanged);
        }
        let next = admission.playback.take().expect("admitted playback");
        {
            let mut state = self.0.state.lock();
            state.playback = next.clone();
            state.generation += 1;
            if let Restart::Reselect { selection, .. } = restart {
                state.selection = Some(selection);
            }
            // The budget refills after a stretch of play since this.
            if state.playing_since.is_some() {
                state.playing_since = Some(Instant::now());
            }
            state.resumed_at = match restart {
                Restart::Resume { .. } => Some(options.start.unwrap_or_default()),
                Restart::Control { .. } | Restart::Reselect { .. } => None,
            };
            // The new playback learns where the viewer is (Continue
            // watching) before the player reports again.
            if state.pending.is_none() && state.position.is_some() {
                state.pending = state.position;
                self.0.notify.notify_one();
            }
        }
        tracing::info!(
            old = %old.0,
            new = %next.handle.0,
            start_ms = options.start.map(|s| s.as_millis() as u64),
            "the server had ended the playback; started it again"
        );
        if !revival(restart) {
            let (backend, old) = (self.0.backend.clone(), old.clone());
            self.0.runtime.spawn(async move {
                let _ = backend.stop(&old).await;
            });
        }
        let _ = self.0.events.send(PlaybackEvent::HandleChanged {
            handle: next.handle.clone(),
        });
        self.0.changed.send_replace(next);
        Ok(())
    }
    pub async fn seek(&self, position: Duration) -> Result<Playback, SdkError> {
        let this = self.clone();
        self.0
            .runtime
            .spawn(async move { this.seek_inner(position).await })
            .await
            .map_err(|e| SdkError::Protocol(format!("playback control task ended: {e}")))?
    }
    async fn seek_inner(&self, position: Duration) -> Result<Playback, SdkError> {
        let _control = self.0.control.lock().await;
        let before = self.live()?;
        let at = Restart::Control { at: position };
        match self.flush_locked().await {
            Ok(()) => {}
            Err(SdkError::PlaybackExpired) => {
                // Pending progress must not prevent recovery of an idle encode.
                self.restart_locked(&before.handle, at).await?;
                self.0.state.lock().position = Some(position);
                self.flush_locked().await?;
                return Ok(self.snapshot());
            }
            Err(error) => return Err(error),
        }
        match self.0.backend.seek(&before.handle, position).await {
            Ok(epoch) => {
                let mut state = self.0.state.lock();
                state.playback.epoch = epoch;
                // A new encode under the same handle.
                state.generation += 1;
                let next = state.playback.clone();
                drop(state);
                self.0.changed.send_replace(next);
            }
            Err(SdkError::PlaybackExpired) => self.restart_locked(&before.handle, at).await?,
            Err(error) => return Err(error),
        }
        self.0.state.lock().position = Some(position);
        Ok(self.snapshot())
    }
    pub async fn change_subtitle(
        &self,
        selected: Option<u32>,
        bitmap: BitmapSubtitles,
        position: Duration,
    ) -> Result<Playback, SdkError> {
        let this = self.clone();
        self.0
            .runtime
            .spawn(async move { this.change_subtitle_inner(selected, bitmap, position).await })
            .await
            .map_err(|e| SdkError::Protocol(format!("playback control task ended: {e}")))?
    }
    async fn change_subtitle_inner(
        &self,
        selected: Option<u32>,
        bitmap: BitmapSubtitles,
        position: Duration,
    ) -> Result<Playback, SdkError> {
        let _control = self.0.control.lock().await;
        let flushed = self.flush_locked().await;
        let p = self.live()?;
        let answer = match flushed {
            Ok(()) => {
                self.0
                    .backend
                    .subtitle(&p.handle, selected, bitmap, position)
                    .await
            }
            Err(error) => Err(error),
        };
        let changed = match answer {
            Ok(changed) => changed,
            Err(SdkError::PlaybackExpired) => {
                // Started again with the subtitle asked for, where asked.
                {
                    let mut state = self.0.state.lock();
                    state.selection.get_or_insert(Selection::default()).subtitle = selected;
                    state.options.bitmap = bitmap;
                }
                self.restart_locked(&p.handle, Restart::Control { at: position })
                    .await?;
                self.0.state.lock().position = Some(position);
                self.flush_locked().await?;
                return Ok(self.snapshot());
            }
            Err(error) => return Err(error),
        };
        let mut state = self.0.state.lock();
        state.playback.plan = changed.plan;
        state.playback.size = changed.size;
        state.playback.epoch = changed.epoch;
        // A new encode under the same handle.
        state.generation += 1;
        state.selection.get_or_insert(Selection::default()).subtitle = selected;
        state.options.bitmap = bitmap;
        state.position = Some(position);
        if let Some(chosen) = state.playback.chosen.as_mut() {
            chosen.selection.subtitle = selected;
        }
        let next = state.playback.clone();
        drop(state);
        self.0.changed.send_replace(next.clone());
        Ok(next)
    }
    /// The link carries only `link_bitrate` bits per second: the
    /// playback is planned again for it where the viewer last was. A plan
    /// that changes is swapped in for every clone, as a subtitle change's
    /// is, and readers of the old one are told to make new ones
    /// ([`SdkError::PlaybackRestarted`]); one that already fits changes
    /// nothing. The direct reader asks for it when it stalls on a link too
    /// slow for the file; an app need not.
    pub async fn limit(&self, link_bitrate: u64) -> Result<Playback, SdkError> {
        let this = self.clone();
        self.0
            .runtime
            .spawn(async move { this.limit_inner(link_bitrate).await })
            .await
            .map_err(|e| SdkError::Protocol(format!("playback control task ended: {e}")))?
    }
    async fn limit_inner(&self, link_bitrate: u64) -> Result<Playback, SdkError> {
        let _control = self.0.control.lock().await;
        // A bitrate or the original was the viewer's choice: the link
        // does not overrule it.
        if self.quality() != PlaybackQuality::Auto {
            return self.live();
        }
        let flushed = self.flush_locked().await;
        let p = self.live()?;
        let (position, bitmap) = {
            let state = self.0.state.lock();
            (state.position.unwrap_or_default(), state.options.bitmap)
        };
        // A flush that finds the playback ended goes the way the server's
        // own PlaybackExpired does (B4), as for every other control.
        let answer = match flushed {
            Ok(()) => {
                self.0
                    .backend
                    .limit(&p.handle, link_bitrate, bitmap, position)
                    .await
            }
            Err(error) => Err(error),
        };
        let changed = match answer {
            Ok(changed) => changed,
            // Started again where the viewer was: the start carries the
            // link's rate, so it is planned for it as well.
            Err(SdkError::PlaybackExpired) => {
                self.restart_locked(&p.handle, Restart::Control { at: position })
                    .await?;
                self.0.state.lock().position = Some(position);
                self.flush_locked().await?;
                return Ok(self.snapshot());
            }
            Err(error) => return Err(error),
        };
        if changed.plan == p.plan && changed.epoch == p.epoch {
            return Ok(p);
        }
        let mut state = self.0.state.lock();
        state.playback.plan = changed.plan;
        state.playback.size = changed.size;
        state.playback.epoch = changed.epoch;
        // A new encode under the same handle.
        state.generation += 1;
        state.position = Some(position);
        let next = state.playback.clone();
        drop(state);
        tracing::info!(
            handle = %p.handle.0,
            link_bitrate,
            epoch = next.epoch,
            "the playback was planned again for its link"
        );
        self.0.changed.send_replace(next.clone());
        Ok(next)
    }
    /// The quality the viewer chose for this playback.
    pub fn quality(&self) -> PlaybackQuality {
        self.0.state.lock().options.quality
    }
    /// Plan the playback again for `quality`, the viewer's choice now, at
    /// `position`. As [`Self::change_subtitle`]: the app makes a
    /// new reader from the result. A choice that gives the same plan
    /// changes nothing but what the next re-plan is for.
    pub async fn change_quality(
        &self,
        quality: PlaybackQuality,
        position: Duration,
    ) -> Result<Playback, SdkError> {
        let this = self.clone();
        self.0
            .runtime
            .spawn(async move { this.change_quality_inner(quality, position).await })
            .await
            .map_err(|e| SdkError::Protocol(format!("playback control task ended: {e}")))?
    }
    async fn change_quality_inner(
        &self,
        quality: PlaybackQuality,
        position: Duration,
    ) -> Result<Playback, SdkError> {
        let _control = self.0.control.lock().await;
        self.change_quality_locked(quality, position).await
    }
    /// A moved share of the uplink for `handle`: plan again in
    /// Auto where the viewer is. Whether to was seen before the control
    /// lock; a change that held it since (the viewer's own pick, or a
    /// resume under another handle) wins, and this one is dropped.
    async fn follow_share(&self, handle: &PlaybackHandle) -> Result<(), SdkError> {
        let _control = self.0.control.lock().await;
        let position = {
            let state = self.0.state.lock();
            if state.options.quality != PlaybackQuality::Auto
                || &state.playback.handle != handle
                || state.closed
            {
                return Ok(());
            }
            state.position.unwrap_or_default()
        };
        self.change_quality_locked(PlaybackQuality::Auto, position)
            .await
            .map(drop)
    }
    async fn change_quality_locked(
        &self,
        quality: PlaybackQuality,
        position: Duration,
    ) -> Result<Playback, SdkError> {
        let flushed = self.flush_locked().await;
        let p = self.live()?;
        let bitmap = self.0.state.lock().options.bitmap;
        let answer = match flushed {
            Ok(()) => {
                self.0
                    .backend
                    .change_quality(&p.handle, quality, bitmap, position)
                    .await
            }
            Err(error) => Err(error),
        };
        let changed = match answer {
            Ok(changed) => changed,
            Err(SdkError::PlaybackExpired) => {
                // Started again where asked, for the quality asked.
                self.0.state.lock().options.quality = quality;
                self.restart_locked(&p.handle, Restart::Control { at: position })
                    .await?;
                self.0.state.lock().position = Some(position);
                self.flush_locked().await?;
                return Ok(self.snapshot());
            }
            Err(error) => return Err(error),
        };
        let mut state = self.0.state.lock();
        state.options.quality = quality;
        state.position = Some(position);
        if changed.plan == p.plan && changed.epoch == p.epoch {
            return Ok(state.playback.clone());
        }
        state.playback.plan = changed.plan;
        state.playback.size = changed.size;
        state.playback.epoch = changed.epoch;
        // A new encode under the same handle.
        state.generation += 1;
        let next = state.playback.clone();
        drop(state);
        self.0.changed.send_replace(next.clone());
        Ok(next)
    }
    /// Report `final_position` and stop. A playback the server had ended
    /// is resumed to take the position (Continue watching), then stopped.
    pub async fn finish(&self, final_position: Duration) -> Result<(), SdkError> {
        self.close_at(Some(final_position)).await
    }
    pub async fn close(&self) -> Result<(), SdkError> {
        self.close_at(None).await
    }
    async fn close_at(&self, position: Option<Duration>) -> Result<(), SdkError> {
        let this = self.clone();
        self.0
            .runtime
            .spawn(async move { this.close_at_inner(position).await })
            .await
            .map_err(|e| SdkError::Protocol(format!("playback control task ended: {e}")))?
    }
    async fn close_at_inner(&self, position: Option<Duration>) -> Result<(), SdkError> {
        let _control = self.0.control.lock().await;
        {
            let mut state = self.0.state.lock();
            if state.closed {
                return Ok(());
            }
            state.finishing = true;
            if position.is_some() {
                state.pending = position;
                state.position = position;
            }
        }
        let progress = self.flush_resuming_locked().await;
        let stop = match self.0.backend.stop(&self.snapshot().handle).await {
            // Gone on the server already: stopped.
            Err(SdkError::PlaybackExpired) => Ok(()),
            answer => answer,
        };
        if stop.is_ok() {
            let mut state = self.0.state.lock();
            state.closed = true;
            state.pending = None;
            drop(state);
            self.0.settled.send_replace(true);
        }
        progress.and(stop)
    }
}
impl Drop for Inner {
    fn drop(&mut self) {
        self.notify.notify_one();
        let state = self.state.get_mut();
        if state.closed {
            return;
        }
        let (backend, handle, pending) = (
            self.backend.clone(),
            state.playback.handle.clone(),
            state.pending,
        );
        self.runtime.spawn(async move {
            if let Some(position) = pending {
                let _ = backend.progress(&handle, position).await;
            }
            let _ = backend.stop(&handle).await;
        });
    }
}
impl Session {
    pub async fn open_playback(
        &self,
        item: &MediaId,
        selection: Option<Selection>,
        options: PlaybackOptions,
    ) -> Result<ManagedPlayback, SdkError> {
        // A track is kept as it plays, with music downloads.
        let keeper = crate::downloads::Keeper::of(self, item, &options).await;
        let playback =
            ManagedPlayback::open(Arc::new(self.clone()), item.clone(), selection, options).await?;
        if let Some(keeper) = keeper {
            let _ = playback.0.keeper.set(keeper);
        }
        Ok(playback)
    }
}

/// An HLS segment read as it arrives
/// ([`ManagedPlayback::segment_stream`]).
#[derive(Debug)]
pub struct SegmentStream {
    playback: ManagedPlayback,
    body: ChunkStream,
    variant: u32,
    n: u32,
    offset: u64,
    /// Where the segment starts, for the error a resume ends it with.
    at: Option<Duration>,
    /// The handle and generation it was opened under, taken before the
    /// open was awaited: a resume since means the rest would come from
    /// another encode. Its own, whatever other readers of the segment
    /// opened since.
    opened: Opened,
    /// Counted for the step-down already.
    counted: bool,
}

impl SegmentStream {
    /// The bytes this answer carries.
    pub fn sent(&self) -> u64 {
        self.body.sent
    }

    /// The segment's bytes written so far: all of it once [`Self::complete`].
    pub fn size(&self) -> u64 {
        self.body.size
    }

    /// The encode has finished the segment: `offset + sent()` of `size()`
    /// is all there will be.
    pub fn complete(&self) -> bool {
        self.body.complete
    }

    pub fn epoch(&self) -> u32 {
        self.body.epoch
    }

    /// The bytes handed on so far.
    pub fn received(&self) -> u64 {
        self.body.received()
    }

    /// The next piece, as much as arrived; `None` once all `sent()` bytes
    /// did. [`SdkError::PlaybackResumed`] if the playback was resumed in
    /// another encode since it opened, [`SdkError::PlaybackRestarted`] if
    /// the encode was replaced under the same handle (a seek, a subtitle
    /// or quality change, a re-plan): read the segment again from 0. Any
    /// other error part-way (a lost connection, a stall): read on from
    /// `offset + received()`.
    pub async fn next(&mut self) -> Result<Option<Vec<u8>>, SdkError> {
        if !self.opened.current(&self.playback.0.state.lock()) {
            return Err(self.playback.segment_moved_on(&self.opened, self.at));
        }
        let piece = self.body.next().await?;
        // Counted at the read that hands on a whole segment's last byte: a
        // consumer that stops once it has the length it promised (the
        // iOS bridge's hyper does) never reads the end. The end counts it
        // otherwise; `segment_read` takes only a whole one. Once, either
        // way.
        let last = self.body.complete && self.offset + self.body.received() == self.body.size;
        if (last || piece.is_none()) && !self.counted {
            self.counted = true;
            self.playback.segment_read(
                self.n,
                self.offset,
                self.body.received(),
                self.body.size,
                self.body.complete,
            );
        }
        Ok(piece)
    }

    /// Read on from where this stream stopped, `offset + received()`, up
    /// to `length` bytes: after an error part-way (a lost connection, a
    /// stall), or an answer shorter than the segment. The rest comes from
    /// the encode this stream was opened in, or not at all:
    /// [`SdkError::PlaybackResumed`] or [`SdkError::PlaybackRestarted`] if
    /// the encode was replaced since, as [`Self::next`] says; read the
    /// segment again from 0.
    pub async fn reopen(&self, length: u64) -> Result<SegmentStream, SdkError> {
        let playback = &self.playback;
        let (p, now) = playback.live_opened()?;
        if now != self.opened {
            return Err(playback.segment_moved_on(&self.opened, self.at));
        }
        let offset = self.offset + self.received();
        let body = playback
            .open_rest(&p, self.variant, self.n, offset, length, self.at)
            .await?;
        Ok(SegmentStream {
            playback: playback.clone(),
            body,
            variant: self.variant,
            n: self.n,
            offset,
            at: self.at,
            opened: self.opened.clone(),
            counted: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spoolframe_types::{Container, MediaKind};
    #[derive(Default)]
    struct Backend {
        events: Mutex<Vec<String>>,
        starts: std::sync::atomic::AtomicUsize,
        expired: std::sync::atomic::AtomicBool,
        stopped: std::sync::atomic::AtomicBool,
        hold_start: std::sync::atomic::AtomicBool,
        hold_seek: std::sync::atomic::AtomicBool,
        /// Hold the next quality change on the server until `release`.
        hold_quality: std::sync::atomic::AtomicBool,
        entered: Notify,
        release: Notify,
        /// The server's share events, once a test sends some (PX-46).
        shares: Mutex<Option<broadcast::Sender<PlaybackShare>>>,
        /// Start a progressive conversion, not a direct play.
        converted: std::sync::atomic::AtomicBool,
        /// Chunks read.
        chunks: std::sync::atomic::AtomicUsize,
        /// The subtitle file's answers, in turn; the plan's subtitle is
        /// read as text while any are left.
        subtitles: Mutex<VecDeque<Result<Subtitle, SdkError>>>,
        /// Plays as a paired server's item, from that server.
        routed: std::sync::atomic::AtomicBool,
        /// Direct reads made.
        reads: std::sync::atomic::AtomicUsize,
        /// Starts refused as Busy before one is let through.
        busy: std::sync::atomic::AtomicUsize,
        /// The converted subtitle is copied into the output.
        copy_subtitle: std::sync::atomic::AtomicBool,
        /// The server's subtitle timing changes, once a test sends some.
        timings: Mutex<Option<broadcast::Sender<crate::SubtitleTimingEvent>>>,
    }
    use std::sync::atomic::Ordering::SeqCst;
    fn item() -> MediaId {
        MediaId::new(spoolframe_types::Namespace::Plex, MediaKind::Movie, "1").unwrap()
    }
    fn playback(n: usize) -> Playback {
        Playback {
            handle: PlaybackHandle(n.to_string()),
            plan: PlaybackPlan::Direct {
                container: Container::Matroska,
                video: None,
                audio: None,
                subtitle: None,
                source_codec: None,
            },
            size: 100,
            chosen: None,
            epoch: 0,
            markers: vec![],
        }
    }
    #[async_trait]
    impl PlaybackBackend for Backend {
        async fn start(
            &self,
            _: &MediaId,
            _: Option<Selection>,
            _: PlaybackOptions,
        ) -> Result<Playback, SdkError> {
            if self
                .busy
                .try_update(SeqCst, SeqCst, |n| n.checked_sub(1))
                .is_ok()
            {
                self.events.lock().push("busy".into());
                return Err(SdkError::Busy);
            }
            let n = self.starts.fetch_add(1, SeqCst) + 1;
            if self.hold_start.load(SeqCst) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            let mut p = playback(n);
            if self.converted.load(SeqCst) {
                p.size = 0;
                p.plan = PlaybackPlan::Converted {
                    delivery: spoolframe_types::Delivery::Progressive,
                    container: Container::Matroska,
                    video: None,
                    audio: None,
                    subtitle: if self.copy_subtitle.load(SeqCst) {
                        Some(spoolframe_protocol::ConvertedSubtitle {
                            index: 0,
                            action: spoolframe_protocol::SubtitleAction::Copy {
                                format: spoolframe_types::SubtitleFormat::Srt,
                            },
                        })
                    } else {
                        (!self.subtitles.lock().is_empty()).then_some(
                            spoolframe_protocol::ConvertedSubtitle {
                                index: 0,
                                action: spoolframe_protocol::SubtitleAction::Ocr {
                                    from: spoolframe_types::SubtitleFormat::Pgs,
                                    to: spoolframe_types::SubtitleFormat::Srt,
                                },
                            },
                        )
                    },
                    ladder: Vec::new(),
                    reasons: vec![],
                    source_codec: None,
                };
            }
            Ok(p)
        }
        async fn read(&self, h: &PlaybackHandle, _: u64, _: u64) -> Result<Vec<u8>, SdkError> {
            self.reads.fetch_add(1, SeqCst);
            if self.stopped.load(SeqCst) {
                Err(SdkError::PlaybackStopped)
            } else if h.0 == "1" && self.expired.load(SeqCst) {
                Err(SdkError::PlaybackExpired)
            } else {
                Ok(vec![42])
            }
        }
        async fn read_chunk(
            &self,
            _: &PlaybackHandle,
            e: u32,
            _: u64,
            _: u64,
        ) -> Result<Chunk, SdkError> {
            self.chunks.fetch_add(1, SeqCst);
            Ok(Chunk {
                bytes: vec![42],
                size: 1,
                complete: true,
                epoch: e,
            })
        }
        async fn seek(&self, _: &PlaybackHandle, _: Duration) -> Result<u32, SdkError> {
            if self.hold_seek.load(SeqCst) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            Ok(1)
        }
        async fn subtitle(
            &self,
            _: &PlaybackHandle,
            selected: Option<u32>,
            _: BitmapSubtitles,
            at: Duration,
        ) -> Result<PlaybackChange, SdkError> {
            self.events
                .lock()
                .push(format!("subtitle-change:{selected:?}:{}", at.as_secs()));
            Err(SdkError::Busy)
        }
        async fn progress(&self, h: &PlaybackHandle, p: Duration) -> Result<(), SdkError> {
            if h.0 == "1" && self.expired.load(SeqCst) {
                return Err(SdkError::PlaybackExpired);
            }
            self.events
                .lock()
                .push(format!("progress:{}:{}", h.0, p.as_secs()));
            Ok(())
        }
        async fn stop(&self, h: &PlaybackHandle) -> Result<(), SdkError> {
            self.events.lock().push(format!("stop:{}", h.0));
            self.entered.notify_one();
            Ok(())
        }
        async fn change_quality(
            &self,
            h: &PlaybackHandle,
            quality: PlaybackQuality,
            _: BitmapSubtitles,
            position: Duration,
        ) -> Result<PlaybackChange, SdkError> {
            self.events.lock().push(format!(
                "quality:{}:{quality:?}:{}",
                h.0,
                position.as_secs()
            ));
            if self.hold_quality.swap(false, SeqCst) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            Ok(PlaybackChange {
                plan: playback(1).plan,
                size: 100,
                epoch: 0,
            })
        }
        fn route(&self) -> Option<crate::RemoteMode> {
            self.routed
                .load(SeqCst)
                .then_some(crate::RemoteMode::Direct)
        }
        fn subtitle_timings(
            &self,
        ) -> Option<(String, broadcast::Receiver<crate::SubtitleTimingEvent>)> {
            let timings = self.timings.lock();
            Some(("srv".into(), timings.as_ref()?.subscribe()))
        }
        async fn read_subtitle(&self, _: &PlaybackHandle) -> Result<Subtitle, SdkError> {
            self.events.lock().push("subtitle".into());
            self.subtitles.lock().pop_front().expect("an answer")
        }
        fn shares(&self) -> Option<broadcast::Receiver<PlaybackShare>> {
            self.shares
                .lock()
                .as_ref()
                .map(broadcast::Sender::subscribe)
        }
    }
    async fn open(b: Arc<Backend>) -> ManagedPlayback {
        ManagedPlayback::open(b, item(), None, PlaybackOptions::default())
            .await
            .unwrap()
    }
    async fn until(mut done: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !done() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    fn share(handle: &str, max_bitrate: Option<u64>) -> PlaybackShare {
        PlaybackShare {
            handle: PlaybackHandle(handle.into()),
            max_bitrate,
        }
    }

    /// The tests a caller makes of it read it as it is now, with no copy.
    #[tokio::test]
    async fn a_playback_is_read_as_it_is_now() {
        let b = Arc::new(Backend::default());
        b.converted.store(true, SeqCst);
        let p = open(b).await;
        let now = p.snapshot();
        assert_eq!(
            (
                p.is_planned(&now.plan),
                p.is_planned(&playback(1).plan),
                p.is_converted(),
                p.is_handle(&PlaybackHandle("1".into())),
                p.is_handle(&PlaybackHandle("2".into())),
            ),
            (true, false, true, true, false)
        );
    }

    /// PX-46: the server tells this playback its share moved; in Auto it
    /// plans itself again where the viewer is. Another handle's event, or
    /// a quality the viewer chose, changes nothing.
    #[tokio::test]
    async fn a_moved_share_plans_an_auto_playback_again() {
        let b = Arc::new(Backend::default());
        let (shares, _) = broadcast::channel(4);
        *b.shares.lock() = Some(shares.clone());
        let p = open(b.clone()).await;
        p.report_position(Duration::from_secs(42)).unwrap();
        shares.send(share("someone-else", Some(1_000_000))).unwrap();
        assert_eq!(p.share(), None, "another playback's share");
        shares.send(share("1", Some(4_500_000))).unwrap();
        until(|| {
            b.events
                .lock()
                .iter()
                .any(|e| e.starts_with("quality:1:Auto:42"))
        })
        .await;
        assert_eq!(p.share(), Some(share("1", Some(4_500_000))));
        assert_eq!(
            b.events
                .lock()
                .iter()
                .filter(|e| e.starts_with("quality:"))
                .count(),
            1
        );
        // Chosen by the viewer: the share is not theirs to follow.
        p.change_quality(PlaybackQuality::Bitrate(4_000_000), Duration::from_secs(42))
            .await
            .unwrap();
        shares.send(share("1", Some(3_000_000))).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Kept all the same, for the app to show.
        assert_eq!(p.share(), Some(share("1", Some(3_000_000))));
        assert_eq!(
            b.events
                .lock()
                .iter()
                .filter(|e| e.starts_with("quality:1:Auto"))
                .count(),
            1
        );
    }

    /// The iOS build 42 review: the viewer picked Original and ended on
    /// Auto. A share event that arrived while the pick was on the server
    /// found the playback still in Auto and queued a re-plan for Auto
    /// behind it; the re-plan must not undo the pick.
    #[tokio::test]
    async fn a_share_queued_behind_a_quality_pick_does_not_undo_it() {
        let b = Arc::new(Backend::default());
        let (shares, _) = broadcast::channel(4);
        *b.shares.lock() = Some(shares.clone());
        let p = open(b.clone()).await;
        b.hold_quality.store(true, SeqCst);
        let q = p.clone();
        let pick = tokio::spawn(async move {
            q.change_quality(PlaybackQuality::Original, Duration::from_secs(42))
                .await
        });
        b.entered.notified().await;
        shares.send(share("1", Some(4_500_000))).unwrap();
        // Taken as this playback's, while it was still in Auto.
        until(|| p.share().is_some()).await;
        b.release.notify_one();
        pick.await.unwrap().unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(p.quality(), PlaybackQuality::Original);
        assert!(
            !b.events
                .lock()
                .iter()
                .any(|e| e.starts_with("quality:1:Auto")),
            "{:?}",
            b.events.lock()
        );
    }

    /// B2: the task that follows the server's share events ends with the
    /// playback, instead of staying parked on a channel that never closes
    /// (one task per playback opened, for the life of the app).
    #[tokio::test]
    async fn the_share_task_ends_when_the_playback_is_dropped() {
        let b = Arc::new(Backend::default());
        let (shares, _) = broadcast::channel(4);
        *b.shares.lock() = Some(shares.clone());
        let p = open(b.clone()).await;
        assert_eq!(shares.receiver_count(), 1);
        drop(p);
        until(|| shares.receiver_count() == 0).await;
    }

    /// B3: two opens of one segment from 0, either side of a seek, whose
    /// answers come back in the wrong order. The older one, recorded
    /// last, must not hide the newer: a read of the rest in the current
    /// encode goes on.
    #[tokio::test]
    async fn a_late_older_open_of_a_segment_does_not_hide_a_newer_one() {
        let b = Arc::new(Backend::default());
        let p = open(b).await;
        let older = Opened::now(&p.0.state.lock());
        p.seek(Duration::from_secs(10)).await.unwrap();
        let newer = Opened::now(&p.0.state.lock());
        p.opened_segment(0, 1, &newer);
        p.opened_segment(0, 1, &older);
        assert_eq!(p.rest_of(0, 1, None).map(|(_, o)| o), Ok(newer));
    }

    /// B4: a re-plan for the link whose progress report finds the
    /// playback ended starts it again where the viewer was, as a seek or
    /// a quality change does, instead of failing with PlaybackExpired.
    #[tokio::test]
    async fn a_limit_that_finds_the_playback_ended_starts_it_again() {
        let b = Arc::new(Backend::default());
        let p = open(b.clone()).await;
        b.expired.store(true, SeqCst);
        p.report_position(Duration::from_secs(42)).unwrap();
        let limited = p.limit(1_000_000).await.map(|p| p.handle);
        assert_eq!(limited, Ok(PlaybackHandle("2".into())));
        assert!(
            b.events.lock().contains(&"progress:2:42".into()),
            "{:?}",
            b.events.lock()
        );
    }

    /// Poll `future` to its end on this thread, with no Tokio runtime
    /// entered: as a player's own thread calls the SDK.
    fn block_on_plain_thread<F: Future>(future: F) -> F::Output {
        struct Unpark(std::thread::Thread);
        impl std::task::Wake for Unpark {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }
        let waker = Arc::new(Unpark(std::thread::current())).into();
        let mut cx = std::task::Context::from_waker(&waker);
        let mut future = std::pin::pin!(future);
        loop {
            if let std::task::Poll::Ready(out) = future.as_mut().poll(&mut cx) {
                return out;
            }
            std::thread::park();
        }
    }

    /// B5: a read from a thread outside any Tokio runtime that finds the
    /// playback ended resumes it on the playback's own runtime, instead
    /// of failing with InvalidInput.
    #[test]
    fn a_resume_from_outside_a_runtime_starts_the_playback_again() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let b = Arc::new(Backend::default());
        let p = runtime.block_on(open(b.clone()));
        b.expired.store(true, SeqCst);
        assert!(tokio::runtime::Handle::try_current().is_err());
        let read = block_on_plain_thread(p.read(0, 1));
        assert_eq!(read, Ok(vec![42]));
        assert_eq!(p.snapshot().handle, PlaybackHandle("2".into()));
        drop(p);
        drop(runtime);
    }

    #[test]
    fn a_link_under_what_the_segments_need_is_too_slow() {
        // Four 6 s segments of 6 MB: 8 Mbit/s.
        let segments = [(6_000_000, Duration::from_secs(6)); 4];
        assert!(too_slow(7_000_000.0, &segments));
        assert!(!too_slow(20_000_000.0, &segments));
        // Nothing timed says nothing.
        assert!(!too_slow(1.0, &[(1_000, Duration::ZERO)]));
    }

    #[tokio::test]
    async fn cancelled_start_stops_its_admitted_handle() {
        let b = Arc::new(Backend::default());
        b.hold_start.store(true, SeqCst);
        let task = tokio::spawn(open(b.clone()));
        b.entered.notified().await;
        task.abort();
        let _ = task.await;
        b.release.notify_one();
        until(|| b.events.lock().contains(&"stop:1".into())).await;
    }
    #[tokio::test]
    async fn concurrent_expiry_replaces_one_handle_for_reads_and_progress() {
        let b = Arc::new(Backend::default());
        let p = open(b.clone()).await;
        let mut changes = p.changes();
        b.expired.store(true, SeqCst);
        let (a, c) = tokio::join!(p.read(0, 1), p.read(1, 1));
        assert_eq!(a.unwrap(), vec![42]);
        assert_eq!(c.unwrap(), vec![42]);
        assert_eq!(b.starts.load(SeqCst), 2);
        changes.changed().await.unwrap();
        assert_eq!(changes.borrow().handle.0, "2");
        // A replacement handle can reuse epoch zero. Its bytes do not belong
        // in a reader of the previous handle, even though the epochs match:
        // that reader is told where the playback was resumed.
        assert_eq!(
            p.read_chunk(&playback(1), 0, 1).await,
            Err(SdkError::PlaybackResumed { at: Duration::ZERO })
        );
        p.finish(Duration::from_secs(90)).await.unwrap();
        assert_eq!(*b.events.lock(), vec!["progress:2:90", "stop:2"]);
    }
    /// An admin's Stop (P3-15) is the end: the playback is not started
    /// again behind the viewer's back, as an expired one is.
    #[tokio::test]
    async fn a_playback_stopped_on_the_server_is_not_started_again() {
        let b = Arc::new(Backend::default());
        let p = open(b.clone()).await;
        b.stopped.store(true, SeqCst);
        assert_eq!(p.read(0, 1).await, Err(SdkError::PlaybackStopped));
        assert_eq!(b.starts.load(SeqCst), 1);
        assert_eq!(p.snapshot().handle.0, "1");
    }
    #[tokio::test]
    async fn final_position_supersedes_pending_and_precedes_stop() {
        let b = Arc::new(Backend::default());
        let p = open(b.clone()).await;
        p.report_position(Duration::from_secs(10)).unwrap();
        p.report_position(Duration::from_secs(20)).unwrap();
        p.finish(Duration::from_secs(30)).await.unwrap();
        assert_eq!(*b.events.lock(), vec!["progress:1:30", "stop:1"]);
        assert_eq!(
            p.report_position(Duration::ZERO),
            Err(SdkError::PlaybackClosed)
        );
        p.close().await.unwrap();
        assert_eq!(b.events.lock().len(), 2);
    }
    #[tokio::test]
    async fn cancelling_seek_still_installs_the_new_epoch() {
        let b = Arc::new(Backend::default());
        let p = open(b.clone()).await;
        b.hold_seek.store(true, SeqCst);
        let q = p.clone();
        let task = tokio::spawn(async move { q.seek(Duration::from_secs(10)).await });
        b.entered.notified().await;
        task.abort();
        let _ = task.await;
        b.release.notify_one();
        until(|| p.snapshot().epoch == 1).await;
        assert_eq!(
            p.read_chunk(&playback(1), 0, 1).await,
            Err(SdkError::PlaybackRestarted { epoch: 1 })
        );
        p.close().await.unwrap();
    }
    #[tokio::test]
    async fn only_the_last_owner_drop_stops_playback() {
        let b = Arc::new(Backend::default());
        let p = open(b.clone()).await;
        let q = p.clone();
        drop(p);
        assert!(b.events.lock().is_empty());
        drop(q);
        until(|| b.events.lock().contains(&"stop:1".into())).await;
    }
    #[tokio::test]
    async fn pending_progress_does_not_block_recovery_on_seek() {
        let b = Arc::new(Backend::default());
        let p = open(b.clone()).await;
        p.report_position(Duration::from_secs(10)).unwrap();
        b.expired.store(true, SeqCst);
        let next = p.seek(Duration::from_secs(60)).await.unwrap();
        assert_eq!(next.handle.0, "2");
        assert_eq!(*b.events.lock(), vec!["progress:2:10"]);
        p.close().await.unwrap();
    }

    fn progress(b: &Backend) -> Vec<String> {
        b.events
            .lock()
            .iter()
            .filter(|e| e.starts_with("progress:"))
            .cloned()
            .collect()
    }

    fn playing(secs: u64) -> PlayerState {
        PlayerState::Playing {
            pos: Duration::from_secs(secs),
        }
    }

    fn paused(secs: u64) -> PlayerState {
        PlayerState::Paused {
            pos: Duration::from_secs(secs),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_playing_playback_reports_every_ten_seconds() {
        let b = Arc::new(Backend::default());
        let p = open(b.clone()).await;
        p.set_state(playing(3)).unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(progress(&b).is_empty(), "{:?}", progress(&b));
        p.set_state(playing(8)).unwrap();
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert_eq!(progress(&b), ["progress:1:8"]);
        p.set_state(playing(18)).unwrap();
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert_eq!(progress(&b), ["progress:1:8", "progress:1:18"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_move_backwards_is_reported_at_once() {
        let b = Arc::new(Backend::default());
        let p = open(b.clone()).await;
        p.set_state(playing(300)).unwrap();
        p.set_state(playing(300) /* jitter */).unwrap();
        p.set_state(PlayerState::Playing {
            pos: Duration::from_millis(299_500),
        })
        .unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(progress(&b).is_empty());
        p.set_state(playing(40)).unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(progress(&b), ["progress:1:40"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_pause_is_reported_at_once_and_kept_alive_every_minute() {
        let b = Arc::new(Backend::default());
        let p = open(b.clone()).await;
        p.set_state(playing(30)).unwrap();
        p.set_state(paused(31)).unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(progress(&b), ["progress:1:31"]);
        // Paused again: nothing new to say.
        p.set_state(paused(31)).unwrap();
        tokio::time::sleep(Duration::from_secs(55)).await;
        assert_eq!(progress(&b).len(), 1);
        tokio::time::sleep(Duration::from_secs(15)).await;
        assert_eq!(progress(&b), ["progress:1:31", "progress:1:31"]);
    }

    /// A paired server's item reports home: the server that plays it is
    /// kept by a read, and home still hears where it is.
    #[tokio::test(start_paused = true)]
    async fn a_paused_paired_servers_item_is_kept_alive_by_a_read() {
        let b = Arc::new(Backend::default());
        b.routed.store(true, SeqCst);
        let p = open(b.clone()).await;
        p.set_state(paused(31)).unwrap();
        tokio::time::sleep(Duration::from_secs(55)).await;
        assert_eq!(b.reads.load(SeqCst), 0);
        // The tick at 70 s, a minute after the pause's report.
        tokio::time::sleep(Duration::from_secs(20)).await;
        assert_eq!(b.reads.load(SeqCst), 1);
        assert_eq!(progress(&b), ["progress:1:31", "progress:1:31"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_server_that_keeps_paused_playbacks_gets_no_keep_alive() {
        let b = Arc::new(Backend::default());
        let p = open(b.clone()).await;
        p.set_keepalive_needed(false);
        p.set_state(paused(31)).unwrap();
        tokio::time::sleep(Duration::from_secs(300)).await;
        assert_eq!(progress(&b), ["progress:1:31"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_conversion_that_reads_nothing_reads_a_byte_every_minute() {
        let b = Arc::new(Backend::default());
        b.converted.store(true, SeqCst);
        let p = open(b.clone()).await;
        p.set_state(playing(0)).unwrap();
        tokio::time::sleep(Duration::from_secs(55)).await;
        assert_eq!(b.chunks.load(SeqCst), 0);
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert_eq!(b.chunks.load(SeqCst), 1);
        // Reads of its own count as use.
        p.read_chunk(&p.snapshot(), 0, 1).await.unwrap();
        tokio::time::sleep(Duration::from_secs(55)).await;
        assert_eq!(b.chunks.load(SeqCst), 2);
        p.set_keepalive_needed(false);
        tokio::time::sleep(Duration::from_secs(300)).await;
        assert_eq!(b.chunks.load(SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn flush_and_close_reports_the_last_position_then_stops() {
        let b = Arc::new(Backend::default());
        let p = open(b.clone()).await;
        p.set_state(playing(70)).unwrap();
        p.flush_and_close().await.unwrap();
        assert_eq!(*b.events.lock(), ["progress:1:70", "stop:1"]);
        assert_eq!(p.set_state(playing(71)), Err(SdkError::PlaybackClosed));
        assert_eq!(p.stopped(), Some(PlaybackStopped::Closed));
    }

    #[tokio::test]
    async fn a_lost_transport_starts_a_new_playback_where_the_viewer_was() {
        let b = Arc::new(Backend::default());
        let p = open(b.clone()).await;
        p.set_state(playing(42)).unwrap();
        let next = p.restart(RestartCause::TransportLost).await.unwrap();
        assert_eq!(next.handle.0, "2");
        until(|| b.events.lock().contains(&"stop:1".into())).await;
        assert!(b.events.lock().contains(&"progress:2:42".into()));
    }

    fn srt(text: &str) -> Result<Subtitle, SdkError> {
        Ok(Subtitle {
            format: spoolframe_types::SubtitleFormat::Srt,
            bytes: text.into(),
        })
    }

    #[tokio::test(start_paused = true)]
    async fn a_subtitle_still_being_read_is_asked_for_every_five_seconds() {
        let b = Arc::new(Backend::default());
        b.converted.store(true, SeqCst);
        *b.subtitles.lock() = VecDeque::from([
            Err(SdkError::SubtitleNotReady { done_percent: 10 }),
            Err(SdkError::SubtitleNotReady { done_percent: 60 }),
            srt("1\n00:00:01,000 --> 00:00:02,000\nHi\n"),
        ]);
        let p = open(b.clone()).await;
        let asked = Instant::now();
        let file = p.subtitle_file().await.unwrap();
        assert_eq!(asked.elapsed(), 2 * ManagedPlayback::SUBTITLE_RETRY);
        assert_eq!(file.format, spoolframe_types::SubtitleFormat::Srt);
        assert_eq!(
            b.events.lock().iter().filter(|e| *e == "subtitle").count(),
            3
        );
    }

    #[tokio::test]
    async fn an_empty_subtitle_file_is_refused() {
        let b = Arc::new(Backend::default());
        b.converted.store(true, SeqCst);
        *b.subtitles.lock() = VecDeque::from([srt("  \n")]);
        let p = open(b.clone()).await;
        assert!(matches!(
            p.subtitle_file().await,
            Err(SdkError::Protocol(why)) if why.contains("empty")
        ));
    }

    #[tokio::test]
    async fn a_plan_with_no_subtitle_file_has_none_to_fetch() {
        let b = Arc::new(Backend::default());
        let p = open(b.clone()).await;
        assert!(matches!(
            p.subtitle_file().await,
            Err(SdkError::InvalidInput(_))
        ));
        assert!(b.events.lock().is_empty());
    }

    fn timing(offset_ms: i64, sync: spoolframe_types::SubtitleSync) -> crate::SubtitleTimingEvent {
        crate::SubtitleTimingEvent {
            server: "srv".into(),
            id: item(),
            track: 0,
            timing: spoolframe_types::SubtitleTiming {
                sync,
                offset_ms,
                offset_by: None,
                offset_at: None,
            },
        }
    }

    /// A playback showing subtitle 0, whose server sends timing changes.
    async fn showing_subtitle(
        b: &Arc<Backend>,
    ) -> (
        ManagedPlayback,
        broadcast::Sender<crate::SubtitleTimingEvent>,
    ) {
        let (timings, _) = broadcast::channel(4);
        *b.timings.lock() = Some(timings.clone());
        let selection = Selection {
            audio: None,
            subtitle: Some(0),
        };
        let p = ManagedPlayback::open(
            b.clone(),
            item(),
            Some(selection),
            PlaybackOptions::default(),
        )
        .await
        .unwrap();
        p.report_position(Duration::from_secs(42)).unwrap();
        (p, timings)
    }

    /// A subtitle in a converted output carries its timing in the
    /// encode: a change on another device plans the playback again.
    #[tokio::test]
    async fn a_retimed_subtitle_in_the_output_plans_the_playback_again() {
        let b = Arc::new(Backend::default());
        b.converted.store(true, SeqCst);
        b.copy_subtitle.store(true, SeqCst);
        let (_p, timings) = showing_subtitle(&b).await;
        timings
            .send(timing(500, spoolframe_types::SubtitleSync::Original))
            .unwrap();
        until(|| {
            b.events
                .lock()
                .contains(&"subtitle-change:Some(0):42".into())
        })
        .await;
    }

    /// One read as text comes as a file: the app is told to read it
    /// again. A sync that has only begun, or another server's track,
    /// changes nothing.
    #[tokio::test]
    async fn a_retimed_subtitle_file_is_read_again() {
        let b = Arc::new(Backend::default());
        b.converted.store(true, SeqCst);
        *b.subtitles.lock() = VecDeque::from([srt("1")]);
        let (p, timings) = showing_subtitle(&b).await;
        let mut events = p.events();
        let mut elsewhere = timing(100, spoolframe_types::SubtitleSync::Original);
        elsewhere.server = "other".into();
        timings.send(elsewhere).unwrap();
        timings
            .send(timing(0, spoolframe_types::SubtitleSync::Original))
            .unwrap();
        assert_eq!(
            events.recv().await,
            Ok(PlaybackEvent::SubtitleRetimed { track: 0 })
        );
        timings
            .send(timing(0, spoolframe_types::SubtitleSync::Syncing))
            .unwrap();
        timings
            .send(timing(300, spoolframe_types::SubtitleSync::Syncing))
            .unwrap();
        assert_eq!(
            events.recv().await,
            Ok(PlaybackEvent::SubtitleRetimed { track: 0 })
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(events.try_recv().is_err());
        assert!(
            !b.events
                .lock()
                .iter()
                .any(|e| e.starts_with("subtitle-change"))
        );
    }

    /// An AirPlay or Chromecast receiver holds a URL bound to the
    /// handle: a new handle is an event.
    #[tokio::test]
    async fn a_new_handle_is_an_event() {
        let b = Arc::new(Backend::default());
        let p = open(b.clone()).await;
        let mut events = p.events();
        b.expired.store(true, SeqCst);
        p.read(0, 1).await.unwrap();
        assert_eq!(
            events.recv().await,
            Ok(PlaybackEvent::HandleChanged {
                handle: PlaybackHandle("2".into())
            })
        );
        // A seek keeps the handle.
        p.seek(Duration::from_secs(5)).await.unwrap();
        assert!(events.try_recv().is_err());
    }

    async fn next_after(
        b: &Arc<Backend>,
        after: &ManagedPlayback,
    ) -> Result<ManagedPlayback, SdkError> {
        ManagedPlayback::open_after(b.clone(), item(), after, None, PlaybackOptions::default())
            .await
    }

    /// Opened at once, the next could end the one playing before its
    /// player read a byte: it waits for that read, 10 s at most.
    #[tokio::test(start_paused = true)]
    async fn the_next_item_waits_for_the_one_playing_to_be_read() {
        let b = Arc::new(Backend::default());
        let playing = open(b.clone()).await;
        let asked = Instant::now();
        next_after(&b, &playing).await.unwrap();
        assert_eq!(asked.elapsed(), Duration::from_secs(10));
        playing.read(0, 1).await.unwrap();
        let asked = Instant::now();
        next_after(&b, &playing).await.unwrap();
        assert_eq!(asked.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn a_busy_next_item_is_tried_again_four_times_in_all() {
        let b = Arc::new(Backend::default());
        let playing = open(b.clone()).await;
        playing.read(0, 1).await.unwrap();
        b.busy.store(3, SeqCst);
        let asked = Instant::now();
        assert!(next_after(&b, &playing).await.is_ok());
        // 500 ms, then 1 s, then 1.5 s.
        assert_eq!(asked.elapsed(), Duration::from_millis(3_000));
        b.busy.store(4, SeqCst);
        assert_eq!(next_after(&b, &playing).await.err(), Some(SdkError::Busy));
        assert_eq!(b.busy.load(SeqCst), 0);
    }

    /// The viewer picked one item, then another while the first was
    /// still being started: the first's late answer is ended, not played.
    #[tokio::test]
    async fn a_start_overtaken_by_another_is_ended_as_it_answers() {
        let b = Arc::new(Backend::default());
        let session = PlayerSession::new(b.clone());
        b.hold_start.store(true, SeqCst);
        let first = {
            let session = session.clone();
            tokio::spawn(async move {
                session
                    .start(item(), None, PlaybackOptions::default())
                    .await
            })
        };
        b.entered.notified().await;
        b.hold_start.store(false, SeqCst);
        let second = {
            let session = session.clone();
            tokio::spawn(async move {
                session
                    .start(item(), None, PlaybackOptions::default())
                    .await
            })
        };
        tokio::task::yield_now().await;
        b.release.notify_one();
        assert_eq!(first.await.unwrap().err(), Some(SdkError::PlaybackClosed));
        let second = second.await.unwrap().unwrap();
        assert_eq!(second.snapshot().handle.0, "2");
        assert_eq!(
            session.current().map(|p| p.snapshot().handle.0),
            Some("2".into())
        );
        assert!(b.events.lock().contains(&"stop:1".into()));
    }

    /// A new start stops what played, after its last report, before it
    /// is asked; an end stops it too.
    #[tokio::test]
    async fn a_start_ends_what_played_first_and_an_end_ends_it() {
        let b = Arc::new(Backend::default());
        let session = PlayerSession::new(b.clone());
        let one = session
            .start(item(), None, PlaybackOptions::default())
            .await
            .unwrap();
        one.report_position(Duration::from_secs(30)).unwrap();
        session
            .start(item(), None, PlaybackOptions::default())
            .await
            .unwrap();
        session.end().await;
        assert_eq!(*b.events.lock(), ["progress:1:30", "stop:1", "stop:2"]);
        assert!(session.current().is_none());
        assert_eq!(
            session.restart(RestartCause::Expired).await.err(),
            Some(SdkError::PlaybackClosed)
        );
    }

    #[test]
    fn an_error_says_whether_the_playback_is_over_for_good() {
        assert_eq!(
            PlaybackStopped::of(&SdkError::PlaybackStopped),
            Some(PlaybackStopped::ByAdmin)
        );
        assert_eq!(
            PlaybackStopped::of(&SdkError::PlaybackExpired),
            Some(PlaybackStopped::TooManyRestarts)
        );
        assert_eq!(PlaybackStopped::of(&SdkError::Busy), None);
    }

    /// A server that ends every live playback on `expire`, as idling out,
    /// a restart or a device gone do; it records each start.
    mod resume {
        use super::*;
        use spoolframe_types::{Container, Delivery};
        use std::collections::HashSet;

        #[derive(Default)]
        struct Server {
            live: Mutex<HashSet<String>>,
            starts: Mutex<Vec<(Option<Selection>, PlaybackOptions)>>,
            refuse: Mutex<Option<SdkError>>,
            /// Every start asked for, refused or not.
            asked: std::sync::atomic::AtomicUsize,
            delivery: Mutex<Option<Delivery>>,
            reports: Mutex<Vec<(String, u64)>>,
            /// Hold the next segment read, once the handle was found live,
            /// until `release`: an answer still on the link.
            hold_read: std::sync::atomic::AtomicBool,
            entered: Notify,
            release: Notify,
            /// Each segment's size; 1 byte if unset.
            segment_len: Mutex<Option<u64>>,
            /// The link the server is on, once a test measures one.
            meter: Mutex<Option<Arc<crate::LinkMeter>>>,
            /// Each link bitrate it was asked to plan for.
            limits: Mutex<Vec<u64>>,
            /// When set, a limit replaces the encode under the same handle
            /// with this epoch, as the server's re-plan for a slow link
            /// does; else it is refused.
            replan_epoch: Mutex<Option<u32>>,
            /// The plan of the last start.
            plan: Mutex<Option<PlaybackPlan>>,
            /// Each track choice kept.
            overrides: Mutex<Vec<Selection>>,
        }
        impl Server {
            fn expire(&self) {
                self.live.lock().clear();
            }
            fn alive(&self, h: &PlaybackHandle) -> Result<(), SdkError> {
                if self.live.lock().contains(&h.0) {
                    Ok(())
                } else {
                    Err(SdkError::PlaybackExpired)
                }
            }
            fn started(&self) -> usize {
                self.starts.lock().len()
            }
            fn last_start(&self) -> Option<Duration> {
                self.starts.lock().last().unwrap().1.start
            }
        }
        /// Five 6 s segments.
        const PLAYLIST: &str = "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:6\n\
            #EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-MAP:URI=\"init.mp4\"\n\
            #EXTINF:6.000,\nseg1.m4s\n#EXTINF:6.000,\nseg2.m4s\n#EXTINF:6.000,\nseg3.m4s\n\
            #EXTINF:6.000,\nseg4.m4s\n#EXTINF:6.000,\nseg5.m4s\n#EXT-X-ENDLIST\n";
        #[async_trait]
        impl PlaybackBackend for Server {
            async fn start(
                &self,
                _: &MediaId,
                selection: Option<Selection>,
                options: PlaybackOptions,
            ) -> Result<Playback, SdkError> {
                self.asked.fetch_add(1, SeqCst);
                if let Some(e) = self.refuse.lock().clone() {
                    return Err(e);
                }
                let mut starts = self.starts.lock();
                starts.push((selection, options));
                let handle = starts.len().to_string();
                self.live.lock().insert(handle.clone());
                let mut p = playback(0);
                p.handle = PlaybackHandle(handle);
                if let Some(delivery) = *self.delivery.lock() {
                    p.size = 0;
                    p.plan = PlaybackPlan::Converted {
                        delivery,
                        container: Container::Mp4,
                        video: None,
                        audio: None,
                        subtitle: None,
                        ladder: Vec::new(),
                        reasons: vec![],
                        source_codec: None,
                    };
                }
                *self.plan.lock() = Some(p.plan.clone());
                Ok(p)
            }
            async fn read(&self, h: &PlaybackHandle, _: u64, _: u64) -> Result<Vec<u8>, SdkError> {
                self.alive(h)?;
                Ok(vec![42])
            }
            async fn read_chunk(
                &self,
                h: &PlaybackHandle,
                e: u32,
                _: u64,
                _: u64,
            ) -> Result<Chunk, SdkError> {
                self.alive(h)?;
                Ok(Chunk {
                    bytes: vec![42],
                    size: 1,
                    complete: true,
                    epoch: e,
                })
            }
            async fn seek(&self, h: &PlaybackHandle, _: Duration) -> Result<u32, SdkError> {
                self.alive(h)?;
                Ok(1)
            }
            async fn subtitle(
                &self,
                h: &PlaybackHandle,
                _: Option<u32>,
                _: BitmapSubtitles,
                _: Duration,
            ) -> Result<PlaybackChange, SdkError> {
                self.alive(h)?;
                Err(SdkError::Busy)
            }
            async fn progress(&self, h: &PlaybackHandle, p: Duration) -> Result<(), SdkError> {
                self.alive(h)?;
                self.reports.lock().push((h.0.clone(), p.as_secs()));
                Ok(())
            }
            async fn stop(&self, h: &PlaybackHandle) -> Result<(), SdkError> {
                self.alive(h)?;
                self.live.lock().remove(&h.0);
                Ok(())
            }
            fn link_meter(&self) -> Option<Arc<crate::LinkMeter>> {
                self.meter.lock().clone()
            }
            async fn set_track_override(
                &self,
                _: &MediaId,
                selection: Selection,
            ) -> Result<(), SdkError> {
                self.overrides.lock().push(selection);
                Ok(())
            }
            async fn limit(
                &self,
                h: &PlaybackHandle,
                link_bitrate: u64,
                _: BitmapSubtitles,
                _: Duration,
            ) -> Result<PlaybackChange, SdkError> {
                self.alive(h)?;
                self.limits.lock().push(link_bitrate);
                match *self.replan_epoch.lock() {
                    Some(epoch) => Ok(PlaybackChange {
                        plan: self.plan.lock().clone().expect("started"),
                        size: 0,
                        epoch,
                    }),
                    // What the plan becomes is not what these tests look at.
                    None => Err(SdkError::Busy),
                }
            }
            async fn manifest(
                &self,
                h: &PlaybackHandle,
                _: ManifestKind,
                _: u32,
            ) -> Result<String, SdkError> {
                self.alive(h)?;
                Ok(PLAYLIST.into())
            }
            async fn read_segment(
                &self,
                h: &PlaybackHandle,
                e: u32,
                _: u32,
                n: u32,
                offset: u64,
                length: u64,
            ) -> Result<Chunk, SdkError> {
                self.alive(h)?;
                if self.hold_read.swap(false, SeqCst) {
                    self.entered.notify_one();
                    self.release.notified().await;
                }
                let size = self.segment_len.lock().unwrap_or(1);
                let end = offset.saturating_add(length).min(size);
                Ok(Chunk {
                    bytes: vec![n as u8; end.saturating_sub(offset) as usize],
                    size,
                    complete: true,
                    epoch: e,
                })
            }
        }
        /// A screen in an SDR mode, 8 bits.
        fn sdr_display() -> spoolframe_types::DisplayCapabilities {
            spoolframe_types::DisplayCapabilities {
                bit_depth: Some(8),
                hdr: Some(std::collections::BTreeSet::new()),
                video_codecs: None,
            }
        }

        async fn opened(server: &Arc<Server>) -> ManagedPlayback {
            let selection = Selection {
                audio: Some(1),
                subtitle: Some(2),
            };
            let options = PlaybackOptions {
                start: Some(Duration::from_secs(5)),
                bitmap: BitmapSubtitles::Text,
                display: Some(sdr_display()),
                ..PlaybackOptions::default()
            };
            ManagedPlayback::open(server.clone(), item(), Some(selection), options)
                .await
                .unwrap()
        }

        #[tokio::test]
        async fn a_resume_starts_the_same_tracks_at_the_reported_position() {
            let s = Arc::new(Server::default());
            let p = opened(&s).await;
            p.report_position(Duration::from_secs(600)).unwrap();
            p.flush().await.unwrap();
            s.expire();
            assert_eq!(p.read(0, 1).await, Ok(vec![42]));
            let (selection, options) = s.starts.lock()[1].clone();
            assert_eq!(selection, s.starts.lock()[0].0);
            assert_eq!(options.bitmap, BitmapSubtitles::Text);
            assert_eq!(options.start, Some(Duration::from_secs(600)));
            // And for the screen it started for (PGPU-06).
            assert_eq!(options.display, Some(sdr_display()));
        }

        /// The new playback knows where the viewer is, for Continue
        /// watching, before the player reports again.
        #[tokio::test]
        async fn a_resume_reports_the_last_position_on_the_new_handle() {
            let s = Arc::new(Server::default());
            let p = opened(&s).await;
            p.report_position(Duration::from_secs(600)).unwrap();
            p.flush().await.unwrap();
            s.expire();
            p.read(0, 1).await.unwrap();
            p.flush().await.unwrap();
            assert_eq!(s.reports.lock().last(), Some(&("2".to_owned(), 600)));
        }

        #[tokio::test]
        async fn a_progress_report_on_an_expired_playback_resumes_it() {
            let s = Arc::new(Server::default());
            let p = opened(&s).await;
            s.expire();
            p.report_position(Duration::from_secs(70)).unwrap();
            p.flush().await.unwrap();
            assert_eq!(p.snapshot().handle.0, "2");
            assert_eq!(s.last_start(), Some(Duration::from_secs(70)));
            assert_eq!(*s.reports.lock(), vec![("2".to_owned(), 70)]);
        }

        /// The Apple TV asked the dead handle 9 times and froze. A
        /// session that ended is not resumed, and not asked again.
        #[tokio::test]
        async fn a_signed_out_session_is_not_resumed_or_retried() {
            let s = Arc::new(Server::default());
            let p = opened(&s).await;
            s.expire();
            *s.refuse.lock() = Some(SdkError::SignInRequired);
            assert_eq!(p.read(0, 1).await, Err(SdkError::SignInRequired));
            assert_eq!(p.read(0, 1).await, Err(SdkError::SignInRequired));
            assert_eq!(s.asked.load(SeqCst), 2);
        }

        /// The item's library was taken away (B-10): the start is
        /// `NotFound`, and that is the end of it.
        #[tokio::test]
        async fn an_item_no_longer_served_is_not_resumed_or_retried() {
            let s = Arc::new(Server::default());
            let p = opened(&s).await;
            s.expire();
            *s.refuse.lock() = Some(SdkError::NotFound("item".into()));
            assert_eq!(p.read(0, 1).await, Err(SdkError::NotFound("item".into())));
            assert_eq!(p.read(0, 1).await, Err(SdkError::NotFound("item".into())));
            assert_eq!(s.asked.load(SeqCst), 2);
        }

        #[tokio::test]
        async fn a_playback_the_client_closed_is_not_resumed() {
            let s = Arc::new(Server::default());
            let p = opened(&s).await;
            p.close().await.unwrap();
            assert_eq!(p.read(0, 1).await, Err(SdkError::PlaybackClosed));
            assert_eq!(s.started(), 1);
        }

        #[tokio::test(start_paused = true)]
        async fn resumes_are_bounded_per_window() {
            let s = Arc::new(Server::default());
            let p = opened(&s).await;
            for _ in 0..ManagedPlayback::MAX_RESUMES {
                s.expire();
                assert_eq!(p.read(0, 1).await, Ok(vec![42]));
            }
            s.expire();
            assert_eq!(p.read(0, 1).await, Err(SdkError::PlaybackExpired));
            assert_eq!(s.started(), 1 + ManagedPlayback::MAX_RESUMES);
            tokio::time::advance(ManagedPlayback::RESUME_WINDOW).await;
            assert_eq!(p.read(0, 1).await, Ok(vec![42]));
        }

        /// One budget: the app's restarts and the SDK's resumes count
        /// together, and two minutes of play make it whole again.
        #[tokio::test(start_paused = true)]
        async fn the_restart_budget_is_shared_and_refilled_by_play() {
            let s = Arc::new(Server::default());
            let p = opened(&s).await;
            p.restart(RestartCause::TransportLost).await.unwrap();
            for _ in 1..ManagedPlayback::MAX_RESUMES {
                s.expire();
                assert_eq!(p.read(0, 1).await, Ok(vec![42]));
            }
            assert_eq!(
                p.restart(RestartCause::Expired).await,
                Err(SdkError::PlaybackExpired)
            );
            p.set_state(PlayerState::Playing {
                pos: Duration::from_secs(10),
            })
            .unwrap();
            tokio::time::sleep(ManagedPlayback::REFILL_AFTER + Duration::from_secs(10)).await;
            assert!(p.restart(RestartCause::Expired).await.is_ok());
        }

        /// The item's library was taken away under a restart: the playback
        /// says why it is over.
        #[tokio::test]
        async fn a_restart_refused_for_good_says_why() {
            let s = Arc::new(Server::default());
            let p = opened(&s).await;
            *s.refuse.lock() = Some(SdkError::NotFound("item".into()));
            assert!(p.restart(RestartCause::Expired).await.is_err());
            assert_eq!(p.stopped(), Some(PlaybackStopped::Gone));
        }

        /// The progressive output of a new encode begins where it was
        /// started: its bytes do not follow the old reader's.
        #[tokio::test]
        async fn a_progressive_reader_is_told_where_its_playback_resumed() {
            let s = Arc::new(Server::default());
            *s.delivery.lock() = Some(Delivery::Progressive);
            let p = opened(&s).await;
            let source = p.snapshot();
            p.report_position(Duration::from_secs(300)).unwrap();
            p.flush().await.unwrap();
            s.expire();
            let resumed = SdkError::PlaybackResumed {
                at: Duration::from_secs(300),
            };
            assert_eq!(p.read_chunk(&source, 4096, 1).await, Err(resumed.clone()));
            assert_eq!(s.last_start(), Some(Duration::from_secs(300)));
            // Another reader of the old handle is told the same.
            assert_eq!(p.read_chunk(&source, 0, 1).await, Err(resumed));
            assert!(p.read_chunk(&p.snapshot(), 0, 1).await.is_ok());
        }

        /// HLS segments are numbered on the item's own time, whatever the
        /// encode started from: the next segment is read from the new
        /// playback with no seam, which begins at that segment.
        #[tokio::test]
        async fn an_hls_segment_read_goes_on_from_the_new_playback() {
            let s = Arc::new(Server::default());
            *s.delivery.lock() = Some(Delivery::Hls);
            let p = opened(&s).await;
            p.manifest(ManifestKind::Media, 0).await.unwrap();
            p.read_segment(0, 1, 0, 1).await.unwrap();
            s.expire();
            assert_eq!(p.read_segment(0, 4, 0, 1).await.unwrap().bytes, vec![4]);
            assert_eq!(p.snapshot().handle.0, "2");
            // Segment 4 starts at 18 s.
            assert_eq!(s.last_start(), Some(Duration::from_secs(18)));
            s.expire();
            assert_eq!(p.manifest(ManifestKind::Media, 0).await.unwrap(), PLAYLIST);
        }

        /// A new encode's segment need not be the same bytes as the old
        /// one's: the rest of a segment is not spliced on.
        #[tokio::test]
        async fn an_hls_segment_read_part_way_is_read_again_from_its_start() {
            let s = Arc::new(Server::default());
            *s.delivery.lock() = Some(Delivery::Hls);
            let p = opened(&s).await;
            p.manifest(ManifestKind::Media, 0).await.unwrap();
            s.expire();
            assert_eq!(
                p.read_segment(0, 2, 100, 1).await,
                Err(SdkError::PlaybackResumed {
                    at: Duration::from_secs(6)
                })
            );
            assert_eq!(p.read_segment(0, 2, 0, 1).await.unwrap().bytes, vec![2]);
        }

        /// A segment streamed as it arrives (PX-46) ends when the playback
        /// is resumed under it: the rest would come from another encode.
        /// Opened again from 0, it comes from the new one, whole.
        #[tokio::test]
        async fn a_streamed_hls_segment_ends_when_the_playback_resumes_under_it() {
            let s = Arc::new(Server::default());
            *s.delivery.lock() = Some(Delivery::Hls);
            let p = opened(&s).await;
            p.manifest(ManifestKind::Media, 0).await.unwrap();
            let mut stream = p.segment_stream(0, 2, 0, 1).await.unwrap();
            assert_eq!((stream.sent(), stream.size()), (1, 1));
            assert!(stream.complete());
            // Another read finds the playback over, and resumes it.
            s.expire();
            assert_eq!(p.read_segment(0, 3, 0, 1).await.unwrap().bytes, vec![3]);
            assert_eq!(
                stream.next().await,
                Err(SdkError::PlaybackResumed {
                    at: Duration::from_secs(6)
                })
            );
            let mut again = p.segment_stream(0, 2, 0, 1).await.unwrap();
            assert_eq!(again.next().await, Ok(Some(vec![2])));
            assert_eq!(again.next().await, Ok(None));
            assert_eq!(again.received(), 1);
            // The rest of a segment begun in the old encode is refused, as
            // a read of it is.
            s.expire();
            p.read_segment(0, 4, 0, 1).await.unwrap();
            assert_eq!(
                p.segment_stream(0, 2, 1, 1).await.err(),
                Some(SdkError::PlaybackResumed {
                    at: Duration::from_secs(6)
                })
            );
        }

        /// The iOS build 42 review, (a): a segment's answer still on the
        /// link when another read resumed the playback is the old encode's,
        /// though it arrives after the resume. It ends as the resume ends a
        /// stream, and its rest is not read from the new encode.
        #[tokio::test]
        async fn a_segment_opened_across_a_resume_belongs_to_the_old_encode() {
            let s = Arc::new(Server::default());
            *s.delivery.lock() = Some(Delivery::Hls);
            let p = opened(&s).await;
            p.manifest(ManifestKind::Media, 0).await.unwrap();
            s.hold_read.store(true, SeqCst);
            let q = p.clone();
            let open = tokio::spawn(async move { q.segment_stream(0, 2, 0, 1).await });
            s.entered.notified().await;
            s.expire();
            p.read_segment(0, 3, 0, 1).await.unwrap();
            assert_eq!(p.snapshot().handle.0, "2");
            s.release.notify_one();
            let mut stream = open.await.unwrap().unwrap();
            let resumed = SdkError::PlaybackResumed {
                at: Duration::from_secs(6),
            };
            assert_eq!(stream.next().await, Err(resumed.clone()));
            assert_eq!(p.segment_stream(0, 2, 1, 1).await.err(), Some(resumed));
        }

        /// The iOS build 42 review, (b): two readers of one segment, one
        /// opened before a resume and one after. The older one reading on
        /// is refused, though the newer one's open came last; the newer one
        /// reads on.
        #[tokio::test]
        async fn an_older_reader_of_a_segment_does_not_read_on_from_a_newer_encode() {
            let s = Arc::new(Server::default());
            *s.delivery.lock() = Some(Delivery::Hls);
            *s.segment_len.lock() = Some(8);
            let p = opened(&s).await;
            p.manifest(ManifestKind::Media, 0).await.unwrap();
            let mut old = p.segment_stream(0, 2, 0, 4).await.unwrap();
            assert_eq!(old.next().await, Ok(Some(vec![2; 4])));
            s.expire();
            p.read_segment(0, 3, 0, 8).await.unwrap();
            let mut new = p.segment_stream(0, 2, 0, 4).await.unwrap();
            assert_eq!(new.next().await, Ok(Some(vec![2; 4])));
            assert_eq!(
                old.reopen(4).await.err(),
                Some(SdkError::PlaybackResumed {
                    at: Duration::from_secs(6)
                })
            );
            let mut rest = new.reopen(4).await.unwrap();
            assert_eq!(rest.next().await, Ok(Some(vec![2; 4])));
        }

        /// B1: a re-plan for a slow link replaces the encode under the same
        /// handle, and the server serves segments by number from the new
        /// one. A reader of a segment begun before it does not read its
        /// rest from the new encode: it is told the playback restarted.
        #[tokio::test]
        async fn an_older_reader_of_a_segment_does_not_read_on_after_a_replan() {
            let s = Arc::new(Server::default());
            *s.delivery.lock() = Some(Delivery::Hls);
            *s.segment_len.lock() = Some(8);
            *s.replan_epoch.lock() = Some(7);
            let p = opened(&s).await;
            p.manifest(ManifestKind::Media, 0).await.unwrap();
            let mut old = p.segment_stream(0, 2, 0, 4).await.unwrap();
            assert_eq!(old.next().await, Ok(Some(vec![2; 4])));
            // Half of segment 3 read in the old encode, as a range read.
            p.read_segment(0, 3, 0, 4).await.unwrap();

            let replanned = p.limit(1_000).await.unwrap();
            assert_eq!(replanned.handle, p.snapshot().handle);
            assert_eq!(replanned.epoch, 7);
            let restarted = SdkError::PlaybackRestarted { epoch: 7 };

            let mut new = p.segment_stream(0, 2, 0, 4).await.unwrap();
            assert_eq!(new.next().await, Ok(Some(vec![2; 4])));
            assert_eq!(old.reopen(4).await.err(), Some(restarted.clone()));
            assert_eq!(old.next().await, Err(restarted.clone()));
            let mut rest = new.reopen(4).await.unwrap();
            assert_eq!(rest.next().await, Ok(Some(vec![2; 4])));

            // The rest of segment 3 by offset is refused too; read from 0
            // in the new encode, it reads on.
            assert_eq!(p.read_segment(0, 3, 4, 4).await, Err(restarted));
            p.read_segment(0, 3, 0, 4).await.unwrap();
            assert_eq!(p.read_segment(0, 3, 4, 4).await.unwrap().bytes, vec![3; 4]);
        }

        /// A link measured at 1000 bytes/s, far under what 100 kB per
        /// 6 s segment needs.
        fn slow_link() -> Arc<crate::LinkMeter> {
            let meter = Arc::new(crate::LinkMeter::default());
            let now = std::time::Instant::now();
            meter.begin(now - Duration::from_secs(3));
            meter.received(now, 3_000);
            meter.end(now);
            meter
        }

        /// The iOS build 42 review, P1: the bridge (hyper) stops reading a
        /// segment's body once it has the length it promised the player,
        /// and never reads the end of it. A segment read to its last byte
        /// counts as whole for the step-down then, and only once.
        #[tokio::test]
        async fn a_streamed_segment_read_to_its_length_counts_once_for_the_step_down() {
            let s = Arc::new(Server::default());
            *s.delivery.lock() = Some(Delivery::Hls);
            *s.segment_len.lock() = Some(100_000);
            *s.meter.lock() = Some(slow_link());
            let p = opened(&s).await;
            p.manifest(ManifestKind::Media, 0).await.unwrap();
            // Read to its end, and the end read too: one segment, not two.
            let mut one = p.segment_stream(0, 1, 0, 100_000).await.unwrap();
            while one.next().await.unwrap().is_some() {}
            drop(one);
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert_eq!(*s.limits.lock(), Vec::<u64>::new());
            // Read to its length and dropped, as the bridge does.
            let mut two = p.segment_stream(0, 2, 0, 100_000).await.unwrap();
            let piece = two.next().await.unwrap().unwrap();
            assert_eq!(piece.len() as u64, two.size());
            drop(two);
            until(|| !s.limits.lock().is_empty()).await;
        }

        /// A link measured at 1000 bytes/s cannot carry 100 bytes in
        /// 10 ms: a stall over 3 s asks once to be planned for it.
        #[tokio::test]
        async fn a_long_stall_on_a_slow_link_plans_once_for_the_link() {
            let s = Arc::new(Server::default());
            *s.meter.lock() = Some(slow_link());
            *s.replan_epoch.lock() = Some(1);
            let p = opened(&s).await;
            let length = Duration::from_millis(10);
            assert_eq!(p.stalled(Duration::from_secs(2), length).await, Ok(None));
            assert!(s.limits.lock().is_empty());
            p.stalled(Duration::from_secs(4), length).await.unwrap();
            p.stalled(Duration::from_secs(9), length).await.unwrap();
            assert_eq!(*s.limits.lock(), [8_000]);
        }

        /// A link that carries the file leaves a stall to the player.
        #[tokio::test]
        async fn a_stall_on_a_link_fast_enough_changes_nothing() {
            let s = Arc::new(Server::default());
            *s.meter.lock() = Some(slow_link());
            let p = opened(&s).await;
            let stalled = p
                .stalled(Duration::from_secs(4), Duration::from_secs(60))
                .await;
            assert_eq!(stalled, Ok(None));
            assert!(s.limits.lock().is_empty());
        }

        /// Other audio comes only with a new playback: started where the
        /// viewer is with the new tracks, the old one stopped, and no
        /// restart budget spent.
        #[tokio::test]
        async fn other_audio_starts_a_new_playback_with_it() {
            let s = Arc::new(Server::default());
            let p = opened(&s).await;
            p.report_position(Duration::from_secs(300)).unwrap();
            let picked = Selection {
                audio: Some(0),
                subtitle: None,
            };
            // Back and forth, past the restart budget.
            for n in 0..=ManagedPlayback::MAX_RESUMES {
                let audio = Some(n as u32 % 2);
                let pick = Selection { audio, ..picked };
                let next = p.change_tracks(pick, true).await.unwrap();
                assert_eq!(next.handle, PlaybackHandle(s.started().to_string()));
            }
            let (selection, options) = s.starts.lock()[1].clone();
            assert_eq!(selection, Some(picked));
            assert_eq!(options.start, Some(Duration::from_secs(300)));
            until(|| !s.live.lock().contains("1")).await;
            assert_eq!(s.overrides.lock().len(), 1 + ManagedPlayback::MAX_RESUMES);
        }

        /// The same audio with another subtitle is a change on the
        /// playback, not a new one.
        #[tokio::test]
        async fn a_new_subtitle_with_the_same_audio_changes_the_playback() {
            let s = Arc::new(Server::default());
            let p = opened(&s).await;
            let picked = Selection {
                audio: Some(1),
                subtitle: Some(0),
            };
            // The test server refuses subtitle changes as busy.
            assert_eq!(p.change_tracks(picked, false).await, Err(SdkError::Busy));
            assert_eq!(s.started(), 1);
            assert!(s.overrides.lock().is_empty());
        }

        /// A different plan on the new start is not swapped in.
        #[tokio::test]
        async fn a_resume_that_plans_differently_is_refused_and_stopped() {
            let s = Arc::new(Server::default());
            let p = opened(&s).await;
            s.expire();
            *s.delivery.lock() = Some(Delivery::Hls);
            assert_eq!(p.read(0, 1).await, Err(SdkError::PlaybackChanged));
            until(|| s.live.lock().is_empty()).await;
            assert_eq!(p.read(0, 1).await, Err(SdkError::PlaybackChanged));
            assert_eq!(s.started(), 2);
        }

        #[tokio::test]
        async fn finishing_an_expired_playback_still_saves_the_final_position() {
            let s = Arc::new(Server::default());
            let p = opened(&s).await;
            s.expire();
            p.finish(Duration::from_secs(900)).await.unwrap();
            assert_eq!(*s.reports.lock(), vec![("2".to_owned(), 900)]);
            assert!(s.live.lock().is_empty());
        }
    }
}
