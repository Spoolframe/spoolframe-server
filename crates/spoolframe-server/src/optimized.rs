//! Optimized versions (PX-37): the pool that makes the server's converted
//! copies, keeps them within `optimized.max_size_mb`, and lends them to
//! whoever reads them.
//!
//! One copy is made at a time, in the order they were asked for, but
//! that binge-ahead copies ([`CopyTarget::Ahead`], [`ahead`]) wait behind
//! every copy asked for in a profile: a download's copy is not held up by
//! a guess at what someone watches next. Copies are work for later, and
//! what the encode budget has spare is live playback's first. The copy at the head of the queue starts once the
//! budget has a free place for it ([`Jobs::hold_yielding`]): it tries
//! again when the jobs give a place back ([`Jobs::place_released`]), and
//! at once when a copy is asked for. A live playback the budget would
//! turn away takes the running copy's place: its ffmpeg is killed, its
//! part file removed, and it waits again at the head of the queue (an
//! `ahead` copy at the head of the `ahead` ones), to be made from the
//! start (ffmpeg cannot resume a half-written MKV).
//!
//! A copy is written as `making/<id>.mkv.part` in `optimized.dir`, from a
//! link to the media file there (PX-11), and renamed to `<id>.mkv` once
//! ffmpeg exits cleanly. A server that stops mid-copy leaves the part:
//! the next start removes `making/` and makes the copy again.
//!
//! The copies made and the one being made (its part, or what it is
//! expected to grow to if larger) are kept within the budget: outdated
//! copies go first, whatever the budget, then `ahead` ones, then the least
//! recently used ([`Store::optimized_copies`]). A copy someone is reading
//! ([`Lease`]) is never removed.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::sync::{Notify, watch};

use spoolframe_media::sandbox::Tool;
use spoolframe_media::{MediaDescriptor, Prober};

use crate::config::OptimizedSettings;
use crate::encode::copy_run::{self, Attempt, CopyInput, CopyRun};
use crate::encode::job::{
    JobError, Jobs, Yielding, exit_blames_gpu, jail, launch, out_time_us, signal,
};
use crate::store::optimized::{CopyRow, CopyState, CopyTarget, CopyVideo, NewCopy};
use crate::store::{Store, StoreError};
use crate::transcode::Transcoding;

/// How soon the worker tries again after the store failed it.
pub const RETRY: Duration = Duration::from_secs(10);

/// How long the copy at the head of the queue waits for a place with no
/// word from the jobs. A place given back or a limit changed wakes it
/// sooner; this is for what frees room without either (the encoders
/// detected again, say).
pub const BUSY_FALLBACK: Duration = Duration::from_secs(300);

/// How often the pool drops the binge-ahead copies nobody is watching
/// towards ([`UNPLAYED`]).
pub const AHEAD_CHECK: Duration = Duration::from_secs(600);

/// How long a viewer may leave a show before the binge-ahead copy asked
/// for them, if not yet made, is dropped.
pub const UNPLAYED: Duration = Duration::from_secs(24 * 60 * 60);

/// The directory in `optimized.dir` copies are made in: emptied at start.
pub const MAKING_DIR: &str = "making";

/// How many of ffmpeg's last stderr lines a failure keeps.
const STDERR_LINES: usize = 5;

/// What the pool works with.
#[derive(Debug, Clone)]
pub struct PoolSources {
    pub store: Store,
    pub jobs: Arc<Jobs>,
    /// The encoders and GPUs found, read as each copy starts.
    pub transcoding: Arc<Transcoding>,
    pub prober: Arc<Prober>,
}

/// Why the pool could not start.
#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    #[error("cannot prepare {dir}: {source}")]
    Dir {
        dir: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// How far a copy is.
#[derive(Debug, Clone, PartialEq)]
pub struct CopyProgress {
    pub state: CopyState,
    /// Of the film written, 0 to 1: for a copy being made, once its length
    /// is known; 1 for one made.
    pub fraction: Option<f64>,
    /// How long it has yet to go, at the speed it has gone so far.
    pub eta: Option<Duration>,
}

/// What [`Pool::delete`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deleted {
    /// The copy and its file are gone.
    Now,
    /// It goes once its readers' leases end, or once its ffmpeg, told to
    /// stop, has: nobody can lease it from now on.
    Deferred,
    /// There was no such copy.
    NotFound,
}

/// The copy pool. Dropping it stops it, as [`Pool::stop`] does.
#[derive(Debug)]
pub struct Pool {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    store: Store,
    jobs: Arc<Jobs>,
    transcoding: Arc<Transcoding>,
    prober: Arc<Prober>,
    dir: PathBuf,
    making: PathBuf,
    max_size: u64,
    /// How soon the worker tries again after an error.
    retry: Duration,
    state: Mutex<State>,
    /// A copy was asked for, or a lease ended: the worker looks again.
    wake: Notify,
    stop: watch::Sender<bool>,
}

