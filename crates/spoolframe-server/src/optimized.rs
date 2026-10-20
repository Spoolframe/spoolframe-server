//! Optimized versions (PX-37): the pool that makes the server's converted
//! copies, keeps them within `optimized.max_size_mb`, and lends them to
//! whoever reads them.
//!
//! A copy is not a file (the user's decision, 2026-10-20): it is its
//! media file and its **tracks**, each converted stream a file of its own
//! ([`crate::encode::track_args`]). One video-only file per video plan,
//! one file per converted audio track; a track kept as it is is always
//! read from the media file. One track serves every copy that needs it:
//! a DTS track turned into E-AC-3 is the same file for a download's
//! Apple copy and for `Mobile`'s. A copy is muxed when it is read
//! ([`mux`], or a playback's remux), which encodes nothing.
//!
//! One track is made at a time, in the order they were asked for, in
//! three lanes ([`Lane`]): a download's tracks first, then those only
//! binge-ahead copies ([`CopyTarget::Ahead`], [`ahead`]) want, then those
//! only an admin's on-demand requests ([`requests`]) want. A download's
//! copy is not held up by a guess at what someone watches next, nor that
//! guess by a whole library asked for at once. Tracks are work for later, and
//! what the encode budget has spare is live playback's first. The track
//! at the head of the queue starts once the budget has a free place for
//! it ([`Jobs::hold_yielding`]): it tries again when the jobs give a
//! place back ([`Jobs::place_released`]), and at once when a copy is
//! asked for. A live playback the budget would turn away takes the
//! running track's place: its ffmpeg is killed, its part file removed,
//! and it waits again at the head of its lane, to be made from the start
//! (ffmpeg cannot resume a half-written MKV).
//!
//! A track is written as `making/<id>.mkv.part` in `optimized.dir`, from
//! a link to the media file there (PX-11), and renamed to `<id>.mkv` once
//! ffmpeg exits cleanly. A server that stops mid-track leaves the part:
//! the next start removes `making/` and makes the track again. Any other
//! `.mkv` there that no track names (0.13's whole-file copies, say) is
//! removed at start too.
//!
//! The tracks made and the one being made (its part, or what it is
//! expected to grow to if larger) are kept within the budget: outdated
//! tracks go first, whatever the budget, then those no copy but `ahead`
//! ones uses, then the least recently used ([`Store::optimized_tracks`]).
//! A track a pinned on-demand request's copy uses is never evicted.
//! A track goes with the copies made with it, which are no longer whole.
//! A track someone is reading ([`Lease`], which holds every track of the
//! copy it lends) is never removed.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::sync::{Notify, watch};

use spoolframe_catalog::Catalog;
use spoolframe_media::sandbox::Tool;
use spoolframe_media::{MediaDescriptor, Prober};

use crate::config::OptimizedSettings;
use crate::encode::job::{
    JobError, Jobs, Yielding, exit_blames_gpu, jail, launch, out_time_us, signal,
};
use crate::encode::track_run::{self, Attempt, TrackInput, TrackRun};
use crate::store::optimized::{CopyRow, CopyState, CopyTarget, NewCopy, Recipe, TrackRow};
use crate::store::{Store, StoreError};
use crate::transcode::Transcoding;

/// How soon the worker tries again after the store failed it.
pub const RETRY: Duration = Duration::from_secs(10);

/// How long the track at the head of the queue waits for a place with no
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

/// The directory in `optimized.dir` tracks are made in: emptied at start.
pub const MAKING_DIR: &str = "making";

/// How many of ffmpeg's last stderr lines a failure keeps.
const STDERR_LINES: usize = 5;

/// What the pool works with.
#[derive(Clone)]
pub struct PoolSources {
    pub store: Store,
    pub jobs: Arc<Jobs>,
    /// The encoders and GPUs found, read as each track starts.
    pub transcoding: Arc<Transcoding>,
    pub prober: Arc<Prober>,
    /// What on-demand requests' scopes hold ([`requests`]).
    pub catalog: Arc<dyn Catalog>,
    /// Told each time the catalog is replaced: the requests' scopes are
    /// looked at again then, and every [`requests::FOLLOW`] besides.
    pub catalog_changes: Option<watch::Receiver<u64>>,
}

