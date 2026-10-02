//! Watch state rules and Home (P1-14, ARCHITECTURE §36): what counts as
//! started and watched, how often progress is written, and what Home
//! shows. Storage is `crate::store::watch`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use spoolframe_catalog::{Catalog, CatalogError};
use spoolframe_media::Prober;
use spoolframe_protocol::HomeScreen;
use spoolframe_types::{Item, MediaId, MediaKind, PlaybackState};

use crate::principal::Principal;
use crate::store::watch::WatchUpdate;
use crate::store::{Store, StoreError, now};

/// The rules, with defaults; `watch { … }` in the config overrides them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WatchRules {
    /// Under this, an item is not in progress.
    pub started_after: Duration,
    /// Watched at this share of the duration…
    pub watched_fraction: f64,
    /// …or with less than this left, whichever comes first (credits).
    pub watched_remaining: Duration,
    /// At most one progress write per principal and item this often.
    pub throttle: Duration,
    /// Shows with activity this recent get a next-up episode.
    pub next_up_window: Duration,
    /// How long a next-up row is reused before it is walked again.
    pub next_up_cache: Duration,
}

impl Default for WatchRules {
    fn default() -> Self {
        Self {
            started_after: Duration::from_secs(60),
            watched_fraction: 0.9,
            watched_remaining: Duration::from_secs(5 * 60),
            throttle: Duration::from_secs(10),
            next_up_window: Duration::from_secs(30 * 24 * 3600),
            next_up_cache: Duration::from_secs(60),
        }
    }
}

impl WatchRules {
    /// Whether `position` into an item of `duration` counts as watched.
    pub fn is_watched(&self, position: Duration, duration: Option<Duration>) -> bool {
        let Some(duration) = duration.filter(|d| !d.is_zero()) else {
            return false;
        };
        position.as_secs_f64() >= duration.as_secs_f64() * self.watched_fraction
            || duration.saturating_sub(position) < self.watched_remaining
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    /// Only movies and episodes have watch state in Phase 1.
    #[error("{0} has no watch state")]
    NotTracked(MediaId),
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

const ROW: usize = 20;
const NEXT_UP_SHOWS: u32 = 20;

pub struct Watch {
    store: Store,
    catalog: Arc<dyn Catalog>,
    prober: Arc<Prober>,
    rules: WatchRules,
    /// When each (principal, item)'s progress was last written.
    written: Mutex<HashMap<(Principal, String), Instant>>,
    next_up: Mutex<HashMap<Principal, (Instant, Vec<Item>)>>,
}

impl std::fmt::Debug for Watch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watch")
            .field("rules", &self.rules)
            .finish_non_exhaustive()
    }
}

fn tracked(id: &MediaId) -> bool {
    matches!(id.kind(), MediaKind::Movie | MediaKind::Episode)
}

fn ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

impl Watch {
    pub fn new(
        store: Store,
        catalog: Arc<dyn Catalog>,
        prober: Arc<Prober>,
        rules: WatchRules,
    ) -> Self {
        Self {
            store,
            catalog,
            prober,
            rules,
            written: Mutex::default(),
            next_up: Mutex::default(),
        }
    }

    /// Record that `principal` is at `position` in `id`. Written at most
    /// once per throttle interval per item, whatever the client sends; a
    /// change of watched state is always written.
    pub async fn progress(
        &self,
        principal: Principal,
        id: &MediaId,
        position: Duration,
    ) -> Result<(), WatchError> {
        if !tracked(id) {
            return Err(WatchError::NotTracked(id.clone()));
        }
        let key = (principal, id.to_string());
        let recent = self
            .written
            .lock()
            .expect("never held across a panic")
            .get(&key)
            .is_some_and(|at| at.elapsed() < self.rules.throttle);
        if recent {
            // Within the throttle only a crossing into watched is written,
            // judged on the duration stored with the last write: no catalog
            // or probe work for the many updates in between.
            let stored = self
                .store
                .watch_states(principal, std::slice::from_ref(&key.1))
                .await?;
            let duration = stored
                .get(&key.1)
                .and_then(|r| r.duration_ms)
                .map(|d| Duration::from_millis(d.max(0) as u64));
            if !self.rules.is_watched(position, duration) {
                return Ok(());
            }
        }
        let found = self.catalog.item(id).await?;
        let duration = self.duration(id, &found.item).await;
        let watched = self.rules.is_watched(position, duration);
        self.save(principal, id, &found, Some(position), duration, watched)
            .await?;
        self.written
            .lock()
            .expect("never held across a panic")
            .insert(key, Instant::now());
        Ok(())
    }

