//! Drives the app's player through a playback's life: the SDK keeps
//! where it is (open, restarting for a seek, failed), the app's
//! [`PlaybackDriver`] only does what it is told.
//!
//! A converted stream cannot seek: each seek restarts the encode there,
//! and the player is given a new reader from its start. Seeks while
//! scrubbing (within [`SEEK_GATHER`]) gather into one restart, and a
//! paused player stays paused. A re-plan, or a playback the SDK started
//! again after the server ended it, gets a new reader at the same place.
//! A direct play resumed before its length is known prerolls paused and
//! seeks once it is.
//!
//! Given a [`MarkerTracker`], the controller follows the item's markers
//! too: the app feeds it positions ([`PlaybackController::on_position`])
//! and gets back what to show, and an intro skipped by itself is sought
//! by the controller, only where a seek has landed.
//!
//! While a seek is in flight the player still says where it was: the
//! markers and the progress ([`PlaybackController::set_state`]) go by the
//! seek's target instead, until a new position lands within
//! [`SEEK_LANDED`] of it or the app says it is done
//! ([`PlaybackController::seek_done`]). A seek that never lands there (cut
//! short at the file's end, snapped to a keyframe further off, refused)
//! counts no more once the player has played on [`SEEK_LANDED`] from
//! where it did land, or after [`SEEK_GIVE_UP`].
//! The controller's own seeks count; one the player makes by itself (its
//! own scrubber) is told with [`PlaybackController::seek_started`]. The
//! gate is the playback's ([`ManagedPlayback::seek_started`]), so an app
//! with no controller (the FFI's) reports through the same one.
//!
//! Everything here stays on the app's thread: the driver need not be
//! `Send`, and the app gives a [`LocalSpawn`] for the controller's own
//! work, as the media reader takes its `Spawn`.
//! Timers run on the playback's runtime, so any executor can wait on
//! them.

use crate::managed::seek_gate::SeekGate;
use crate::markers::{MarkerAction, MarkerTracker};
use crate::{ManagedPlayback, Playback, PlayerState, SdkError};
use futures_util::future::LocalBoxFuture;
use spoolframe_protocol::PlaybackPlan;
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::Duration;

/// How long seeks gather into one restart of a converted stream.
pub const SEEK_GATHER: Duration = Duration::from_millis(250);

pub use crate::managed::seek_gate::{SEEK_GIVE_UP, SEEK_LANDED};

/// Runs a future on the app's own thread (a GTK main loop's
/// `spawn_future_local`, a `LocalSet`).
pub type LocalSpawn = Rc<dyn Fn(LocalBoxFuture<'static, ()>)>;

/// The app's player, as the SDK drives it.
pub trait PlaybackDriver {
    /// Play `playback` with a new reader made from it, from its start.
    /// `resume`: where to seek once the length is known (direct play;
    /// a converted encode begins where it was asked).
    fn load(&self, playback: Playback, resume: Option<Duration>);
    fn play(&self);
    fn pause(&self);
    /// A seek in the file (direct play).
    fn seek(&self, to: Duration);
    /// Stop the player, done once it reads no more of its reader.
    fn stop_and_wait(&self) -> LocalBoxFuture<'static, ()>;
    /// The playback failed: say so.
    fn failed(&self, error: SdkError);
}

/// Where the playback is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// The player has a reader. `resume`: where to seek once the length
    /// is known.
    Open {
        resume: Option<Duration>,
    },
    /// A converted restart: the server is asked, once `sent`, for a new
    /// encode at `to`. Seeks while scrubbing gather into the one not yet
    /// sent.
    Restarting {
        to: Duration,
        sent: bool,
    },
    Failed,
}

/// Owns the stages of a playback the app's player plays
/// ([`PlaybackDriver`]). One per item; drop it to let go.
pub struct PlaybackController(Rc<Controller>);

struct Controller {
    playback: ManagedPlayback,
    driver: Rc<dyn PlaybackDriver>,
    spawn: LocalSpawn,
    stage: Cell<Stage>,
    /// One more for each restart begun: a slow one does not replace a
    /// newer.
    restarts: Cell<u64>,
    /// The viewer paused (or never played): a new reader stays paused.
    held: Cell<bool>,
    /// The playback's generation the player's reader is of
    /// ([`ManagedPlayback::generation`]).
    loaded: Cell<u64>,
    core: Core,
}

