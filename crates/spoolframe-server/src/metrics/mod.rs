//! The server's metrics (P3-14): what `GET /metrics` on the admin listener
//! answers a Prometheus scraper with.
//!
//! A small registry of our own rather than the `metrics` crate's global
//! recorder: every test in a process starts servers of its own, and a
//! global recorder would add up all of theirs. Each [`Metrics`] is one
//! server's, and its families are read from that server's parts when
//! asked for, so nothing is counted twice and nothing is kept that the
//! parts do not already know.

pub mod histogram;
pub mod history;
pub mod process;
mod prometheus;

use parking_lot::RwLock;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::Instant;

pub use histogram::Histograms;
pub use prometheus::{CONTENT_TYPE, render};

use crate::admin::AddressRanges;
use crate::artwork::ArtworkCache;
use crate::catalog::LiveCatalog;
use crate::encode::job::{Jobs, Load};
use crate::link_stats::{PathKind, Punch};
use crate::network::{Connections, Services};
use crate::playback::{Plan, Playbacks};
use crate::plex_sync::WatchSync;
use crate::previews::Previews;
use crate::store::Store;
use crate::store::users::LocalUserId;
use crate::subtitles::Subtitles;
use crate::telemetry::Signal;

/// A count that only goes up.
#[derive(Debug, Default)]
pub struct Counter(AtomicU64);

impl Counter {
    pub fn inc(&self) {
        self.add(1);
    }

    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// What a family's samples are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Only goes up (until the server restarts).
    Counter,
    /// Goes up and down.
    Gauge,
    Histogram,
}

/// One metric, by its Prometheus name, with all its labelled samples.
#[derive(Debug, Clone, PartialEq)]
pub struct Family {
    pub name: &'static str,
    pub help: &'static str,
    pub kind: Kind,
    pub samples: Vec<Sample>,
}

impl Family {
    /// A family with one sample and no labels.
    pub fn single(name: &'static str, help: &'static str, kind: Kind, value: f64) -> Self {
        Self {
            name,
            help,
            kind,
            samples: vec![Sample::labelled(&[], value)],
        }
    }

    /// The value of the sample with exactly `labels`, if it is a number.
    pub fn value(&self, labels: &[(&str, &str)]) -> Option<f64> {
        self.samples
            .iter()
            .find(|s| {
                s.labels.len() == labels.len()
                    && labels
                        .iter()
                        .all(|(k, v)| s.labels.iter().any(|(sk, sv)| sk == k && sv == v))
            })
            .and_then(|s| match s.value {
                Value::Number(v) => Some(v),
                Value::Histogram(_) => None,
            })
    }

    /// Every number sample added up.
    pub fn total(&self) -> f64 {
        self.samples
            .iter()
            .map(|s| match s.value {
                Value::Number(v) => v,
                Value::Histogram(_) => 0.0,
            })
            .sum()
    }
}

/// One series of a family.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub labels: Vec<(&'static str, String)>,
    pub value: Value,
}

