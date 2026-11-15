//! Tracks kept from listening: a playback of a track writes the
//! bytes its reads bring into a part ([`PartSink`]), and once the whole
//! file is here it becomes an entry held by the cache (`listening`), with
//! no second transfer.
//!
//! What is kept is what was streamed: the file itself for a direct play
//! (range reads, in any order, as the player seeks), the conversion's
//! output for a progressive one (in order, from its start; a seek starts
//! a new encode and nothing is kept of it), or an HLS remux's segments as
//! the HLS bridge serves them to AVPlayer (the init segment, then every
//! media segment in order, one encode's: together a fragmented MP4 of
//! the whole track). A track skipped part-way, or an HLS play that skipped
//! a segment, leaves nothing: its part is removed with the playback. A
//! part is never gone on with after the client stops (the ranges are not
//! saved): the manager removes them all at its start.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use spoolframe_protocol::PlaybackPlan;
use spoolframe_protocol::downloads::{DownloadFile, DownloadSource, DownloadTracks, FileTracks};
use spoolframe_types::{Container, Delivery, Item, MediaId, MediaTracks};
use tokio::sync::mpsc;

use super::store::{EntryKind, Hold, Store, kept_key};
use super::writer::BlockWriter;
use super::{DownloadPhase, LocalWatch, Shared, manager, music, now};
use crate::{Chunk, PlaybackOptions, Session};

/// What a track's playback keeps of it.
pub(crate) struct Keeper {
    shared: Arc<Shared>,
    session: Session,
    id: MediaId,
    /// The encode began at the track's start: its output is the whole
    /// track.
    from_start: bool,
    /// How the playback is delivered, set once it is open ([`Keeper::attach`]).
    kind: OnceLock<Kind>,
    /// Given up on, or kept: nothing more is taken. Read without the lock,
    /// so a read that brings nothing to keep costs nothing.
    done: AtomicBool,
    /// How many media segments an HLS play's media playlist lists, once it
    /// was read; 0 before.
    segments: AtomicUsize,
    /// Where an HLS play's reads are handed, to be written off the read
    /// path ([`Keeper::offer_segment`]).
    feed: OnceLock<mpsc::Sender<Piece>>,
    state: tokio::sync::Mutex<State>,
}

/// How a playback is delivered, as the keeper takes its reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// The file as it is: range reads.
    Direct(Container),
    /// A conversion's output, streamed in order.
    Progressive(Container),
    /// The server's HLS: its segments, fragmented MP4.
    Hls,
    /// Nothing this keeps.
    Other,
}

impl Kind {
    fn of(plan: &PlaybackPlan) -> Self {
        match plan {
            PlaybackPlan::Direct { container, .. } => Kind::Direct(*container),
            PlaybackPlan::Converted {
                delivery: Delivery::Progressive,
                container,
                ..
            } => Kind::Progressive(*container),
            PlaybackPlan::Converted {
                delivery: Delivery::Hls,
                ..
            } => Kind::Hls,
            _ => Kind::Other,
        }
    }

    /// What the kept file is: the server's HLS segments are always
    /// fragmented MP4.
    fn container(self) -> Container {
        match self {
            Kind::Direct(container) | Kind::Progressive(container) => container,
            Kind::Hls | Kind::Other => Container::Mp4,
        }
    }
}

enum State {
    /// Nothing read yet.
    Idle,
    /// Writing the part; `order` says where each read goes.
    Writing { sink: Box<PartSink>, order: Order },
    /// Kept, here: reads are played from it.
    Kept(PathBuf),
    /// Given up on: nothing more is written.
    Over,
}

/// Where a play's reads go in its file.
enum Order {
    /// Direct reads, at their own offsets, in any order.
    Direct,
    /// A conversion's output from its start, in encode `epoch`.
    Progressive { epoch: u32 },
    /// HLS segments, in order from the init segment.
    Segments(SegmentOrder),
}

/// The part a play's bytes are written to, and how it ends: kept
/// ([`Keeper::keep`]), or removed.
struct PartSink {
    writer: BlockWriter,
    part: PathBuf,
    container: Container,
    /// The file as it is (a direct play), not a conversion's output.
    direct: bool,
}