/// What a played item shares, from a server or a file on this device
/// ([`PlaybackController`], [`LocalController`]): its markers and its
/// length. Where the player is comes through a seek gate: the playback's
/// ([`ManagedPlayback::seek_started`]), or a local file's own.
#[derive(Default)]
struct Core {
    /// The item's length, once the player knows it.
    length: Cell<Option<Duration>>,
    markers: RefCell<Option<MarkerTracker>>,
}

impl Core {
    /// The player is at `at` (through the gate), `settled` or not: what to
    /// show of the markers. An intro skipped by itself is sought with
    /// `seek`, only where settled.
    fn on_position(
        &self,
        at: Duration,
        settled: bool,
        seek: impl Fn(Duration),
    ) -> Vec<MarkerAction> {
        let actions = match self.markers.borrow_mut().as_mut() {
            Some(tracker) => tracker.on_position(at, self.length.get(), settled),
            None => Vec::new(),
        };
        for action in &actions {
            if let MarkerAction::AutoSkip { to } = action {
                seek(*to);
            }
        }
        actions
    }

    /// Undo the intro skipped by itself in the last few seconds: back
    /// where it was, with `seek`. False when there is none to undo.
    fn undo_skip(&self, seek: impl Fn(Duration)) -> bool {
        let back = self.markers.borrow_mut().as_mut().and_then(|t| t.undo());
        back.inspect(|at| seek(*at)).is_some()
    }

    fn set_has_next(&self, has_next: bool) {
        if let Some(tracker) = self.markers.borrow_mut().as_mut() {
            tracker.markers_mut().set_has_next(has_next);
        }
    }

    fn set_auto_skip(&self, auto_skip: bool) {
        if let Some(tracker) = self.markers.borrow_mut().as_mut() {
            tracker.set_auto_skip(auto_skip);
        }
    }
}

fn converted(plan: &PlaybackPlan) -> bool {
    matches!(plan, PlaybackPlan::Converted { .. })
}

impl PlaybackController {
    /// Give the player `playback`. `resume`: where the viewer was; a
    /// direct play prerolls paused and seeks there once its length is
    /// known ([`Self::length_known`]). `autoplay`: play once ready, else
    /// wait paused for [`Self::play`].
    pub fn new(
        playback: ManagedPlayback,
        driver: Rc<dyn PlaybackDriver>,
        spawn: LocalSpawn,
        resume: Option<Duration>,
        autoplay: bool,
    ) -> Self {
        let (p, loaded) = playback.snapshot_with_generation();
        let resume = resume.filter(|_| !converted(&p.plan));
        let this = Rc::new(Controller {
            playback,
            driver,
            spawn,
            stage: Cell::new(Stage::Open { resume }),
            restarts: Cell::new(0),
            held: Cell::new(!autoplay),
            loaded: Cell::new(loaded),
            core: Core::default(),
        });
        // Until the player is there, what it says (a paused tick at 0
        // before the length is known) is the resume's position, not 0.
        if let Some(at) = resume {
            this.playback.seek_started(at);
        }
        this.driver.load(p, resume);
        if resume.is_some() || !autoplay {
            this.driver.pause();
        } else {
            this.driver.play();
        }
        follow_changes(&this);
        Self(this)
    }

    pub fn play(&self) {
        self.0.held.set(false);
        self.0.driver.play();
    }

    pub fn pause(&self) {
        self.0.held.set(true);
        self.0.driver.pause();
    }

    /// The player knows the length now, `length`: a direct play waiting
    /// to resume seeks there, and plays unless paused.
    pub fn length_known(&self, length: Duration) {
        self.0.core.length.set(Some(length));
        let Stage::Open { resume: Some(at) } = self.0.stage.get() else {
            return;
        };
        self.0.stage.set(Stage::Open { resume: None });
        self.0.playback.seek_started(at);
        self.0.driver.seek(at);
        if !self.0.held.get() {
            self.0.driver.play();
        }
    }

    /// Follow the item's markers with `tracker` (made from the
    /// playback's `markers`).
    pub fn with_markers(self, tracker: MarkerTracker) -> Self {
        *self.0.core.markers.borrow_mut() = Some(tracker);
        self
    }

    /// The player is at `at`: what to show of the markers, each only when
    /// it changes. An intro skipped by itself is sought here, and only
    /// where the playback has settled (no restart under way, no resume
    /// waiting for the length). Nothing without markers.
    pub fn on_position(&self, at: Duration) -> Vec<MarkerAction> {
        let (at, settled) = self.0.position(at);
        self.0.core.on_position(at, settled, |to| self.0.seek(to))
    }

