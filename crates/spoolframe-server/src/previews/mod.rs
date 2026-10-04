//! Frame previews for scrubbing (P3-13): for each film or episode, a
//! picture every `interval` (10 s by default), 320 px wide, packed into
//! sheets of 10 × 10 ([`sheets`]), made in the background
//! ([`make`]) and kept in `state_dir/cache/previews` ([`cache`]).
//!
//! A set is named by its file as it is now (path, size, modification
//! time, as P2-07's subtitle text is) and the interval: a replaced file
//! gets new previews, and the old ones age out of the cache.
//!
//! One file is made at a time, by one worker, at the lowest CPU and I/O
//! priority, and outside the conversion slots (`transcode.max_jobs`), so
//! a playback never waits for previews, and never finds the server busy
//! because of them. A playback, or a client asking, puts its item at the
//! front of the queue; the queue does not preempt the file being made.

pub mod ahead;
pub mod cache;
pub mod make;
pub mod sheets;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use spoolframe_catalog::{Catalog, CatalogError, PreviewFrame};
use spoolframe_media::codecs;
use spoolframe_media::{Prober, VideoRange};
use spoolframe_types::{ArtworkId, MediaId, MediaKind, Namespace};
use tokio::sync::Notify;

use self::cache::Cache;
pub use self::cache::SetMeta;
use crate::transcode::Transcoding;

/// When previews are made (`previews.when`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    /// For every file, ahead of any play, newest and in-progress first,
    /// while the cache has room; and for anything played or asked for.
    Ahead,
    /// For a file when it is first played, or a client asks.
    OnPlay,
    /// Never: clients show the time alone.
    Never,
}

/// `previews { … }`, with the defaults filled in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewSettings {
    pub when: When,
    /// The time between two tiles.
    pub interval: Duration,
    /// The most the cache may hold.
    pub cache_limit_bytes: u64,
}

/// Where an item's previews are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewState {
    /// Made: the set under `key`.
    Ready { key: String, meta: SetMeta },
    /// Being made, or waiting in the queue (`done_percent` 0).
    Making { done_percent: u8 },
    /// It has none, and will not: music, a file without pictures, one
    /// whose previews failed, or `when = never`.
    None,
}

#[derive(Debug, Default)]
struct Queue {
    /// Played or asked for: first.
    urgent: VecDeque<MediaId>,
    /// Made ahead (`when = ahead`): while nothing is urgent.
    ahead: VecDeque<MediaId>,
    queued: HashSet<MediaId>,
    /// Being made now, and how far it has got.
    current: Option<(MediaId, u8)>,
}

impl Queue {
    fn push(&mut self, id: &MediaId, urgent: bool) {
        if self.current.as_ref().is_some_and(|(c, _)| c == id) {
            return;
        }
        if self.queued.contains(id) {
            if urgent && let Some(at) = self.ahead.iter().position(|q| q == id) {
                // Asked for while waiting to be made ahead: now.
                self.ahead.remove(at);
                self.urgent.push_back(id.clone());
            }
            return;
        }
        self.queued.insert(id.clone());
        if urgent {
            self.urgent.push_back(id.clone());
        } else {
            self.ahead.push_back(id.clone());
        }
    }

    /// Queue `ids` to be made ahead, in order, before what was queued
    /// ahead earlier; after anything urgent.
    fn ahead_of_the_rest(&mut self, ids: impl Iterator<Item = MediaId>) {
        let before = std::mem::take(&mut self.ahead);
        for id in &before {
            self.queued.remove(id);
        }
        for id in ids.chain(before) {
            self.push(&id, false);
        }
    }

    fn pop(&mut self) -> Option<(MediaId, bool)> {
        let (id, urgent) = match self.urgent.pop_front() {
            Some(id) => (id, true),
            None => (self.ahead.pop_front()?, false),
        };
        self.queued.remove(&id);
        self.current = Some((id.clone(), 0));
        Some((id, urgent))
    }

    fn progress(&self, id: &MediaId) -> Option<u8> {
        match &self.current {
            Some((current, done)) if current == id => Some(*done),
            _ => self.queued.contains(id).then_some(0),
        }
    }
}

/// What the previews are made from and with.
pub struct PreviewSources {
    pub catalog: Arc<dyn Catalog>,
    pub prober: Arc<Prober>,
    /// The hardware decoders P2-02 found.
    pub transcoding: Arc<Transcoding>,
    pub ffmpeg: PathBuf,
}