impl PartSink {
    /// Remove the part.
    fn abort(&self) {
        let _ = std::fs::remove_file(&self.part);
    }
}

/// An HLS play's segments: those read in order are written straight
/// through, those read ahead of them held until their turn.
struct SegmentOrder {
    /// The encode (the playback's generation) and the rung read.
    generation: u64,
    variant: u32,
    /// The next segment to write (0 the init segment), and how much of it
    /// is written.
    next: u32,
    next_written: u64,
    /// Where the next segment starts in the file.
    start: u64,
    /// Segments past `next` read so far, and whether each is whole.
    ahead: BTreeMap<u32, (Vec<u8>, bool)>,
    /// The bytes in `ahead`.
    held: usize,
}

/// How much a read ahead asks for at once.
const AHEAD: u64 = 1 << 20;

/// How far past the next segment to write a segment may be read: a
/// player reads one or two ahead; one further on skipped some.
const SEGMENTS_AHEAD: u32 = 4;

/// The most bytes of segments held while an earlier one is read.
const PENDING_MAX: usize = 16 << 20;

/// How many reads of an HLS play wait to be written; past it the track is
/// not kept, rather than hold the player up.
const FEED_DEPTH: usize = 64;

/// What an HLS play read of a segment: `bytes` at `offset` of segment `n`
/// of rung `variant`, in the encode of `generation`; the segment is
/// `size` bytes so far, all of it once `complete`.
pub(crate) struct SegmentRead<'a> {
    pub generation: u64,
    pub variant: u32,
    pub n: u32,
    pub offset: u64,
    pub bytes: &'a [u8],
    pub size: u64,
    pub complete: bool,
}

/// A [`SegmentRead`], its bytes copied, on its way to the writer.
struct Piece {
    generation: u64,
    variant: u32,
    n: u32,
    offset: u64,
    bytes: Vec<u8>,
    size: u64,
    complete: bool,
}

/// What a segment read means for the file.
enum Step {
    /// Nothing to write: read already, or held.
    Nothing,
    /// Write these, in order, where the file has got to.
    Write(Vec<Vec<u8>>),
    /// Not the whole track in order: keep nothing.
    GiveUp,
}

/// What of `bytes`, read at `offset`, is past the `have` bytes here: a
/// read again from 0, or one that overlaps, brings the same bytes of the
/// same encode. `None` for a read that starts past them: a gap.
fn past(have: u64, offset: u64, bytes: &[u8]) -> Option<&[u8]> {
    let skip = usize::try_from(have.checked_sub(offset)?).ok()?;
    Some(bytes.get(skip..).unwrap_or_default())
}

impl SegmentOrder {
    fn new(generation: u64, variant: u32) -> Self {
        Self {
            generation,
            variant,
            next: 0,
            next_written: 0,
            start: 0,
            ahead: BTreeMap::new(),
            held: 0,
        }
    }

    /// Where the file has got to.
    fn at(&self) -> u64 {
        self.start + self.next_written
    }

    /// Take `piece`; a segment is whole once its `size` bytes are here
    /// and the encode had finished it.
    fn take(&mut self, piece: &Piece) -> Step {
        if piece.generation != self.generation || piece.variant != self.variant {
            return Step::GiveUp;
        }
        // Read again: written already.
        if piece.n < self.next {
            return Step::Nothing;
        }
        if piece.n > self.next + SEGMENTS_AHEAD {
            return Step::GiveUp;
        }
        let whole = |len: usize| piece.complete && len as u64 == piece.size;
        if piece.n > self.next {
            let (held, done) = self
                .ahead
                .entry(piece.n)
                .or_insert_with(|| (Vec::with_capacity(piece.size as usize), false));
            let Some(new) = past(held.len() as u64, piece.offset, &piece.bytes) else {
                return Step::GiveUp;
            };
            held.extend_from_slice(new);
            self.held += new.len();
            *done = whole(held.len());
            return if self.held > PENDING_MAX {
                Step::GiveUp
            } else {
                Step::Nothing
            };
        }
        let Some(new) = past(self.next_written, piece.offset, &piece.bytes) else {
            return Step::GiveUp;
        };
        let mut out = Vec::new();
        if !new.is_empty() {
            self.next_written += new.len() as u64;
            out.push(new.to_vec());
        }
        if whole(self.next_written as usize) {
            self.advance();
            // What was read ahead follows it: each whole one, then the
            // start of one still being read.
            while let Some((bytes, done)) = self.ahead.remove(&self.next) {
                self.held -= bytes.len();
                self.next_written = bytes.len() as u64;
                out.push(bytes);
                if !done {
                    break;
                }
                self.advance();
            }
        }
        if out.is_empty() {
            Step::Nothing
        } else {
            Step::Write(out)
        }
    }

