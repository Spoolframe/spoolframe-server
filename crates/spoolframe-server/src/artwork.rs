//! The artwork proxy's cache (P1-05): images from the catalog, resized on
//! the server, kept on disk under a size limit.
//!
//! Clients never reach Plex for images (§5). The original is fetched once
//! per artwork id and every size is made from it, here rather than by the
//! PMS photo transcoder, so a NativeCatalog (P9) gets resizing for free.
//! Artwork ids change when the image does (Plex paths end in an update
//! timestamp), so nothing is ever invalidated, only evicted.

use std::collections::HashMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use image::ImageFormat;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use spoolframe_catalog::{Catalog, CatalogError, Image};
use spoolframe_protocol::ArtworkSize;
use spoolframe_types::ArtworkId;

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
        if let Some(old) = self.entries.insert(key.clone(), entry) {
            self.total -= old.size;
        }
        let mut evicted = Vec::new();
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
    inflight: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
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
        std::fs::create_dir_all(dir)?;
        let mut found = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
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
            let meta = entry.metadata()?;
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
            inflight: Mutex::default(),
        })
    }

    /// Bytes in the cache now.
    pub fn total(&self) -> u64 {
        self.lock().total
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Index> {
        self.index.lock().expect("never held across a panic")
    }

    /// The image for `id` at `size`.
    pub async fn get(&self, id: &ArtworkId, size: ArtworkSize) -> Result<Artwork, ArtworkError> {
        if size == ArtworkSize::Unknown {
            return Err(ArtworkError::BadSize(size));
        }
        let key = cache_key(id, size);
        if let Some(hit) = self.read(&key).await? {
            return Ok(hit);
        }
        let gate = self.gate(&key);
        let _held = gate.lock().await;
        // Whoever held the gate before us may have filled it.
        if let Some(hit) = self.read(&key).await? {
            self.release(&key);
            return Ok(hit);
        }
        let made = self.make(id, size).await;
        self.release(&key);
        let image = made?;
        self.store(&key, &image).await
    }

    async fn make(&self, id: &ArtworkId, size: ArtworkSize) -> Result<Image, ArtworkError> {
        let Some(width) = size.width() else {
            return Ok(self.catalog.artwork(id).await?);
        };
        // Every size is made from the cached original, so the catalog is
        // asked once per artwork, not once per size.
        let original = Box::pin(self.get(id, ArtworkSize::Original)).await?;
        tokio::task::spawn_blocking(move || resize(original, width))
            .await
            .map_err(|e| ArtworkError::Image(e.to_string()))?
    }

    async fn read(&self, key: &str) -> Result<Option<Artwork>, ArtworkError> {
        let Some(entry) = self.lock().touch(key) else {
            return Ok(None);
        };
        match tokio::fs::read(&entry.file).await {
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

    async fn store(&self, key: &str, image: &Image) -> Result<Artwork, ArtworkError> {
        let file = self
            .dir
            .join(format!("{key}.{}", extension(&image.content_type)));
        let tmp = file.with_extension("tmp");
        tokio::fs::write(&tmp, &image.bytes).await?;
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
            bytes: image.bytes.clone(),
            content_type: image.content_type.clone(),
            etag: key.to_owned(),
        })
    }

    fn gate(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.inflight
            .lock()
            .expect("never held across a panic")
            .entry(key.to_owned())
            .or_default()
            .clone()
    }

    fn release(&self, key: &str) {
        let mut inflight = self.inflight.lock().expect("never held across a panic");
        // Only the last holder removes it; waiters still have a clone.
        if inflight.get(key).is_some_and(|g| Arc::strong_count(g) <= 2) {
            inflight.remove(key);
        }
    }
}

/// `original` scaled down to `width` (never up). Transparent images stay
/// PNG; everything else becomes a JPEG.
fn resize(original: Artwork, width: u32) -> Result<Image, ArtworkError> {
    let decoded =
        image::load_from_memory(&original.bytes).map_err(|e| ArtworkError::Image(e.to_string()))?;
    if decoded.width() <= width {
        return Ok(Image {
            bytes: original.bytes,
            content_type: original.content_type,
        });
    }
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
    use spoolframe_catalog::{ItemPage, ItemWithChildren, Page};
    use spoolframe_types::{Item, Library, LibraryId, MediaId, Namespace};

    use super::*;

    /// Serves an 800×400 PNG for every image path, counting fetches.
    #[derive(Debug, Default)]
    struct Counting {
        fetches: AtomicUsize,
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
        async fn search(&self, _: &str, _: u32) -> Result<Vec<Item>, CatalogError> {
            unreachable!()
        }
        async fn media_path(&self, _: &MediaId) -> Result<PathBuf, CatalogError> {
            unreachable!()
        }
        async fn recently_added(&self, _: u32) -> Result<Vec<Item>, CatalogError> {
            unreachable!()
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
            ArtworkCache::open(&path, limit, catalog.clone()).unwrap(),
            catalog,
            tmp,
        )
    }

    fn fetches(c: &Counting) -> usize {
        c.fetches.load(Ordering::SeqCst)
    }

    #[tokio::test]
    async fn a_second_request_is_served_from_the_cache() {
        let (cache, catalog, _dir) = cache("hit", u64::MAX).await;
        let first = cache.get(&id(1), ArtworkSize::Original).await.unwrap();
        let second = cache.get(&id(1), ArtworkSize::Original).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(fetches(&catalog), 1);
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
}
