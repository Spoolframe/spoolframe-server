//! What the federation counts for the metrics (P3-14): its syncs, the
//! pairs' requests it answered, its plays through a pair, and, read every
//! few seconds, its pairs and its cache. Every label is one of a few fixed
//! names: never a pair, a user or an item.

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use crate::metrics::{Counter, Histograms, histogram};

/// What a pair asks this server, as the metrics name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerRequest {
    Libraries,
    Sync,
    Artwork,
    Assertion,
    /// A subtitle search, download or removal (P8-07).
    Subtitles,
    /// A notice that what the pair shares with this server changed.
    SharesChanged,
}

impl PeerRequest {
    pub const ALL: [PeerRequest; 6] = [
        PeerRequest::Libraries,
        PeerRequest::Sync,
        PeerRequest::Artwork,
        PeerRequest::Assertion,
        PeerRequest::Subtitles,
        PeerRequest::SharesChanged,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            PeerRequest::Libraries => "libraries",
            PeerRequest::Sync => "sync",
            PeerRequest::Artwork => "artwork",
            PeerRequest::Assertion => "assertion",
            PeerRequest::Subtitles => "subtitles",
            PeerRequest::SharesChanged => "shares_changed",
        }
    }
}

/// How a pair's request went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answered {
    Ok,
    /// Not shared with the pair, not there, or not a request it may make.
    Refused,
    /// Past the pair's pace, or this server could not answer now.
    Unavailable,
}

impl Answered {
    pub const ALL: [Answered; 3] = [Answered::Ok, Answered::Refused, Answered::Unavailable];

    pub fn as_str(self) -> &'static str {
        match self {
            Answered::Ok => "ok",
            Answered::Refused => "refused",
            Answered::Unavailable => "unavailable",
        }
    }
}

/// Whose items a play or download through a pair is of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Items {
    /// This server's, by a pair's users.
    Ours,
    /// A pair's, by this server's users.
    Theirs,
}

impl Items {
    pub const ALL: [Items; 2] = [Items::Ours, Items::Theirs];

    pub fn as_str(self) -> &'static str {
        match self {
            Items::Ours => "ours",
            Items::Theirs => "theirs",
        }
    }
}

/// The pairs and the cache, as last read from the store.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Census {
    /// Live pairs.
    pub peers: u64,
    /// Of them, those this server holds a connection to.
    pub online: u64,
    /// The pairs' artwork kept here, in bytes.
    pub cache_bytes: u64,
}

/// Everything the federation counts, since the server started.
#[derive(Debug, Default)]
pub struct Stats {
    syncs_ok: Counter,
    syncs_failed: Counter,
    /// Unix ms of the last sync that went through; 0 before one.
    last_sync_ms: AtomicI64,
    /// By [`PeerRequest::ALL`], then [`Answered::ALL`].
    requests: [[Counter; 3]; 6],
    /// Plays and downloads by [`Items::ALL`].
    plays: [Counter; 2],
    downloads: [Counter; 2],
    proxied_active: Arc<AtomicU64>,
    proxied_bytes: Arc<Counter>,
    census: parking_lot::RwLock<Census>,
    histograms: OnceLock<Arc<Histograms>>,
}

/// A snapshot of [`Stats`], for the metrics' families.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Counts {
    pub syncs_ok: u64,
    pub syncs_failed: u64,
    pub last_sync_ms: Option<i64>,
    pub requests: Vec<(PeerRequest, Answered, u64)>,
    pub plays: [u64; 2],
    pub downloads: [u64; 2],
    pub proxied_active: u64,
    pub proxied_bytes: u64,
    pub census: Census,
}

/// A proxied playback, counted active until dropped.
#[derive(Debug)]
pub struct Proxying(Arc<AtomicU64>);

impl Drop for Proxying {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Stats {
    /// Observe each sync's time into `histograms` from now on.
    pub fn record_into(&self, histograms: Arc<Histograms>) {
        let _ = self.histograms.set(histograms);
    }

