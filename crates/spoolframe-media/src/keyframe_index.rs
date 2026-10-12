//! A file's keyframes from its own index (PX-25), read without reading
//! the file: Matroska's `Cues` and MP4's sync sample table (`stss`).
//!
//! The full list is ffprobe over every packet, which reads all of a file:
//! on a large remux, tens of seconds of disk before an HLS playback of a
//! copied video can start. The index is a few KiB to a few MiB at a known
//! place. What it says is only a claim (a muxer may index some keyframes,
//! or none), so [`crate::Prober::keyframes`] checks it against what
//! ffprobe reads around a few of its entries before it is used.
//!
//! The times come out as [`crate::Prober`]'s full scan gives them: the
//! main (first) video track's keyframes, in ms, ascending, one before 0
//! as 0, the same one-dedup rules.
//!
//! The file is untrusted, and read in the server's own process. The parser
//! reads at most [`MAX_READ`] bytes in all and [`MAX_ELEMENT`] at once,
//! walks MP4's sample tables where they lie in what it read, allocates
//! nothing from a size or count the file states, keeps at most
//! [`MAX_KEYFRAMES`] times, and never recurses on the file's nesting: at
//! most 48 MiB read and 32 MiB of times held, per file.
//! Anything it does not follow is [`NoIndex`]: the caller falls back to
//! the full scan.

use std::io::{Read, Seek, SeekFrom};

/// The most bytes read from a file in all.
pub const MAX_READ: u64 = 48 << 20;

/// The most bytes of one element or box read into memory: a two-hour
/// film's `moov` is one to a few MiB, a four-hour 60 fps one's about 12.
pub const MAX_ELEMENT: u64 = 32 << 20;

/// The most keyframes kept: as many as the full scan's listing holds
/// (about 4 million packets).
pub const MAX_KEYFRAMES: usize = 4 << 20;

/// The most top-level elements or boxes walked past.
const MAX_ELEMENTS: usize = 4096;

/// Which index the keyframes came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexKind {
    /// Matroska `Cues`.
    Cues,
    /// MP4 `stss` (or no `stss`: every sample is a keyframe).
    SyncSamples,
}

/// The keyframes an index lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Indexed {
    pub kind: IndexKind,
    /// ms, ascending, deduplicated; one before 0 as 0.
    pub times_ms: Vec<u64>,
    /// The first keyframe ffmpeg reads, its own time in ms: the last at or
    /// before 0, which is before 0 where an edit list starts the track
    /// after it (an MP4 cut with stream copy), though `times_ms` has it at
    /// 0; any before it ffmpeg leaves out.
    pub first_ms: i64,
}

/// Why a file's index cannot be used. None is an error of the file's:
/// the full scan reads it instead.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NoIndex {
    #[error("neither Matroska nor MP4")]
    Format,
    #[error("the file ends inside {0}")]
    Truncated(&'static str),
    #[error("{0} is malformed")]
    Malformed(&'static str),
    #[error("{0} is larger than the index reader takes")]
    TooLarge(&'static str),
    #[error("no video track")]
    NoVideo,
    #[error("no Cues for the video track")]
    NoCues,
    #[error("fragmented MP4: its keyframes are in every fragment")]
    Fragmented,
    #[error("an edit list the index reader does not follow")]
    EditList,
    #[error("keyframes marked other than by stss (stps or a sample group)")]
    OtherSyncMarks,
    #[error("more than {MAX_KEYFRAMES} keyframes")]
    TooMany,
    #[error("reading the file: {0}")]
    Io(String),
}

/// The keyframes `file`'s index lists, reading only the index. `len` is
/// the file's size.
pub fn read<R: Read + Seek>(file: &mut R, len: u64) -> Result<Indexed, NoIndex> {
    let mut file = Bounded {
        inner: file,
        len,
        read: 0,
    };
    let head = file.at(0, 12.min(len), "the file's head")?;
    if head.starts_with(&EBML_MAGIC) {
        matroska(&mut file)
    } else if head.len() >= 8 && MP4_FIRST.contains(&&head[4..8]) {
        mp4(&mut file)
    } else {
        Err(NoIndex::Format)
    }
}

/// `seconds` as the full scan gives a time: in ms, one before 0 as 0.
pub fn ms(seconds: f64) -> u64 {
    (seconds.max(0.0) * 1000.0).round() as u64
}

/// `seconds` in ms, rounded as [`ms`] rounds it, before 0 kept.
pub fn signed_ms(seconds: f64) -> i64 {
    (seconds * 1000.0).round() as i64
}

/// `times` sorted and deduplicated; none is no index. `start`: the last
/// keyframe at or before 0, in s, if any: where ffmpeg starts the track.
fn finish(kind: IndexKind, mut times_ms: Vec<u64>, start: Option<f64>) -> Result<Indexed, NoIndex> {
    times_ms.sort_unstable();
    times_ms.dedup();
    if times_ms.is_empty() {
        return Err(match kind {
            IndexKind::Cues => NoIndex::NoCues,
            IndexKind::SyncSamples => NoIndex::Malformed("stss"),
        });
    }
    let first_ms = start.map_or(times_ms[0] as i64, signed_ms);
    Ok(Indexed {
        kind,
        times_ms,
        first_ms,
    })
}

/// The file, read only within its length and [`MAX_READ`] in all.
struct Bounded<'a, R> {
    inner: &'a mut R,
    len: u64,
    read: u64,
}

impl<R: Read + Seek> Bounded<'_, R> {
    /// `n` bytes at `pos`, or [`NoIndex::Truncated`] if the file ends
    /// first.
    fn at(&mut self, pos: u64, n: u64, what: &'static str) -> Result<Vec<u8>, NoIndex> {
        if n > MAX_ELEMENT {
            return Err(NoIndex::TooLarge(what));
        }
        if pos.checked_add(n).is_none_or(|end| end > self.len) {
            return Err(NoIndex::Truncated(what));
        }
        self.read += n;
        if self.read > MAX_READ {
            return Err(NoIndex::TooLarge("the index, in all,"));
        }
        let io = |e: std::io::Error| NoIndex::Io(e.to_string());
        self.inner.seek(SeekFrom::Start(pos)).map_err(io)?;
        // `n` is within MAX_ELEMENT and the file's length.
        let mut bytes = vec![0; n as usize];
        self.inner
            .read_exact(&mut bytes)
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::UnexpectedEof => NoIndex::Truncated(what),
                _ => io(e),
            })?;
        Ok(bytes)
    }

    /// Up to `n` bytes at `pos`: fewer at the end of the file.
    fn up_to(&mut self, pos: u64, n: u64, what: &'static str) -> Result<Vec<u8>, NoIndex> {
        let n = n.min(self.len.saturating_sub(pos));
        self.at(pos, n, what)
    }
}

