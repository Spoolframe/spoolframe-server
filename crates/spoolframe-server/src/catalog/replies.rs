//! The catalog's replies kept in memory for a short while (foyer, without
//! a disk tier), so Home and browsing ask the Plex servers less.
//!
//! Only replies with nothing of a user's in them are kept, and that is so
//! by construction: a catalog's items carry no watch state (the Plex
//! catalog drops Plex's `viewOffset`, `viewCount` and `lastViewedAt`), and
//! each user's state is filled in from Spoolframe's own store after the
//! catalog is read ([`crate::watch::Watch::fill`]). A position written or
//! a sync from Plex a moment ago therefore shows on the next read, kept
//! reply or not. Plex's watch states themselves (the sync's) are read
//! from Plex, never from here.
//!
//! What can go stale is the library itself: an item added, edited or
//! removed in Plex. A reply is used for [`KEPT_FOR`] after it was asked
//! for, then asked again; and a replaced catalog (a library or Plex server
//! added, removed or changed) forgets every reply, one still being fetched
//! as it was replaced among them.
//!
//! Not kept: searches (each is typed once), files' paths (a playback
//! checks the file), artwork and preview indexes (their own caches).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use spoolframe_catalog::{CatalogError, ItemPage, ItemWithChildren};
use spoolframe_types::{Item, Library};

use crate::metrics::Counter;
use crate::metrics::caches::{CacheStats, Evictions};

/// How long a reply is used: a new episode shows on Home within this.
pub const KEPT_FOR: Duration = Duration::from_secs(60);

/// A catalog call, by what it asks for: ids as their text.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Call {
    Libraries,
    /// A library, the offset and the limit.
    Items(String, u32, u32),
    Item(String),
    Episodes(String),
    ExternalIds(String),
    PersonItems(String),
    RecentlyAdded(u32),
}

impl Call {
    /// Whether the call is about a paired server's `fed:` id: an item, a
    /// library or a person of theirs.
    fn is_federated(&self) -> bool {
        match self {
            Call::Libraries | Call::RecentlyAdded(_) => false,
            Call::Items(id, ..)
            | Call::Item(id)
            | Call::Episodes(id)
            | Call::ExternalIds(id)
            | Call::PersonItems(id) => spoolframe_protocol::federation::federated_id(id),
        }
    }
}

/// What a call answered.
#[derive(Debug, Clone)]
pub enum Reply {
    Libraries(Vec<Library>),
    Page(ItemPage),
    Item(Box<ItemWithChildren>),
    List(Vec<Item>),
    Ids(Vec<String>),
}

/// A reply, when it was asked for, and under which catalog.
#[derive(Debug)]
struct Kept {
    at: Instant,
    generation: u64,
    reply: Reply,
}

/// Replies let go of before the memory limit made them go: all of them
/// as the catalog was replaced (counted once per replace), and each one
/// found past [`KEPT_FOR`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Invalidated {
    pub replaced: u64,
    pub expired: u64,
}

pub struct Replies {
    cache: foyer::Cache<Call, Kept>,
    /// Bumped as the catalog is replaced: a reply of another is not used.
    generation: AtomicU64,
    /// [`KEPT_FOR`], but in tests.
    kept_for: Duration,
    /// The limit last set: foyer says only the one it was built with.
    limit: AtomicU64,
    hits: Counter,
    misses: Counter,
    evictions: Arc<Counter>,
    replaced: Counter,
    expired: Counter,
}

impl std::fmt::Debug for Replies {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Replies")
            .field("limit", &self.limit())
            .finish_non_exhaustive()
    }
}

impl Replies {
    /// At most `bytes` of replies, about: a reply weighs as its items do
    /// as JSON.
    pub fn new(bytes: u64) -> Self {
        let evictions = Arc::new(Counter::default());
        let cache = foyer::CacheBuilder::new(bytes as usize)
            .with_name("plex-replies")
            .with_event_listener(Arc::new(Evictions::new(evictions.clone())))
            .with_eviction_config(foyer::LruConfig {
                high_priority_pool_ratio: 0.0,
            })
            .with_weighter(|call: &Call, kept: &Kept| weigh(call, &kept.reply))
            .build();
        Self {
            cache,
            generation: AtomicU64::new(0),
            kept_for: KEPT_FOR,
            limit: AtomicU64::new(bytes),
            hits: Counter::default(),
            misses: Counter::default(),
            evictions,
            replaced: Counter::default(),
            expired: Counter::default(),
        }
    }

