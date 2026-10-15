//! Caches with a memory tier in front of a disk tier, on foyer: the
//! artwork's and the previews'.
//!
//! Every entry is written to disk when it is put in, not when the memory
//! tier evicts it, so what was made survives a crash as well as a restart.
//! foyer checksums each entry and skips one it cannot read back whole, so
//! after a power loss an entry is whole or missing. The memory tier evicts
//! the least recently used entry; the disk tier evicts whole blocks, the
//! oldest written first.

use std::path::Path;
use std::sync::Arc;

use crate::metrics::Counter;
use crate::metrics::caches::Evictions;

use foyer::{
    BlockEngineConfig, Code, DeviceBuilder, FsDeviceBuilder, HybridCache, HybridCacheBuilder,
    HybridCachePolicy, LruConfig, RecoverMode, StorageKey, StorageValue,
};

/// The most a disk block holds, and so the largest entry kept on disk.
const MOST_BLOCK: u64 = 64 << 20;
/// The least: a test's tiny cache still has a few blocks.
const LEAST_BLOCK: u64 = 64 << 10;
/// Blocks a disk tier is cut into, at most: the unit it evicts.
const BLOCKS: u64 = 64;
/// The smallest disk tier: one asked to be smaller (a test's) is this.
const LEAST_DISK: u64 = 16 * LEAST_BLOCK;

/// The file that says how the disk tier was laid out.
const LAYOUT: &str = "layout";

/// A cache's two sizes, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tiers {
    pub memory: u64,
    pub disk: u64,
}

impl Tiers {
    /// The disk tier's size: at least [`LEAST_DISK`].
    fn disk(&self) -> u64 {
        self.disk.max(LEAST_DISK)
    }

    /// The disk tier's block: a 64th of it, from 64 KiB to `largest`.
    fn block(&self, largest: u64) -> u64 {
        (self.disk() / BLOCKS).clamp(LEAST_BLOCK, largest.max(LEAST_BLOCK))
    }

    /// The memory tier's shards, each a share of it locked on its own: up
    /// to 8, as many as still hold the `largest` entry each (one larger
    /// than its share is not kept in memory).
    fn shards(&self, largest: u64) -> usize {
        (self.memory / largest.max(1)).clamp(1, 8) as usize
    }
}

/// How a cache is opened.
pub struct Options<K, V> {
    /// For foyer's own logs.
    pub name: &'static str,
    pub tiers: Tiers,
    /// About the largest entry the disk tier keeps (up to 64 MiB): its
    /// blocks are this big, or a 64th of the disk tier when that is
    /// smaller. Its two write buffers are a block each, in memory outside
    /// the memory tier.
    pub largest: u64,
    /// An entry's size in memory, in bytes.
    pub weigh: fn(&K, &V) -> usize,
    /// Counts the entries the memory tier evicts.
    pub evictions: Arc<Counter>,
}

/// The cache in `dir`, made if missing. A `dir` laid out otherwise (by an
/// older server, or with other sizes) is emptied first: it is only a
/// cache, and foyer cannot read blocks of another size.
pub async fn open<K, V>(dir: &Path, options: Options<K, V>) -> std::io::Result<HybridCache<K, V>>
where
    K: StorageKey + Clone,
    V: StorageValue,
{
    crate::files::private_dir(dir)?;
    let block = options.tiers.block(options.largest.min(MOST_BLOCK));
    let disk = options.tiers.disk();
    let layout = format!("foyer 0.22, disk {disk}, block {block}\n");
    let laid_out = std::fs::read_to_string(dir.join(LAYOUT)).ok();
    if laid_out.as_deref() != Some(layout.as_str()) {
        empty(dir)?;
        if laid_out.is_none() {
            tracing::info!(dir = %dir.display(), "the {} cache starts empty: a new layout", options.name);
        } else {
            tracing::info!(dir = %dir.display(), "the {} cache starts empty: its size changed", options.name);
        }
    }
    let device = FsDeviceBuilder::new(dir)
        .with_capacity(disk as usize)
        .build()
        .map_err(std::io::Error::other)?;
    let cache = HybridCacheBuilder::new()
        .with_name(options.name)
        .with_event_listener(Arc::new(Evictions::new(options.evictions)))
        .with_policy(HybridCachePolicy::WriteOnInsertion)
        .memory(options.tiers.memory as usize)
        .with_shards(options.tiers.shards(options.largest))
        .with_eviction_config(LruConfig {
            high_priority_pool_ratio: 0.0,
        })
        .with_weighter(options.weigh)
        .storage()
        .with_engine_config(
            BlockEngineConfig::new(device)
                .with_block_size(block as usize)
                // An entry is written whole from a buffer, so one holds a
                // block's entry. A larger queue drops what comes after.
                .with_buffer_pool_size(block as usize)
                .with_submit_queue_size_threshold(2 * block as usize)
                // Entries are put again (a set made again), so a restart
                // must not bring back one that was replaced or removed.
                .with_tombstone_log(true),
        )
        .with_recover_mode(RecoverMode::Quiet)
        .build()
        .await
        .map_err(std::io::Error::other)?;
    // Written once foyer has its files: a crash before this empties the
    // cache again next time, which is harmless.
    crate::files::write_synced(&dir.join(LAYOUT), layout.as_bytes()).await?;
    Ok(cache)
}

