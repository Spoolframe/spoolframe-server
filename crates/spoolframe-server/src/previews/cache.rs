//! Made previews on disk, under a size limit: one directory per file's
//! set, `<key>/`, holding `previews.json` and the sheets `0.jpg`, `1.jpg`,
//! …. A set is the unit of eviction, least recently used first; reading it
//! counts as using it, and its `previews.json`'s modification time keeps
//! that order across restarts (touched when a sheet is read, off the async
//! workers).

use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// What a set is: everything a client needs to find the tile for a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetMeta {
    /// Tile `i` is the picture at `i × interval_ms`.
    pub interval_ms: u64,
    pub tile_width: u32,
    pub tile_height: u32,
    pub columns: u32,
    pub rows: u32,
    /// Tiles in all; the last sheet may be part full.
    pub count: u32,
    pub sheets: u32,
}

const META: &str = "previews.json";
/// A set being written, renamed into place when whole.
const PARTIAL: &str = "partial";

#[derive(Debug, Clone)]
struct Entry {
    meta: SetMeta,
    size: u64,
    /// Larger is more recently used.
    used: u64,
}

#[derive(Debug, Default)]
struct Index {
    sets: HashMap<String, Entry>,
    total: u64,
    clock: u64,
    limit: u64,
}

impl Index {
    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// Remove least recently used sets, never `keep`, until the total fits
    /// the limit. Their keys.
    fn evict(&mut self, keep: Option<&str>) -> Vec<String> {
        let mut gone = Vec::new();
        while self.total > self.limit {
            let Some(oldest) = self
                .sets
                .iter()
                .filter(|(k, _)| Some(k.as_str()) != keep)
                .min_by_key(|(_, e)| e.used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            let entry = self.sets.remove(&oldest).expect("just found");
            self.total -= entry.size;
            gone.push(oldest);
        }
        gone
    }
}

#[derive(Debug)]
pub struct Cache {
    dir: PathBuf,
    index: Mutex<Index>,
}

impl Cache {
    /// The cache in `dir`, holding at most `limit` bytes. Sets already
    /// there are kept, the most recently used last; a set left half
    /// written by a stopped server is removed.
    pub fn open(dir: &Path, limit: u64) -> std::io::Result<Self> {
        crate::files::private_dir(dir)?;
        let mut found = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            // One unreadable entry is skipped, not the whole cache (and the
            // server's start) refused (review 2026-10-11 L24).
            let path = match entry {
                Ok(entry) => entry.path(),
                Err(e) => {
                    tracing::warn!(dir = %dir.display(), error = %e, "a previews cache entry cannot be read; skipped");
                    continue;
                }
            };
            let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
                continue;
            };
            if name.ends_with(PARTIAL) {
                let _ = std::fs::remove_dir_all(&path);
                continue;
            }
            let meta_file = path.join(META);
            let read = std::fs::read(&meta_file)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<SetMeta>(&bytes).ok());
            let Some(meta) = read else {
                // Not a set (or a broken one): nothing serves it.
                let _ = std::fs::remove_dir_all(&path);
                continue;
            };
            let used = std::fs::metadata(&meta_file)
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            found.push((used, name, meta, dir_size(&path)));
        }
        found.sort_by_key(|(used, ..)| *used);
        let mut index = Index {
            limit,
            ..Index::default()
        };
        for (_, key, meta, size) in found {
            let used = index.tick();
            index.total += size;
            index.sets.insert(key, Entry { meta, size, used });
        }
        let cache = Self {
            dir: dir.to_owned(),
            index: Mutex::new(index),
        };
        let gone = cache.lock().evict(None);
        remove_sets(&cache.dir, gone);
        Ok(cache)
    }

    fn lock(&self) -> parking_lot::MutexGuard<'_, Index> {
        self.index.lock()
    }

    /// Remove the evicted sets `keys` on the blocking pool: a set is a
    /// directory of sheets, and a lowered limit can evict hundreds (review
    /// 2026-10-11 L18).
    async fn remove(&self, keys: Vec<String>) {
        if keys.is_empty() {
            return;
        }
        let dir = self.dir.clone();
        if let Err(e) = tokio::task::spawn_blocking(move || remove_sets(&dir, keys)).await {
            tracing::warn!(error = %e, "removing evicted previews stopped");
        }
    }

    /// Bytes in the cache now.
    pub fn total(&self) -> u64 {
        self.lock().total
    }

    /// Change the limit; a lower one evicts at once, the sets' files
    /// removed on the blocking pool.
    pub fn set_limit(&self, limit: u64) {
        let gone = {
            let mut index = self.lock();
            index.limit = limit;
            index.evict(None)
        };
        if gone.is_empty() {
            return;
        }
        let dir = self.dir.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn_blocking(move || remove_sets(&dir, gone));
            }
            Err(_) => remove_sets(&dir, gone),
        }
    }

    /// Whether another set of the usual size fits without evicting one:
    /// previews made ahead of a play stop there, rather than push out
    /// ones made earlier.
    pub fn has_room(&self) -> bool {
        let index = self.lock();
        let usual = match index.sets.len() as u64 {
            0 => 0,
            n => index.total / n,
        };
        index.total + usual < index.limit
    }

    /// The set under `key`, if it is here; counts as a use. Only in
    /// memory: the file system is touched when a sheet is read (review
    /// 2026-10-11 L18).
    pub fn get(&self, key: &str) -> Option<SetMeta> {
        let mut index = self.lock();
        let used = index.tick();
        let entry = index.sets.get_mut(key)?;
        entry.used = used;
        Some(entry.meta)
    }

    /// Sheet `n` of the set under `key`, if both are here.
    pub async fn sheet(&self, key: &str, n: u32) -> Option<Vec<u8>> {
        let meta = self.get(key)?;
        if n >= meta.sheets {
            return None;
        }
        let read = tokio::fs::read(self.sheet_path(key, n)).await;
        match read {
            Ok(bytes) if !bytes.is_empty() => {
                self.touch(key).await;
                Some(bytes)
            }
            // Empty: a write a power loss cut short (review 2026-10-11
            // L19). Removed behind our back, or that: forget the set, so it
            // is made again.
            read => {
                let error = read.err().map_or("empty".to_owned(), |e| e.to_string());
                tracing::warn!(key, n, error, "a previews sheet cannot be read");
                let mut index = self.lock();
                if let Some(gone) = index.sets.remove(key) {
                    index.total -= gone.size;
                }
                None
            }
        }
    }

    /// The set under `key` marked used on disk, so the order outlives a
    /// restart. Only an order: a failure is harmless.
    async fn touch(&self, key: &str) {
        let meta = self.dir.join(key).join(META);
        let _ = tokio::task::spawn_blocking(move || {
            if let Ok(file) = std::fs::File::options().write(true).open(meta) {
                let _ = file.set_modified(SystemTime::now());
            }
        })
        .await;
    }

    fn sheet_path(&self, key: &str, n: u32) -> PathBuf {
        self.dir.join(key).join(format!("{n}.jpg"))
    }

    /// Keep `sheets` as the set under `key`, then evict least recently
    /// used sets, never this one, until the cache fits its limit.
    pub async fn store(&self, key: &str, meta: SetMeta, sheets: &[Vec<u8>]) -> std::io::Result<()> {
        let partial = self.dir.join(format!("{key}.{PARTIAL}"));
        let _ = tokio::fs::remove_dir_all(&partial).await;
        tokio::fs::create_dir_all(&partial).await?;
        let mut size = 0;
        // Each file synced before the set is renamed into place: after a
        // power loss a set is whole or not there (review 2026-10-11 L19).
        for (n, sheet) in sheets.iter().enumerate() {
            crate::files::write_synced(&partial.join(format!("{n}.jpg")), sheet).await?;
            size += sheet.len() as u64;
        }
        let json = serde_json::to_vec(&meta).map_err(std::io::Error::other)?;
        size += json.len() as u64;
        crate::files::write_synced(&partial.join(META), &json).await?;
        let whole = self.dir.join(key);
        let _ = tokio::fs::remove_dir_all(&whole).await;
        tokio::fs::rename(&partial, &whole).await?;
        let gone = {
            let mut index = self.lock();
            let used = index.tick();
            index.total += size;
            if let Some(old) = index
                .sets
                .insert(key.to_owned(), Entry { meta, size, used })
            {
                index.total -= old.size;
            }
            index.evict(Some(key))
        };
        self.remove(gone).await;
        Ok(())
    }
}

