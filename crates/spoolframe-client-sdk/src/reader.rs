//! Random access to a playback's file over the session's range reads
//! (P1-08, P1-16).
//!
//! GStreamer asks for many small reads (a few KiB, often out of order while
//! it probes the container) on its streaming thread, and wants the bytes
//! back on that thread. The server serves at most 16 MiB a read, with a
//! network round trip each. So the reader fetches whole blocks and keeps a
//! time's worth of them ahead of the playhead. A read blocks its caller
//! until its block arrives, which is fine on a streaming thread and never
//! happens on the main thread.
//!
//! How far ahead is a matter of time, not bytes: the file's size over its
//! length gives its bitrate, and the reader aims for [`TARGET_AHEAD`] of
//! playback, within a memory cap. When steady playback catches up with the
//! fetching anyway (a slow link), the read reports buffering and waits
//! until [`RESUME_AHEAD`] is ready again, so playback resumes smoothly
//! instead of stuttering block by block.

use parking_lot::{Condvar, Mutex, MutexGuard};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::{ManagedPlayback, SdkError as BackendError};
use spoolframe_protocol::MAX_READ;
use std::{future::Future, pin::Pin};
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// How much one fetch reads. Half the server's limit, so a fetch is quick
/// to arrive after a seek.
pub const BLOCK: u64 = 8 * 1024 * 1024;
const _: () = assert!(BLOCK <= MAX_READ);
/// How much playback the reader keeps fetched ahead of the playhead.
pub const TARGET_AHEAD: Duration = Duration::from_secs(20);
/// After an underrun, how much must be ready before playback goes on.
pub const RESUME_AHEAD: Duration = Duration::from_secs(4);
/// The most memory the read-ahead may take, whatever the bitrate.
pub const MAX_AHEAD_BYTES: u64 = 256 * 1024 * 1024;
/// Read-ahead without a known length: four blocks.
const UNKNOWN_AHEAD_BLOCKS: u64 = 4;
/// Blocks kept behind the playhead, for small seeks back.
const BEHIND: u64 = 2;
/// Read-ahead fetches in flight at once. A block a read waits for is
/// fetched at once on top of these (see `Shared::demand`).
pub const MAX_IN_FLIGHT: usize = 4;
/// Of those, the most for the place a resume will seek to: the demuxer
/// reads the header and the index first, and they must not queue behind
/// the middle of the file.
const EXPECT_IN_FLIGHT: usize = 2;
/// How often a waiting read reports progress while buffering.
const REPORT_EVERY: Duration = Duration::from_millis(250);

/// Runs a fetch somewhere it can await the network: the app passes its
/// tokio runtime, tests a thread.
pub type Spawn = Arc<dyn Fn(BoxFuture<'static, ()>) + Send + Sync>;

/// Why a read returned no bytes.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReadError {
    /// The reader was told to stop waiting (a seek or a stop).
    #[error("flushing")]
    Flushing,
    #[error(transparent)]
    Backend(#[from] BackendError),
}

/// How the read-ahead is doing, for the player's diagnostics.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Status {
    /// Bytes ready from the playhead on, without a gap.
    pub ready_bytes: u64,
    /// The same as playback time, when the bitrate is known.
    pub ready: Option<Duration>,
    /// What the reader aims for.
    pub target: Option<Duration>,
    pub in_flight: usize,
    /// Recent fetch throughput, bytes per second.
    pub throughput: Option<f64>,
}

/// Blocks to keep fetched ahead: `target` of playback at the file's
/// average bitrate, at least two blocks, at most the memory cap.
pub fn ahead_blocks(size: u64, length: Option<Duration>, target: Duration) -> u64 {
    let Some(length) = length.filter(|l| !l.is_zero()) else {
        return UNKNOWN_AHEAD_BLOCKS;
    };
    let bytes_per_second = size as f64 / length.as_secs_f64();
    let bytes = (bytes_per_second * target.as_secs_f64()) as u64;
    bytes.clamp(2 * BLOCK, MAX_AHEAD_BYTES).div_ceil(BLOCK)
}

/// Where `at` probably is in a file of `size` bytes lasting `length`: its
/// share of the bytes, less a block for the keyframe before it and the
/// bitrate's swings. A guess to read ahead from, not a seek.
pub fn guess_offset(size: u64, length: Duration, at: Duration) -> u64 {
    if length.is_zero() {
        return 0;
    }
    let share = (at.as_secs_f64() / length.as_secs_f64()).clamp(0.0, 1.0);
    ((size as f64 * share) as u64).saturating_sub(BLOCK)
}

