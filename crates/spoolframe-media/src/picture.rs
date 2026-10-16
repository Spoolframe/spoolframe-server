//! Pictures people upload (PX-53): a playlist's cover, sent by whoever may
//! edit the playlist, from any device. Untrusted, so nothing of the file
//! itself is kept:
//!
//! - [`check`] reads the header alone, here, in safe code: which of the
//!   four formats it is (JPEG, PNG, WebP, HEIC) and how many pixels it
//!   says it has. A file over [`MAX_BYTES`] or a picture over
//!   [`MAX_PIXELS`] is refused before anything is decoded, so a
//!   decompression bomb that says what it is never reaches a decoder.
//! - [`decode_square`] has ffmpeg decode it, confined as every run of it
//!   is ([`crate::sandbox`]), on one thread, under a memory, CPU and wall
//!   clock limit ([`DecodeLimits`]), and with its own pixel limit: a bomb
//!   that lies in its header is stopped there, in a process of its own,
//!   and the server goes on. ffmpeg crops it to its centre square, scales
//!   it down, and writes raw RGB, which the caller encodes again: no
//!   EXIF, GPS or other block of the upload survives.
//!
//! One decoder for all four formats, HEIC included, the one the server
//! already ships and confines: no image library with C parsers runs in
//! the server's own process.

use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt as _;

use crate::sandbox::{Jail, Limits, Memory, Tool};

/// The largest file taken: an iPhone's 48-megapixel HEIC is under 10 MB,
/// a JPEG of it a little more.
pub const MAX_BYTES: u64 = 20 << 20;

/// The most pixels a picture may say it has: 48 MP is 8064 × 6048, a
/// little over; so 40 MP takes every phone's 12 to 24 MP photo, and a
/// 48 MP one exported at 24.
pub const MAX_PIXELS: u64 = 40_000_000;

/// The pixel limit ffmpeg's decoder holds itself to: the header's limit
/// with room for the padding a decoder adds (a JPEG's 16-pixel blocks).
const DECODER_PIXELS: u64 = MAX_PIXELS + MAX_PIXELS / 5;

/// The longest error ffmpeg is listened to for.
const MAX_STDERR: u64 = 16 << 10;

/// A format taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    Jpeg,
    Png,
    Webp,
    /// HEIC: HEVC in a HEIF file, as an iPhone takes photos.
    Heif,
}

impl Format {
    /// ffmpeg's demuxer for it, named so ffmpeg never guesses one.
    fn demuxer(self) -> &'static str {
        match self {
            Format::Jpeg => "jpeg_pipe",
            Format::Png => "png_pipe",
            Format::Webp => "webp_pipe",
            Format::Heif => "mov",
        }
    }

    /// The extension its file is written with.
    pub fn extension(self) -> &'static str {
        match self {
            Format::Jpeg => "jpg",
            Format::Png => "png",
            Format::Webp => "webp",
            Format::Heif => "heic",
        }
    }
}

/// What a file's header says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub format: Format,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum PictureError {
    #[error("the file is {bytes} bytes; a picture is at most {MAX_BYTES} bytes")]
    TooManyBytes { bytes: u64 },
    #[error("the picture is {width} × {height}; at most {MAX_PIXELS} pixels are taken")]
    TooManyPixels { width: u32, height: u32 },
    /// Not one of the four formats, or one whose size cannot be read.
    #[error("{0}")]
    Unreadable(String),
    /// ffmpeg could not decode it: a broken file, or one that hit a limit.
    #[error("the picture could not be decoded: {0}")]
    Failed(String),
    /// ffmpeg did not run.
    #[error("cannot run ffmpeg: {0}")]
    Tool(String),
}

/// `bytes`' format and size, if it is a picture taken: within
/// [`MAX_BYTES`] and [`MAX_PIXELS`]. Nothing is decoded.
pub fn check(bytes: &[u8]) -> Result<Header, PictureError> {
    let length = bytes.len() as u64;
    if length > MAX_BYTES {
        return Err(PictureError::TooManyBytes { bytes: length });
    }
    let header = header(bytes)?;
    if u64::from(header.width) * u64::from(header.height) > MAX_PIXELS {
        return Err(PictureError::TooManyPixels {
            width: header.width,
            height: header.height,
        });
    }
    Ok(header)
}

