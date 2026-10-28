//! Reading a converted playback's output while the server encodes it.
//!
//! The output is one Matroska stream that grows as the encode runs. Its
//! size is unknown until the encode ends, and a byte offset in it means
//! nothing once a seek restarts the encode (a new epoch). So unlike
//! [`crate::reader::MediaReader`] this reads in order only: one fetch at a
//! time from where the last one ended, kept up to [`MAX_BUFFERED`] ahead of
//! the demuxer. A seek is a new encode and a new reader, from offset 0.
//!
//! When the demuxer catches up with the encode mid-stream, the read
//! reports buffering and waits until [`RESUME_BYTES`] are ready (or the
//! encode ends), so playback resumes smoothly rather than frame by frame.

use parking_lot::{Condvar, Mutex, MutexGuard};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::reader::{ByteSource, ReadError, Spawn, Status};
use crate::{ManagedPlayback, SdkError as BackendError};

/// How much one fetch asks for.
pub const CHUNK: u64 = 4 * 1024 * 1024;
/// The most fetched ahead of the demuxer.
pub const MAX_BUFFERED: u64 = 128 * 1024 * 1024;
/// After the demuxer caught up with the encode, how much must be ready
/// before playback goes on, when the output's bitrate is not known.
pub const RESUME_BYTES: u64 = 4 * 1024 * 1024;
/// With a known bitrate, the margin is this much playback...
const RESUME_SECONDS: u64 = 4;
/// ...but never less than this.
const MIN_RESUME_BYTES: u64 = 256 * 1024;
// A read waiting for its margin needs the fetch to get there: the cap
// must leave room for it.
const _: () = assert!(RESUME_BYTES < MAX_BUFFERED);
/// Kept behind the last read, for the demuxer's small steps back.
const KEEP_BEHIND: u64 = 1024 * 1024;
/// How often a waiting read reports progress while buffering.
const REPORT_EVERY: Duration = Duration::from_millis(250);

pub struct ProgressiveReader {
    shared: Arc<Shared>,
}

struct Shared {
    playback: ManagedPlayback,
    source: crate::Playback,
    /// The most fetched ahead of the last read.
    cap: u64,
    /// The margin to have ready after running dry (see [`resume_bytes`]).
    /// The first this many bytes of an encode are its start, not a stall.
    resume: u64,
    spawn: Spawn,
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default)]
struct State {
    /// Fetched bytes, in order, each with its offset.
    chunks: VecDeque<(u64, Vec<u8>)>,
    /// Where fetching has got to: the end of the last chunk.
    end: u64,
    /// The encode has ended and every byte is fetched.
    complete: bool,
    failed: Option<BackendError>,
    flushing: bool,
    /// The reader was dropped: the fetch stops.
    stopped: bool,
    /// A fetch is running.
    fetching: bool,
    /// Where the last read ended.
    playhead: u64,
    throughput: Option<f64>,
}

impl ProgressiveReader {
    /// Pins the current handle and epoch of a converted playback, reading
    /// from byte zero, for `bitrate` bits per second of output (the plan's
    /// cap): a stall waits for seconds of it, not a fixed byte count.
    /// Fetching starts at once.
    pub fn new(playback: ManagedPlayback, spawn: Spawn, bitrate: Option<u64>) -> Self {
        let resume = resume_bytes(bitrate);
        Self::with_cap(playback, spawn, MAX_BUFFERED, resume)
    }

    fn with_cap(playback: ManagedPlayback, spawn: Spawn, cap: u64, resume: u64) -> Self {
        let shared = Arc::new(Shared {
            source: playback.snapshot(),
            playback,
            cap,
            resume,
            spawn,
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        });
        shared.top_up(&shared.clone(), &mut shared.lock());
        Self { shared }
    }
}

/// The margin to wait for after running dry: RESUME_SECONDS of an output
/// of `bitrate` bits per second, within MIN_RESUME_BYTES and
/// RESUME_BYTES; RESUME_BYTES when the bitrate is unknown.
pub fn resume_bytes(bitrate: Option<u64>) -> u64 {
    bitrate.map_or(RESUME_BYTES, |bits| {
        (bits / 8 * RESUME_SECONDS).clamp(MIN_RESUME_BYTES, RESUME_BYTES)
    })
}

impl Drop for ProgressiveReader {
    fn drop(&mut self) {
        self.shared.lock().stopped = true;
        self.shared.changed.notify_all();
    }
}