/// Whether waiting for block `index` means steady playback outran the
/// fetching (the block next to the last one read), rather than a jump: a
/// seek, or GStreamer probing the container. Only an underrun waits for
/// [`RESUME_AHEAD`]; a jump waits only for its block.
pub fn is_underrun(last: Option<u64>, index: u64) -> bool {
    last.is_some_and(|last| index == last || index == last + 1)
}

/// What the `spoolframe://` source reads: a file by range (direct play,
/// [`MediaReader`]) or an encode as it grows (converted,
/// [`crate::progressive::ProgressiveReader`]).
pub trait ByteSource: Send + Sync {
    /// Up to `length` bytes from `offset`; none at the end. Blocks until
    /// the bytes are here, calling `buffering` with a percentage while it
    /// waits through an underrun, and with 100 when it is over.
    fn read_reporting(
        &self,
        offset: u64,
        length: u64,
        buffering: &mut dyn FnMut(u8),
    ) -> Result<Vec<u8>, ReadError>;

    /// Makes waiting reads return [`ReadError::Flushing`] until called
    /// with `false`. GStreamer's unlock and unlock_stop.
    fn set_flushing(&self, flushing: bool);

    /// The size in bytes when it is known and fixed; the source is then
    /// seekable in bytes.
    fn size(&self) -> Option<u64>;

    /// How far ahead the reader is, for the diagnostics.
    fn status(&self) -> Status;
}

impl ByteSource for MediaReader {
    fn read_reporting(
        &self,
        offset: u64,
        length: u64,
        buffering: &mut dyn FnMut(u8),
    ) -> Result<Vec<u8>, ReadError> {
        MediaReader::read_reporting(self, offset, length, buffering)
    }

    fn set_flushing(&self, flushing: bool) {
        MediaReader::set_flushing(self, flushing)
    }

    fn size(&self) -> Option<u64> {
        Some(MediaReader::size(self))
    }

    fn status(&self) -> Status {
        MediaReader::status(self)
    }
}

pub struct MediaReader {
    shared: Arc<Shared>,
}

struct Shared {
    playback: ManagedPlayback,
    size: u64,
    /// Bytes per second of playback, when the length is known.
    bitrate: Option<f64>,
    ahead: u64,
    spawn: Spawn,
    state: Mutex<State>,
    changed: Condvar,
}

struct State {
    blocks: HashMap<u64, Arc<Vec<u8>>>,
    pending: HashSet<u64>,
    failed: HashMap<u64, BackendError>,
    flushing: bool,
    /// Where the last read ended, and its block.
    playhead: u64,
    last_block: Option<u64>,
    /// Where reading is expected to go next (a resume), held against
    /// eviction until a read reaches it; see [`MediaReader::expect`].
    expected: Option<u64>,
    /// An average of recent fetch throughput, bytes per second.
    throughput: Option<f64>,
    /// The reader was dropped: fetches still running start no more.
    stopped: bool,
}

impl Drop for MediaReader {
    fn drop(&mut self) {
        self.shared.lock().stopped = true;
        self.shared.changed.notify_all();
    }
}

impl MediaReader {
    /// A range reader for a direct playback. Its owner keeps the selected
    /// tracks and replaces expired handles for all readers and controls.
    /// With the item's `length` read-ahead is a time; without, a few blocks.
    pub fn new(playback: ManagedPlayback, spawn: Spawn, length: Option<Duration>) -> Self {
        let size = playback.snapshot().size;
        let length = length.filter(|l| !l.is_zero());
        MediaReader {
            shared: Arc::new(Shared {
                playback,
                size,
                // None for a size of 0 too: a rate of 0 made status()'s
                // seconds NaN, and Duration panics on NaN.
                bitrate: length
                    .map(|l| size as f64 / l.as_secs_f64())
                    .filter(|rate| *rate > 0.0),
                ahead: ahead_blocks(size, length, TARGET_AHEAD),
                spawn,
                state: Mutex::new(State {
                    blocks: HashMap::new(),
                    pending: HashSet::new(),
                    failed: HashMap::new(),
                    flushing: false,
                    playhead: 0,
                    last_block: None,
                    expected: None,
                    throughput: None,
                    stopped: false,
                }),
                changed: Condvar::new(),
            }),
        }
    }

    /// The file's size in bytes.
    pub fn size(&self) -> u64 {
        self.shared.size
    }

    /// Up to `length` bytes from `offset`; fewer only at the end of the
    /// file, none past it. Blocks until the bytes are here.
    pub fn read(&self, offset: u64, length: u64) -> Result<Vec<u8>, ReadError> {
        self.read_reporting(offset, length, &mut |_| {})
    }

