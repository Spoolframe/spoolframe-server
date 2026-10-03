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
    /// …or with less than this left, whichever comes first (credits); only
    /// for items at least four times this long.
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
        let past_fraction =
            position.as_secs_f64() >= duration.as_secs_f64() * self.watched_fraction;
        // The time-left rule is for credits. On a short item it would be
        // most of the item, so there only the fraction counts.
        let in_credits = duration >= self.watched_remaining * REMAINING_RULE_MIN_LENGTHS
            && duration.saturating_sub(position) < self.watched_remaining;
        past_fraction || in_credits
    }
}

/// The time-left rule applies to items at least this many times
/// `watched_remaining` long: it never calls an item watched before three
/// quarters of it.
const REMAINING_RULE_MIN_LENGTHS: u32 = 4;

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
    /// Who saved a state last, for whoever mirrors it elsewhere (PX-04).
    changed: tokio::sync::watch::Sender<Option<Principal>>,
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
            changed: tokio::sync::watch::Sender::new(None),
        }
    }

    /// Fires after each state saved through [`Watch`], with its principal.
    pub fn changes(&self) -> tokio::sync::watch::Receiver<Option<Principal>> {
        self.changed.subscribe()
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
            // judged on the row of the last write (its duration, and whether
            // it is watched already): no catalog or probe work for the many
            // updates in between.
            let stored = self
                .store
                .watch_states(principal, std::slice::from_ref(&key.1))
                .await?;
            let row = stored.get(&key.1);
            let duration = row
                .and_then(|r| r.duration_ms)
                .map(|d| Duration::from_millis(d.max(0) as u64));
            let crossing =
                self.rules.is_watched(position, duration) && !row.is_some_and(|r| r.watched);
            if !crossing {
                // Not written, but played: it is back in Continue watching
                // (P1-22).
                let show = row.and_then(|r| r.show_id.as_deref());
                self.store.clear_hidden(principal, &key.1, show).await?;
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

    /// Remove `id` from `principal`'s Continue watching (P1-22): a movie
    /// itself, an episode its show (from Next up too). Its watch state does
    /// not change; the next one written clears this.
    pub async fn hide(&self, principal: Principal, id: &MediaId) -> Result<(), WatchError> {
        let (target, external_ids) = self.hide_target(id).await?;
        self.store
            .hide_continue(principal, &target.to_string(), &external_ids)
            .await?;
        Ok(())
    }

    /// Undo [`Watch::hide`].
    pub async fn unhide(&self, principal: Principal, id: &MediaId) -> Result<(), WatchError> {
        let (target, _) = self.hide_target(id).await?;
        self.store
            .unhide_continue(principal, &target.to_string())
            .await?;
        Ok(())
    }

    /// What hiding `id` hides, with its external ids: the movie, or the
    /// episode's show.
    async fn hide_target(&self, id: &MediaId) -> Result<(MediaId, Vec<String>), WatchError> {
        if !tracked(id) {
            return Err(WatchError::NotTracked(id.clone()));
        }
        let found = self.catalog.item(id).await?;
        match &found.item {
            Item::Episode(e) => {
                let show = self.catalog.item(&e.show_id).await?;
                Ok((e.show_id.clone(), show.external_ids))
            }
            _ => Ok((id.clone(), found.external_ids)),
        }
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
        self.relink(principal, id, &found.external_ids).await?;
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
                    unhide: true,
                },
            )
            .await?;
        // What is next up may have changed.
        self.next_up
            .lock()
            .expect("never held across a panic")
            .remove(&principal);
        self.changed.send_replace(Some(principal));
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
    /// re-linking, for an item a client is looking at). An earlier key is
    /// one the catalog no longer has: a key still there is another item
    /// with the same external ids, such as a second copy of the film, and
    /// keeps its state.
    pub async fn relink(
        &self,
        principal: Principal,
        id: &MediaId,
        external_ids: &[String],
    ) -> Result<(), WatchError> {
        let media_id = id.to_string();
        if tracked(id)
            && let Some(old) = self
                .store
                .watch_under_another_key(principal, &media_id, external_ids)
                .await?
            && self.gone(&old).await?
        {
            self.store.move_watch(principal, &old, &media_id).await?;
        }
        // A show's hide follows it too (P1-22).
        if let Some(old) = self
            .store
            .hidden_under_another_key(principal, &media_id, external_ids)
            .await?
            && self.gone(&old).await?
        {
            self.store.move_hidden(principal, &old, &media_id).await?;
        }
        Ok(())
    }

    /// Whether the item stored under `key` has left the catalog.
    async fn gone(&self, key: &str) -> Result<bool, WatchError> {
        let Ok(id) = key.parse::<MediaId>() else {
            return Ok(false);
        };
        match self.catalog.item(&id).await {
            Ok(_) => Ok(false),
            Err(CatalogError::NotFound(_)) => Ok(true),
            Err(e) => Err(e.into()),
        }
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
        // Removed from Continue watching (P1-22): movies, and shows.
        let hidden = self.store.hidden(principal).await?;
        let mut continue_watching = Vec::new();
        let min = ms(self.rules.started_after);
        // As many more as are hidden, so the row stays full.
        let wanted = (ROW + hidden.len()) as u32;
        for row in self.store.in_progress(principal, min, wanted).await? {
            if continue_watching.len() == ROW {
                break;
            }
            if hidden.contains(&row.media_id)
                || row.show_id.as_ref().is_some_and(|s| hidden.contains(s))
            {
                continue;
            }
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
        let mut next_up = self.next_up(principal).await?;
        next_up.retain(|item| match item {
            Item::Episode(e) => !hidden.contains(&e.show_id.to_string()),
            _ => true,
        });
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
    /// episode after the one watched most recently. It walks the
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
        // The most recently watched, not the furthest: a rewatch (or a
        // show picked up again from the middle) goes on from where the
        // person is, as Plex's On Deck does. A season marked watched at
        // once ties; the later episode wins.
        let Some((_, latest)) = episodes
            .iter()
            .enumerate()
            .filter_map(|(i, e)| watched.get(&e.id().to_string()).map(|at| (*at, i)))
            .max()
        else {
            return Ok(None);
        };
        Ok(episodes.into_iter().nth(latest + 1))
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
    fn a_short_item_is_watched_only_at_ninety_percent() {
        let rules = WatchRules::default();
        // A 4-minute clip always has less than 5 minutes left.
        let clip = Some(Duration::from_secs(240));
        assert!(!rules.is_watched(Duration::from_secs(10), clip));
        assert!(!rules.is_watched(Duration::from_secs(215), clip));
        assert!(rules.is_watched(Duration::from_secs(216), clip));
        // 7 min: 2:00 is 5 minutes from the end, and 29% in.
        let episode = Some(Duration::from_secs(420));
        assert!(!rules.is_watched(Duration::from_secs(121), episode));
        // Just under 20 min: the 5 minutes would be more than a quarter.
        let under = Some(Duration::from_secs(1199));
        assert!(!rules.is_watched(Duration::from_secs(1000), under));
        assert!(rules.is_watched(Duration::from_secs(1080), under));
    }

    #[test]
    fn without_a_duration_nothing_is_watched_by_progress() {
        assert!(!WatchRules::default().is_watched(Duration::from_secs(99_999), None));
    }
}
