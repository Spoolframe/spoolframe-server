//! Keyframes read ahead of any play (`experimental.index_ahead`).
//!
//! An HLS playback with its video copied is cut at the film's keyframes
//! (PX-25). Matroska and MP4 list them in an index, read in moments; a
//! transport stream (`.ts`, `.m2ts`) lists none, so its first such play
//! reads every packet of the file first: 155 s for a 58 GB remux on the
//! dev server, past the 135 s an app waits for the playback to start.
//! With the switch on, each such file is read before anyone plays it, and
//! the list is kept on disk ([`spoolframe_media::KeyframeStore`]).
//!
//! The order is previews' made ahead ([`crate::previews::ahead`]): the
//! catalog's newest items every [`NEWEST_EVERY`], and every movie and
//! episode of every library at start, when the switch is turned on, when
//! the catalog changes, and every [`ALL_EVERY`]. The newest go ahead of
//! the rest each time.
//!
//! One file at a time, at the lowest CPU and I/O priority, and started
//! only while no conversion runs and no playback is being prepared
//! ([`Encoding`]). A read is one ffprobe run that cannot be paused: once
//! started it runs to its end, at that priority, even if an encode starts.
//! A play of the file being read waits for that read rather than starting
//! another ([`spoolframe_media::Prober::keyframes_ahead`]).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use spoolframe_catalog::Catalog;
use spoolframe_media::{Ahead, ProbeError, Prober};
use spoolframe_types::{MediaId, MediaKind};
use tokio::sync::{Notify, watch};

use crate::encode::Encoding;
use crate::metrics::Counter;
use crate::previews::ahead::{LibraryEpisodes, candidates};

/// How often the newest items are queued.
pub const NEWEST_EVERY: Duration = Duration::from_secs(10 * 60);
/// How often every library is walked.
pub const ALL_EVERY: Duration = Duration::from_secs(6 * 3600);
/// How often the sweep looks at whether one is due.
const TICK: Duration = Duration::from_secs(30);

/// What the files are read with, and from.
pub struct IndexAheadSources {
    pub catalog: Arc<dyn Catalog>,
    /// A TV library's episodes in pages; `None` walks show by show.
    pub episodes: Option<Arc<dyn LibraryEpisodes>>,
    pub prober: Arc<Prober>,
    /// The conversions running and playbacks being prepared, which a read
    /// waits for before it starts.
    pub encoding: Encoding,
    /// The catalog's changes, when it tells them: every library is walked
    /// again.
    pub changes: Option<watch::Receiver<u64>>,
}

#[derive(Debug, Default)]
struct Queue {
    items: VecDeque<MediaId>,
    queued: HashSet<MediaId>,
}

impl Queue {
    /// `ids` first, in order, then what was queued before.
    fn ahead_of_the_rest(&mut self, ids: impl Iterator<Item = MediaId>) {
        let before = std::mem::take(&mut self.items);
        self.queued.clear();
        for id in ids.chain(before) {
            if self.queued.insert(id.clone()) {
                self.items.push_back(id);
            }
        }
    }

    fn pop(&mut self) -> Option<MediaId> {
        let id = self.items.pop_front()?;
        self.queued.remove(&id);
        Some(id)
    }

    fn clear(&mut self) {
        self.items.clear();
        self.queued.clear();
    }
}

/// The file as it was when it was last looked at: its size and its
/// modification time, ns.
type Seen = (u64, u128);

pub struct IndexAhead {
    sources: IndexAheadSources,
    on: AtomicBool,
    stopped: AtomicBool,
    queue: Mutex<Queue>,
    /// Woken when anything is queued, the switch changes, or the reads
    /// stop.
    wake: Arc<Notify>,
    /// Files looked at already, as they were then: read, kept, indexed,
    /// or failed for a reason of the file's. Not looked at again until
    /// they change or the server restarts.
    seen: Mutex<HashMap<PathBuf, Seen>>,
    scanned: Counter,
    failed: Counter,
    read_bytes: Counter,
    /// ms spent reading.
    read_ms: Counter,
}

/// The reads ahead's work now, and done since the server started.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IndexAheadCounts {
    pub queued: u64,
    pub scanned: u64,
    pub failed: u64,
    pub read_bytes: u64,
    pub read_seconds: f64,
}