    /// [`MediaReader::read`], calling `buffering` with a percentage while
    /// it waits through an underrun, and with 100 when it is over.
    pub fn read_reporting(
        &self,
        offset: u64,
        length: u64,
        buffering: &mut dyn FnMut(u8),
    ) -> Result<Vec<u8>, ReadError> {
        let end = offset.saturating_add(length).min(self.shared.size);
        let mut bytes = Vec::with_capacity(end.saturating_sub(offset) as usize);
        let mut at = offset;
        while at < end {
            let index = at / BLOCK;
            let waited = Instant::now();
            let block = self.block(index, buffering)?;
            let waited = waited.elapsed();
            if waited > std::time::Duration::from_millis(20) {
                tracing::debug!(
                    offset = at,
                    index,
                    ms = waited.as_millis() as u64,
                    "read waited for its block"
                );
            }
            let start = (at - index * BLOCK) as usize;
            let stop = ((end - index * BLOCK) as usize).min(block.len());
            if start >= stop {
                // The server sent less than the file's size promised.
                return Err(BackendError::Protocol(format!(
                    "short read at {at} of {} bytes",
                    self.shared.size
                ))
                .into());
            }
            bytes.extend_from_slice(&block[start..stop]);
            at = index * BLOCK + stop as u64;
        }
        let mut state = self.shared.lock();
        state.playhead = at;
        let last = at.saturating_sub(1) / BLOCK;
        state.last_block = Some(last);
        // Reading reached the expected place: ordinary read-ahead from here.
        if let Some(expected) = state.expected
            && (expected.saturating_sub(BEHIND)..=expected + self.shared.ahead).contains(&last)
        {
            state.expected = None;
        }
        Ok(bytes)
    }

    /// Makes waiting reads return [`ReadError::Flushing`] until called with
    /// `false`. GStreamer's unlock and unlock_stop.
    pub fn set_flushing(&self, flushing: bool) {
        self.shared.lock().flushing = flushing;
        self.shared.changed.notify_all();
    }

    /// How far ahead the reader is.
    pub fn status(&self) -> Status {
        let state = self.shared.lock();
        let ready_bytes = self.shared.ready_from(&state, state.playhead);
        Status {
            ready_bytes,
            ready: self
                .shared
                .bitrate
                .map(|rate| Duration::from_secs_f64(ready_bytes as f64 / rate)),
            target: self.shared.bitrate.map(|_| TARGET_AHEAD),
            in_flight: state.pending.len(),
            throughput: state.throughput,
        }
    }

    /// Block `index`, fetched now if needed, with the read-ahead after it
    /// queued. Through an underrun, waits until enough is ready to go on.
    fn block(&self, index: u64, buffering: &mut dyn FnMut(u8)) -> Result<Arc<Vec<u8>>, ReadError> {
        let shared = &self.shared;
        let mut state = shared.lock();
        shared.evict(&mut state, index);
        shared.demand(&shared.clone(), &mut state, index);
        shared.top_up(&shared.clone(), &mut state, index);
        let underrun = !state.blocks.contains_key(&index) && is_underrun(state.last_block, index);
        // Bytes to have ready before an underrun ends: RESUME_AHEAD of
        // playback, or the rest of the file if that is less.
        let resume_bytes = shared
            .bitrate
            .map_or(BLOCK, |rate| (rate * RESUME_AHEAD.as_secs_f64()) as u64)
            .max(1)
            .min(shared.size.saturating_sub(index * BLOCK));
        let mut reported = false;
        loop {
            // Leaving early still ends a buffering this read started:
            // GstPlay pauses under 100 and plays again only at 100.
            let early = if state.flushing {
                Some(ReadError::Flushing)
            } else {
                state.failed.remove(&index).map(ReadError::from)
            };
            if let Some(error) = early {
                drop(state);
                if reported {
                    buffering(100);
                }
                return Err(error);
            }
            if !state.blocks.contains_key(&index) && !state.pending.contains(&index) {
                // Evicted or failed and forgotten between two waits.
                shared.demand(&shared.clone(), &mut state, index);
                shared.top_up(&shared.clone(), &mut state, index);
            }
            if let Some(block) = state.blocks.get(&index) {
                let ready = shared.ready_from(&state, index * BLOCK);
                if !underrun || ready >= resume_bytes {
                    let block = block.clone();
                    drop(state);
                    if reported {
                        buffering(100);
                    }
                    return Ok(block);
                }
            }
            if underrun {
                // A read-ahead block that failed is fetched again here:
                // the underrun waits for a run of blocks, and nothing else
                // would restart one (a flaky link stalled at N% for good).
                shared.top_up(&shared.clone(), &mut state, index);
                let ready = shared.ready_from(&state, index * BLOCK);
                let percent = (ready * 100 / resume_bytes).min(99) as u8;
                // Reported without the lock: GStreamer handles a posted
                // message on this thread, and fetches must not wait on it.
                drop(state);
                buffering(percent);
                reported = true;
                state = shared.lock();
            }
            shared.changed.wait_for(&mut state, REPORT_EVERY);
        }
    }