    /// A sync of a pair ended, `ok` or not, after `took`, at `at_ms`.
    pub fn synced(&self, ok: bool, took: Duration, at_ms: i64) {
        let outcome = if ok {
            self.syncs_ok.inc();
            self.last_sync_ms.store(at_ms, Ordering::Relaxed);
            "ok"
        } else {
            self.syncs_failed.inc();
            "failed"
        };
        if let Some(histograms) = self.histograms.get() {
            histograms.observe(
                &histogram::FEDERATION_SYNC_DURATION,
                took.as_secs_f64(),
                &[("outcome", outcome)],
            );
        }
    }

    /// A pair's `request` was answered as `answered`.
    pub fn answered(&self, request: PeerRequest, answered: Answered) {
        let r = PeerRequest::ALL
            .iter()
            .position(|&k| k == request)
            .expect("in PeerRequest::ALL");
        let a = Answered::ALL
            .iter()
            .position(|&k| k == answered)
            .expect("in Answered::ALL");
        self.requests[r][a].inc();
    }

    pub fn played(&self, items: Items) {
        self.plays[items as usize].inc();
    }

    pub fn downloaded(&self, items: Items) {
        self.downloads[items as usize].inc();
    }

    /// A proxied playback began: counted active until the guard drops.
    pub fn proxying(&self) -> Proxying {
        self.proxied_active.fetch_add(1, Ordering::Relaxed);
        Proxying(self.proxied_active.clone())
    }

    /// Where each proxied playback counts the bytes of a pair's media it
    /// relays to a device here.
    pub fn proxied_bytes(&self) -> Arc<Counter> {
        self.proxied_bytes.clone()
    }

    pub fn set_census(&self, census: Census) {
        *self.census.write() = census;
    }

    pub fn counts(&self) -> Counts {
        let last = self.last_sync_ms.load(Ordering::Relaxed);
        Counts {
            syncs_ok: self.syncs_ok.get(),
            syncs_failed: self.syncs_failed.get(),
            last_sync_ms: (last > 0).then_some(last),
            requests: PeerRequest::ALL
                .iter()
                .zip(&self.requests)
                .flat_map(|(&request, by)| {
                    Answered::ALL
                        .iter()
                        .zip(by)
                        .map(move |(&answered, n)| (request, answered, n.get()))
                })
                .collect(),
            plays: [self.plays[0].get(), self.plays[1].get()],
            downloads: [self.downloads[0].get(), self.downloads[1].get()],
            proxied_active: self.proxied_active.load(Ordering::Relaxed),
            proxied_bytes: self.proxied_bytes.get(),
            census: *self.census.read(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_is_counted_under_its_kind_and_outcome_alone() {
        let stats = Stats::default();
        stats.answered(PeerRequest::Artwork, Answered::Refused);
        stats.answered(PeerRequest::Artwork, Answered::Refused);
        stats.answered(PeerRequest::Sync, Answered::Ok);
        let counts = stats.counts();
        let n = |request, answered| {
            counts
                .requests
                .iter()
                .find(|(r, a, _)| *r == request && *a == answered)
                .map(|(_, _, n)| *n)
        };
        assert_eq!(n(PeerRequest::Artwork, Answered::Refused), Some(2));
        assert_eq!(n(PeerRequest::Sync, Answered::Ok), Some(1));
        assert_eq!(n(PeerRequest::Artwork, Answered::Ok), Some(0));
    }

    #[test]
    fn a_proxied_playback_is_active_until_it_drops() {
        let stats = Stats::default();
        let one = stats.proxying();
        let two = stats.proxying();
        assert_eq!(stats.counts().proxied_active, 2);
        drop(one);
        assert_eq!(stats.counts().proxied_active, 1);
        drop(two);
        assert_eq!(stats.counts().proxied_active, 0);
    }

    #[test]
    fn only_a_sync_that_went_through_moves_the_last_sync() {
        let stats = Stats::default();
        stats.synced(false, Duration::from_millis(5), 1_000);
        assert_eq!(stats.counts().last_sync_ms, None);
        stats.synced(true, Duration::from_millis(5), 2_000);
        stats.synced(false, Duration::from_millis(5), 3_000);
        let counts = stats.counts();
        assert_eq!(
            (counts.syncs_ok, counts.syncs_failed, counts.last_sync_ms),
            (1, 2, Some(2_000))
        );
    }
}