impl Sample {
    pub fn labelled(labels: &[(&'static str, &str)], value: f64) -> Self {
        Self {
            labels: labels.iter().map(|(k, v)| (*k, (*v).to_owned())).collect(),
            value: Value::Number(value),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Number(f64),
    Histogram(HistogramValue),
}

/// A histogram as Prometheus has it: cumulative counts at or under each
/// upper bound (`le`), the `+Inf` bucket being `count`.
#[derive(Debug, Clone, PartialEq)]
pub struct HistogramValue {
    pub buckets: Vec<(f64, u64)>,
    pub count: u64,
    pub sum: f64,
}

/// Whether `GET /metrics` answers, and to whom (`admin.metrics`,
/// `admin.metrics_from`): live settings.
#[derive(Debug)]
pub struct MetricsAccess {
    enabled: AtomicBool,
    from: RwLock<AddressRanges>,
    /// Scrapes refused in the last hour, by address: when each was.
    refused:
        parking_lot::Mutex<std::collections::BTreeMap<IpAddr, std::collections::VecDeque<i64>>>,
}

/// An address refused `GET /metrics` in the last hour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub address: IpAddr,
    pub count: u64,
    /// Unix ms.
    pub last_at: i64,
}

impl Default for MetricsAccess {
    /// Off; from loopback once on.
    fn default() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            from: RwLock::new(AddressRanges::loopback()),
            refused: Default::default(),
        }
    }
}

impl MetricsAccess {
    pub fn set(&self, enabled: bool, from: AddressRanges) {
        *self.from.write() = from;
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// The most addresses kept; past it the one refused longest ago goes.
    const REFUSED_ADDRESSES: usize = 64;

    /// A scrape from `ip` was refused, at `at` (Unix ms).
    pub fn refused(&self, ip: IpAddr, at: i64) {
        let mut refused = self.refused.lock();
        refused.entry(ip.to_canonical()).or_default().push_back(at);
        Self::prune(&mut refused, at);
        while refused.len() > Self::REFUSED_ADDRESSES {
            let oldest = refused
                .iter()
                .min_by_key(|(_, times)| times.back().copied())
                .map(|(ip, _)| *ip);
            if let Some(ip) = oldest {
                refused.remove(&ip);
            }
        }
    }

    fn prune(
        refused: &mut std::collections::BTreeMap<IpAddr, std::collections::VecDeque<i64>>,
        now: i64,
    ) {
        let hour_ago = now - 3_600_000;
        for times in refused.values_mut() {
            while times.front().is_some_and(|&t| t < hour_ago) {
                times.pop_front();
            }
        }
        refused.retain(|_, times| !times.is_empty());
    }

    /// Who was refused in the hour before `now`, most often first.
    pub fn refused_last_hour(&self, now: i64) -> Vec<Refused> {
        let mut refused = self.refused.lock();
        Self::prune(&mut refused, now);
        let mut list: Vec<Refused> = refused
            .iter()
            .map(|(ip, times)| Refused {
                address: *ip,
                count: times.len() as u64,
                last_at: times.back().copied().unwrap_or_default(),
            })
            .collect();
        list.sort_by(|a, b| b.count.cmp(&a.count).then(b.last_at.cmp(&a.last_at)));
        list
    }

    /// Whether a scraper at `ip` may read them.
    pub fn allows(&self, ip: IpAddr) -> bool {
        self.from.read().contains(ip)
    }
}

/// The parts of a running server its metrics are read from. Each is set
/// once it exists; a part a server (or a test) does not have is left out.
#[derive(Default)]
struct Sources {
    connections: Option<Connections>,
    playbacks: Option<Arc<Playbacks>>,
    jobs: Option<Arc<Jobs>>,
    artwork: Option<Arc<ArtworkCache>>,
    previews: Option<Arc<Previews>>,
    subtitles: Option<Arc<Subtitles>>,
    plex_sync: Option<Arc<WatchSync>>,
    catalog: Option<Arc<LiveCatalog>>,
    auth: Option<crate::auth::Auth>,
}

/// One user's live sessions, by binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserSessions {
    pub user: LocalUserId,
    pub app: u64,
    pub web: u64,
}

/// One server's metrics.
pub struct Metrics {
    started: Instant,
    access: MetricsAccess,
    sources: RwLock<Sources>,
    /// Admin API sockets open now (the CLI's and the web UI's).
    web_connections: Arc<AtomicU64>,
    /// Live sessions per user, as the store had them when last asked:
    /// [`Metrics::refresh`] asks, every few seconds, so a scrape never
    /// waits on the database.
    sessions: RwLock<Vec<UserSessions>>,
    store: RwLock<Option<Store>>,
    /// The last hour, sampled.
    history: history::History,
    /// Range reads answered with bytes, and the bytes.
    pub media_reads: Counter,
    pub media_read_bytes: Counter,
    /// Subtitle files and segments served.
    pub subtitle_reads: Counter,
    /// `PlaybackExpired` and `PlaybackFailed` sent to clients.
    pub playback_expired: Counter,
    pub playback_failed: Counter,
    /// OTLP records dropped from a full queue, and exports that failed,
    /// by signal: traces, metrics, logs (P3-14).
    telemetry_dropped: [Counter; 3],
    telemetry_failed: [Counter; 3],
    /// Histogram values for the OTLP export, as they are observed.
    histograms: Arc<Histograms>,
    /// Whether the NAT maps ports per destination, as iroh's last net
    /// report said: [`NAT_UNKNOWN`] until one says.
    nat_mapping: AtomicU8,
    /// Client requests answered, by type and outcome.
    client_requests: parking_lot::Mutex<std::collections::BTreeMap<(String, String), u64>>,
    /// Whether the endpoint has a home relay: [`RELAY_NONE`] while it has
    /// no relays to have one of.
    home_relay: AtomicU8,
    /// Bytes client connections carried, by [`LINK_PATHS`], sent and
    /// received.
    link_bytes: [[Counter; 2]; 4],
    /// Connections that started on a relay, and those of them that went
    /// direct.
    hole_punches: [Counter; 2],
    /// Each library's items by kind, as Plex last counted them.
    library_items: RwLock<Vec<LibraryItems>>,
}

/// How often the libraries' items are counted: each count asks Plex.
pub const LIBRARY_ITEMS_EVERY: std::time::Duration = std::time::Duration::from_secs(300);

/// A library's items of one kind, as Plex counted them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LibraryItems {
    /// Its id: the Plex server's id in the config and the section's key,
    /// never its title, which its owner chose.
    library: String,
    kind: spoolframe_types::MediaKind,
    count: u64,
}

const RELAY_NONE: u8 = 0;
const RELAY_LOST: u8 = 1;
const RELAY_HOME: u8 = 2;

/// The paths a connection's bytes are counted by.
const LINK_PATHS: [PathKind; 4] = [
    PathKind::Lan,
    PathKind::Direct,
    PathKind::Relay,
    PathKind::Unknown,
];

const NAT_UNKNOWN: u8 = 0;
const NAT_SAME: u8 = 1;
const NAT_VARIES: u8 = 2;

impl std::fmt::Debug for Metrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Metrics")
            .field("access", &self.access)
            .finish_non_exhaustive()
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            access: MetricsAccess::default(),
            sources: RwLock::default(),
            web_connections: Arc::default(),
            sessions: RwLock::default(),
            store: RwLock::default(),
            history: history::History::default(),
            media_reads: Counter::default(),
            media_read_bytes: Counter::default(),
            subtitle_reads: Counter::default(),
            playback_expired: Counter::default(),
            playback_failed: Counter::default(),
            telemetry_dropped: Default::default(),
            telemetry_failed: Default::default(),
            histograms: Arc::default(),
            nat_mapping: AtomicU8::new(NAT_UNKNOWN),
            client_requests: parking_lot::Mutex::default(),
            home_relay: AtomicU8::new(RELAY_NONE),
            link_bytes: Default::default(),
            hole_punches: Default::default(),
            library_items: RwLock::default(),
        }
    }
}

/// An admin API socket, counted open until dropped.
#[derive(Debug)]
pub struct WebConnection(Arc<AtomicU64>);

impl Drop for WebConnection {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Metrics {
    pub fn access(&self) -> &MetricsAccess {
        &self.access
    }

    fn sources(&self) -> parking_lot::RwLockWriteGuard<'_, Sources> {
        self.sources.write()
    }

    /// Read the client protocol's parts, and its open connections.
    pub fn observe_services(&self, services: &Services, connections: &Connections) {
        services.playbacks.record_into(self.histograms.clone());
        services.jobs.record_into(self.histograms.clone());
        let mut sources = self.sources();
        sources.auth = Some(services.auth.clone());
        sources.connections = Some(connections.clone());
        sources.playbacks = Some(services.playbacks.clone());
        sources.jobs = Some(services.jobs.clone());
        sources.artwork = Some(services.artwork.clone());
        sources.previews = Some(services.previews.clone());
        sources.subtitles = Some(services.subtitles.clone());
        if let Some(sync) = &services.plex_sync {
            sync.record_into(self.histograms.clone());
            sources.plex_sync = Some(sync.clone());
        }
    }