    /// Starts reading ahead from `offset` before anything asks for it, and
    /// keeps what arrives until a read gets there. For a resume: the
    /// demuxer reads the header and index first, then seeks, and by then
    /// the blocks around the resume point are on their way or here.
    pub fn expect(&self, offset: u64) {
        let index = offset.min(self.shared.size.saturating_sub(1)) / BLOCK;
        let mut state = self.shared.lock();
        state.expected = Some(index);
        self.shared
            .top_up_to(&self.shared.clone(), &mut state, index, EXPECT_IN_FLIGHT);
    }

    /// Starts fetches up to the in-flight limit, nearest first; for tests
    /// and for the player to warm up before playing.
    pub fn prefetch(&self, offset: u64) {
        let mut state = self.shared.lock();
        self.shared
            .top_up(&self.shared.clone(), &mut state, offset / BLOCK);
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock()
    }

    /// The last block of the file.
    fn last_index(&self) -> u64 {
        self.size.saturating_sub(1) / BLOCK
    }

    /// Bytes ready from `offset` on, without a gap.
    fn ready_from(&self, state: &State, offset: u64) -> u64 {
        let mut index = offset / BLOCK;
        let mut ready = 0;
        while let Some(block) = state.blocks.get(&index) {
            ready += block.len() as u64;
            index += 1;
        }
        ready.saturating_sub(offset - (offset / BLOCK) * BLOCK)
    }

    /// Drops blocks too far from `from`, the block being read, to be read
    /// soon. Only a read evicts: a fetch arriving for a block a read
    /// jumped to must stay until that read takes it.
    /// The first and last blocks stay: demuxers read the header and the
    /// index at the end of the file again and again while they open it,
    /// and a resume's log showed each fetched six or seven times.
    fn evict(&self, state: &mut State, from: u64) {
        let near = |at: u64| at.saturating_sub(BEHIND)..=at + self.ahead;
        let (keep, expected) = (near(from), state.expected.map(near));
        let last = self.last_index();
        state.blocks.retain(|index, _| {
            *index == 0
                || *index == last
                || keep.contains(index)
                || expected.as_ref().is_some_and(|e| e.contains(index))
        });
    }

    /// Fetches block `index` now, whatever the read-ahead has in flight:
    /// a read waits for it. Without this, a resume's read-ahead around the
    /// saved position took every slot first, and the demuxer's reads of
    /// the header and the index queued behind four blocks of the middle of
    /// the file: seconds of black screen.
    fn demand(&self, this: &Arc<Self>, state: &mut State, index: u64) {
        if index > self.last_index()
            || state.blocks.contains_key(&index)
            || state.pending.contains(&index)
        {
            return;
        }
        self.start(this, state, index);
    }

    /// Queues the blocks from `from` through the read-ahead, nearest
    /// first, up to the in-flight limit.
    fn top_up(&self, this: &Arc<Self>, state: &mut State, from: u64) {
        self.top_up_to(this, state, from, MAX_IN_FLIGHT);
    }

    /// The same, stopping once `limit` fetches are in flight.
    fn top_up_to(&self, this: &Arc<Self>, state: &mut State, from: u64, limit: usize) {
        let last = (from + self.ahead).min(self.last_index());
        for index in from..=last {
            if state.pending.len() >= limit {
                break;
            }
            if state.blocks.contains_key(&index) || state.pending.contains(&index) {
                continue;
            }
            self.start(this, state, index);
        }
    }

    fn start(&self, this: &Arc<Self>, state: &mut State, index: u64) {
        if state.stopped {
            return;
        }
        state.pending.insert(index);
        state.failed.remove(&index);
        let shared = this.clone();
        (self.spawn)(Box::pin(async move { shared.fetch(index).await }));
    }