    /// Segment `next` is written whole: on to the one after.
    fn advance(&mut self) {
        self.start += self.next_written;
        self.next += 1;
        self.next_written = 0;
    }
}

impl Keeper {
    /// A keeper of `id` played in `session` with `options`, when it is a
    /// track this device keeps: music downloads on, a downloads
    /// directory, and not here whole already, downloaded or kept (then
    /// only its last play is noted).
    pub(crate) async fn of(
        session: &Session,
        id: &MediaId,
        options: &PlaybackOptions,
    ) -> Option<Arc<Self>> {
        if id.kind() != spoolframe_types::MediaKind::Track {
            return None;
        }
        let shared = session.client.inner.downloads.clone();
        shared.store.as_ref()?;
        if shared.played(&session.server(), id).await {
            return None;
        }
        Some(Arc::new(Self {
            shared,
            session: session.clone(),
            id: id.clone(),
            from_start: options.start.is_none_or(|start| start.is_zero()),
            kind: OnceLock::new(),
            done: AtomicBool::new(false),
            segments: AtomicUsize::new(0),
            feed: OnceLock::new(),
            state: tokio::sync::Mutex::new(State::Idle),
        }))
    }

    /// The playback is open, planned as `plan`: what it keeps follows
    /// from it. An HLS play's reads are written by a task on `runtime`,
    /// which ends with the keeper.
    pub(crate) fn attach(self: &Arc<Self>, plan: &PlaybackPlan, runtime: &tokio::runtime::Handle) {
        let kind = Kind::of(plan);
        let _ = self.kind.set(kind);
        if kind == Kind::Other {
            self.done.store(true, Ordering::Relaxed);
        }
        if kind != Kind::Hls || !self.from_start {
            return;
        }
        let (send, mut receive) = mpsc::channel(FEED_DEPTH);
        let _ = self.feed.set(send);
        let keeper: Weak<Self> = Arc::downgrade(self);
        runtime.spawn(async move {
            while let Some(piece) = receive.recv().await {
                let Some(keeper) = keeper.upgrade() else {
                    return;
                };
                keeper.segment(piece).await;
            }
        });
    }

    /// The media playlist of an HLS play lists `n` segments.
    pub(crate) fn set_segments(&self, n: usize) {
        self.segments.store(n, Ordering::Relaxed);
    }

    fn kind(&self) -> Kind {
        self.kind.get().copied().unwrap_or(Kind::Other)
    }

    fn part(&self, store: &Store) -> PathBuf {
        let key = kept_key(&self.session.server(), &self.id);
        // One playback's part: two playbacks of the same track do not
        // write over each other.
        let mut nonce = [0u8; 8];
        getrandom::fill(&mut nonce).expect("the OS gives random bytes");
        let nonce = u64::from_le_bytes(nonce);
        store
            .listening_parts()
            .join(format!("{key}-{nonce:016x}.part"))
    }

    /// A direct read of the file of `size` bytes brought `bytes` at
    /// `offset`.
    pub(crate) async fn direct(&self, size: u64, offset: u64, bytes: &[u8]) {
        if self.done.load(Ordering::Relaxed) || !matches!(self.kind(), Kind::Direct(_)) {
            return;
        }
        let mut state = self.state.lock().await;
        if matches!(*state, State::Idle) {
            if size == 0 {
                return;
            }
            self.start(&mut state, size, Order::Direct).await;
        }
        let State::Writing { sink, .. } = &mut *state else {
            return;
        };
        let wrote = sink.writer.write_at(offset, bytes).await;
        self.wrote(&mut state, wrote).await;
    }