    /// Read each Plex server's requests from the catalog, as its settings
    /// make it now, and have each one's time recorded as it ends.
    pub fn observe_catalog(&self, catalog: Arc<LiveCatalog>) {
        let histograms = self.histograms.clone();
        catalog.on_replace(move |composite| {
            for server in composite.servers() {
                let histograms = histograms.clone();
                let labels = [opentelemetry::KeyValue::new("server", server.id.clone())];
                server.catalog.observe_requests(Arc::new(move |took| {
                    histograms.record(
                        &histogram::PLEX_REQUEST_DURATION,
                        took.as_secs_f64(),
                        &labels,
                    );
                }));
            }
        });
        self.sources().catalog = Some(catalog);
    }

    /// Where histogram values go as they are observed: the OTLP export.
    pub fn histograms(&self) -> &Histograms {
        &self.histograms
    }

    /// Count live sessions from `store`, at each [`Metrics::refresh`].
    pub fn observe_store(&self, store: Store) {
        *self.store.write() = Some(store);
    }

    /// An admin API socket opened: counted until the guard is dropped.
    pub fn web_connection(&self) -> WebConnection {
        self.web_connections.fetch_add(1, Ordering::Relaxed);
        WebConnection(self.web_connections.clone())
    }

    /// Read what is too slow to read at a scrape: the live sessions.
    pub async fn refresh(&self) {
        let store = self.store.read().clone();
        let Some(store) = store else {
            return;
        };
        match store.live_session_counts().await {
            Ok(counts) => {
                *self.sessions.write() = counts
                    .into_iter()
                    .map(|(user, app, web)| UserSessions { user, app, web })
                    .collect();
            }
            Err(e) => tracing::warn!(error = %e, "cannot count the live sessions"),
        }
    }

    /// Live sessions per user, as last read.
    pub fn sessions(&self) -> Vec<UserSessions> {
        self.sessions.read().clone()
    }

    /// Refresh, and keep a reading in the history (P3-14): what the
    /// sampler does every [`history::SAMPLE_EVERY`].
    pub async fn sample(&self) {
        self.refresh().await;
        self.history
            .record(history::Reading::of(&self.families(), history::unix_ms()));
    }

    /// The history's points, oldest first, and the point for now.
    pub fn activity(&self) -> (Vec<spoolframe_admin_api::ActivityPoint>, history::Reading) {
        let now = history::Reading::of(&self.families(), history::unix_ms());
        (self.history.points(), now)
    }

    /// The point for `now`, its rates since the last sample.
    pub fn now_point(&self, now: &history::Reading) -> spoolframe_admin_api::ActivityPoint {
        self.history.now(now)
    }

    /// Each Plex server: its id, its name, and its requests.
    pub fn plex_servers(&self) -> Vec<(String, String, spoolframe_plex::RequestCounts)> {
        let sources = self.sources.read();
        let Some(composite) = sources.catalog.as_ref().and_then(|c| c.composite()) else {
            return Vec::new();
        };
        composite
            .servers()
            .iter()
            .map(|s| (s.id.clone(), s.name.clone(), s.catalog.request_counts()))
            .collect()
    }

    /// Count each library's items, of each kind it has, from its Plex
    /// server. A library whose count fails keeps its last one; one no
    /// longer served goes.
    pub async fn count_library_items(&self) {
        use spoolframe_catalog::{Catalog as _, Inspect as _};
        let composite = self
            .sources
            .read()
            .catalog
            .as_ref()
            .and_then(|c| c.composite());
        let Some(composite) = composite else {
            return;
        };
        let libraries = match composite.libraries().await {
            Ok(libraries) => libraries,
            Err(e) => {
                tracing::debug!(error = %e, "cannot list the libraries to count their items");
                return;
            }
        };
        let last = self.library_items.read().clone();
        let mut now = Vec::new();
        for library in libraries {
            let id = library.id.to_string();
            for &kind in crate::admin::library_kinds(library.kind) {
                match composite.count(&library.id, kind).await {
                    Ok(count) => now.push(LibraryItems {
                        library: id.clone(),
                        kind,
                        count: u64::from(count),
                    }),
                    Err(e) => {
                        tracing::debug!(library = %id, error = %e, "cannot count a library's items");
                        now.extend(
                            last.iter()
                                .filter(|l| l.library == id && l.kind == kind)
                                .cloned(),
                        );
                    }
                }
            }
        }
        *self.library_items.write() = now;
    }

