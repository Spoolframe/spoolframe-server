//! Frame previews for scrubbing (P3-13): for each film or episode, a
//! picture every `interval` (10 s by default), 320 px wide, packed into
//! sheets of 10 × 10 ([`sheets`]), made in the background
//! ([`make`]) and kept in `state_dir/cache/previews` ([`cache`]).
//!
//! A set is named by its file as it is now (path, size, modification
//! time, as P2-07's subtitle text is) and the interval: a replaced file
//! gets new previews, and the old ones age out of the cache.
//!
//! Every ffmpeg run is at the lowest CPU and I/O priority, and outside
//! the encode budget (`transcode.max_jobs`), so a playback never waits
//! for previews, and never finds the server busy because of them.
//!
//! Two kinds of work, each with its own workers:
//!
//! - Made ahead (`when = ahead`): up to `previews.workers` files at once
//!   (the physical cores by default, [`workers`]), only while no
//!   conversion runs, an encode or a remux, and no playback is being
//!   prepared (its keyframes read, PX-25): [`Encoding`]. When one starts,
//!   each file being made stops before its next tile and waits, keeping
//!   the tiles it has; when the last one ends, they go on from there.
//! - Played, or asked for by a client: one file at a time, at once,
//!   encodes or not. A file being made ahead that is then played goes on
//!   at once too, where it is.

pub mod ahead;
pub mod cache;
pub mod make;
pub mod sheets;
pub mod workers;

use parking_lot::Mutex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use spoolframe_catalog::{Catalog, CatalogError, PreviewFrame};
use spoolframe_media::codecs;
use spoolframe_media::{Prober, VideoRange};
use spoolframe_types::{ArtworkId, MediaId, MediaKind, Namespace};

use crate::metrics::Counter;
use tokio::sync::{Notify, watch};

use self::cache::Cache;
pub use self::cache::SetMeta;
use crate::encode::Encoding;
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
    /// Files made ahead at once, while no encode runs.
    pub workers: usize,
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

/// Where an item's previews are, looked at without queuing them (P3-15).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewPeek {
    Ready(SetMeta),
    /// Being made, or waiting in the queue (0).
    Making {
        done_percent: u8,
    },
    /// Not made, nor queued: made when it is first played.
    NotMade,
    /// It will have none, and why.
    None(&'static str),
}

/// A file being made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Making {
    /// How far it has got.
    done_percent: u8,
    /// Played or asked for: made though encodes run.
    urgent: bool,
    /// Held back before its next tile while an encode runs.
    paused: bool,
}

/// Items queued as played or asked for at once (PX-09 finding 15).
/// How long a set found still served is not checked again (review
/// 2026-10-11 M16).
const VERIFIED_FOR: std::time::Duration = std::time::Duration::from_secs(60);
/// Sheets kept scaled for a client's size: a few sets' worth.
const RESIZED_KEPT: usize = 32;

/// A sheet scaled for a size: its set's key, its number, the width, and
/// the picture.
type Resized = (String, u32, u32, Arc<Vec<u8>>);

const MAX_URGENT: usize = 16;
/// Played films whose previews are made at once (review 2026-10-11 M17).
const URGENT_SLOTS: usize = 2;

#[derive(Debug, Default)]
struct Queue {
    /// Played or asked for: one at a time, encodes or not.
    urgent: VecDeque<MediaId>,
    /// Made ahead (`when = ahead`): `workers` at a time, while no encode
    /// runs.
    ahead: VecDeque<MediaId>,
    queued: HashSet<MediaId>,
    /// Being made now.
    making: HashMap<MediaId, Making>,
}

impl Queue {
    fn push(&mut self, id: &MediaId, urgent: bool) {
        if let Some(making) = self.making.get_mut(id) {
            // Played while being made ahead: on at once, where it is.
            making.urgent |= urgent;
            return;
        }
        if self.queued.contains(id) {
            if urgent && let Some(at) = self.ahead.iter().position(|q| q == id) {
                // Asked for while waiting to be made ahead: now.
                self.ahead.remove(at);
                self.push_urgent(id);
            }
            return;
        }
        self.queued.insert(id.clone());
        if urgent {
            self.push_urgent(id);
        } else {
            self.ahead.push_back(id.clone());
        }
    }