pub struct Previews {
    cache: Cache,
    dir: PathBuf,
    settings: Mutex<PreviewSettings>,
    sources: PreviewSources,
    queue: Mutex<Queue>,
    wake: Arc<Notify>,
    /// Sets that could not be made, by key, and why: not tried again
    /// until the file changes or the server restarts.
    failed: Mutex<HashMap<String, String>>,
    /// The server is stopping: the file being made is given up between
    /// two tiles, and nothing more is made.
    stopped: AtomicBool,
}

impl std::fmt::Debug for Previews {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Previews")
            .field("dir", &self.dir)
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl Previews {
    /// Previews kept in `dir`, made by a worker started here.
    pub fn start(
        dir: &Path,
        settings: PreviewSettings,
        sources: PreviewSources,
    ) -> std::io::Result<Arc<Self>> {
        let cache = Cache::open(dir, settings.cache_limit_bytes)?;
        let work = dir.join(WORK);
        if work.exists() {
            std::fs::remove_dir_all(&work)?;
        }
        let previews = Arc::new(Self {
            cache,
            dir: dir.to_owned(),
            settings: Mutex::new(settings),
            sources,
            queue: Mutex::default(),
            wake: Arc::default(),
            failed: Mutex::default(),
            stopped: AtomicBool::new(false),
        });
        tokio::spawn(work_loop(Arc::downgrade(&previews), previews.wake.clone()));
        Ok(previews)
    }

    pub fn settings(&self) -> PreviewSettings {
        self.settings.lock().expect("never poisoned").clone()
    }

    /// Use `settings` from now on. A new interval names new sets; a lower
    /// limit evicts at once; `never` empties the queue.
    pub fn set_settings(&self, settings: PreviewSettings) {
        self.cache.set_limit(settings.cache_limit_bytes);
        let mut queue = self.queue.lock().expect("never poisoned");
        match settings.when {
            When::Never => {
                queue.urgent.clear();
                queue.ahead.clear();
                queue.queued.clear();
            }
            When::OnPlay => {
                for id in std::mem::take(&mut queue.ahead) {
                    queue.queued.remove(&id);
                }
            }
            When::Ahead => {}
        }
        *self.settings.lock().expect("never poisoned") = settings;
    }

    /// Make nothing more: the file being made is given up after its tile
    /// in progress, its work removed. For a stopping server.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        let mut queue = self.lock_queue();
        queue.urgent.clear();
        queue.ahead.clear();
        queue.queued.clear();
        drop(queue);
        self.wake.notify_one();
    }

    /// Bytes in the cache now.
    pub fn cache_total(&self) -> u64 {
        self.cache.total()
    }

    fn lock_queue(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue.lock().expect("never poisoned")
    }

    fn enqueue(&self, id: &MediaId, urgent: bool) {
        self.lock_queue().push(id, urgent);
        self.wake.notify_one();
    }

    /// `id` is being played: make its previews next, unless they are off.
    pub fn played(&self, id: &MediaId) {
        if self.settings().when != When::Never && has_pictures(id) {
            self.enqueue(id, true);
        }
    }

    /// Make previews for `ids`, in order, once nothing played is waiting
    /// (`when = ahead` only). They go ahead of what was queued ahead
    /// before: the newest and in-progress items come first each time.
    pub fn ahead(&self, ids: impl IntoIterator<Item = MediaId>) {
        if self.settings().when != When::Ahead {
            return;
        }
        self.lock_queue()
            .ahead_of_the_rest(ids.into_iter().filter(has_pictures));
        self.wake.notify_one();
    }

    /// [`Previews::stop`] was called.
    pub fn stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// Where `id`'s previews are. Not made yet, they are queued first, as
    /// for a playback: a client asks while it plays.
    pub async fn state(&self, id: &MediaId) -> Result<PreviewState, CatalogError> {
        if !has_pictures(id) {
            return Ok(PreviewState::None);
        }
        let path = self.sources.catalog.media_path(id).await?;
        let settings = self.settings();
        let Ok(key) = key(&path, settings.interval).await else {
            // media_path has just read it: gone since.
            return Ok(PreviewState::None);
        };
        if let Some(meta) = self.cache.get(&key) {
            return Ok(PreviewState::Ready { key, meta });
        }
        if settings.when == When::Never
            || self
                .failed
                .lock()
                .expect("never poisoned")
                .contains_key(&key)
        {
            return Ok(PreviewState::None);
        }
        let mut queue = self.lock_queue();
        queue.push(id, true);
        let done_percent = queue.progress(id).unwrap_or(0);
        drop(queue);
        self.wake.notify_one();
        Ok(PreviewState::Making { done_percent })
    }