    async fn fetch(self: Arc<Self>, index: u64) {
        let started = Instant::now();
        let result = self.read_block(index).await;
        let elapsed = started.elapsed().as_secs_f64();
        tracing::debug!(
            index,
            bytes = result.as_ref().map_or(0, Vec::len),
            ms = (elapsed * 1000.0) as u64,
            ok = result.is_ok(),
            "block fetched"
        );
        let mut state = self.lock();
        state.pending.remove(&index);
        match result {
            Ok(bytes) => {
                if elapsed > 0.0 {
                    let rate = bytes.len() as f64 / elapsed;
                    state.throughput = Some(match state.throughput {
                        Some(old) => old * 0.7 + rate * 0.3,
                        None => rate,
                    });
                }
                state.blocks.insert(index, Arc::new(bytes));
                // Keep the read-ahead going from where playback is, and
                // from where it is expected to go.
                let from = state.last_block.unwrap_or(index);
                self.top_up(&self, &mut state, from);
                if let Some(expected) = state.expected {
                    self.top_up_to(&self, &mut state, expected, EXPECT_IN_FLIGHT);
                }
            }
            Err(error) => {
                state.failed.insert(index, error);
            }
        }
        drop(state);
        self.changed.notify_all();
    }

    /// Reads a whole block. The server may answer a read with fewer bytes
    /// than asked; the rest is asked for from where it stopped. On an
    /// expired handle, the playback is started again (once, whoever finds
    /// out first) and the read goes on.
    async fn read_block(&self, index: u64) -> Result<Vec<u8>, BackendError> {
        let start = index * BLOCK;
        let want = BLOCK.min(self.size.saturating_sub(start));
        let mut bytes = Vec::new();
        while (bytes.len() as u64) < want {
            let got = bytes.len() as u64;
            match self.playback.read(start + got, want - got).await {
                Ok(more) if more.is_empty() => break,
                // The whole block in one answer, as usual: kept as it came.
                Ok(more) if bytes.is_empty() && more.len() as u64 == want => return Ok(more),
                Ok(more) => {
                    bytes.reserve_exact(want as usize - bytes.len());
                    bytes.extend_from_slice(&more);
                }
                Err(e) => return Err(e),
            }
        }
        Ok(bytes)
    }
}

#[cfg(test)]
impl MediaReader {
    fn for_test(
        session: Arc<crate::reader_tests::TestBackend>,
        item: spoolframe_types::MediaId,
        selection: spoolframe_protocol::Selection,
        start: crate::Playback,
        spawn: Spawn,
        length: Option<Duration>,
    ) -> Self {
        let _enter = crate::reader_tests::runtime().enter();
        Self::new(
            ManagedPlayback::adopt(
                session,
                item,
                Some(selection),
                crate::PlaybackOptions::default(),
                start,
            ),
            spawn,
            length,
        )
    }
}

#[cfg(test)]
mod tests {

    use std::path::PathBuf;

    use spoolframe_types::{MediaKind, Namespace};

    use super::*;
    use crate::Playback as PlaybackStart;
    use crate::reader_tests::{FakeClient, TestBackend as Session, direct_plan};
    use spoolframe_protocol::{PlaybackHandle, Selection};
    use spoolframe_types::MediaId;

    use crate::reader_tests::{media_file, ready, thread_spawn};

    fn reader_for(path: PathBuf) -> (MediaReader, Arc<Session>) {
        let client = FakeClient::new(Some(path));
        let profile = FakeClient::profile("andrey");
        let session = client.session(&profile).unwrap();
        let item = MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap();
        let start = ready(session.start_playback(&item, None, None)).unwrap();
        let reader = MediaReader::for_test(
            session.clone(),
            item,
            Selection::default(),
            start,
            thread_spawn(),
            None,
        );
        (reader, session)
    }

    #[test]
    fn a_small_read_returns_exactly_its_range() {
        let (_dir, path, bytes) = media_file("reader-small", 10_000);
        let (reader, _) = reader_for(path);
        assert_eq!(reader.read(4_000, 100).unwrap(), bytes[4_000..4_100]);
    }

    #[test]
    fn a_read_across_a_block_boundary_joins_both_blocks() {
        let len = (BLOCK + 1_000) as usize;
        let (_dir, path, bytes) = media_file("reader-boundary", len);
        let (reader, _) = reader_for(path);
        let from = BLOCK as usize - 500;
        assert_eq!(
            reader.read(from as u64, 1_000).unwrap(),
            bytes[from..from + 1_000]
        );
    }

    #[test]
    fn a_read_at_the_end_returns_what_is_left() {
        let (_dir, path, bytes) = media_file("reader-end", 5_000);
        let (reader, _) = reader_for(path);
        assert_eq!(reader.read(4_990, 100).unwrap(), bytes[4_990..]);
    }

