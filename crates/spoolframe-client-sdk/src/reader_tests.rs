//! Independent SDK test transport. No server or platform-client dependencies.
use crate::{
    Chunk, ManagedPlayback, Playback, PlaybackBackend, PlaybackChange, PlaybackOptions, SdkError,
};
use async_trait::async_trait;
use parking_lot::Mutex;
use spoolframe_protocol::{PlaybackHandle, PlaybackPlan, Selection};
use spoolframe_types::{Container, Delivery, MediaId, MediaKind, Namespace};
use std::{
    collections::HashMap,
    future::Future,
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::Duration,
};

pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    })
}
pub fn ready<T>(future: impl Future<Output = T>) -> T {
    runtime().block_on(future)
}
pub fn thread_spawn() -> crate::reader::Spawn {
    Arc::new(|future| {
        runtime().spawn(future);
    })
}
pub fn media_file(name: &str, len: usize) -> (spoolframe_test_dir::TestDir, PathBuf, Vec<u8>) {
    let dir = spoolframe_test_dir::TestDir::new_blocking(name);
    let path = dir.join("media");
    let bytes: Vec<_> = (0..len).map(|i| (i % 251) as u8).collect();
    std::fs::write(&path, &bytes).unwrap();
    (dir, path, bytes)
}
pub fn direct_plan() -> PlaybackPlan {
    PlaybackPlan::Direct {
        container: Container::Matroska,
        video: None,
        audio: None,
        subtitle: None,
        source_codec: None,
    }
}
pub struct TestProfile;
pub struct FakeClient {
    path: PathBuf,
    limit: u64,
    grow: Option<u64>,
    meter: Option<Arc<crate::LinkMeter>>,
}
impl FakeClient {
    pub fn new(path: Option<PathBuf>) -> Self {
        Self {
            path: path.unwrap(),
            limit: u64::MAX,
            grow: None,
            meter: None,
        }
    }
    /// A link measured by `meter`, which `LimitPlayback` plans for: it
    /// becomes a progressive conversion, epoch 1.
    pub fn with_meter(mut self, meter: Arc<crate::LinkMeter>) -> Self {
        self.meter = Some(meter);
        self
    }
    pub fn with_read_limit(mut self, limit: u64) -> Self {
        self.limit = limit;
        self
    }
    pub fn converting(mut self, grow: u64) -> Self {
        self.grow = Some(grow);
        self
    }
    pub fn profile(_name: &str) -> TestProfile {
        TestProfile
    }
    pub fn session(&self, _: &TestProfile) -> Result<Arc<TestBackend>, SdkError> {
        Ok(Arc::new(TestBackend {
            bytes: std::fs::read(&self.path).unwrap(),
            limit: self.limit,
            grow: self.grow,
            meter: self.meter.clone(),
            limits: Mutex::default(),
            fail_next_limit: std::sync::atomic::AtomicBool::new(false),
            state: Mutex::new(TestState::default()),
        }))
    }
}
#[derive(Default)]
struct TestState {
    next: u64,
    handles: HashMap<String, (u32, u64)>,
}
pub struct TestBackend {
    bytes: Vec<u8>,
    limit: u64,
    grow: Option<u64>,
    meter: Option<Arc<crate::LinkMeter>>,
    /// The link rates `limit` was asked for.
    pub limits: Mutex<Vec<u64>>,
    /// The next `limit` fails with [`SdkError::Busy`].
    pub fail_next_limit: std::sync::atomic::AtomicBool,
    state: Mutex<TestState>,
}
impl TestBackend {
    fn playback(&self, handle: PlaybackHandle, epoch: u32) -> Playback {
        Playback {
            handle,
            epoch,
            size: if self.grow.is_some() {
                0
            } else {
                self.bytes.len() as u64
            },
            chosen: None,
            markers: vec![],
            duration: None,
            plan: if self.grow.is_some() {
                PlaybackPlan::Converted {
                    delivery: Delivery::Progressive,
                    container: Container::Matroska,
                    video: None,
                    audio: None,
                    subtitle: None,
                    ladder: Vec::new(),
                    reasons: vec![],
                    source_codec: None,
                }
            } else {
                direct_plan()
            },
        }
    }
    pub async fn start_playback(
        &self,
        item: &MediaId,
        selection: Option<Selection>,
        start: Option<Duration>,
    ) -> Result<Playback, SdkError> {
        self.start(
            item,
            selection,
            PlaybackOptions {
                start,
                ..Default::default()
            },
        )
        .await
    }
    pub async fn seek_playback(
        &self,
        handle: &PlaybackHandle,
        position: Duration,
    ) -> Result<u32, SdkError> {
        self.seek(handle, position).await
    }
    pub fn adopt(self: Arc<Self>, handle: PlaybackHandle, epoch: u32) -> ManagedPlayback {
        let _enter = runtime().enter();
        let p = self.playback(handle, epoch);
        ManagedPlayback::adopt(
            self,
            MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap(),
            None,
            PlaybackOptions::default(),
            p,
        )
    }
}
#[async_trait]
impl PlaybackBackend for TestBackend {
    async fn start(
        &self,
        _: &MediaId,
        _: Option<Selection>,
        _: PlaybackOptions,
    ) -> Result<Playback, SdkError> {
        let mut s = self.state.lock();
        s.next += 1;
        let handle = s.next.to_string();
        s.handles.insert(handle.clone(), (0, 0));
        Ok(self.playback(PlaybackHandle(handle), 0))
    }
    async fn read(&self, h: &PlaybackHandle, o: u64, n: u64) -> Result<Vec<u8>, SdkError> {
        if !self.state.lock().handles.contains_key(&h.0) {
            return Err(SdkError::PlaybackExpired);
        }
        let from = o.min(self.bytes.len() as u64) as usize;
        let to = o
            .saturating_add(n.min(self.limit))
            .min(self.bytes.len() as u64) as usize;
        Ok(self.bytes[from..to].to_vec())
    }
    async fn read_chunk(
        &self,
        h: &PlaybackHandle,
        e: u32,
        o: u64,
        n: u64,
    ) -> Result<Chunk, SdkError> {
        let mut s = self.state.lock();
        let (epoch, written) = s.handles.get_mut(&h.0).ok_or(SdkError::PlaybackExpired)?;
        if *epoch != e {
            return Err(SdkError::PlaybackRestarted { epoch: *epoch });
        }
        *written = written
            .saturating_add(self.grow.unwrap_or(u64::MAX))
            .min(self.bytes.len() as u64);
        let end = o.saturating_add(n).min(*written);
        let from = o.min(end);
        Ok(Chunk {
            bytes: self.bytes[from as usize..end as usize].to_vec(),
            size: *written,
            complete: *written == self.bytes.len() as u64,
            epoch: *epoch,
        })
    }
    async fn seek(&self, h: &PlaybackHandle, _: Duration) -> Result<u32, SdkError> {
        let mut s = self.state.lock();
        let (epoch, written) = s.handles.get_mut(&h.0).ok_or(SdkError::PlaybackExpired)?;
        *epoch += 1;
        *written = 0;
        Ok(*epoch)
    }
    async fn subtitle(
        &self,
        _: &PlaybackHandle,
        _: Option<u32>,
        _: &crate::Replan,
    ) -> Result<PlaybackChange, SdkError> {
        Err(SdkError::InvalidInput(
            "test transport has no subtitles".into(),
        ))
    }
    async fn progress(&self, _: &PlaybackHandle, _: Duration) -> Result<(), SdkError> {
        Ok(())
    }
    async fn stop(&self, h: &PlaybackHandle) -> Result<(), SdkError> {
        self.state.lock().handles.remove(&h.0);
        Ok(())
    }
    fn link_meter(&self) -> Option<Arc<crate::LinkMeter>> {
        self.meter.clone()
    }
    async fn limit(
        &self,
        _: &PlaybackHandle,
        link_bitrate: u64,
        _: &crate::Replan,
    ) -> Result<PlaybackChange, SdkError> {
        self.limits.lock().push(link_bitrate);
        if self
            .fail_next_limit
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(SdkError::Busy);
        }
        Ok(PlaybackChange {
            plan: PlaybackPlan::Converted {
                delivery: Delivery::Progressive,
                container: Container::Matroska,
                video: None,
                audio: None,
                subtitle: None,
                ladder: Vec::new(),
                reasons: vec![],
                source_codec: None,
            },
            size: 0,
            epoch: 1,
        })
    }
    fn shares(&self) -> Option<tokio::sync::broadcast::Receiver<crate::managed::PlaybackShare>> {
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