    /// Sheet `n` of the set under `key`.
    pub async fn sheet(&self, key: &str, n: u32) -> Option<Vec<u8>> {
        self.cache.sheet(key, n).await
    }

    /// Make the next item's previews, if anything waits. `false` when the
    /// queue is empty.
    async fn work_once(&self) -> bool {
        if self.stopped.load(Ordering::SeqCst) {
            return false;
        }
        let Some((id, urgent)) = self.lock_queue().pop() else {
            return false;
        };
        if let Err(e) = self.make(&id, urgent).await {
            tracing::warn!(%id, error = %e, "cannot make previews");
        }
        self.lock_queue().current = None;
        true
    }

    async fn make(&self, id: &MediaId, urgent: bool) -> Result<(), String> {
        let path = match self.sources.catalog.media_path(id).await {
            Ok(path) => path,
            // Plex or the mount is away: not a failure of the file's, and
            // the next play asks again.
            Err(e) => {
                tracing::debug!(%id, error = %e, "no file to make previews of");
                return Ok(());
            }
        };
        let settings = self.settings();
        if settings.when == When::Never {
            return Ok(());
        }
        let key = key(&path, settings.interval)
            .await
            .map_err(|e| e.to_string())?;
        if self.cache.get(&key).is_some() {
            return Ok(());
        }
        if !urgent && !self.cache.has_room() {
            return Ok(());
        }
        let started = std::time::Instant::now();
        match self.make_set(id, &path, &key, settings.interval).await {
            Ok(meta) => {
                tracing::info!(
                    %id,
                    tiles = meta.count,
                    sheets = meta.sheets,
                    secs = started.elapsed().as_secs_f32(),
                    "made previews"
                );
                Ok(())
            }
            Err(_) if self.stopped.load(Ordering::SeqCst) => Ok(()),
            Err(reason) => {
                self.failed
                    .lock()
                    .expect("never poisoned")
                    .insert(key, reason.clone());
                Err(reason)
            }
        }
    }

    async fn make_set(
        &self,
        id: &MediaId,
        path: &Path,
        key: &str,
        interval: Duration,
    ) -> Result<SetMeta, String> {
        // The catalog's own first: Plex has made them for most files, and
        // reading them decodes nothing and reads nothing of the file.
        let tiles = match self.sources.catalog.preview_frames(id).await {
            Ok(Some(frames)) => {
                tracing::debug!(%id, frames = frames.frames.len(), "previews from the catalog's index");
                pick(&frames.frames, interval)
            }
            Ok(None) => self.decode(id, path, key, interval).await?,
            Err(e) => {
                tracing::info!(%id, error = %e, "cannot read the catalog's previews; making them");
                self.decode(id, path, key, interval).await?
            }
        };
        let packed = tokio::task::spawn_blocking(move || sheets::pack(&tiles))
            .await
            .map_err(|e| e.to_string())??;
        let meta = SetMeta {
            interval_ms: interval.as_millis() as u64,
            tile_width: packed.tile_width,
            tile_height: packed.tile_height,
            columns: sheets::COLUMNS,
            rows: sheets::ROWS,
            count: packed.count,
            sheets: packed.sheets.len() as u32,
        };
        self.cache
            .store(key, meta, &packed.sheets)
            .await
            .map_err(|e| e.to_string())?;
        Ok(meta)
    }