impl ByteSource for ProgressiveReader {
    fn read_reporting(
        &self,
        offset: u64,
        length: u64,
        buffering: &mut dyn FnMut(u8),
    ) -> Result<Vec<u8>, ReadError> {
        let shared = &self.shared;
        let mut state = shared.lock();
        // Mid-stream, running dry means the encode is behind: wait for a
        // margin. At the start there is nothing to resume.
        let mut reported = false;
        let finish = |reported: bool, buffering: &mut dyn FnMut(u8)| {
            if reported {
                buffering(100);
            }
        };
        loop {
            if state.flushing {
                drop(state);
                finish(reported, buffering);
                return Err(ReadError::Flushing);
            }
            let ready = state.end.saturating_sub(offset);
            let enough = !reported || ready >= shared.resume || state.complete;
            if ready > 0 && enough {
                let bytes = state.copy(offset, length.min(ready));
                let Some(bytes) = bytes else {
                    return Err(BackendError::Protocol(format!(
                        "read at {offset}, behind what the reader keeps"
                    ))
                    .into());
                };
                state.playhead = offset + bytes.len() as u64;
                state.trim();
                shared.top_up(&shared.clone(), &mut state);
                drop(state);
                finish(reported, buffering);
                return Ok(bytes);
            }
            if state.complete && offset >= state.end {
                drop(state);
                finish(reported, buffering);
                return Ok(Vec::new());
            }
            if let Some(error) = state.failed.clone() {
                drop(state);
                finish(reported, buffering);
                return Err(error.into());
            }
            state.playhead = offset;
            shared.top_up(&shared.clone(), &mut state);
            // Waiting within the encode's first bytes is its start, not a
            // stall: only past them does it report buffering and wait for
            // the margin (every new encode's second read used to).
            if offset > shared.resume {
                let percent = (ready * 100 / shared.resume).min(99) as u8;
                // Reported without the lock: GStreamer handles a posted
                // message on this thread, and the fetch must not wait on it.
                drop(state);
                buffering(percent);
                reported = true;
                state = shared.lock();
            }
            shared.changed.wait_for(&mut state, REPORT_EVERY);
        }
    }

    fn set_flushing(&self, flushing: bool) {
        self.shared.lock().flushing = flushing;
        self.shared.changed.notify_all();
    }

    /// Unknown until the encode ends, and never seekable in bytes.
    fn size(&self) -> Option<u64> {
        None
    }

    fn status(&self) -> Status {
        let state = self.shared.lock();
        Status {
            ready_bytes: state.end.saturating_sub(state.playhead),
            ready: None,
            target: None,
            in_flight: usize::from(state.fetching),
            throughput: state.throughput,
        }
    }
}

impl State {
    /// `length` bytes from `offset`, which must be fetched; `None` if they
    /// were already let go.
    fn copy(&self, offset: u64, length: u64) -> Option<Vec<u8>> {
        let (first, _) = self.chunks.front()?;
        if offset < *first {
            return None;
        }
        let end = offset + length;
        let mut bytes = Vec::with_capacity(length as usize);
        for (start, chunk) in &self.chunks {
            let stop = start + chunk.len() as u64;
            if stop <= offset || *start >= end {
                continue;
            }
            let from = (offset.max(*start) - start) as usize;
            let to = (end.min(stop) - start) as usize;
            bytes.extend_from_slice(&chunk[from..to]);
        }
        Some(bytes)
    }

    /// Lets go of chunks well behind the playhead.
    fn trim(&mut self) {
        let keep_from = self.playhead.saturating_sub(KEEP_BEHIND);
        while let Some((start, chunk)) = self.chunks.front() {
            if start + chunk.len() as u64 > keep_from {
                break;
            }
            self.chunks.pop_front();
        }
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock()
    }

    /// Starts the fetch if there is room ahead and nothing stops it.
    fn top_up(&self, this: &Arc<Self>, state: &mut State) {
        let room = state.end.saturating_sub(state.playhead) < self.cap;
        if state.fetching || state.complete || state.stopped || state.failed.is_some() || !room {
            return;
        }
        state.fetching = true;
        let shared = this.clone();
        (self.spawn)(Box::pin(async move { shared.fetch().await }));
    }

    /// Fetches chunks in order until the buffer is full, the encode ends,
    /// a read fails or the reader is dropped.
    async fn fetch(self: Arc<Self>) {
        loop {
            let offset = {
                let mut state = self.lock();
                let room = state.end.saturating_sub(state.playhead) < self.cap;
                if state.stopped || state.complete || !room {
                    state.fetching = false;
                    return;
                }
                state.end
            };
            let started = Instant::now();
            let result = self.playback.read_chunk(&self.source, offset, CHUNK).await;
            let elapsed = started.elapsed().as_secs_f64();
            let mut state = self.lock();
            match result {
                Ok(chunk) => {
                    let got = chunk.bytes.len() as u64;
                    if got > 0 {
                        if elapsed > 0.0 {
                            let rate = got as f64 / elapsed;
                            state.throughput = Some(match state.throughput {
                                Some(old) => old * 0.7 + rate * 0.3,
                                None => rate,
                            });
                        }
                        state.chunks.push_back((offset, chunk.bytes));
                        state.end = offset + got;
                    }
                    // An empty read that is not the end: the server waited
                    // and the encode had nothing new yet. Ask again.
                    state.complete = chunk.complete && state.end >= chunk.size;
                }
                Err(error) => {
                    tracing::warn!(error = %error, "reading the converted output failed");
                    state.failed = Some(error);
                    state.fetching = false;
                    drop(state);
                    self.changed.notify_all();
                    return;
                }
            }
            drop(state);
            self.changed.notify_all();
        }
    }
}

