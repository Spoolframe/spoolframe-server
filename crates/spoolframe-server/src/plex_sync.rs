//! Watch state sync with each linked user's Plex account (PX-04, P3-12).
//!
//! Every user with a Plex link ([`crate::plex_accounts`]) has their own
//! runs: their Spoolframe state against their own account's on each Plex
//! server their libraries come from. Each run reads both sides whole and
//! plans per item; nothing is kept between runs but what is cheap to ask
//! again, so a missed run or a Plex outage loses nothing.
//!
//! Each library says which way it syncs: `from Plex` only takes Plex's
//! state, `to Plex` only sends Spoolframe's, `off` does neither, and
//! `both` is PX-04's rules: the newer side wins, by Plex's `lastViewedAt`
//! against the row's `updated_at`. A pull keeps Plex's timestamp, so the
//! next run finds both sides equal and does nothing: no ping-pong.

use futures_util::stream::{self, StreamExt};
use parking_lot::Mutex;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use spoolframe_admin_api::{PlexSyncRun, SyncDirection};
use spoolframe_catalog::CatalogError;
use spoolframe_plex::{PlexCatalog, PlexWatch};
use spoolframe_types::{LibraryKind, MediaId, MediaKind, Namespace};

use crate::catalog::{Composite, LiveCatalog, PlexSource, Synced, ids};
use crate::plex_accounts::{AccountLink, Link, PlexAccounts};
use crate::principal::Principal;
use crate::store::StoreError;
use crate::store::users::LocalUserId;
use crate::store::watch::{WatchRow, WatchUpdate};

/// The most section lookups a sync asks one PMS at once: the user's own
/// server, so a modest load.
const SECTION_LOOKUPS: usize = 8;

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

/// One run's plan for one Plex server.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub actions: Vec<Action>,
    /// Items that take part and are left as they are: the same on both
    /// sides, or held back by their library's direction.
    pub skipped: u32,
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
fn push(id: &MediaId, state: State, plex: Option<&State>) -> Option<Action> {
    match (state.watched, state.position_ms) {
        (_, Some(position_ms)) => Some(Action::PushPosition {
            id: id.clone(),
            position_ms,
        }),
        (true, None) => Some(Action::PushWatched { id: id.clone() }),
        // Marked unwatched here: only worth sending if Plex has something.
        (false, None) if plex.is_some() => Some(Action::PushUnwatched { id: id.clone() }),
        (false, None) => None,
    }
}

/// Whether `id` takes part at all: a Plex movie or episode.
fn synced_kind(id: &MediaId) -> bool {
    id.namespace() == Namespace::Plex && matches!(id.kind(), MediaKind::Movie | MediaKind::Episode)
}

fn started_ms(started: Duration) -> u64 {
    u64::try_from(started.as_millis()).unwrap_or(u64::MAX)
}

/// The rows with something to say that Plex does not list: Spoolframe's
/// alone, whose library has to be looked up before anything is sent.
pub fn only_here(rows: &[WatchRow], plex: &[PlexWatch], started: Duration) -> Vec<MediaId> {
    let listed: HashSet<&MediaId> = plex.iter().map(|w| &w.id).collect();
    rows.iter()
        .filter(|row| ours(row, started_ms(started)).is_some())
        .filter_map(|row| row.media_id.parse::<MediaId>().ok())
        .filter(|id| synced_kind(id) && !listed.contains(id))
        .collect()
}

/// Plan one run for one Plex server: `rows` are the user's state of that
/// server's items and `plex` their account's there, with the same IDs;
/// `direction` says which way each item's library syncs.
pub fn plan(
    rows: &[WatchRow],
    plex: &[PlexWatch],
    started: Duration,
    direction: impl Fn(&MediaId) -> SyncDirection,
) -> Plan {
    let started_ms = started_ms(started);
    let ours: HashMap<MediaId, State> = rows
        .iter()
        .filter_map(|row| {
            let id: MediaId = row.media_id.parse().ok()?;
            synced_kind(&id).then(|| Some((id, ours(row, started_ms)?)))?
        })
        .collect();
    let theirs: HashMap<&MediaId, State> = plex.iter().map(|w| (&w.id, theirs(w))).collect();

    // In a stable order, for the report and the tests.
    let ids: BTreeMap<String, &MediaId> = ours
        .keys()
        .chain(theirs.keys().copied())
        .map(|id| (id.to_string(), id))
        .collect();
    let mut plan = Plan::default();
    for (_, id) in ids {
        let way = direction(id);
        if way == SyncDirection::Off {
            continue;
        }
        let action = match (ours.get(id), theirs.get(id)) {
            (None, Some(_)) => way.pulls().then(|| Action::Pull { id: id.clone() }),
            (Some(here), None) => push(id, *here, None).filter(|_| way.pushes()),
            (Some(here), Some(there)) if here.same(there) => None,
            (Some(here), Some(there)) if there.at > here.at => {
                way.pulls().then(|| Action::Pull { id: id.clone() })
            }
            (Some(here), Some(there)) => push(id, *here, Some(there)).filter(|_| way.pushes()),
            (None, None) => None,
        };
        match action {
            Some(action) => plan.actions.push(action),
            None => plan.skipped += 1,
        }
    }
    plan
}

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("Plex: {0}")]
    Plex(#[from] CatalogError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("cannot read the Plex link: {0}")]
    Link(#[from] std::io::Error),
    #[error("not linked to a Plex account")]
    NotLinked,
}

/// What a run did, or would do, one line per item.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub pulled: Vec<String>,
    pub pushed: Vec<String>,
    pub failed: Vec<String>,
    pub skipped: u32,
    /// The libraries synced.
    pub libraries: u32,
    /// What was skipped whole: "Cabin movies skipped: Cabin can't be
    /// reached".
    pub notes: Vec<String>,
}