impl std::fmt::Debug for IndexAhead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexAhead")
            .field("on", &self.on)
            .finish_non_exhaustive()
    }
}

impl IndexAhead {
    /// Reads ahead, on if `on`, with their sweep and their worker started.
    pub fn start(sources: IndexAheadSources, on: bool) -> Arc<Self> {
        let encoding = sources.encoding.watch();
        let changes = sources.changes.clone();
        let ahead = Arc::new(Self {
            sources,
            on: AtomicBool::new(on),
            stopped: AtomicBool::new(false),
            queue: Mutex::default(),
            wake: Arc::default(),
            seen: Mutex::default(),
            scanned: Counter::default(),
            failed: Counter::default(),
            read_bytes: Counter::default(),
            read_ms: Counter::default(),
        });
        tokio::spawn(sweep_loop(
            Arc::downgrade(&ahead),
            ahead.wake.clone(),
            changes,
        ));
        tokio::spawn(work_loop(
            Arc::downgrade(&ahead),
            ahead.wake.clone(),
            encoding,
        ));
        ahead
    }

    /// Whether files are read ahead.
    pub fn on(&self) -> bool {
        self.on.load(Ordering::SeqCst)
    }

    /// Read ahead from now on, or not: off empties the queue and gives up
    /// the read in progress; on walks every library at once.
    pub fn set_on(&self, on: bool) {
        if self.on.swap(on, Ordering::SeqCst) != on {
            tracing::info!(on, "keyframes read ahead");
        }
        if !on {
            self.queue.lock().clear();
        }
        self.wake.notify_waiters();
    }

    /// Read nothing more, and give up the read in progress: for a
    /// stopping server.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.queue.lock().clear();
        self.wake.notify_waiters();
    }

    fn stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// The work now, and what was done.
    pub fn counts(&self) -> IndexAheadCounts {
        IndexAheadCounts {
            queued: self.queue.lock().items.len() as u64,
            scanned: self.scanned.get(),
            failed: self.failed.get(),
            read_bytes: self.read_bytes.get(),
            read_seconds: self.read_ms.get() as f64 / 1000.0,
        }
    }

    /// Queue `ids`, in order, ahead of what was queued before. Only films
    /// and episodes are read.
    fn queue(&self, ids: Vec<MediaId>) {
        let ids = ids
            .into_iter()
            .filter(|id| matches!(id.kind(), MediaKind::Movie | MediaKind::Episode));
        self.queue.lock().ahead_of_the_rest(ids);
        self.wake.notify_waiters();
    }

    /// Read `id`'s keyframes, if its file needs a scan and was not looked
    /// at as it is now.
    async fn work(&self, id: &MediaId) {
        let path = match self.sources.catalog.media_path(id).await {
            Ok(path) => path,
            // Plex or the mount is away: the next sweep asks again.
            Err(e) => {
                tracing::debug!(%id, error = %e, "no file to read keyframes of");
                return;
            }
        };
        let Ok(now) = crate::files::stamp(&path).await else {
            return;
        };
        if self.seen.lock().get(&path) == Some(&now) {
            return;
        }
        let started = Instant::now();
        let looked = match self.sources.prober.keyframes_ahead(&path).await {
            Ok(Ahead::Scanned { bytes }) => {
                let took = started.elapsed();
                self.scanned.inc();
                self.read_bytes.add(bytes);
                self.read_ms.add(took.as_millis() as u64);
                tracing::info!(%id, bytes, secs = took.as_secs_f32(), "keyframes read ahead");
                true
            }
            Ok(Ahead::Known | Ahead::Indexed) => true,
            Err(e) => {
                self.failed.inc();
                self.read_ms.add(started.elapsed().as_millis() as u64);
                tracing::warn!(%id, error = %e, "cannot read keyframes ahead");
                // A timeout is the moment's (a busy disk at the idle
                // priority), not the file's: the next sweep tries again.
                !matches!(e, ProbeError::TimedOut { .. })
            }
        };
        if looked {
            self.seen.lock().insert(path, now);
        }
    }
}

impl Drop for IndexAhead {
    fn drop(&mut self) {
        // The loops hold only a weak reference: wake them to find that
        // the reads are gone, and end.
        self.wake.notify_waiters();
    }
}