/// `bytes`' format and size, from its header alone.
pub fn header(bytes: &[u8]) -> Result<Header, PictureError> {
    let format = sniff(bytes)
        .ok_or_else(|| PictureError::Unreadable("not a JPEG, PNG, WebP or HEIC picture".into()))?;
    let size = match format {
        Format::Jpeg => jpeg_size(bytes),
        Format::Png => png_size(bytes),
        Format::Webp => webp_size(bytes),
        Format::Heif => heif_size(bytes),
    };
    match size {
        Some((width, height)) if width > 0 && height > 0 => Ok(Header {
            format,
            width,
            height,
        }),
        _ => Err(PictureError::Unreadable(format!(
            "a {} file whose size cannot be read",
            format.extension()
        ))),
    }
}

/// The brands of an HEIF file whose picture is HEVC.
const HEVC_BRANDS: [&[u8; 4]; 6] = [b"heic", b"heix", b"heim", b"heis", b"hevc", b"hevx"];

fn sniff(b: &[u8]) -> Option<Format> {
    if b.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some(Format::Jpeg);
    }
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(Format::Png);
    }
    if b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        return Some(Format::Webp);
    }
    // `ftyp` first: its major brand, a version, then compatible brands.
    // HEVC must be among them; `mif1` alone may be AVIF, not taken.
    if b.len() >= 16 && &b[4..8] == b"ftyp" {
        let size = (be32(b, 0)? as usize).min(b.len());
        let mut brands = vec![&b[8..12]];
        let mut at = 16;
        while at + 4 <= size {
            brands.push(&b[at..at + 4]);
            at += 4;
        }
        if brands
            .iter()
            .any(|brand| HEVC_BRANDS.iter().any(|h| h.as_slice() == *brand))
        {
            return Some(Format::Heif);
        }
    }
    None
}

fn be16(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(2)?)?;
    Some(u32::from(u16::from_be_bytes([s[0], s[1]])))
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

fn be64(b: &[u8], at: usize) -> Option<u64> {
    let s = b.get(at..at.checked_add(8)?)?;
    Some(u64::from_be_bytes(s.try_into().ok()?))
}

fn le16(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(2)?)?;
    Some(u32::from(u16::from_le_bytes([s[0], s[1]])))
}

fn le24(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(3)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], 0]))
}

fn le32(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// A JPEG's size, from its first start-of-frame marker: every marker
/// before it is stepped over by its length.
fn jpeg_size(b: &[u8]) -> Option<(u32, u32)> {
    let mut at = 2;
    loop {
        if *b.get(at)? != 0xff {
            return None;
        }
        // Fill bytes: any number of 0xFF before the marker.
        while *b.get(at)? == 0xff {
            at += 1;
        }
        let marker = b[at];
        at += 1;
        match marker {
            // Markers without a length.
            0x01 | 0xd0..=0xd8 => continue,
            // The end, or the picture's data, before any frame.
            0xd9 | 0xda => return None,
            _ => {}
        }
        let length = be16(b, at)? as usize;
        if length < 2 {
            return None;
        }
        // SOF0 to SOF15, but DHT, JPG and DAC, which share the range.
        if (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
            let height = be16(b, at + 3)?;
            let width = be16(b, at + 5)?;
            return Some((width, height));
        }
        at = at.checked_add(length)?;
    }
}

/// A PNG's size, from `IHDR`, which must be its first chunk.
fn png_size(b: &[u8]) -> Option<(u32, u32)> {
    if b.get(12..16)? != b"IHDR" {
        return None;
    }
    Some((be32(b, 16)?, be32(b, 20)?))
}

/// A WebP's size, from its first chunk: a lossy frame (`VP8 `), a
/// lossless one (`VP8L`), or the extended header's canvas (`VP8X`).
fn webp_size(b: &[u8]) -> Option<(u32, u32)> {
    match b.get(12..16)? {
        b"VP8 " => {
            // A frame tag of 3 bytes, then the key frame's start code.
            if b.get(23..26)? != [0x9d, 0x01, 0x2a] {
                return None;
            }
            Some((le16(b, 26)? & 0x3fff, le16(b, 28)? & 0x3fff))
        }
        b"VP8L" => {
            if *b.get(20)? != 0x2f {
                return None;
            }
            let bits = le32(b, 21)?;
            Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
        }
        b"VP8X" => Some((le24(b, 24)? + 1, le24(b, 27)? + 1)),
        _ => None,
    }
}

/// An HEIF's size: the largest image spatial extent (`ispe`) among its
/// items' properties (`meta` › `iprp` › `ipco`). The primary picture of
/// an iPhone's photo is a grid of 512-pixel tiles, and its own extent,
/// the whole picture's, is the largest.
fn heif_size(b: &[u8]) -> Option<(u32, u32)> {
    let mut largest: Option<(u32, u32)> = None;
    for (kind, meta) in Boxes::new(b) {
        // A full box: version and flags first.
        if kind != *b"meta" || meta.len() < 4 {
            continue;
        }
        for (kind, iprp) in Boxes::new(&meta[4..]) {
            if kind != *b"iprp" {
                continue;
            }
            for (kind, ipco) in Boxes::new(iprp) {
                if kind != *b"ipco" {
                    continue;
                }
                for (kind, ispe) in Boxes::new(ipco) {
                    if kind != *b"ispe" {
                        continue;
                    }
                    let (Some(width), Some(height)) = (be32(ispe, 4), be32(ispe, 8)) else {
                        continue;
                    };
                    let area = |(w, h): (u32, u32)| u64::from(w) * u64::from(h);
                    if largest.is_none_or(|l| area((width, height)) > area(l)) {
                        largest = Some((width, height));
                    }
                }
            }
        }
    }
    largest
}

/// The ISO BMFF boxes in a run of bytes, each its type and body; one that
/// runs past the end stops the run.
struct Boxes<'a> {
    rest: &'a [u8],
}