// Matroska.

const EBML_MAGIC: [u8; 4] = [0x1A, 0x45, 0xDF, 0xA3];
const EBML: u32 = 0x1A45_DFA3;
const DOC_TYPE: u32 = 0x4282;
const SEGMENT: u32 = 0x1853_8067;
const SEEK_HEAD: u32 = 0x114D_9B74;
const SEEK: u32 = 0x4DBB;
const SEEK_ID: u32 = 0x53AB;
const SEEK_POSITION: u32 = 0x53AC;
const INFO: u32 = 0x1549_A966;
const TIMESTAMP_SCALE: u32 = 0x2A_D7B1;
const TRACKS: u32 = 0x1654_AE6B;
const TRACK_ENTRY: u32 = 0xAE;
const TRACK_NUMBER: u32 = 0xD7;
const TRACK_TYPE: u32 = 0x83;
const CUES: u32 = 0x1C53_BB6B;
const CUE_POINT: u32 = 0xBB;
const CUE_TIME: u32 = 0xB3;
const CUE_TRACK_POSITIONS: u32 = 0xB7;
const CUE_TRACK: u32 = 0xF7;
const CLUSTER: u32 = 0x1F43_B675;

/// An element's header: its ID (marker kept, as the spec writes IDs), its
/// data's size (`None`: unknown), and the header's length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Header {
    id: u32,
    size: Option<u64>,
    len: u64,
}

/// A variable-length integer's length from its first byte: 1 to 8.
fn vint_len(first: u8) -> Option<u64> {
    (first != 0).then(|| u64::from(first.leading_zeros()) + 1)
}

/// The header at the start of `bytes`, if all of it is there.
fn header(bytes: &[u8]) -> Option<Header> {
    let id_len = vint_len(*bytes.first()?)?;
    if id_len > 4 {
        return None;
    }
    let id_bytes = bytes.get(..id_len as usize)?;
    let id = id_bytes
        .iter()
        .fold(0u32, |id, &b| (id << 8) | u32::from(b));
    let rest = &bytes[id_len as usize..];
    let size_len = vint_len(*rest.first()?)?;
    let size_bytes = rest.get(..size_len as usize)?;
    // The marker bit cleared.
    let mut size = u64::from(size_bytes[0]) & (0xFF >> size_len);
    for &b in &size_bytes[1..] {
        size = (size << 8) | u64::from(b);
    }
    // All ones: unknown.
    let unknown = size == (1u64 << (7 * size_len)) - 1;
    Some(Header {
        id,
        size: (!unknown).then_some(size),
        len: id_len + size_len,
    })
}

/// The header at `pos` in the file.
fn header_at<R: Read + Seek>(
    file: &mut Bounded<'_, R>,
    pos: u64,
    what: &'static str,
) -> Result<Header, NoIndex> {
    let bytes = file.up_to(pos, 12, what)?;
    header(&bytes).ok_or(NoIndex::Malformed(what))
}

