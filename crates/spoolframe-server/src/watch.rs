//! Watch state rules and Home (P1-14, ARCHITECTURE §36): what counts as
//! started and watched, how often progress is written, and what Home
//! shows. Storage is `crate::store::watch`.

use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{StreamExt, TryStreamExt, stream};

use spoolframe_catalog::{Catalog, CatalogError};
use spoolframe_media::Prober;
use spoolframe_protocol::HomeScreen;
use spoolframe_types::{Item, MediaId, MediaKind, PlaybackState};

use crate::principal::Principal;
use crate::store::plays::PlayedKind;
use crate::store::watch::WatchUpdate;
use crate::store::{Store, StoreError};
pub use spoolframe_policy::watch::WatchRules;
use spoolframe_policy::watch::credits_to_end;

mod plays;
pub use plays::Plays;

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
/// The most catalog (Plex) lookups one Home load or one show-wide mark
/// has in flight at once: they go out a few at a time, and come back in
/// the order asked, so rows, errors and saves keep their order.
const CATALOG_FETCHES: usize = 8;

pub struct Watch {
    store: Store,
    catalog: Arc<dyn Catalog>,
    prober: Arc<Prober>,
    /// A live setting (P3-10): read at each use.
    rules: parking_lot::RwLock<WatchRules>,
    /// Each (principal, item)'s last progress write, while it is still
    /// within the throttle.
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

/// Whether `id` has a watch state: movies and episodes. A track has its
/// plays alone ([`Plays`]).
pub fn tracked(id: &MediaId) -> bool {
    matches!(id.kind(), MediaKind::Movie | MediaKind::Episode)
}

/// The length an item is judged by: `probed`, the server's own probe of
/// its file, else the catalog's; never a client's. The one length its
/// watch state, its plays and the Playback page all use, tracks included.
pub fn length(probed: Option<Duration>, item: &Item) -> Option<Duration> {
    probed.or(match item {
        Item::Movie(m) => m.duration,
        Item::Episode(e) => e.duration,
        Item::Track(t) => t.duration,
        _ => None,
    })
}

/// The watch state and plays `item` carries: a film's, an episode's or a
/// track's; nothing else has them.
fn playback_mut(item: &mut Item) -> Option<&mut PlaybackState> {
    match item {
        Item::Movie(m) => Some(&mut m.playback),
        Item::Episode(e) => Some(&mut e.playback),
        Item::Track(t) => Some(&mut t.playback),
        _ => None,
    }
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
            rules: parking_lot::RwLock::new(rules),
            written: Mutex::default(),
            next_up: Mutex::default(),
            copies: Mutex::default(),
            changed: tokio::sync::broadcast::Sender::new(CHANGES),
        }
    }

    /// Now, by its store's clock: what the throttle and the caches are
    /// timed by (PX-09).
    fn now(&self) -> Instant {
        self.store.clock().now()
    }

    /// How long ago `at` was, by the same clock.
    fn since(&self, at: Instant) -> Duration {
        self.now().saturating_duration_since(at)
    }

    /// The rules now.
    pub fn rules(&self) -> WatchRules {
        *self.rules.read()
    }

    /// Judge by `rules` from now on (`watch { … }` changed at run time).
    pub fn set_rules(&self, rules: WatchRules) {
        *self.rules.write() = rules;
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
            .get(&key)
            .copied()
            .filter(|written| self.since(written.at) < self.rules().throttle);
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
                self.store.clear_hidden(principal, &key.1).await?;
                return Ok(());
            }
        }
        let found = self.catalog.item(id).await?;
        let duration = self.duration(id, &found.item).await;
        let credits = credits_to_end(&found.markers);
        let watched = self.rules().is_watched(position, duration, credits);
        self.save(principal, id, &found, Some(position), duration, watched)
            .await?;
        // Writes older than the throttle judge nothing more: let them go,
        // so the map holds what is being played now, not every item ever
        // played since the server started.
        let throttle = self.rules().throttle;
        let mut written = self.written.lock();
        written.retain(|_, w| self.since(w.at) < throttle);
        written.insert(
            key,
            Written {
                at: self.now(),
                credits,
            },
        );
        Ok(())
    }

    /// Remove the card `id`, a movie or an episode, from `principal`'s
    /// Continue watching or Next up (P1-22). Only that card: the show's
    /// other episodes stay. Its watch state does not change; the next one
    /// written for it clears this.
    pub async fn hide(&self, principal: Principal, id: &MediaId) -> Result<(), WatchError> {
        if !tracked(id) {
            return Err(WatchError::NotTracked(id.clone()));
        }
        let found = self.catalog.item(id).await?;
        self.store
            .hide_continue(principal, &id.to_string(), &found.external_ids)
            .await?;
        Ok(())
    }

    /// Undo [`Watch::hide`].
    pub async fn unhide(&self, principal: Principal, id: &MediaId) -> Result<(), WatchError> {
        if !tracked(id) {
            return Err(WatchError::NotTracked(id.clone()));
        }
        self.store
            .unhide_continue(principal, &id.to_string())
            .await?;
        Ok(())
    }

    /// Mark `id` watched or not. Either way its progress starts over.
    pub async fn mark(
        &self,
        principal: Principal,
        id: &MediaId,
        watched: bool,
    ) -> Result<(), WatchError> {
        match id.kind() {
            MediaKind::Season | MediaKind::Show => {
                return self.mark_episodes(principal, id, watched).await;
            }
            _ if !tracked(id) => return Err(WatchError::NotTracked(id.clone())),
            _ => {}
        }
        let found = self.catalog.item(id).await?;
        let duration = self.duration(id, &found.item).await;
        self.save(principal, id, &found, None, duration, watched)
            .await
    }

    /// Mark every episode of a season, or of a show's seasons (specials
    /// included), as a client's "mark season watched" means. The state is
    /// kept per episode, as if each were marked: there is none for a
    /// season. Durations are the catalog's, not probed: a show can have
    /// hundreds of files, and a mark does not need the exact length.
    async fn mark_episodes(
        &self,
        principal: Principal,
        id: &MediaId,
        watched: bool,
    ) -> Result<(), WatchError> {
        let found = self.catalog.item(id).await?;
        let mut seasons = Vec::new();
        match found.item {
            Item::Show(_) => {
                for child in &found.children {
                    if let Item::Season(s) = child {
                        seasons.push(s.id.clone());
                    }
                }
            }
            _ => seasons.push(id.clone()),
        }
        let seasons: Vec<_> = stream::iter(seasons)
            .map(|season| async move { self.catalog.item(&season).await })
            .buffered(CATALOG_FETCHES)
            .try_collect()
            .await?;
        let episodes: Vec<(MediaId, Option<Duration>)> = seasons
            .iter()
            .flat_map(|season| season.children.iter())
            .filter_map(|child| match child {
                Item::Episode(e) => Some((e.id.clone(), e.duration)),
                _ => None,
            })
            .collect();
        // Fetched a few ahead, saved one by one in order: a failure on one
        // leaves those before it saved and none after.
        let mut fetched = stream::iter(episodes)
            .map(|(id, duration)| async move {
                let found = self.catalog.item(&id).await;
                (id, duration, found)
            })
            .buffered(CATALOG_FETCHES);
        while let Some((id, duration, found)) = fetched.next().await {
            let episode = found?;
            self.save(principal, &id, &episode, None, duration, watched)
                .await?;
        }
        Ok(())
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
        self.next_up.lock().remove(&principal);
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
        length(probed, item)
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
        // The three lookups are reads of separate tables: asked side by
        // side. The moves after them stay in order.
        let (watch, plays, hidden) = tokio::try_join!(
            async {
                if !tracked(id) {
                    return Ok(None);
                }
                self.store
                    .watch_under_another_key(principal, &media_id, external_ids)
                    .await
            },
            // Its plays follow it (PX-52), all but those of a copy.
            async {
                if PlayedKind::of(id.kind()).is_none() {
                    return Ok(None);
                }
                self.store
                    .plays_under_another_key(principal, &media_id, external_ids)
                    .await
            },
            // A hide follows it too (P1-22).
            self.store
                .hidden_under_another_key(principal, &media_id, external_ids),
        )?;
        if let Some(old) = watch
            && self.left(&old, &media_id, external_ids).await
        {
            self.store.move_watch(principal, &old, &media_id).await?;
        }
        if let Some(old) = plays
            && self.left(&old, &media_id, external_ids).await
        {
            self.store.move_plays(principal, &old, &media_id).await?;
        }
        if let Some(old) = hidden
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
        let fresh = |at: &Instant| self.since(*at) < SAME_ITEM_CACHE;
        if self.copies.lock().get(&pair).is_some_and(fresh) {
            return false;
        }
        match self.catalog.external_ids(&id).await {
            Ok(there) if there.iter().any(|id| external_ids.contains(id)) => {
                let mut copies = self.copies.lock();
                copies.retain(|_, at| fresh(at));
                copies.insert(pair, self.now());
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
                .map(|p| Duration::from_millis(p.max(0) as u64));
            // Only a movie or an episode has a row: `ids` holds no other.
            if let Some(playback) = playback_mut(item) {
                *playback = self.rules().playback_state(position, row.watched);
            }
        }
        Ok(())
    }

    /// Set `last_viewed_at` on `items` from `principal`'s watch state:
    /// when it was last saved, by a progress report from any device of
    /// theirs, a mark as watched or unwatched, or a Plex sync with Plex's
    /// own time. Asked for only where a reply carries it to a client that
    /// takes it (`spoolframe:last-viewed`); [`Watch::fill`] leaves it out.
    pub async fn fill_last_viewed(
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
            if !tracked(item.id()) {
                continue;
            }
            let at = states.get(&item.id().to_string()).map(|row| row.updated_at);
            if let Some(playback) = playback_mut(item) {
                playback.last_viewed_at = at;
            }
        }
        Ok(())
    }

    /// A page of what `principal` watched or started, the most recently
    /// first (PX-42's `watch_history`): each item as the catalog has it,
    /// with their state, and how many states there are. An item gone from
    /// the catalog, not served now, or on a Plex server that is down is
    /// left out of the page, as Continue watching leaves it out.
    pub async fn history(
        &self,
        principal: Principal,
        offset: u32,
        limit: u32,
    ) -> Result<(Vec<Item>, u32), WatchError> {
        let (rows, total) = self.store.watch_history(principal, offset, limit).await?;
        let mut items = self
            .look_up(rows.into_iter().map(|row| row.media_id), "watch history")
            .await?;
        let mut all: Vec<&mut Item> = items.iter_mut().collect();
        self.fill(principal, &mut all).await?;
        Ok((items, total))
    }

    /// The items of `ids` as the catalog has them, in their order, a few
    /// at a time. One gone from the catalog, not served now, or on a Plex
    /// server that is down is left out (logged as left out of `list`).
    async fn look_up(
        &self,
        ids: impl IntoIterator<Item = String>,
        list: &'static str,
    ) -> Result<Vec<Item>, WatchError> {
        let ids: Vec<MediaId> = ids
            .into_iter()
            .filter_map(|id| id.parse::<MediaId>().ok())
            .collect();
        let found: Vec<(MediaId, Result<_, CatalogError>)> = stream::iter(ids)
            .map(|id| async move {
                let item = self.catalog.item(&id).await;
                (id, item)
            })
            .buffered(CATALOG_FETCHES)
            .collect()
            .await;
        let mut items = Vec::new();
        for (id, item) in found {
            match item {
                Ok(found) => items.push(found.item),
                Err(CatalogError::NotFound(_) | CatalogError::NotServed(_)) => {}
                Err(e) if is_down(&e) => {
                    tracing::warn!(item = %id, error = %e, "{list}: left out");
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(items)
    }

    /// `principal`'s home screen.
    pub async fn home(&self, principal: Principal) -> Result<HomeScreen, WatchError> {
        // Cards removed (P1-22): movies and episodes, each by its own id.
        let hidden = self.store.hidden(principal).await?;
        let mut continue_watching = Vec::new();
        let min = ms(self.rules().started_after);
        // As many more as are hidden, so the row stays full.
        let wanted = (ROW + hidden.len()) as u32;
        let ids: Vec<MediaId> = self
            .store
            .in_progress(principal, min, wanted)
            .await?
            .into_iter()
            .filter(|row| !hidden.contains(&row.media_id))
            .filter_map(|row| row.media_id.parse::<MediaId>().ok())
            .collect();
        // Looked up a few at a time, in the row's order: one Plex server
        // down costs one timeout, not one per item on it.
        let mut found = stream::iter(ids)
            .map(|id| async move {
                let item = self.catalog.item(&id).await;
                (id, item)
            })
            .buffered(CATALOG_FETCHES);
        while continue_watching.len() < ROW {
            let Some((id, item)) = found.next().await else {
                break;
            };
            // An item gone from the catalog, or in a library not served
            // now, drops out of the row; so does one whose Plex server is
            // down (P3-11), until it is back, while the others still show.
            match item {
                Ok(found) => continue_watching.push(found.item),
                Err(CatalogError::NotFound(_) | CatalogError::NotServed(_)) => {}
                Err(e) if is_down(&e) => {
                    tracing::warn!(item = %id, error = %e, "Continue watching: left out");
                }
                Err(e) => return Err(e.into()),
            }
        }
        let mut next_up = self.next_up(principal).await?;
        // One row per show: while an episode of a show is in progress, the
        // show is in Continue watching and has nothing in Next up. Every
        // episode in progress counts, not only the ones the row has room
        // for, but not one removed: its card is not there to hold the
        // show. Judged here, after the cache, so an episode just started,
        // or a card just removed, shows at once.
        let shows: Vec<String> = next_up
            .iter()
            .filter_map(|item| match item {
                Item::Episode(e) => Some(e.show_id.to_string()),
                _ => None,
            })
            .collect();
        let started: HashSet<String> = self
            .store
            .in_progress_in_shows(principal, min, &shows)
            .await?
            .into_iter()
            .filter(|row| !hidden.contains(&row.media_id))
            .filter_map(|row| row.show_id)
            .collect();
        // A removed Next up episode stays out until it is watched, which
        // moves Next up past it.
        next_up.retain(|item| match item {
            Item::Episode(e) => {
                !hidden.contains(&e.id.to_string()) && !started.contains(&e.show_id.to_string())
            }
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
        if let Some((at, items)) = self.next_up.lock().get(&principal)
            && self.since(*at) < self.rules().next_up_cache
        {
            return Ok(items.clone());
        }
        let since = self.store.now() - self.rules().next_up_window.as_secs() as i64;
        let shows: Vec<MediaId> = self
            .store
            .active_shows(principal, since, NEXT_UP_SHOWS)
            .await?
            .into_iter()
            .filter_map(|show| show.parse::<MediaId>().ok())
            .collect();
        let mut next = Vec::new();
        let mut found = stream::iter(shows)
            .map(|show_id| async move {
                let episode = self.next_in_show(principal, &show_id).await;
                (show_id, episode)
            })
            .buffered(CATALOG_FETCHES);
        while let Some((show_id, episode)) = found.next().await {
            match episode {
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
            .insert(principal, (self.now(), next.clone()));
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
        // All of them at once: one request to Plex, not one per season.
        let episodes = match self.catalog.episodes(show).await {
            Ok(episodes) => show_run(episodes),
            Err(CatalogError::NotFound(_) | CatalogError::NotServed(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
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
        let Some(next) = episodes.into_iter().nth(latest + 1) else {
            return Ok(None);
        };
        // Started already: it is in Continue watching, never in both rows.
        let id = next.id().to_string();
        let min = ms(self.rules().started_after);
        let started = self
            .store
            .watch_states(principal, std::slice::from_ref(&id))
            .await?
            .get(&id)
            .is_some_and(|row| !row.watched && row.position_ms.is_some_and(|p| p >= min));
        Ok((!started).then_some(next))
    }
}

/// A show's run, of its `episodes` (the catalog's): specials (season 0)
/// are not part of it, and the rest go by season and number. Next up and
/// binge-ahead (PX-37) both go on along it.
pub(crate) fn show_run(mut episodes: Vec<Item>) -> Vec<Item> {
    episodes.retain(|e| matches!(e, Item::Episode(e) if e.season > 0));
    episodes.sort_by_key(|e| match e {
        Item::Episode(e) => (e.season, e.episode),
        _ => (u32::MAX, u32::MAX),
    });
    episodes
}

#[cfg(test)]
mod tests;