    /// Mark `id` watched or not. Either way its progress starts over.
    pub async fn mark(
        &self,
        principal: Principal,
        id: &MediaId,
        watched: bool,
    ) -> Result<(), WatchError> {
        if !tracked(id) {
            return Err(WatchError::NotTracked(id.clone()));
        }
        let found = self.catalog.item(id).await?;
        let duration = self.duration(id, &found.item).await;
        self.save(principal, id, &found, None, duration, watched)
            .await
    }

    async fn save(
        &self,
        principal: Principal,
        id: &MediaId,
        found: &spoolframe_catalog::ItemWithChildren,
        position: Option<Duration>,
        duration: Option<Duration>,
        watched: bool,
    ) -> Result<(), WatchError> {
        let media_id = id.to_string();
        // Bring over any state this item had under an earlier key first.
        self.store
            .relink_watch(principal, &media_id, &found.external_ids)
            .await?;
        let show_id = match &found.item {
            Item::Episode(e) => Some(e.show_id.to_string()),
            _ => None,
        };
        self.store
            .save_watch(
                principal,
                WatchUpdate {
                    media_id: &media_id,
                    show_id: show_id.as_deref(),
                    external_ids: &found.external_ids,
                    position_ms: position.map(ms),
                    duration_ms: duration.map(ms),
                    watched,
                },
            )
            .await?;
        // What is next up may have changed.
        self.next_up
            .lock()
            .expect("never held across a panic")
            .remove(&principal);
        Ok(())
    }

    /// The duration from the server's own probe, else the catalog's; never
    /// the client's.
    async fn duration(&self, id: &MediaId, item: &Item) -> Option<Duration> {
        let probed = match self.catalog.media_path(id).await {
            Ok(path) => self
                .prober
                .describe(&path)
                .await
                .ok()
                .and_then(|d| d.duration),
            Err(_) => None,
        };
        probed.or(match item {
            Item::Movie(m) => m.duration,
            Item::Episode(e) => e.duration,
            _ => None,
        })
    }

    /// If `id` had state under an earlier key, bring it over (P1-14's
    /// re-linking, for an item a client is looking at).
    pub async fn relink(
        &self,
        principal: Principal,
        id: &MediaId,
        external_ids: &[String],
    ) -> Result<(), WatchError> {
        if tracked(id) {
            self.store
                .relink_watch(principal, &id.to_string(), external_ids)
                .await?;
        }
        Ok(())
    }

    /// Fill in `principal`'s state on every movie and episode in `items`,
    /// with one lookup for all of them.
    pub async fn fill(
        &self,
        principal: Principal,
        items: &mut [&mut Item],
    ) -> Result<(), WatchError> {
        let ids: Vec<String> = items
            .iter()
            .filter(|i| tracked(i.id()))
            .map(|i| i.id().to_string())
            .collect();
        let states = self.store.watch_states(principal, &ids).await?;
        for item in items.iter_mut() {
            let Some(row) = states.get(&item.id().to_string()) else {
                continue;
            };
            let position = row
                .position_ms
                .map(|p| Duration::from_millis(p.max(0) as u64))
                .filter(|p| *p >= self.rules.started_after && !row.watched);
            let state = PlaybackState {
                position,
                watched: row.watched,
            };
            match item {
                Item::Movie(m) => m.playback = state,
                Item::Episode(e) => e.playback = state,
                _ => {}
            }
        }
        Ok(())
    }

