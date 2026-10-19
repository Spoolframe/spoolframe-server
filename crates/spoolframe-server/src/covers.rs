//! Playlist covers (PX-53): what a playlist's `artwork` names, every kind
//! of it an [`ArtworkId`] in the `image:` namespace, served through the
//! artwork cache ([`crate::artwork`]) and its sizes like any other:
//!
//! - `image:playlist/<hash>`: a cover an editor uploaded, kept in
//!   `state_dir/images/playlists/<hash>.jpg`. Only what the server
//!   encoded itself is kept: [`Covers::ingest`] has ffmpeg decode the
//!   upload confined ([`spoolframe_media::picture`]), and writes its
//!   centre square, at most [`SIDE`] pixels, as a new JPEG, so no EXIF,
//!   GPS or other block of the upload survives. Named by the hash of
//!   that JPEG: the same picture twice is one file.
//! - `image:mosaic/<ids>`: a 2×2 grid of four album covers or posters,
//!   the four artwork ids themselves in its key (base64url), so the id
//!   is the cache's key and changes when any of the four does. Made on
//!   demand from the four, each authorized as itself; never kept as a
//!   playlist's cover.
//! - `image:tile/liked/2`, `image:tile/all/2`, `image:tile/empty/1`:
//!   Liked's and All's fixed tiles, pictures built into the server
//!   (`assets/tiles`), and an empty playlist's placeholder, drawn here.
//!   The number is the picture's: a new one gets a new id, so no app
//!   keeps showing the old one from its cache.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, GenericImage as _, Rgb, RgbImage};
use spoolframe_media::picture::{self, DecodeLimits, PictureError};
use spoolframe_media::sandbox::Tool;
use spoolframe_types::{ArtworkId, Namespace};

/// The widest and tallest cover kept.
pub const SIDE: u32 = 2048;

/// JPEG quality for a kept cover, a mosaic and a tile.
const JPEG_QUALITY: u8 = 88;

/// A mosaic's tiles, each this many pixels a side: 1200 in all, near the
/// `large` size.
const MOSAIC_TILE: u32 = 600;

/// A drawn tile's side.
const TILE_SIDE: u32 = 1024;

/// Uploads decoded at once: each is a confined ffmpeg of up to
/// [`DecodeLimits::DEFAULT`]'s memory.
const DECODES: usize = 2;