    /// The tiles, decoded from the file with ffmpeg.
    async fn decode(
        &self,
        id: &MediaId,
        path: &Path,
        key: &str,
        interval: Duration,
    ) -> Result<Vec<Vec<u8>>, String> {
        let descriptor = self
            .sources
            .prober
            .describe(path)
            .await
            .map_err(|e| e.to_string())?;
        let video = descriptor.video.as_ref().ok_or("the file has no video")?;
        let duration = descriptor.duration.ok_or("the file's length is unknown")?;
        let codec = codecs::video_codec(&video.codec);
        let encoders = self.sources.transcoding.encoders();
        let hardware = encoders
            .hardware_decoders
            .iter()
            .find(|d| d.codec == codec)
            .map(|d| {
                (
                    d.hardware,
                    self.sources.transcoding.settings().vaapi_device.clone(),
                )
            });
        let source = make::Source {
            input: path.to_owned(),
            stream: video.index,
            duration,
            hdr: video.range() != VideoRange::Sdr,
            hardware,
        };
        let work = self.dir.join(WORK).join(key);
        tokio::fs::create_dir_all(&work)
            .await
            .map_err(|e| e.to_string())?;
        let made = make::tiles(
            &self.sources.ffmpeg,
            &source,
            interval,
            &work,
            &self.stopped,
            &|done, total| {
                if let Some((current, percent)) = &mut self.lock_queue().current
                    && current == id
                {
                    *percent = (done * 100 / total.max(1)).min(99) as u8;
                }
            },
        )
        .await;
        let _ = tokio::fs::remove_dir_all(&work).await;
        made
    }
}

impl Drop for Previews {
    fn drop(&mut self) {
        // The worker holds only a weak reference: wake it to find that
        // the previews are gone, and end.
        self.wake.notify_one();
    }
}

/// Where tiles are made, under the cache; emptied at start.
const WORK: &str = "work";

/// One item after another, for as long as the previews exist.
async fn work_loop(previews: Weak<Previews>, wake: Arc<Notify>) {
    loop {
        let notified = wake.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let Some(previews) = previews.upgrade() else {
            return;
        };
        if previews.stopped.load(Ordering::SeqCst) {
            return;
        }
        let worked = previews.work_once().await;
        drop(previews);
        if !worked {
            notified.await;
        }
    }
}

/// Sheet `n` of the set under `key`, as clients fetch it on the artwork
/// stream: `preview:<key>/<n>`.
pub fn sheet_id(key: &str, n: u32) -> ArtworkId {
    ArtworkId::new(Namespace::Preview, format!("{key}/{n}")).expect("never empty")
}

/// The set and sheet a [`sheet_id`] names. Anything else is `None`: the
/// key becomes a path in the cache, so only a key [`key`] could have made
/// is taken.
pub fn sheet_of(id: &ArtworkId) -> Option<(&str, u32)> {
    if id.namespace() != Namespace::Preview {
        return None;
    }
    let (key, n) = id.key().split_once('/')?;
    let hex = key.len() == KEY_LEN && key.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    hex.then_some(())?;
    Some((key, n.parse().ok()?))
}

/// The set under `key` as a client gets it.
pub fn reply(key: &str, meta: &SetMeta) -> spoolframe_protocol::Previews {
    spoolframe_protocol::Previews {
        interval_ms: meta.interval_ms,
        tile_width: meta.tile_width,
        tile_height: meta.tile_height,
        columns: meta.columns,
        rows: meta.rows,
        count: meta.count,
        sheets: (0..meta.sheets).map(|n| sheet_id(key, n)).collect(),
    }
}

/// How long a set's key is, in hex digits.
const KEY_LEN: usize = 32;

/// A tile every `interval` from a catalog's frames (in order of time):
/// for each, the last frame at or before its time, as a tile made from
/// the file shows the keyframe at or before it. Up to the last frame.
fn pick(frames: &[PreviewFrame], interval: Duration) -> Vec<Vec<u8>> {
    let Some(last) = frames.last() else {
        return Vec::new();
    };
    let interval = interval.as_millis().max(1) as u64;
    (0..=last.at_ms / interval)
        .map(|i| {
            let before = frames.partition_point(|f| f.at_ms <= i * interval);
            frames[before.saturating_sub(1)].jpeg.clone()
        })
        .collect()
}

/// Only films and episodes have pictures to preview.
fn has_pictures(id: &MediaId) -> bool {
    matches!(id.kind(), MediaKind::Movie | MediaKind::Episode)
}

/// A set's name: the file as it is now (path, size, modification time)
/// and the interval.
async fn key(path: &Path, interval: Duration) -> std::io::Result<String> {
    let meta = tokio::fs::metadata(path).await?;
    let modified = meta
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut hash = blake3::Hasher::new();
    for part in [
        path.display().to_string(),
        meta.len().to_string(),
        modified.to_string(),
        interval.as_millis().to_string(),
        sheets::TILE_WIDTH.to_string(),
    ] {
        hash.update(part.as_bytes());
        hash.update(&[0]);
    }
    Ok(hash.finalize().to_hex()[..KEY_LEN].to_owned())
}

#[cfg(test)]
mod tests;
