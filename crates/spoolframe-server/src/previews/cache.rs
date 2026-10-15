//! Made previews, in memory and on disk, each under a size limit
//! ([`crate::hybrid`]): one entry per file's set, its [`SetMeta`] and
//! every sheet together.
//!
//! One entry, not one per sheet: foyer writes and checksums an entry
//! whole, so after a crash or a power loss a set is whole or missing, as
//! the rename of a whole directory made it before; and the disk tier,
//! which evicts whole blocks, cannot keep some of a set's sheets and not
//! the others. A set is a few MB (a sheet of 100 tiles is a few hundred
//! KB), read whole the first time one of its sheets is: a scrub fetches
//! the others next.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::hybrid::{self, Tiers};
use crate::metrics::Counter;
use crate::metrics::caches::CacheStats;

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

/// A set as the cache keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Set {
    meta: SetMeta,
    sheets: Vec<Vec<u8>>,
}

impl Set {
    fn bytes(&self) -> usize {
        self.sheets.iter().map(Vec::len).sum::<usize>() + std::mem::size_of::<SetMeta>()
    }
}

impl foyer::Code for Set {
    fn encode(&self, writer: &mut impl std::io::Write) -> foyer::Result<()> {
        let m = &self.meta;
        m.interval_ms.encode(writer)?;
        for n in [m.tile_width, m.tile_height, m.columns, m.rows, m.count] {
            n.encode(writer)?;
        }
        (self.sheets.len() as u32).encode(writer)?;
        for sheet in &self.sheets {
            hybrid::put_bytes(writer, sheet)?;
        }
        Ok(())
    }

    fn decode(reader: &mut impl std::io::Read) -> foyer::Result<Self> {
        let interval_ms = u64::decode(reader)?;
        let mut n = || u32::decode(reader);
        let (tile_width, tile_height, columns, rows, count) = (n()?, n()?, n()?, n()?, n()?);
        let sheets = u32::decode(reader)?;
        let sheets = (0..sheets)
            .map(|_| hybrid::get_bytes(reader))
            .collect::<foyer::Result<Vec<_>>>()?;
        Ok(Self {
            meta: SetMeta {
                interval_ms,
                tile_width,
                tile_height,
                columns,
                rows,
                count,
                sheets: sheets.len() as u32,
            },
            sheets,
        })
    }

    fn estimated_size(&self) -> usize {
        32 + self.sheets.len() * 8 + self.bytes()
    }
}

/// About the largest set kept on disk: a three-hour film's at 5 s is
/// about 10 MB. A larger one is kept in memory only.
const LARGEST: u64 = 16 << 20;

/// The file that says how much has been put on disk since the disk tier
/// was laid out: `<bytes> <sets>`.
const FILLED: &str = "filled";

/// Bytes and sets put on disk since the disk tier was laid out. The disk
/// tier never says how full it is, and a set is never put twice, so this
/// is how full it is until it passes the limit, when the oldest blocks
/// go: from then on it is full.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Filled {
    bytes: u64,
    sets: u64,
}

impl Filled {
    fn read(dir: &Path) -> Self {
        let text = std::fs::read_to_string(dir.join(FILLED)).unwrap_or_default();
        let mut parts = text.split_whitespace().map(str::parse::<u64>);
        match (parts.next(), parts.next()) {
            (Some(Ok(bytes)), Some(Ok(sets))) => Self { bytes, sets },
            // Missing, or a write cut short: counted from nothing, which
            // at worst makes a few sets ahead that push out older ones.
            _ => Self::default(),
        }
    }
}

#[derive(Debug)]
pub struct Cache {
    cache: foyer::HybridCache<String, Set>,
    dir: PathBuf,
    tiers: Tiers,
    /// Held while it is written to [`FILLED`], so the last count is the
    /// one on disk.
    filled: tokio::sync::Mutex<Filled>,
    /// [`Self::filled`], read without waiting.
    seen: parking_lot::Mutex<Filled>,
    /// Sets and sheets read, found or not; what the memory tier evicted.
    hits: Counter,
    misses: Counter,
    evictions: Arc<Counter>,
}