/// The longest mosaic key read: four artwork ids, each a path or so.
const MAX_MOSAIC_KEY: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub enum CoverError {
    /// Over [`picture::MAX_BYTES`], or [`picture::MAX_PIXELS`] by its
    /// header: not decoded.
    #[error(transparent)]
    TooLarge(PictureError),
    /// Not a picture taken, or one that did not decode within its limits.
    #[error(transparent)]
    Unreadable(PictureError),
    /// ffmpeg did not run: the server's, not the picture's, fault.
    #[error(transparent)]
    Tool(PictureError),
    #[error("cannot keep the cover: {0}")]
    Io(#[from] std::io::Error),
}

impl From<PictureError> for CoverError {
    fn from(e: PictureError) -> Self {
        match e {
            PictureError::TooManyBytes { .. } | PictureError::TooManyPixels { .. } => {
                CoverError::TooLarge(e)
            }
            PictureError::Unreadable(_) | PictureError::Failed(_) => CoverError::Unreadable(e),
            PictureError::Tool(_) => CoverError::Tool(e),
        }
    }
}

/// The uploaded covers kept, and how uploads are decoded.
#[derive(Debug)]
pub struct Covers {
    /// `state_dir/images/playlists`.
    dir: PathBuf,
    /// Where an upload waits for ffmpeg, each in a directory of its own.
    scratch: PathBuf,
    ffmpeg: Tool,
    limits: DecodeLimits,
    decodes: tokio::sync::Semaphore,
}

impl Covers {
    /// Covers kept in `state_dir/images/playlists`, uploads decoded by
    /// `ffmpeg` in `state_dir/cache/uploads`.
    pub fn new(state_dir: &Path, ffmpeg: Tool) -> Self {
        Self {
            dir: state_dir.join("images").join("playlists"),
            scratch: state_dir.join("cache").join("uploads"),
            ffmpeg,
            limits: DecodeLimits::DEFAULT,
            decodes: tokio::sync::Semaphore::new(DECODES),
        }
    }

    /// Decode within `limits` instead: a test's.
    pub fn with_limits(mut self, limits: DecodeLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Where the covers are kept.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// `bytes` as a cover kept: its id. Refused, before anything is
    /// decoded, past the size limits; decoded by a confined ffmpeg; its
    /// centre square, at most [`SIDE`], written as a JPEG of the server's.
    pub async fn ingest(&self, bytes: &[u8]) -> Result<ArtworkId, CoverError> {
        let header = picture::check(bytes)?;
        let _turn = self
            .decodes
            .acquire()
            .await
            .map_err(|e| CoverError::Io(std::io::Error::other(e)))?;
        // A directory of its own, which the run alone is given to read.
        let place = self.scratch.join(uuid::Uuid::now_v7().to_string());
        tokio::fs::create_dir_all(&place).await?;
        let input = place.join(format!("upload.{}", header.format.extension()));
        let decoded = async {
            tokio::fs::write(&input, bytes).await?;
            Ok::<_, CoverError>(
                picture::decode_square(&self.ffmpeg, &input, header.format, SIDE, self.limits)
                    .await?,
            )
        }
        .await;
        if let Err(e) = tokio::fs::remove_dir_all(&place).await {
            tracing::warn!(error = %e, dir = %place.display(), "cannot remove an upload's directory");
        }
        let square = decoded?;
        let jpeg = tokio::task::spawn_blocking(move || encode_square(square))
            .await
            .map_err(|e| CoverError::Io(std::io::Error::other(e)))??;
        let hash = blake3::hash(&jpeg).to_hex()[..32].to_owned();
        self.keep(&hash, &jpeg).await?;
        Ok(cover_id(&hash))
    }

    /// Write cover `hash` unless it is there: whole or not at all.
    async fn keep(&self, hash: &str, jpeg: &[u8]) -> std::io::Result<()> {
        let path = self.path(hash);
        if tokio::fs::try_exists(&path).await? {
            return Ok(());
        }
        tokio::fs::create_dir_all(&self.dir).await?;
        let partial = self.dir.join(format!(".{hash}.{}", uuid::Uuid::now_v7()));
        tokio::fs::write(&partial, jpeg).await?;
        tokio::fs::rename(&partial, &path).await
    }

    fn path(&self, hash: &str) -> PathBuf {
        self.dir.join(format!("{hash}.jpg"))
    }

    /// Whether cover `id` is kept.
    pub async fn has(&self, id: &ArtworkId) -> bool {
        match local(id) {
            Some(Local::Cover(hash)) => tokio::fs::try_exists(self.path(&hash))
                .await
                .unwrap_or(false),
            _ => false,
        }
    }

    /// Cover `id`'s JPEG.
    pub async fn read(&self, id: &ArtworkId) -> std::io::Result<Vec<u8>> {
        match local(id) {
            Some(Local::Cover(hash)) => tokio::fs::read(self.path(&hash)).await,
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{id} is not a cover"),
            )),
        }
    }

    /// Delete cover `id`'s file: no playlist uses it any more.
    pub async fn remove(&self, id: &ArtworkId) {
        let Some(Local::Cover(hash)) = local(id) else {
            return;
        };
        match tokio::fs::remove_file(self.path(&hash)).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(error = %e, cover = %id, "cannot delete a cover no playlist uses")
            }
        }
    }

    /// Delete every kept cover not in `used`, and what uploads left: at
    /// start, after playlists went with a user who left, or a crash.
    pub async fn sweep(&self, used: &std::collections::HashSet<String>) -> std::io::Result<usize> {
        let _ = tokio::fs::remove_dir_all(&self.scratch).await;
        let mut entries = match tokio::fs::read_dir(&self.dir).await {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e),
        };
        let mut swept = 0;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let kept = name
                .strip_suffix(".jpg")
                .filter(|hash| is_hash(hash))
                .is_some_and(|hash| used.contains(&cover_id(hash).to_string()));
            if !kept {
                tokio::fs::remove_file(entry.path()).await?;
                swept += 1;
            }
        }
        Ok(swept)
    }
}