#[cfg(test)]
impl ProgressiveReader {
    fn for_test(
        session: Arc<crate::reader_tests::TestBackend>,
        handle: spoolframe_protocol::PlaybackHandle,
        epoch: u32,
        spawn: Spawn,
        bitrate: Option<u64>,
    ) -> Self {
        Self::cap_for_test(
            session,
            handle,
            epoch,
            spawn,
            MAX_BUFFERED,
            resume_bytes(bitrate),
        )
    }
    fn cap_for_test(
        session: Arc<crate::reader_tests::TestBackend>,
        handle: spoolframe_protocol::PlaybackHandle,
        epoch: u32,
        spawn: Spawn,
        cap: u64,
        resume: u64,
    ) -> Self {
        let playback = session.adopt(handle, epoch);
        Self::with_cap(playback, spawn, cap, resume)
    }
}

#[cfg(test)]
mod tests {

    use std::path::PathBuf;

    use spoolframe_types::{MediaId, MediaKind, Namespace};

    use super::*;
    use crate::reader_tests::{FakeClient, TestBackend as Session};
    use spoolframe_protocol::{PlaybackHandle, PlaybackPlan};

    use crate::reader_tests::{media_file, ready, thread_spawn};

    /// A converted playback on the fake, whose output grows by `grow`
    /// bytes per chunk read.
    fn converted(path: PathBuf, grow: u64) -> (Arc<Session>, PlaybackHandle, PlaybackPlan) {
        let client = FakeClient::new(Some(path)).converting(grow);
        let session = client.session(&FakeClient::profile("andrey")).unwrap();
        let item = MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap();
        let start = ready(session.start_playback(&item, None, None)).unwrap();
        (session, start.handle, start.plan)
    }

    /// Reads from 0 to the end in `step`-byte reads, as BaseSrc does.
    fn read_all(reader: &ProgressiveReader, step: u64) -> Vec<u8> {
        let mut all = Vec::new();
        loop {
            let bytes = reader
                .read_reporting(all.len() as u64, step, &mut |_| {})
                .unwrap();
            if bytes.is_empty() {
                return all;
            }
            all.extend_from_slice(&bytes);
        }
    }

    #[test]
    fn the_fake_plans_a_progressive_conversion() {
        let (_dir, path, _) = media_file("progressive-plan", 10);
        let (_, _, plan) = converted(path, 10);
        assert!(matches!(
            plan,
            PlaybackPlan::Converted {
                delivery: spoolframe_types::Delivery::Progressive,
                ..
            }
        ));
    }

    #[test]
    fn the_whole_output_is_read_in_order_as_it_grows() {
        let (_dir, path, bytes) = media_file("progressive-whole", 3 * CHUNK as usize + 1_234);
        let (session, handle, _) = converted(path, 1_000_000);
        let reader = ProgressiveReader::for_test(session, handle, 0, thread_spawn(), None);
        assert!(read_all(&reader, 65_536) == bytes, "the bytes differ");
    }

    #[test]
    fn a_read_past_what_is_encoded_waits_until_it_is() {
        // 100 bytes appear per chunk read: the read at 5 000 needs fifty.
        let (_dir, path, bytes) = media_file("progressive-wait", 10_000);
        let (session, handle, _) = converted(path, 100);
        let reader = ProgressiveReader::for_test(session, handle, 0, thread_spawn(), None);
        read_all_to(&reader, 5_000);
        assert_eq!(
            reader.read_reporting(5_000, 10, &mut |_| {}).unwrap(),
            bytes[5_000..5_010]
        );
    }

    /// Reads up to `to` in small steps.
    fn read_all_to(reader: &ProgressiveReader, to: u64) {
        let mut at = 0;
        while at < to {
            at += reader
                .read_reporting(at, (to - at).min(512), &mut |_| {})
                .unwrap()
                .len() as u64;
        }
    }

    #[test]
    fn the_end_of_the_encode_is_the_end_of_the_stream() {
        let (_dir, path, _) = media_file("progressive-end", 1_000);
        let (session, handle, _) = converted(path, 1_000);
        let reader = ProgressiveReader::for_test(session, handle, 0, thread_spawn(), None);
        read_all_to(&reader, 1_000);
        assert_eq!(
            reader.read_reporting(1_000, 10, &mut |_| {}).unwrap(),
            Vec::<u8>::new()
        );
    }