impl Report {
    fn run(&self, at: i64) -> PlexSyncRun {
        let count = |v: &Vec<String>| u32::try_from(v.len()).unwrap_or(u32::MAX);
        PlexSyncRun {
            at,
            libraries: self.libraries,
            pulled: count(&self.pulled),
            pushed: count(&self.pushed),
            skipped: self.skipped,
            failed: count(&self.failed),
            notes: self.notes.clone(),
        }
    }
}

/// One user's runs.
#[derive(Debug, Default)]
struct UserRuns {
    /// One run at a time.
    running: tokio::sync::Mutex<()>,
    last: Mutex<Option<PlexSyncRun>>,
}

/// A user's account on one server, ready to read and write.
struct OnServer {
    plex: PlexCatalog,
    /// The movie and show sections synced, each with its direction.
    sections: HashMap<String, SyncDirection>,
    /// The libraries, for notes.
    libraries: Vec<String>,
}

/// The watch rules' threshold for "in progress" a sync judges by.
#[derive(Clone)]
pub enum StartedAfter {
    /// The same for the server's life (tests).
    Fixed(Duration),
    /// The live rules' (`watch.started_after_secs` changes at run time),
    /// read at each run.
    Rules(Arc<crate::watch::Watch>),
}

impl StartedAfter {
    fn get(&self) -> Duration {
        match self {
            Self::Fixed(started) => *started,
            Self::Rules(watch) => watch.rules().started_after,
        }
    }
}

impl From<Duration> for StartedAfter {
    fn from(started: Duration) -> Self {
        Self::Fixed(started)
    }
}

/// The watch sync of every linked user.
pub struct WatchSync {
    /// The Plex servers and libraries as they are at each run: live
    /// settings (P3-10, P3-11).
    catalog: Arc<LiveCatalog>,
    accounts: Arc<PlexAccounts>,
    started: StartedAfter,
    interval: Duration,
    users: Mutex<HashMap<LocalUserId, Arc<UserRuns>>>,
    /// The section an item only Spoolframe knows is in, by PMS address and
    /// key: an item never moves, and asking Plex at every run would be a
    /// call per item.
    sections: Mutex<HashMap<(String, MediaId), Option<String>>>,
    /// Each PMS's machine identifier, by address.
    machines: Mutex<HashMap<String, String>>,
    /// The token each linked account uses on each server, by machine
    /// identifier, as plex.tv's resources listed them.
    access: Mutex<HashMap<LocalUserId, HashMap<String, String>>>,
    /// Each linked user's scheduled runs.
    tasks: Mutex<HashMap<LocalUserId, tokio::task::AbortHandle>>,
    /// Runs since the server started that synced, and that could not
    /// (P3-14). A dry run is not one.
    runs_ok: crate::metrics::Counter,
    runs_failed: crate::metrics::Counter,
    /// Where each run's time goes.
    histograms: std::sync::OnceLock<Arc<crate::metrics::Histograms>>,
}

impl std::fmt::Debug for WatchSync {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatchSync")
            .field("interval", &self.interval)
            .finish_non_exhaustive()
    }
}