impl Cache {
    /// The cache in `dir`, holding at most `tiers`' bytes in memory and on
    /// disk. Sets already on disk are kept; a `dir` of an older server's,
    /// a directory per set, is emptied.
    pub async fn open(dir: &Path, tiers: Tiers) -> std::io::Result<Self> {
        let evictions = Arc::new(Counter::default());
        let cache = hybrid::open(
            dir,
            hybrid::Options {
                name: "previews",
                tiers,
                largest: LARGEST,
                weigh: |key: &String, set: &Set| key.len() + set.bytes(),
                evictions: evictions.clone(),
            },
        )
        .await?;
        // After `open`, which empties a `dir` laid out otherwise.
        let filled = Filled::read(dir);
        Ok(Self {
            cache,
            dir: dir.to_owned(),
            tiers,
            filled: tokio::sync::Mutex::new(filled),
            seen: parking_lot::Mutex::new(filled),
            hits: Counter::default(),
            misses: Counter::default(),
            evictions,
        })
    }

    /// Bytes put on disk, up to its limit: what it holds.
    pub fn total(&self) -> u64 {
        self.seen.lock().bytes.min(self.tiers.disk)
    }

    /// Its memory, disk, reads and evictions now (P3-14).
    pub fn stats(&self) -> CacheStats {
        CacheStats {
            memory_bytes: self.memory_bytes(),
            disk_bytes: Some(crate::metrics::caches::disk_bytes(&self.dir)),
            hits: self.hits.get(),
            misses: self.misses.get(),
            evictions: self.evictions.get(),
        }
    }

    /// Bytes in the memory tier now.
    pub fn memory_bytes(&self) -> u64 {
        self.cache.memory().usage() as u64
    }

    /// Change the memory tier's limit; a lower one evicts from memory at
    /// once, the sets staying on disk. The disk tier's is fixed at start.
    pub fn set_memory_limit(&self, bytes: u64) {
        if let Err(e) = self.cache.memory().resize(bytes as usize) {
            tracing::warn!(error = %e, "cannot change the previews' memory limit");
        }
    }

    /// Writes still queued for the disk finished, and the disk tier
    /// closed: as the server stops.
    pub async fn close(&self) {
        if let Err(e) = self.cache.close().await {
            tracing::warn!(error = %e, "the previews cache did not close cleanly");
        }
    }

    /// Whether another set of the usual size fits without evicting one:
    /// previews made ahead of a play stop there, rather than push out
    /// ones made earlier.
    pub fn has_room(&self) -> bool {
        let filled = *self.seen.lock();
        let usual = filled.bytes.checked_div(filled.sets).unwrap_or(0);
        filled.bytes + usual < self.tiers.disk
    }

    /// The set under `key`, read from disk into memory if it is only
    /// there.
    async fn set(&self, key: &str) -> Option<foyer::HybridCacheEntry<String, Set>> {
        match self.cache.get(key).await {
            Ok(set) => set,
            Err(e) => {
                tracing::warn!(key, error = %e, "a previews set cannot be read");
                None
            }
        }
    }

    /// The set under `key`, counted as a read, found or not, and the
    /// span it is read in told which.
    async fn read(&self, key: &str) -> Option<foyer::HybridCacheEntry<String, Set>> {
        let set = self.set(key).await;
        if set.is_some() {
            self.hits.inc();
        } else {
            self.misses.inc();
        }
        crate::metrics::caches::record_hit(set.is_some());
        set
    }

    /// The set under `key`, if it is here; counts as a use.
    pub async fn get(&self, key: &str) -> Option<SetMeta> {
        Some(self.read(key).await?.value().meta)
    }

    /// Whether the set under `key` is here: not a read a client asked
    /// for, so not counted as one.
    pub async fn has(&self, key: &str) -> bool {
        self.set(key).await.is_some()
    }