    /// Forget every reply: the catalog was replaced.
    pub fn forget(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.cache.clear();
        self.replaced.inc();
    }

    /// Change the limit; a lower one evicts at once.
    pub fn set_limit(&self, bytes: u64) {
        self.limit.store(bytes, Ordering::Relaxed);
        if let Err(e) = self.cache.resize(bytes as usize) {
            tracing::warn!(error = %e, "cannot change the Plex replies' memory limit");
        }
    }

    /// The most they may take, in bytes.
    pub fn limit(&self) -> u64 {
        self.limit.load(Ordering::Relaxed)
    }

    /// Bytes kept now.
    pub fn memory_bytes(&self) -> u64 {
        self.cache.usage() as u64
    }

    /// Their memory, reads and evictions now; no disk (P3-14).
    pub fn stats(&self) -> CacheStats {
        CacheStats {
            memory_bytes: self.memory_bytes(),
            disk_bytes: None,
            hits: self.hits.get(),
            misses: self.misses.get(),
            evictions: self.evictions.get(),
        }
    }

    pub fn invalidated(&self) -> Invalidated {
        Invalidated {
            replaced: self.replaced.get(),
            expired: self.expired.get(),
        }
    }

    /// `call`'s reply, kept if it was asked for less than [`KEPT_FOR`] ago
    /// under this catalog; otherwise `fetch`ed, and kept if it succeeds.
    /// `of` finds the reply's value, `into` makes one of it. A paired
    /// server's call is only `fetch`ed.
    pub async fn read<T: Clone>(
        &self,
        call: Call,
        of: fn(&Reply) -> Option<&T>,
        into: fn(T) -> Reply,
        fetch: impl Future<Output = Result<T, CatalogError>>,
    ) -> Result<T, CatalogError> {
        // Replies are kept for Plex's answers only: a paired server's
        // cache (`fed:` ids) is read from this server's own database, and
        // a sync changes it at any time.
        if call.is_federated() {
            return fetch.await;
        }
        // Before `fetch` reads which catalog is served (it is not polled
        // yet): a reply of one replaced after this is never kept.
        let generation = self.generation.load(Ordering::SeqCst);
        if let Some(entry) = self.cache.get(&call) {
            let kept = entry.value();
            let fresh = kept.at.elapsed() < self.kept_for;
            if kept.generation == generation
                && fresh
                && let Some(value) = of(&kept.reply)
            {
                self.hits.inc();
                crate::metrics::caches::record_hit(true);
                return Ok(value.clone());
            }
            if !fresh {
                self.expired.inc();
            }
        }
        self.misses.inc();
        crate::metrics::caches::record_hit(false);
        let at = Instant::now();
        let value = fetch.await?;
        if self.generation.load(Ordering::SeqCst) == generation {
            self.cache.insert(
                call,
                Kept {
                    at,
                    generation,
                    reply: into(value.clone()),
                },
            );
        }
        Ok(value)
    }
}

