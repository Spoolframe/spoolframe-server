//! A paired server's item played through the SDK
//! ([`Session::open_any_playback`]).
//!
//! The playback carries its route: reads, seeks, changes and the stop go
//! to the session that plays it (the paired server's, direct; the
//! user's own, proxied), and its progress to the user's own server, by
//! the `fed:` id, however it plays ([`Session::update_watch_progress`]).
//! A playback started again (a resume, a restart, other audio) picks its
//! route anew, and its progress still goes home.

use super::{ManagedPlayback, PlaybackBackend, PlaybackShare};
use crate::{
    Chunk, ChunkStream, Playback, PlaybackChange, PlaybackOptions, PlaybackQuality, RemoteMode,
    RemotePlayback, SdkError, Session, Subtitle,
};
use async_trait::async_trait;
use parking_lot::Mutex;
use spoolframe_protocol::{ManifestKind, PlaybackHandle, Selection};
use spoolframe_types::{MediaId, Namespace};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

/// The backend of a paired server's item: the user's own session, and
/// the route its playback took last.
struct Routed {
    home: Session,
    /// The item's `fed:` id, which its progress is reported by.
    item: MediaId,
    route: Mutex<Option<RemotePlayback>>,
}

impl Routed {
    /// The session the playback is read and controlled through.
    fn on(&self) -> Session {
        self.route
            .lock()
            .as_ref()
            .map_or_else(|| self.home.clone(), |r| r.session.clone())
    }
}

#[async_trait]
impl PlaybackBackend for Routed {
    async fn start(
        &self,
        item: &MediaId,
        selection: Option<Selection>,
        options: PlaybackOptions,
    ) -> Result<Playback, SdkError> {
        let remote = self
            .home
            .start_remote_playback(item, selection, options)
            .await?;
        let playback = remote.playback.clone();
        // The old route's relays go with it.
        *self.route.lock() = Some(remote);
        Ok(playback)
    }
    async fn read(&self, h: &PlaybackHandle, o: u64, n: u64) -> Result<Vec<u8>, SdkError> {
        PlaybackBackend::read(&self.on(), h, o, n).await
    }
    async fn read_chunk(
        &self,
        h: &PlaybackHandle,
        e: u32,
        o: u64,
        n: u64,
    ) -> Result<Chunk, SdkError> {
        PlaybackBackend::read_chunk(&self.on(), h, e, o, n).await
    }
    async fn seek(&self, h: &PlaybackHandle, p: Duration) -> Result<u32, SdkError> {
        PlaybackBackend::seek(&self.on(), h, p).await
    }
    async fn subtitle(
        &self,
        h: &PlaybackHandle,
        s: Option<u32>,
        replan: &super::Replan,
    ) -> Result<PlaybackChange, SdkError> {
        PlaybackBackend::subtitle(&self.on(), h, s, replan).await
    }
    /// Home, by the `fed:` id: the paired server keeps nothing of it.
    async fn progress(&self, _: &PlaybackHandle, position: Duration) -> Result<(), SdkError> {
        self.home.update_watch_progress(&self.item, position).await
    }
    async fn stop(&self, h: &PlaybackHandle) -> Result<(), SdkError> {
        PlaybackBackend::stop(&self.on(), h).await
    }
    async fn change_quality(
        &self,
        h: &PlaybackHandle,
        q: PlaybackQuality,
        replan: &super::Replan,
    ) -> Result<PlaybackChange, SdkError> {
        PlaybackBackend::change_quality(&self.on(), h, q, replan).await
    }
    async fn limit(
        &self,
        h: &PlaybackHandle,
        l: u64,
        replan: &super::Replan,
    ) -> Result<PlaybackChange, SdkError> {
        PlaybackBackend::limit(&self.on(), h, l, replan).await
    }
    async fn http_playback_url(&self, h: &PlaybackHandle) -> Result<crate::HttpPlayback, SdkError> {
        PlaybackBackend::http_playback_url(&self.on(), h).await
    }
    async fn manifest(
        &self,
        h: &PlaybackHandle,
        k: ManifestKind,
        v: u32,
    ) -> Result<String, SdkError> {
        PlaybackBackend::manifest(&self.on(), h, k, v).await
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
        PlaybackBackend::read_segment(&self.on(), h, e, v, n, o, l).await
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
        PlaybackBackend::open_segment(&self.on(), h, e, v, n, o, l).await
    }
    async fn open_chunk(
        &self,
        h: &PlaybackHandle,
        e: u32,
        o: u64,
        l: u64,
    ) -> Result<ChunkStream, SdkError> {
        PlaybackBackend::open_chunk(&self.on(), h, e, o, l).await
    }
    async fn subtitle_segment(&self, h: &PlaybackHandle, n: u32) -> Result<Subtitle, SdkError> {
        PlaybackBackend::subtitle_segment(&self.on(), h, n).await
    }
    async fn read_subtitle(&self, h: &PlaybackHandle) -> Result<Subtitle, SdkError> {
        PlaybackBackend::read_subtitle(&self.on(), h).await
    }
    /// Kept home: the pick is the user's.
    async fn set_track_override(&self, item: &MediaId, s: Selection) -> Result<(), SdkError> {
        PlaybackBackend::set_track_override(&self.home, item, s).await
    }
    fn link_meter(&self) -> Option<Arc<crate::LinkMeter>> {
        PlaybackBackend::link_meter(&self.on())
    }
    fn shares(&self) -> Option<broadcast::Receiver<PlaybackShare>> {
        PlaybackBackend::shares(&self.home)
    }
    fn route(&self) -> Option<RemoteMode> {
        self.route.lock().as_ref().map(|r| r.mode)
    }
    /// None followed: the playback picks its server once, at start, and a
    /// paired server's events name its own item, not the `fed:` id.
    fn subtitle_timings(
        &self,
    ) -> Option<(String, broadcast::Receiver<crate::SubtitleTimingEvent>)> {
        None
    }
}

impl ManagedPlayback {
    /// How a paired server's item plays now: from that server (direct)
    /// or through the user's own (proxied); `None` for the user's own
    /// server's item.
    pub fn route(&self) -> Option<RemoteMode> {
        self.0.backend.route()
    }

    /// Whether a paired server's item plays through the user's own
    /// server. An HTTP URL for it is the user's server's.
    pub fn proxied(&self) -> bool {
        self.route() == Some(RemoteMode::Proxied)
    }
}

impl Session {
    /// Play any item: a paired server's (`fed:`), with federation on,
    /// from that server or through this one
    /// ([`Session::start_remote_playback`]), its route carried by the
    /// playback ([`ManagedPlayback::route`]); anything else as
    /// [`Session::open_playback`].
    pub async fn open_any_playback(
        &self,
        id: &MediaId,
        selection: Option<Selection>,
        options: PlaybackOptions,
    ) -> Result<ManagedPlayback, SdkError> {
        if id.namespace() != Namespace::Federated || !self.experimental().federation {
            return self.open_playback(id, selection, options).await;
        }
        let routed = Arc::new(Routed {
            home: self.clone(),
            item: id.clone(),
            route: Mutex::new(None),
        });
        ManagedPlayback::open(routed, id.clone(), selection, options).await
    }
}