    /// A progressive read of `source` brought `chunk` at `offset`.
    pub(crate) async fn chunk(&self, source: &crate::Playback, offset: u64, chunk: &Chunk) {
        if self.done.load(Ordering::Relaxed) || !matches!(self.kind(), Kind::Progressive(_)) {
            return;
        }
        let mut state = self.state.lock().await;
        if matches!(*state, State::Idle) {
            // Only an encode from the track's start, read from its first
            // byte, is the whole track.
            if !(self.from_start && source.epoch == 0 && offset == 0) {
                self.give_up(&mut state);
                return;
            }
            let order = Order::Progressive { epoch: chunk.epoch };
            self.start(&mut state, u64::MAX, order).await;
        }
        let State::Writing {
            sink,
            order: Order::Progressive { epoch },
        } = &mut *state
        else {
            return;
        };
        // A seek: another encode, from elsewhere.
        if chunk.epoch != *epoch {
            self.give_up(&mut state);
            return;
        }
        let mut wrote = sink.writer.write_at(offset, &chunk.bytes).await;
        if wrote.is_ok() && chunk.complete {
            wrote = sink.writer.set_size(chunk.size);
        }
        self.wrote(&mut state, wrote).await;
    }

    /// An HLS play read part of a segment ([`SegmentRead`]): handed on to
    /// be written, off the read path. When too many wait, the track is
    /// not kept. Nothing is copied once the keeper is done, or for a play
    /// it does not keep.
    pub(crate) fn offer_segment(&self, read: SegmentRead<'_>) {
        if self.done.load(Ordering::Relaxed) {
            return;
        }
        let Some(feed) = self.feed.get() else {
            return;
        };
        let piece = Piece {
            generation: read.generation,
            variant: read.variant,
            n: read.n,
            offset: read.offset,
            bytes: read.bytes.to_vec(),
            size: read.size,
            complete: read.complete,
        };
        if feed.try_send(piece).is_err() {
            tracing::debug!(id = %self.id, "a track's segments came faster than kept: not kept");
            self.done.store(true, Ordering::Relaxed);
        }
    }

    /// Write an HLS play's `piece`. Kept once the init segment and every
    /// media segment the playlist lists are written, in order, from one
    /// encode; a segment skipped, a read from another encode or rung, or
    /// one from inside a segment not read up to there, keeps nothing.
    async fn segment(&self, piece: Piece) {
        let mut state = self.state.lock().await;
        if self.done.load(Ordering::Relaxed) {
            // Given up on by `offer_segment`: its part goes now.
            if matches!(*state, State::Writing { .. }) {
                self.give_up(&mut state);
            }
            return;
        }
        if matches!(*state, State::Idle) {
            let order = SegmentOrder::new(piece.generation, piece.variant);
            self.start(&mut state, u64::MAX, Order::Segments(order))
                .await;
        }
        let State::Writing {
            sink,
            order: Order::Segments(order),
        } = &mut *state
        else {
            return;
        };
        // Where the file has got to, before the piece moves it.
        let at = order.at();
        let mut wrote = match order.take(&piece) {
            Step::Nothing => return,
            Step::GiveUp => {
                self.give_up(&mut state);
                return;
            }
            Step::Write(blocks) => {
                let mut at = at;
                let mut wrote = Ok(());
                for bytes in blocks {
                    wrote = sink.writer.write_at(at, &bytes).await;
                    if wrote.is_err() {
                        break;
                    }
                    at += bytes.len() as u64;
                }
                wrote
            }
        };
        // The init segment and every media segment: the whole track.
        let segments = self.segments.load(Ordering::Relaxed);
        if wrote.is_ok() && segments > 0 && order.next as usize == segments + 1 {
            wrote = sink.writer.set_size(order.start);
        }
        self.wrote(&mut state, wrote).await;
    }

    /// `length` bytes at `offset` of a direct play's file, if they are
    /// here: read ahead, or the track kept whole. A read past the end is
    /// cut at it.
    pub(crate) async fn local(&self, size: u64, offset: u64, length: u64) -> Option<Vec<u8>> {
        let length = length.min(size.checked_sub(offset)?);
        if length == 0 {
            return None;
        }
        let path = match &*self.state.lock().await {
            State::Writing {
                sink,
                order: Order::Direct,
            } if sink.writer.has(offset, length) => sink.part.clone(),
            State::Kept(path) => path.clone(),
            _ => return None,
        };
        read_range(&path, offset, length).await.ok()
    }