#[derive(Debug, Default)]
struct State {
    /// The copies waiting.
    queue: Queue,
    /// Something changed that the sweep may act on: a copy asked for (it
    /// may outdate another), or a lease ended. Read and cleared by the
    /// worker.
    changed: bool,
    /// The copies being read, and by how many.
    leases: HashMap<String, usize>,
    /// Copies being removed now: none is lent.
    removing: HashSet<String>,
    /// Copies deleted while lent: removed as their last lease ends.
    doomed: HashSet<String>,
    running: Option<Running>,
    /// Each viewer's binge-ahead copy, by user id.
    binges: HashMap<String, Binge>,
}

/// The copies waiting, each kind in the order they were asked for: those
/// in a profile first, then the `ahead` ones.
#[derive(Debug, Default)]
struct Queue {
    profile: VecDeque<String>,
    ahead: VecDeque<String>,
}

impl Queue {
    fn of(&mut self, ahead: bool) -> &mut VecDeque<String> {
        if ahead {
            &mut self.ahead
        } else {
            &mut self.profile
        }
    }

    /// Copy `id` last of its kind.
    fn push(&mut self, id: String, ahead: bool) {
        self.of(ahead).push_back(id);
    }

    /// Copy `id` first of its kind.
    fn push_front(&mut self, id: String, ahead: bool) {
        self.of(ahead).push_front(id);
    }

    fn iter(&self) -> impl Iterator<Item = &String> {
        self.profile.iter().chain(&self.ahead)
    }

    fn contains(&self, id: &str) -> bool {
        self.iter().any(|q| q == id)
    }

    /// The next to be made.
    fn head(&self) -> Option<&str> {
        self.profile
            .front()
            .or(self.ahead.front())
            .map(String::as_str)
    }

    fn pop_head(&mut self) {
        if self.profile.pop_front().is_none() {
            self.ahead.pop_front();
        }
    }

    fn remove(&mut self, id: &str) {
        self.profile.retain(|q| q != id);
        self.ahead.retain(|q| q != id);
    }
}

/// The binge-ahead copy a viewer asked for last, and when they last
/// played its show.
#[derive(Debug, Clone)]
struct Binge {
    show: String,
    copy: String,
    /// Seconds since the Unix epoch, by the store's clock.
    played_at: i64,
}

/// The copy whose ffmpeg runs.
#[derive(Debug)]
struct Running {
    id: String,
    /// Its ffmpeg, which leads its process group; 0 between runs.
    pid: u32,
    duration_ms: Option<u64>,
    encoded_ms: u64,
    began: Instant,
    /// What it is expected to grow to: the budget keeps room for it.
    reserved: u64,
    /// Told when the copy is deleted while being made.
    deleted: Arc<Notify>,
}

impl Running {
    /// Of the film written, once its length is known, and how long it has
    /// yet to go at the speed it has gone so far.
    fn progress(&self) -> (Option<f64>, Option<Duration>) {
        let fraction = self
            .duration_ms
            .filter(|&ms| ms > 0)
            .map(|ms| (self.encoded_ms as f64 / ms as f64).min(1.0));
        let eta = fraction
            .filter(|&f| f > 0.0)
            .map(|f| self.began.elapsed().mul_f64((1.0 - f) / f));
        (fraction, eta)
    }
}

/// A copy lent to a reader: it is not removed until this is dropped.
#[derive(Debug)]
pub struct Lease {
    inner: Arc<Inner>,
    id: String,
    path: PathBuf,
}

impl Lease {
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The copy's file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let doomed = {
            let mut state = self.inner.state.lock();
            let last = match state.leases.get_mut(&self.id) {
                Some(n) if *n > 1 => {
                    *n -= 1;
                    false
                }
                _ => {
                    state.leases.remove(&self.id);
                    true
                }
            };
            last && state.doomed.remove(&self.id) && claim(&mut state, &self.id)
        };
        if doomed {
            let inner = self.inner.clone();
            let id = self.id.clone();
            match tokio::runtime::Handle::try_current() {
                Ok(runtime) => {
                    runtime.spawn(async move {
                        if let Err(e) = inner.discard(&id).await {
                            tracing::warn!(copy = id, error = %e, "cannot remove a deleted copy");
                        }
                    });
                }
                // No runtime: the server is gone, and the next start's
                // sweep has the row still.
                Err(_) => {
                    self.inner.state.lock().removing.remove(&self.id);
                }
            }
        } else {
            // A copy no longer read may be one to evict.
            self.inner.changed();
        }
    }
}