/// Everything in `dir` removed, `dir` kept.
fn empty(dir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() && !path.is_symlink() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// `bytes` written with their length before them, for [`Code`].
pub fn put_bytes(writer: &mut impl std::io::Write, bytes: &[u8]) -> foyer::Result<()> {
    (bytes.len() as u64).encode(writer)?;
    writer.write_all(bytes).map_err(foyer::Error::io_error)
}

/// What [`put_bytes`] wrote.
pub fn get_bytes(reader: &mut impl std::io::Read) -> foyer::Result<Vec<u8>> {
    use std::io::Read as _;
    let len = u64::decode(reader)?;
    let mut bytes = Vec::new();
    // `take`, not a buffer of `len`: a length read from a damaged entry
    // does not allocate past what is there.
    std::io::Read::take(reader, len)
        .read_to_end(&mut bytes)
        .map_err(foyer::Error::io_error)?;
    if bytes.len() as u64 != len {
        return Err(foyer::Error::io_error(
            std::io::ErrorKind::UnexpectedEof.into(),
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_read_back_as_written_and_a_short_entry_is_refused() {
        let mut out = Vec::new();
        put_bytes(&mut out, b"poster").unwrap();
        put_bytes(&mut out, b"").unwrap();
        let mut reader = out.as_slice();
        assert_eq!(get_bytes(&mut reader).unwrap(), b"poster");
        assert_eq!(get_bytes(&mut reader).unwrap(), b"");
        let cut = &out[..out.len() - 9];
        let mut reader = cut;
        assert!(get_bytes(&mut reader).is_err());
    }

    #[test]
    fn blocks_are_a_64th_of_the_disk_within_bounds() {
        let tiers = |disk| Tiers { memory: 0, disk };
        assert_eq!(tiers(1024 << 20).block(64 << 20), 16 << 20);
        assert_eq!(tiers(4096 << 20).block(64 << 20), 64 << 20);
        assert_eq!(tiers(4096 << 20).block(16 << 20), 16 << 20);
        assert_eq!(tiers(1 << 20).block(64 << 20), 64 << 10);
        assert_eq!(tiers(0).disk(), 1 << 20);
        let memory = |memory| Tiers { memory, disk: 0 };
        assert_eq!(memory(128 << 20).shards(16 << 20), 8);
        assert_eq!(memory(64 << 20).shards(16 << 20), 4);
        assert_eq!(memory(64 << 20).shards(64 << 20), 1);
        assert_eq!(memory(0).shards(64 << 20), 1);
    }

    fn options(tiers: Tiers) -> Options<u64, Vec<u8>> {
        Options {
            name: "test",
            tiers,
            largest: 64 << 20,
            weigh: |_, v| v.len(),
            evictions: Arc::default(),
        }
    }

    #[tokio::test]
    async fn a_restart_keeps_what_was_put_and_another_size_empties_it() {
        let tmp = spoolframe_test_dir::TestDir::new("spoolframe-hybrid-restart").await;
        let dir = tmp.as_ref().join("cache");
        let tiers = Tiers {
            memory: 1 << 20,
            disk: 4 << 20,
        };
        let cache = open(&dir, options(tiers)).await.unwrap();
        cache.insert(1, vec![7; 1000]);
        cache.close().await.unwrap();
        drop(cache);
        let again = open(&dir, options(tiers)).await.unwrap();
        let kept = again.get(&1).await.unwrap().map(|e| e.value().clone());
        assert_eq!(kept, Some(vec![7; 1000]));
        again.close().await.unwrap();
        drop(again);
        let resized = Tiers {
            disk: 8 << 20,
            ..tiers
        };
        let emptied = open(&dir, options(resized)).await.unwrap();
        assert!(emptied.get(&1).await.unwrap().is_none());
    }

    /// More than the disk holds pushes the oldest out of both tiers.
    #[tokio::test]
    async fn the_disk_tier_stays_within_its_size() {
        let tmp = spoolframe_test_dir::TestDir::new("spoolframe-hybrid-full").await;
        let dir = tmp.as_ref().join("cache");
        // 64 KiB blocks; 32 KiB entries, so 2 MiB holds about 64 of them.
        let tiers = Tiers {
            memory: 64 << 10,
            disk: 2 << 20,
        };
        let cache = open(&dir, options(tiers)).await.unwrap();
        for n in 0..200 {
            cache.insert(n, vec![n as u8; 32 << 10]);
            // As artwork comes in: not all in one go, which the write
            // queue would drop.
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        cache.close().await.unwrap();
        assert!(cache.get(&0).await.unwrap().is_none(), "0 should be gone");
        let last = cache.get(&199).await.unwrap().map(|e| e.value().len());
        assert_eq!(last, Some(32 << 10));
    }

    #[tokio::test]
    async fn an_older_layout_is_removed() {
        let tmp = spoolframe_test_dir::TestDir::new("spoolframe-hybrid-older").await;
        let dir = tmp.as_ref().join("cache");
        std::fs::create_dir_all(dir.join("set")).unwrap();
        std::fs::write(dir.join("set").join("0.jpg"), b"old").unwrap();
        std::fs::write(dir.join("0123abcd.jpg"), b"old").unwrap();
        let tiers = Tiers {
            memory: 1 << 20,
            disk: 4 << 20,
        };
        let _cache = open(&dir, options(tiers)).await.unwrap();
        assert!(!dir.join("set").exists());
        assert!(!dir.join("0123abcd.jpg").exists());
        assert!(dir.join(LAYOUT).exists());
    }
}