    /// `principal`'s home screen.
    pub async fn home(&self, principal: Principal) -> Result<HomeScreen, WatchError> {
        let mut continue_watching = Vec::new();
        let min = ms(self.rules.started_after);
        for row in self.store.in_progress(principal, min, ROW as u32).await? {
            let Ok(id) = row.media_id.parse::<MediaId>() else {
                continue;
            };
            // An item gone from the catalog drops out of the row.
            match self.catalog.item(&id).await {
                Ok(found) => continue_watching.push(found.item),
                Err(CatalogError::NotFound(_)) => {}
                Err(e) => return Err(e.into()),
            }
        }
        let next_up = self.next_up(principal).await?;
        let recently_added = self.catalog.recently_added(ROW as u32).await?;
        let mut home = HomeScreen {
            continue_watching,
            next_up,
            recently_added,
        };
        let mut all: Vec<&mut Item> = home
            .continue_watching
            .iter_mut()
            .chain(home.next_up.iter_mut())
            .chain(home.recently_added.iter_mut())
            .collect();
        self.fill(principal, &mut all).await?;
        Ok(home)
    }

    /// For each show with an episode watched or in progress lately, the
    /// first unwatched episode after the latest watched one. It walks the
    /// show through the catalog, so it covers recent shows only, and is
    /// cached briefly.
    async fn next_up(&self, principal: Principal) -> Result<Vec<Item>, WatchError> {
        if let Some((at, items)) = self
            .next_up
            .lock()
            .expect("never held across a panic")
            .get(&principal)
            && at.elapsed() < self.rules.next_up_cache
        {
            return Ok(items.clone());
        }
        let since = now() - self.rules.next_up_window.as_secs() as i64;
        let mut next = Vec::new();
        for show in self
            .store
            .active_shows(principal, since, NEXT_UP_SHOWS)
            .await?
        {
            let Ok(show_id) = show.parse::<MediaId>() else {
                continue;
            };
            if let Some(episode) = self.next_in_show(principal, &show_id).await? {
                next.push(episode);
            }
        }
        self.next_up
            .lock()
            .expect("never held across a panic")
            .insert(principal, (Instant::now(), next.clone()));
        Ok(next)
    }

    async fn next_in_show(
        &self,
        principal: Principal,
        show: &MediaId,
    ) -> Result<Option<Item>, WatchError> {
        let watched = self
            .store
            .watched_in_show(principal, &show.to_string())
            .await?;
        if watched.is_empty() {
            // Only in progress: that episode is in Continue watching.
            return Ok(None);
        }
        let seasons = match self.catalog.item(show).await {
            Ok(found) => found.children,
            Err(CatalogError::NotFound(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        // Specials (season 0) are not part of the run.
        let mut seasons: Vec<_> = seasons
            .into_iter()
            .filter_map(|s| match s {
                Item::Season(s) if s.season > 0 => Some(s),
                _ => None,
            })
            .collect();
        seasons.sort_by_key(|s| s.season);
        let mut episodes = Vec::new();
        for season in seasons {
            let mut in_season: Vec<_> = self
                .catalog
                .item(&season.id)
                .await?
                .children
                .into_iter()
                .filter(|e| matches!(e, Item::Episode(_)))
                .collect();
            in_season.sort_by_key(|e| match e {
                Item::Episode(e) => e.episode,
                _ => u32::MAX,
            });
            episodes.extend(in_season);
        }
        let Some(latest) = episodes
            .iter()
            .rposition(|e| watched.contains(&e.id().to_string()))
        else {
            return Ok(None);
        };
        Ok(episodes
            .into_iter()
            .skip(latest + 1)
            .find(|e| !watched.contains(&e.id().to_string())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watched_at_ninety_percent_or_five_minutes_left() {
        let rules = WatchRules::default();
        let movie = Some(Duration::from_secs(7200));
        assert!(!rules.is_watched(Duration::from_secs(6000), movie));
        // 90% of 2 h is 108 min.
        assert!(rules.is_watched(Duration::from_secs(6480), movie));
        let short = Some(Duration::from_secs(1200));
        // 20 min: 5 minutes left comes before 90%.
        assert!(rules.is_watched(Duration::from_secs(901), short));
        assert!(!rules.is_watched(Duration::from_secs(800), short));
    }

    #[test]
    fn without_a_duration_nothing_is_watched_by_progress() {
        assert!(!WatchRules::default().is_watched(Duration::from_secs(99_999), None));
    }
}