    /// [`Metrics::sample`] every [`history::SAMPLE_EVERY`], and
    /// [`Metrics::count_library_items`] every [`LIBRARY_ITEMS_EVERY`], for
    /// as long as the metrics are held anywhere else. Apart: a Plex that
    /// is slow to count holds up no sample.
    pub fn spawn_sampler(self: &Arc<Self>) {
        let counting = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(LIBRARY_ITEMS_EVERY);
            loop {
                tick.tick().await;
                let Some(metrics) = counting.upgrade() else {
                    return;
                };
                metrics.count_library_items().await;
            }
        });
        let metrics = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(history::SAMPLE_EVERY);
            loop {
                tick.tick().await;
                let Some(metrics) = metrics.upgrade() else {
                    return;
                };
                metrics.sample().await;
            }
        });
    }

    /// What iroh's net report says of the NAT: whether the public address
    /// it maps this server to varies by destination; `None` when it cannot
    /// tell. Logged once each time it changes, where iroh warns at every
    /// report.
    pub fn nat_mapping_varies(&self, varies: Option<bool>) {
        let Some(varies) = varies else {
            return;
        };
        let now = if varies { NAT_VARIES } else { NAT_SAME };
        if self.nat_mapping.swap(now, Ordering::Relaxed) == now {
            return;
        }
        if varies {
            tracing::info!(
                "the NAT maps this server to a different public port for each destination: \
                 hole punching is harder, so more devices may go through a relay"
            );
        } else {
            tracing::info!("the NAT maps this server to one public port for every destination");
        }
    }

    /// A client request of type `request` answered with `outcome` (`ok`,
    /// or the error's type on the wire, as its span says) in `took`. The
    /// type is one this build knows, or `unknown`: the client chooses it.
    pub fn client_request(&self, request: &str, outcome: &str, took: std::time::Duration) {
        *self
            .client_requests
            .lock()
            .entry((request.to_owned(), outcome.to_owned()))
            .or_default() += 1;
        self.histograms.observe(
            &histogram::CLIENT_REQUEST_DURATION,
            took.as_secs_f64(),
            &[("request", request)],
        );
    }

    fn requests(&self, families: &mut Vec<Family>) {
        families.push(Family {
            name: "spoolframe_client_requests_total",
            help: "Client requests answered, by type (`unknown` for one this build does not \
                   know) and outcome: `ok`, or the error's type on the wire.",
            kind: Kind::Counter,
            samples: self
                .client_requests
                .lock()
                .iter()
                .map(|((request, outcome), n)| {
                    Sample::labelled(&[("request", request), ("outcome", outcome)], *n as f64)
                })
                .collect(),
        });
    }

    fn refusals(&self, families: &mut Vec<Family>) {
        let sources = self.sources.read();
        let counts = sources
            .auth
            .as_ref()
            .map(crate::auth::Auth::refusal_counts)
            .unwrap_or_default();
        families.push(Family {
            name: "spoolframe_requests_refused_total",
            help: "Requests refused, as the spoolframe::refused log lines say, those left out \
                   of the log too: by kind (sign_in_failed, unauthenticated, no_session, \
                   forbidden, rate_limited, recent_sign_in_required, profile_locked, \
                   hello_refused) and channel (quic, the apps'; admin_api, the web UI's and \
                   CLI's socket; web, the admin listener's HTTP routes).",
            kind: Kind::Counter,
            samples: counts
                .into_iter()
                .map(|(kind, channel, n)| {
                    Sample::labelled(
                        &[("kind", kind.as_str()), ("channel", channel.as_str())],
                        n as f64,
                    )
                })
                .collect(),
        });
    }

    /// The histograms kept as they are observed ([`histogram::KEPT`]).
    fn observed(&self, families: &mut Vec<Family>) {
        for spec in histogram::KEPT {
            families.push(self.histograms.family(spec));
        }
    }

    /// Whether the endpoint is connected to a home relay now.
    pub fn home_relay(&self, connected: bool) {
        let now = if connected { RELAY_HOME } else { RELAY_LOST };
        self.home_relay.store(now, Ordering::Relaxed);
    }

    /// A client connection carried `sent` and `received` bytes more, on a
    /// path of kind `path`.
    pub fn link_bytes(&self, path: PathKind, sent: u64, received: u64) {
        let at = LINK_PATHS
            .iter()
            .position(|&p| p == path)
            .expect("in LINK_PATHS");
        self.link_bytes[at][0].add(sent);
        self.link_bytes[at][1].add(received);
    }

    /// A client connection's round-trip time on the path it goes by, of
    /// kind `path`.
    pub fn link_rtt(&self, path: PathKind, rtt: std::time::Duration) {
        self.histograms.observe(
            &histogram::CONNECTION_RTT,
            rtt.as_secs_f64(),
            &[("path", path.as_str())],
        );
    }

    /// A connection started on a relay, or went direct from one.
    pub fn hole_punch(&self, punch: Punch) {
        match punch {
            Punch::Attempt => self.hole_punches[0].inc(),
            Punch::Success => self.hole_punches[1].inc(),
        }
    }

    fn network(&self, families: &mut Vec<Family>) {
        families.push(Family {
            name: "spoolframe_home_relay_connected",
            help: "1 while the server is connected to a home relay from network.relays, 0 \
                   while it is not: devices that cannot dial in cannot connect. No sample on a \
                   server with no relays.",
            kind: Kind::Gauge,
            samples: match self.home_relay.load(Ordering::Relaxed) {
                RELAY_NONE => Vec::new(),
                now => vec![Sample::labelled(
                    &[],
                    f64::from(u8::from(now == RELAY_HOME)),
                )],
            },
        });
        families.push(Family {
            name: "spoolframe_connection_bytes_total",
            help: "UDP bytes client connections carried, by the kind of path they went by \
                   (lan, direct, relay; unknown before one is selected) and direction (sent, \
                   received). Read from each connection every 10 s and as it closes, so bytes \
                   go to the path it was on at each reading.",
            kind: Kind::Counter,
            samples: LINK_PATHS
                .iter()
                .zip(&self.link_bytes)
                .flat_map(|(path, [sent, received])| {
                    [("sent", sent), ("received", received)].map(|(direction, n)| {
                        Sample::labelled(
                            &[("path", path.as_str()), ("direction", direction)],
                            n.get() as f64,
                        )
                    })
                })
                .collect(),
        });
        families.push(Family::single(
            "spoolframe_hole_punch_attempts_total",
            "Client connections whose first path was a relay: each one iroh tries to take \
             direct by punching a hole through NAT.",
            Kind::Counter,
            self.hole_punches[0].get() as f64,
        ));
        families.push(Family::single(
            "spoolframe_hole_punch_successes_total",
            "Client connections that went from a relay to a direct (or LAN) path.",
            Kind::Counter,
            self.hole_punches[1].get() as f64,
        ));
    }

    /// A span or log record the OTLP queue had no room for.
    pub fn telemetry_dropped(&self, signal: Signal) {
        self.telemetry_dropped[signal as usize].inc();
    }

    /// An OTLP export that failed.
    pub fn telemetry_failed(&self, signal: Signal) {
        self.telemetry_failed[signal as usize].inc();
    }

    /// Dropped records of `signal` so far.
    pub fn telemetry_dropped_count(&self, signal: Signal) -> u64 {
        self.telemetry_dropped[signal as usize].get()
    }

    fn telemetry(&self, families: &mut Vec<Family>) {
        let by_signal = |counters: &[Counter; 3]| {
            Signal::ALL
                .iter()
                .map(|&s| {
                    Sample::labelled(&[("signal", s.as_str())], counters[s as usize].get() as f64)
                })
                .collect()
        };
        families.push(Family {
            name: "spoolframe_telemetry_dropped_total",
            help: "Spans and log records dropped because the OTLP queue was full.",
            kind: Kind::Counter,
            samples: by_signal(&self.telemetry_dropped),
        });
        families.push(Family {
            name: "spoolframe_telemetry_export_failures_total",
            help: "OTLP exports that failed or timed out.",
            kind: Kind::Counter,
            samples: by_signal(&self.telemetry_failed),
        });
    }

    /// Every family, as it is now.
    pub fn families(&self) -> Vec<Family> {
        let mut families = Vec::new();
        self.connections(&mut families);
        self.network(&mut families);
        self.requests(&mut families);
        self.refusals(&mut families);
        self.playbacks(&mut families);
        self.conversions(&mut families);
        self.reads(&mut families);
        self.plex(&mut families);
        self.background(&mut families);
        self.telemetry(&mut families);
        self.process(&mut families);
        self.observed(&mut families);
        families
    }

    fn connections(&self, families: &mut Vec<Family>) {
        let sources = self.sources.read();
        let app = sources
            .connections
            .as_ref()
            .map_or(0, |c| c.snapshot().len());
        let (direct, relayed) = sources
            .connections
            .as_ref()
            .map_or((0, 0), Connections::paths);
        families.push(Family {
            name: "spoolframe_connections_open",
            help: "Open connections: app (clients over Iroh) and web (admin API sockets).",
            kind: Kind::Gauge,
            samples: vec![
                Sample::labelled(&[("kind", "app")], app as f64),
                Sample::labelled(
                    &[("kind", "web")],
                    self.web_connections.load(Ordering::Relaxed) as f64,
                ),
            ],
        });
        families.push(Family {
            name: "spoolframe_connection_paths",
            help: "Open app connections by the way they go now: direct to the device, or through a relay.",
            kind: Kind::Gauge,
            samples: vec![
                Sample::labelled(&[("path", "direct")], direct as f64),
                Sample::labelled(&[("path", "relay")], relayed as f64),
            ],
        });
        families.push(Family::single(
            "spoolframe_nat_mapping_varies",
            "1 when the NAT maps this server to a different public port for each destination \
             (iroh's net report), which makes hole punching harder; 0 when it does not, or is \
             not known yet.",
            Kind::Gauge,
            f64::from(u8::from(
                self.nat_mapping.load(Ordering::Relaxed) == NAT_VARIES,
            )),
        ));
        // Every user's together: a label per user would add series with
        // every user. The activity page has them by user.
        let (app, web) = self
            .sessions()
            .iter()
            .fold((0, 0), |(app, web), s| (app + s.app, web + s.web));
        families.push(Family {
            name: "spoolframe_sessions",
            help: "Live sessions: app (devices') and web (the admin API's).",
            kind: Kind::Gauge,
            samples: vec![
                Sample::labelled(&[("kind", "app")], app as f64),
                Sample::labelled(&[("kind", "web")], web as f64),
            ],
        });
    }

    fn playbacks(&self, families: &mut Vec<Family>) {
        let sources = self.sources.read();
        let counts = sources
            .playbacks
            .as_ref()
            .map(|p| p.counts())
            .unwrap_or_default();
        families.push(Family {
            name: "spoolframe_playbacks_active",
            help: "Open playbacks by plan: direct (the file), remux (copied into another \
                   container) or transcode (encoded).",
            kind: Kind::Gauge,
            samples: Plan::ALL
                .iter()
                .map(|&plan| {
                    Sample::labelled(&[("plan", plan.as_str())], counts.active(plan) as f64)
                })
                .collect(),
        });
        families.push(Family::single(
            "spoolframe_playbacks_started_total",
            "Playbacks started.",
            Kind::Counter,
            counts.started as f64,
        ));
        families.push(Family::single(
            "spoolframe_playbacks_ended_total",
            "Playbacks ended: stopped, idle, their device gone, or replaced.",
            Kind::Counter,
            counts.ended as f64,
        ));
        families.push(Family {
            name: "spoolframe_playback_errors_total",
            help: "PlaybackExpired and PlaybackFailed sent to clients.",
            kind: Kind::Counter,
            samples: vec![
                Sample::labelled(&[("error", "expired")], self.playback_expired.get() as f64),
                Sample::labelled(&[("error", "failed")], self.playback_failed.get() as f64),
            ],
        });
    }

    fn conversions(&self, families: &mut Vec<Family>) {
        let sources = self.sources.read();
        let counts = sources
            .jobs
            .as_ref()
            .map(|j| j.counts())
            .unwrap_or_default();
        families.push(Family {
            name: "spoolframe_conversions_running",
            help: "Encodes whose ffmpeg runs, paused or not, by kind: remux, audio (only the \
                   audio encoded), gpu or software (the video encoded), gpu_only (the video \
                   decoded and encoded on the GPU, nothing on the CPU).",
            kind: Kind::Gauge,
            samples: Load::ALL
                .iter()
                .zip(counts.running_by_load)
                .map(|(load, n)| Sample::labelled(&[("kind", load.as_str())], n as f64))
                .collect(),
        });
        let budget = sources
            .jobs
            .as_ref()
            .map(|j| j.budget())
            .unwrap_or_default();
        families.push(Family::single(
            "spoolframe_encode_budget_used",
            "The encode budget the conversions running take, in software video encodes: a \
             GPU video encode is 0.5, one all on the GPU or an audio-only one 0.25, a remux 0.",
            Kind::Gauge,
            budget.used.as_jobs(),
        ));
        families.push(Family::single(
            "spoolframe_encode_budget_max",
            "The encode budget, transcode.max_jobs, in software video encodes.",
            Kind::Gauge,
            budget.max.as_jobs(),
        ));
        families.push(Family::single(
            "spoolframe_conversions_paused",
            "Encodes held back because they are far ahead of their reader.",
            Kind::Gauge,
            counts.paused as f64,
        ));
        families.push(Family::single(
            "spoolframe_conversions_refused_total",
            "Conversions refused because they did not fit the encode budget (transcode.max_jobs) \
             or their cap (max_remux, max_gpu_jobs): there is no queue.",
            Kind::Counter,
            counts.refused as f64,
        ));
        families.push(Family::single(
            "spoolframe_conversions_stalled_total",
            "Encodes ended because ffmpeg made no progress.",
            Kind::Counter,
            counts.stalled as f64,
        ));
        families.push(Family {
            name: "spoolframe_conversion_restarts_total",
            help: "ffmpeg runs started to replace a playback's running one, by why: seek, \
                   switch (to another rung), restart (at a segment far from where the run \
                   was) or change (of subtitle or quality).",
            kind: Kind::Counter,
            samples: crate::encode::job::RESTART_REASONS
                .iter()
                .zip(counts.restarts)
                .map(|(reason, n)| Sample::labelled(&[("reason", reason)], n as f64))
                .collect(),
        });
        families.push(Family::single(
            "spoolframe_conversion_output_seconds_total",
            "Seconds of media encoded; its rate is seconds of output per second.",
            Kind::Counter,
            counts.output_seconds,
        ));
    }

    fn reads(&self, families: &mut Vec<Family>) {
        let sources = self.sources.read();
        families.push(Family::single(
            "spoolframe_media_reads_total",
            "MEDIA range reads served.",
            Kind::Counter,
            self.media_reads.get() as f64,
        ));
        families.push(Family::single(
            "spoolframe_media_read_bytes_total",
            "Bytes sent by MEDIA range reads.",
            Kind::Counter,
            self.media_read_bytes.get() as f64,
        ));
        let artwork = sources
            .artwork
            .as_ref()
            .map(|a| a.counts())
            .unwrap_or_default();
        families.push(Family::single(
            "spoolframe_artwork_requests_total",
            "Artwork asked for, by clients and the web UI.",
            Kind::Counter,
            artwork.requests as f64,
        ));
        families.push(Family::single(
            "spoolframe_artwork_cache_hits_total",
            "Artwork found in the cache.",
            Kind::Counter,
            artwork.hits as f64,
        ));
        families.push(Family::single(
            "spoolframe_subtitle_reads_total",
            "Subtitle files and segments served.",
            Kind::Counter,
            self.subtitle_reads.get() as f64,
        ));
    }

    fn plex(&self, families: &mut Vec<Family>) {
        let sources = self.sources.read();
        let mut requests = Vec::new();
        let mut errors = Vec::new();
        let mut latency = Vec::new();
        if let Some(composite) = sources.catalog.as_ref().and_then(|c| c.composite()) {
            for server in composite.servers() {
                let counts = server.catalog.request_counts();
                let labels = [("server", server.id.as_str())];
                requests.push(Sample::labelled(&labels, counts.requests as f64));
                errors.push(Sample::labelled(&labels, counts.errors as f64));
                latency.push(Sample {
                    labels: vec![("server", server.id.clone())],
                    value: Value::Histogram(HistogramValue {
                        buckets: counts.buckets,
                        count: counts.requests,
                        sum: counts.sum_seconds,
                    }),
                });
            }
        }
        families.push(Family {
            name: "spoolframe_plex_requests_total",
            help: "Requests to each Plex server, by its id. Counted from when its settings \
                   last changed.",
            kind: Kind::Counter,
            samples: requests,
        });
        families.push(Family {
            name: "spoolframe_plex_errors_total",
            help: "Requests to each Plex server that failed (a 404 is an answer).",
            kind: Kind::Counter,
            samples: errors,
        });
        families.push(Family {
            name: histogram::PLEX_REQUEST_DURATION.name,
            help: histogram::PLEX_REQUEST_DURATION.help,
            kind: Kind::Histogram,
            samples: latency,
        });
        families.push(Family {
            name: "spoolframe_library_items",
            help: "Each library's items by kind (movie; show, season, episode; artist, album, \
                   track), as its Plex server counted them, every 5 minutes. `library` is its \
                   id: the Plex server's id and the section's key, never its name.",
            kind: Kind::Gauge,
            samples: self
                .library_items
                .read()
                .iter()
                .map(|l| {
                    Sample::labelled(
                        &[("library", &l.library), ("kind", media_kind(l.kind))],
                        l.count as f64,
                    )
                })
                .collect(),
        });
        let (ok, failed) = sources
            .plex_sync
            .as_ref()
            .map_or((0, 0), |s| s.run_counts());
        families.push(Family {
            name: "spoolframe_plex_sync_runs_total",
            help: "Watch state sync runs with linked Plex accounts.",
            kind: Kind::Counter,
            samples: vec![
                Sample::labelled(&[("outcome", "ok")], ok as f64),
                Sample::labelled(&[("outcome", "failed")], failed as f64),
            ],
        });
    }

    fn background(&self, families: &mut Vec<Family>) {
        let sources = self.sources.read();
        let previews = sources
            .previews
            .as_ref()
            .map(|p| p.counts())
            .unwrap_or_default();
        families.push(Family {
            name: "spoolframe_previews_queued",
            help: "Frame previews waiting to be made, and being made.",
            kind: Kind::Gauge,
            samples: vec![
                Sample::labelled(&[("state", "waiting")], previews.queued as f64),
                Sample::labelled(&[("state", "making")], previews.making as f64),
            ],
        });
        families.push(Family {
            name: "spoolframe_previews_total",
            help: "Frame preview sets made, and ones that could not be.",
            kind: Kind::Counter,
            samples: vec![
                Sample::labelled(&[("outcome", "done")], previews.done as f64),
                Sample::labelled(&[("outcome", "failed")], previews.failed as f64),
            ],
        });
        let ocr = sources
            .subtitles
            .as_ref()
            .map(|s| s.ocr_counts())
            .unwrap_or_default();
        families.push(Family::single(
            "spoolframe_ocr_queued",
            "Bitmap subtitle tracks waiting to be read as text, or being read.",
            Kind::Gauge,
            ocr.queued as f64,
        ));
        families.push(Family {
            name: "spoolframe_ocr_total",
            help: "Bitmap subtitle tracks read as text, and ones that could not be.",
            kind: Kind::Counter,
            samples: vec![
                Sample::labelled(&[("outcome", "done")], ocr.done as f64),
                Sample::labelled(&[("outcome", "failed")], ocr.failed as f64),
            ],
        });
    }

    fn process(&self, families: &mut Vec<Family>) {
        families.push(Family::single(
            "spoolframe_process_uptime_seconds",
            "Seconds since the server started.",
            Kind::Gauge,
            self.started.elapsed().as_secs_f64(),
        ));
        if let Some(bytes) = process::resident_bytes() {
            families.push(Family::single(
                "spoolframe_process_resident_memory_bytes",
                "The server's resident memory.",
                Kind::Gauge,
                bytes as f64,
            ));
        }
        if let Some(files) = process::open_files() {
            families.push(Family::single(
                "spoolframe_process_open_files",
                "Files, sockets and pipes the server has open.",
                Kind::Gauge,
                files as f64,
            ));
        }
    }
}

