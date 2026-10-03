//! Two-way watch state sync with the Plex owner's account (PX-04).
//!
//! One Spoolframe user's watch state mirrors the account the PMS serves us
//! as: the owner, on a LAN PMS that lets clients in without a token. Each
//! run reads both sides whole and plans per item; nothing is kept between
//! runs, so a missed run or a Plex outage loses nothing.
//!
//! The newer side wins, by Plex's `lastViewedAt` against the row's
//! `updated_at`. A pull keeps Plex's timestamp, so the next run finds both
//! sides equal and does nothing.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use spoolframe_catalog::CatalogError;
use spoolframe_plex::{PlexCatalog, PlexWatch};
use spoolframe_types::{MediaId, MediaKind, Namespace};

use crate::principal::Principal;
use crate::store::watch::{WatchRow, WatchUpdate};
use crate::store::{Store, StoreError};

/// Positions this close count as the same: a client reports every few
/// seconds, and Plex rounds.
const SAME_POSITION_MS: u64 = 30_000;

/// How long after a Spoolframe change the sync waits, so a burst of
/// progress reports is one run.
const SETTLE: Duration = Duration::from_secs(5);

/// The least time between a run and the next one a Spoolframe change
/// asks for. A run reads all of Plex's watch state, and during a playback
/// progress changes every few seconds.
const CHANGE_INTERVAL: Duration = Duration::from_secs(60);

/// One side's state of one item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct State {
    watched: bool,
    /// A resume position worth keeping.
    position_ms: Option<u64>,
    /// When it changed, Unix seconds.
    at: i64,
}

impl State {
    fn same(&self, other: &State) -> bool {
        match (self.position_ms, other.position_ms) {
            // A rewatch in progress may be "watched" on one side only.
            (Some(a), Some(b)) => a.abs_diff(b) <= SAME_POSITION_MS,
            (None, None) => self.watched == other.watched,
            _ => false,
        }
    }
}

/// What one run does to one item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Take Plex's state.
    Pull {
        id: MediaId,
    },
    PushWatched {
        id: MediaId,
    },
    PushUnwatched {
        id: MediaId,
    },
    PushPosition {
        id: MediaId,
        position_ms: u64,
    },
}

/// The Spoolframe side of an item, or `None` when it has nothing to say:
/// no row, or only a few seconds of trying the file.
fn ours(row: &WatchRow, started_ms: u64) -> Option<State> {
    let position = row.position_ms.and_then(|p| u64::try_from(p).ok());
    let started = position.filter(|&p| p >= started_ms);
    if !row.watched && position.is_some() && started.is_none() {
        return None;
    }
    Some(State {
        watched: row.watched,
        position_ms: started,
        at: row.updated_at,
    })
}

fn theirs(watch: &PlexWatch) -> State {
    State {
        watched: watch.watched,
        position_ms: watch.position_ms,
        at: watch.last_viewed_at.unwrap_or(0),
    }
}

/// The Spoolframe state, sent to Plex.
fn push(id: &MediaId, state: State, plex: Option<&State>) -> Vec<Action> {
    match (state.watched, state.position_ms) {
        (_, Some(position_ms)) => vec![Action::PushPosition {
            id: id.clone(),
            position_ms,
        }],
        (true, None) => vec![Action::PushWatched { id: id.clone() }],
        // Marked unwatched here: only worth sending if Plex has something.
        (false, None) if plex.is_some() => vec![Action::PushUnwatched { id: id.clone() }],
        (false, None) => Vec::new(),
    }
}

/// Plan one run. Only movies and episodes from Plex take part.
pub fn plan(rows: &[WatchRow], plex: &[PlexWatch], started: Duration) -> Vec<Action> {
    let started_ms = u64::try_from(started.as_millis()).unwrap_or(u64::MAX);
    let ours: HashMap<MediaId, State> = rows
        .iter()
        .filter_map(|row| {
            let id: MediaId = row.media_id.parse().ok()?;
            let synced = id.namespace() == Namespace::Plex
                && matches!(id.kind(), MediaKind::Movie | MediaKind::Episode);
            synced.then(|| Some((id, ours(row, started_ms)?)))?
        })
        .collect();
    let theirs: HashMap<&MediaId, State> = plex.iter().map(|w| (&w.id, theirs(w))).collect();

    // In a stable order, for the report and the tests.
    let ids: BTreeMap<String, &MediaId> = ours
        .keys()
        .chain(theirs.keys().copied())
        .map(|id| (id.to_string(), id))
        .collect();
    let mut actions = Vec::new();
    for (_, id) in ids {
        match (ours.get(id), theirs.get(id)) {
            (None, Some(_)) => actions.push(Action::Pull { id: id.clone() }),
            (Some(here), None) => actions.extend(push(id, *here, None)),
            (Some(here), Some(there)) if here.same(there) => {}
            (Some(here), Some(there)) if there.at > here.at => {
                actions.push(Action::Pull { id: id.clone() });
            }
            (Some(here), Some(there)) => actions.extend(push(id, *here, Some(there))),
            (None, None) => {}
        }
    }
    actions
}

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("Plex: {0}")]
    Plex(#[from] CatalogError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// What a run did, or would do, one line per item.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub pulled: Vec<String>,
    pub pushed: Vec<String>,
    pub failed: Vec<String>,
}