fn clock(ms: u64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// Why a server was skipped, for people.
fn why(server: &str, e: &CatalogError) -> String {
    match e {
        CatalogError::Unreachable { .. } => format!("{server} can't be reached"),
        other => format!("{server} answered: {other}"),
    }
}

impl WatchSync {
    /// The sync of every user `accounts` links. `started` is the watch
    /// rules' threshold for "in progress", read at each run; `interval`
    /// how often each user is synced when nothing changes.
    pub fn new(
        catalog: Arc<LiveCatalog>,
        accounts: Arc<PlexAccounts>,
        started: impl Into<StartedAfter>,
        interval: Duration,
    ) -> Self {
        Self {
            catalog,
            accounts,
            started: started.into(),
            interval,
            users: Mutex::default(),
            sections: Mutex::default(),
            machines: Mutex::default(),
            access: Mutex::default(),
            tasks: Mutex::default(),
            runs_ok: Default::default(),
            runs_failed: Default::default(),
            histograms: std::sync::OnceLock::new(),
        }
    }

    /// Observe each run's time into `histograms` (P3-14).
    pub fn record_into(&self, histograms: Arc<crate::metrics::Histograms>) {
        let _ = self.histograms.set(histograms);
    }

    /// Runs that synced, and runs that could not, since the server started
    /// (P3-14).
    pub fn run_counts(&self) -> (u64, u64) {
        (self.runs_ok.get(), self.runs_failed.get())
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }

    pub fn accounts(&self) -> &Arc<PlexAccounts> {
        &self.accounts
    }

    fn runs(&self, user: LocalUserId) -> Arc<UserRuns> {
        self.users.lock().entry(user).or_default().clone()
    }

    /// `user`'s last run since the server started, and whether one is
    /// going on.
    pub fn status(&self, user: LocalUserId) -> (Option<PlexSyncRun>, bool) {
        let runs = self.runs(user);
        let last = runs.last.lock().clone();
        (last, runs.running.try_lock().is_err())
    }

    /// One run of `user`'s: plan, and apply unless `dry_run`.
    pub async fn run(&self, user: LocalUserId, dry_run: bool) -> Result<Report, SyncError> {
        use tracing::Instrument as _;
        // Never the user: spans are exported (P3-14). The run's log lines
        // name them.
        let span = tracing::info_span!("plex sync run", dry_run);
        let began = std::time::Instant::now();
        let ran = self.run_once(user, dry_run).instrument(span).await;
        if !dry_run {
            let outcome = match &ran {
                Ok(_) => {
                    self.runs_ok.inc();
                    "ok"
                }
                Err(_) => {
                    self.runs_failed.inc();
                    "failed"
                }
            };
            if let Some(histograms) = self.histograms.get() {
                histograms.observe(
                    &crate::metrics::histogram::PLEX_SYNC_DURATION,
                    began.elapsed().as_secs_f64(),
                    &[("outcome", outcome)],
                );
            }
        }
        ran
    }

    /// Each Plex server as `user`'s linked account sees it: its own
    /// token there, every section (PX-40's import reads their playlists),
    /// by the server's id here. A server the account has no access to is
    /// left out, with a note.
    pub async fn user_views(
        &self,
        user: LocalUserId,
    ) -> Result<(Vec<(String, PlexCatalog)>, Vec<String>), SyncError> {
        let link = self.accounts.link(user)?.ok_or(SyncError::NotLinked)?;
        let composite = self.catalog.composite().ok_or(CatalogError::NotSetUp)?;
        let (mut views, mut notes) = (Vec::new(), Vec::new());
        let mut refreshed = false;
        for server in composite.servers() {
            let plex = match &link {
                Link::Owner => server.catalog.unrestricted(),
                Link::Account(account) => {
                    match self.token_on(user, account, server, &mut refreshed).await {
                        Ok(token) => server.catalog.with_token(Some(token)),
                        Err(reason) => {
                            notes.push(reason);
                            continue;
                        }
                    }
                }
            };
            views.push((server.id.clone(), plex));
        }
        Ok((views, notes))
    }

    async fn run_once(&self, user: LocalUserId, dry_run: bool) -> Result<Report, SyncError> {
        let runs = self.runs(user);
        let _running = runs.running.lock().await;
        let link = self.accounts.link(user)?.ok_or(SyncError::NotLinked)?;
        let composite = self.catalog.composite().ok_or(CatalogError::NotSetUp)?;
        let principal = Principal::LocalUser(user);
        let rows = self.accounts.store().all_watch(principal).await?;
        let mut report = Report::default();
        let mut refreshed = false;
        for server in composite.servers() {
            let on = self
                .on_server(user, &link, server, &composite, &mut refreshed, &mut report)
                .await;
            if let Some(on) = on {
                self.sync_server(principal, server, on, &rows, dry_run, &mut report)
                    .await;
            }
        }
        if !dry_run {
            *runs.last.lock() = Some(report.run(self.accounts.store().now()));
        }
        Ok(report)
    }

    /// `user`'s account on `server` and the sections synced there; `None`,
    /// with a note when it matters, when nothing is synced there.
    async fn on_server(
        &self,
        user: LocalUserId,
        link: &Link,
        server: &PlexSource,
        composite: &Composite,
        refreshed: &mut bool,
        report: &mut Report,
    ) -> Option<OnServer> {
        let (mut sections, mut libraries) = (HashMap::new(), Vec::new());
        match composite.synced(&server.id) {
            Synced::Sections(chosen) => {
                for (section, way, name) in chosen {
                    sections.insert(section, way);
                    libraries.push(name);
                }
            }
            Synced::Every(way) => match server.catalog.sections().await {
                Ok(all) => {
                    for s in all {
                        if matches!(s.kind, Some(LibraryKind::Movies | LibraryKind::Shows)) {
                            sections.insert(s.key, way);
                            libraries.push(s.title);
                        }
                    }
                }
                Err(e) => {
                    report.notes.push(format!(
                        "{}'s libraries skipped: {}",
                        server.name,
                        why(&server.name, &e)
                    ));
                    return None;
                }
            },
        }
        if sections.is_empty() {
            return None;
        }
        let plex = match link {
            Link::Owner => server.catalog.unrestricted(),
            Link::Account(account) => match self.token_on(user, account, server, refreshed).await {
                Ok(token) => server.catalog.with_token(Some(token)),
                Err(reason) => {
                    report
                        .notes
                        .push(format!("{} skipped: {reason}", libraries.join(", ")));
                    return None;
                }
            },
        };
        Some(OnServer {
            plex,
            sections,
            libraries,
        })
    }

    /// The token `account` uses on `server`: plex.tv's for that server, or
    /// the account's own when plex.tv cannot be asked (it works on the
    /// account's own servers). plex.tv is asked at most once a run, and
    /// only when the server is not known yet.
    async fn token_on(
        &self,
        user: LocalUserId,
        account: &AccountLink,
        server: &PlexSource,
        refreshed: &mut bool,
    ) -> Result<String, String> {
        let machine = self
            .machine(&server.catalog)
            .await
            .map_err(|e| why(&server.name, &e))?;
        let no_access = || format!("the linked Plex account has no access to {}", server.name);
        if let Some(token) = self.known_token(user, &machine) {
            return Ok(token);
        }
        if *refreshed {
            return Err(no_access());
        }
        *refreshed = true;
        match self.accounts.servers(&account.token).await {
            Ok(resources) => {
                let tokens: HashMap<String, String> = resources
                    .into_iter()
                    .filter_map(|r| Some((r.machine_id, r.access_token?)))
                    .collect();
                self.access.lock().insert(user, tokens);
                self.known_token(user, &machine).ok_or_else(no_access)
            }
            Err(e) => {
                tracing::warn!(
                    user = %user,
                    error = %e,
                    "cannot list the linked Plex account's servers; trying its own token"
                );
                Ok(account.token.clone())
            }
        }
    }

    fn known_token(&self, user: LocalUserId, machine: &str) -> Option<String> {
        self.access
            .lock()
            .get(&user)
            .and_then(|tokens| tokens.get(machine).cloned())
    }

    /// The machine identifier of the PMS `plex` reaches, asked once.
    async fn machine(&self, plex: &PlexCatalog) -> Result<String, CatalogError> {
        let url = plex.base_url().to_owned();
        let known = self.machines.lock().get(&url).cloned();
        if let Some(id) = known {
            return Ok(id);
        }
        // A PMS that names no machine is not remembered as "": every
        // token looked up under it would be none, and the run would say
        // the account has no access until a restart (review 2026-10-11
        // L35). The next run asks again.
        let Some(id) = plex.machine_id().await? else {
            return Err(CatalogError::Backend {
                url,
                call: "GET /identity".into(),
                item: None,
                message: "no machineIdentifier in the answer".into(),
            });
        };
        self.machines.lock().insert(url, id.clone());
        Ok(id)
    }

    /// Forget what `user`'s account was told about servers: plex.tv is
    /// asked again at their next run.
    fn forget_access(&self, user: LocalUserId) {
        self.access.lock().remove(&user);
    }

    /// The section each of `ids` is in, asked of Plex once per item, up to
    /// [`SECTION_LOOKUPS`] at once; left out for one Plex does not have.
    async fn sections_of(&self, plex: &PlexCatalog, ids: &[MediaId]) -> HashMap<MediaId, String> {
        let url = plex.base_url().to_owned();
        let mut found = HashMap::new();
        let mut missing = Vec::new();
        {
            let cache = self.sections.lock();
            for id in ids {
                match cache.get(&(url.clone(), id.clone())) {
                    Some(Some(section)) => {
                        found.insert(id.clone(), section.clone());
                    }
                    Some(None) => {}
                    None => missing.push(id.clone()),
                }
            }
        }
        let asked: Vec<_> = stream::iter(missing)
            .map(|id| async move {
                let section = plex.section_of(&id).await;
                (id, section)
            })
            .buffer_unordered(SECTION_LOOKUPS)
            .collect()
            .await;
        for (id, section) in asked {
            let asked = match section {
                Ok(section) => Some(section),
                // Gone from Plex: nothing to send it to.
                Err(CatalogError::NotFound(_)) => Some(None),
                // Asked again next run.
                Err(e) => {
                    tracing::debug!(item = %id, error = %e, "cannot tell which library an item is in");
                    None
                }
            };
            if let Some(section) = &asked {
                self.sections
                    .lock()
                    .insert((url.clone(), id.clone()), section.clone());
            }
            if let Some(Some(section)) = asked {
                found.insert(id, section);
            }
        }
        found
    }

    async fn sync_server(
        &self,
        principal: Principal,
        server: &PlexSource,
        on: OnServer,
        rows: &[WatchRow],
        dry_run: bool,
        report: &mut Report,
    ) {
        // Only a local user links a Plex account (P7-06).
        let Principal::LocalUser(user) = principal else {
            return;
        };
        // The rules as they are now: they change at run time.
        let started = self.started.get();
        let keys: HashSet<String> = on.sections.keys().cloned().collect();
        let plex = match on.plex.watch_states(Some(&keys)).await {
            Ok(plex) => plex,
            Err(e) => {
                self.forget_access(user);
                report.notes.push(format!(
                    "{} skipped: {}",
                    on.libraries.join(", "),
                    why(&server.name, &e)
                ));
                return;
            }
        };
        report.libraries += u32::try_from(on.libraries.len()).unwrap_or(u32::MAX);
        // Both sides in the server's own IDs: the rows of its items only.
        let rows: Vec<WatchRow> = rows
            .iter()
            .filter_map(|row| {
                let id: MediaId = row.media_id.parse().ok()?;
                let (from, local) = ids::media_local(&id);
                (from == server.id).then(|| WatchRow {
                    media_id: local.to_string(),
                    ..row.clone()
                })
            })
            .collect();
        let mut section: HashMap<MediaId, String> = plex
            .iter()
            .map(|w| (w.id.clone(), w.section.clone()))
            .collect();
        let here = only_here(&rows, &plex, started);
        section.extend(self.sections_of(&on.plex, &here).await);
        let direction = |id: &MediaId| {
            section
                .get(id)
                .and_then(|s| on.sections.get(s))
                .copied()
                .unwrap_or(SyncDirection::Off)
        };
        let planned = plan(&rows, &plex, started, direction);
        report.skipped += planned.skipped;
        let by_id: HashMap<&MediaId, &PlexWatch> = plex.iter().map(|w| (&w.id, w)).collect();
        for action in planned.actions {
            let qualified = |id: &MediaId| ids::media(&server.id, id.clone());
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
                Action::PushWatched { id } => (format!("{}: watched", qualified(id)), false),
                Action::PushUnwatched { id } => (format!("{}: unwatched", qualified(id)), false),
                Action::PushPosition { id, position_ms } => (
                    format!("{}: at {}", qualified(id), clock(*position_ms)),
                    false,
                ),
            };
            if !dry_run
                && let Err(e) = self
                    .apply(principal, &server.id, &on.plex, &action, &by_id)
                    .await
            {
                tracing::warn!(user = %user, item = line, error = %e, "Plex watch sync");
                report.failed.push(format!("{line} ({e})"));
                continue;
            }
            if pulled {
                report.pulled.push(line);
            } else {
                report.pushed.push(line);
            }
        }
    }

    async fn apply(
        &self,
        principal: Principal,
        server: &str,
        plex: &PlexCatalog,
        action: &Action,
        theirs: &HashMap<&MediaId, &PlexWatch>,
    ) -> Result<(), SyncError> {
        match action {
            Action::Pull { id } => {
                let w = theirs[id];
                let media_id = ids::media(server, id.clone()).to_string();
                let show = w
                    .show
                    .as_ref()
                    .map(|s| ids::media(server, s.clone()).to_string());
                self.accounts
                    .store()
                    .save_watch_at(
                        principal,
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
            Action::PushWatched { id } => plex.set_watched(id, true).await?,
            Action::PushUnwatched { id } => plex.set_watched(id, false).await?,
            Action::PushPosition { id, position_ms } => {
                plex.set_position(id, *position_ms).await?;
            }
        }
        Ok(())
    }

    /// Run every linked user now, every interval and soon after their
    /// state changes (`schedule`), and follow links made and removed,
    /// for as long as the server runs.
    pub fn start(self: &Arc<Self>, changes: tokio::sync::broadcast::Receiver<Principal>) {
        let sync = self.clone();
        // Before anything else can link: none is missed.
        let mut links = self.accounts.changes();
        tokio::spawn(async move {
            sync.reconcile(&changes).await;
            loop {
                let user = match links.recv().await {
                    Ok(user) => user,
                    // Links made or removed were missed: who is linked is
                    // read again, as at start (review 2026-10-11 L36).
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                        tracing::warn!(missed, "Plex link changes were missed; reading them again");
                        sync.reconcile(&changes).await;
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                };
                match sync.accounts.link(user) {
                    Ok(Some(_)) => sync.follow(user, changes.resubscribe()),
                    Ok(None) => sync.unfollow(user),
                    Err(e) => tracing::warn!(user = %user, error = %e, "cannot read a Plex link"),
                }
            }
        });
    }

    /// Follow every user linked to Plex now and no one else: at start,
    /// and after link changes were missed.
    async fn reconcile(self: &Arc<Self>, changes: &tokio::sync::broadcast::Receiver<Principal>) {
        let users = match self.accounts.users().await {
            Ok(users) => users,
            Err(e) => {
                tracing::warn!(error = %e, "cannot list who is linked to Plex");
                return;
            }
        };
        let linked: HashSet<LocalUserId> = users
            .into_iter()
            .filter(|(_, link)| link.is_some())
            .map(|(user, _)| user.id)
            .collect();
        let followed: Vec<LocalUserId> = self.tasks.lock().keys().copied().collect();
        for user in &followed {
            if !linked.contains(user) {
                self.unfollow(*user);
            }
        }
        for user in linked {
            if !followed.contains(&user) {
                self.follow(user, changes.resubscribe());
            }
        }
    }

    /// Start (or start again) `user`'s scheduled runs.
    fn follow(
        self: &Arc<Self>,
        user: LocalUserId,
        changes: tokio::sync::broadcast::Receiver<Principal>,
    ) {
        self.forget_access(user);
        let sync = self.clone();
        let task = tokio::spawn(async move {
            let principal = Principal::LocalUser(user);
            schedule(sync.interval, changes, principal, || sync.run_logged(user)).await;
        });
        let before = self.tasks.lock().insert(user, task.abort_handle());
        if let Some(before) = before {
            before.abort();
        }
    }

    /// Stop `user`'s runs: unlinked, not synced.
    fn unfollow(&self, user: LocalUserId) {
        let task = self.tasks.lock().remove(&user);
        if let Some(task) = task {
            task.abort();
        }
        self.forget_access(user);
        *self.runs(user).last.lock() = None;
    }

    async fn run_logged(&self, user: LocalUserId) {
        match self.run(user, false).await {
            Ok(r) => tracing::info!(
                user = %user,
                pulled = r.pulled.len(),
                pushed = r.pushed.len(),
                skipped = r.skipped,
                failed = r.failed.len(),
                notes = r.notes.join("; "),
                "Plex watch sync"
            ),
            Err(e) => {
                tracing::warn!(user = %user, error = %e, "Plex watch sync; trying again later");
                *self.runs(user).last.lock() = Some(PlexSyncRun {
                    at: self.accounts.store().now(),
                    notes: vec![format!("Not synced: {e}")],
                    ..PlexSyncRun::default()
                });
            }
        }
    }
}

/// When a user's sync runs: now, every `interval`, and shortly after a
/// change of `principal`'s state, until nothing can change any more. A
/// change brings the next run forward to [`SETTLE`] after it, but to no
/// sooner than [`CHANGE_INTERVAL`] after the run before.
async fn schedule<F, Fut>(
    interval: Duration,
    mut changes: tokio::sync::broadcast::Receiver<Principal>,
    principal: Principal,
    mut run: F,
) where
    F: FnMut() -> Fut,
    Fut: Future<Output = ()>,
{
    use tokio::sync::broadcast::error::TryRecvError;
    loop {
        // What changed before this run is in it.
        while !matches!(
            changes.try_recv(),
            Err(TryRecvError::Empty | TryRecvError::Closed)
        ) {}
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
/// change any more. Falling behind counts as a change.
async fn wait_for(
    changes: &mut tokio::sync::broadcast::Receiver<Principal>,
    principal: Principal,
) -> bool {
    use tokio::sync::broadcast::error::RecvError;
    loop {
        match changes.recv().await {
            Ok(changed) if changed == principal => return true,
            Ok(_) => {}
            Err(RecvError::Lagged(_)) => return true,
            Err(RecvError::Closed) => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sync judges "in progress" by the rules as they are at its run,
    /// not as they were at start (review 2026-10-11 M27).
    #[tokio::test]
    async fn the_live_rules_threshold_is_read_at_each_run() {
        use crate::watch::{Watch, WatchRules};
        let dir = spoolframe_test_dir::TestDir::new("spoolframe-sync-started").await;
        let store = crate::store::Store::open(dir.as_ref()).await.unwrap();
        let watch = Arc::new(Watch::new(
            store,
            Arc::new(LiveCatalog::default()),
            Arc::new(spoolframe_media::Prober::new(
                spoolframe_media::sandbox::Tool::unconfined("ffprobe"),
                spoolframe_media::Prober::DEFAULT_TIMEOUT,
            )),
            WatchRules::default(),
        ));
        let started = StartedAfter::Rules(watch.clone());
        let later = Duration::from_secs(600);
        assert_ne!(started.get(), later);
        watch.set_rules(WatchRules {
            started_after: later,
            ..WatchRules::default()
        });
        assert_eq!(started.get(), later);
    }

    const STARTED: Duration = Duration::from_secs(60);

    fn accounts_at(
        dir: &spoolframe_test_dir::TestDir,
        store: crate::store::Store,
    ) -> Arc<PlexAccounts> {
        Arc::new(PlexAccounts::new(
            crate::config::secrets::Secrets::new(dir.as_ref()),
            store,
            crate::plex_accounts::PlexSite {
                api: "http://127.0.0.1:9".parse().unwrap(),
                app: "http://127.0.0.1:9".parse().unwrap(),
            },
            None,
        ))
    }

    /// A PMS that names no machine is an error, not "" kept until a
    /// restart: once it names one, it is found (review 2026-10-11 L35).
    #[tokio::test]
    async fn a_pms_that_names_no_machine_is_asked_again() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let dir = spoolframe_test_dir::TestDir::new("spoolframe-sync-machine").await;
        let store = crate::store::Store::open(dir.as_ref()).await.unwrap();
        let sync = WatchSync::new(Arc::default(), accounts_at(&dir, store), STARTED, STARTED);
        let pms = MockServer::start().await;
        let plex = PlexCatalog::new(pms.uri().parse().unwrap(), None).unwrap();
        Mock::given(method("GET"))
            .and(path("/identity"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "MediaContainer": {} })),
            )
            .up_to_n_times(1)
            .mount(&pms)
            .await;
        Mock::given(method("GET"))
            .and(path("/identity"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "MediaContainer": { "machineIdentifier": "tower" } }),
            ))
            .mount(&pms)
            .await;
        assert!(sync.machine(&plex).await.is_err());
        assert_eq!(sync.machine(&plex).await.unwrap(), "tower");
    }

    /// After missed link changes, a user followed but no longer linked
    /// is let go (review 2026-10-11 L36).
    #[tokio::test]
    async fn a_reconcile_lets_go_of_who_is_no_longer_linked() {
        let dir = spoolframe_test_dir::TestDir::new("spoolframe-sync-reconcile").await;
        let store = crate::store::Store::open(dir.as_ref()).await.unwrap();
        let alice = store
            .add_user("alice", None, "phc", spoolframe_types::Role::Consumer)
            .await
            .unwrap();
        let sync = Arc::new(WatchSync::new(
            Arc::default(),
            accounts_at(&dir, store),
            STARTED,
            STARTED,
        ));
        let (_tx, changes) = tokio::sync::broadcast::channel(4);
        sync.follow(alice, changes.resubscribe());
        assert!(sync.tasks.lock().contains_key(&alice));
        sync.reconcile(&changes).await;
        assert!(!sync.tasks.lock().contains_key(&alice));
    }

    /// The metrics count runs (P3-14): one that cannot sync is a failure,
    /// and a dry run is no run.
    #[tokio::test]
    async fn runs_are_counted_and_dry_runs_are_not() {
        let dir = spoolframe_test_dir::TestDir::new("spoolframe-sync-runs").await;
        let store = crate::store::Store::open(dir.as_ref()).await.unwrap();
        let alice = store
            .add_user("alice", None, "phc", spoolframe_types::Role::Consumer)
            .await
            .unwrap();
        let accounts = Arc::new(PlexAccounts::new(
            crate::config::secrets::Secrets::new(dir.as_ref()),
            store,
            crate::plex_accounts::PlexSite {
                api: "http://127.0.0.1:9".parse().unwrap(),
                app: "http://127.0.0.1:9".parse().unwrap(),
            },
            None,
        ));
        let sync = WatchSync::new(Arc::default(), accounts, STARTED, STARTED);
        assert!(matches!(
            sync.run(alice, true).await,
            Err(SyncError::NotLinked)
        ));
        assert_eq!(sync.run_counts(), (0, 0));
        assert!(sync.run(alice, false).await.is_err());
        assert_eq!(sync.run_counts(), (0, 1));
    }

    /// Each run is timed by its outcome; a dry run is not.
    #[tokio::test]
    async fn runs_are_timed_and_dry_runs_are_not() {
        let dir = spoolframe_test_dir::TestDir::new("spoolframe-sync-timed").await;
        let store = crate::store::Store::open(dir.as_ref()).await.unwrap();
        let alice = store
            .add_user("alice", None, "phc", spoolframe_types::Role::Consumer)
            .await
            .unwrap();
        let sync = WatchSync::new(Arc::default(), accounts_at(&dir, store), STARTED, STARTED);
        let histograms = Arc::new(crate::metrics::Histograms::default());
        sync.record_into(histograms.clone());
        let _ = sync.run(alice, true).await;
        let _ = sync.run(alice, false).await;
        let family = histograms.family(&crate::metrics::histogram::PLEX_SYNC_DURATION);
        let counts: Vec<_> = family
            .samples
            .iter()
            .map(|s| match &s.value {
                crate::metrics::Value::Histogram(h) => (s.labels.clone(), h.count),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(counts, [(vec![("outcome", "failed".to_owned())], 1)]);
    }

    /// A run's span says whether it was a dry run, never whose it was:
    /// spans are exported, and no span names a user (P3-14).
    #[tokio::test]
    async fn a_run_s_span_does_not_name_its_user() {
        use tracing_subscriber::layer::SubscriberExt as _;

        /// Each span opened: its name and its fields' names.
        type Opened = Vec<(&'static str, Vec<&'static str>)>;

        #[derive(Clone, Default)]
        struct Fields(Arc<Mutex<Opened>>);

        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Fields {
            fn on_new_span(
                &self,
                attrs: &tracing::span::Attributes<'_>,
                _: &tracing::span::Id,
                _: tracing_subscriber::layer::Context<'_, S>,
            ) {
                let names = attrs.fields().iter().map(|f| f.name()).collect();
                self.0.lock().push((attrs.metadata().name(), names));
            }
        }

        let dir = spoolframe_test_dir::TestDir::new("spoolframe-sync-span").await;
        let store = crate::store::Store::open(dir.as_ref()).await.unwrap();
        let alice = store
            .add_user("alice", None, "phc", spoolframe_types::Role::Consumer)
            .await
            .unwrap();
        let sync = WatchSync::new(Arc::default(), accounts_at(&dir, store), STARTED, STARTED);
        let fields = Fields::default();
        let subscriber = tracing_subscriber::registry().with(fields.clone());
        let _guard = tracing::subscriber::set_default(subscriber);
        let _ = sync.run(alice, false).await;
        let spans = fields.0.lock().clone();
        assert!(
            spans
                .iter()
                .any(|(name, fields)| *name == "plex sync run" && fields == &["dry_run"]),
            "{spans:?}"
        );
    }

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
            section: "1".into(),
        }
    }

    /// PX-04's plan: both ways, for everything.
    fn both(rows: &[WatchRow], plex: &[PlexWatch], started: Duration) -> Vec<Action> {
        plan(rows, plex, started, |_| SyncDirection::Both).actions
    }

    #[test]
    fn the_same_state_on_both_sides_does_nothing() {
        let e = episode("1");
        let m = id(MediaKind::Movie, "2");
        let planned = plan(
            &[row(&e, true, None, 10), row(&m, false, Some(600_000), 10)],
            &[plex(&e, true, None, 50), plex(&m, false, Some(610_000), 50)],
            STARTED,
            |_| SyncDirection::Both,
        );
        assert!(planned.actions.is_empty());
        assert_eq!(planned.skipped, 2);
    }

    #[test]
    fn what_only_plex_knows_is_pulled() {
        let banshee = episode("229060");
        assert_eq!(
            both(&[], &[plex(&banshee, true, None, 100)], STARTED),
            [Action::Pull { id: banshee }]
        );
    }

    #[test]
    fn a_newer_plex_state_is_pulled_and_a_newer_spoolframe_one_pushed() {
        let e = episode("1");
        assert_eq!(
            both(
                &[row(&e, false, Some(600_000), 10)],
                &[plex(&e, true, None, 20)],
                STARTED
            ),
            [Action::Pull { id: e.clone() }]
        );
        assert_eq!(
            both(
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
            both(
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
        assert!(both(&[row(&e, false, Some(2_822), 100)], &[], STARTED).is_empty());
        // Nor does it hide what Plex has.
        assert_eq!(
            both(
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
            both(
                &[row(&e, false, None, 30)],
                &[plex(&e, true, None, 20)],
                STARTED
            ),
            [Action::PushUnwatched { id: e.clone() }]
        );
        // Unwatched here with nothing in Plex: nothing to say.
        assert!(both(&[row(&e, false, None, 30)], &[], STARTED).is_empty());
    }

    #[test]
    fn a_pull_keeps_plex_s_time_so_the_next_plan_is_empty() {
        let e = episode("1");
        let there = plex(&e, false, Some(1_932_000), 1_790_906_395);
        assert_eq!(
            both(
                &[row(&e, true, None, 10)],
                std::slice::from_ref(&there),
                STARTED
            ),
            [Action::Pull { id: e.clone() }]
        );
        // What the pull writes: Plex's state, as of Plex's time.
        let pulled = row(&e, false, Some(1_932_000), 1_790_906_395);
        assert!(both(&[pulled], &[there], STARTED).is_empty());
    }

    #[test]
    fn a_rewatch_in_progress_is_the_same_on_both_sides() {
        let e = episode("1");
        // Plex: watched before, now 20 minutes in; Spoolframe: the same
        // position, not marked watched.
        assert!(
            both(
                &[row(&e, false, Some(1_200_000), 10)],
                &[plex(&e, true, Some(1_210_000), 50)],
                STARTED
            )
            .is_empty()
        );
    }

    /// Four items, each needing one thing: a pull of what only Plex has
    /// (1), a pull of a newer Plex state (2), a push of what only
    /// Spoolframe has (3), and a push of a newer Spoolframe state (4).
    fn four() -> (Vec<WatchRow>, Vec<PlexWatch>) {
        let (a, b, c, d) = (episode("1"), episode("2"), episode("3"), episode("4"));
        (
            vec![
                row(&b, false, Some(600_000), 10),
                row(&c, true, None, 10),
                row(&d, true, None, 30),
            ],
            vec![
                plex(&a, true, None, 10),
                plex(&b, true, None, 20),
                plex(&d, false, Some(600_000), 20),
            ],
        )
    }

    #[test]
    fn from_plex_only_pulls_and_never_writes_to_plex() {
        let (rows, theirs) = four();
        let planned = plan(&rows, &theirs, STARTED, |_| SyncDirection::FromPlex);
        assert_eq!(
            planned.actions,
            [
                Action::Pull { id: episode("1") },
                Action::Pull { id: episode("2") }
            ]
        );
        assert_eq!(planned.skipped, 2);
    }

    #[test]
    fn to_plex_only_pushes_and_never_writes_to_spoolframe() {
        let (rows, theirs) = four();
        let planned = plan(&rows, &theirs, STARTED, |_| SyncDirection::ToPlex);
        assert_eq!(
            planned.actions,
            [
                Action::PushWatched { id: episode("3") },
                Action::PushWatched { id: episode("4") }
            ]
        );
        assert_eq!(planned.skipped, 2);
    }

    #[test]
    fn off_does_neither_and_is_not_counted() {
        let (rows, theirs) = four();
        assert_eq!(
            plan(&rows, &theirs, STARTED, |_| SyncDirection::Off),
            Plan::default()
        );
    }

    #[test]
    fn both_does_all_four_and_each_library_goes_its_own_way() {
        let (rows, theirs) = four();
        assert_eq!(both(&rows, &theirs, STARTED).len(), 4);
        // Episodes 1 and 3 in a library synced from Plex, 2 and 4 in one
        // synced to it.
        let mixed = plan(&rows, &theirs, STARTED, |id| match id.key() {
            "1" | "3" => SyncDirection::FromPlex,
            _ => SyncDirection::ToPlex,
        });
        assert_eq!(
            mixed.actions,
            [
                Action::Pull { id: episode("1") },
                Action::PushWatched { id: episode("4") }
            ]
        );
    }

    #[test]
    fn only_rows_with_something_to_send_that_plex_does_not_list_are_looked_up() {
        let (mut rows, theirs) = four();
        // A few seconds of trying a file: nothing to send, nothing to ask.
        rows.push(row(&episode("5"), false, Some(2_000), 10));
        assert_eq!(only_here(&rows, &theirs, STARTED), [episode("3")]);
    }

    #[test]
    fn only_plex_movies_and_episodes_take_part() {
        let track = id(MediaKind::Track, "5");
        assert!(both(&[row(&track, true, None, 10)], &[], STARTED).is_empty());
    }

    const ME: Principal = Principal::LocalUser(crate::store::users::LocalUserId::from_uuid(
        uuid::Uuid::from_u128(1),
    ));
    const SOMEONE_ELSE: Principal = Principal::LocalUser(
        crate::store::users::LocalUserId::from_uuid(uuid::Uuid::from_u128(2)),
    );

    /// Run [`schedule`] every 2 minutes; the seconds at which it ran.
    fn scheduled() -> (
        tokio::sync::broadcast::Sender<Principal>,
        Arc<parking_lot::Mutex<Vec<u64>>>,
    ) {
        let changed = tokio::sync::broadcast::Sender::new(16);
        let changes = changed.subscribe();
        let runs = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let ran = runs.clone();
        let start = tokio::time::Instant::now();
        tokio::spawn(schedule(Duration::from_secs(120), changes, ME, move || {
            ran.lock().push(start.elapsed().as_secs());
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
            changed.send(ME).unwrap();
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
        assert_eq!(*runs.lock(), [0, 60, 120, 180, 240, 300]);
    }

    /// The minute is counted from the last run, not from the change, and
    /// the change still gets its 5 seconds to settle.
    #[tokio::test(start_paused = true)]
    async fn a_change_a_while_after_the_last_run_is_synced_within_seconds() {
        let (changed, runs) = scheduled();
        tokio::time::sleep(Duration::from_secs(70)).await;
        changed.send(ME).unwrap();
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert_eq!(*runs.lock(), [0, 75]);
    }

    /// The interval's run is due at 120 s. A change 3 s before it does not
    /// put it off to settle, and the intervals go on from that run.
    #[tokio::test(start_paused = true)]
    async fn a_change_just_before_the_interval_s_run_does_not_put_it_off() {
        let (changed, runs) = scheduled();
        tokio::time::sleep(Duration::from_secs(117)).await;
        changed.send(ME).unwrap();
        tokio::time::sleep(Duration::from_secs(133)).await;
        assert_eq!(*runs.lock(), [0, 120, 240]);
    }

    /// Changes while a run waits to settle are in that run: none brings
    /// another one a minute later.
    #[tokio::test(start_paused = true)]
    async fn changes_before_a_run_are_in_it() {
        let (changed, runs) = scheduled();
        tokio::time::sleep(Duration::from_secs(70)).await;
        for _ in 0..3 {
            changed.send(ME).unwrap();
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        tokio::time::sleep(Duration::from_secs(80)).await;
        assert_eq!(*runs.lock(), [0, 75]);
    }

    /// Another user's changes are theirs: they bring no run of mine
    /// forward, and one of mine among them is not lost.
    #[tokio::test(start_paused = true)]
    async fn another_user_s_changes_do_not_run_mine_nor_hide_mine() {
        let (changed, runs) = scheduled();
        tokio::time::sleep(Duration::from_secs(70)).await;
        changed.send(SOMEONE_ELSE).unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        changed.send(ME).unwrap();
        changed.send(SOMEONE_ELSE).unwrap();
        tokio::time::sleep(Duration::from_secs(9)).await;
        assert_eq!(*runs.lock(), [0, 76]);
        changed.send(SOMEONE_ELSE).unwrap();
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(*runs.lock(), [0, 76]);
    }
}