impl<'a> Boxes<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { rest: bytes }
    }
}

impl<'a> Iterator for Boxes<'a> {
    type Item = ([u8; 4], &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        let rest = self.rest;
        let size = u64::from(be32(rest, 0)?);
        let kind: [u8; 4] = rest.get(4..8)?.try_into().ok()?;
        let (header, size) = match size {
            // A 64-bit size follows the type.
            1 => (16, be64(rest, 8)?),
            // To the end.
            0 => (8, rest.len() as u64),
            n => (8, n),
        };
        if size < header || size > rest.len() as u64 {
            self.rest = &[];
            return None;
        }
        let size = size as usize;
        self.rest = &rest[size..];
        Some((kind, &rest[header as usize..size]))
    }
}

/// What one decode may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeLimits {
    /// `RLIMIT_DATA`, bytes.
    pub memory: u64,
    /// `RLIMIT_CPU`, seconds.
    pub cpu_secs: u64,
    /// How long it may take, start to end.
    pub wall: Duration,
}

impl DecodeLimits {
    /// A 40-megapixel picture of 16 bits a channel is 320 MiB decoded; a
    /// JPEG or HEIC of that size decodes in a second or two on one core.
    pub const DEFAULT: Self = Self {
        memory: 1 << 30,
        cpu_secs: 30,
        wall: Duration::from_secs(30),
    };
}

/// A square picture's raw pixels: `side` × `side`, RGB, a byte each.
#[derive(Clone, PartialEq, Eq)]
pub struct Square {
    pub side: u32,
    pub rgb: Vec<u8>,
}

impl std::fmt::Debug for Square {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Square")
            .field("side", &self.side)
            .field("bytes", &self.rgb.len())
            .finish()
    }
}