    /// Undo the intro skipped by itself in the last few seconds: back
    /// where it was. False when there is none to undo.
    pub fn undo_skip(&self) -> bool {
        self.0.core.undo_skip(|at| self.0.seek(at))
    }

    /// Whether something follows the item: the credits' button is Next.
    pub fn set_has_next(&self, has_next: bool) {
        self.0.core.set_has_next(has_next);
    }

    /// Skip intros by themselves, or not.
    pub fn set_auto_skip(&self, auto_skip: bool) {
        self.0.core.set_auto_skip(auto_skip);
    }

    /// Seek to `to`. In a direct play, the player seeks; a converted
    /// stream restarts its encode there, seeks within [`SEEK_GATHER`]
    /// gathered into one restart.
    pub fn seek_coalesced(&self, to: Duration) {
        self.0.seek(to);
    }

    /// The player is in `state`: reported as the playback's progress
    /// ([`ManagedPlayback::set_state`]), at a seek's target while the seek
    /// is in flight.
    pub fn set_state(&self, state: PlayerState) -> Result<(), SdkError> {
        self.0.playback.set_state(state)
    }

    /// The player seeks to `target` by itself (its own scrubber): as
    /// [`ManagedPlayback::seek_started`].
    pub fn seek_started(&self, target: Duration) {
        self.0.playback.seek_started(target);
    }

    /// The player says its seek is done: its positions count again.
    pub fn seek_done(&self) {
        self.0.playback.seek_done();
    }

    /// Whether a converted restart is under way: the player's positions
    /// are the old encode's until it is done.
    pub fn restarting(&self) -> bool {
        matches!(self.0.stage.get(), Stage::Restarting { .. })
    }
}

/// The controller of a file on this device (a download played offline):
/// the markers, the length and the seek gate of a [`PlaybackController`],
/// with no server to restart or report to. The app's player plays the file
/// itself; `seek` is how the controller moves it (an intro skipped by
/// itself, an undo).
pub struct LocalController {
    core: Core,
    /// Its own seek gate: there is no playback to hold one.
    gate: RefCell<SeekGate>,
    seek: Box<dyn Fn(Duration)>,
}

impl LocalController {
    pub fn new(seek: impl Fn(Duration) + 'static) -> Self {
        Self {
            core: Core::default(),
            gate: RefCell::default(),
            seek: Box::new(seek),
        }
    }

    /// Follow the item's markers with `tracker`.
    pub fn with_markers(self, tracker: MarkerTracker) -> Self {
        *self.core.markers.borrow_mut() = Some(tracker);
        self
    }

    /// The player knows the length now: intros are skipped by themselves
    /// from here.
    pub fn length_known(&self, length: Duration) {
        self.core.length.set(Some(length));
    }

    /// The player is at `at`: as [`PlaybackController::on_position`].
    pub fn on_position(&self, at: Duration) -> Vec<MarkerAction> {
        let (at, settled) = self.gate.borrow_mut().position(at);
        self.core.on_position(at, settled, |to| self.seek_to(to))
    }

    /// Where the player is for the app's own progress: a seek's target
    /// while the seek is in flight.
    pub fn position(&self, at: Duration) -> Duration {
        self.gate.borrow_mut().position(at).0
    }

    /// Seek the player to `to`, a seek in flight until it lands.
    pub fn seek_to(&self, to: Duration) {
        self.gate.borrow_mut().started(to);
        (self.seek)(to);
    }

    /// As [`PlaybackController::seek_started`].
    pub fn seek_started(&self, target: Duration) {
        self.gate.borrow_mut().started(target);
    }

    /// As [`PlaybackController::seek_done`].
    pub fn seek_done(&self) {
        self.gate.borrow_mut().done();
    }

    /// As [`PlaybackController::undo_skip`].
    pub fn undo_skip(&self) -> bool {
        self.core.undo_skip(|at| self.seek_to(at))
    }

    pub fn set_has_next(&self, has_next: bool) {
        self.core.set_has_next(has_next);
    }

    pub fn set_auto_skip(&self, auto_skip: bool) {
        self.core.set_auto_skip(auto_skip);
    }
}

impl Controller {
    /// Where the playback is for the markers and the progress, the player
    /// at `at`, and whether that is settled: a converted restart's or an
    /// unlanded seek's target is not.
    fn position(&self, at: Duration) -> (Duration, bool) {
        match self.stage.get() {
            Stage::Restarting { to, .. } => (to, false),
            Stage::Failed => (at, false),
            Stage::Open { resume } => {
                let (at, landed) = self.playback.gated(at);
                (at, landed && resume.is_none())
            }
        }
    }