/// `square` as a JPEG of the server's own: the pixels and nothing else.
fn encode_square(square: picture::Square) -> Result<Vec<u8>, CoverError> {
    let picture = RgbImage::from_raw(square.side, square.side, square.rgb).ok_or_else(|| {
        CoverError::Unreadable(PictureError::Failed("pixels of the wrong length".into()))
    })?;
    jpeg(&picture).map_err(|e| CoverError::Io(std::io::Error::other(e)))
}

fn jpeg(picture: &RgbImage) -> Result<Vec<u8>, String> {
    let mut out = Cursor::new(Vec::new());
    JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY)
        .encode_image(picture)
        .map_err(|e| e.to_string())?;
    Ok(out.into_inner())
}

/// What an `image:` artwork id names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Local {
    /// An uploaded cover, by its hash.
    Cover(String),
    /// The four covers a mosaic is made of, in order.
    Mosaic(Vec<ArtworkId>),
    Tile(Tile),
}

/// A drawn tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tile {
    /// Liked's: a heart.
    Liked,
    /// All's: the word.
    All,
    /// A playlist with nothing to make a mosaic of.
    Empty,
}

impl Tile {
    fn key(self) -> &'static str {
        match self {
            Tile::Liked => "tile/liked/2",
            Tile::All => "tile/all/2",
            Tile::Empty => "tile/empty/1",
        }
    }
}

fn is_hash(text: &str) -> bool {
    text.len() == 32
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Uploaded cover `hash`'s id.
pub fn cover_id(hash: &str) -> ArtworkId {
    ArtworkId::new(Namespace::Image, format!("playlist/{hash}")).expect("never empty")
}

/// The id of the mosaic of `parts`, four covers in order.
pub fn mosaic_id(parts: &[ArtworkId]) -> ArtworkId {
    let joined = parts
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    ArtworkId::new(
        Namespace::Image,
        format!("mosaic/{}", URL_SAFE_NO_PAD.encode(joined)),
    )
    .expect("never empty")
}

/// Tile `tile`'s id.
pub fn tile_id(tile: Tile) -> ArtworkId {
    ArtworkId::new(Namespace::Image, tile.key()).expect("never empty")
}

/// What `id` names, if it is an `image:` id this server makes.
pub fn local(id: &ArtworkId) -> Option<Local> {
    if id.namespace() != Namespace::Image {
        return None;
    }
    let key = id.key();
    if let Some(hash) = key.strip_prefix("playlist/") {
        return is_hash(hash).then(|| Local::Cover(hash.to_owned()));
    }
    if let Some(encoded) = key.strip_prefix("mosaic/") {
        if encoded.len() > MAX_MOSAIC_KEY {
            return None;
        }
        let joined = String::from_utf8(URL_SAFE_NO_PAD.decode(encoded).ok()?).ok()?;
        let parts: Vec<ArtworkId> = joined
            .split('\n')
            .map(|part| part.parse().ok())
            .collect::<Option<_>>()?;
        // Four of the catalog's: a mosaic is never made of mosaics.
        let catalog = parts.iter().all(|p| p.namespace() != Namespace::Image);
        return (parts.len() == 4 && catalog).then_some(Local::Mosaic(parts));
    }
    [Tile::Liked, Tile::All, Tile::Empty]
        .into_iter()
        .find(|tile| tile.key() == key)
        .map(Local::Tile)
}

/// The mosaic of `tiles`, four pictures in order (left to right, top to
/// bottom), each cropped to its centre square: a JPEG.
pub fn compose(tiles: &[Vec<u8>]) -> Result<Vec<u8>, String> {
    let mut canvas = RgbImage::new(MOSAIC_TILE * 2, MOSAIC_TILE * 2);
    for (n, bytes) in tiles.iter().take(4).enumerate() {
        let picture = crate::artwork::decode(bytes).map_err(|e| e.to_string())?;
        let square =
            centre_square(picture).resize_exact(MOSAIC_TILE, MOSAIC_TILE, FilterType::Lanczos3);
        let (x, y) = ((n as u32 % 2) * MOSAIC_TILE, (n as u32 / 2) * MOSAIC_TILE);
        canvas
            .copy_from(&square.to_rgb8(), x, y)
            .map_err(|e| e.to_string())?;
    }
    jpeg(&canvas)
}

fn centre_square(picture: DynamicImage) -> DynamicImage {
    let side = picture.width().min(picture.height());
    let x = (picture.width() - side) / 2;
    let y = (picture.height() - side) / 2;
    picture.crop_imm(x, y, side, side)
}

/// Catppuccin Mocha, as the web UI's dark look.
mod mocha {
    pub const SURFACE0: [u8; 3] = [0x31, 0x32, 0x44];
    pub const OVERLAY0: [u8; 3] = [0x6c, 0x70, 0x86];
}

/// Liked's tile: a heart, 1024 px a side.
const LIKED_TILE: &[u8] = include_bytes!("../assets/tiles/liked.jpg");

/// All's tile: the word, 1024 px a side.
const ALL_TILE: &[u8] = include_bytes!("../assets/tiles/all.jpg");

/// The placeholder's JPEG, drawn once.
static EMPTY_TILE: OnceLock<Result<Vec<u8>, String>> = OnceLock::new();

/// Tile `tile`'s JPEG if it needs no drawing: Liked's and All's built-in
/// pictures, and the placeholder once drawn. Served as it is, off no
/// blocking thread.
pub fn ready(tile: Tile) -> Option<&'static [u8]> {
    match tile {
        Tile::Liked => Some(LIKED_TILE),
        Tile::All => Some(ALL_TILE),
        Tile::Empty => EMPTY_TILE.get()?.as_deref().ok(),
    }
}