    /// What a read ahead fetches next of a direct play's file: `None` once
    /// it is all here, kept, given up on, or not a direct play.
    pub(crate) async fn gap(&self) -> Option<(u64, u64)> {
        match &*self.state.lock().await {
            State::Writing {
                sink,
                order: Order::Direct,
            } => sink.writer.first_gap(AHEAD),
            _ => None,
        }
    }

    /// Open the part, `size` bytes (`u64::MAX` until known), to be
    /// written in `order`; given up on if it cannot be.
    async fn start(&self, state: &mut State, size: u64, order: Order) {
        let Some(store) = self.shared.store.as_ref() else {
            self.give_up(state);
            return;
        };
        let part = self.part(store);
        let direct = matches!(order, Order::Direct);
        let opened = async {
            tokio::fs::create_dir_all(store.listening_parts()).await?;
            let writer = BlockWriter::open(&part, size, 0).await?;
            // Only a direct play's part is read while it is written.
            Ok::<_, std::io::Error>(if direct { writer } else { writer.unflushed() })
        };
        match opened.await {
            Ok(writer) => {
                *state = State::Writing {
                    sink: Box::new(PartSink {
                        writer,
                        part,
                        container: self.kind().container(),
                        direct,
                    }),
                    order,
                };
            }
            Err(e) => {
                tracing::info!(error = %e, "a track is not kept from listening");
                self.give_up(state);
            }
        }
    }

    /// After a write: a failed one gives up; one that made the file whole
    /// keeps it.
    async fn wrote(&self, state: &mut State, wrote: std::io::Result<()>) {
        if let Err(e) = wrote {
            tracing::debug!(error = %e, "a track is not kept from listening");
            self.give_up(state);
            return;
        }
        if matches!(state, State::Writing { sink, .. } if sink.writer.is_complete()) {
            self.keep(state).await;
        }
    }

    fn give_up(&self, state: &mut State) {
        if let State::Writing { sink, .. } = &*state {
            sink.abort();
        }
        *state = State::Over;
        self.done.store(true, Ordering::Relaxed);
    }

    /// The whole file is here: it becomes an entry of the cache, with the
    /// track's details and artwork, and the cache is brought under its
    /// cap.
    async fn keep(&self, state: &mut State) {
        self.done.store(true, Ordering::Relaxed);
        let State::Writing { sink, .. } = std::mem::replace(state, State::Over) else {
            return;
        };
        let part = sink.part.clone();
        match self.kept(*sink).await {
            Ok(Some(media)) => *state = State::Kept(media),
            Ok(None) => {
                let _ = tokio::fs::remove_file(&part).await;
            }
            Err(e) => {
                tracing::info!(id = %self.id, error = %e, "a track played whole is not kept");
                let _ = tokio::fs::remove_file(&part).await;
            }
        }
    }

    async fn kept(&self, sink: PartSink) -> Result<Option<PathBuf>, crate::SdkError> {
        let PartSink {
            writer,
            part,
            container,
            direct,
        } = sink;
        let shared = &self.shared;
        let store = shared.store()?;
        let session = &self.session;
        let server = session.server();
        let user = session.user().clone();
        // Downloaded or kept meanwhile: that stands for it.
        if shared.played(&server, &self.id).await {
            return Ok(None);
        }
        let size = {
            let mut writer = writer;
            writer.sync().await.map_err(super::disk)?;
            std::fs::metadata(&part).map_err(super::disk)?.len()
        };
        let detail = session.item(&self.id).await?;
        let Item::Track(track) = &detail.item else {
            return Ok(None);
        };
        let (album, artist) = tokio::join!(
            session.item(&track.album_id),
            session.item(&track.artist_id)
        );
        let (album, artist) = (album.ok().map(|d| d.item), artist.ok().map(|d| d.item));
        let key = kept_key(&server, &self.id);
        let mut m = Store::manifest(
            EntryKind::Track,
            key.clone(),
            server,
            user.user_id,
            user.username,
            detail.item.clone(),
            MediaTracks::default(),
            // The file as it is when it played direct; a conversion's
            // output is of no quality a download asks for.
            if direct {
                DownloadSource::Original
            } else {
                DownloadSource::Unknown
            },
            DownloadTracks::All,
            LocalWatch::default(),
            now(),
        );
        m.holds = vec![Hold::listening()];
        m.album = album;
        m.artist = artist;
        m.file = Some(DownloadFile {
            container,
            tracks: FileTracks {
                audio: vec![0],
                subtitles: Vec::new(),
            },
        });
        m.size = Some(size);
        m.offset = size;
        // Not hashed: only a download of the same size needs it, and
        // `music::kept_is` hashes it then.
        m.phase = DownloadPhase::Done;
        m.last_played_at = Some(now());
        let mut art = shared.kept_art.lock().clone();
        m.artwork = manager::fetch_artwork(session, store, &m, &mut art).await;
        shared.kept_art.lock().extend(art);
        let media = store.media(&m).expect("a kept track has its container");
        {
            let _writing = shared.manifests.lock().await;
            std::fs::create_dir_all(store.dir(&m)).map_err(super::disk)?;
            std::fs::rename(&part, &media).map_err(super::disk)?;
            store.write(&m).map_err(super::disk)?;
        }
        tracing::debug!(key, size, "a track played whole is kept");
        shared.emit(&m);
        music::evict(shared, Some(&key)).await?;
        Ok(Some(media))
    }
}

