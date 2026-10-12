//! The artwork proxy's cache (P1-05): images from the catalog, resized on
//! the server, kept on disk under a size limit.
//!
//! Clients never reach Plex for images (§5). The original is fetched once
//! per artwork id and every size is made from it, here rather than by the
//! PMS photo transcoder, so a NativeCatalog (P9) gets resizing for free.
//! Artwork ids change when the image does (Plex paths end in an update
//! timestamp), so nothing is ever invalidated, only evicted.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{ImageFormat, ImageReader, Limits};
use spoolframe_catalog::{Catalog, CatalogError, Image};
use spoolframe_protocol::ArtworkSize;
use spoolframe_types::ArtworkId;

use crate::metrics::Counter;

/// JPEG quality for resized images without transparency.
const JPEG_QUALITY: u8 = 85;

/// A cached image.
#[derive(Clone, PartialEq, Eq)]
pub struct Artwork {
    pub bytes: Vec<u8>,
    pub content_type: String,
    /// Stable for this `(id, size)`.
    pub etag: String,
}

impl std::fmt::Debug for Artwork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Artwork")
            .field("content_type", &self.content_type)
            .field("bytes", &self.bytes.len())
            .field("etag", &self.etag)
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ArtworkError {
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    #[error("size {0:?} is not one the server serves")]
    BadSize(ArtworkSize),
    #[error("cannot resize the image: {0}")]
    Image(String),
    #[error("artwork cache: {0}")]
    Io(#[from] std::io::Error),
}

/// One file in the cache.
#[derive(Debug, Clone)]
struct Entry {
    file: PathBuf,
    content_type: String,
    size: u64,
    /// Larger is more recently used.
    used: u64,
}

#[derive(Debug, Default)]
struct Index {
    entries: HashMap<String, Entry>,
    total: u64,
    clock: u64,
}

impl Index {
    fn touch(&mut self, key: &str) -> Option<Entry> {
        self.clock += 1;
        let clock = self.clock;
        let entry = self.entries.get_mut(key)?;
        entry.used = clock;
        Some(entry.clone())
    }

    /// Record `entry`, then evict least recently used files until the
    /// total fits `limit` (never the one just added).
    fn insert(&mut self, key: String, mut entry: Entry, limit: u64) -> Vec<PathBuf> {
        self.clock += 1;
        entry.used = self.clock;
        self.total += entry.size;
        let mut evicted = Vec::new();
        let file = entry.file.clone();
        if let Some(old) = self.entries.insert(key.clone(), entry) {
            self.total -= old.size;
            // The same image under another extension (its content type
            // changed): the old file is nobody's now (review 2026-10-11
            // L24).
            if old.file != file {
                evicted.push(old.file);
            }
        }
        while self.total > limit {
            let Some(oldest) = self
                .entries
                .iter()
                .filter(|(k, _)| **k != key)
                .min_by_key(|(_, e)| e.used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            let gone = self.entries.remove(&oldest).expect("just found");
            self.total -= gone.size;
            evicted.push(gone.file);
        }
        evicted
    }
}

pub struct ArtworkCache {
    dir: PathBuf,
    limit: u64,
    catalog: Arc<dyn Catalog>,
    index: Mutex<Index>,
    /// One fetch or resize per key at a time; others wait and then find it
    /// cached.
    inflight: crate::keyed_locks::KeyedLocks,
    /// Images asked for, and of those found in the cache (P3-14).
    requests: Counter,
    hits: Counter,
    /// Artwork the catalog said this server serves, and when: asked again
    /// only after [`AUTHORIZED_FOR`], or once the catalog changed (review
    /// 2026-10-11 H3). Plex answers it with a GET of the item, so a grid of
    /// cached posters cost one Plex request each.
    authorized: Mutex<Authorized>,
    /// The catalog's changes ([`crate::catalog::LiveCatalog::changes`]):
    /// a library added, removed or changed forgets every answer.
    changes: Option<tokio::sync::watch::Receiver<u64>>,
    /// Resizes running at once, one per core.
    resizes: tokio::sync::Semaphore,
}

/// How long the catalog's word that an artwork is served is taken: a
/// library unshared in Plex itself, which the server is not told of, is
/// denied within this.
pub const AUTHORIZED_FOR: std::time::Duration = std::time::Duration::from_secs(300);

/// The most artwork answers kept: a large library's posters, and then
/// all are forgotten at once.
const MOST_AUTHORIZED: usize = 50_000;

#[derive(Debug, Default)]
struct Authorized {
    /// The catalog change they were given under.
    change: u64,
    at: std::collections::HashMap<String, std::time::Instant>,
}

/// Images asked for since the server started, and how many of them the
/// cache had (P3-14).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ArtworkCounts {
    pub requests: u64,
    pub hits: u64,
}

impl std::fmt::Debug for ArtworkCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArtworkCache")
            .field("dir", &self.dir)
            .field("limit", &self.limit)
            .finish_non_exhaustive()
    }
}