    #[test]
    fn a_read_past_the_end_is_empty() {
        let (_dir, path, _) = media_file("reader-past", 5_000);
        let (reader, _) = reader_for(path);
        assert_eq!(reader.read(6_000, 100).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn an_expired_handle_is_started_again_and_the_read_succeeds() {
        let (_dir, path, bytes) = media_file("reader-expired", 3_000);
        let client = FakeClient::new(Some(path));
        let profile = FakeClient::profile("andrey");
        let session = client.session(&profile).unwrap();
        let item = MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap();
        // A handle the fake never issued reads as expired.
        let stale = PlaybackStart {
            handle: PlaybackHandle("long-gone".into()),
            plan: direct_plan(),
            size: 3_000,
            chosen: None,
            epoch: 0,
            markers: Vec::new(),
        };
        let reader = MediaReader::for_test(
            session,
            item,
            Selection::default(),
            stale,
            thread_spawn(),
            None,
        );
        assert_eq!(reader.read(0, 10).unwrap(), bytes[..10]);
    }

    #[test]
    fn short_reads_are_asked_again_until_the_block_is_whole() {
        let (_dir, path, bytes) = media_file("reader-short", 50_000);
        let client = FakeClient::new(Some(path)).with_read_limit(4_096);
        let profile = FakeClient::profile("andrey");
        let session = client.session(&profile).unwrap();
        let item = MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap();
        let start = ready(session.start_playback(&item, None, None)).unwrap();
        let reader = MediaReader::for_test(
            session,
            item,
            Selection::default(),
            start,
            thread_spawn(),
            None,
        );
        assert_eq!(reader.read(40_000, 9_000).unwrap(), bytes[40_000..49_000]);
    }

    #[test]
    fn a_flushing_reader_stops_waiting() {
        let (_dir, path, _) = media_file("reader-flushing", 1_000);
        let (reader, _) = reader_for(path);
        reader.set_flushing(true);
        assert_eq!(reader.read(0, 10), Err(ReadError::Flushing));
    }

    #[test]
    fn a_low_bitrate_file_reads_ahead_the_two_block_minimum() {
        // 1 GiB over two hours: 20 s is about 3 MiB.
        let blocks = ahead_blocks(1 << 30, Some(Duration::from_secs(7200)), TARGET_AHEAD);
        assert_eq!(blocks, 2);
    }

    #[test]
    fn a_high_bitrate_file_reads_ahead_twenty_seconds() {
        // About 10 MB/s (an 80 Mbit/s remux): 20 s is 200 MB, 24 blocks.
        let blocks = ahead_blocks(
            72_000_000_000,
            Some(Duration::from_secs(7200)),
            TARGET_AHEAD,
        );
        assert_eq!(blocks, 24);
    }

    #[test]
    fn the_read_ahead_never_passes_the_memory_cap() {
        let blocks = ahead_blocks(
            500_000_000_000,
            Some(Duration::from_secs(3600)),
            TARGET_AHEAD,
        );
        assert_eq!(blocks, MAX_AHEAD_BYTES / BLOCK);
    }

    #[test]
    fn without_a_length_the_read_ahead_is_a_few_blocks() {
        assert_eq!(
            ahead_blocks(1 << 30, None, TARGET_AHEAD),
            UNKNOWN_AHEAD_BLOCKS
        );
    }

    #[test]
    fn waiting_for_the_next_block_is_an_underrun() {
        assert!(is_underrun(Some(4), 5));
    }

    #[test]
    fn a_jump_is_not_an_underrun() {
        assert!(!is_underrun(Some(4), 40));
        assert!(!is_underrun(None, 0));
    }

    /// A spawn that holds fetches without running them: the reader's
    /// queueing shows without any arriving.
    fn held_spawn() -> (Spawn, Arc<Mutex<usize>>) {
        let spawned = Arc::new(Mutex::new(0));
        let count = spawned.clone();
        let spawn: Spawn = Arc::new(move |future| {
            *count.lock() += 1;
            drop(future);
        });
        (spawn, spawned)
    }

    #[test]
    fn no_more_than_the_limit_are_fetched_at_once() {
        let (_dir, path, _) = media_file("reader-in-flight", 10);
        let client = FakeClient::new(Some(path));
        let profile = FakeClient::profile("andrey");
        let session = client.session(&profile).unwrap();
        let item = MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap();
        let mut start = ready(session.start_playback(&item, None, None)).unwrap();
        // Pretend the file is large and fast, so the read-ahead is long.
        start.size = 100 * BLOCK;
        let (spawn, spawned) = held_spawn();
        let reader = MediaReader::for_test(
            session,
            item,
            Selection::default(),
            start,
            spawn,
            Some(Duration::from_secs(60)),
        );
        reader.prefetch(0);
        assert_eq!(*spawned.lock(), MAX_IN_FLIGHT);
        assert_eq!(reader.status().in_flight, MAX_IN_FLIGHT);
    }

    #[test]
    fn after_a_read_the_status_counts_the_bytes_ready_ahead() {
        let (_dir, path, _) = media_file("reader-status", 10_000);
        let (reader, _) = reader_for(path);
        reader.read(0, 1_000).unwrap();
        assert_eq!(reader.status().ready_bytes, 9_000);
    }

    #[test]
    fn a_jump_past_the_read_ahead_gets_its_block() {
        // Four blocks over ten hours: the read-ahead is the two-block
        // minimum, so the last block is outside it. A container probe
        // reads there; the read-ahead's own fetches finishing must not
        // evict the block before the probe takes it.
        let len = 3 * BLOCK as usize + 1_000;
        let (_dir, path, bytes) = media_file("reader-jump", len);
        let client = FakeClient::new(Some(path));
        let profile = FakeClient::profile("andrey");
        let session = client.session(&profile).unwrap();
        let item = MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap();
        let start = ready(session.start_playback(&item, None, None)).unwrap();
        let reader = MediaReader::for_test(
            session,
            item,
            Selection::default(),
            start,
            thread_spawn(),
            Some(Duration::from_secs(36_000)),
        );
        reader.read(0, 100).unwrap();
        let at = len as u64 - 500;
        assert_eq!(reader.read(at, 500).unwrap(), bytes[at as usize..]);
    }

    /// A seek (flushing) in the middle of an underrun still ends the
    /// buffering: GstPlay pauses on a buffering message under 100 and
    /// plays again only on 100, so without it playback stayed paused until
    /// the user pressed play.
    #[test]
    fn a_flush_during_an_underrun_ends_the_buffering() {
        let len = BLOCK as usize + 1_000;
        let (_dir, path, _) = media_file("reader-flush-underrun", len);
        let client = FakeClient::new(Some(path));
        let profile = FakeClient::profile("andrey");
        let session = client.session(&profile).unwrap();
        let item = MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap();
        let start = ready(session.start_playback(&item, None, None)).unwrap();
        // Only the first fetch runs; the second block never arrives.
        let first = Arc::new(Mutex::new(true));
        let threads = thread_spawn();
        let spawn: Spawn = Arc::new(move |future| {
            if std::mem::replace(&mut *first.lock(), false) {
                threads(future);
            } else {
                drop(future);
            }
        });
        let reader = Arc::new(MediaReader::for_test(
            session,
            item,
            Selection::default(),
            start,
            spawn,
            None,
        ));
        reader.read(BLOCK - 100, 100).unwrap();
        {
            let reader = reader.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                reader.set_flushing(true);
            });
        }
        let mut reports = Vec::new();
        let read = reader.read_reporting(BLOCK, 100, &mut |p| reports.push(p));
        assert_eq!(read, Err(ReadError::Flushing));
        assert_eq!(reports.last(), Some(&100), "reports: {reports:?}");
    }

