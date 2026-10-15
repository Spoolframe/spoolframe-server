//! The caches in the metrics: each one's memory, its disk, the reads it
//! answered and what its memory tier let go of. The `cache` label is one
//! of a few fixed names, never a key.

use std::marker::PhantomData;
use std::path::Path;
use std::sync::Arc;

use super::{Counter, Family, Kind, Sample};

/// One cache, as it is now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Bytes in its memory tier.
    pub memory_bytes: u64,
    /// Bytes its disk tier takes on disk; `None` without one.
    pub disk_bytes: Option<u64>,
    /// Reads found in the cache, and not.
    pub hits: u64,
    pub misses: u64,
    /// Entries its memory tier evicted to stay within its limit.
    pub evictions: u64,
}

/// Counts what a foyer memory tier evicts: a listener of its events.
pub struct Evictions<K, V> {
    count: Arc<Counter>,
    entries: PhantomData<fn(K, V)>,
}

impl<K, V> Evictions<K, V> {
    pub fn new(count: Arc<Counter>) -> Self {
        Self {
            count,
            entries: PhantomData,
        }
    }
}

impl<K: 'static, V: 'static> foyer::EventListener for Evictions<K, V> {
    type Key = K;
    type Value = V;

    fn on_leave(&self, reason: foyer::Event, _: &K, _: &V)
    where
        K: foyer::Key,
        V: foyer::Value,
    {
        if reason == foyer::Event::Evict {
            self.count.inc();
        }
    }
}

/// The space the files directly in `dir` take on disk: the blocks they
/// have, not their length, as foyer's are made at their full size and
/// filled as they are written.
pub fn disk_bytes(dir: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.blocks() * 512)
        .sum()
}

/// The caches' families, from each cache's `cache` label and stats.
/// Every family is there with no cache too: the OTLP export makes its
/// instruments of the families there are when it starts.
pub fn families(caches: &[(&str, CacheStats)], families: &mut Vec<Family>) {
    let each = |value: fn(&CacheStats) -> Option<u64>| -> Vec<Sample> {
        caches
            .iter()
            .filter_map(|(cache, stats)| {
                Some(Sample::labelled(&[("cache", cache)], value(stats)? as f64))
            })
            .collect()
    };
    families.push(Family {
        name: "spoolframe_cache_memory_bytes",
        help: "Bytes in each cache's memory tier: artwork, previews, plex_replies.",
        kind: Kind::Gauge,
        samples: each(|s| Some(s.memory_bytes)),
    });
    families.push(Family {
        name: "spoolframe_cache_disk_bytes",
        help: "Bytes each cache's disk tier takes on disk: artwork, previews.",
        kind: Kind::Gauge,
        samples: each(|s| s.disk_bytes),
    });
    families.push(Family {
        name: "spoolframe_cache_requests_total",
        help: "Reads of each cache: found in it (hit) or not (miss).",
        kind: Kind::Counter,
        samples: caches
            .iter()
            .flat_map(|(cache, stats)| {
                [
                    Sample::labelled(&[("cache", cache), ("result", "hit")], stats.hits as f64),
                    Sample::labelled(&[("cache", cache), ("result", "miss")], stats.misses as f64),
                ]
            })
            .collect(),
    });
    families.push(Family {
        name: "spoolframe_cache_evictions_total",
        help: "Entries each cache's memory tier evicted to stay within its limit.",
        kind: Kind::Counter,
        samples: each(|s| Some(s.evictions)),
    });
}

/// Record on the span the read runs in, if it has one, whether the cache
/// had what it read. No span is made for it.
pub fn record_hit(hit: bool) {
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;
    tracing::Span::current().set_attribute("cache.hit", hit);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_family_is_there_without_a_cache_and_disk_only_with_one() {
        let mut none = Vec::new();
        families(&[], &mut none);
        let names: Vec<_> = none.iter().map(|f| f.name).collect();
        assert_eq!(
            names,
            [
                "spoolframe_cache_memory_bytes",
                "spoolframe_cache_disk_bytes",
                "spoolframe_cache_requests_total",
                "spoolframe_cache_evictions_total",
            ]
        );
        let mut some = Vec::new();
        let stats = CacheStats {
            memory_bytes: 10,
            disk_bytes: None,
            hits: 3,
            misses: 1,
            evictions: 2,
        };
        families(&[("plex_replies", stats)], &mut some);
        assert_eq!(some[0].value(&[("cache", "plex_replies")]), Some(10.0));
        assert!(some[1].samples.is_empty(), "no disk tier");
        let requests = &some[2];
        assert_eq!(
            requests.value(&[("cache", "plex_replies"), ("result", "miss")]),
            Some(1.0)
        );
        assert_eq!(some[3].value(&[("cache", "plex_replies")]), Some(2.0));
    }

    #[test]
    fn disk_bytes_are_the_blocks_written_not_the_length() {
        let tmp = spoolframe_test_dir::TestDir::new_blocking("spoolframe-disk-bytes");
        let sparse = std::fs::File::create(tmp.as_ref().join("sparse")).unwrap();
        sparse.set_len(64 << 20).unwrap();
        let written = std::fs::File::create(tmp.as_ref().join("written")).unwrap();
        std::io::Write::write_all(&mut &written, &[1u8; 64 << 10]).unwrap();
        // Synced, so a filesystem that allocates at commit (ZFS) has. Only
        // more than nothing: a compressing one (ZFS, btrfs) keeps 64 KiB
        // of ones in far less.
        written.sync_all().unwrap();
        let bytes = disk_bytes(tmp.as_ref());
        assert!((1..1 << 20).contains(&bytes), "{bytes}");
    }
}
