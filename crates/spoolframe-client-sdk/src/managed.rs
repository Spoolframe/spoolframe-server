//! Playback ownership independent of any UI or media engine.
//!
//! Clones share a handle. Recovery replaces it for readers, progress and controls
//! together. Call `finish`/`close` before shutting down the runtime: Drop cleanup
//! is best effort and cannot run after runtime shutdown.
use crate::{Chunk, Playback, PlaybackChange, PlaybackOptions, SdkError, Session};
use async_trait::async_trait;
use parking_lot::Mutex;
use spoolframe_protocol::{BitmapSubtitles, PlaybackHandle, PlaybackPlan, Selection};
use spoolframe_types::MediaId;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Notify, broadcast, oneshot, watch};

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
}

struct State {
    playback: Playback,
    selection: Option<Selection>,
    options: PlaybackOptions,
    pending: Option<Duration>,
    finishing: bool,
    closed: bool,
}
struct Inner {
    backend: Arc<dyn PlaybackBackend>,
    item: MediaId,
    state: Mutex<State>,
    control: tokio::sync::Mutex<()>,
    changed: watch::Sender<Playback>,
    errors: broadcast::Sender<SdkError>,
    notify: Arc<Notify>,
    runtime: tokio::runtime::Handle,
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
async fn admitted(
    backend: Arc<dyn PlaybackBackend>,
    item: MediaId,
    selection: Option<Selection>,
    options: PlaybackOptions,
) -> Result<Admission, SdkError> {
    let runtime =
        tokio::runtime::Handle::try_current().map_err(|e| SdkError::InvalidInput(e.to_string()))?;
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

impl ManagedPlayback {
    pub async fn open(
        backend: Arc<dyn PlaybackBackend>,
        item: MediaId,
        selection: Option<Selection>,
        options: PlaybackOptions,
    ) -> Result<Self, SdkError> {
        let mut admission = admitted(backend.clone(), item.clone(), selection, options).await?;
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
                finishing: false,
                closed: false,
            }),
            control: tokio::sync::Mutex::new(()),
            changed,
            errors,
            notify: notify.clone(),
            runtime: tokio::runtime::Handle::current(),
        });
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
        Self(inner)
    }
    pub fn snapshot(&self) -> Playback {
        self.0.state.lock().playback.clone()
    }
    pub fn changes(&self) -> watch::Receiver<Playback> {
        self.0.changed.subscribe()
    }
    /// Background reporting failures; explicit `flush` and `finish` return errors.
    pub fn errors(&self) -> broadcast::Receiver<SdkError> {
        self.0.errors.subscribe()
    }
    pub fn report_position(&self, position: Duration) -> Result<(), SdkError> {
        let mut state = self.0.state.lock();
        if state.finishing || state.closed {
            return Err(SdkError::PlaybackClosed);
        }
        state.pending = Some(position);
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
        self.flush_locked().await
    }
    async fn flush_locked(&self) -> Result<(), SdkError> {
        let report = {
            let mut state = self.0.state.lock();
            state
                .pending
                .take()
                .map(|p| (state.playback.handle.clone(), p))
        };
        if let Some((handle, position)) = report
            && let Err(error) = self.0.backend.progress(&handle, position).await
        {
            let mut state = self.0.state.lock();
            if !state.closed && state.pending.is_none() {
                state.pending = Some(position);
            }
            return Err(error);
        }
        Ok(())
    }
    fn live(&self) -> Result<Playback, SdkError> {
        let state = self.0.state.lock();
        if state.finishing || state.closed {
            Err(SdkError::PlaybackClosed)
        } else {
            Ok(state.playback.clone())
        }
    }
    pub async fn read(&self, offset: u64, length: u64) -> Result<Vec<u8>, SdkError> {
        let playback = self.live()?;
        match self.0.backend.read(&playback.handle, offset, length).await {
            Err(SdkError::PlaybackExpired)
                if matches!(playback.plan, PlaybackPlan::Direct { .. }) =>
            {
                self.recover(&playback.handle, None).await?;
                self.0
                    .backend
                    .read(&self.live()?.handle, offset, length)
                    .await
            }
            answer => answer,
        }
    }
    /// A reader remains pinned to its handle and epoch snapshot. On restart, the media engine must
    /// retire that reader and create a replacement from byte zero.
    pub async fn read_chunk(
        &self,
        source: &Playback,
        offset: u64,
        length: u64,
    ) -> Result<Chunk, SdkError> {
        let p = self.live()?;
        if p.handle != source.handle || p.epoch != source.epoch {
            return Err(SdkError::PlaybackRestarted { epoch: p.epoch });
        }
        self.0
            .backend
            .read_chunk(&p.handle, source.epoch, offset, length)
            .await
    }
    async fn recover(&self, old: &PlaybackHandle, at: Option<Duration>) -> Result<(), SdkError> {
        let _control = self.0.control.lock().await;
        self.recover_locked(old, at).await
    }
    async fn recover_locked(
        &self,
        old: &PlaybackHandle,
        at: Option<Duration>,
    ) -> Result<(), SdkError> {
        let before = self.live()?;
        if &before.handle != old {
            return Ok(());
        }
        let selection = self.0.state.lock().selection;
        let options = PlaybackOptions {
            start: at,
            ..self.0.state.lock().options
        };
        let mut admission = admitted(
            self.0.backend.clone(),
            self.0.item.clone(),
            selection,
            options,
        )
        .await?;
        let next = admission.playback.as_ref().expect("admitted playback");
        if at.is_none()
            && (!matches!(next.plan, PlaybackPlan::Direct { .. }) || next.size != before.size)
        {
            return Err(SdkError::PlaybackChanged);
        }
        let next = admission.playback.take().expect("admitted playback");
        self.0.state.lock().playback = next.clone();
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
        match self.flush_locked().await {
            Ok(()) => {}
            Err(SdkError::PlaybackExpired) => {
                // Pending progress must not prevent recovery of an idle encode.
                self.recover_locked(&before.handle, Some(position)).await?;
                self.flush_locked().await?;
                return Ok(self.snapshot());
            }
            Err(error) => return Err(error),
        }
        match self.0.backend.seek(&before.handle, position).await {
            Ok(epoch) => {
                let mut state = self.0.state.lock();
                state.playback.epoch = epoch;
                let next = state.playback.clone();
                drop(state);
                self.0.changed.send_replace(next);
            }
            Err(SdkError::PlaybackExpired) => {
                self.recover_locked(&before.handle, Some(position)).await?
            }
            Err(error) => return Err(error),
        }
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
        self.flush_locked().await?;
        let p = self.live()?;
        let changed = self
            .0
            .backend
            .subtitle(&p.handle, selected, bitmap, position)
            .await?;
        let mut state = self.0.state.lock();
        state.playback.plan = changed.plan;
        state.playback.size = changed.size;
        state.playback.epoch = changed.epoch;
        state.selection.get_or_insert(Selection::default()).subtitle = selected;
        state.options.bitmap = bitmap;
        if let Some(chosen) = state.playback.chosen.as_mut() {
            chosen.selection.subtitle = selected;
        }
        let next = state.playback.clone();
        drop(state);
        self.0.changed.send_replace(next.clone());
        Ok(next)
    }
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
            }
        }
        let progress = self.flush_locked().await;
        let stop = self.0.backend.stop(&self.snapshot().handle).await;
        if stop.is_ok() {
            let mut state = self.0.state.lock();
            state.closed = true;
            state.pending = None;
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
        ManagedPlayback::open(Arc::new(self.clone()), item.clone(), selection, options).await
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
        entered: Notify,
        release: Notify,
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
            let n = self.starts.fetch_add(1, SeqCst) + 1;
            if self.hold_start.load(SeqCst) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            Ok(playback(n))
        }
        async fn read(&self, h: &PlaybackHandle, _: u64, _: u64) -> Result<Vec<u8>, SdkError> {
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
            _: Option<u32>,
            _: BitmapSubtitles,
            _: Duration,
        ) -> Result<PlaybackChange, SdkError> {
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
        // in a reader of the previous handle, even though the epochs match.
        assert_eq!(
            p.read_chunk(&playback(1), 0, 1).await,
            Err(SdkError::PlaybackRestarted { epoch: 0 })
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
}