    #[test]
    fn a_resume_point_is_guessed_by_its_share_of_the_file() {
        // Half way through 100 blocks: block 50, less one.
        let offset = guess_offset(
            100 * BLOCK,
            Duration::from_secs(100),
            Duration::from_secs(50),
        );
        assert_eq!(offset, 49 * BLOCK);
    }

    #[test]
    fn a_guess_near_the_start_is_the_start() {
        assert_eq!(
            guess_offset(100 * BLOCK, Duration::from_secs(100), Duration::ZERO),
            0
        );
    }

    /// Reading the header must not throw away what was fetched for the
    /// resume point: the read there is served from memory.
    #[test]
    fn what_was_fetched_for_a_resume_survives_reading_the_header() {
        let len = 5 * BLOCK as usize;
        let (_dir, path, bytes) = media_file("reader-expect", len);
        let client = FakeClient::new(Some(path));
        let profile = FakeClient::profile("andrey");
        let session = client.session(&profile).unwrap();
        let item = MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap();
        let start = ready(session.start_playback(&item, None, None)).unwrap();
        let fetches = Arc::new(Mutex::new(0usize));
        let spawn: Spawn = {
            let (fetches, threads) = (fetches.clone(), thread_spawn());
            Arc::new(move |future| {
                *fetches.lock() += 1;
                threads(future);
            })
        };
        // Ten hours: the read-ahead is two blocks, so block 4 is far from
        // block 0.
        let reader = MediaReader::for_test(
            session,
            item,
            Selection::default(),
            start,
            spawn,
            Some(Duration::from_secs(36_000)),
        );
        reader.expect(4 * BLOCK);
        reader.read(0, 100).unwrap();
        // Let the fetches for block 4 land.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !reader.shared.lock().blocks.contains_key(&4) && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        reader.read(BLOCK, 100).unwrap();
        let before = *fetches.lock();
        let at = 4 * BLOCK + 10;
        assert_eq!(
            reader.read(at, 10).unwrap(),
            bytes[at as usize..at as usize + 10]
        );
        assert_eq!(*fetches.lock(), before, "block 4 was fetched again");
    }