fn cache_key(id: &ArtworkId, size: ArtworkSize) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(id.to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(size.as_str().as_bytes());
    hasher.finalize().to_hex()[..32].to_owned()
}

fn extension(content_type: &str) -> &'static str {
    match content_type {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "img",
    }
}

fn content_type_of(extension: &str) -> &'static str {
    match extension {
        "jpg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => "application/octet-stream",
    }
}

impl ArtworkCache {
    /// A cache in `dir` holding at most `limit` bytes, filled from
    /// `catalog`. Files already in `dir` are kept, the most recently
    /// written counting as most recently used.
    pub fn open(dir: &Path, limit: u64, catalog: Arc<dyn Catalog>) -> std::io::Result<Self> {
        crate::files::private_dir(dir)?;
        let mut found = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            // One unreadable entry is skipped, not the whole cache (and the
            // server's start) refused (review 2026-10-11 L24).
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    tracing::warn!(dir = %dir.display(), error = %e, "an artwork cache entry cannot be read; skipped");
                    continue;
                }
            };
            let path = entry.path();
            let (Some(stem), Some(ext)) = (
                path.file_stem().and_then(|s| s.to_str()),
                path.extension().and_then(|s| s.to_str()),
            ) else {
                continue;
            };
            if ext == "tmp" {
                // A write that never finished.
                let _ = std::fs::remove_file(&path);
                continue;
            }
            let meta = match entry.metadata() {
                Ok(meta) => meta,
                Err(e) => {
                    tracing::warn!(file = %path.display(), error = %e, "an artwork cache file cannot be read; skipped");
                    continue;
                }
            };
            found.push((
                meta.modified().ok(),
                stem.to_owned(),
                path.clone(),
                ext.to_owned(),
                meta.len(),
            ));
        }
        found.sort_by_key(|(modified, ..)| *modified);
        let mut index = Index::default();
        let mut evicted = Vec::new();
        for (_, key, file, ext, size) in found {
            let entry = Entry {
                file,
                content_type: content_type_of(&ext).to_owned(),
                size,
                used: 0,
            };
            evicted.extend(index.insert(key, entry, limit));
        }
        for file in evicted {
            let _ = std::fs::remove_file(file);
        }
        Ok(Self {
            dir: dir.to_owned(),
            limit,
            catalog,
            index: Mutex::new(index),
            inflight: Default::default(),
            requests: Counter::default(),
            hits: Counter::default(),
            authorized: Mutex::default(),
            changes: None,
            resizes: tokio::sync::Semaphore::new(
                std::thread::available_parallelism().map_or(2, |n| n.get()),
            ),
        })
    }

    /// Forget what the catalog said each time it changes (`changes`, the
    /// live catalog's).
    pub fn with_changes(mut self, changes: tokio::sync::watch::Receiver<u64>) -> Self {
        self.changes = Some(changes);
        self
    }

    /// Whether the catalog said lately that `id` is served, under the
    /// catalog as it is now.
    fn authorized_lately(&self, id: &ArtworkId) -> bool {
        let change = self.changes.as_ref().map_or(0, |c| *c.borrow());
        let mut authorized = self.authorized.lock();
        if authorized.change != change {
            authorized.at.clear();
            authorized.change = change;
        }
        authorized
            .at
            .get(&id.to_string())
            .is_some_and(|at| at.elapsed() < AUTHORIZED_FOR)
    }

    fn remember_authorized(&self, id: &ArtworkId) {
        let mut authorized = self.authorized.lock();
        if authorized.at.len() >= MOST_AUTHORIZED {
            authorized.at.clear();
        }
        authorized
            .at
            .insert(id.to_string(), std::time::Instant::now());
    }

    /// What was asked for, and found (P3-14).
    pub fn counts(&self) -> ArtworkCounts {
        ArtworkCounts {
            requests: self.requests.get(),
            hits: self.hits.get(),
        }
    }

    /// Bytes in the cache now.
    pub fn total(&self) -> u64 {
        self.lock().total
    }

    fn lock(&self) -> parking_lot::MutexGuard<'_, Index> {
        self.index.lock()
    }

    /// The image for `id` at `size`. Counted once, as asked for: the
    /// original a size is made from is not another request.
    pub async fn get(&self, id: &ArtworkId, size: ArtworkSize) -> Result<Artwork, ArtworkError> {
        self.requests.inc();
        let (image, hit) = self.fetch(id, size).await?;
        if hit {
            self.hits.inc();
        }
        Ok(image)
    }

    /// The image, and whether the cache had it.
    async fn fetch(
        &self,
        id: &ArtworkId,
        size: ArtworkSize,
    ) -> Result<(Artwork, bool), ArtworkError> {
        if size == ArtworkSize::Unknown {
            return Err(ArtworkError::BadSize(size));
        }
        if !self.authorized_lately(id) {
            self.catalog.authorize_artwork(id).await?;
            self.remember_authorized(id);
        }
        self.get_authorized(id, size).await
    }

    /// The image, the caller allowed to see it, and whether the cache had
    /// it.
    async fn get_authorized(
        &self,
        id: &ArtworkId,
        size: ArtworkSize,
    ) -> Result<(Artwork, bool), ArtworkError> {
        let key = cache_key(id, size);
        if let Some(hit) = self.read(&key).await? {
            return Ok((hit, true));
        }
        let _held = self.inflight.lock(&key).await;
        // Whoever held the gate before us may have filled it.
        if let Some(hit) = self.read(&key).await? {
            return Ok((hit, true));
        }
        let image = self.make(id, size).await?;
        Ok((self.store(&key, image).await?, false))
    }

    async fn make(&self, id: &ArtworkId, size: ArtworkSize) -> Result<Image, ArtworkError> {
        let Some(width) = size.width() else {
            return Ok(self.catalog.artwork(id).await?);
        };
        // Every size is made from the cached original, so the catalog is
        // asked once per artwork, not once per size.
        let (original, _) = Box::pin(self.get_authorized(id, ArtworkSize::Original)).await?;
        // One resize per core at a time: a grid of new sizes is a decode
        // and a Lanczos3 each, of up to 8192² (review 2026-10-11 L25).
        let _turn = self
            .resizes
            .acquire()
            .await
            .map_err(|e| ArtworkError::Image(e.to_string()))?;
        let original = Image {
            bytes: original.bytes,
            content_type: original.content_type,
        };
        tokio::task::spawn_blocking(move || resize(original, width))
            .await
            .map_err(|e| ArtworkError::Image(e.to_string()))?
    }

    async fn read(&self, key: &str) -> Result<Option<Artwork>, ArtworkError> {
        let Some(entry) = self.lock().touch(key) else {
            return Ok(None);
        };
        match tokio::fs::read(&entry.file).await {
            // Empty: a write a power loss cut short (review 2026-10-11
            // L19). Made again, as if missing.
            Ok(bytes) if bytes.is_empty() => {
                tracing::warn!(file = %entry.file.display(), "a cached artwork is empty; made again");
                let mut index = self.lock();
                if let Some(gone) = index.entries.remove(key) {
                    index.total -= gone.size;
                }
                Ok(None)
            }
            Ok(bytes) => Ok(Some(Artwork {
                bytes,
                content_type: entry.content_type,
                etag: key.to_owned(),
            })),
            // Removed behind our back: forget it and fetch again.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut index = self.lock();
                if let Some(gone) = index.entries.remove(key) {
                    index.total -= gone.size;
                }
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn store(&self, key: &str, image: Image) -> Result<Artwork, ArtworkError> {
        let file = self
            .dir
            .join(format!("{key}.{}", extension(&image.content_type)));
        let tmp = file.with_extension("tmp");
        crate::files::write_synced(&tmp, &image.bytes).await?;
        tokio::fs::rename(&tmp, &file).await?;
        let entry = Entry {
            file,
            content_type: image.content_type.clone(),
            size: image.bytes.len() as u64,
            used: 0,
        };
        let evicted = self.lock().insert(key.to_owned(), entry, self.limit);
        for file in evicted {
            let _ = tokio::fs::remove_file(file).await;
        }
        Ok(Artwork {
            bytes: image.bytes,
            content_type: image.content_type,
            etag: key.to_owned(),
        })
    }
}

/// The widest and tallest artwork decoded (finding 54): the Linux client's
/// limit (C-14). A poster or backdrop is at most 4K wide.
const MAX_SIDE: u32 = 8192;

/// The most one artwork decode may allocate: an 8192 × 8192 RGBA image.
/// The image crate's own default is 512 MiB.
const MAX_ALLOC: u64 = 256 << 20;

/// `bytes`, which come from the catalog (a PMS), as a reader: only JPEG,
/// PNG or WebP, and held to [`MAX_SIDE`] and [`MAX_ALLOC`]. A header past
/// them is refused before anything is decoded.
fn reader(bytes: &[u8]) -> Result<ImageReader<Cursor<&[u8]>>, ArtworkError> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| ArtworkError::Image(e.to_string()))?;
    match reader.format() {
        Some(ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::WebP) => {}
        other => {
            return Err(ArtworkError::Image(format!(
                "{other:?} is not JPEG, PNG or WebP"
            )));
        }
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(MAX_ALLOC);
    reader.limits(limits);
    Ok(reader)
}