/// The picture in `input`, of `format`, decoded by `ffmpeg` within
/// `limits`: its centre square, scaled down to at most `most` pixels a
/// side (never up). `input`'s directory is read by the run: one holding
/// that file alone.
pub async fn decode_square(
    ffmpeg: &Tool,
    input: &Path,
    format: Format,
    most: u32,
    limits: DecodeLimits,
) -> Result<Square, PictureError> {
    let mut jail = Jail::new(Limits {
        memory: Memory::fixed(limits.memory),
        cpu_secs: limits.cpu_secs,
        // It writes to a pipe, never a file.
        file_size: 1 << 20,
    })
    .input(input);
    // The upload is in a directory of the server's own, of its own: the
    // state directory is otherwise hidden from every run.
    if let Some(dir) = input.parent() {
        jail = jail.reads(dir);
    }
    let tool =
        |e: std::io::Error| PictureError::Tool(format!("{}: {e}", ffmpeg.program().display()));
    let mut command = ffmpeg.command_off_thread(jail).await.map_err(tool)?;
    command
        .args(decode_args(input, format, most))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(tool)?;
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return Err(PictureError::Tool("ffmpeg's output was not piped".into()));
    };
    let largest = ppm_length(most);
    let run = async {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut stdout = stdout.take(largest + 1);
        let mut stderr = stderr.take(MAX_STDERR);
        let (read, said) =
            tokio::join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err),);
        read?;
        said?;
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((status, out, err))
    };
    // Bound first: the run borrows the child until it is dropped here.
    let outcome = tokio::time::timeout(limits.wall, run).await;
    let (status, out, err) = match outcome {
        Ok(Ok(done)) => done,
        Ok(Err(e)) => return Err(PictureError::Failed(e.to_string())),
        Err(_) => {
            let _ = child.start_kill();
            return Err(PictureError::Failed(format!(
                "it took over {:?}",
                limits.wall
            )));
        }
    };
    if !status.success() {
        let said = String::from_utf8_lossy(&err);
        let last = said.lines().last().unwrap_or("no message").trim();
        return Err(PictureError::Failed(format!("ffmpeg {status}: {last}")));
    }
    if out.len() as u64 > largest {
        return Err(PictureError::Failed(
            "ffmpeg wrote more than the picture asked for".into(),
        ));
    }
    let square = ppm(out)
        .ok_or_else(|| PictureError::Failed("ffmpeg's picture was not a square of RGB".into()))?;
    if square.side > most {
        return Err(PictureError::Failed(format!(
            "ffmpeg's picture is {} pixels a side, past {most}",
            square.side
        )));
    }
    Ok(square)
}

/// ffmpeg's arguments for [`decode_square`].
fn decode_args(input: &Path, format: Format, most: u32) -> Vec<OsString> {
    // Cropped first, then scaled: both sides the same, whatever a crop's
    // rounding to the chroma grid left.
    let side = format!("'min({most},min(iw,ih))'");
    let filter = format!(
        "crop='min(iw,ih)':'min(iw,ih)',scale=w={side}:h={side}:flags=lanczos,setsar=1,format=rgb24"
    );
    let mut args: Vec<OsString> = [
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        // One thread: the memory limit is the picture's, not a thread
        // stack's per core.
        "-threads",
        "1",
        "-filter_threads",
        "1",
        // The file and nothing it names.
        "-protocol_whitelist",
        "file",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    args.extend([
        "-max_pixels".into(),
        DECODER_PIXELS.to_string().into(),
        "-f".into(),
        format.demuxer().into(),
        "-i".into(),
        input.as_os_str().to_owned(),
    ]);
    args.extend(
        [
            "-frames:v",
            "1",
            "-an",
            "-sn",
            "-dn",
            "-vf",
            &filter,
            "-c:v",
            "ppm",
            "-f",
            "image2pipe",
            "pipe:1",
        ]
        .iter()
        .map(OsString::from),
    );
    args
}

/// The most bytes a PPM of `side` × `side` RGB takes: its pixels, and a
/// header of at most 32 bytes.
fn ppm_length(side: u32) -> u64 {
    u64::from(side) * u64::from(side) * 3 + 32
}

/// A binary PPM (`P6`) of 8-bit RGB as ffmpeg writes one: `P6\n<w> <h>\n255\n`
/// and the pixels; `None` unless it is that, and square.
fn ppm(mut bytes: Vec<u8>) -> Option<Square> {
    let mut fields = Vec::with_capacity(4);
    let mut at = 0;
    while fields.len() < 4 {
        let start = at;
        while at < bytes.len() && !bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        if at == start || at >= bytes.len() || at - start > 10 {
            return None;
        }
        fields.push(std::str::from_utf8(&bytes[start..at]).ok()?.to_owned());
        // One whitespace byte after each field; the pixels follow the last.
        at += 1;
    }
    if fields[0] != "P6" || fields[3] != "255" {
        return None;
    }
    let width: u32 = fields[1].parse().ok()?;
    let height: u32 = fields[2].parse().ok()?;
    if width != height || width == 0 {
        return None;
    }
    let pixels = usize::try_from(u64::from(width) * u64::from(height) * 3).ok()?;
    if bytes.len() - at != pixels {
        return None;
    }
    bytes.drain(..at);
    Some(Square {
        side: width,
        rgb: bytes,
    })
}

#[cfg(test)]
mod tests;