impl std::fmt::Debug for PoolSources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PoolSources")
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
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
    /// Of its tracks written, 0 to 1: for a copy being made, once the
    /// running track's length is known; 1 for one made.
    pub fraction: Option<f64>,
    /// How long its running track has yet to go, at the speed it has gone
    /// so far.
    pub eta: Option<Duration>,
}

/// What [`Pool::delete`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deleted {
    /// The copy is gone, and its tracks no other copy uses with it.
    Now,
    /// It goes once its readers' leases end, or a track of its once its
    /// ffmpeg, told to stop, has: nobody can lease it from now on.
    Deferred,
    /// There was no such copy.
    NotFound,
}

/// The copy pool. Dropping it stops it, as [`Pool::stop`] does.
#[derive(Debug)]
pub struct Pool {
    inner: Arc<Inner>,
}

struct Inner {
    store: Store,
    jobs: Arc<Jobs>,
    transcoding: Arc<Transcoding>,
    prober: Arc<Prober>,
    catalog: Arc<dyn Catalog>,
    dir: PathBuf,
    making: PathBuf,
    max_size: u64,
    /// How soon the worker tries again after an error.
    retry: Duration,
    state: Mutex<State>,
    /// A copy was asked for, or a lease ended: the worker looks again.
    wake: Notify,
    /// An on-demand request has items to ask for: its asker looks again.
    ask: Notify,
    stop: watch::Sender<bool>,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("dir", &self.dir)
            .field("max_size", &self.max_size)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
struct State {
    /// The tracks waiting.
    queue: Queue,
    /// Something changed that the sweep may act on: a copy asked for (it
    /// may outdate another), or a lease ended. Read and cleared by the
    /// worker.
    changed: bool,
    /// The copies and tracks being read, and by how many leases.
    leases: HashMap<String, usize>,
    /// Tracks being removed now: none is lent.
    removing: HashSet<String>,
    running: Option<Running>,
    /// Each viewer's binge-ahead copy, by user id.
    binges: HashMap<String, Binge>,
}

/// Which of the queue's lanes a track waits in: the first lane's go
/// first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Lane {
    /// A download's copy, or a profile's asked for by its own caller.
    Now,
    /// Only binge-ahead copies want it.
    Ahead,
    /// Only an admin's on-demand requests want it (and maybe binge-ahead
    /// copies): background work, behind everything else.
    Background,
}

impl Lane {
    const ALL: [Self; 3] = [Self::Now, Self::Ahead, Self::Background];

    /// The lane track `row` waits in at start, by the copies that want it.
    fn of(row: &TrackRow) -> Self {
        if row.ahead_only {
            Self::Ahead
        } else if row.on_demand_only {
            Self::Background
        } else {
            Self::Now
        }
    }
}

/// The tracks waiting, each lane in the order they were asked for.
#[derive(Debug, Default)]
struct Queue {
    lanes: [VecDeque<String>; 3],
}

impl Queue {
    fn of(&mut self, lane: Lane) -> &mut VecDeque<String> {
        &mut self.lanes[lane as usize]
    }

    /// The lane track `id` waits in, if it does.
    fn lane_of(&self, id: &str) -> Option<Lane> {
        Lane::ALL
            .into_iter()
            .find(|&lane| self.lanes[lane as usize].iter().any(|q| q == id))
    }

    /// Track `id` last of `lane`. One waiting in a later lane moves to
    /// this one; one in this lane or an earlier one stays where it is.
    fn push(&mut self, id: String, lane: Lane) {
        match self.lane_of(&id) {
            Some(now) if now <= lane => return,
            Some(now) => self.of(now).retain(|q| *q != id),
            None => {}
        }
        self.of(lane).push_back(id);
    }