    fn seek(self: &Rc<Self>, to: Duration) {
        let stage = self.stage.get();
        match stage {
            Stage::Restarting { sent: false, .. } => {
                self.stage.set(Stage::Restarting { to, sent: false });
            }
            Stage::Restarting { .. } | Stage::Open { .. } if self.playback.is_converted() => {
                self.stage.set(Stage::Restarting { to, sent: false });
                let restart = self.restarts.get() + 1;
                self.restarts.set(restart);
                let weak = Rc::downgrade(self);
                let playback = self.playback.clone();
                (self.spawn)(Box::pin(async move {
                    playback.delay(SEEK_GATHER).await;
                    if let Some(this) = weak.upgrade() {
                        this.send_restart(restart).await;
                    }
                }));
            }
            Stage::Open { .. } => {
                self.stage.set(Stage::Open { resume: None });
                self.playback.seek_started(to);
                self.driver.seek(to);
            }
            Stage::Restarting { .. } | Stage::Failed => {}
        }
    }

    /// The gathered restart goes to the server; its answer plays, as the
    /// viewer left it.
    async fn send_restart(self: Rc<Self>, restart: u64) {
        if self.restarts.get() != restart {
            return;
        }
        let Stage::Restarting { to, sent: false } = self.stage.get() else {
            return;
        };
        self.stage.set(Stage::Restarting { to, sent: true });
        self.driver.stop_and_wait().await;
        let answer = self.playback.seek(to).await;
        // A newer restart replaces this one.
        if self.restarts.get() != restart
            || self.stage.get() != (Stage::Restarting { to, sent: true })
        {
            return;
        }
        match answer {
            // The playback as it is now, which a re-plan since the answer
            // may have moved on.
            Ok(_) => {
                let (next, generation) = self.playback.snapshot_with_generation();
                self.load(next, generation, None);
            }
            Err(error) => {
                self.stage.set(Stage::Failed);
                self.driver.failed(error);
            }
        }
    }

    /// The player gets a new reader of `next`, of `generation`, paused or
    /// playing as the viewer left it. The two are read together
    /// ([`ManagedPlayback::snapshot_with_generation`]): a generation read
    /// apart could be a newer re-plan's, and that re-plan then skipped.
    fn load(&self, next: Playback, generation: u64, resume: Option<Duration>) {
        self.loaded.set(generation);
        self.playback.seek_done();
        self.stage.set(Stage::Open { resume });
        self.driver.load(next, resume);
        if self.held.get() || resume.is_some() {
            self.driver.pause();
        } else {
            self.driver.play();
        }
    }
}

