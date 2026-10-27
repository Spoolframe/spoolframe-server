//! Watch state rules and Home (P1-14, ARCHITECTURE §36): what counts as
//! started and watched, how often progress is written, and what Home
//! shows. Storage is `crate::store::watch`.

use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{StreamExt, TryStreamExt, stream};

use spoolframe_catalog::{Catalog, CatalogError, Reach};
use spoolframe_media::Prober;
use spoolframe_protocol::HomeScreen;
use spoolframe_types::{Item, MediaId, MediaKind, Namespace, PlaybackState};

use crate::principal::Principal;
use crate::store::plays::PlayedKind;
use crate::store::watch::WatchUpdate;
use crate::store::{Store, StoreError};
pub use spoolframe_policy::watch::WatchRules;
use spoolframe_policy::watch::credits_to_end;

mod browse;
mod plays;
pub use plays::{OfflinePlay, Plays};

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

/// How long a show's external ids are kept for its episodes' title keys.
const SHOW_IDS_CACHE: Duration = Duration::from_secs(10 * 60);

const ROW: usize = 20;
const NEXT_UP_SHOWS: u32 = 20;
/// The most catalog (Plex) lookups one Home load or one show-wide mark
/// has in flight at once: they go out a few at a time, and come back in
/// the order asked, so rows, errors and saves keep their order.
const CATALOG_FETCHES: usize = 8;

/// Whose Next up is cached, and whether a paired server's shows are in it
/// (P8-02).
type NextUpKey = (Principal, bool);

pub struct Watch {
    store: Store,
    catalog: Arc<dyn Catalog>,
    prober: Arc<Prober>,
    /// A live setting (P3-10): read at each use.
    rules: parking_lot::RwLock<WatchRules>,
    /// Each (principal, item)'s last progress write, while it is still
    /// within the throttle.
    written: Mutex<HashMap<(Principal, String), Written>>,
    next_up: Mutex<HashMap<NextUpKey, (Instant, Vec<Item>)>>,
    /// (Another key, an item): when the catalog last said that the key
    /// holds a copy of the item, so its state is not the item's to take.
    copies: Mutex<HashMap<(String, String), Instant>>,
    /// Each show's external ids, as the catalog last gave them: what its
    /// episodes' title keys are made of, asked once a while rather than at
    /// each progress report.
    show_ids: Mutex<HashMap<MediaId, (Instant, Vec<String>)>>,
    /// Whose state was saved, for whoever mirrors it elsewhere (PX-04):
    /// each one, since every linked user's state syncs (P3-12).
    changed: tokio::sync::broadcast::Sender<Principal>,
    /// Libraries walked for a filter or sort by watch state, kept for the
    /// pages after the first ([`browse`]).
    walks: browse::Walks,
}

/// Where a device got to in an item while it had no server (PX-38).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Offline {
    /// Where it stopped, if it played.
    pub position: Option<Duration>,
    /// Marked watched (`true`) or unwatched (`false`) there.
    pub watched: Option<bool>,
    /// When, by the device's clock (Unix seconds).
    pub at: i64,
    /// The device's clock now, as it sent this (Unix seconds).
    pub device_now: i64,
}

/// What became of an [`Offline`] state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Synced {
    /// Written: newer than the server's.
    Applied,
    /// Not written: the server's is as new or newer.
    Older,
    /// Not written: the item has no watch state, or is not there.
    NotTracked,
}

impl From<Synced> for spoolframe_protocol::downloads::OfflineWatchOutcome {
    fn from(synced: Synced) -> Self {
        match synced {
            Synced::Applied => Self::Applied,
            Synced::Older => Self::Older,
            Synced::NotTracked => Self::NotTracked,
        }
    }
}