/// The children of an element's data held in memory: each child's
/// header and data. One of unknown size, or running past the data, ends
/// the walk as malformed.
fn children<'a>(
    mut data: &'a [u8],
    what: &'static str,
) -> impl Iterator<Item = Result<(u32, &'a [u8]), NoIndex>> + 'a {
    std::iter::from_fn(move || {
        if data.is_empty() {
            return None;
        }
        let found = header(data).and_then(|h| {
            let start = usize::try_from(h.len).ok()?;
            let end = start.checked_add(usize::try_from(h.size?).ok()?)?;
            let body = data.get(start..end)?;
            data = &data[end..];
            Some((h.id, body))
        });
        if found.is_none() {
            data = &[];
        }
        Some(found.ok_or(NoIndex::Malformed(what)))
    })
}

/// A big-endian unsigned integer of up to 8 bytes.
fn uint(bytes: &[u8], what: &'static str) -> Result<u64, NoIndex> {
    if bytes.len() > 8 {
        return Err(NoIndex::Malformed(what));
    }
    Ok(bytes.iter().fold(0, |n, &b| (n << 8) | u64::from(b)))
}

/// Where the parts read are, as found.
#[derive(Default)]
struct Parts {
    info: Option<u64>,
    tracks: Option<u64>,
    cues: Option<u64>,
}

fn matroska<R: Read + Seek>(file: &mut Bounded<'_, R>) -> Result<Indexed, NoIndex> {
    let ebml = header_at(file, 0, "the EBML header")?;
    let ebml_size = ebml.size.ok_or(NoIndex::Malformed("the EBML header"))?;
    if ebml.id != EBML || ebml_size > 4096 {
        return Err(NoIndex::Malformed("the EBML header"));
    }
    let body = file.at(ebml.len, ebml_size, "the EBML header")?;
    let mut doc_type_ok = false;
    for child in children(&body, "the EBML header") {
        let (id, data) = child?;
        if id == DOC_TYPE {
            doc_type_ok = matches!(data, b"matroska" | b"webm");
        }
    }
    if !doc_type_ok {
        return Err(NoIndex::Format);
    }

    let segment_at = ebml.len + ebml_size;
    let segment = header_at(file, segment_at, "the Segment")?;
    if segment.id != SEGMENT {
        return Err(NoIndex::Malformed("the Segment"));
    }
    // Positions in a SeekHead count from here.
    let data = segment_at + segment.len;
    let end = match segment.size {
        Some(size) => data.saturating_add(size).min(file.len),
        None => file.len,
    };

    // The top-level elements up to the first Cluster: where the muxer
    // puts the SeekHead, Info and Tracks, and sometimes the Cues.
    let mut found = Parts::default();
    let mut seek_heads = Vec::new();
    let mut pos = data;
    for _ in 0..MAX_ELEMENTS {
        if pos >= end {
            break;
        }
        let h = header_at(file, pos, "a top-level element")?;
        match h.id {
            CLUSTER => break,
            SEEK_HEAD => seek_heads.push(pos),
            INFO => found.info = Some(pos),
            TRACKS => found.tracks = Some(pos),
            CUES => found.cues = Some(pos),
            _ => {}
        }
        let size = h.size.ok_or(NoIndex::Malformed("a top-level element"))?;
        pos = pos.saturating_add(h.len).saturating_add(size);
    }
    // The rest from the SeekHead: the Cues are usually after the
    // Clusters. One SeekHead may point at a second.
    let mut read_heads = 0;
    while let Some(at) = seek_heads.pop() {
        read_heads += 1;
        if read_heads > 2 {
            break;
        }
        let body = element(file, at, SEEK_HEAD, "the SeekHead")?;
        for seek in children(&body, "the SeekHead") {
            let (id, seek) = seek?;
            if id != SEEK {
                continue;
            }
            let (mut target, mut position) = (None, None);
            for field in children(seek, "a Seek") {
                match field? {
                    (SEEK_ID, bytes) => target = Some(uint(bytes, "a SeekID")?),
                    (SEEK_POSITION, bytes) => position = Some(uint(bytes, "a SeekPosition")?),
                    _ => {}
                }
            }
            let (Some(target), Some(position)) = (target, position) else {
                continue;
            };
            let at = data.saturating_add(position);
            let slot = match u32::try_from(target) {
                Ok(INFO) => &mut found.info,
                Ok(TRACKS) => &mut found.tracks,
                Ok(CUES) => &mut found.cues,
                Ok(SEEK_HEAD) if at != data && !seek_heads.contains(&at) => {
                    seek_heads.push(at);
                    continue;
                }
                _ => continue,
            };
            slot.get_or_insert(at);
        }
    }

    let scale = match found.info {
        Some(at) => {
            let body = element(file, at, INFO, "the Info")?;
            let mut scale = 1_000_000;
            for child in children(&body, "the Info") {
                if let (TIMESTAMP_SCALE, bytes) = child? {
                    scale = uint(bytes, "the TimestampScale")?;
                }
            }
            scale
        }
        None => 1_000_000,
    };
    if scale == 0 {
        return Err(NoIndex::Malformed("the TimestampScale"));
    }
    let tracks_at = found.tracks.ok_or(NoIndex::NoVideo)?;
    let video = first_video_track(&element(file, tracks_at, TRACKS, "the Tracks")?)?;
    let cues_at = found.cues.ok_or(NoIndex::NoCues)?;
    let cues = element(file, cues_at, CUES, "the Cues")?;

    let mut times = Vec::new();
    for point in children(&cues, "the Cues") {
        let (id, point) = point?;
        if id != CUE_POINT {
            continue;
        }
        let mut time = None;
        let mut ours = false;
        for field in children(point, "a CuePoint") {
            match field? {
                (CUE_TIME, bytes) => time = Some(uint(bytes, "a CueTime")?),
                (CUE_TRACK_POSITIONS, positions) => {
                    for position in children(positions, "a CueTrackPositions") {
                        if let (CUE_TRACK, bytes) = position? {
                            ours |= uint(bytes, "a CueTrack")? == video;
                        }
                    }
                }
                _ => {}
            }
        }
        if let (Some(time), true) = (time, ours) {
            if times.len() == MAX_KEYFRAMES {
                return Err(NoIndex::TooMany);
            }
            // As ffmpeg times a block: TimestampScale ns per tick.
            times.push(ms(time as f64 * scale as f64 / 1e9));
        }
    }
    // A CueTime is unsigned: none is before 0.
    finish(IndexKind::Cues, times, None)
}

/// The data of the element at `pos`, which must be an `id`.
fn element<R: Read + Seek>(
    file: &mut Bounded<'_, R>,
    pos: u64,
    id: u32,
    what: &'static str,
) -> Result<Vec<u8>, NoIndex> {
    let h = header_at(file, pos, what)?;
    if h.id != id {
        return Err(NoIndex::Malformed(what));
    }
    let size = h.size.ok_or(NoIndex::Malformed(what))?;
    file.at(pos + h.len, size, what)
}

/// The first video track's number: the stream ffprobe's `v:0` is.
fn first_video_track(tracks: &[u8]) -> Result<u64, NoIndex> {
    for entry in children(tracks, "the Tracks") {
        let (id, entry) = entry?;
        if id != TRACK_ENTRY {
            continue;
        }
        let (mut number, mut kind) = (None, None);
        for field in children(entry, "a TrackEntry") {
            match field? {
                (TRACK_NUMBER, bytes) => number = Some(uint(bytes, "a TrackNumber")?),
                (TRACK_TYPE, bytes) => kind = Some(uint(bytes, "a TrackType")?),
                _ => {}
            }
        }
        if let (Some(number), Some(1)) = (number, kind) {
            return Ok(number);
        }
    }
    Err(NoIndex::NoVideo)
}

// MP4.

/// What an MP4 file starts with.
const MP4_FIRST: [&[u8]; 6] = [b"ftyp", b"moov", b"free", b"skip", b"wide", b"mdat"];

/// A box's type, its data, as found in memory.
fn boxes<'a>(
    mut data: &'a [u8],
    what: &'static str,
) -> impl Iterator<Item = Result<([u8; 4], &'a [u8]), NoIndex>> + 'a {
    std::iter::from_fn(move || {
        if data.is_empty() {
            return None;
        }
        let found = (|| {
            let size = u32::from_be_bytes(data.get(..4)?.try_into().ok()?);
            let kind: [u8; 4] = data.get(4..8)?.try_into().ok()?;
            let (start, size) = match size {
                0 => (8, data.len()),
                1 => (
                    16,
                    usize::try_from(u64::from_be_bytes(data.get(8..16)?.try_into().ok()?)).ok()?,
                ),
                n => (8, n as usize),
            };
            let body = data.get(start..size)?;
            data = &data[size..];
            Some((kind, body))
        })();
        if found.is_none() {
            data = &[];
        }
        Some(found.ok_or(NoIndex::Malformed(what)))
    })
}

/// The first box of `kind` in `data`.
fn find<'a>(
    data: &'a [u8],
    kind: &[u8; 4],
    what: &'static str,
) -> Result<Option<&'a [u8]>, NoIndex> {
    for b in boxes(data, what) {
        let (k, body) = b?;
        if &k == kind {
            return Ok(Some(body));
        }
    }
    Ok(None)
}