/// Queue items as the sweeps fall due, for as long as the reads exist and
/// are not stopped.
async fn sweep_loop(
    ahead: Weak<IndexAhead>,
    wake: Arc<Notify>,
    mut changes: Option<watch::Receiver<u64>>,
) {
    let mut last_all: Option<Instant> = None;
    let mut last_newest: Option<Instant> = None;
    let mut was_on = false;
    loop {
        let woken = wake.notified();
        tokio::pin!(woken);
        woken.as_mut().enable();
        let Some(now) = ahead.upgrade() else {
            return;
        };
        if now.stopped() {
            return;
        }
        let on = now.on();
        // Turned on: everything, at once.
        if on && !was_on {
            last_all = None;
            last_newest = None;
        }
        was_on = on;
        let everything = last_all.is_none_or(|t| t.elapsed() >= ALL_EVERY);
        let newest = last_newest.is_none_or(|t| t.elapsed() >= NEWEST_EVERY);
        if on && (everything || newest) {
            let ids = candidates(
                now.sources.catalog.as_ref(),
                now.sources.episodes.as_deref(),
                Vec::new(),
                everything,
            )
            .await;
            tracing::debug!(
                items = ids.len(),
                everything,
                "keyframes queued to read ahead"
            );
            // Turned off during the walk: nothing queued.
            if now.on() {
                now.queue(ids);
            }
            last_newest = Some(Instant::now());
            if everything {
                last_all = Some(Instant::now());
            }
        }
        drop(now);
        let changed = async {
            match changes.as_mut() {
                Some(changes) => changes.changed().await.is_ok(),
                None => std::future::pending().await,
            }
        };
        let changed = tokio::select! {
            () = tokio::time::sleep(TICK) => None,
            () = &mut woken => None,
            still = changed => Some(still),
        };
        match changed {
            // Other libraries, or other servers: walk them all.
            Some(true) => last_all = None,
            // The catalog is gone: nothing more to hear of it.
            Some(false) => changes = None,
            None => {}
        }
    }
}

/// Read the queued items one at a time, each started only while nothing
/// converts, for as long as the reads exist.
async fn work_loop(
    ahead: Weak<IndexAhead>,
    wake: Arc<Notify>,
    mut encoding: watch::Receiver<usize>,
) {
    loop {
        let woken = wake.notified();
        tokio::pin!(woken);
        woken.as_mut().enable();
        let Some(now) = ahead.upgrade() else {
            return;
        };
        if now.stopped() {
            return;
        }
        let next = (now.on() && *encoding.borrow_and_update() == 0)
            .then(|| now.queue.lock().pop())
            .flatten();
        let Some(id) = next else {
            drop(now);
            tokio::select! {
                () = &mut woken => {}
                changed = encoding.changed() => if changed.is_err() {
                    return;
                },
            }
            continue;
        };
        // Given up, its ffprobe killed, when the switch is turned off or
        // the server stops; a play waiting on it then reads the file
        // itself.
        let given_up = async {
            loop {
                let woken = wake.notified();
                tokio::pin!(woken);
                woken.as_mut().enable();
                if now.stopped() || !now.on() {
                    return;
                }
                woken.await;
            }
        };
        tokio::select! {
            () = now.work(&id) => {}
            () = given_up => {
                tracing::debug!(%id, "keyframes read ahead given up");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spoolframe_types::Namespace;

    fn id(key: &str) -> MediaId {
        MediaId::new(Namespace::Plex, MediaKind::Movie, key).unwrap()
    }

    fn keys(queue: &Queue) -> Vec<&str> {
        queue.items.iter().map(MediaId::key).collect()
    }

    /// The newest, queued again, go ahead of the rest, each once.
    #[test]
    fn the_newest_go_ahead_of_the_rest_once() {
        let mut queue = Queue::default();
        queue.ahead_of_the_rest([id("a"), id("b"), id("c")].into_iter());
        queue.ahead_of_the_rest([id("c"), id("d")].into_iter());
        assert_eq!(keys(&queue), ["c", "d", "a", "b"]);
        assert_eq!(queue.pop().map(|i| i.key().to_owned()), Some("c".into()));
        assert!(!queue.queued.contains(&id("c")));
    }
}