    /// `id` played or asked for, after the others. Past [`MAX_URGENT`] the
    /// one asked for longest ago is dropped: every `GetPreviews` of
    /// another item queued it, without end (PX-09 finding 15).
    fn push_urgent(&mut self, id: &MediaId) {
        self.urgent.push_back(id.clone());
        while self.urgent.len() > MAX_URGENT {
            if let Some(dropped) = self.urgent.pop_front() {
                self.queued.remove(&dropped);
            }
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

    /// The next file played or asked for, now being made.
    fn pop_urgent(&mut self) -> Option<MediaId> {
        let id = self.urgent.pop_front()?;
        Some(self.start(id, true))
    }

    /// The next file to make ahead, now being made.
    fn pop_ahead(&mut self) -> Option<MediaId> {
        let id = self.ahead.pop_front()?;
        Some(self.start(id, false))
    }

    fn start(&mut self, id: MediaId, urgent: bool) -> MediaId {
        self.queued.remove(&id);
        self.making.insert(
            id.clone(),
            Making {
                done_percent: 0,
                urgent,
                paused: false,
            },
        );
        id
    }

    fn progress(&self, id: &MediaId) -> Option<u8> {
        match self.making.get(id) {
            Some(making) => Some(making.done_percent),
            None => self.queued.contains(id).then_some(0),
        }
    }
}

/// What the previews are made from and with.
pub struct PreviewSources {
    pub catalog: Arc<dyn Catalog>,
    pub prober: Arc<Prober>,
    /// The hardware decoders P2-02 found.
    pub transcoding: Arc<Transcoding>,
    pub ffmpeg: spoolframe_media::sandbox::Tool,
    /// The conversions running and playbacks being prepared, which
    /// previews made ahead wait for.
    pub encoding: Encoding,
}

pub struct Previews {
    cache: Cache,
    dir: PathBuf,
    settings: Mutex<PreviewSettings>,
    sources: PreviewSources,
    /// What makes the tiles: ffmpeg, but in tests.
    maker: Arc<dyn make::MakeTile>,
    queue: Mutex<Queue>,
    /// Woken (all waiters) when anything is queued, a setting changes,
    /// or the previews stop.
    wake: Arc<Notify>,
    /// Sets that could not be made, by key, and why: not tried again
    /// until the file changes or the server restarts.
    failed: Mutex<HashMap<String, String>>,
    /// One making of a set at a time, by key: two items of one file (the
    /// same film in two libraries) would each decode it, and the second
    /// store would remove the set the first serves (review 2026-10-11 L20).
    sets: crate::keyed_locks::KeyedLocks,
    /// The item each set was last shown for, by key (PX-09 finding 25): a
    /// sheet is served only while that item is still in the catalog and
    /// its file still makes the key. Empty at start: a client asks for
    /// the previews again before it fetches their sheets.
    shown: Mutex<HashMap<String, MediaId>>,
    /// Each set last found still served ([`Previews::sheet`]): when, under
    /// which catalog (its change number), and its file. Plex is asked
    /// again only after [`VERIFIED_FOR`] or a catalog change, not per sheet
    /// fetched; the file is looked at each time (review 2026-10-11 M16).
    verified: Mutex<HashMap<String, (std::time::Instant, u64, PathBuf)>>,
    /// The catalog's changes, when it tells them: a change (a library no
    /// longer served) asks Plex again at once.
    changes: Mutex<Option<watch::Receiver<u64>>>,
    /// Sheets made smaller for a size a client asked for, newest last, at
    /// most [`RESIZED_KEPT`]: a scrub fetches each sheet of a set at one
    /// size, again and again (review 2026-10-11 M16).
    resized: Mutex<std::collections::VecDeque<Resized>>,
    /// The server is stopping: the file being made is given up between
    /// two tiles, and nothing more is made.
    stopped: AtomicBool,
    /// Sets made since the server started (P3-14).
    made: Counter,
    /// Sets that could not be made since the server started.
    failures: Counter,
}

/// The previews' work now (P3-14).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreviewCounts {
    /// Waiting to be made.
    pub queued: u64,
    /// Being made now.
    pub making: u64,
    pub done: u64,
    pub failed: u64,
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
    /// Previews kept in `dir`, made by workers started here.
    pub fn start(
        dir: &Path,
        settings: PreviewSettings,
        sources: PreviewSources,
    ) -> std::io::Result<Arc<Self>> {
        let maker = Arc::new(make::Ffmpeg(sources.ffmpeg.clone()));
        let default = workers::default_workers();
        tracing::info!(
            workers = settings.workers,
            default = default.count,
            counted = %default.counted,
            "previews made ahead at once"
        );
        Self::start_with(dir, settings, sources, maker)
    }

    /// [`Previews::start`], making tiles with `maker`.
    fn start_with(
        dir: &Path,
        settings: PreviewSettings,
        sources: PreviewSources,
        maker: Arc<dyn make::MakeTile>,
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
            maker,
            queue: Mutex::default(),
            wake: Arc::default(),
            failed: Mutex::default(),
            sets: Default::default(),
            shown: Mutex::default(),
            verified: Mutex::default(),
            changes: Mutex::default(),
            resized: Mutex::default(),
            stopped: AtomicBool::new(false),
            made: Counter::default(),
            failures: Counter::default(),
        });
        tokio::spawn(urgent_loop(
            Arc::downgrade(&previews),
            previews.wake.clone(),
        ));
        tokio::spawn(ahead_loop(
            Arc::downgrade(&previews),
            previews.wake.clone(),
            previews.sources.encoding.watch(),
        ));
        Ok(previews)
    }

    pub fn settings(&self) -> PreviewSettings {
        self.settings.lock().clone()
    }

    /// Use `settings` from now on. A new interval names new sets; a lower
    /// limit evicts at once; `never` empties the queue; more workers start
    /// at once, and with fewer the files being made are finished.
    pub fn set_settings(&self, settings: PreviewSettings) {
        self.cache.set_limit(settings.cache_limit_bytes);
        let mut queue = self.queue.lock();
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
        drop(queue);
        *self.settings.lock() = settings;
        self.wake.notify_waiters();
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
        self.wake.notify_waiters();
    }

    /// The work now, and what was done (P3-14).
    pub fn counts(&self) -> PreviewCounts {
        let queue = self.lock_queue();
        PreviewCounts {
            queued: (queue.urgent.len() + queue.ahead.len()) as u64,
            making: queue.making.len() as u64,
            done: self.made.get(),
            failed: self.failures.get(),
        }
    }

    /// Bytes in the cache now.
    pub fn cache_total(&self) -> u64 {
        self.cache.total()
    }

    fn lock_queue(&self) -> parking_lot::MutexGuard<'_, Queue> {
        self.queue.lock()
    }

    fn enqueue(&self, id: &MediaId, urgent: bool) {
        self.lock_queue().push(id, urgent);
        self.wake.notify_waiters();
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
        self.wake.notify_waiters();
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
            self.shown.lock().insert(key.clone(), id.clone());
            return Ok(PreviewState::Ready { key, meta });
        }
        if settings.when == When::Never || self.failed.lock().contains_key(&key) {
            return Ok(PreviewState::None);
        }
        let mut queue = self.lock_queue();
        queue.push(id, true);
        let done_percent = queue.progress(id).unwrap_or(0);
        drop(queue);
        self.wake.notify_waiters();
        Ok(PreviewState::Making { done_percent })
    }

    /// Where `id`'s previews are, as playback debug shows it (P3-15):
    /// unlike [`Previews::state`], looking queues nothing.
    pub async fn peek(&self, id: &MediaId) -> Result<PreviewPeek, CatalogError> {
        if !has_pictures(id) {
            return Ok(PreviewPeek::None("only films and episodes have previews"));
        }
        let path = self.sources.catalog.media_path(id).await?;
        let settings = self.settings();
        let Ok(key) = key(&path, settings.interval).await else {
            return Ok(PreviewPeek::None("the file could not be read"));
        };
        if let Some(meta) = self.cache.get(&key) {
            return Ok(PreviewPeek::Ready(meta));
        }
        if settings.when == When::Never {
            return Ok(PreviewPeek::None("previews are off (previews.when)"));
        }
        if self.failed.lock().contains_key(&key) {
            return Ok(PreviewPeek::None("they could not be made of this file"));
        }
        Ok(match self.lock_queue().progress(id) {
            Some(done_percent) => PreviewPeek::Making { done_percent },
            None => PreviewPeek::NotMade,
        })
    }

    /// Sheet `n` of the set under `key`, if its item is still served
    /// (PX-09 finding 25): the catalog still has the item a client was
    /// last told of the set for, and its file still makes `key`. Without
    /// that, a sheet's id was enough to fetch it for as long as the cache
    /// kept it, after its library stopped being served too.
    ///
    /// Found served, a set's item is not asked of Plex again for
    /// [`VERIFIED_FOR`] or until the catalog changes: a scrub fetches its
    /// sheets one after another. Its file is still looked at each time.
    pub async fn sheet(&self, key: &str, n: u32) -> Option<Vec<u8>> {
        let shown = self.shown.lock().get(key).cloned()?;
        let change = self.changes.lock().as_ref().map_or(0, |c| *c.borrow());
        let kept = self
            .verified
            .lock()
            .get(key)
            .filter(|(at, under, _)| at.elapsed() < VERIFIED_FOR && *under == change)
            .map(|(_, _, path)| path.clone());
        let path = match &kept {
            Some(path) => Some(path.clone()),
            None => self.sources.catalog.media_path(&shown).await.ok(),
        };
        let still = match &path {
            Some(path) => self::key(path, self.settings().interval)
                .await
                .is_ok_and(|now| now == key),
            None => false,
        };
        if !still {
            self.shown.lock().remove(key);
            self.verified.lock().remove(key);
            return None;
        }
        if kept.is_none()
            && let Some(path) = path
        {
            self.verified
                .lock()
                .insert(key.to_owned(), (std::time::Instant::now(), change, path));
        }
        self.cache.sheet(key, n).await
    }

    /// Ask the catalog again at once, past [`VERIFIED_FOR`], whenever
    /// `changes` says it changed.
    pub fn watch_catalog(&self, changes: watch::Receiver<u64>) {
        *self.changes.lock() = Some(changes);
    }

    /// Sheet `n` of `key` at most `width` wide: [`Previews::sheet`],
    /// scaled whole. One scaled already is kept and served again.
    pub async fn sheet_at(&self, key: &str, n: u32, width: u32) -> Option<Arc<Vec<u8>>> {
        let bytes = self.sheet(key, n).await?;
        if let Some((_, _, _, kept)) = self
            .resized
            .lock()
            .iter()
            .find(|(k, at, w, _)| k == key && *at == n && *w == width)
        {
            return Some(kept.clone());
        }
        let original = spoolframe_catalog::Image {
            bytes,
            content_type: "image/jpeg".into(),
        };
        let image = tokio::task::spawn_blocking(move || crate::artwork::resize(original, width))
            .await
            .ok()?
            .map_err(|e| tracing::warn!(key, n, error = %e, "a preview sheet cannot be scaled"))
            .ok()?;
        let bytes = Arc::new(image.bytes);
        let mut resized = self.resized.lock();
        resized.retain(|(k, at, w, _)| !(k == key && *at == n && *w == width));
        if resized.len() >= RESIZED_KEPT {
            resized.pop_front();
        }
        resized.push_back((key.to_owned(), n, width, bytes.clone()));
        Some(bytes)
    }

    /// Make `id`'s previews, which [`Queue::pop_urgent`] or
    /// [`Queue::pop_ahead`] marked as being made.
    async fn work(&self, id: &MediaId, urgent: bool) {
        if let Err(e) = self.make(id, urgent).await {
            tracing::warn!(%id, error = %e, "cannot make previews");
        }
        self.lock_queue().making.remove(id);
    }

    /// Whether `id` may make its next tile now, once it may: at once when
    /// it is played or asked for, or no encode runs; otherwise when the
    /// last encode ends, or it is played. `false` when the previews stop.
    async fn go_on(&self, id: &MediaId) -> bool {
        let mut encoding = self.sources.encoding.watch();
        let mut paused = false;
        loop {
            let woken = self.wake.notified();
            tokio::pin!(woken);
            woken.as_mut().enable();
            if self.stopped.load(Ordering::SeqCst) {
                return false;
            }
            let urgent = self
                .lock_queue()
                .making
                .get(id)
                .is_none_or(|making| making.urgent);
            if urgent || *encoding.borrow_and_update() == 0 {
                if paused {
                    self.set_paused(id, false);
                    tracing::debug!(%id, "previews made ahead go on");
                }
                return true;
            }
            if !paused {
                paused = true;
                self.set_paused(id, true);
                tracing::debug!(%id, "previews made ahead wait for the encodes to end");
            }
            tokio::select! {
                _ = encoding.changed() => {}
                () = &mut woken => {}
            }
        }
    }

    fn set_paused(&self, id: &MediaId, paused: bool) {
        if let Some(making) = self.lock_queue().making.get_mut(id) {
            making.paused = paused;
        }
    }

    async fn make(&self, id: &MediaId, urgent: bool) -> Result<(), String> {
        let settings = self.settings();
        if settings.when == When::Never {
            return Ok(());
        }
        // A set made ahead fits or is not made: decided before asking Plex
        // where the file is, which a full cache would otherwise ask for
        // every item in the library each sweep (review 2026-10-11 M13).
        if !urgent && !self.cache.has_room() {
            return Ok(());
        }
        let path = match self.sources.catalog.media_path(id).await {
            Ok(path) => path,
            // Plex or the mount is away: not a failure of the file's, and
            // the next play asks again.
            Err(e) => {
                tracing::debug!(%id, error = %e, "no file to make previews of");
                return Ok(());
            }
        };
        let key = key(&path, settings.interval)
            .await
            .map_err(|e| e.to_string())?;
        let _making = self.sets.lock(&key).await;
        // Whoever held it before may have made the set, or failed to.
        if self.cache.get(&key).is_some() {
            return Ok(());
        }
        if let Some(reason) = self.failed.lock().get(&key) {
            return Err(reason.clone());
        }
        let started = std::time::Instant::now();
        match self.make_set(id, &path, &key, settings.interval).await {
            Ok(meta) => {
                self.made.inc();
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
                self.failures.inc();
                // A timeout or ffmpeg that would not start is the moment's,
                // not the file's: the next play tries again (review
                // 2026-10-11 L21).
                if !make::passing(&reason) {
                    self.failed.lock().insert(key, reason.clone());
                }
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
            Ok(Some(frames)) => match pick(&frames.frames, interval) {
                Ok(tiles) => {
                    tracing::debug!(%id, frames = frames.frames.len(), "previews from the catalog's index");
                    tiles
                }
                Err(e) => {
                    tracing::info!(%id, error = %e, "the catalog's previews are refused; making them");
                    self.decoded(id, path, key, interval).await?
                }
            },
            Ok(None) => self.decoded(id, path, key, interval).await?,
            Err(e) => {
                tracing::info!(%id, error = %e, "cannot read the catalog's previews; making them");
                self.decoded(id, path, key, interval).await?
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

    /// [`Self::decode`]'s tiles, as [`pick`] gives them.
    async fn decoded(
        &self,
        id: &MediaId,
        path: &Path,
        key: &str,
        interval: Duration,
    ) -> Result<Vec<Arc<[u8]>>, String> {
        let tiles = self.decode(id, path, key, interval).await?;
        Ok(tiles.into_iter().map(Arc::from).collect())
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
        // The first GPU, in the order jobs try them, that decoded the
        // codec in a test (PGPU-05), confined to its card.
        let detected = self.sources.transcoding.detected();
        let gpu = detected.gpus.iter().find_map(|gpu| {
            let decoder = gpu
                .encoders()
                .hardware_decoders
                .iter()
                .find(|d| d.codec == codec)?;
            Some((decoder.hardware, gpu))
        });
        let source = make::Source {
            input: path.to_owned(),
            stream: video.index,
            duration,
            hdr: video.range() != VideoRange::Sdr,
            hardware: gpu.map(|(family, gpu)| (family, gpu.vaapi_device())),
            gpu_ffmpeg: gpu.map(|(_, gpu)| gpu.ffmpeg.clone()),
        };
        let work = self.dir.join(WORK).join(key);
        tokio::fs::create_dir_all(&work)
            .await
            .map_err(|e| e.to_string())?;
        let made = make::tiles(
            self.maker.as_ref(),
            &source,
            interval,
            &work,
            || self.go_on(id),
            &|done, total| {
                if let Some(making) = self.lock_queue().making.get_mut(id) {
                    making.done_percent = (done * 100 / total.max(1)).min(99) as u8;
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
        // The workers hold only a weak reference: wake them to find that
        // the previews are gone, and end.
        self.wake.notify_waiters();
    }
}

/// Where tiles are made, under the cache; emptied at start.
const WORK: &str = "work";

/// Played and asked-for items, one after another, for as long as the
/// previews exist.
async fn urgent_loop(previews: Weak<Previews>, wake: Arc<Notify>) {
    // Two at once: one slow film (a NAS that answers a tile in minutes)
    // holds up only itself, not the previews of everything else played
    // after it (review 2026-10-11 M17).
    let mut running = tokio::task::JoinSet::new();
    loop {
        let notified = wake.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let Some(now) = previews.upgrade() else {
            return;
        };
        if now.stopped.load(Ordering::SeqCst) {
            drop(now);
            // Each gives up its film between two tiles and cleans up:
            // dropping the set would abort them mid-file.
            while running.join_next().await.is_some() {}
            return;
        }
        while running.len() < URGENT_SLOTS {
            let Some(id) = now.lock_queue().pop_urgent() else {
                break;
            };
            let previews = now.clone();
            running.spawn(async move { previews.work(&id, true).await });
        }
        drop(now);
        if running.is_empty() {
            notified.await;
        } else {
            tokio::select! {
                _ = running.join_next() => {}
                () = &mut notified => {}
            }
        }
    }
}

/// Items made ahead, up to `workers` at once, started only while no
/// encode runs, for as long as the previews exist.
async fn ahead_loop(
    previews: Weak<Previews>,
    wake: Arc<Notify>,
    mut encoding: watch::Receiver<usize>,
) {
    let mut running = tokio::task::JoinSet::new();
    loop {
        let notified = wake.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let Some(now) = previews.upgrade() else {
            break;
        };
        if now.stopped.load(Ordering::SeqCst) {
            break;
        }
        let workers = now.settings().workers;
        while running.len() < workers && *encoding.borrow_and_update() == 0 {
            let Some(id) = now.lock_queue().pop_ahead() else {
                break;
            };
            let previews = now.clone();
            running.spawn(async move { previews.work(&id, false).await });
        }
        drop(now);
        tokio::select! {
            () = &mut notified => {}
            Some(_) = running.join_next(), if !running.is_empty() => {}
            changed = encoding.changed() => if changed.is_err() {
                break;
            },
        }
    }
    // Stopping: those being made give up before their next tile, and
    // remove their work.
    while running.join_next().await.is_some() {}
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
///
/// The last frame's time sizes the tiles, a JPEG copy each, and comes
/// from the catalog (a PMS's index), so an index past the longest film
/// the decode path makes tiles of is refused (finding 52): no more tiles
/// than [`make::tiles`] would make.
fn pick(frames: &[PreviewFrame], interval: Duration) -> Result<Vec<Arc<[u8]>>, String> {
    let Some(last) = frames.last() else {
        return Ok(Vec::new());
    };
    if u128::from(last.at_ms) > spoolframe_media::MAX_DURATION.as_millis() {
        return Err(format!(
            "the index has a frame at {} hours; previews are made only of files up to {} hours",
            last.at_ms / 3_600_000,
            spoolframe_media::MAX_DURATION.as_secs() / 3600
        ));
    }
    let interval = interval.as_millis().max(1) as u64;
    // Each frame's bytes once, shared by every tile that shows it: a
    // sparse index repeats a frame many times (review 2026-10-11 L23).
    let shared: Vec<Arc<[u8]>> = frames
        .iter()
        .map(|f| Arc::from(f.jpeg.as_slice()))
        .collect();
    Ok((0..=last.at_ms / interval)
        .map(|i| {
            let before = frames.partition_point(|f| f.at_ms <= i * interval);
            shared[before.saturating_sub(1)].clone()
        })
        .collect())
}

/// Only films and episodes have pictures to preview.
fn has_pictures(id: &MediaId) -> bool {
    matches!(id.kind(), MediaKind::Movie | MediaKind::Episode)
}

/// A set's name: the file as it is now (path, size, modification time)
/// and the interval.
async fn key(path: &Path, interval: Duration) -> std::io::Result<String> {
    let (size, modified) = crate::files::stamp(path).await?;
    let hash = crate::files::hash_parts(&[
        path.display().to_string(),
        size.to_string(),
        modified.to_string(),
        interval.as_millis().to_string(),
        sheets::TILE_WIDTH.to_string(),
    ]);
    Ok(hash.to_hex()[..KEY_LEN].to_owned())
}

#[cfg(test)]
mod tests;