/// A big-endian reader over a box's data.
struct Cursor<'a> {
    data: &'a [u8],
    what: &'static str,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8], what: &'static str) -> Self {
        Self { data, what }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], NoIndex> {
        if self.data.len() < n {
            return Err(NoIndex::Malformed(self.what));
        }
        let (head, rest) = self.data.split_at(n);
        self.data = rest;
        Ok(head)
    }

    fn u32(&mut self) -> Result<u32, NoIndex> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }

    fn u64(&mut self) -> Result<u64, NoIndex> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }

    /// A full box's version, its flags skipped.
    fn version(&mut self) -> Result<u8, NoIndex> {
        Ok(self.take(4)?[0])
    }

    /// An entry count, and that the rest holds that many `size`-byte
    /// entries: nothing is allocated from a count the data cannot hold.
    fn entries(&mut self, size: usize) -> Result<usize, NoIndex> {
        let count = self.u32()? as usize;
        if count.checked_mul(size).is_none_or(|n| n > self.data.len()) {
            return Err(NoIndex::Malformed(self.what));
        }
        Ok(count)
    }
}

/// A sample table's entries of `size` bytes each, after its version,
/// flags and count, and its version. The count must fit the box.
fn table<'a>(
    body: &'a [u8],
    size: usize,
    what: &'static str,
) -> Result<(std::slice::ChunksExact<'a, u8>, u8), NoIndex> {
    let mut c = Cursor::new(body, what);
    let version = c.version()?;
    let n = c.entries(size)?;
    Ok((c.data[..n * size].chunks_exact(size), version))
}