/// `e` from the image crate; a limit's names the limits.
fn image_error(e: image::ImageError) -> ArtworkError {
    match e {
        image::ImageError::Limits(e) => ArtworkError::Image(format!(
            "past {MAX_SIDE} × {MAX_SIDE} pixels or {} MiB: {e}",
            MAX_ALLOC >> 20
        )),
        e => ArtworkError::Image(e.to_string()),
    }
}

/// `original` scaled down to `width` (never up). Transparent images stay
/// PNG; everything else becomes a JPEG.
pub(crate) fn resize(original: Image, width: u32) -> Result<Image, ArtworkError> {
    // The size from the header first: a picture no wider is served as it
    // is, and is not decoded at all.
    let (wide, _) = reader(&original.bytes)?
        .into_dimensions()
        .map_err(image_error)?;
    if wide <= width {
        return Ok(original);
    }
    let decoded = reader(&original.bytes)?.decode().map_err(image_error)?;
    let scaled = decoded.resize(width, u32::MAX, FilterType::Lanczos3);
    let mut out = Cursor::new(Vec::new());
    let content_type = if scaled.color().has_alpha() {
        scaled
            .write_to(&mut out, ImageFormat::Png)
            .map_err(|e| ArtworkError::Image(e.to_string()))?;
        "image/png"
    } else {
        JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY)
            .encode_image(&scaled.to_rgb8())
            .map_err(|e| ArtworkError::Image(e.to_string()))?;
        "image/jpeg"
    };
    Ok(Image {
        bytes: out.into_inner(),
        content_type: content_type.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use async_trait::async_trait;
    use spoolframe_catalog::{Found, ItemPage, ItemWithChildren, Page, SearchQuery};
    use spoolframe_types::{Item, Library, LibraryId, MediaId, Namespace, PersonId};

    use super::*;

    /// Serves an 800×400 PNG for every image path, counting fetches and
    /// authorizations; `changed` is its changes, as the live catalog's.
    #[derive(Debug)]
    struct Counting {
        fetches: AtomicUsize,
        authorizations: AtomicUsize,
        denied: std::sync::atomic::AtomicBool,
        changed: tokio::sync::watch::Sender<u64>,
    }

    impl Default for Counting {
        fn default() -> Self {
            Self {
                fetches: AtomicUsize::default(),
                authorizations: AtomicUsize::default(),
                denied: Default::default(),
                changed: tokio::sync::watch::Sender::new(0),
            }
        }
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(width, height, image::Rgb([200, 30, 30]));
        let mut out = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    #[async_trait]
    impl Catalog for Counting {
        async fn libraries(&self) -> Result<Vec<Library>, CatalogError> {
            unreachable!()
        }
        async fn items(&self, _: &LibraryId, _: Page) -> Result<ItemPage, CatalogError> {
            unreachable!()
        }
        async fn item(&self, _: &MediaId) -> Result<ItemWithChildren, CatalogError> {
            unreachable!()
        }
        async fn external_ids(&self, _: &MediaId) -> Result<Vec<String>, CatalogError> {
            unreachable!()
        }
        async fn search(&self, _: &SearchQuery) -> Result<Found, CatalogError> {
            unreachable!()
        }
        async fn person_items(&self, _: &PersonId) -> Result<Vec<Item>, CatalogError> {
            unreachable!()
        }
        async fn media_path(&self, _: &MediaId) -> Result<PathBuf, CatalogError> {
            unreachable!()
        }
        async fn recently_added(&self, _: u32) -> Result<Vec<Item>, CatalogError> {
            unreachable!()
        }
        async fn authorize_artwork(
            &self,
            id: &spoolframe_types::ArtworkId,
        ) -> Result<(), CatalogError> {
            self.authorizations.fetch_add(1, Ordering::SeqCst);
            if self.denied.load(Ordering::SeqCst) {
                return Err(CatalogError::NotServed(id.to_string()));
            }
            Ok(())
        }

        async fn artwork(&self, id: &ArtworkId) -> Result<Image, CatalogError> {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            if id.key().ends_with("/404") {
                return Err(CatalogError::NotFound(id.to_string()));
            }
            // Slow enough for concurrent requests to overlap.
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok(Image {
                bytes: png(800, 400),
                content_type: "image/png".into(),
            })
        }
    }

    /// A directory of the test's own, removed when dropped, and the
    /// cache's place in it, not made yet.
    async fn dir(test: &str) -> (spoolframe_test_dir::TestDir, PathBuf) {
        let tmp = spoolframe_test_dir::TestDir::new(&format!("spoolframe-artwork-{test}")).await;
        let path = tmp.as_ref().join("cache");
        (tmp, path)
    }

    fn id(n: u32) -> ArtworkId {
        ArtworkId::new(Namespace::Plex, format!("/library/metadata/{n}/thumb/1")).unwrap()
    }

    /// The cache, its catalog, and its directory, kept for the test.
    async fn cache(
        test: &str,
        limit: u64,
    ) -> (ArtworkCache, Arc<Counting>, spoolframe_test_dir::TestDir) {
        let catalog = Arc::new(Counting::default());
        let (tmp, path) = dir(test).await;
        (
            ArtworkCache::open(&path, limit, catalog.clone())
                .unwrap()
                .with_changes(catalog.changed.subscribe()),
            catalog,
            tmp,
        )
    }

    fn fetches(c: &Counting) -> usize {
        c.fetches.load(Ordering::SeqCst)
    }

    #[tokio::test]
    async fn removing_a_library_denies_cached_originals_and_resized_images() {
        let (cache, catalog, _dir) = cache("authorization", u64::MAX).await;
        cache.get(&id(1), ArtworkSize::Small).await.unwrap();
        let fetched = catalog.fetches.load(Ordering::SeqCst);
        catalog.denied.store(true, Ordering::SeqCst);
        // The library removed: the catalog is replaced.
        catalog.changed.send_modify(|n| *n += 1);
        for size in [ArtworkSize::Small, ArtworkSize::Original] {
            assert!(matches!(
                cache.get(&id(1), size).await,
                Err(ArtworkError::Catalog(CatalogError::NotServed(_)))
            ));
        }
        assert_eq!(catalog.fetches.load(Ordering::SeqCst), fetched);
    }

    /// A cached poster asked for again is not authorized with the catalog
    /// again (a Plex GET of its item each time), until the catalog changes.
    /// Review 2026-10-11 H3.
    #[tokio::test]
    async fn a_cached_image_is_not_authorized_again_until_the_catalog_changes() {
        let (cache, catalog, _dir) = cache("authorized-once", u64::MAX).await;
        let asked = || catalog.authorizations.load(Ordering::SeqCst);
        for _ in 0..3 {
            cache.get(&id(1), ArtworkSize::Small).await.unwrap();
        }
        assert_eq!(asked(), 1);
        catalog.changed.send_modify(|n| *n += 1);
        cache.get(&id(1), ArtworkSize::Small).await.unwrap();
        assert_eq!(asked(), 2);
    }

    #[tokio::test]
    async fn a_second_request_is_served_from_the_cache() {
        let (cache, catalog, _dir) = cache("hit", u64::MAX).await;
        let first = cache.get(&id(1), ArtworkSize::Original).await.unwrap();
        let second = cache.get(&id(1), ArtworkSize::Original).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(fetches(&catalog), 1);
        assert_eq!(
            cache.counts(),
            ArtworkCounts {
                requests: 2,
                hits: 1
            }
        );
    }

    #[tokio::test]
    async fn parallel_requests_share_one_fetch() {
        let (cache, catalog, _dir) = cache("single-flight", u64::MAX).await;
        let cache = Arc::new(cache);
        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let cache = cache.clone();
                tokio::spawn(async move { cache.get(&id(1), ArtworkSize::Small).await.unwrap() })
            })
            .collect();
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(fetches(&catalog), 1);
    }

    #[tokio::test]
    async fn sizes_are_made_from_one_fetch_of_the_original() {
        let (cache, catalog, _dir) = cache("sizes", u64::MAX).await;
        let small = cache.get(&id(1), ArtworkSize::Small).await.unwrap();
        let medium = cache.get(&id(1), ArtworkSize::Medium).await.unwrap();
        assert_eq!(fetches(&catalog), 1);
        assert_eq!(small.content_type, "image/jpeg");
        assert_eq!(image::load_from_memory(&small.bytes).unwrap().width(), 320);
        assert_eq!(image::load_from_memory(&medium.bytes).unwrap().width(), 640);
        assert_ne!(small.etag, medium.etag);
    }

    #[tokio::test]
    async fn a_size_larger_than_the_original_is_not_scaled_up() {
        let (cache, _, _dir) = cache("no-upscale", u64::MAX).await;
        let large = cache.get(&id(1), ArtworkSize::Large).await.unwrap();
        assert_eq!(image::load_from_memory(&large.bytes).unwrap().width(), 800);
    }

    #[tokio::test]
    async fn the_cache_stays_under_its_limit_evicting_the_least_recently_used() {
        let one = png(800, 400).len() as u64;
        // Room for two originals.
        let (cache, catalog, _dir) = cache("evict", one * 2 + one / 2).await;
        cache.get(&id(1), ArtworkSize::Original).await.unwrap();
        cache.get(&id(2), ArtworkSize::Original).await.unwrap();
        // Use 1 again, so 2 is the least recently used.
        cache.get(&id(1), ArtworkSize::Original).await.unwrap();
        cache.get(&id(3), ArtworkSize::Original).await.unwrap();
        assert!(cache.total() <= one * 2 + one / 2);
        assert_eq!(fetches(&catalog), 3);
        cache.get(&id(1), ArtworkSize::Original).await.unwrap();
        assert_eq!(fetches(&catalog), 3, "1 should still be cached");
        cache.get(&id(2), ArtworkSize::Original).await.unwrap();
        assert_eq!(fetches(&catalog), 4, "2 should have been evicted");
    }

    #[tokio::test]
    async fn a_restart_keeps_what_is_on_disk() {
        let catalog = Arc::new(Counting::default());
        let (_dir, path) = dir("restart").await;
        let first = ArtworkCache::open(&path, u64::MAX, catalog.clone()).unwrap();
        first.get(&id(1), ArtworkSize::Small).await.unwrap();
        drop(first);
        let again = ArtworkCache::open(&path, u64::MAX, catalog.clone()).unwrap();
        let small = again.get(&id(1), ArtworkSize::Small).await.unwrap();
        assert_eq!(small.content_type, "image/jpeg");
        assert_eq!(fetches(&catalog), 1);
    }

    #[tokio::test]
    async fn a_missing_image_is_not_cached_and_says_not_found() {
        let (cache, catalog, _dir) = cache("missing", u64::MAX).await;
        let missing = ArtworkId::new(Namespace::Plex, "/library/metadata/1/thumb/404").unwrap();
        for _ in 0..2 {
            assert!(matches!(
                cache.get(&missing, ArtworkSize::Original).await,
                Err(ArtworkError::Catalog(CatalogError::NotFound(_)))
            ));
        }
        assert_eq!(fetches(&catalog), 2);
        assert_eq!(cache.total(), 0);
    }

    #[tokio::test]
    async fn an_unknown_size_is_refused() {
        let (cache, catalog, _dir) = cache("bad-size", u64::MAX).await;
        assert!(matches!(
            cache.get(&id(1), ArtworkSize::Unknown).await,
            Err(ArtworkError::BadSize(_))
        ));
        assert_eq!(fetches(&catalog), 0);
    }

    fn artwork(bytes: Vec<u8>) -> Image {
        Image {
            bytes,
            content_type: "image/png".into(),
        }
    }

    /// CRC-32 (ISO-HDLC), as PNG chunks carry.
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &b in bytes {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    /// A PNG of only its signature, header and an empty IDAT, saying it is
    /// `width` × `height` RGBA: 45 bytes.
    fn png_header(width: u32, height: u32) -> Vec<u8> {
        let mut chunk = b"IHDR".to_vec();
        chunk.extend(width.to_be_bytes());
        chunk.extend(height.to_be_bytes());
        // 8 bits, RGBA, deflate, adaptive filtering, no interlace.
        chunk.extend([8, 6, 0, 0, 0]);
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend(13u32.to_be_bytes());
        png.extend(&chunk);
        png.extend(crc32(&chunk).to_be_bytes());
        // An empty IDAT: the decoder reads up to the first before it says
        // the size.
        png.extend(0u32.to_be_bytes());
        png.extend(b"IDAT");
        png.extend(crc32(b"IDAT").to_be_bytes());
        png
    }

    /// Finding 54 (G-5): the PMS's bytes were decoded whole before their
    /// size was looked at. A bomb's header says how big it is; past
    /// 8192 × 8192 it is refused from that, before anything is decoded.
    #[test]
    fn a_bomb_is_refused_by_its_header() {
        let err = resize(artwork(png_header(100_000, 100_000)), 320).unwrap_err();
        assert!(err.to_string().contains("8192"), "{err}");
    }

    /// The limit is on the picture, not on what it would cost: a picture
    /// wider than any poster is refused even when it is small.
    #[test]
    fn a_picture_wider_than_the_limit_is_refused() {
        let err = resize(artwork(png(9000, 1)), 320).unwrap_err();
        assert!(err.to_string().contains("8192"), "{err}");
    }

    #[test]
    fn a_picture_within_the_limit_is_resized() {
        let image = resize(artwork(png(800, 400)), 320).unwrap();
        let resized = image::load_from_memory(&image.bytes).unwrap();
        assert_eq!((resized.width(), resized.height()), (320, 160));
    }

    /// A cached file a power loss left empty is made again, not served
    /// empty. Review 2026-10-11 L19.
    #[tokio::test]
    async fn an_empty_cached_file_is_made_again() {
        let (cache, catalog, _dir) = cache("empty", u64::MAX).await;
        cache.get(&id(1), ArtworkSize::Original).await.unwrap();
        let file = cache.lock().entries.values().next().unwrap().file.clone();
        std::fs::write(&file, b"").unwrap();
        let again = cache.get(&id(1), ArtworkSize::Original).await.unwrap();
        assert!(!again.bytes.is_empty());
        assert_eq!(fetches(&catalog), 2);
    }

    /// An image stored again under another content type leaves no file of
    /// the old one behind. Review 2026-10-11 L24.
    #[tokio::test]
    async fn an_image_stored_as_another_type_removes_the_old_file() {
        let (cache, _, dir) = cache("retype", u64::MAX).await;
        let image = |content_type: &str| Image {
            bytes: vec![1, 2, 3],
            content_type: content_type.to_owned(),
        };
        cache.store("k", image("image/png")).await.unwrap();
        cache.store("k", image("image/jpeg")).await.unwrap();
        let files: Vec<_> = std::fs::read_dir(dir.as_ref().join("cache"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(files, ["k.jpg"]);
        assert_eq!(cache.total(), 3);
    }
}