/// A kind of item as the metrics name it.
fn media_kind(kind: spoolframe_types::MediaKind) -> &'static str {
    use spoolframe_types::MediaKind;
    match kind {
        MediaKind::Movie => "movie",
        MediaKind::Show => "show",
        MediaKind::Season => "season",
        MediaKind::Episode => "episode",
        MediaKind::Artist => "artist",
        MediaKind::Album => "album",
        MediaKind::Track => "track",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_are_off_and_for_loopback_until_set() {
        let access = MetricsAccess::default();
        assert!(!access.enabled());
        assert!(access.allows("127.0.0.1".parse().unwrap()));
        assert!(access.allows("::1".parse().unwrap()));
        assert!(!access.allows("192.0.2.7".parse().unwrap()));
        access.set(true, AddressRanges::new(&["10.0.0.0/8"]).unwrap());
        assert!(access.enabled());
        assert!(access.allows("10.1.2.3".parse().unwrap()));
        assert!(!access.allows("127.0.0.1".parse().unwrap()));
    }

    #[test]
    fn refused_scrapes_are_counted_by_address_for_an_hour() {
        let access = MetricsAccess::default();
        let a: IpAddr = "192.0.2.7".parse().unwrap();
        let b: IpAddr = "198.51.100.1".parse().unwrap();
        access.refused(a, 1_000);
        access.refused(a, 2_000);
        access.refused(b, 3_000);
        let first = |at| access.refused_last_hour(at).first().cloned().unwrap();
        assert_eq!(
            first(3_600_500),
            Refused {
                address: a,
                count: 2,
                last_at: 2_000
            }
        );
        // An hour on, a's first is gone: one each, the latest first.
        let later = access.refused_last_hour(3_601_500);
        let seen: Vec<_> = later.iter().map(|r| (r.address, r.count)).collect();
        assert_eq!(seen, [(b, 1), (a, 1)]);
        assert!(access.refused_last_hour(3_603_001).is_empty());
    }

    #[test]
    fn the_process_is_measured() {
        let families = Metrics::default().families();
        let names: Vec<_> = families.iter().map(|f| f.name).collect();
        assert!(
            names.contains(&"spoolframe_process_uptime_seconds"),
            "{names:?}"
        );
        #[cfg(target_os = "linux")]
        for name in [
            "spoolframe_process_resident_memory_bytes",
            "spoolframe_process_open_files",
        ] {
            assert!(names.contains(&name), "{names:?}");
        }
    }

    #[test]
    fn the_nat_gauge_flips_and_each_change_is_logged_once() {
        use crate::logs::{LogRing, RingLayer};
        use tracing_subscriber::layer::SubscriberExt as _;
        let metrics = Metrics::default();
        let gauge = || {
            metrics
                .families()
                .into_iter()
                .find(|f| f.name == "spoolframe_nat_mapping_varies")
                .and_then(|f| f.value(&[]))
        };
        let ring = Arc::new(LogRing::new(10));
        let subscriber = tracing_subscriber::registry().with(RingLayer(ring.clone()));
        tracing::subscriber::with_default(subscriber, || {
            assert_eq!(gauge(), Some(0.0));
            // A report every 20 s, the same each time.
            for _ in 0..3 {
                metrics.nat_mapping_varies(Some(true));
            }
            assert_eq!(gauge(), Some(1.0));
            // A report that cannot tell changes nothing.
            metrics.nat_mapping_varies(None);
            assert_eq!(gauge(), Some(1.0));
            metrics.nat_mapping_varies(Some(false));
            metrics.nat_mapping_varies(Some(false));
            assert_eq!(gauge(), Some(0.0));
        });
        let lines = ring.page(&spoolframe_admin_api::LogQuery::default()).lines;
        let seen: Vec<_> = lines.iter().map(|l| (l.level, l.text.as_str())).collect();
        assert_eq!(
            seen,
            [
                (
                    spoolframe_admin_api::LogLevel::Info,
                    "the NAT maps this server to a different public port for each \
                     destination: hole punching is harder, so more devices may go through \
                     a relay"
                ),
                (
                    spoolframe_admin_api::LogLevel::Info,
                    "the NAT maps this server to one public port for every destination"
                ),
            ]
        );
    }

    /// Sessions are counted by kind, every user's together: a `user`
    /// label would add series with every user.
    #[test]
    fn sessions_are_by_kind_without_the_user() {
        let metrics = Metrics::default();
        *metrics.sessions.write() = vec![
            UserSessions {
                user: LocalUserId::generate(),
                app: 2,
                web: 1,
            },
            UserSessions {
                user: LocalUserId::generate(),
                app: 1,
                web: 0,
            },
        ];
        let sessions = metrics
            .families()
            .into_iter()
            .find(|f| f.name == "spoolframe_sessions")
            .unwrap();
        let samples: Vec<_> = sessions
            .samples
            .iter()
            .map(|s| (s.labels.clone(), s.value.clone()))
            .collect();
        assert_eq!(
            samples,
            [
                (vec![("kind", "app".to_owned())], Value::Number(3.0)),
                (vec![("kind", "web".to_owned())], Value::Number(1.0)),
            ]
        );
    }

    /// Each library's items are counted by kind from its Plex server, and
    /// labelled by its id, not its name.
    #[tokio::test]
    async fn each_library_s_items_are_counted_by_kind() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let pms = MockServer::start().await;
        let container = |body: serde_json::Value| {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "MediaContainer": body }))
        };
        Mock::given(method("GET"))
            .and(path("/library/sections"))
            .respond_with(container(serde_json::json!({ "Directory": [
                { "key": "2", "type": "movie", "title": "Dad's films" },
                { "key": "1", "type": "show", "title": "TV Shows" },
            ] })))
            .mount(&pms)
            .await;
        for (section, kind, total) in [("2", "1", 7), ("1", "2", 3), ("1", "3", 9), ("1", "4", 120)]
        {
            Mock::given(method("GET"))
                .and(path(format!("/library/sections/{section}/all")))
                .and(query_param("type", kind))
                .respond_with(container(
                    serde_json::json!({ "totalSize": total, "Metadata": [] }),
                ))
                .mount(&pms)
                .await;
        }
        let plex = spoolframe_plex::PlexCatalog::new(pms.uri().parse().unwrap(), None).unwrap();
        let metrics = Metrics::default();
        metrics.observe_catalog(Arc::new(LiveCatalog::new(Some(Arc::new(
            crate::catalog::Composite::single("Tower", plex),
        )))));
        let items = || {
            metrics
                .families()
                .into_iter()
                .find(|f| f.name == "spoolframe_library_items")
                .unwrap()
        };
        assert!(items().samples.is_empty());
        metrics.count_library_items().await;
        let items = items();
        assert_eq!(items.kind, Kind::Gauge);
        let mut seen: Vec<_> = items
            .samples
            .iter()
            .map(|s| (s.labels.clone(), s.value.clone()))
            .collect();
        seen.sort_by(|a, b| a.0.cmp(&b.0));
        let label = |library: &str, kind: &str| {
            vec![("library", library.to_owned()), ("kind", kind.to_owned())]
        };
        assert_eq!(
            seen,
            [
                (label("plex:1", "episode"), Value::Number(120.0)),
                (label("plex:1", "season"), Value::Number(9.0)),
                (label("plex:1", "show"), Value::Number(3.0)),
                (label("plex:2", "movie"), Value::Number(7.0)),
            ]
        );
    }

    /// Every histogram is declared, so the OTLP export has an instrument
    /// for it: one that is not would never be exported.
    #[test]
    fn every_histogram_is_declared_for_the_export() {
        for family in Metrics::default().families() {
            if family.kind == Kind::Histogram {
                let spec = histogram::ALL.iter().find(|s| s.name == family.name);
                assert!(spec.is_some_and(|s| s.help == family.help), "{family:?}");
            }
        }
    }

    /// The Plex request histogram in `/metrics` is as it was before the
    /// OTLP export recorded it on its own: names, labels and buckets.
    #[tokio::test]
    async fn the_plex_histogram_s_prometheus_text_is_unchanged() {
        let metrics = Metrics::default();
        let plex =
            spoolframe_plex::PlexCatalog::new("http://127.0.0.1:9".parse().unwrap(), None).unwrap();
        let live = Arc::new(LiveCatalog::new(Some(Arc::new(
            crate::catalog::Composite::single("Tower", plex),
        ))));
        metrics.observe_catalog(live.clone());
        let _ = live.plex().unwrap().server_name().await;
        let families: Vec<_> = metrics
            .families()
            .into_iter()
            .filter(|f| f.name == "spoolframe_plex_request_duration_seconds")
            .collect();
        let text = render(&families);
        let le = |text: &str| {
            text.lines()
                .filter_map(|l| l.split("le=\"").nth(1)?.split('"').next())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            le(&text),
            [
                "0.005", "0.01", "0.025", "0.05", "0.1", "0.25", "0.5", "1", "2.5", "5", "10",
                "+Inf"
            ],
            "{text}"
        );
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(
            lines[..2],
            [
                "# HELP spoolframe_plex_request_duration_seconds How long requests to each Plex \
                 server took.",
                "# TYPE spoolframe_plex_request_duration_seconds histogram",
            ]
        );
        assert!(
            lines.contains(
                &"spoolframe_plex_request_duration_seconds_bucket{server=\"plex\",le=\"+Inf\"} 1"
            ),
            "{text}"
        );
        assert!(
            lines.contains(&"spoolframe_plex_request_duration_seconds_count{server=\"plex\"} 1"),
            "{text}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l
                    .starts_with("spoolframe_plex_request_duration_seconds_sum{server=\"plex\"} ")),
            "{text}"
        );
    }
}