    /// Track `id` first of `lane`.
    fn push_front(&mut self, id: String, lane: Lane) {
        self.of(lane).push_front(id);
    }

    #[cfg(test)]
    fn iter(&self) -> impl Iterator<Item = &String> {
        self.lanes.iter().flatten()
    }

    /// The next to be made, and its lane.
    fn head(&self) -> Option<(&str, Lane)> {
        Lane::ALL.into_iter().find_map(|lane| {
            self.lanes[lane as usize]
                .front()
                .map(|id| (id.as_str(), lane))
        })
    }

    fn pop_head(&mut self) {
        if let Some((_, lane)) = self.head() {
            self.of(lane).pop_front();
        }
    }

    fn remove(&mut self, id: &str) {
        for lane in &mut self.lanes {
            lane.retain(|q| q != id);
        }
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

/// The track whose ffmpeg runs.
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
    /// Told when the track is outdated while being made: no copy wants it.
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

/// A copy's stored tracks, by what they stand in for: the video, and the
/// audio tracks by their stream index in the media file. A stream not
/// here is read from the media file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CopyFiles {
    pub video: Option<PathBuf>,
    pub audio: BTreeMap<u32, PathBuf>,
}

/// A copy lent to a reader, with every track it is made of: none is
/// removed until this is dropped.
#[derive(Debug)]
pub struct Lease {
    inner: Arc<Inner>,
    id: String,
    /// The tracks it holds.
    tracks: Vec<String>,
    files: CopyFiles,
}

impl Lease {
    /// The copy's id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The copy's stored tracks.
    pub fn files(&self) -> &CopyFiles {
        &self.files
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock();
        for id in std::iter::once(&self.id).chain(&self.tracks) {
            match state.leases.get_mut(id) {
                Some(n) if *n > 1 => *n -= 1,
                _ => {
                    state.leases.remove(id);
                }
            }
        }
        drop(state);
        // A copy or track no longer read may be one to remove.
        self.inner.changed();
    }
}

/// Mark track `id` as being removed, out of the queue: false, and nothing
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
    /// removed and waits again, the tracks waiting are queued oldest
    /// first, a track whose file is gone is forgotten with its copies,
    /// and a file no track names is removed.
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
                tracing::info!(dir = %making.display(), "removed the tracks a stopped server was making")
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(prepare(&making)(e)),
        }
        crate::files::private_dir(&making).map_err(prepare(&making))?;
        let store = sources.store;
        let requeued = store.requeue_interrupted_optimized_tracks().await?;
        if requeued > 0 {
            tracing::info!(
                tracks = requeued,
                "tracks a stopped server was making wait again"
            );
        }
        let mut waiting = Vec::new();
        let mut named = HashSet::new();
        for track in store.optimized_tracks().await? {
            match track.state {
                CopyState::Ready if !track_file(&dir, &track.id).exists() => {
                    tracing::warn!(
                        track = track.id,
                        "a track's file is gone: it is forgotten, with its copies"
                    );
                    store.delete_optimized_track(&track.id).await?;
                }
                CopyState::Ready => {
                    named.insert(format!("{}.mkv", track.id));
                }
                CopyState::Waiting => waiting.push(track),
                _ => {}
            }
        }
        let strays = remove_strays(&dir, &named);
        if strays > 0 {
            tracing::info!(
                files = strays,
                "removed the files in optimized.dir no track names (0.13's whole-file copies)"
            );
        }
        waiting.sort_by(|a, b| (a.requested_at, &a.id).cmp(&(b.requested_at, &b.id)));
        let mut queue = Queue::default();
        for track in waiting {
            let lane = Lane::of(&track);
            queue.push(track.id, lane);
        }
        let inner = Arc::new(Inner {
            store,
            jobs: sources.jobs,
            transcoding: sources.transcoding,
            prober: sources.prober,
            catalog: sources.catalog,
            dir,
            making,
            max_size: settings.max_size_bytes,
            retry,
            state: Mutex::new(State {
                queue,
                ..State::default()
            }),
            wake: Notify::new(),
            ask: Notify::new(),
            stop: watch::Sender::new(false),
        });
        // Before the worker starts: it sweeps again only once woken.
        inner.sweep_logged().await;
        tokio::spawn(inner.clone().work());
        tokio::spawn(inner.clone().cancel_unplayed_every(AHEAD_CHECK));
        tokio::spawn(inner.clone().ask_for_requests(sources.catalog_changes));
        Ok(Self { inner })
    }

    /// The copy of `copy`'s file for its target: the one there is, or a
    /// new one, its tracks not made yet queued (PX-38's downloads ask for
    /// theirs here, in a profile or in original quality). One that failed
    /// is tried again.
    pub async fn request(&self, copy: NewCopy) -> Result<CopyRow, StoreError> {
        self.inner.request(copy, Lane::Now).await
    }

    /// Where track `id`'s file is once made: to describe it before it is
    /// lent, as a lease counts as a use.
    pub fn track_file(&self, id: &str) -> PathBuf {
        track_file(&self.inner.dir, id)
    }

    /// Copy `id`'s stored tracks, by their store rows: where each file is,
    /// by what it stands in for. Not lent: [`Pool::lease`] lends them.
    pub fn files_of(&self, tracks: &[TrackRow]) -> CopyFiles {
        files_of(&self.inner.dir, tracks)
    }

    /// How far copy `id` is; `None` for no such copy.
    pub async fn progress(&self, id: &str) -> Result<Option<CopyProgress>, StoreError> {
        let Some(row) = self.inner.store.optimized_copy(id).await? else {
            return Ok(None);
        };
        let (fraction, eta) = match row.state {
            CopyState::Ready => (Some(1.0), None),
            CopyState::Making => {
                let tracks = self.inner.store.optimized_copy_tracks(id).await?;
                let made = tracks
                    .iter()
                    .filter(|t| t.state == CopyState::Ready)
                    .count();
                let state = self.inner.state.lock();
                let running = state
                    .running
                    .as_ref()
                    .filter(|r| tracks.iter().any(|t| t.id == r.id));
                match running.map(Running::progress) {
                    Some((Some(f), eta)) if !tracks.is_empty() => {
                        (Some((made as f64 + f) / tracks.len() as f64), eta)
                    }
                    Some((None, eta)) => (None, eta),
                    _ => (None, None),
                }
            }
            CopyState::Waiting | CopyState::Failed => (None, None),
        };
        Ok(Some(CopyProgress {
            state: row.state,
            fraction,
            eta,
        }))
    }

    /// Copy `id` to read, if it is made and neither it nor a track of its
    /// is outdated: it and its tracks stay until the lease is dropped.
    /// Counts as a use of each, for eviction.
    pub async fn lease(&self, id: &str) -> Result<Option<Lease>, StoreError> {
        let tracks = self.inner.store.optimized_copy_tracks(id).await?;
        let ids: Vec<String> = tracks.iter().map(|t| t.id.clone()).collect();
        {
            let mut state = self.inner.state.lock();
            if ids.iter().any(|t| state.removing.contains(t)) {
                return Ok(None);
            }
            for held in std::iter::once(id).chain(ids.iter().map(String::as_str)) {
                *state.leases.entry(held.to_owned()).or_default() += 1;
            }
        }
        // Held from here: dropped on any way out, which lets it go.
        let lease = Lease {
            inner: self.inner.clone(),
            id: id.to_owned(),
            files: files_of(&self.inner.dir, &tracks),
            tracks: ids,
        };
        if !self.inner.store.optimized_copy_used_if_ready(id).await? {
            return Ok(None);
        }
        Ok(Some(lease))
    }

    /// Delete copy `id`, and its tracks no other copy uses. One being
    /// read goes when its last lease ends, and a track being made once
    /// its ffmpeg is stopped ([`Deleted::Deferred`]).
    pub async fn delete(&self, id: &str) -> Result<Deleted, StoreError> {
        self.inner.delete(id).await
    }

    /// Remove the tracks over the budget and the outdated ones now, as
    /// the pool does after each track: after a profile is edited, say.
    pub async fn sweep(&self) -> Result<(), StoreError> {
        self.inner.sweep().await
    }

    /// Stop making tracks, killing the running one's ffmpeg: the server
    /// is stopping. Its row stays `making`, and the next start makes it
    /// again.
    pub fn stop(&self) {
        self.inner.stop.send_replace(true);
        if let Some(pid) = self.inner.state.lock().running.as_ref().map(|r| r.pid) {
            signal(pid, libc::SIGKILL);
        }
    }

    /// The tracks waiting, the next first: for tests.
    #[cfg(test)]
    fn queued(&self) -> Vec<String> {
        self.inner.state.lock().queue.iter().cloned().collect()
    }

    /// Binge-ahead ([`ahead`]): `copy`, an `ahead` copy of the next
    /// episode of `show`, asked for `viewer` (a user id). Its tracks wait
    /// behind every other copy's. It is the viewer's one: the one asked
    /// for them before is dropped if it has not started, unless another
    /// viewer asked for it too; one being made or made stays.
    pub async fn request_ahead(
        &self,
        viewer: &str,
        show: &str,
        copy: NewCopy,
    ) -> Result<CopyRow, StoreError> {
        let row = self.request(copy).await?;
        let old = {
            let mut state = self.inner.state.lock();
            let before = state.binges.insert(
                viewer.to_owned(),
                Binge {
                    show: show.to_owned(),
                    copy: row.id.clone(),
                    played_at: self.inner.store.now(),
                },
            );
            before
                .map(|b| b.copy)
                .filter(|old| *old != row.id && !state.binges.values().any(|b| b.copy == *old))
        };
        if let Some(old) = old
            && self
                .inner
                .store
                .optimized_copy(&old)
                .await?
                .is_some_and(|c| c.state == CopyState::Waiting)
        {
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
    /// for them has played the show of for [`UNPLAYED`]: their tracks no
    /// other copy wants are removed, one being made stopped. One asked for
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

/// Where track `id`'s file is, once made.
fn track_file(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.mkv"))
}

/// Where track `id` is written while it is made.
fn part_file(dir: &Path, id: &str) -> PathBuf {
    dir.join(MAKING_DIR).join(format!("{id}.mkv.part"))
}

/// `tracks`' files in `dir`, by what each stands in for.
fn files_of(dir: &Path, tracks: &[TrackRow]) -> CopyFiles {
    let mut files = CopyFiles::default();
    for track in tracks {
        let file = track_file(dir, &track.id);
        match track.recipe {
            Recipe::Video(_) => files.video = Some(file),
            Recipe::Audio(audio) => {
                files.audio.insert(audio.stream, file);
            }
        }
    }
    files
}

/// Remove the `.mkv` files right in `dir` that `named` (the ready
/// tracks' file names) does not hold: how many.
fn remove_strays(dir: &Path, named: &HashSet<String>) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let file = entry.file_type().is_ok_and(|t| t.is_file());
        if file && name.ends_with(".mkv") && !named.contains(&name) {
            remove_file(&entry.path());
            removed += 1;
        }
    }
    removed
}

