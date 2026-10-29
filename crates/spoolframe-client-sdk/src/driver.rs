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
//! Everything here stays on the app's thread: the driver need not be
//! `Send`, and the app gives a [`LocalSpawn`] for the controller's own
//! work, as the media reader takes its `Spawn`.
//! Timers run on the playback's runtime, so any executor can wait on
//! them.

use crate::markers::{MarkerAction, MarkerTracker};
use crate::{ManagedPlayback, Playback, SdkError};
use futures_util::future::LocalBoxFuture;
use spoolframe_protocol::{PlaybackHandle, PlaybackPlan};
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::Duration;

/// How long seeks gather into one restart of a converted stream.
pub const SEEK_GATHER: Duration = Duration::from_millis(250);

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
    /// The handle and epoch the player's reader is of.
    loaded: RefCell<(PlaybackHandle, u32)>,
    /// The item's length, once the player knows it.
    length: Cell<Option<Duration>>,
    markers: RefCell<Option<MarkerTracker>>,
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
        let p = playback.snapshot();
        let resume = resume.filter(|_| !converted(&p.plan));
        let this = Rc::new(Controller {
            playback,
            driver,
            spawn,
            stage: Cell::new(Stage::Open { resume }),
            restarts: Cell::new(0),
            held: Cell::new(!autoplay),
            loaded: RefCell::new((p.handle.clone(), p.epoch)),
            length: Cell::new(None),
            markers: RefCell::new(None),
        });
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
        self.0.length.set(Some(length));
        let Stage::Open { resume: Some(at) } = self.0.stage.get() else {
            return;
        };
        self.0.stage.set(Stage::Open { resume: None });
        self.0.driver.seek(at);
        if !self.0.held.get() {
            self.0.driver.play();
        }
    }

    /// Follow the item's markers with `tracker` (made from the
    /// playback's `markers`).
    pub fn with_markers(self, tracker: MarkerTracker) -> Self {
        *self.0.markers.borrow_mut() = Some(tracker);
        self
    }

    /// The player is at `at`: what to show of the markers, each only when
    /// it changes. An intro skipped by itself is sought here, and only
    /// where the playback has settled (no restart under way, no resume
    /// waiting for the length). Nothing without markers.
    pub fn on_position(&self, at: Duration) -> Vec<MarkerAction> {
        let settled = self.0.stage.get() == (Stage::Open { resume: None });
        let actions = match self.0.markers.borrow_mut().as_mut() {
            Some(tracker) => tracker.on_position(at, self.0.length.get(), settled),
            None => return Vec::new(),
        };
        for action in &actions {
            if let MarkerAction::AutoSkip { to } = action {
                self.0.seek(*to);
            }
        }
        actions
    }

    /// Undo the intro skipped by itself in the last few seconds: back
    /// where it was. False when there is none to undo.
    pub fn undo_skip(&self) -> bool {
        let back = self.0.markers.borrow_mut().as_mut().and_then(|t| t.undo());
        match back {
            Some(at) => {
                self.0.seek(at);
                true
            }
            None => false,
        }
    }

    /// Whether something follows the item: the credits' button is Next.
    pub fn set_has_next(&self, has_next: bool) {
        if let Some(tracker) = self.0.markers.borrow_mut().as_mut() {
            tracker.markers_mut().set_has_next(has_next);
        }
    }

    /// Skip intros by themselves, or not.
    pub fn set_auto_skip(&self, auto_skip: bool) {
        if let Some(tracker) = self.0.markers.borrow_mut().as_mut() {
            tracker.set_auto_skip(auto_skip);
        }
    }

    /// Seek to `to`. In a direct play, the player seeks; a converted
    /// stream restarts its encode there, seeks within [`SEEK_GATHER`]
    /// gathered into one restart.
    pub fn seek_coalesced(&self, to: Duration) {
        self.0.seek(to);
    }

    /// Whether a converted restart is under way: the player's positions
    /// are the old encode's until it is done.
    pub fn restarting(&self) -> bool {
        matches!(self.0.stage.get(), Stage::Restarting { .. })
    }
}

impl Controller {
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
            Ok(next) => self.load(next, None),
            Err(error) => {
                self.stage.set(Stage::Failed);
                self.driver.failed(error);
            }
        }
    }

    /// The player gets a new reader of `next`, paused or playing as the
    /// viewer left it.
    fn load(&self, next: Playback, resume: Option<Duration>) {
        *self.loaded.borrow_mut() = (next.handle.clone(), next.epoch);
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
            let next = changes.borrow_and_update().clone();
            let Some(this) = weak.upgrade() else { return };
            if !matches!(this.stage.get(), Stage::Open { .. }) {
                continue;
            }
            if *this.loaded.borrow() == (next.handle.clone(), next.epoch) {
                continue;
            }
            let at = this.playback.last_position();
            // A converted encode begins where it was started.
            let resume = at.filter(|_| !converted(&next.plan));
            this.load(next, resume);
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Chunk, PlaybackBackend, PlaybackChange, PlaybackOptions};
    use async_trait::async_trait;
    use parking_lot::Mutex;
    use spoolframe_protocol::{BitmapSubtitles, Selection};
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
            Ok(self.epoch.fetch_add(1, SeqCst) + 1)
        }
        async fn subtitle(
            &self,
            _: &PlaybackHandle,
            _: Option<u32>,
            _: BitmapSubtitles,
            _: Duration,
        ) -> Result<PlaybackChange, SdkError> {
            Err(SdkError::Busy)
        }
        async fn progress(&self, _: &PlaybackHandle, _: Duration) -> Result<(), SdkError> {
            Ok(())
        }
        async fn stop(&self, _: &PlaybackHandle) -> Result<(), SdkError> {
            Ok(())
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

    fn intro() -> MarkerTracker {
        use crate::markers::Markers;
        use spoolframe_types::{Marker, MarkerKind};
        let intro = Marker {
            kind: MarkerKind::Intro,
            start_ms: 60_000,
            end_ms: 90_000,
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