/// About how much memory `call` and its `reply` take: the items as JSON.
fn weigh(call: &Call, reply: &Reply) -> usize {
    let items = |items: &[Item]| -> usize {
        items
            .iter()
            .map(|i| serde_json::to_vec(i).map_or(1024, |j| j.len()))
            .sum()
    };
    let reply = match reply {
        Reply::Libraries(libraries) => libraries.len() * 256,
        Reply::Page(page) => items(&page.items),
        Reply::Item(item) => items(std::slice::from_ref(&item.item)) + items(&item.children) + 256,
        Reply::List(list) => items(list),
        Reply::Ids(ids) => ids.iter().map(String::len).sum(),
    };
    let call = match call {
        Call::Libraries | Call::RecentlyAdded(_) => 0,
        Call::Items(id, ..)
        | Call::Item(id)
        | Call::Episodes(id)
        | Call::ExternalIds(id)
        | Call::PersonItems(id) => id.len(),
    };
    std::mem::size_of::<Call>() + std::mem::size_of::<Kept>() + call + reply
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn ids(reply: &Reply) -> Option<&Vec<String>> {
        match reply {
            Reply::Ids(ids) => Some(ids),
            _ => None,
        }
    }

    /// `replies`' answer to one call, counting the fetches in `asked`.
    async fn ask(replies: &Replies, asked: &AtomicUsize) -> Result<Vec<String>, CatalogError> {
        replies
            .read(
                Call::ExternalIds("plex:movie:1".into()),
                ids,
                Reply::Ids,
                async {
                    asked.fetch_add(1, Ordering::SeqCst);
                    Ok(vec!["imdb://tt0078748".to_owned()])
                },
            )
            .await
    }

    #[tokio::test]
    async fn a_reply_is_kept_for_its_time_then_asked_for_again() {
        let mut replies = Replies::new(1 << 20);
        replies.kept_for = Duration::from_millis(200);
        let asked = AtomicUsize::new(0);
        for _ in 0..3 {
            assert_eq!(ask(&replies, &asked).await.unwrap(), ["imdb://tt0078748"]);
        }
        assert_eq!(asked.load(Ordering::SeqCst), 1);
        let stats = replies.stats();
        assert_eq!((stats.hits, stats.misses), (2, 1));
        assert!(stats.memory_bytes > 0);
        tokio::time::sleep(Duration::from_millis(250)).await;
        ask(&replies, &asked).await.unwrap();
        assert_eq!(asked.load(Ordering::SeqCst), 2);
        assert_eq!(replies.invalidated().expired, 1);
    }

    #[tokio::test]
    async fn a_lower_limit_evicts_at_once() {
        let replies = Replies::new(1 << 20);
        let asked = AtomicUsize::new(0);
        ask(&replies, &asked).await.unwrap();
        assert!(replies.memory_bytes() > 0);
        replies.set_limit(0);
        assert_eq!(replies.memory_bytes(), 0);
        assert_eq!(replies.limit(), 0);
    }

    #[tokio::test]
    async fn a_failure_is_not_kept() {
        let replies = Replies::new(1 << 20);
        let call = || Call::ExternalIds("plex:movie:1".into());
        let failed = replies
            .read(call(), ids, Reply::Ids, async {
                Err(CatalogError::NotFound("plex:movie:1".into()))
            })
            .await;
        assert!(failed.is_err());
        let asked = AtomicUsize::new(0);
        ask(&replies, &asked).await.unwrap();
        assert_eq!(asked.load(Ordering::SeqCst), 1);
    }

    /// A replaced catalog forgets every reply, and one fetched from the
    /// catalog it replaced while that happened is not kept.
    #[tokio::test]
    async fn a_replaced_catalog_forgets_and_keeps_nothing_of_the_old_one() {
        let replies = Replies::new(1 << 20);
        let asked = AtomicUsize::new(0);
        ask(&replies, &asked).await.unwrap();
        replies.forget();
        ask(&replies, &asked).await.unwrap();
        assert_eq!(asked.load(Ordering::SeqCst), 2);
        assert_eq!(replies.invalidated().replaced, 1);
        // Replaced while the old catalog answers.
        let old = replies
            .read(
                Call::ExternalIds("plex:movie:2".into()),
                ids,
                Reply::Ids,
                async {
                    replies.forget();
                    Ok(vec!["old".to_owned()])
                },
            )
            .await
            .unwrap();
        assert_eq!(old, ["old"]);
        let again = replies
            .read(
                Call::ExternalIds("plex:movie:2".into()),
                ids,
                Reply::Ids,
                async { Ok(vec!["new".to_owned()]) },
            )
            .await
            .unwrap();
        assert_eq!(again, ["new"]);
    }
}