pub struct PlexSync {
    plex: Arc<PlexCatalog>,
    store: Store,
    principal: Principal,
    started: Duration,
    interval: Duration,
    /// One run at a time.
    running: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for PlexSync {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlexSync")
            .field("principal", &self.principal)
            .field("interval", &self.interval)
            .finish_non_exhaustive()
    }
}

fn clock(ms: u64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

impl PlexSync {
    /// Mirror `principal`'s state with the account `plex` serves as.
    /// `started` is the watch rules' threshold for "in progress".
    pub fn new(
        plex: Arc<PlexCatalog>,
        store: Store,
        principal: Principal,
        started: Duration,
        interval: Duration,
    ) -> Self {
        Self {
            plex,
            store,
            principal,
            started,
            interval,
            running: tokio::sync::Mutex::default(),
        }
    }

    /// One run: plan, and apply unless `dry_run`.
    pub async fn run(&self, dry_run: bool) -> Result<Report, SyncError> {
        let _running = self.running.lock().await;
        let plex = self.plex.watch_states().await?;
        let rows = self.store.all_watch(self.principal).await?;
        let by_id: HashMap<&MediaId, &PlexWatch> = plex.iter().map(|w| (&w.id, w)).collect();
        let mut report = Report::default();
        for action in plan(&rows, &plex, self.started) {
            let (line, pulled) = match &action {
                Action::Pull { id } => {
                    let w = by_id[id];
                    let state = match (w.watched, w.position_ms) {
                        (_, Some(p)) => format!("at {}", clock(p)),
                        (true, None) => "watched".to_owned(),
                        (false, None) => "unwatched".to_owned(),
                    };
                    (format!("{}: {state}", w.title), true)
                }
                Action::PushWatched { id } => (format!("{id}: watched"), false),
                Action::PushUnwatched { id } => (format!("{id}: unwatched"), false),
                Action::PushPosition { id, position_ms } => {
                    (format!("{id}: at {}", clock(*position_ms)), false)
                }
            };
            if !dry_run && let Err(e) = self.apply(&action, &by_id).await {
                tracing::warn!(item = line, error = %e, "Plex watch sync");
                report.failed.push(format!("{line} ({e})"));
                continue;
            }
            if pulled {
                report.pulled.push(line);
            } else {
                report.pushed.push(line);
            }
        }
        Ok(report)
    }

    async fn apply(
        &self,
        action: &Action,
        plex: &HashMap<&MediaId, &PlexWatch>,
    ) -> Result<(), SyncError> {
        match action {
            Action::Pull { id } => {
                let w = plex[id];
                let media_id = id.to_string();
                let show = w.show.as_ref().map(ToString::to_string);
                self.store
                    .save_watch_at(
                        self.principal,
                        WatchUpdate {
                            media_id: &media_id,
                            show_id: show.as_deref(),
                            external_ids: &w.external_ids,
                            position_ms: w.position_ms.map(|p| p as i64),
                            duration_ms: w.duration_ms.map(|d| d as i64),
                            watched: w.watched,
                            // Hides are this server's own (P1-22).
                            unhide: false,
                        },
                        w.last_viewed_at.unwrap_or(0),
                    )
                    .await?;
            }
            Action::PushWatched { id } => self.plex.set_watched(id, true).await?,
            Action::PushUnwatched { id } => self.plex.set_watched(id, false).await?,
            Action::PushPosition { id, position_ms } => {
                self.plex.set_position(id, *position_ms).await?;
            }
        }
        Ok(())
    }

    /// Run now, every interval, and shortly after this principal's state
    /// changes in Spoolframe, for as long as the server runs.
    pub fn spawn(self: Arc<Self>, changes: tokio::sync::watch::Receiver<Option<Principal>>) {
        tokio::spawn(async move {
            schedule(self.interval, changes, self.principal, || self.run_logged()).await;
        });
    }

    async fn run_logged(&self) {
        match self.run(false).await {
            Ok(r) if r.pulled.is_empty() && r.pushed.is_empty() && r.failed.is_empty() => {}
            Ok(r) => tracing::info!(
                pulled = r.pulled.len(),
                pushed = r.pushed.len(),
                failed = r.failed.len(),
                "Plex watch sync"
            ),
            Err(e) => tracing::warn!(error = %e, "Plex watch sync; trying again later"),
        }
    }
}

/// When the sync runs: now, every `interval`, and shortly after a change
/// of `principal`'s state, until nothing can change any more. A change
/// brings the next run forward to [`SETTLE`] after it, but to no sooner
/// than [`CHANGE_INTERVAL`] after the run before.
async fn schedule<F, Fut>(
    interval: Duration,
    mut changes: tokio::sync::watch::Receiver<Option<Principal>>,
    principal: Principal,
    mut run: F,
) where
    F: FnMut() -> Fut,
    Fut: Future<Output = ()>,
{
    loop {
        run().await;
        let ran = tokio::time::Instant::now();
        let timer = ran + interval;
        tokio::select! {
            () = tokio::time::sleep_until(timer) => {}
            changed = wait_for(&mut changes, principal) => {
                if !changed {
                    return;
                }
                let settled = tokio::time::Instant::now() + SETTLE;
                let soonest = settled.max(ran + CHANGE_INTERVAL);
                tokio::time::sleep_until(soonest.min(timer)).await;
            }
        }
    }
}

/// Wait for a change of `principal`'s state; `false` once nothing can
/// change any more.
async fn wait_for(
    changes: &mut tokio::sync::watch::Receiver<Option<Principal>>,
    principal: Principal,
) -> bool {
    loop {
        if changes.changed().await.is_err() {
            return false;
        }
        if *changes.borrow_and_update() == Some(principal) {
            return true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STARTED: Duration = Duration::from_secs(60);

    fn id(kind: MediaKind, key: &str) -> MediaId {
        MediaId::new(Namespace::Plex, kind, key).unwrap()
    }

    fn episode(key: &str) -> MediaId {
        id(MediaKind::Episode, key)
    }

    fn row(media: &MediaId, watched: bool, position_ms: Option<i64>, at: i64) -> WatchRow {
        WatchRow {
            media_id: media.to_string(),
            show_id: None,
            position_ms,
            duration_ms: Some(3_600_000),
            watched,
            updated_at: at,
        }
    }

    fn plex(media: &MediaId, watched: bool, position_ms: Option<u64>, at: i64) -> PlexWatch {
        PlexWatch {
            id: media.clone(),
            show: None,
            watched,
            position_ms,
            duration_ms: Some(3_600_000),
            last_viewed_at: Some(at),
            external_ids: Vec::new(),
            title: media.to_string(),
        }
    }

    #[test]
    fn the_same_state_on_both_sides_does_nothing() {
        let e = episode("1");
        let m = id(MediaKind::Movie, "2");
        assert!(
            plan(
                &[row(&e, true, None, 10), row(&m, false, Some(600_000), 10)],
                &[plex(&e, true, None, 50), plex(&m, false, Some(610_000), 50)],
                STARTED,
            )
            .is_empty()
        );
    }

    #[test]
    fn what_only_plex_knows_is_pulled() {
        let banshee = episode("229060");
        assert_eq!(
            plan(&[], &[plex(&banshee, true, None, 100)], STARTED),
            [Action::Pull { id: banshee }]
        );
    }

    #[test]
    fn a_newer_plex_state_is_pulled_and_a_newer_spoolframe_one_pushed() {
        let e = episode("1");
        assert_eq!(
            plan(
                &[row(&e, false, Some(600_000), 10)],
                &[plex(&e, true, None, 20)],
                STARTED
            ),
            [Action::Pull { id: e.clone() }]
        );
        assert_eq!(
            plan(
                &[row(&e, true, None, 30)],
                &[plex(&e, false, Some(600_000), 20)],
                STARTED
            ),
            [Action::PushWatched { id: e.clone() }]
        );
    }

    #[test]
    fn what_only_spoolframe_knows_is_pushed() {
        let e = episode("1");
        let m = id(MediaKind::Movie, "2");
        assert_eq!(
            plan(
                &[row(&e, true, None, 10), row(&m, false, Some(120_000), 10)],
                &[],
                STARTED
            ),
            [
                Action::PushWatched { id: e },
                Action::PushPosition {
                    id: m,
                    position_ms: 120_000
                },
            ]
        );
    }

    #[test]
    fn a_few_seconds_of_trying_a_file_is_never_pushed() {
        let e = episode("228796");
        assert!(plan(&[row(&e, false, Some(2_822), 100)], &[], STARTED).is_empty());
        // Nor does it hide what Plex has.
        assert_eq!(
            plan(
                &[row(&e, false, Some(2_822), 100)],
                &[plex(&e, false, Some(900_000), 50)],
                STARTED
            ),
            [Action::Pull { id: e }]
        );
    }

    #[test]
    fn marked_unwatched_here_after_plex_s_last_view_is_pushed() {
        let e = episode("1");
        assert_eq!(
            plan(
                &[row(&e, false, None, 30)],
                &[plex(&e, true, None, 20)],
                STARTED
            ),
            [Action::PushUnwatched { id: e.clone() }]
        );
        // Unwatched here with nothing in Plex: nothing to say.
        assert!(plan(&[row(&e, false, None, 30)], &[], STARTED).is_empty());
    }

    #[test]
    fn a_pull_keeps_plex_s_time_so_the_next_plan_is_empty() {
        let e = episode("1");
        let there = plex(&e, false, Some(1_932_000), 1_790_906_395);
        assert_eq!(
            plan(
                &[row(&e, true, None, 10)],
                std::slice::from_ref(&there),
                STARTED
            ),
            [Action::Pull { id: e.clone() }]
        );
        // What the pull writes: Plex's state, as of Plex's time.
        let pulled = row(&e, false, Some(1_932_000), 1_790_906_395);
        assert!(plan(&[pulled], &[there], STARTED).is_empty());
    }

    #[test]
    fn a_rewatch_in_progress_is_the_same_on_both_sides() {
        let e = episode("1");
        // Plex: watched before, now 20 minutes in; Spoolframe: the same
        // position, not marked watched.
        assert!(
            plan(
                &[row(&e, false, Some(1_200_000), 10)],
                &[plex(&e, true, Some(1_210_000), 50)],
                STARTED
            )
            .is_empty()
        );
    }

    const ME: Principal = Principal::LocalUser(crate::store::users::LocalUserId(1));

    /// Run [`schedule`] every 2 minutes; the seconds at which it ran.
    fn scheduled() -> (
        tokio::sync::watch::Sender<Option<Principal>>,
        Arc<std::sync::Mutex<Vec<u64>>>,
    ) {
        let (changed, changes) = tokio::sync::watch::channel(None);
        let runs = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ran = runs.clone();
        let start = tokio::time::Instant::now();
        tokio::spawn(schedule(Duration::from_secs(120), changes, ME, move || {
            ran.lock().unwrap().push(start.elapsed().as_secs());
            async {}
        }));
        (changed, runs)
    }

    /// A film playing: progress is written every 10 s. Each run reads all
    /// of Plex's watch state, so it is not one run per write.
    #[tokio::test(start_paused = true)]
    async fn changes_all_through_a_playback_run_the_sync_once_a_minute() {
        let (changed, runs) = scheduled();
        tokio::time::sleep(Duration::from_secs(3)).await;
        for _ in 0..30 {
            changed.send_replace(Some(ME));
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
        assert_eq!(*runs.lock().unwrap(), [0, 60, 120, 180, 240, 300]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_change_a_while_after_the_last_run_is_synced_within_seconds() {
        let (changed, runs) = scheduled();
        tokio::time::sleep(Duration::from_secs(70)).await;
        changed.send_replace(Some(ME));
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert_eq!(*runs.lock().unwrap(), [0, 75]);
    }

    #[tokio::test(start_paused = true)]
    async fn without_changes_the_sync_runs_every_interval() {
        let (_changed, runs) = scheduled();
        tokio::time::sleep(Duration::from_secs(250)).await;
        assert_eq!(*runs.lock().unwrap(), [0, 120, 240]);
    }

    #[test]
    fn only_plex_movies_and_episodes_take_part() {
        let track = id(MediaKind::Track, "5");
        assert!(plan(&[row(&track, true, None, 10)], &[], STARTED).is_empty());
    }
}
