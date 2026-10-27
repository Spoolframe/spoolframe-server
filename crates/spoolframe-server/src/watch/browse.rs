//! A library's items sorted and filtered,
//! the session user's watch state included.
//!
//! The catalog sorts and filters by what its items have. The `watched`
//! filter and the `last_played` sort are by the user's watch state, which
//! is here: a catalog that keeps its items in this server's database (the
//! paired servers' cache) joins it there ([`WatchParts::InCatalog`]); for
//! one that does not (Plex), the library is walked in the order asked
//! without them, that order filtered and reordered by the user's state
//! here, and the result kept a while ([`Walks`]) for the pages that
//! follow.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use spoolframe_catalog::{CatalogError, ItemPage, ItemQuery, Listed, Page, Viewer, WatchParts};
use spoolframe_types::{
    ItemFilter, ItemFilterValues, ItemSort, Library, MediaId, MediaKind, SortKey,
};

use super::{Watch, WatchError};
use crate::principal::Principal;
use crate::store::watch::Seen;

/// How long a walk is kept for the pages after the first.
const WALK_FOR: Duration = Duration::from_secs(60);

/// The most ids all kept walks hold together: about 40 MB. The oldest go
/// first; a walk larger than this alone is used once and not kept.
const WALKS_HOLD: usize = 1_000_000;

/// One walk's key: whose, which library, and what was asked.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct WalkKey {
    principal: Principal,
    library: String,
    sort: ItemSort,
    filter: ItemFilter,
}

/// One walk kept: what was asked, when, and the ids in order.
type Kept = (WalkKey, Instant, Arc<Vec<MediaId>>);

/// Walks kept, oldest first, at most `hold` ids together.
#[derive(Debug)]
pub(super) struct Walks {
    kept: Mutex<VecDeque<Kept>>,
    hold: usize,
}

impl Default for Walks {
    fn default() -> Self {
        Self {
            kept: Mutex::default(),
            hold: WALKS_HOLD,
        }
    }
}

impl Walks {
    fn get(&self, key: &WalkKey) -> Option<Arc<Vec<MediaId>>> {
        let mut kept = self.kept.lock();
        kept.retain(|(_, at, _)| at.elapsed() < WALK_FOR);
        kept.iter()
            .find(|(k, _, _)| k == key)
            .map(|(_, _, ids)| ids.clone())
    }

    fn keep(&self, key: WalkKey, ids: Arc<Vec<MediaId>>) {
        if ids.len() > self.hold {
            return;
        }
        let mut kept = self.kept.lock();
        kept.retain(|(k, _, _)| *k != key);
        let mut held: usize = kept.iter().map(|(_, _, ids)| ids.len()).sum();
        while held + ids.len() > self.hold {
            let Some((_, _, gone)) = kept.pop_front() else {
                break;
            };
            held -= gone.len();
        }
        kept.push_back((key, Instant::now(), ids));
    }

    /// Forget `principal`'s walks: their watch state changed.
    pub(super) fn forget(&self, principal: Principal) {
        self.kept
            .lock()
            .retain(|(k, _, _)| k.principal != principal);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.kept.lock().len()
    }
}

/// `walked`, the library in the order asked without the watch state,
/// kept by `filter`'s watch states and ordered by `sort` when it is by
/// last played: those played, by when, then the rest in title order.
fn by_watch_state(
    walked: Vec<Listed>,
    seen: &HashMap<String, Seen>,
    sort: ItemSort,
    filter: &ItemFilter,
) -> Vec<MediaId> {
    let none = Seen::default();
    let mut kept: Vec<(Listed, Option<i64>)> = walked
        .into_iter()
        .filter_map(|listed| {
            let seen = seen.get(&listed.id.to_string()).unwrap_or(&none);
            let show = listed.id.kind() == MediaKind::Show;
            let state = seen.state(listed.leaves, show);
            let keep = filter.watched.is_empty() || filter.watched.contains(&state);
            keep.then_some((listed, seen.last))
        })
        .collect();
    if sort.key == SortKey::LastPlayed {
        // Stable: equal times, and the never played, keep the walk's
        // title order.
        kept.sort_by(|(_, a), (_, b)| match (a, b) {
            (Some(a), Some(b)) if sort.descending => b.cmp(a),
            (Some(a), Some(b)) => a.cmp(b),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        });
    }
    kept.into_iter().map(|(listed, _)| listed.id).collect()
}

