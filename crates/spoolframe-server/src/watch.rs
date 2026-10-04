//! Watch state rules and Home (P1-14, ARCHITECTURE §36): what counts as
//! started and watched, how often progress is written, and what Home
//! shows. Storage is `crate::store::watch`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use spoolframe_catalog::{Catalog, CatalogError};
use spoolframe_media::Prober;
use spoolframe_protocol::HomeScreen;
use spoolframe_types::{Item, Marker, MarkerKind, MediaId, MediaKind, PlaybackState};

use crate::principal::Principal;
use crate::store::watch::WatchUpdate;
use crate::store::{Store, StoreError, now};

/// The rules, with defaults; `watch { … }` in the config overrides them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WatchRules {
    /// Under this, an item is not in progress.
    pub started_after: Duration,
    /// Watched at this share of the duration, for an item with no credits
    /// that run to its end.
    pub watched_fraction: f64,
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
            watched_fraction: 0.95,
            throttle: Duration::from_secs(10),
            next_up_window: Duration::from_secs(30 * 24 * 3600),
            next_up_cache: Duration::from_secs(60),
        }
    }
}

impl WatchRules {
    /// Whether `position` into an item of `duration` counts as watched.
    /// `credits` is where the credits that run to its end start, when the
    /// catalog knows (P2-10): then that is the point, exactly, and the
    /// share of the duration does not apply.
    pub fn is_watched(
        &self,
        position: Duration,
        duration: Option<Duration>,
        credits: Option<Duration>,
    ) -> bool {
        if let Some(credits) = credits {
            return position >= credits;
        }
        let Some(duration) = duration.filter(|d| !d.is_zero()) else {
            return false;
        };
        position.as_secs_f64() >= duration.as_secs_f64() * self.watched_fraction
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

/// How long "that key still holds this item" is remembered. A second copy
/// with no state of its own asks at each look; a real re-key waits at most
/// this long to be seen.
const SAME_ITEM_CACHE: Duration = Duration::from_secs(60);

const ROW: usize = 20;
const NEXT_UP_SHOWS: u32 = 20;

pub struct Watch {
    store: Store,
    catalog: Arc<dyn Catalog>,
    prober: Arc<Prober>,
    /// A live setting (P3-10): read at each use.
    rules: std::sync::RwLock<WatchRules>,
    /// Each (principal, item)'s last progress write.
    written: Mutex<HashMap<(Principal, String), Written>>,
    next_up: Mutex<HashMap<Principal, (Instant, Vec<Item>)>>,
    /// (Another key, an item): when the catalog last said that the key
    /// holds a copy of the item, so its state is not the item's to take.
    copies: Mutex<HashMap<(String, String), Instant>>,
    /// Whose state was saved, for whoever mirrors it elsewhere (PX-04):
    /// each one, since every linked user's state syncs (P3-12).
    changed: tokio::sync::broadcast::Sender<Principal>,
}

/// A progress write, for judging the reports the throttle skips.
#[derive(Debug, Clone, Copy)]
struct Written {
    at: Instant,
    /// The item's [`credits_to_end`], as the catalog had them then.
    credits: Option<Duration>,
}

impl std::fmt::Debug for Watch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watch")
            .field("rules", &self.rules())
            .finish_non_exhaustive()
    }
}

/// Where the credits that run to the end of the item start, if `markers`
/// has such credits.
fn credits_to_end(markers: &[Marker]) -> Option<Duration> {
    markers
        .iter()
        .filter(|m| m.kind == MarkerKind::Credits && m.to_end)
        .map(|m| Duration::from_millis(m.start_ms))
        .min()
}

/// Whether `e` is a Plex server that cannot be reached or does not answer
/// properly: one server's trouble, which Home goes on without.
fn is_down(e: &CatalogError) -> bool {
    matches!(
        e,
        CatalogError::Unreachable { .. } | CatalogError::Backend { .. }
    )
}

