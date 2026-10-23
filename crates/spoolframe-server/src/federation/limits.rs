//! What a pair may send this server and ask of it (P8-04): the limits
//! P8-02 and P8-03 shipped as constants, operator settings since the
//! 2026-10-23 decision (`federation { … }`), with those constants as
//! their defaults. A very large library syncs once its limits are raised.

use std::sync::atomic::Ordering;
use std::time::Duration;

use super::Federation;
use super::replica::SyncLimits;
use crate::store::federation_tokens::TokenLimits;

/// A pair's limits, as the settings give them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// What one sync, and one origin, may put in the cache (H1).
    pub sync: SyncLimits,
    /// How long a library's walk answers every pair's sync (H2).
    pub walk_reuse: Duration,
    /// The most streams one pair's connection is answered on at once (H2).
    pub max_streams: usize,
    /// Each pair's images in a burst, then a hundred a second (H2).
    pub artwork_burst: u32,
    /// Each pair's assertions in a burst, then one a second (H2).
    pub assertion_burst: u32,
    /// Each pair's subtitle searches and downloads in a burst, then one
    /// every 10 s (P8-07).
    pub subtitle_burst: u32,
    /// The most play tokens that work at once (H3).
    pub tokens: TokenLimits,
    /// The most download tokens that work at once (P8-07): unused ones,
    /// and the download sessions open. Apart from the play tokens' so
    /// that downloads, which live for hours, never take a play's place.
    pub download_tokens: TokenLimits,
}

impl Default for Limits {
    /// Today's values: generous for a big library (a 200,000-item answer
    /// takes 1,000 full pages), a sync and a few images at a time, and a
    /// user's few plays in a token's five minutes.
    fn default() -> Self {
        Self {
            sync: SyncLimits {
                pages: 2_000,
                items: 200_000,
                origin_items: 500_000,
            },
            walk_reuse: Duration::from_secs(60),
            max_streams: spoolframe_federation::MAX_STREAMS,
            artwork_burst: 200,
            assertion_burst: 30,
            subtitle_burst: 20,
            tokens: TokenLimits {
                per_user: 10,
                per_peer: 500,
            },
            // A followed playlist's tracks, each its own download.
            download_tokens: TokenLimits {
                per_user: 200,
                per_peer: 2_000,
            },
        }
    }
}

impl Federation {
    /// The pairs' limits now.
    pub fn limits(&self) -> Limits {
        *self.limits.lock()
    }

    /// New limits: a live setting. A sync, a walk, an ask and a mint read
    /// them as they start; a connection's streams are fixed when it opens,
    /// so the new count holds for the connections made after.
    pub fn set_limits(&self, limits: Limits) {
        *self.limits.lock() = limits;
        self.max_streams
            .store(limits.max_streams, Ordering::Relaxed);
    }
}