/// Mark copy `id` as being removed, out of the queue: false, and nothing
/// marked, while it is lent, made or removed already.
fn claim(state: &mut State, id: &str) -> bool {
    if state.leases.contains_key(id)
        || state.removing.contains(id)
        || state.running.as_ref().is_some_and(|r| r.id == id)
    {
        return false;
    }
    state.removing.insert(id.to_owned());
    state.queue.remove(id);
    true
}

impl Pool {
    /// The pool over `settings.dir`: what a stopped server was making is
    /// removed and waits again, the copies waiting are queued oldest
    /// first, and a copy whose file is gone is forgotten.
    pub async fn start(
        sources: PoolSources,
        settings: &OptimizedSettings,
    ) -> Result<Self, PoolError> {
        Self::start_with(sources, settings, RETRY).await
    }

    /// [`Pool::start`], trying again `retry` after an error.
    pub(crate) async fn start_with(
        sources: PoolSources,
        settings: &OptimizedSettings,
        retry: Duration,
    ) -> Result<Self, PoolError> {
        let dir = settings.dir.clone();
        let making = dir.join(MAKING_DIR);
        let prepare = |dir: &Path| {
            let dir = dir.to_path_buf();
            move |source| PoolError::Dir { dir, source }
        };
        crate::files::private_dir(&dir).map_err(prepare(&dir))?;
        match std::fs::remove_dir_all(&making) {
            Ok(()) => {
                tracing::info!(dir = %making.display(), "removed the copies a stopped server was making")
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(prepare(&making)(e)),
        }
        crate::files::private_dir(&making).map_err(prepare(&making))?;
        let store = sources.store;
        let requeued = store.requeue_interrupted_optimized_copies().await?;
        if requeued > 0 {
            tracing::info!(
                copies = requeued,
                "copies a stopped server was making wait again"
            );
        }
        let mut waiting = Vec::new();
        for copy in store.optimized_copies().await? {
            match copy.state {
                CopyState::Ready if !copy_file(&dir, &copy.id).exists() => {
                    tracing::warn!(
                        copy = copy.id,
                        "a copy's file is gone: the copy is forgotten"
                    );
                    store.delete_optimized_copy(&copy.id).await?;
                }
                CopyState::Waiting => waiting.push(copy),
                _ => {}
            }
        }
        waiting.sort_by(|a, b| (a.requested_at, &a.id).cmp(&(b.requested_at, &b.id)));
        let mut queue = Queue::default();
        for copy in waiting {
            queue.push(copy.id, copy.target == CopyTarget::Ahead);
        }
        let inner = Arc::new(Inner {
            store,
            jobs: sources.jobs,
            transcoding: sources.transcoding,
            prober: sources.prober,
            dir,
            making,
            max_size: settings.max_size_bytes,
            retry,
            state: Mutex::new(State {
                queue,
                ..State::default()
            }),
            wake: Notify::new(),
            stop: watch::Sender::new(false),
        });
        // Before the worker starts: it sweeps again only once woken.
        inner.sweep_logged().await;
        tokio::spawn(inner.clone().work());
        tokio::spawn(inner.clone().cancel_unplayed_every(AHEAD_CHECK));
        Ok(Self { inner })
    }

    /// The copy of `copy`'s file for its target: the one there is, or a
    /// new one, queued (PX-38's downloads ask for theirs here). One that
    /// failed is tried again.
    pub async fn request(&self, copy: NewCopy) -> Result<CopyRow, StoreError> {
        let inner = &self.inner;
        let (mut row, new) = inner.store.request_optimized_copy(&copy).await?;
        let again = !new && row.state == CopyState::Failed;
        if again {
            inner.store.optimized_copy_requeued(&row.id).await?;
            row.state = CopyState::Waiting;
            row.failure = None;
        }
        if new || again {
            let mut state = inner.state.lock();
            if !state.queue.contains(&row.id) {
                state
                    .queue
                    .push(row.id.clone(), row.target == CopyTarget::Ahead);
            }
        }
        // A new copy may have outdated the file's old one: evicted too.
        inner.changed();
        Ok(row)
    }

    /// Where copy `id`'s file is once made: to describe it before it is
    /// lent, as a lease counts as a use.
    pub fn file(&self, id: &str) -> PathBuf {
        copy_file(&self.inner.dir, id)
    }

    /// How far copy `id` is; `None` for no such copy.
    pub async fn progress(&self, id: &str) -> Result<Option<CopyProgress>, StoreError> {
        let Some(row) = self.inner.store.optimized_copy(id).await? else {
            return Ok(None);
        };
        let (fraction, eta) = match row.state {
            CopyState::Ready => (Some(1.0), None),
            CopyState::Making => self
                .inner
                .state
                .lock()
                .running
                .as_ref()
                .filter(|r| r.id == id)
                .map_or((None, None), Running::progress),
            CopyState::Waiting | CopyState::Failed => (None, None),
        };
        Ok(Some(CopyProgress {
            state: row.state,
            fraction,
            eta,
        }))
    }

    /// The copy being made now, if one is (at most one is), with how far
    /// it is: its id, the fraction written and how long it has to go, as
    /// [`Pool::progress`] says them.
    pub fn running_progress(&self) -> Option<(String, Option<f64>, Option<Duration>)> {
        let state = self.inner.state.lock();
        let running = state.running.as_ref()?;
        let (fraction, eta) = running.progress();
        Some((running.id.clone(), fraction, eta))
    }

    /// Copy `id` to read, if it is made and not outdated: it stays until
    /// the lease is dropped. Counts as a use, for eviction.
    pub async fn lease(&self, id: &str) -> Result<Option<Lease>, StoreError> {
        {
            let mut state = self.inner.state.lock();
            if state.removing.contains(id) || state.doomed.contains(id) {
                return Ok(None);
            }
            *state.leases.entry(id.to_owned()).or_default() += 1;
        }
        // Held from here: dropped on any way out, which lets it go.
        let lease = Lease {
            inner: self.inner.clone(),
            id: id.to_owned(),
            path: copy_file(&self.inner.dir, id),
        };
        if !self.inner.store.optimized_copy_used_if_ready(id).await? {
            return Ok(None);
        }
        Ok(Some(lease))
    }

    /// Delete copy `id` and its file. One being read goes when its last
    /// lease ends, and one being made once its ffmpeg is stopped
    /// ([`Deleted::Deferred`]).
    pub async fn delete(&self, id: &str) -> Result<Deleted, StoreError> {
        self.inner.delete(id).await
    }

    /// Remove the copies over the budget and the outdated ones now, as
    /// the pool does after each copy: after a profile is edited, say.
    pub async fn sweep(&self) -> Result<(), StoreError> {
        self.inner.sweep().await
    }

    /// Stop making copies, killing the running one's ffmpeg: the server is
    /// stopping. Its row stays `making`, and the next start makes it again.
    pub fn stop(&self) {
        self.inner.stop.send_replace(true);
        if let Some(pid) = self.inner.state.lock().running.as_ref().map(|r| r.pid) {
            signal(pid, libc::SIGKILL);
        }
    }

    /// The copies waiting, the next first: for tests.
    #[cfg(test)]
    fn queued(&self) -> Vec<String> {
        self.inner.state.lock().queue.iter().cloned().collect()
    }

    /// Binge-ahead ([`ahead`]): `copy`, an `ahead` copy of the next
    /// episode of `show`, asked for `viewer` (a user id). It waits behind
    /// every copy in a profile. It is the viewer's one: the one asked for
    /// them before is dropped if it has not started, unless another viewer
    /// asked for it too; one being made or made stays.
    pub async fn request_ahead(
        &self,
        viewer: &str,
        show: &str,
        copy: NewCopy,
    ) -> Result<CopyRow, StoreError> {
        let row = self.request(copy).await?;
        let replaced = {
            let mut state = self.inner.state.lock();
            let before = state.binges.insert(
                viewer.to_owned(),
                Binge {
                    show: show.to_owned(),
                    copy: row.id.clone(),
                    played_at: self.inner.store.now(),
                },
            );
            before.map(|b| b.copy).filter(|old| {
                *old != row.id
                    && state.queue.contains(old)
                    && !state.binges.values().any(|b| b.copy == *old)
            })
        };
        if let Some(old) = replaced {
            tracing::info!(copy = old, "a binge-ahead copy is replaced by a newer one");
            self.delete(&old).await?;
        }
        Ok(row)
    }

    /// `viewer` plays `show` now: the binge-ahead copy asked for them in it
    /// stays wanted for another [`UNPLAYED`].
    pub fn played(&self, viewer: &str, show: &str) {
        let now = self.inner.store.now();
        if let Some(binge) = self.inner.state.lock().binges.get_mut(viewer)
            && binge.show == show
        {
            binge.played_at = now;
        }
    }

    /// Drop the binge-ahead copies, not yet made, that no viewer who asked
    /// for them has played the show of for [`UNPLAYED`]: one waiting is
    /// removed, one being made is stopped and removed. One asked for
    /// before the server started, which no viewer holds now, counts from
    /// when it was asked for. A made one stays, first to be evicted. The
    /// pool does this every [`AHEAD_CHECK`]; how many it dropped.
    pub async fn cancel_unplayed(&self) -> Result<usize, StoreError> {
        self.inner.cancel_unplayed().await
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Where copy `id`'s file is, once made.
fn copy_file(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.mkv"))
}

/// Where copy `id` is written while it is made.
fn part_file(dir: &Path, id: &str) -> PathBuf {
    dir.join(MAKING_DIR).join(format!("{id}.mkv.part"))
}

/// What the worker did with the copy at the head of the queue.
enum Tried {
    /// Made it, failed it, or found it gone: the next one's turn.
    Went,
    /// No place in the budget for it: it waits.
    Busy,
}

/// How one ffmpeg run of a copy ended.
enum Ended {
    Exited {
        status: std::io::Result<std::process::ExitStatus>,
        /// ffmpeg's last stderr lines.
        last: VecDeque<String>,
    },
    /// It could not be run.
    Spawn(String),
    /// Live playback took its place.
    Yielded,
    /// The copy was deleted.
    Deleted,
    /// The server is stopping.
    Stopped,
}

impl Inner {
    async fn work(self: Arc<Self>) {
        let mut stop = self.stop.subscribe();
        // The head's descriptor, kept while it waits for a place.
        let mut described = None;
        while !*stop.borrow() {
            let head = self.state.lock().queue.head().map(str::to_owned);
            // A copy made, failed or gone may let another go.
            let mut went = false;
            match head {
                None => self.idle(&mut stop, None).await,
                Some(id) => match self.try_make(&id, &mut described, &mut stop).await {
                    Ok(Tried::Went) => went = true,
                    Ok(Tried::Busy) => self.wait_for_place(&mut stop).await,
                    Err(e) => {
                        tracing::warn!(copy = id, error = %e, "cannot make a copy");
                        self.idle(&mut stop, Some(self.retry)).await;
                    }
                },
            }
            // Only when something changed: not after each look for a
            // place. A copy asked for may have outdated another, and one
            // no longer read may go.
            if went || std::mem::take(&mut self.state.lock().changed) {
                self.sweep_logged().await;
            }
        }
    }

    /// [`Pool::cancel_unplayed`] every `every`, until the pool stops.
    async fn cancel_unplayed_every(self: Arc<Self>, every: Duration) {
        let mut stop = self.stop.subscribe();
        loop {
            tokio::select! {
                () = tokio::time::sleep(every) => {}
                () = stopped(&mut stop) => return,
            }
            if let Err(e) = self.cancel_unplayed().await {
                tracing::warn!(error = %e, "cannot drop the binge-ahead copies nobody watches");
            }
        }
    }

    /// [`Pool::cancel_unplayed`].
    async fn cancel_unplayed(&self) -> Result<usize, StoreError> {
        let since = self.store.now() - UNPLAYED.as_secs() as i64;
        let wanted: HashSet<String> = {
            let mut state = self.state.lock();
            state.binges.retain(|_, b| b.played_at >= since);
            state.binges.values().map(|b| b.copy.clone()).collect()
        };
        let mut dropped = 0;
        for copy in self.store.optimized_copies().await? {
            let pending = matches!(copy.state, CopyState::Waiting | CopyState::Making);
            if copy.target != CopyTarget::Ahead
                || !pending
                || wanted.contains(&copy.id)
                || copy.requested_at >= since
            {
                continue;
            }
            tracing::info!(
                copy = copy.id,
                media = copy.media_path,
                "a binge-ahead copy nobody is watching towards is dropped"
            );
            if self.delete(&copy.id).await? != Deleted::NotFound {
                dropped += 1;
            }
        }
        Ok(dropped)
    }

    /// [`Pool::delete`].
    async fn delete(&self, id: &str) -> Result<Deleted, StoreError> {
        {
            let mut state = self.state.lock();
            if let Some(running) = state.running.as_ref().filter(|r| r.id == id) {
                running.deleted.notify_one();
                return Ok(Deleted::Deferred);
            }
            if state.leases.contains_key(id) {
                state.doomed.insert(id.to_owned());
                return Ok(Deleted::Deferred);
            }
            if !claim(&mut state, id) {
                // Being removed already.
                return Ok(Deleted::Now);
            }
        }
        Ok(if self.discard(id).await? {
            Deleted::Now
        } else {
            Deleted::NotFound
        })
    }

    async fn sweep_logged(&self) {
        if let Err(e) = self.sweep().await {
            tracing::warn!(error = %e, "cannot evict copies");
        }
    }

    /// Something changed for the sweep: the worker looks again.
    fn changed(&self) {
        self.state.lock().changed = true;
        self.wake.notify_one();
    }

    /// Until the jobs give a place back, or woken, stopped, or
    /// [`BUSY_FALLBACK`] has passed.
    async fn wait_for_place(&self, stop: &mut watch::Receiver<bool>) {
        tokio::select! {
            () = self.jobs.place_released() => {}
            () = self.idle(stop, Some(BUSY_FALLBACK)) => {}
        }
    }

    /// Until woken, stopped, or `wait` has passed.
    async fn idle(&self, stop: &mut watch::Receiver<bool>, wait: Option<Duration>) {
        let timer = async {
            match wait {
                Some(wait) => tokio::time::sleep(wait).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            () = self.wake.notified() => {}
            () = timer => {}
            () = stopped(stop) => {}
        }
    }

    /// Make copy `id`, the head of the queue, if the budget has a place.
    async fn try_make(
        &self,
        id: &str,
        described: &mut Option<(String, Arc<MediaDescriptor>)>,
        stop: &mut watch::Receiver<bool>,
    ) -> Result<Tried, StoreError> {
        let row = match self.store.optimized_copy(id).await? {
            Some(row) if row.state == CopyState::Waiting && !row.outdated => row,
            // Gone, or outdated (the sweep removes it), or not waiting.
            _ => {
                self.pop(id);
                return Ok(Tried::Went);
            }
        };
        let media = PathBuf::from(&row.media_path);
        let descriptor = match described.as_ref().filter(|(of, _)| of == id) {
            Some((_, descriptor)) => descriptor.clone(),
            None => match self.prober.describe(&media).await {
                Ok(descriptor) => {
                    *described = Some((id.to_owned(), descriptor.clone()));
                    descriptor
                }
                Err(e) => {
                    return self
                        .fail(id, &format!("the media file cannot be read: {e}"))
                        .await;
                }
            },
        };
        match crate::files::stamp_text(&media).await {
            Ok(stamp) if stamp == row.source_stamp => {}
            Ok(_) => {
                return self
                    .fail(id, "the media file changed since the copy was asked for")
                    .await;
            }
            Err(e) => {
                return self
                    .fail(id, &format!("the media file cannot be read: {e}"))
                    .await;
            }
        }
        let link = self.making.join(format!("{id}.input"));
        let part = part_file(&self.dir, id);
        if let Err(e) = relink(&media, &link) {
            return self
                .fail(id, &format!("cannot link the media file: {e}"))
                .await;
        }
        let detected = self.transcoding.detected();
        let input = CopyInput {
            file: &media,
            input: &link,
            descriptor: &descriptor,
            video: row.video,
            part: &part,
        };
        let run = match copy_run::prepare(&self.jobs, &detected, &input) {
            Ok(run) => run,
            Err(JobError::Busy) => return Ok(Tried::Busy),
            Err(e) => return self.fail(id, &e.to_string()).await,
        };
        let Ok(hold) = self.jobs.hold_yielding(run.load, run.card.clone()) else {
            return Ok(Tried::Busy);
        };
        let deleted = Arc::new(Notify::new());
        {
            let mut state = self.state.lock();
            // Deleted while it was prepared: no longer queued.
            if state.queue.head() != Some(id) {
                drop(state);
                remove_file(&link);
                return Ok(Tried::Went);
            }
            state.queue.pop_head();
            state.running = Some(Running {
                id: id.to_owned(),
                pid: 0,
                duration_ms: descriptor.duration.map(|d| d.as_millis() as u64),
                encoded_ms: 0,
                began: Instant::now(),
                reserved: reserve(row.video, &descriptor, &media),
                deleted: deleted.clone(),
            });
        }
        *described = None;
        let made = self
            .make(&row, &media, run, hold, &deleted, stop, (&link, &part))
            .await;
        self.state.lock().running = None;
        remove_file(&link);
        made.map(|()| Tried::Went)
    }

    /// Run copy `row`'s ffmpeg in `hold`'s place, and keep what it made.
    #[expect(
        clippy::too_many_arguments,
        reason = "one caller, which has each part at hand"
    )]
    async fn make(
        &self,
        row: &CopyRow,
        media: &Path,
        run: CopyRun,
        hold: Yielding,
        deleted: &Notify,
        stop: &mut watch::Receiver<bool>,
        (link, part): (&Path, &Path),
    ) -> Result<(), StoreError> {
        let id = &row.id;
        self.store.optimized_copy_making(id).await?;
        // Room for it, before it is written.
        self.sweep_logged().await;
        let input = std::fs::read_link(link).unwrap_or_else(|_| media.to_path_buf());
        let mut attempt = run.first;
        let mut software = run.software;
        tracing::info!(copy = id, media = %media.display(), load = run.load.as_str(), "making a copy");
        let ended = loop {
            let ended = self
                .run_ffmpeg(&run.ffmpeg, &attempt, &input, &hold, deleted, stop)
                .await;
            match ended {
                // A GPU decode that failed is tried again in software,
                // once, as a conversion's is.
                Ended::Exited { ref status, .. } if exit_blames_gpu(status) => {
                    let Some(next) = software.take() else {
                        break ended;
                    };
                    if self.jobs.remember_decode_failure(media) {
                        tracing::warn!(media = %media.display(), "decoding on the GPU failed: the file decodes in software from now on");
                    }
                    remove_file(part);
                    attempt = next;
                }
                ended => break ended,
            }
        };
        // The place is given back before the bookkeeping.
        drop(hold);
        match ended {
            Ended::Exited {
                status: Ok(status), ..
            } if status.success() => {
                let file = copy_file(&self.dir, id);
                let kept = std::fs::rename(part, &file).and_then(|()| std::fs::metadata(&file));
                match kept {
                    Ok(meta) => {
                        self.store.optimized_copy_ready(id, meta.len()).await?;
                        // Made now: the last to be evicted of its kind.
                        self.store.optimized_copy_used(id).await?;
                        tracing::info!(copy = id, bytes = meta.len(), "a copy is made");
                    }
                    Err(e) => {
                        remove_file(part);
                        self.store
                            .optimized_copy_failed(id, &format!("cannot keep the copy: {e}"))
                            .await?;
                    }
                }
            }
            Ended::Exited { status, last } => {
                remove_file(part);
                let why = match (last.back(), status) {
                    (Some(line), _) => line.clone(),
                    (None, Ok(status)) => format!("ffmpeg ended: {status}"),
                    (None, Err(e)) => format!("ffmpeg could not be waited for: {e}"),
                };
                tracing::warn!(copy = id, reason = why, stderr = ?last, "a copy failed");
                self.store.optimized_copy_failed(id, &why).await?;
            }
            Ended::Spawn(why) => {
                remove_file(part);
                tracing::warn!(copy = id, error = why, "cannot run ffmpeg for a copy");
                self.store
                    .optimized_copy_failed(id, "ffmpeg could not be run")
                    .await?;
            }
            Ended::Yielded => {
                // At the head again (of the `ahead` copies, for one): the
                // next of its kind to start once there is room.
                self.state
                    .lock()
                    .queue
                    .push_front(id.clone(), row.target == CopyTarget::Ahead);
                remove_file(part);
                self.store.optimized_copy_requeued(id).await?;
                tracing::info!(
                    copy = id,
                    "live playback took a copy's place: it starts over later"
                );
            }
            Ended::Deleted => {
                remove_file(part);
                self.store.delete_optimized_copy(id).await?;
            }
            Ended::Stopped => remove_file(part),
        }
        Ok(())
    }

    /// One run of `attempt`, until it exits or is killed for another's
    /// sake.
    async fn run_ffmpeg(
        &self,
        ffmpeg: &Tool,
        attempt: &Attempt,
        input: &Path,
        hold: &Yielding,
        deleted: &Notify,
        stop: &mut watch::Receiver<bool>,
    ) -> Ended {
        // It writes in `making/` alone, no file larger than the budget.
        let jail = jail(self.max_size, &self.making, &[], &[input.to_path_buf()])
            .gpu(attempt.gpu)
            .vulkan(attempt.vulkan.clone());
        let mut child = match launch(ffmpeg, &jail, &attempt.args) {
            Ok(child) => child,
            Err(e) => return Ended::Spawn(format!("{}: {e}", ffmpeg.program().display())),
        };
        let pid = child.id().unwrap_or(0);
        if let Some(running) = self.state.lock().running.as_mut() {
            running.pid = pid;
            running.encoded_ms = 0;
        }
        // Stopped as it was spawned: `Pool::stop` saw no pid.
        if *stop.borrow() {
            signal(pid, libc::SIGKILL);
        }
        let mut out = BufReader::new(child.stdout.take().expect("piped")).lines();
        let mut err = BufReader::new(child.stderr.take().expect("piped")).lines();
        let mut last = VecDeque::new();
        let (mut out_open, mut err_open) = (true, true);
        let killed = loop {
            tokio::select! {
                status = child.wait() => {
                    // Killed by `Pool::stop`, not failed: made again at
                    // the next start.
                    if *stop.borrow() {
                        return Ended::Stopped;
                    }
                    // The rest of what it said, briefly.
                    let rest = async {
                        while let Ok(Some(line)) = err.next_line().await {
                            keep(&mut last, line);
                        }
                    };
                    let _ = tokio::time::timeout(Duration::from_secs(1), rest).await;
                    return Ended::Exited { status, last };
                }
                line = out.next_line(), if out_open => match line {
                    Ok(Some(line)) => {
                        if let Some(us) = out_time_us(&line)
                            && let Some(running) = self.state.lock().running.as_mut()
                        {
                            running.encoded_ms = us / 1000;
                        }
                    }
                    _ => out_open = false,
                },
                line = err.next_line(), if err_open => match line {
                    Ok(Some(line)) => keep(&mut last, line),
                    _ => err_open = false,
                },
                () = hold.yielded() => break Ended::Yielded,
                () = deleted.notified() => break Ended::Deleted,
                () = stopped(stop) => break Ended::Stopped,
            }
        };
        signal(pid, libc::SIGKILL);
        let _ = child.wait().await;
        killed
    }

    /// Copy `id`, the head of the queue, cannot be made: off the queue,
    /// and failed for `why`.
    async fn fail(&self, id: &str, why: &str) -> Result<Tried, StoreError> {
        self.pop(id);
        tracing::warn!(copy = id, reason = why, "a copy cannot be made");
        self.store.optimized_copy_failed(id, why).await?;
        Ok(Tried::Went)
    }

    /// `id` off the head of the queue, if it is there.
    fn pop(&self, id: &str) {
        let mut state = self.state.lock();
        if state.queue.head() == Some(id) {
            state.queue.pop_head();
        }
    }

    /// Remove the outdated copies, then, while the pool is over its
    /// budget, the copies made in eviction order; never one lent or being
    /// made.
    async fn sweep(&self) -> Result<(), StoreError> {
        let copies = self.store.optimized_copies().await?;
        let mut total = self.used_bytes(&copies).await;
        for copy in &copies {
            let over = total > self.max_size;
            if !(copy.outdated || over && copy.state == CopyState::Ready) {
                continue;
            }
            if !claim(&mut self.state.lock(), &copy.id) {
                continue;
            }
            self.discard(&copy.id).await?;
            if copy.state == CopyState::Ready {
                total = total.saturating_sub(copy.size_bytes.unwrap_or(0));
            }
            tracing::info!(
                copy = copy.id,
                outdated = copy.outdated,
                bytes = copy.size_bytes,
                "a copy is evicted"
            );
        }
        Ok(())
    }

    /// What the budget counts of `copies` (the store's rows): the copies
    /// made, and the one being made at what it is written to or expected
    /// to grow to, whichever is more.
    async fn used_bytes(&self, copies: &[CopyRow]) -> u64 {
        let running = {
            let state = self.state.lock();
            state.running.as_ref().map(|r| (r.id.clone(), r.reserved))
        };
        let made: u64 = copies
            .iter()
            .filter(|copy| copy.state == CopyState::Ready)
            .map(|copy| copy.size_bytes.unwrap_or(0))
            .sum();
        let making = match running {
            Some((id, reserved))
                if copies
                    .iter()
                    .any(|c| c.id == id && c.state == CopyState::Making) =>
            {
                let written = tokio::fs::metadata(part_file(&self.dir, &id))
                    .await
                    .map_or(0, |m| m.len());
                written.max(reserved)
            }
            _ => 0,
        };
        made + making
    }

    /// Remove claimed copy `id`'s file and row: whether there was a row.
    async fn discard(&self, id: &str) -> Result<bool, StoreError> {
        remove_file(&copy_file(&self.dir, id));
        let gone = self.store.delete_optimized_copy(id).await;
        self.state.lock().removing.remove(id);
        gone
    }
}

/// What a copy of `video` from `media` is expected to grow to: its video
/// at its most, a quarter more for the audio; the file's own size for a
/// copy at the encoder's own bitrate.
fn reserve(video: CopyVideo, descriptor: &MediaDescriptor, media: &Path) -> u64 {
    match (video.max_bitrate, descriptor.duration) {
        (Some(rate), Some(length)) => {
            let bits = u128::from(rate) * 5 / 4 * length.as_millis() / 1000;
            u64::try_from(bits / 8).unwrap_or(u64::MAX)
        }
        _ => std::fs::metadata(media).map_or(0, |m| m.len()),
    }
}

/// Until the pool is stopped. (The guard `wait_for` gives back is not
/// `Send`: it is dropped here, not held across the worker's awaits.)
async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|stopped| *stopped).await;
}

/// `line` kept as ffmpeg's last, with at most [`STDERR_LINES`] before it.
fn keep(last: &mut VecDeque<String>, line: String) {
    if last.len() == STDERR_LINES {
        last.pop_front();
    }
    last.push_back(line);
}

/// `link` made to lead to `media`, whatever it led to before.
fn relink(media: &Path, link: &Path) -> std::io::Result<()> {
    remove_file(link);
    std::os::unix::fs::symlink(std::path::absolute(media)?, link)
}

/// `path` removed; one already gone is fine.
fn remove_file(path: &Path) {
    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(path = %path.display(), error = %e, "cannot remove a copy's file");
    }
}

pub mod admin;
pub mod ahead;
pub mod play;

#[cfg(test)]
mod tests;