    /// Sheet `n` of the set under `key`, if both are here.
    pub async fn sheet(&self, key: &str, n: u32) -> Option<Vec<u8>> {
        let set = self.read(key).await?;
        set.value().sheets.get(n as usize).cloned()
    }

    /// Keep `sheets` as the set under `key`: in memory now, on disk as
    /// soon as it is written there, whole.
    pub async fn store(&self, key: &str, meta: SetMeta, sheets: &[Vec<u8>]) -> std::io::Result<()> {
        let set = Set {
            meta,
            sheets: sheets.to_vec(),
        };
        let bytes = set.bytes() as u64;
        self.cache.insert(key.to_owned(), set);
        let mut filled = self.filled.lock().await;
        filled.bytes += bytes;
        filled.sets += 1;
        *self.seen.lock() = *filled;
        let text = format!("{} {}\n", filled.bytes, filled.sets);
        let partial = self.dir.join(format!("{FILLED}.partial"));
        crate::files::write_synced(&partial, text.as_bytes()).await?;
        tokio::fs::rename(&partial, self.dir.join(FILLED)).await
    }
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

    /// A set of `sheets` sheets of 1000 bytes each, sheet `n` all `n`.
    async fn put(cache: &Cache, key: &str, sheets: u32) {
        let all: Vec<_> = (0..sheets).map(|n| vec![n as u8; 1000]).collect();
        cache.store(key, meta(sheets), &all).await.unwrap();
    }

    async fn dir(test: &str) -> (spoolframe_test_dir::TestDir, PathBuf) {
        let tmp = spoolframe_test_dir::TestDir::new(&format!("spoolframe-previews-{test}")).await;
        let path = tmp.as_ref().join("previews");
        (tmp, path)
    }

    fn tiers(memory: u64, disk: u64) -> Tiers {
        Tiers { memory, disk }
    }

    fn in_memory(cache: &Cache, key: &str) -> bool {
        cache.cache.memory().contains(key)
    }

    /// The memory tier keeps the sets used last, within its limit; a set it
    /// lets go of is still on disk, whole.
    #[tokio::test]
    async fn memory_keeps_the_most_recently_used_and_disk_the_rest() {
        let (_tmp, path) = dir("evict").await;
        // Room for two sets of two sheets in memory.
        let cache = Cache::open(&path, tiers(4500, 8 << 20)).await.unwrap();
        put(&cache, "a", 2).await;
        put(&cache, "b", 2).await;
        // a is used again, so b is the least recently used.
        assert!(cache.sheet("a", 0).await.is_some());
        put(&cache, "c", 2).await;
        assert!(cache.memory_bytes() <= 4500, "{}", cache.memory_bytes());
        assert!(in_memory(&cache, "a") && in_memory(&cache, "c"));
        assert!(!in_memory(&cache, "b"), "b should have left memory");
        assert_eq!(cache.sheet("b", 1).await, Some(vec![1; 1000]));
        assert_eq!(cache.get("b").await, Some(meta(2)));
        let stats = cache.stats();
        assert!(stats.evictions >= 1, "{stats:?}");
        assert_eq!((stats.hits, stats.misses), (3, 0));
        assert!(stats.disk_bytes.unwrap() > 0);
    }

    /// A lower memory limit, a live setting, evicts from memory at once.
    #[tokio::test]
    async fn a_lower_memory_limit_evicts_at_once() {
        let (_tmp, path) = dir("lower").await;
        let cache = Cache::open(&path, tiers(1 << 20, 8 << 20)).await.unwrap();
        put(&cache, "a", 2).await;
        put(&cache, "b", 2).await;
        cache.set_memory_limit(2500);
        assert!(cache.memory_bytes() <= 2500, "{}", cache.memory_bytes());
        assert!(!in_memory(&cache, "a"));
        assert!(in_memory(&cache, "b"));
    }