/// A progress write, for judging the reports the throttle skips.
#[derive(Debug, Clone)]
struct Written {
    at: Instant,
    /// The item's [`credits_to_end`], as the catalog had them then.
    credits: Option<Duration>,
    /// The row the state was written to: the item's own, or, for a paired
    /// server's copy of a title the user has here, this server's copy's
    /// (P8-03). What the throttle judges and un-hides.
    row: String,
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
            show_ids: Mutex::default(),
            changed: tokio::sync::broadcast::Sender::new(CHANGES),
            walks: browse::Walks::default(),
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
            .cloned()
            .filter(|written| self.since(written.at) < self.rules().throttle);
        if let Some(written) = recent {
            // Within the throttle only a crossing into watched is written,
            // judged on what the last write knew (the row's duration and
            // whether it is watched already, and the item's credits): no
            // catalog or probe work for the many updates in between.
            let stored = self
                .store
                .watch_states(principal, std::slice::from_ref(&written.row))
                .await?;
            let row = stored.get(&written.row);
            let duration = row
                .and_then(|r| r.duration_ms)
                .map(|d| Duration::from_millis(d.max(0) as u64));
            let crossing = self.rules().is_watched(position, duration, written.credits)
                && !row.is_some_and(|r| r.watched);
            if !crossing {
                // Not written, but played: it is back in Continue watching
                // (P1-22).
                self.store.clear_hidden(principal, &written.row).await?;
                return Ok(());
            }
        }
        let found = self.catalog.item(id).await?;
        let duration = self.duration(id, &found.item).await;
        let credits = credits_to_end(&found.markers);
        let watched = self.rules().is_watched(position, duration, credits);
        let row = self
            .save_as_of(
                principal,
                id,
                &found,
                Some(position),
                duration,
                watched,
                None,
            )
            .await?
            .unwrap_or_else(|| key.1.clone());
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
                row,
            },
        );
        Ok(())
    }

    /// Take in where `principal` got to in `id` on a device with no
    /// server (PX-38): written only when newer than the state the server
    /// has, newest winning whichever device it came from. The device's
    /// clock is set right first: its `at` is moved by how far its clock
    /// is from the server's, and kept no later than now. Watched is
    /// judged by the rules from the position and the item's credits, as
    /// any progress is, or marked as the device says; the throttle does
    /// not apply. Written, it is told as any save is (PX-04's Plex
    /// mirror). Each of `entries` in turn, the items fetched a few ahead:
    /// a failure on one leaves those before it written and none after.
    pub async fn offline(
        &self,
        principal: Principal,
        entries: Vec<(MediaId, Offline)>,
    ) -> Result<Vec<Synced>, WatchError> {
        let mut fetched = stream::iter(entries)
            .map(|(id, offline)| async move {
                let found = if tracked(&id) {
                    self.served_item(&id).await
                } else {
                    Ok(None)
                };
                (id, offline, found)
            })
            .buffered(CATALOG_FETCHES);
        let mut synced = Vec::new();
        while let Some((id, offline, found)) = fetched.next().await {
            synced.push(match found? {
                Some(found) => self.offline_one(principal, &id, &found, offline).await?,
                None => Synced::NotTracked,
            });
        }
        Ok(synced)
    }

    /// [`Watch::offline`] of `offline` in `id`, `found`.
    async fn offline_one(
        &self,
        principal: Principal,
        id: &MediaId,
        found: &spoolframe_catalog::ItemWithChildren,
        offline: Offline,
    ) -> Result<Synced, WatchError> {
        let at = self.store.server_time_of(offline.at, offline.device_now);
        let duration = self.duration(id, &found.item).await;
        let (position, watched) = match (offline.position, offline.watched) {
            (Some(position), marked) => {
                let credits = credits_to_end(&found.markers);
                let judged = self.rules().is_watched(position, duration, credits);
                (Some(position), judged || marked == Some(true))
            }
            // Marked, as `mark` does: its progress starts over.
            (None, Some(marked)) => (None, marked),
            // Nothing to write is newer than nothing.
            (None, None) => return Ok(Synced::Older),
        };
        let written = self
            .save_as_of(principal, id, found, position, duration, watched, Some(at))
            .await?;
        Ok(if written.is_some() {
            Synced::Applied
        } else {
            Synced::Older
        })
    }

    /// Item `id`, or `None` when it is gone, another catalog's, or in a
    /// library no longer served: an offline entry's answer alone
    /// (`NotTracked`), never its batch's.
    async fn served_item(
        &self,
        id: &MediaId,
    ) -> Result<Option<spoolframe_catalog::ItemWithChildren>, WatchError> {
        match self.catalog.item(id).await {
            Ok(found) => Ok(Some(found)),
            Err(e) if crate::client::unserved(&e) => Ok(None),
            Err(e) => Err(e.into()),
        }
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
        let title = self.title_key(&found).await;
        self.store
            .hide_continue(principal, &id.to_string(), &title)
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
        self.save_as_of(principal, id, found, position, duration, watched, None)
            .await
            .map(|_| ())
    }

    /// [`Watch::save`], now; or, with `at` (Unix seconds), as of then and
    /// only when newer than the state there is: the row written, the
    /// item's own or its title's copy here (P8-03), or `None` when it was
    /// not.
    #[allow(clippy::too_many_arguments)]
    async fn save_as_of(
        &self,
        principal: Principal,
        id: &MediaId,
        found: &spoolframe_catalog::ItemWithChildren,
        position: Option<Duration>,
        duration: Option<Duration>,
        watched: bool,
        at: Option<i64>,
    ) -> Result<Option<String>, WatchError> {
        let media_id = id.to_string();
        let title = self.title_key(found).await;
        // Bring over any state this item had under an earlier key first.
        self.relink(principal, id, &title).await?;
        // One state per title (P8-03): a paired server's copy writes the
        // state of this server's own copy, when the user has one; the
        // title's other states on paired servers' keys are folded into
        // the one written. This server's own copies keep theirs.
        let rows = self.store.title_rows(principal, &title).await?;
        let into = match rows.iter().find(|row| !remote(row)) {
            Some(own) if id.namespace() == Namespace::Federated => own.clone(),
            _ => media_id.clone(),
        };
        let folded: Vec<String> = rows
            .into_iter()
            .chain(std::iter::once(media_id.clone()))
            .filter(|row| remote(row) && *row != into)
            .collect();
        let show_id = match &found.item {
            Item::Episode(e) => Some(e.show_id.to_string()),
            _ => None,
        };
        let own = into == media_id;
        let update = WatchUpdate {
            media_id: &into,
            // Another copy's row keeps its own show, length and ids: what
            // a paired server says of its copy changes nothing of it.
            show_id: show_id.as_deref().filter(|_| own),
            external_ids: if own { &title } else { &[] },
            position_ms: position.map(ms),
            duration_ms: duration.map(ms).filter(|_| own),
            watched,
            unhide: true,
        };
        if !self
            .store
            .save_title_watch(principal, update, &folded, at)
            .await?
        {
            return Ok(None);
        }
        // What is next up may have changed, and what a filter by watch
        // state keeps.
        self.next_up
            .lock()
            .retain(|(cached, _), _| *cached != principal);
        self.walks.forget(principal);
        // Nobody listening is fine: nothing syncs.
        let _ = self.changed.send(principal);
        Ok(Some(into))
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

    /// The title `found` is a copy of (P8-03): the ids its state, plays
    /// and hide are kept with, and what its other copies, here or on a
    /// paired server, are matched to it by. Its external ids, and for an
    /// episode its show's, with its season and number
    /// (`tmdb://1396#s1e2`), as two servers' copies of an episode have
    /// their show's ids in common more often than their own. Every write
    /// of a state, a play or a hide keys by this, whichever path made it.
    /// A show the catalog cannot place now adds none.
    pub async fn title_key(&self, found: &spoolframe_catalog::ItemWithChildren) -> Vec<String> {
        let mut ids = found.external_ids.clone();
        if let Item::Episode(episode) = &found.item {
            match self.show_ids(&episode.show_id).await {
                Ok(show) => ids.extend(spoolframe_catalog::episode_ids(
                    &show,
                    episode.season,
                    episode.episode,
                )),
                Err(e) => tracing::debug!(
                    show = %episode.show_id,
                    error = %e,
                    "an episode is matched by its own ids alone"
                ),
            }
        }
        ids
    }

    /// Show `id`'s external ids, from the catalog at most once a while.
    async fn show_ids(&self, id: &MediaId) -> Result<Vec<String>, CatalogError> {
        let fresh = |at: &Instant| self.since(*at) < SHOW_IDS_CACHE;
        if let Some((_, ids)) = self.show_ids.lock().get(id).filter(|(at, _)| fresh(at)) {
            return Ok(ids.clone());
        }
        let ids = self.catalog.external_ids(id).await?;
        let mut cached = self.show_ids.lock();
        cached.retain(|_, (at, _)| fresh(at));
        cached.insert(id.clone(), (self.now(), ids.clone()));
        Ok(ids)
    }

    /// [`Watch::fill`] for one item, a copy of the title `title`
    /// ([`Watch::title_key`]): with no state of its own, or an older one,
    /// it shows the newest state of its title's other copies (P8-03), so
    /// the film half watched from a paired server resumes here where it
    /// was left, and the other way round.
    pub async fn fill_title(
        &self,
        principal: Principal,
        item: &mut Item,
        title: &[String],
    ) -> Result<(), WatchError> {
        self.fill_titles(principal, &mut [(item, title)]).await
    }

    /// [`Watch::fill_title`] for each of `items`, each with its title, in
    /// two reads however many there are: a search's hits (P8-05).
    pub async fn fill_titles(
        &self,
        principal: Principal,
        items: &mut [(&mut Item, &[String])],
    ) -> Result<(), WatchError> {
        let mut ids: Vec<String> = items
            .iter()
            .filter(|(item, _)| tracked(item.id()))
            .flat_map(|(_, title)| title.iter().cloned())
            .collect();
        ids.sort();
        ids.dedup();
        let rows = self.store.title_rows_by_id(principal, &ids).await?;
        // Each item's keys: its title's copies', and its own.
        let keys: Vec<Vec<String>> = items
            .iter()
            .map(|(item, title)| {
                if !tracked(item.id()) {
                    return Vec::new();
                }
                let mut keys: Vec<String> = rows
                    .iter()
                    .filter(|(id, _)| title.contains(id))
                    .map(|(_, key)| key.clone())
                    .collect();
                keys.push(item.id().to_string());
                keys.sort();
                keys.dedup();
                keys
            })
            .collect();
        let mut every: Vec<String> = keys.iter().flatten().cloned().collect();
        every.sort();
        every.dedup();
        let states = self.store.watch_states(principal, &every).await?;
        for ((item, _), keys) in items.iter_mut().zip(&keys) {
            let Some(newest) = keys
                .iter()
                .filter_map(|key| states.get(key))
                .max_by_key(|row| row.updated_at)
            else {
                continue;
            };
            let position = newest
                .position_ms
                .map(|p| Duration::from_millis(p.max(0) as u64));
            if let Some(playback) = playback_mut(item) {
                *playback = self.rules().playback_state(position, newest.watched);
            }
        }
        Ok(())
    }

    /// Whether the item `media_id`, with `external_ids`, has left `key`:
    /// the catalog has nothing under it now, or another item (after a
    /// rebuild Plex hands the same keys out again), or it is not a key of
    /// this catalog. It has not if what is under `key` shares an external
    /// id with the item (an episode's show ids, season and number count,
    /// [`Watch::title_key`]). If the catalog cannot say, it has not, for
    /// now.
    ///
    /// A copy on another server (one key a paired server's, P8-03) never
    /// has: a state is never moved between servers' keys, so nothing a
    /// paired server says of its items takes a state from this server's
    /// copy. A title's copies share their state by its key instead
    /// ([`Watch::fill_title`]).
    async fn left(&self, key: &str, media_id: &str, external_ids: &[String]) -> bool {
        let Ok(id) = key.parse::<MediaId>() else {
            return false;
        };
        if media_id
            .parse::<MediaId>()
            .is_ok_and(|to| to.origin() != id.origin())
        {
            return false;
        }
        let pair = (key.to_owned(), media_id.to_owned());
        let fresh = |at: &Instant| self.since(*at) < SAME_ITEM_CACHE;
        if self.copies.lock().get(&pair).is_some_and(fresh) {
            return false;
        }
        let there = match self.catalog.external_ids(&id).await {
            // Matched by its show's ids, season and number only: an
            // episode looked at whole.
            Ok(there)
                if id.kind() == MediaKind::Episode
                    && !there.iter().any(|id| external_ids.contains(id)) =>
            {
                match self.catalog.item(&id).await {
                    Ok(found) => Ok(self.title_key(&found).await),
                    Err(e) => Err(e),
                }
            }
            there => there,
        };
        match there {
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
    /// own time. Asked for only where a reply carries it; [`Watch::fill`]
    /// leaves it out.
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
        Ok(self.look_up_kept(ids, list, false).await?.0)
    }

    /// [`Watch::look_up`]; with `federated`, a paired server's item no
    /// longer available (its pair or library gone) is kept, as the cache
    /// last had it, and named in the second list (P8-03).
    async fn look_up_kept(
        &self,
        ids: impl IntoIterator<Item = String>,
        list: &'static str,
        federated: bool,
    ) -> Result<(Vec<Item>, Vec<MediaId>), WatchError> {
        let ids: Vec<MediaId> = ids
            .into_iter()
            .filter_map(|id| id.parse::<MediaId>().ok())
            .collect();
        let found: Vec<(MediaId, Result<_, CatalogError>)> = stream::iter(ids)
            .map(|id| async move {
                let item = self.catalog.item_kept(&id).await;
                (id, item)
            })
            .buffered(CATALOG_FETCHES)
            .collect()
            .await;
        let mut items = Vec::new();
        let mut unavailable = Vec::new();
        for (id, item) in found {
            match item {
                // A paired server's item whose pair or library went stays,
                // as no longer available (P8-02).
                Ok((found, Reach::Gone)) => {
                    if federated {
                        items.push(found.item);
                        unavailable.push(id);
                    }
                }
                Ok((found, _)) => items.push(found.item),
                Err(CatalogError::NotFound(_) | CatalogError::NotServed(_)) => {}
                Err(e) if is_down(&e) => {
                    tracing::warn!(item = %id, error = %e, "{list}: left out");
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok((items, unavailable))
    }

    /// `principal`'s home screen, with nothing of a paired server's in
    /// it: for a client that did not name the experimental `federation`
    /// feature (P8-02).
    pub async fn home(&self, principal: Principal) -> Result<HomeScreen, WatchError> {
        self.home_as(principal, false).await
    }

    /// `principal`'s home screen. With `federated` (a client that named
    /// the experimental `federation` feature, P8-02), a paired server's
    /// items are in Continue watching and Next up as the server's own are,
    /// and those no longer available (their pair or library gone) stay,
    /// listed as such; without, none of them is, and the rows fill with
    /// the server's own as if it had no pairs.
    pub async fn home_as(
        &self,
        principal: Principal,
        federated: bool,
    ) -> Result<HomeScreen, WatchError> {
        // Cards removed (P1-22): movies and episodes, each by its own id.
        let hidden = self.store.hidden(principal).await?;
        let min = ms(self.rules().started_after);
        // As many more as are hidden, so the row stays full.
        let wanted = (ROW + hidden.len()) as u32;
        let ids: Vec<String> = self
            .store
            .in_progress(principal, min, wanted, federated)
            .await?
            .into_iter()
            .map(|row| row.media_id)
            .filter(|id| !hidden.contains(id))
            .collect();
        // An item gone from the catalog, or in a library not served now,
        // drops out of the row; so does one whose Plex server is down
        // (P3-11), until it is back, while the others still show.
        let (mut continue_watching, mut unavailable) = self
            .look_up_kept(ids, "Continue watching", federated)
            .await?;
        continue_watching.truncate(ROW);
        unavailable.retain(|id| continue_watching.iter().any(|item| item.id() == id));
        let mut next_up = self.next_up(principal, federated).await?;
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
            unavailable,
            servers: Vec::new(),
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
    /// A paired server's shows only when `federated` (P8-02).
    async fn next_up(
        &self,
        principal: Principal,
        federated: bool,
    ) -> Result<Vec<Item>, WatchError> {
        if let Some((at, items)) = self.next_up.lock().get(&(principal, federated))
            && self.since(*at) < self.rules().next_up_cache
        {
            return Ok(items.clone());
        }
        let since = self.store.now() - self.rules().next_up_window.as_secs() as i64;
        let shows: Vec<MediaId> = self
            .store
            .active_shows(principal, since, NEXT_UP_SHOWS, federated)
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
            .insert((principal, federated), (self.now(), next.clone()));
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

/// Whether `key` is a paired server's item's.
fn remote(key: &str) -> bool {
    key.starts_with("fed:")
}

/// The paired server `id` is from, if it is one's (P8-02): `None` for this
/// server's own.
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