/// What the worker did with the track at the head of the queue.
enum Tried {
    /// Made it, failed it, or found it gone: the next one's turn.
    Went,
    /// No place in the budget for it: it waits.
    Busy,
}

/// How one ffmpeg run of a track ended.
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
    /// No copy wants it any more.
    Deleted,
    /// The server is stopping.
    Stopped,
}

impl Inner {
    /// [`Pool::request`], its tracks not made yet queued in `lane` (an
    /// `ahead` copy's in [`Lane::Ahead`]).
    async fn request(&self, copy: NewCopy, lane: Lane) -> Result<CopyRow, StoreError> {
        let recipes = self.recipes(&copy).await;
        let tracks = recipes.as_deref().unwrap_or_default();
        let (mut row, mut new) = self.store.request_optimized_copy(&copy, tracks).await?;
        // Its file could not be read when it was asked for: asked anew.
        if !new
            && row.failure.is_some()
            && self.store.optimized_copy_tracks(&row.id).await?.is_empty()
        {
            self.store.delete_optimized_copy(&row.id).await?;
            (row, new) = self.store.request_optimized_copy(&copy, tracks).await?;
        }
        if let (true, Err(why)) = (new, &recipes) {
            tracing::warn!(copy = row.id, reason = why, "a copy cannot be made");
            self.store.optimized_copy_failed(&row.id, why).await?;
            row.state = CopyState::Failed;
            row.failure = Some(why.clone());
            return Ok(row);
        }
        let lane = if row.target == CopyTarget::Ahead {
            Lane::Ahead
        } else {
            lane
        };
        let mut again = false;
        for track in self.store.optimized_copy_tracks(&row.id).await? {
            match track.state {
                CopyState::Failed => {
                    self.store.optimized_track_requeued(&track.id).await?;
                    again = true;
                }
                CopyState::Waiting => {}
                CopyState::Making | CopyState::Ready => continue,
            }
            self.state.lock().queue.push(track.id, lane);
        }
        if again {
            row = self.store.optimized_copy(&row.id).await?.unwrap_or(row);
        }
        // A new copy may have outdated the file's old one: evicted too.
        self.changed();
        Ok(row)
    }