/// A big-endian `u32` from the first four bytes of `bytes`.
fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes[..4].try_into().expect("4 bytes"))
}

fn mp4<R: Read + Seek>(file: &mut Bounded<'_, R>) -> Result<Indexed, NoIndex> {
    let mut pos = 0;
    let mut moov = None;
    for _ in 0..MAX_ELEMENTS {
        if pos >= file.len {
            break;
        }
        let head = file.up_to(pos, 16, "a top-level box")?;
        if head.len() < 8 {
            return Err(NoIndex::Truncated("a top-level box"));
        }
        let size = u64::from(u32::from_be_bytes(head[..4].try_into().expect("4 bytes")));
        let (header, size) = match size {
            0 => (8, file.len - pos),
            1 if head.len() == 16 => (16, u64::from_be_bytes(head[8..16].try_into().expect("8"))),
            n if n >= 8 => (8, n),
            _ => return Err(NoIndex::Malformed("a top-level box")),
        };
        if size < header {
            return Err(NoIndex::Malformed("a top-level box"));
        }
        match &head[4..8] {
            b"moov" => {
                moov = Some(file.at(pos + header, size - header, "the moov box")?);
                break;
            }
            // Fragments before any moov: not a file the index reader takes.
            b"moof" => return Err(NoIndex::Fragmented),
            _ => {}
        }
        pos = pos.saturating_add(size);
    }
    let moov = moov.ok_or(NoIndex::Malformed("the file: no moov box"))?;
    if find(&moov, b"mvex", "the moov box")?.is_some() {
        return Err(NoIndex::Fragmented);
    }
    let movie_scale = match find(&moov, b"mvhd", "the moov box")? {
        Some(mvhd) => {
            let mut c = Cursor::new(mvhd, "the mvhd box");
            match c.version()? {
                1 => c.take(16)?,
                _ => c.take(8)?,
            };
            c.u32()?
        }
        None => return Err(NoIndex::Malformed("the moov box: no mvhd")),
    };
    for b in boxes(&moov, "the moov box") {
        let (kind, trak) = b?;
        if &kind != b"trak" {
            continue;
        }
        let mdia = find(trak, b"mdia", "a trak box")?.ok_or(NoIndex::Malformed("a trak box"))?;
        let hdlr = find(mdia, b"hdlr", "an mdia box")?.ok_or(NoIndex::Malformed("an mdia box"))?;
        let mut c = Cursor::new(hdlr, "an hdlr box");
        c.version()?;
        c.take(4)?;
        if c.take(4)? != b"vide" {
            continue;
        }
        return video_track(trak, mdia, movie_scale);
    }
    Err(NoIndex::NoVideo)
}