    #[tokio::test]
    async fn a_restart_keeps_the_sets_and_how_full_the_disk_is() {
        let (_tmp, path) = dir("restart").await;
        let roomy = tiers(1 << 20, 8 << 20);
        let cache = Cache::open(&path, roomy).await.unwrap();
        put(&cache, "a", 3).await;
        let total = cache.total();
        assert!(total >= 3000, "{total}");
        cache.close().await;
        drop(cache);
        let again = Cache::open(&path, roomy).await.unwrap();
        assert_eq!(again.get("a").await, Some(meta(3)));
        assert_eq!(again.sheet("a", 2).await, Some(vec![2; 1000]));
        assert_eq!(again.total(), total);
    }

    /// A set a power loss left part written (here, one byte of a sheet
    /// changed on disk) fails its checksum: it is missing, to be made
    /// again, never served in part.
    #[tokio::test]
    async fn a_set_damaged_on_disk_is_missing_not_served_in_part() {
        let (_tmp, path) = dir("damaged").await;
        let roomy = tiers(1 << 20, 8 << 20);
        let cache = Cache::open(&path, roomy).await.unwrap();
        put(&cache, "a", 3).await;
        cache.close().await;
        drop(cache);
        // Sheet 2 is 1000 bytes of 2: find it in foyer's files.
        let sheet = vec![2u8; 1000];
        let damaged = std::fs::read_dir(&path).unwrap().flatten().any(|file| {
            let mut bytes = std::fs::read(file.path()).unwrap();
            let Some(at) = bytes.windows(1000).position(|w| w == sheet) else {
                return false;
            };
            bytes[at + 500] = 3;
            std::fs::write(file.path(), bytes).unwrap();
            true
        });
        assert!(damaged, "sheet 2 is not on disk");
        let again = Cache::open(&path, roomy).await.unwrap();
        assert_eq!(again.get("a").await, None);
        assert_eq!(again.sheet("a", 0).await, None);
    }

    /// An older server's cache, a directory per set, is not read: it is
    /// emptied, and its sets made again as they are asked for.
    #[tokio::test]
    async fn an_older_server_s_sets_are_removed() {
        let (_tmp, path) = dir("older").await;
        std::fs::create_dir_all(path.join("a")).unwrap();
        std::fs::write(path.join("a").join("previews.json"), b"{}").unwrap();
        std::fs::create_dir_all(path.join("b.partial")).unwrap();
        let cache = Cache::open(&path, tiers(1 << 20, 8 << 20)).await.unwrap();
        assert!(!path.join("a").exists() && !path.join("b.partial").exists());
        assert_eq!(cache.get("a").await, None);
        assert_eq!(cache.total(), 0);
    }

    #[tokio::test]
    async fn previews_made_ahead_stop_where_they_would_evict() {
        let (_tmp, path) = dir("room").await;
        let cache = Cache::open(&path, tiers(1 << 20, 3500)).await.unwrap();
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
        let cache = Cache::open(&path, tiers(1 << 20, 8 << 20)).await.unwrap();
        put(&cache, "a", 2).await;
        assert_eq!(cache.sheet("a", 1).await.map(|s| s.len()), Some(1000));
        assert_eq!(cache.sheet("a", 2).await, None);
        assert_eq!(cache.sheet("b", 0).await, None);
        // A sheet past the set's last is a read of a set that is there.
        let stats = cache.stats();
        assert_eq!((stats.hits, stats.misses), (2, 1));
    }

    #[test]
    fn a_count_cut_short_counts_from_nothing() {
        let tmp = spoolframe_test_dir::TestDir::new_blocking("spoolframe-previews-filled");
        let dir = tmp.as_ref();
        std::fs::write(dir.join(FILLED), "4096 2\n").unwrap();
        assert_eq!(
            Filled::read(dir),
            Filled {
                bytes: 4096,
                sets: 2
            }
        );
        std::fs::write(dir.join(FILLED), "40").unwrap();
        assert_eq!(Filled::read(dir), Filled::default());
    }
}