    #[test]
    fn a_flushing_reader_stops_waiting() {
        let (_dir, path, _) = media_file("progressive-flushing", 1_000);
        let (session, handle, _) = converted(path, 1_000);
        let reader = ProgressiveReader::for_test(session, handle, 0, thread_spawn(), None);
        reader.set_flushing(true);
        assert_eq!(
            reader.read_reporting(0, 10, &mut |_| {}),
            Err(ReadError::Flushing)
        );
    }

    #[test]
    fn a_reader_of_a_replaced_encode_fails_with_the_new_epoch() {
        let (_dir, path, _) = media_file("progressive-restarted", 1_000);
        let (session, handle, _) = converted(path, 10);
        ready(session.seek_playback(&handle, Duration::from_secs(60))).unwrap();
        let reader = ProgressiveReader::for_test(session, handle, 0, thread_spawn(), None);
        assert_eq!(
            reader.read_reporting(0, 10, &mut |_| {}),
            Err(ReadError::Backend(BackendError::PlaybackRestarted {
                epoch: 1
            }))
        );
    }

    #[test]
    fn the_new_epoch_reads_from_the_start() {
        let (_dir, path, bytes) = media_file("progressive-new-epoch", 2_000);
        let (session, handle, _) = converted(path, 500);
        let epoch = ready(session.seek_playback(&handle, Duration::from_secs(60))).unwrap();
        let reader = ProgressiveReader::for_test(session, handle, epoch, thread_spawn(), None);
        assert_eq!(read_all(&reader, 300), bytes);
    }

    #[test]
    fn running_dry_mid_stream_reports_buffering_until_a_margin_is_ready() {
        // The cap holds fetching at 5 MiB, so a read at 10 MiB finds
        // nothing there: it reports, waits for RESUME_BYTES, then says 100.
        let mib = 1024 * 1024;
        let (_dir, path, _) = media_file("progressive-buffering", 16 * mib as usize);
        let (session, handle, _) = converted(path, 256 * 1024);
        let reader = ProgressiveReader::cap_for_test(
            session,
            handle,
            0,
            thread_spawn(),
            5 * mib,
            RESUME_BYTES,
        );
        let mut reports = Vec::new();
        reader
            .read_reporting(10 * mib, 10, &mut |p| reports.push(p))
            .unwrap();
        assert_eq!(
            (reports.first().is_some_and(|p| *p < 100), reports.last()),
            (true, Some(&100)),
            "reports: {reports:?}"
        );
    }

    #[test]
    fn fetching_stops_at_the_cap_ahead_of_the_reader() {
        let (_dir, path, _) = media_file("progressive-cap", 10 * CHUNK as usize);
        let (session, handle, _) = converted(path, 10 * CHUNK);
        let reader = ProgressiveReader::cap_for_test(
            session,
            handle,
            0,
            thread_spawn(),
            2 * CHUNK,
            RESUME_BYTES,
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while reader.shared.lock().fetching && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(reader.status().ready_bytes, 2 * CHUNK);
    }

    #[test]
    fn its_size_is_unknown() {
        let (_dir, path, _) = media_file("progressive-size", 10);
        let (session, handle, _) = converted(path, 10);
        let reader = ProgressiveReader::for_test(session, handle, 0, thread_spawn(), None);
        assert_eq!(reader.size(), None);
    }

    #[test]
    fn the_start_of_an_encode_is_not_a_stall() {
        // 100 bytes appear per chunk read: the read at 100 waits, but it
        // is the encode's start, so it reports no buffering.
        let (_dir, path, _) = media_file("progressive-start", 10_000);
        let (session, handle, _) = converted(path, 100);
        let reader = ProgressiveReader::for_test(session, handle, 0, thread_spawn(), None);
        let _ = reader.read_reporting(0, 100, &mut |_| {}).unwrap();
        let mut reports = Vec::new();
        reader
            .read_reporting(100, 100, &mut |p| reports.push(p))
            .unwrap();
        assert_eq!(reports, Vec::<u8>::new());
    }

    #[test]
    fn a_low_bitrate_waits_for_seconds_of_it() {
        // 2 Mbit/s: four seconds is 1 MB, not 4 MiB.
        assert_eq!(resume_bytes(Some(2_000_000)), 1_000_000);
    }

    #[test]
    fn the_margin_stays_within_its_bounds() {
        assert_eq!(
            (
                resume_bytes(Some(64_000)),
                resume_bytes(Some(100_000_000)),
                resume_bytes(None)
            ),
            (MIN_RESUME_BYTES, RESUME_BYTES, RESUME_BYTES)
        );
    }
}