/// A video track's sync samples' presentation times.
fn video_track(trak: &[u8], mdia: &[u8], movie_scale: u32) -> Result<Indexed, NoIndex> {
    let mdhd = find(mdia, b"mdhd", "an mdia box")?.ok_or(NoIndex::Malformed("an mdia box"))?;
    let mut c = Cursor::new(mdhd, "the mdhd box");
    match c.version()? {
        1 => c.take(16)?,
        _ => c.take(8)?,
    };
    let scale = c.u32()?;
    if scale == 0 || movie_scale == 0 {
        return Err(NoIndex::Malformed("a timescale"));
    }
    let edit = edit(trak, scale, movie_scale)?;

    let minf = find(mdia, b"minf", "an mdia box")?.ok_or(NoIndex::Malformed("an mdia box"))?;
    let stbl = find(minf, b"stbl", "a minf box")?.ok_or(NoIndex::Malformed("a minf box"))?;
    let mut stts = None;
    let mut ctts = None;
    let mut stss = None;
    for b in boxes(stbl, "the stbl box") {
        match b? {
            (kind, body) if &kind == b"stts" => stts = Some(body),
            (kind, body) if &kind == b"ctts" => ctts = Some(body),
            (kind, body) if &kind == b"stss" => stss = Some(body),
            // ffmpeg also takes partial sync samples, and samples a `rap `
            // or `sync` group marks, as keyframes.
            (kind, _) if &kind == b"stps" => return Err(NoIndex::OtherSyncMarks),
            (kind, body) if &kind == b"sbgp" => {
                let mut c = Cursor::new(body, "an sbgp box");
                c.version()?;
                if matches!(c.take(4)?, b"rap " | b"sync") {
                    return Err(NoIndex::OtherSyncMarks);
                }
            }
            _ => {}
        }
    }
    let stts = stts.ok_or(NoIndex::Malformed("the stbl box: no stts"))?;

    // The tables are walked where they are, in the moov box's bytes:
    // nothing is allocated for them.
    let decode = table(stts, 8, "the stts box")?.0;
    let samples: u64 = decode.clone().map(|run| u64::from(be32(&run[..4]))).sum();
    // Composition offsets: runs of (count, offset), signed in version 1.
    let (mut composition, signed) = match ctts {
        Some(ctts) => {
            let (runs, version) = table(ctts, 8, "the ctts box")?;
            (Some(runs), version == 1)
        }
        None => (None, false),
    };
    // Sync samples, numbered from 1, ascending; none listed means every
    // one.
    let sync: Box<dyn Iterator<Item = u64>> = match stss {
        Some(stss) => Box::new(
            table(stss, 4, "the stss box")?
                .0
                .map(|n| u64::from(be32(n))),
        ),
        None if samples > MAX_KEYFRAMES as u64 => return Err(NoIndex::TooMany),
        None => Box::new(1..=samples),
    };

    // One pass over the runs, the sync samples in order.
    let mut times = Vec::new();
    // The last keyframe at or before 0, s: where ffmpeg starts the track,
    // leaving out any before it.
    let mut start: Option<f64> = None;
    let mut decode = decode.map(|run| (be32(&run[..4]), be32(&run[4..])));
    let (mut d_run, mut d_first, mut d_base) = (decode.next(), 1u64, 0i64);
    let mut c_run = composition.as_mut().and_then(Iterator::next);
    let mut c_first = 1u64;
    let mut previous = 0;
    for sample in sync {
        if sample <= previous || sample > samples {
            return Err(NoIndex::Malformed("the stss box"));
        }
        previous = sample;
        if times.len() == MAX_KEYFRAMES {
            return Err(NoIndex::TooMany);
        }
        // The decode run holding `sample`, and its first sample's time.
        while let Some((count, delta)) = d_run {
            if sample < d_first + u64::from(count) {
                break;
            }
            d_base = d_base.saturating_add(i64::from(count).saturating_mul(i64::from(delta)));
            d_first += u64::from(count);
            d_run = decode.next();
        }
        let (_, delta) = d_run.ok_or(NoIndex::Malformed("the stts box"))?;
        let into_run = ((sample - d_first) as i64).saturating_mul(i64::from(delta));
        let dts = d_base.saturating_add(into_run);
        let mut offset = 0;
        while let Some(run) = c_run {
            let count = u64::from(be32(&run[..4]));
            if sample < c_first + count {
                let raw = be32(&run[4..]);
                offset = if signed {
                    i64::from(raw as i32)
                } else {
                    i64::from(raw)
                };
                break;
            }
            c_first += count;
            c_run = composition.as_mut().and_then(Iterator::next);
        }
        let pts = dts.saturating_add(offset);
        let in_edit = pts.saturating_sub(edit.media_time);
        if edit.end.is_some_and(|end| in_edit >= end) {
            // Past the edit: ffmpeg leaves it out.
            continue;
        }
        let shown = in_edit as f64 / f64::from(scale) + edit.delay;
        if shown <= 0.0 {
            start = Some(start.map_or(shown, |s| s.max(shown)));
        }
        times.push(ms(shown));
    }
    finish(IndexKind::SyncSamples, times, start)
}

/// What a track's edit list does to its times, as ffmpeg applies it.
struct Edit {
    /// Media time shown at the edit's start, in the track's timescale.
    media_time: i64,
    /// An empty edit first: how late the media starts, s.
    delay: f64,
    /// How much of the media is shown, in the track's timescale; `None`
    /// to its end.
    end: Option<i64>,
}