/// How many saves a listener may fall behind by; one that does is told
/// so, and treats it as a change.
const CHANGES: usize = 256;

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
            rules: std::sync::RwLock::new(rules),
            written: Mutex::default(),
            next_up: Mutex::default(),
            copies: Mutex::default(),
            changed: tokio::sync::broadcast::Sender::new(CHANGES),
        }
    }

    /// The rules now.
    pub fn rules(&self) -> WatchRules {
        *self.rules.read().expect("never held across a panic")
    }

    /// Judge by `rules` from now on (`watch { … }` changed at run time).
    pub fn set_rules(&self, rules: WatchRules) {
        *self.rules.write().expect("never held across a panic") = rules;
    }

    /// Fires after each state saved through [`Watch`], with its principal.
    pub fn changes(&self) -> tokio::sync::broadcast::Receiver<Principal> {
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
            .copied()
            .filter(|written| written.at.elapsed() < self.rules().throttle);
        if let Some(written) = recent {
            // Within the throttle only a crossing into watched is written,
            // judged on what the last write knew (the row's duration and
            // whether it is watched already, and the item's credits): no
            // catalog or probe work for the many updates in between.
            let stored = self
                .store
                .watch_states(principal, std::slice::from_ref(&key.1))
                .await?;
            let row = stored.get(&key.1);
            let duration = row
                .and_then(|r| r.duration_ms)
                .map(|d| Duration::from_millis(d.max(0) as u64));
            let crossing = self.rules().is_watched(position, duration, written.credits)
                && !row.is_some_and(|r| r.watched);
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
        let credits = credits_to_end(&found.markers);
        let watched = self.rules().is_watched(position, duration, credits);
        self.save(principal, id, &found, Some(position), duration, watched)
            .await?;
        self.written
            .lock()
            .expect("never held across a panic")
            .insert(
                key,
                Written {
                    at: Instant::now(),
                    credits,
                },
            );
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
        // Nobody listening is fine: nothing syncs.
        let _ = self.changed.send(principal);
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
    /// one that no longer holds the item: a key that still does is a copy
    /// of it, such as the same film in a second library, and keeps its
    /// state. A catalog that cannot say leaves the state where it is, and
    /// the next call asks again.
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
            && self.left(&old, &media_id, external_ids).await
        {
            self.store.move_watch(principal, &old, &media_id).await?;
        }
        // A show's hide follows it too (P1-22).
        if let Some(old) = self
            .store
            .hidden_under_another_key(principal, &media_id, external_ids)
            .await?
            && self.left(&old, &media_id, external_ids).await
        {
            self.store.move_hidden(principal, &old, &media_id).await?;
        }
        Ok(())
    }

    /// Whether the item `media_id`, with `external_ids`, has left `key`:
    /// the catalog has nothing under it now, or another item (after a
    /// rebuild Plex hands the same keys out again), or it is not a key of
    /// this catalog. It has not if what is under `key` shares an external
    /// id with the item. If the catalog cannot say, it has not, for now.
    async fn left(&self, key: &str, media_id: &str, external_ids: &[String]) -> bool {
        let Ok(id) = key.parse::<MediaId>() else {
            return false;
        };
        let pair = (key.to_owned(), media_id.to_owned());
        let fresh = |at: &Instant| at.elapsed() < SAME_ITEM_CACHE;
        if self
            .copies
            .lock()
            .expect("never held across a panic")
            .get(&pair)
            .is_some_and(fresh)
        {
            return false;
        }
        match self.catalog.external_ids(&id).await {
            Ok(there) if there.iter().any(|id| external_ids.contains(id)) => {
                let mut copies = self.copies.lock().expect("never held across a panic");
                copies.retain(|_, at| fresh(at));
                copies.insert(pair, Instant::now());
                false
            }
            Ok(_) | Err(CatalogError::NotFound(_) | CatalogError::ForeignId(_)) => true,
            // In a library removed for now (P3-11): it has not left, and
            // adding the library back finds its state where it was.
            Err(CatalogError::NotServed(_)) => false,
            Err(e) => {
                tracing::warn!(
                    from = key,
                    to = media_id,
                    error = %e,
                    "cannot tell whether watch state is to be re-linked; left where it is"
                );
                false
            }
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
                .filter(|p| *p >= self.rules().started_after && !row.watched);
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
        let min = ms(self.rules().started_after);
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
            // An item gone from the catalog, or in a library not served
            // now, drops out of the row; so does one whose Plex server is
            // down (P3-11), until it is back, while the others still show.
            match self.catalog.item(&id).await {
                Ok(found) => continue_watching.push(found.item),
                Err(CatalogError::NotFound(_) | CatalogError::NotServed(_)) => {}
                Err(e) if is_down(&e) => {
                    tracing::warn!(item = %id, error = %e, "Continue watching: left out");
                }
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
            && at.elapsed() < self.rules().next_up_cache
        {
            return Ok(items.clone());
        }
        let since = now() - self.rules().next_up_window.as_secs() as i64;
        let mut next = Vec::new();
        for show in self
            .store
            .active_shows(principal, since, NEXT_UP_SHOWS)
            .await?
        {
            let Ok(show_id) = show.parse::<MediaId>() else {
                continue;
            };
            match self.next_in_show(principal, &show_id).await {
                Ok(Some(episode)) => next.push(episode),
                Ok(None) => {}
                Err(WatchError::Catalog(e)) if is_down(&e) => {
                    tracing::warn!(show = %show_id, error = %e, "Next up: left out");
                }
                Err(e) => return Err(e),
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
            Err(CatalogError::NotFound(_) | CatalogError::NotServed(_)) => return Ok(None),
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
    fn without_credits_an_item_is_watched_at_ninety_five_percent() {
        let rules = WatchRules::default();
        // 95% of 2 h is 114 min.
        let movie = Some(Duration::from_secs(7200));
        assert!(!rules.is_watched(Duration::from_secs(6839), movie, None));
        assert!(rules.is_watched(Duration::from_secs(6840), movie, None));
    }

    #[test]
    fn time_left_does_not_make_an_item_watched() {
        let rules = WatchRules::default();
        // 44 min, with 4 minutes left: 91%.
        let episode = Some(Duration::from_secs(2640));
        assert!(!rules.is_watched(Duration::from_secs(2400), episode, None));
        // A 4-minute clip always has less than 5 minutes left.
        let clip = Some(Duration::from_secs(240));
        assert!(!rules.is_watched(Duration::from_secs(10), clip, None));
        assert!(!rules.is_watched(Duration::from_secs(227), clip, None));
        assert!(rules.is_watched(Duration::from_secs(228), clip, None));
    }

    #[test]
    fn without_a_duration_nothing_is_watched_by_progress() {
        assert!(!WatchRules::default().is_watched(Duration::from_secs(99_999), None, None));
    }

    #[test]
    fn with_credits_that_run_to_the_end_watched_is_at_their_start_exactly() {
        let rules = WatchRules::default();
        let episode = Some(Duration::from_secs(2640));
        // Short credits, from 98%: 95% (41:48) is not the point.
        let credits = Some(Duration::from_secs(2590));
        assert!(!rules.is_watched(Duration::from_secs(2589), episode, credits));
        assert!(rules.is_watched(Duration::from_secs(2590), episode, credits));
        // Long credits: before 95%.
        let long = Some(Duration::from_secs(1800));
        assert!(rules.is_watched(Duration::from_secs(1800), episode, long));
    }

    #[test]
    fn credits_that_run_to_the_end_need_no_duration() {
        let rules = WatchRules::default();
        let credits = Some(Duration::from_secs(2400));
        assert!(rules.is_watched(Duration::from_secs(2400), None, credits));
        assert!(!rules.is_watched(Duration::from_secs(2399), None, credits));
    }

    fn marker(kind: MarkerKind, start_ms: u64, to_end: bool) -> Marker {
        Marker {
            kind,
            start_ms,
            end_ms: start_ms + 60_000,
            to_end,
        }
    }

    #[test]
    fn only_credits_that_run_to_the_end_give_the_watched_point() {
        // An intro, and credits with a scene after them, do not.
        assert_eq!(
            credits_to_end(&[
                marker(MarkerKind::Intro, 30_000, false),
                marker(MarkerKind::Credits, 2_000_000, false),
            ]),
            None
        );
        assert_eq!(
            credits_to_end(&[
                marker(MarkerKind::Credits, 2_000_000, false),
                marker(MarkerKind::Credits, 2_400_000, true),
            ]),
            Some(Duration::from_secs(2400))
        );
    }
}