    /// A resume's read-ahead must not hold up the header: a read of block
    /// 0 starts its own fetch even when the read-ahead has slots in
    /// flight. Before, it queued behind them (seconds of black screen).
    #[test]
    fn a_read_is_fetched_at_once_whatever_the_read_ahead_has_in_flight() {
        let (_dir, path, _) = media_file("reader-demand", 10);
        let client = FakeClient::new(Some(path));
        let session = client.session(&FakeClient::profile("andrey")).unwrap();
        let item = MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap();
        let mut start = ready(session.start_playback(&item, None, None)).unwrap();
        start.size = 100 * BLOCK;
        let (spawn, spawned) = held_spawn();
        let reader = Arc::new(MediaReader::for_test(
            session,
            item,
            Selection::default(),
            start,
            spawn,
            Some(Duration::from_secs(60)),
        ));
        // Read-ahead far from the read takes every slot, and never arrives.
        reader.prefetch(50 * BLOCK);
        assert_eq!(reader.status().in_flight, MAX_IN_FLIGHT);
        let before = *spawned.lock();
        let reading = {
            let reader = reader.clone();
            std::thread::spawn(move || reader.read(0, 10))
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !reader.shared.lock().pending.contains(&0) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let header_fetched = reader.shared.lock().pending.contains(&0);
        reader.set_flushing(true);
        let _ = reading.join();
        assert!(
            header_fetched,
            "block 0 waited behind {before} read-ahead fetches"
        );
    }

    #[test]
    fn a_resume_takes_at_most_two_fetch_slots() {
        let (_dir, path, _) = media_file("reader-expect-slots", 10);
        let client = FakeClient::new(Some(path));
        let session = client.session(&FakeClient::profile("andrey")).unwrap();
        let item = MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap();
        let mut start = ready(session.start_playback(&item, None, None)).unwrap();
        start.size = 100 * BLOCK;
        let (spawn, spawned) = held_spawn();
        let reader = MediaReader::for_test(
            session,
            item,
            Selection::default(),
            start,
            spawn,
            Some(Duration::from_secs(60)),
        );
        reader.expect(50 * BLOCK);
        assert_eq!(*spawned.lock(), EXPECT_IN_FLIGHT);
    }

    #[test]
    fn the_header_and_index_blocks_survive_a_jump() {
        let len = 40 * BLOCK as usize + 100;
        let (_dir, path, _) = media_file("reader-pinned", len);
        let (reader, _) = reader_for(path);
        reader.read(0, 10).unwrap();
        reader.read(len as u64 - 10, 10).unwrap();
        // A read far from both.
        reader.read(20 * BLOCK, 10).unwrap();
        let state = reader.shared.lock();
        assert!(state.blocks.contains_key(&0) && state.blocks.contains_key(&40));
    }

    #[test]
    fn a_dropped_reader_starts_no_more_fetches() {
        let (_dir, path, _) = media_file("reader-stopped", 10);
        let client = FakeClient::new(Some(path));
        let session = client.session(&FakeClient::profile("andrey")).unwrap();
        let item = MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap();
        let mut start = ready(session.start_playback(&item, None, None)).unwrap();
        start.size = 100 * BLOCK;
        let (spawn, spawned) = held_spawn();
        let reader = MediaReader::for_test(session, item, Selection::default(), start, spawn, None);
        let shared = reader.shared.clone();
        drop(reader);
        shared.top_up(&shared.clone(), &mut shared.lock(), 0);
        assert_eq!(*spawned.lock(), 0);
    }

    #[test]
    fn an_empty_file_with_a_length_has_a_status() {
        let (_dir, path, _) = media_file("reader-empty", 0);
        let client = FakeClient::new(Some(path));
        let session = client.session(&FakeClient::profile("andrey")).unwrap();
        let item = MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap();
        let start = ready(session.start_playback(&item, None, None)).unwrap();
        let reader = MediaReader::for_test(
            session,
            item,
            Selection::default(),
            start,
            thread_spawn(),
            Some(Duration::from_secs(60)),
        );
        assert_eq!(reader.status().ready, None);
    }
}