/// The track's edit list: none, one edit at rate 1, or an empty edit and
/// one at rate 1. Anything else is [`NoIndex::EditList`].
fn edit(trak: &[u8], scale: u32, movie_scale: u32) -> Result<Edit, NoIndex> {
    let none = Edit {
        media_time: 0,
        delay: 0.0,
        end: None,
    };
    let Some(edts) = find(trak, b"edts", "a trak box")? else {
        return Ok(none);
    };
    let Some(elst) = find(edts, b"elst", "an edts box")? else {
        return Ok(none);
    };
    let mut c = Cursor::new(elst, "the elst box");
    let long = c.version()? == 1;
    let n = c.entries(if long { 20 } else { 12 })?;
    let mut entries = Vec::with_capacity(n.min(2));
    for _ in 0..n {
        let (duration, media_time) = if long {
            (c.u64()?, c.u64()? as i64)
        } else {
            (u64::from(c.u32()?), i64::from(c.u32()? as i32))
        };
        let rate = c.u32()?;
        if rate != 0x0001_0000 {
            return Err(NoIndex::EditList);
        }
        entries.push((duration, media_time));
        if entries.len() > 2 {
            return Err(NoIndex::EditList);
        }
    }
    let in_track = |movie: u64| -> i64 {
        (u128::from(movie) * u128::from(scale) / u128::from(movie_scale)).min(i64::MAX as u128)
            as i64
    };
    match entries[..] {
        [] => Ok(none),
        [(duration, media_time)] if media_time >= 0 => Ok(Edit {
            media_time,
            delay: 0.0,
            end: (duration > 0).then(|| in_track(duration)),
        }),
        [(empty, -1), (duration, media_time)] if media_time >= 0 => Ok(Edit {
            media_time,
            delay: empty as f64 / f64::from(movie_scale),
            end: (duration > 0).then(|| in_track(duration)),
        }),
        _ => Err(NoIndex::EditList),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_bytes(bytes: &[u8]) -> Result<Indexed, NoIndex> {
        read(&mut std::io::Cursor::new(bytes), bytes.len() as u64)
    }

    /// An EBML element: a one-to-four-byte ID, a size, the data.
    fn el(id: u32, data: &[u8]) -> Vec<u8> {
        let mut out: Vec<u8> = id
            .to_be_bytes()
            .into_iter()
            .skip_while(|&b| b == 0)
            .collect();
        out.push(0x01); // an 8-byte size
        out.extend_from_slice(&(data.len() as u64).to_be_bytes()[1..]);
        out.extend_from_slice(data);
        out
    }

    fn uint_el(id: u32, n: u64) -> Vec<u8> {
        el(id, &n.to_be_bytes())
    }

    fn cue(ms: u64, track: u64) -> Vec<u8> {
        let positions = uint_el(CUE_TRACK, track);
        el(
            CUE_POINT,
            &[uint_el(CUE_TIME, ms), el(CUE_TRACK_POSITIONS, &positions)].concat(),
        )
    }

    /// A Matroska file: Tracks (audio 1, video 2), then a Cluster, then
    /// `cues`, found through a SeekHead.
    fn matroska_with(cues: Option<Vec<u8>>) -> Vec<u8> {
        let ebml = el(EBML, &el(DOC_TYPE, b"matroska"));
        let audio = el(
            TRACK_ENTRY,
            &[uint_el(TRACK_NUMBER, 1), uint_el(TRACK_TYPE, 2)].concat(),
        );
        let video = el(
            TRACK_ENTRY,
            &[uint_el(TRACK_NUMBER, 2), uint_el(TRACK_TYPE, 1)].concat(),
        );
        let tracks = el(TRACKS, &[audio, video].concat());
        let cluster = el(CLUSTER, &[0; 64]);
        // SeekHead's own size is fixed: 1 Seek of two 8-byte uints.
        let seek_head_len = el(
            SEEK_HEAD,
            &el(
                SEEK,
                &[uint_el(SEEK_ID, 0), uint_el(SEEK_POSITION, 0)].concat(),
            ),
        )
        .len();
        let cues_position = (seek_head_len + tracks.len() + cluster.len()) as u64;
        let seek_head = el(
            SEEK_HEAD,
            &el(
                SEEK,
                &[
                    uint_el(SEEK_ID, u64::from(CUES)),
                    uint_el(SEEK_POSITION, cues_position),
                ]
                .concat(),
            ),
        );
        let mut segment = [seek_head, tracks, cluster].concat();
        if let Some(cues) = cues {
            segment.extend(el(CUES, &cues));
        }
        [ebml, el(SEGMENT, &segment)].concat()
    }

    #[test]
    fn the_cues_of_the_video_track_are_its_keyframes() {
        let cues = [cue(4000, 2), cue(0, 2), cue(2000, 2), cue(1000, 1)].concat();
        let indexed = read_bytes(&matroska_with(Some(cues))).unwrap();
        assert_eq!(indexed.kind, IndexKind::Cues);
        assert_eq!(indexed.times_ms, [0, 2000, 4000]);
    }

    #[test]
    fn a_file_with_no_cues_or_none_for_its_video_has_no_index() {
        // The SeekHead points past the end.
        assert_eq!(
            read_bytes(&matroska_with(None)),
            Err(NoIndex::Malformed("the Cues"))
        );
        let audio_only = cue(1000, 1);
        assert_eq!(
            read_bytes(&matroska_with(Some(audio_only))),
            Err(NoIndex::NoCues)
        );
    }

    #[test]
    fn a_cut_short_file_is_no_index_not_a_panic() {
        let file = matroska_with(Some([cue(0, 2), cue(2000, 2)].concat()));
        for len in 0..file.len() {
            assert!(read_bytes(&file[..len]).is_err(), "{len} bytes");
        }
    }

    #[test]
    fn a_size_past_the_cap_is_refused_before_it_is_read() {
        // A Cues element that says it holds 1 TiB.
        let mut file = matroska_with(Some(cue(0, 2)));
        let at = file
            .windows(4)
            .rposition(|w| w == CUES.to_be_bytes())
            .unwrap();
        file[at + 4..at + 12].copy_from_slice(&[0x01, 0, 0, 0x01, 0, 0, 0, 0]);
        assert!(matches!(
            read_bytes(&file),
            Err(NoIndex::TooLarge(_) | NoIndex::Truncated(_))
        ));
    }

    #[test]
    fn neither_format_is_no_index() {
        assert_eq!(read_bytes(b"RIFF\0\0\0\0AVI LIST"), Err(NoIndex::Format));
        assert_eq!(read_bytes(b""), Err(NoIndex::Format));
    }

    fn mp4_box(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        [&((data.len() + 8) as u32).to_be_bytes()[..], kind, data].concat()
    }

    fn full(kind: &[u8; 4], version: u8, data: &[u8]) -> Vec<u8> {
        mp4_box(kind, &[&[version, 0, 0, 0][..], data].concat())
    }

    fn table(entries: &[&[u32]]) -> Vec<u8> {
        let mut out = (entries.len() as u32).to_be_bytes().to_vec();
        for entry in entries {
            for n in *entry {
                out.extend(n.to_be_bytes());
            }
        }
        out
    }

    /// A video track at 1000 per second: 10 samples 500 apart, B-frame
    /// offsets of 1000, keyframes `sync`, and `elst` if given.
    fn mp4_with(sync: Option<&[u32]>, elst: Option<Vec<u8>>, extra: Vec<u8>) -> Vec<u8> {
        let mvhd = full(
            b"mvhd",
            0,
            &[&[0; 8][..], &1000u32.to_be_bytes(), &[0; 4]].concat(),
        );
        let mdhd = full(
            b"mdhd",
            0,
            &[&[0; 8][..], &1000u32.to_be_bytes(), &[0; 4]].concat(),
        );
        let hdlr = full(b"hdlr", 0, &[&[0; 4][..], b"vide", &[0; 12]].concat());
        let mut stbl = full(b"stts", 0, &table(&[&[10, 500]]));
        stbl.extend(full(b"ctts", 0, &table(&[&[10, 1000]])));
        if let Some(sync) = sync {
            let entries: Vec<&[u32]> = sync.iter().map(std::slice::from_ref).collect();
            stbl.extend(full(b"stss", 0, &table(&entries)));
        }
        stbl.extend(extra);
        let minf = mp4_box(b"minf", &mp4_box(b"stbl", &stbl));
        let mdia = mp4_box(b"mdia", &[mdhd, hdlr, minf].concat());
        let mut trak = Vec::new();
        if let Some(elst) = elst {
            trak.extend(mp4_box(b"edts", &elst));
        }
        trak.extend(mdia);
        let moov = mp4_box(b"moov", &[mvhd, mp4_box(b"trak", &trak)].concat());
        [
            mp4_box(b"ftyp", b"isom\0\0\0\0"),
            mp4_box(b"mdat", &[0; 32]),
            moov,
        ]
        .concat()
    }

    fn elst(entries: &[(u32, i32)]) -> Vec<u8> {
        let rows: Vec<[u32; 3]> = entries
            .iter()
            .map(|&(d, m)| [d, m as u32, 0x0001_0000])
            .collect();
        let rows: Vec<&[u32]> = rows.iter().map(|r| &r[..]).collect();
        full(b"elst", 0, &table(&rows))
    }

    #[test]
    fn stss_gives_each_keyframe_s_presentation_time() {
        let indexed = read_bytes(&mp4_with(Some(&[1, 5, 9]), None, Vec::new())).unwrap();
        assert_eq!(indexed.kind, IndexKind::SyncSamples);
        // Decode at 0, 2000, 4000; shown 1000 later.
        assert_eq!(indexed.times_ms, [1000, 3000, 5000]);
    }

    /// The edit starts the film at media time 1000, the first keyframe's
    /// presentation: what ffmpeg shows at 0.
    #[test]
    fn an_edit_moves_the_times_and_ends_them() {
        let edits = Some(elst(&[(4000, 1000)]));
        let indexed = read_bytes(&mp4_with(Some(&[1, 5, 9]), edits, Vec::new())).unwrap();
        // The third, at 4000 after the edit, is where the edit ends.
        assert_eq!(indexed.times_ms, [0, 2000]);
        let delayed = Some(elst(&[(500, -1), (0, 1000)]));
        let indexed = read_bytes(&mp4_with(Some(&[1, 5, 9]), delayed, Vec::new())).unwrap();
        assert_eq!(indexed.times_ms, [500, 2500, 4500]);
    }

    #[test]
    fn no_stss_is_every_sample_a_keyframe() {
        let indexed = read_bytes(&mp4_with(None, None, Vec::new())).unwrap();
        assert_eq!(indexed.times_ms.len(), 10);
    }

    #[test]
    fn what_the_index_reader_does_not_follow_is_no_index() {
        let two = Some(elst(&[(1000, 0), (1000, 3000)]));
        assert_eq!(
            read_bytes(&mp4_with(Some(&[1]), two, Vec::new())),
            Err(NoIndex::EditList)
        );
        let stps = full(b"stps", 0, &table(&[&[3]]));
        assert_eq!(
            read_bytes(&mp4_with(Some(&[1]), None, stps)),
            Err(NoIndex::OtherSyncMarks)
        );
        let past = read_bytes(&mp4_with(Some(&[1, 11]), None, Vec::new()));
        assert_eq!(past, Err(NoIndex::Malformed("the stss box")));
    }

    /// A count the box cannot hold allocates nothing and is malformed.
    #[test]
    fn an_entry_count_past_the_box_is_malformed() {
        let mut file = mp4_with(Some(&[1, 5]), None, Vec::new());
        let at = file.windows(4).position(|w| w == b"stss").unwrap();
        // version and flags, then the count.
        file[at + 8..at + 12].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(read_bytes(&file), Err(NoIndex::Malformed("the stss box")));
    }

    #[test]
    fn a_cut_short_mp4_is_no_index_not_a_panic() {
        let file = mp4_with(Some(&[1, 5, 9]), Some(elst(&[(0, 1000)])), Vec::new());
        for len in 0..file.len() {
            assert!(read_bytes(&file[..len]).is_err(), "{len} bytes");
        }
    }
}