/// Tile `tile`: a JPEG. Liked's and All's are the built-in pictures; the
/// placeholder is drawn the first time, which blocks.
pub fn draw(tile: Tile) -> Result<&'static [u8], String> {
    match tile {
        Tile::Liked => Ok(LIKED_TILE),
        Tile::All => Ok(ALL_TILE),
        Tile::Empty => EMPTY_TILE
            .get_or_init(|| jpeg(&drawn(|_, _| mocha::SURFACE0, mocha::OVERLAY0, lines)))
            .as_deref()
            .map_err(Clone::clone),
    }
}

/// A tile of `background`, with the shape `inside` says (at a point in
/// 0..1) in `ink`, each pixel's edge smoothed by 4×4 samples.
fn drawn(
    background: impl Fn(f32, f32) -> [u8; 3],
    ink: [u8; 3],
    inside: fn(f32, f32) -> bool,
) -> RgbImage {
    const SAMPLES: u32 = 4;
    let side = TILE_SIDE as f32;
    RgbImage::from_fn(TILE_SIDE, TILE_SIDE, |px, py| {
        let mut covered = 0;
        for sy in 0..SAMPLES {
            for sx in 0..SAMPLES {
                let x = (px as f32 + (sx as f32 + 0.5) / SAMPLES as f32) / side;
                let y = (py as f32 + (sy as f32 + 0.5) / SAMPLES as f32) / side;
                covered += u32::from(inside(x, y));
            }
        }
        let t = covered as f32 / (SAMPLES * SAMPLES) as f32;
        let back = background(px as f32 / side, py as f32 / side);
        let mix = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8;
        Rgb([
            mix(back[0], ink[0]),
            mix(back[1], ink[1]),
            mix(back[2], ink[2]),
        ])
    })
}

/// Three lines, a list's: an empty playlist's placeholder.
fn lines(x: f32, y: f32) -> bool {
    let within = |from: f32, to: f32, v: f32| (from..to).contains(&v);
    within(0.3, 0.7, x) && (within(0.36, 0.40, y) || within(0.48, 0.52, y) || within(0.60, 0.64, y))
}

#[cfg(test)]
mod tests;