/// A re-plan, or a playback started again behind the app, gets a new
/// reader where the viewer is; a restart under way makes its own.
fn follow_changes(this: &Rc<Controller>) {
    let mut changes = this.playback.changes();
    let weak: Weak<Controller> = Rc::downgrade(this);
    (this.spawn)(Box::pin(async move {
        while changes.changed().await.is_ok() {
            changes.borrow_and_update();
            let Some(this) = weak.upgrade() else { return };
            if !matches!(this.stage.get(), Stage::Open { .. }) {
                continue;
            }
            // The same bytes at every offset: the reader reads on. The
            // atomic first, to not copy the playback for nothing.
            if this.loaded.get() == this.playback.generation() {
                continue;
            }
            let (next, generation) = this.playback.snapshot_with_generation();
            if this.loaded.get() == generation {
                continue;
            }
            let at = this.playback.last_position();
            // A converted encode begins where it was started.
            let resume = at.filter(|_| !converted(&next.plan));
            this.load(next, generation, resume);
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Chunk, PlaybackBackend, PlaybackChange, PlaybackOptions};
    use async_trait::async_trait;
    use parking_lot::Mutex;
    use spoolframe_protocol::{BitmapSubtitles, PlaybackHandle, Selection};
    use spoolframe_types::{Container, Delivery, MediaId, MediaKind, Namespace};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::SeqCst};

    /// A server whose seeks each make a new encode.
    #[derive(Default)]
    struct Server {
        converted: AtomicBool,
        expired: AtomicBool,
        epoch: AtomicU32,
        seeks: Mutex<Vec<u64>>,
        starts: AtomicU32,
        /// Each seek takes this long on the link.
        seek_takes: Mutex<Duration>,
    }

    #[async_trait]
    impl PlaybackBackend for Server {
        async fn start(
            &self,
            _: &MediaId,
            _: Option<Selection>,
            _: PlaybackOptions,
        ) -> Result<Playback, SdkError> {
            let n = self.starts.fetch_add(1, SeqCst) + 1;
            let plan = if self.converted.load(SeqCst) {
                PlaybackPlan::Converted {
                    delivery: Delivery::Progressive,
                    container: Container::Matroska,
                    video: None,
                    audio: None,
                    subtitle: None,
                    ladder: Vec::new(),
                    reasons: Vec::new(),
                    source_codec: None,
                }
            } else {
                PlaybackPlan::Direct {
                    container: Container::Matroska,
                    video: None,
                    audio: None,
                    subtitle: None,
                    source_codec: None,
                }
            };
            Ok(Playback {
                handle: PlaybackHandle(n.to_string()),
                plan,
                size: 100,
                chosen: None,
                epoch: 0,
                markers: Vec::new(),
                duration: None,
            })
        }
        async fn read(&self, h: &PlaybackHandle, _: u64, _: u64) -> Result<Vec<u8>, SdkError> {
            if h.0 == "1" && self.expired.load(SeqCst) {
                return Err(SdkError::PlaybackExpired);
            }
            Ok(vec![1])
        }
        async fn read_chunk(
            &self,
            _: &PlaybackHandle,
            e: u32,
            _: u64,
            _: u64,
        ) -> Result<Chunk, SdkError> {
            Ok(Chunk {
                bytes: vec![1],
                size: 1,
                complete: true,
                epoch: e,
            })
        }
        async fn seek(&self, _: &PlaybackHandle, at: Duration) -> Result<u32, SdkError> {
            self.seeks.lock().push(at.as_secs());
            let takes = *self.seek_takes.lock();
            tokio::time::sleep(takes).await;
            Ok(self.epoch.fetch_add(1, SeqCst) + 1)
        }
        /// A new encode with the subtitle, as a converted playback's.
        async fn subtitle(
            &self,
            _: &PlaybackHandle,
            _: Option<u32>,
            _: &crate::Replan,
        ) -> Result<PlaybackChange, SdkError> {
            let p = self
                .start(
                    &MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap(),
                    None,
                    PlaybackOptions::default(),
                )
                .await?;
            Ok(PlaybackChange {
                plan: p.plan,
                size: p.size,
                epoch: self.epoch.fetch_add(1, SeqCst) + 1,
            })
        }
        async fn progress(&self, _: &PlaybackHandle, _: Duration) -> Result<(), SdkError> {
            Ok(())
        }
        async fn stop(&self, _: &PlaybackHandle) -> Result<(), SdkError> {
            Ok(())
        }
        fn link_meter(&self) -> Option<std::sync::Arc<crate::LinkMeter>> {
            None
        }
        fn shares(
            &self,
        ) -> Option<tokio::sync::broadcast::Receiver<crate::managed::PlaybackShare>> {
            None
        }
        async fn set_track_override(
            &self,
            _: &spoolframe_types::MediaId,
            _: spoolframe_protocol::Selection,
        ) -> Result<(), crate::SdkError> {
            Err(crate::SdkError::InvalidInput(
                "no track choices here".into(),
            ))
        }
        async fn read_subtitle(
            &self,
            _: &spoolframe_protocol::PlaybackHandle,
        ) -> Result<crate::Subtitle, crate::SdkError> {
            Err(crate::SdkError::InvalidInput(
                "no subtitle files here".into(),
            ))
        }
        fn subtitle_timings(
            &self,
        ) -> Option<(
            String,
            tokio::sync::broadcast::Receiver<crate::SubtitleTimingEvent>,
        )> {
            None
        }
        fn route(&self) -> Option<crate::RemoteMode> {
            None
        }
    }

    #[derive(Default)]
    struct Player(RefCell<Vec<String>>);

    impl PlaybackDriver for Player {
        fn load(&self, p: Playback, resume: Option<Duration>) {
            let resume = resume.map(|r| r.as_secs());
            self.0
                .borrow_mut()
                .push(format!("load:{}:{}:{resume:?}", p.handle.0, p.epoch));
        }
        fn play(&self) {
            self.0.borrow_mut().push("play".into());
        }
        fn pause(&self) {
            self.0.borrow_mut().push("pause".into());
        }
        fn seek(&self, to: Duration) {
            self.0.borrow_mut().push(format!("seek:{}", to.as_secs()));
        }
        fn stop_and_wait(&self) -> LocalBoxFuture<'static, ()> {
            self.0.borrow_mut().push("stop".into());
            Box::pin(async {})
        }
        fn failed(&self, error: SdkError) {
            self.0.borrow_mut().push(format!("failed:{error}"));
        }
    }

    fn local() -> LocalSpawn {
        Rc::new(|future| {
            tokio::task::spawn_local(future);
        })
    }

    async fn start(server: &Arc<Server>) -> ManagedPlayback {
        let item = MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap();
        ManagedPlayback::open(server.clone(), item, None, PlaybackOptions::default())
            .await
            .unwrap()
    }

    fn took(player: &Player) -> Vec<String> {
        std::mem::take(&mut *player.0.borrow_mut())
    }

    /// Scrubbing a converted stream: three seeks inside 250 ms are one
    /// restart, at the last, and a paused player stays paused.
    #[tokio::test(start_paused = true)]
    async fn seeks_while_scrubbing_gather_into_one_restart() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let server = Arc::new(Server::default());
                server.converted.store(true, SeqCst);
                let player = Rc::new(Player::default());
                let c = PlaybackController::new(
                    start(&server).await,
                    player.clone(),
                    local(),
                    Some(Duration::from_secs(30)),
                    true,
                );
                assert_eq!(took(&player), ["load:1:0:None", "play"]);
                c.pause();
                c.seek_coalesced(Duration::from_secs(100));
                tokio::time::sleep(Duration::from_millis(100)).await;
                c.seek_coalesced(Duration::from_secs(200));
                c.seek_coalesced(Duration::from_secs(300));
                assert!(c.restarting());
                tokio::time::sleep(Duration::from_millis(500)).await;
                assert_eq!(*server.seeks.lock(), [300]);
                assert_eq!(took(&player), ["pause", "stop", "load:1:1:None", "pause"]);
                assert!(!c.restarting());
            })
            .await;
    }

    /// A seek after the last was sent is a restart of its own; the
    /// earlier answer, overtaken, is not played.
    #[tokio::test(start_paused = true)]
    async fn a_seek_after_the_restart_was_sent_restarts_again() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let server = Arc::new(Server::default());
                server.converted.store(true, SeqCst);
                let player = Rc::new(Player::default());
                let c = PlaybackController::new(
                    start(&server).await,
                    player.clone(),
                    local(),
                    None,
                    true,
                );
                took(&player);
                c.seek_coalesced(Duration::from_secs(100));
                tokio::time::sleep(Duration::from_millis(300)).await;
                c.seek_coalesced(Duration::from_secs(200));
                tokio::time::sleep(Duration::from_millis(300)).await;
                assert_eq!(*server.seeks.lock(), [100, 200]);
                let loads: Vec<_> = took(&player)
                    .into_iter()
                    .filter(|e| e.starts_with("load"))
                    .collect();
                assert_eq!(loads.last().map(String::as_str), Some("load:1:2:None"));
            })
            .await;
    }

    /// A re-plan that lands while a restart's seek is answered is what
    /// the player gets: the restart loads the playback as it is then,
    /// not the seek's answer it overtook.
    #[tokio::test(start_paused = true)]
    async fn a_re_plan_landing_with_a_restart_s_answer_is_played() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let server = Arc::new(Server::default());
                server.converted.store(true, SeqCst);
                *server.seek_takes.lock() = Duration::from_millis(100);
                let player = Rc::new(Player::default());
                let playback = start(&server).await;
                let c =
                    PlaybackController::new(playback.clone(), player.clone(), local(), None, true);
                took(&player);
                c.seek_coalesced(Duration::from_secs(100));
                // The restart is sent at 250 ms; its seek is answered at
                // 350 ms, with the subtitle change waiting behind it.
                tokio::time::sleep(Duration::from_millis(300)).await;
                tokio::task::spawn_local(async move {
                    let _ = playback
                        .change_subtitle(Some(2), BitmapSubtitles::Auto, Duration::from_secs(100))
                        .await;
                });
                tokio::time::sleep(Duration::from_millis(300)).await;
                let loads: Vec<_> = took(&player)
                    .into_iter()
                    .filter(|e| e.starts_with("load"))
                    .collect();
                assert_eq!(loads.last().map(String::as_str), Some("load:1:2:None"));
            })
            .await;
    }

    /// Resumed before the length is known, a direct play prerolls
    /// paused, then seeks and plays.
    #[tokio::test(start_paused = true)]
    async fn a_direct_resume_seeks_once_the_length_is_known() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let server = Arc::new(Server::default());
                let player = Rc::new(Player::default());
                let c = PlaybackController::new(
                    start(&server).await,
                    player.clone(),
                    local(),
                    Some(Duration::from_secs(60)),
                    true,
                );
                assert_eq!(took(&player), ["load:1:0:Some(60)", "pause"]);
                c.length_known(Duration::from_secs(5400));
                assert_eq!(took(&player), ["seek:60", "play"]);
                c.length_known(Duration::from_secs(5400));
                // A direct seek goes to the player.
                c.seek_coalesced(Duration::from_secs(90));
                assert_eq!(took(&player), ["seek:90"]);
                assert!(server.seeks.lock().is_empty());
            })
            .await;
    }

    /// A paused tick at 0 while a resume waits for the length is the
    /// player not there yet: the position stays the resume's.
    #[tokio::test(start_paused = true)]
    async fn a_resume_waiting_for_the_length_keeps_its_position() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let server = Arc::new(Server::default());
                let player = Rc::new(Player::default());
                let playback = start(&server).await;
                let _c = PlaybackController::new(
                    playback.clone(),
                    player.clone(),
                    local(),
                    Some(Duration::from_secs(60)),
                    true,
                );
                playback
                    .set_state(crate::PlayerState::Paused {
                        pos: Duration::ZERO,
                    })
                    .unwrap();
                assert_eq!(playback.last_position(), Some(Duration::from_secs(60)));
            })
            .await;
    }

    fn intro() -> MarkerTracker {
        use crate::markers::Markers;
        use spoolframe_types::{Marker, MarkerKind};
        let intro = Marker {
            kind: MarkerKind::Intro,
            start: std::time::Duration::from_millis(60_000),
            end: std::time::Duration::from_millis(90_000),
            to_end: false,
        };
        MarkerTracker::new(Markers::new(vec![intro], false), true)
    }

    /// The controller follows the markers: an intro is skipped by itself
    /// only once the resume has landed, and the skip can be undone.
    #[tokio::test(start_paused = true)]
    async fn an_intro_is_skipped_by_the_controller_once_settled() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let server = Arc::new(Server::default());
                let player = Rc::new(Player::default());
                let c = PlaybackController::new(
                    start(&server).await,
                    player.clone(),
                    local(),
                    Some(Duration::from_secs(65)),
                    true,
                )
                .with_markers(intro());
                took(&player);
                let offered = c.on_position(Duration::from_secs(65));
                assert!(
                    !offered
                        .iter()
                        .any(|a| matches!(a, MarkerAction::AutoSkip { .. })),
                    "not while the resume waits for the length"
                );
                c.length_known(Duration::from_secs(1800));
                took(&player);
                let actions = c.on_position(Duration::from_secs(66));
                assert!(actions.contains(&MarkerAction::AutoSkip {
                    to: Duration::from_secs(90)
                }));
                assert_eq!(took(&player), ["seek:90"]);
                assert!(c.undo_skip());
                assert_eq!(took(&player), ["seek:66"]);
                // Skipped once: back in the intro it is offered, not skipped.
                let again = c.on_position(Duration::from_secs(67));
                assert!(
                    !again
                        .iter()
                        .any(|a| matches!(a, MarkerAction::AutoSkip { .. }))
                );
                assert!(took(&player).is_empty());
            })
            .await;
    }

    /// While a seek is in flight the player still says where it was: the
    /// markers and the progress go by the target until it lands.
    #[tokio::test(start_paused = true)]
    async fn positions_before_a_seek_lands_count_as_its_target() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let server = Arc::new(Server::default());
                let player = Rc::new(Player::default());
                let c = PlaybackController::new(
                    start(&server).await,
                    player.clone(),
                    local(),
                    None,
                    true,
                )
                .with_markers(intro());
                c.length_known(Duration::from_secs(1800));
                // The player's own scrubber, from the intro to 300 s.
                c.seek_started(Duration::from_secs(300));
                assert_eq!(c.on_position(Duration::from_secs(65)), []);
                c.set_state(PlayerState::Playing {
                    pos: Duration::from_secs(65),
                })
                .unwrap();
                assert_eq!(c.0.playback.last_position(), Some(Duration::from_secs(300)));
                // Landed: positions count again, and back in the intro it
                // is skipped.
                assert_eq!(c.on_position(Duration::from_secs(301)), []);
                c.seek_coalesced(Duration::from_secs(65));
                assert!(
                    c.on_position(Duration::from_secs(65))
                        .contains(&MarkerAction::AutoSkip {
                            to: Duration::from_secs(90)
                        })
                );
                // Told it is done, an unlanded seek counts no more.
                c.seek_started(Duration::from_secs(600));
                c.seek_done();
                assert_eq!(
                    c.on_position(Duration::from_secs(70)),
                    [MarkerAction::Offer(Some(crate::MarkerOffer::SkipIntro {
                        to: Duration::from_secs(90)
                    }))]
                );
            })
            .await;
    }

    fn local_at(sought: Rc<RefCell<Vec<u64>>>) -> LocalController {
        LocalController::new(move |to: Duration| sought.borrow_mut().push(to.as_secs()))
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// A seek cut short (the catalog's length is past the file's end)
    /// holds its target only so long: the player's real position counts
    /// again after SEEK_GIVE_UP.
    #[tokio::test(start_paused = true)]
    async fn a_seek_cut_short_counts_no_more_after_a_while() {
        let c = local_at(Rc::default());
        c.on_position(secs(50));
        c.seek_to(secs(6000));
        assert_eq!(c.position(secs(5880)), secs(6000));
        tokio::time::advance(SEEK_GIVE_UP).await;
        assert_eq!(c.position(secs(5880)), secs(5880));
    }

    /// A seek snapped to a keyframe more than SEEK_LANDED off is over once
    /// the player has played on from where it landed.
    #[tokio::test(start_paused = true)]
    async fn a_snapped_seek_is_over_once_the_player_plays_on() {
        let c = local_at(Rc::default());
        c.on_position(secs(10));
        c.seek_to(secs(60));
        assert_eq!(c.position(secs(55)), secs(60));
        assert_eq!(c.position(secs(56)), secs(60));
        assert_eq!(
            c.position(Duration::from_millis(57_500)),
            Duration::from_millis(57_500)
        );
    }

    /// A short seek is not landed by the position the player gave before
    /// it: only a new one lands it.
    #[tokio::test(start_paused = true)]
    async fn a_short_seek_lands_on_a_new_position_only() {
        let c = local_at(Rc::default());
        c.on_position(secs(10));
        c.seek_to(secs(11));
        assert_eq!(c.position(secs(10)), secs(11));
        assert_eq!(
            c.position(Duration::from_millis(11_100)),
            Duration::from_millis(11_100)
        );
    }

    /// A seek that lands where it was asked still counts at once.
    #[tokio::test(start_paused = true)]
    async fn a_seek_landing_where_asked_counts_at_once() {
        let c = local_at(Rc::default());
        c.on_position(secs(10));
        c.seek_to(secs(600));
        assert_eq!(c.position(secs(10)), secs(600));
        assert_eq!(c.position(secs(601)), secs(601));
    }

    /// A file on this device goes through the same markers and seek gate,
    /// with no server: the intro is skipped once the seek there landed.
    #[test]
    fn a_local_file_skips_its_intro_through_the_same_core() {
        let sought = Rc::new(RefCell::new(Vec::new()));
        let log = sought.clone();
        let c = LocalController::new(move |to: Duration| log.borrow_mut().push(to.as_secs()))
            .with_markers(intro());
        c.length_known(Duration::from_secs(1800));
        c.seek_to(Duration::from_secs(62));
        // Still where it was: the target counts, but is not settled, so
        // the intro is offered, not skipped.
        assert_eq!(
            c.on_position(Duration::from_secs(10)),
            [MarkerAction::Offer(Some(crate::MarkerOffer::SkipIntro {
                to: Duration::from_secs(90)
            }))]
        );
        assert_eq!(c.position(Duration::from_secs(10)), Duration::from_secs(62));
        assert!(
            c.on_position(Duration::from_secs(62))
                .contains(&MarkerAction::AutoSkip {
                    to: Duration::from_secs(90)
                })
        );
        assert_eq!(*sought.borrow(), [62, 90]);
        assert!(c.undo_skip());
        assert_eq!(*sought.borrow(), [62, 90, 62]);
    }

    /// The server ended the playback and the SDK started it again: the
    /// player gets a new reader where the viewer was.
    #[tokio::test(start_paused = true)]
    async fn a_playback_started_again_gets_a_new_reader_where_it_was() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let server = Arc::new(Server::default());
                let player = Rc::new(Player::default());
                let playback = start(&server).await;
                let _c =
                    PlaybackController::new(playback.clone(), player.clone(), local(), None, true);
                took(&player);
                playback
                    .set_state(crate::PlayerState::Playing {
                        pos: Duration::from_secs(42),
                    })
                    .unwrap();
                server.expired.store(true, SeqCst);
                playback.read(0, 1).await.unwrap();
                tokio::time::sleep(Duration::from_millis(10)).await;
                assert_eq!(took(&player), ["load:2:0:Some(42)", "pause"]);
            })
            .await;
    }
}