/// `length` bytes of the file at `path` from `offset`.
async fn read_range(path: &std::path::Path, offset: u64, length: u64) -> std::io::Result<Vec<u8>> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut file = tokio::fs::File::open(path).await?;
    file.seek(std::io::SeekFrom::Start(offset)).await?;
    let mut bytes = vec![0; length as usize];
    file.read_exact(&mut bytes).await?;
    Ok(bytes)
}

impl Drop for Keeper {
    fn drop(&mut self) {
        // Skipped part-way: what was written goes.
        if let State::Writing { sink, .. } = &*self.state.get_mut() {
            sink.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn piece(n: u32, offset: u64, bytes: &[u8], size: u64) -> Piece {
        Piece {
            generation: 1,
            variant: 0,
            n,
            offset,
            bytes: bytes.to_vec(),
            size,
            complete: true,
        }
    }

    /// What `pieces` write, in order, or `None` once given up.
    fn written(pieces: &[Piece]) -> Option<Vec<u8>> {
        let mut order = SegmentOrder::new(1, 0);
        let mut file = Vec::new();
        for p in pieces {
            match order.take(p) {
                Step::Nothing => {}
                Step::Write(blocks) => blocks.into_iter().for_each(|b| file.extend(b)),
                Step::GiveUp => return None,
            }
        }
        Some(file)
    }

    #[test]
    fn in_order_reads_are_written_straight_through() {
        let pieces = [
            piece(0, 0, b"in", 4),
            piece(0, 2, b"it", 4),
            piece(1, 0, b"aa", 2),
        ];
        assert_eq!(written(&pieces).as_deref(), Some(&b"initaa"[..]));
    }

    #[test]
    fn a_segment_read_ahead_follows_the_one_before_it() {
        let pieces = [
            piece(0, 0, b"init", 4),
            piece(2, 0, b"bb", 2),
            piece(3, 0, b"c", 2),
            piece(1, 0, b"aa", 2),
            piece(3, 1, b"c", 2),
        ];
        assert_eq!(written(&pieces).as_deref(), Some(&b"initaabbcc"[..]));
    }

    #[test]
    fn a_read_again_or_overlapping_adds_only_what_is_new() {
        let pieces = [
            piece(0, 0, b"ini", 4),
            piece(0, 0, b"init", 4),
            piece(0, 0, b"init", 4),
        ];
        assert_eq!(written(&pieces).as_deref(), Some(&b"init"[..]));
    }

    #[test]
    fn a_skip_a_gap_or_another_encode_gives_up() {
        let far = [
            piece(0, 0, b"init", 4),
            piece(SEGMENTS_AHEAD + 2, 0, b"x", 1),
        ];
        let gap = [piece(0, 0, b"in", 4), piece(0, 3, b"t", 4)];
        let mut other = piece(1, 0, b"aa", 2);
        other.generation = 2;
        let encode = [piece(0, 0, b"init", 4), other];
        assert_eq!(
            (written(&far), written(&gap), written(&encode)),
            (None, None, None)
        );
    }
}