impl Watch {
    /// What `library` can be sorted and filtered by, with the values its
    /// items have.
    pub async fn filter_values(&self, library: &Library) -> Result<ItemFilterValues, WatchError> {
        Ok(self.catalog.filter_values(library).await?)
    }

    /// A page of `library`'s items as `sort` and `filter` ask, by
    /// `principal`'s watch state where they ask for it; without either,
    /// the library's plain listing. A key or part the library does not
    /// have is [`CatalogError::CannotBrowse`].
    pub async fn browse(
        &self,
        principal: Principal,
        library: &Library,
        sort: Option<ItemSort>,
        filter: Option<ItemFilter>,
        page: Page,
    ) -> Result<ItemPage, WatchError> {
        if sort.is_none() && filter.is_none() {
            return Ok(self.catalog.items(&library.id, page).await?);
        }
        let cannot = |e: spoolframe_types::browse::BrowseError| {
            WatchError::Catalog(CatalogError::CannotBrowse(e.to_string()))
        };
        if let Some(sort) = &sort {
            sort.check().map_err(cannot)?;
        }
        if let Some(filter) = &filter {
            filter.check().map_err(cannot)?;
        }
        let browsing = self.catalog.browsing(library).await?;
        if let Some(why) = browsing.refuses(sort.as_ref(), filter.as_ref()) {
            return Err(CatalogError::CannotBrowse(why).into());
        }
        let mut query = ItemQuery {
            sort: sort.unwrap_or(ItemSort::DEFAULT),
            filter: filter.unwrap_or_default(),
            viewer: None,
        };
        if !query.needs_viewer() {
            return Ok(self.catalog.query_items(&library.id, &query, page).await?);
        }
        if browsing.watch == WatchParts::InCatalog {
            query.viewer = Some(Viewer {
                principal: principal.to_string(),
                started_after_ms: u64::try_from(self.rules().started_after.as_millis())
                    .unwrap_or(u64::MAX),
            });
            return Ok(self.catalog.query_items(&library.id, &query, page).await?);
        }
        let ids = self.walked(principal, library, &query).await?;
        let total = u32::try_from(ids.len()).unwrap_or(u32::MAX);
        let start = usize::try_from(page.offset).unwrap_or(usize::MAX);
        let end = start
            .saturating_add(usize::try_from(page.limit).unwrap_or(usize::MAX))
            .min(ids.len());
        let items = match ids.get(start..end) {
            Some(slice) if !slice.is_empty() => self.catalog.items_by_id(slice).await?,
            _ => Vec::new(),
        };
        Ok(ItemPage { items, total })
    }

    /// `library` in `query`'s order and kept by it, the watch state
    /// included: a walk kept from a page before, or one made now.
    async fn walked(
        &self,
        principal: Principal,
        library: &Library,
        query: &ItemQuery,
    ) -> Result<Arc<Vec<MediaId>>, WatchError> {
        let key = WalkKey {
            principal,
            library: library.id.to_string(),
            sort: query.sort,
            filter: query.filter.clone(),
        };
        if let Some(ids) = self.walks.get(&key) {
            return Ok(ids);
        }
        let started = Instant::now();
        let (walked, rows) = futures_util::future::try_join(
            async {
                self.catalog
                    .walk_items(&library.id, &query.without_watch_state())
                    .await
                    .map_err(WatchError::from)
            },
            async {
                self.store
                    .all_watch(principal)
                    .await
                    .map_err(WatchError::from)
            },
        )
        .await?;
        let read = walked.len();
        let started_ms = i64::try_from(self.rules().started_after.as_millis()).unwrap_or(i64::MAX);
        let seen = Seen::of_rows(&rows, started_ms);
        let ids = Arc::new(by_watch_state(walked, &seen, query.sort, &query.filter));
        tracing::debug!(
            library = %library.id,
            read,
            kept = ids.len(),
            took = ?started.elapsed(),
            "library walked for the watch state"
        );
        self.walks.keep(key, ids.clone());
        Ok(ids)
    }
}

#[cfg(test)]
mod tests;