    /// The tracks `copy` stores, from its file's description; why there
    /// are none, said for the admin, when its file cannot be read or no
    /// encoder converts its audio.
    async fn recipes(&self, copy: &NewCopy) -> Result<Vec<Recipe>, String> {
        let descriptor = self
            .prober
            .describe(Path::new(&copy.media_path))
            .await
            .map_err(|e| format!("the media file cannot be read: {e}"))?;
        let detected = self.transcoding.detected();
        crate::encode::track_args::recipes(
            &descriptor,
            copy.video,
            copy.target.keep_audio().as_ref(),
            &detected.software,
        )
    }

    async fn work(self: Arc<Self>) {
        let mut stop = self.stop.subscribe();
        // The head's descriptor, kept while it waits for a place.
        let mut described = None;
        while !*stop.borrow() {
            let head = self
                .state
                .lock()
                .queue
                .head()
                .map(|(id, lane)| (id.to_owned(), lane));
            // A track made, failed or gone may let another go.
            let mut went = false;
            match head {
                None => self.idle(&mut stop, None).await,
                Some((id, lane)) => {
                    match self.try_make(&id, lane, &mut described, &mut stop).await {
                        Ok(Tried::Went) => went = true,
                        Ok(Tried::Busy) => self.wait_for_place(&mut stop).await,
                        Err(e) => {
                            tracing::warn!(track = id, error = %e, "cannot make a track");
                            self.idle(&mut stop, Some(self.retry)).await;
                        }
                    }
                }
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
        for copy in self.store.ahead_optimized_copies().await? {
            let pending = matches!(copy.state, CopyState::Waiting | CopyState::Making);
            if !pending || wanted.contains(&copy.id) || copy.requested_at >= since {
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

    /// [`Pool::delete`]: a copy read is outdated, to go once its leases
    /// end; else its row goes now. Either way its tracks no other copy
    /// uses are outdated, and those neither read nor made go now.
    async fn delete(&self, id: &str) -> Result<Deleted, StoreError> {
        if self.store.optimized_copy(id).await?.is_none() {
            return Ok(Deleted::NotFound);
        }
        let leased = self.state.lock().leases.contains_key(id);
        let unshared = self.store.outdate_unshared_optimized_tracks(id).await?;
        if leased {
            self.store.outdate_optimized_copy(id).await?;
        } else {
            self.store.delete_optimized_copy(id).await?;
        }
        let mut deferred = leased;
        for track in &unshared {
            let claimed = {
                let mut state = self.state.lock();
                if let Some(running) = state.running.as_ref().filter(|r| r.id == *track) {
                    running.deleted.notify_one();
                }
                claim(&mut state, track)
            };
            if claimed {
                self.discard(track).await?;
            } else {
                deferred = true;
            }
        }
        // What is deferred goes as its lease ends or its ffmpeg stops,
        // each of which wakes the sweep.
        Ok(if deferred {
            Deleted::Deferred
        } else {
            Deleted::Now
        })
    }

    async fn sweep_logged(&self) {
        if let Err(e) = self.sweep().await {
            tracing::warn!(error = %e, "cannot evict tracks");
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

    /// Make track `id`, the head of the queue (of `lane`), if the budget
    /// has a place.
    async fn try_make(
        &self,
        id: &str,
        lane: Lane,
        described: &mut Option<(String, Arc<MediaDescriptor>)>,
        stop: &mut watch::Receiver<bool>,
    ) -> Result<Tried, StoreError> {
        let row = match self.store.optimized_track(id).await? {
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
        let input = TrackInput {
            file: &media,
            input: &link,
            descriptor: &descriptor,
            recipe: row.recipe,
            part: &part,
        };
        let run = match track_run::prepare(&self.jobs, &detected, &input) {
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
            if state.queue.head().map(|(head, _)| head) != Some(id) {
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
                reserved: reserve(&row.recipe, &descriptor, &media),
                deleted: deleted.clone(),
            });
        }
        *described = None;
        let made = self
            .make(
                &row,
                lane,
                &media,
                run,
                hold,
                &deleted,
                stop,
                (&link, &part),
            )
            .await;
        self.state.lock().running = None;
        remove_file(&link);
        made.map(|()| Tried::Went)
    }

    /// Run track `row`'s ffmpeg in `hold`'s place, and keep what it made.
    #[expect(
        clippy::too_many_arguments,
        reason = "one caller, which has each part at hand"
    )]
    async fn make(
        &self,
        row: &TrackRow,
        lane: Lane,
        media: &Path,
        run: TrackRun,
        hold: Yielding,
        deleted: &Notify,
        stop: &mut watch::Receiver<bool>,
        (link, part): (&Path, &Path),
    ) -> Result<(), StoreError> {
        let id = &row.id;
        self.store.optimized_track_making(id).await?;
        // Room for it, before it is written.
        self.sweep_logged().await;
        let input = std::fs::read_link(link).unwrap_or_else(|_| media.to_path_buf());
        let mut attempt = run.first;
        let mut software = run.software;
        tracing::info!(track = id, recipe = row.recipe.key(), media = %media.display(), load = run.load.as_str(), "making a track");
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
                let file = track_file(&self.dir, id);
                let kept = std::fs::rename(part, &file).and_then(|()| std::fs::metadata(&file));
                match kept {
                    Ok(meta) => {
                        self.store.optimized_track_ready(id, meta.len()).await?;
                        // Made now: the last to be evicted of its kind.
                        self.store.optimized_track_used(id).await?;
                        tracing::info!(track = id, bytes = meta.len(), "a track is made");
                    }
                    Err(e) => {
                        remove_file(part);
                        self.store
                            .optimized_track_failed(id, &format!("cannot keep the track: {e}"))
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
                tracing::warn!(track = id, reason = why, stderr = ?last, "a track failed");
                self.store.optimized_track_failed(id, &why).await?;
            }
            Ended::Spawn(why) => {
                remove_file(part);
                tracing::warn!(track = id, error = why, "cannot run ffmpeg for a track");
                self.store
                    .optimized_track_failed(id, "ffmpeg could not be run")
                    .await?;
            }
            Ended::Yielded => {
                // At the head of its lane again: the next of its kind to
                // start once there is room.
                self.state.lock().queue.push_front(id.clone(), lane);
                remove_file(part);
                self.store.optimized_track_requeued(id).await?;
                tracing::info!(
                    track = id,
                    "live playback took a track's place: it starts over later"
                );
            }
            Ended::Deleted => {
                remove_file(part);
                self.store.delete_optimized_track(id).await?;
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

    /// Track `id`, the head of the queue, cannot be made: off the queue,
    /// and failed for `why`.
    async fn fail(&self, id: &str, why: &str) -> Result<Tried, StoreError> {
        self.pop(id);
        tracing::warn!(track = id, reason = why, "a track cannot be made");
        self.store.optimized_track_failed(id, why).await?;
        Ok(Tried::Went)
    }

    /// `id` off the head of the queue, if it is there.
    fn pop(&self, id: &str) {
        let mut state = self.state.lock();
        if state.queue.head().map(|(head, _)| head) == Some(id) {
            state.queue.pop_head();
        }
    }

    /// Remove the outdated copies no lease holds; then the outdated
    /// tracks, then, while the pool is over its budget, the tracks made in
    /// eviction order: never one lent, being made, or pinned.
    async fn sweep(&self) -> Result<(), StoreError> {
        for copy in self.store.outdated_optimized_copies().await? {
            if !self.state.lock().leases.contains_key(&copy) {
                self.store.delete_optimized_copy(&copy).await?;
            }
        }
        let tracks = self.store.optimized_tracks().await?;
        let mut total = self.used(&tracks).await;
        for track in &tracks {
            let over = total > self.max_size;
            if !(track.outdated || over && track.state == CopyState::Ready && !track.pinned) {
                continue;
            }
            if !claim(&mut self.state.lock(), &track.id) {
                continue;
            }
            let copies = self.discard(&track.id).await?;
            if track.state == CopyState::Ready {
                total = total.saturating_sub(track.size_bytes.unwrap_or(0));
            }
            tracing::info!(
                track = track.id,
                recipe = track.recipe.key(),
                outdated = track.outdated,
                bytes = track.size_bytes,
                copies = ?copies,
                "a track is evicted, with the copies made with it"
            );
        }
        Ok(())
    }

    /// What the budget counts of `tracks` (the store's rows): the tracks
    /// made, and the one being made at what it is written to or expected
    /// to grow to, whichever is more.
    async fn used(&self, tracks: &[TrackRow]) -> u64 {
        let running = {
            let state = self.state.lock();
            state.running.as_ref().map(|r| (r.id.clone(), r.reserved))
        };
        let made: u64 = tracks
            .iter()
            .filter(|t| t.state == CopyState::Ready)
            .map(|t| t.size_bytes.unwrap_or(0))
            .sum();
        let making = match running {
            Some((id, reserved))
                if tracks
                    .iter()
                    .any(|t| t.id == id && t.state == CopyState::Making) =>
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

    /// Remove claimed track `id`'s file and row, and the copies made with
    /// it: their ids.
    async fn discard(&self, id: &str) -> Result<Vec<String>, StoreError> {
        remove_file(&track_file(&self.dir, id));
        let gone = self.store.delete_optimized_track(id).await;
        self.state.lock().removing.remove(id);
        gone
    }
}

/// What track `recipe` of `media` is expected to grow to: its bitrate
/// over the film's length, a tenth more for the container; the file's
/// own size for a video at the encoder's own bitrate.
fn reserve(recipe: &Recipe, descriptor: &MediaDescriptor, media: &Path) -> u64 {
    let rate = match recipe {
        Recipe::Video(video) => video.max_bitrate,
        Recipe::Audio(audio) => Some(audio.bitrate),
    };
    match (rate, descriptor.duration) {
        (Some(rate), Some(length)) => {
            let bits = u128::from(rate) * 11 / 10 * length.as_millis() / 1000;
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
        tracing::warn!(path = %path.display(), error = %e, "cannot remove a track's file");
    }
}

pub mod admin;
pub mod ahead;
pub mod mux;
pub mod play;
pub mod requests;

#[cfg(test)]
mod tests;