/// Remove the sets `keys` from `dir`.
fn remove_sets(dir: &Path, keys: Vec<String>) {
    for key in keys {
        if let Err(e) = std::fs::remove_dir_all(dir.join(&key)) {
            tracing::warn!(key, error = %e, "cannot remove evicted previews");
        }
    }
}

fn dir_size(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(sheets: u32) -> SetMeta {
        SetMeta {
            interval_ms: 10_000,
            tile_width: 320,
            tile_height: 180,
            columns: 10,
            rows: 10,
            count: sheets * 100,
            sheets,
        }
    }

    /// A set of `sheets` sheets of 1000 bytes each, about 1 KB a sheet in
    /// the cache with its `previews.json`.
    async fn put(cache: &Cache, key: &str, sheets: u32) {
        let sheet = vec![0u8; 1000];
        let all = vec![sheet; sheets as usize];
        cache.store(key, meta(sheets), &all).await.unwrap();
    }

    async fn dir(test: &str) -> (spoolframe_test_dir::TestDir, PathBuf) {
        let tmp = spoolframe_test_dir::TestDir::new(&format!("spoolframe-previews-{test}")).await;
        let path = tmp.as_ref().join("previews");
        (tmp, path)
    }

    #[tokio::test]
    async fn the_cache_stays_under_its_limit_evicting_the_least_recently_used() {
        let (_tmp, path) = dir("evict").await;
        // Room for two sets of two sheets.
        let cache = Cache::open(&path, 4500).unwrap();
        put(&cache, "a", 2).await;
        put(&cache, "b", 2).await;
        // a is used again, so b is the least recently used.
        assert!(cache.sheet("a", 0).await.is_some());
        put(&cache, "c", 2).await;
        assert!(cache.total() <= 4500, "{}", cache.total());
        assert!(cache.get("a").is_some());
        assert!(cache.get("b").is_none(), "b should have been evicted");
        assert!(!path.join("b").exists());
        assert!(cache.get("c").is_some());
    }

    #[tokio::test]
    async fn a_set_larger_than_the_limit_is_kept_alone() {
        let (_tmp, path) = dir("alone").await;
        let cache = Cache::open(&path, 1500).unwrap();
        put(&cache, "a", 1).await;
        put(&cache, "big", 3).await;
        assert!(cache.get("big").is_some());
        assert!(cache.get("a").is_none());
    }

    #[tokio::test]
    async fn a_lower_limit_evicts_at_once() {
        let (_tmp, path) = dir("lower").await;
        let cache = Cache::open(&path, u64::MAX).unwrap();
        put(&cache, "a", 2).await;
        put(&cache, "b", 2).await;
        cache.set_limit(2500);
        assert!(cache.get("a").is_none());
        assert!(cache.get("b").is_some());
        assert!(cache.total() <= 2500);
    }

    #[tokio::test]
    async fn a_restart_keeps_the_sets_and_their_order() {
        let (_tmp, path) = dir("restart").await;
        let cache = Cache::open(&path, u64::MAX).unwrap();
        put(&cache, "a", 1).await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        put(&cache, "b", 1).await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        // a is read last, so after a restart b is the oldest.
        cache.sheet("a", 0).await.unwrap();
        drop(cache);
        std::fs::create_dir_all(path.join("c.partial")).unwrap();
        let again = Cache::open(&path, 2500).unwrap();
        assert_eq!(again.get("a"), Some(meta(1)));
        assert!(!path.join("c.partial").exists());
        put(&again, "d", 1).await;
        assert!(again.get("b").is_none(), "b was the least recently used");
        assert!(again.get("a").is_some());
    }

    #[tokio::test]
    async fn previews_made_ahead_stop_where_they_would_evict() {
        let (_tmp, path) = dir("room").await;
        let cache = Cache::open(&path, 3500).unwrap();
        assert!(cache.has_room());
        put(&cache, "a", 1).await;
        put(&cache, "b", 1).await;
        assert!(cache.has_room());
        put(&cache, "c", 1).await;
        assert!(!cache.has_room());
    }

    #[tokio::test]
    async fn only_the_sheets_a_set_has_are_served() {
        let (_tmp, path) = dir("sheets").await;
        let cache = Cache::open(&path, u64::MAX).unwrap();
        put(&cache, "a", 2).await;
        assert_eq!(cache.sheet("a", 1).await.map(|s| s.len()), Some(1000));
        assert_eq!(cache.sheet("a", 2).await, None);
        assert_eq!(cache.sheet("b", 0).await, None);
    }

    /// Looking a set up touches nothing on disk; reading a sheet does, off
    /// the async workers. Review 2026-10-11 L18.
    #[tokio::test]
    async fn looking_a_set_up_touches_nothing_on_disk() {
        let (_tmp, path) = dir("touch").await;
        let cache = Cache::open(&path, u64::MAX).unwrap();
        put(&cache, "a", 1).await;
        let modified = || {
            std::fs::metadata(path.join("a").join(META))
                .unwrap()
                .modified()
                .unwrap()
        };
        let old = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        std::fs::File::options()
            .write(true)
            .open(path.join("a").join(META))
            .unwrap()
            .set_modified(old)
            .unwrap();
        cache.get("a").unwrap();
        assert_eq!(modified(), old);
        cache.sheet("a", 0).await.unwrap();
        assert!(modified() > old);
    }

    /// A sheet a power loss left empty is not served; its set is made
    /// again. Review 2026-10-11 L19.
    #[tokio::test]
    async fn an_empty_sheet_is_not_served() {
        let (_tmp, path) = dir("empty").await;
        let cache = Cache::open(&path, u64::MAX).unwrap();
        put(&cache, "a", 1).await;
        std::fs::write(path.join("a").join("0.jpg"), b"").unwrap();
        assert_eq!(cache.sheet("a", 0).await, None);
        assert!(cache.get("a").is_none());
    }
}
