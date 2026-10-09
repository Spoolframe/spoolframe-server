//! The server's metrics (P3-14): what `GET /metrics` on the admin listener
//! answers a Prometheus scraper with.
//!
//! A small registry of our own rather than the `metrics` crate's global
//! recorder: every test in a process starts servers of its own, and a
//! global recorder would add up all of theirs. Each [`Metrics`] is one
//! server's, and its families are read from that server's parts when
//! asked for, so nothing is counted twice and nothing is kept that the
//! parts do not already know.

pub mod history;
pub mod process;
mod prometheus;

use parking_lot::RwLock;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

pub use prometheus::{CONTENT_TYPE, render};

use crate::admin::AddressRanges;
use crate::artwork::ArtworkCache;
use crate::catalog::LiveCatalog;
use crate::encode::job::{Jobs, Load};
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
}

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
        let mut sources = self.sources();
        sources.connections = Some(connections.clone());
        sources.playbacks = Some(services.playbacks.clone());
        sources.jobs = Some(services.jobs.clone());
        sources.artwork = Some(services.artwork.clone());
        sources.previews = Some(services.previews.clone());
        sources.subtitles = Some(services.subtitles.clone());
        if let Some(sync) = &services.plex_sync {
            sources.plex_sync = Some(sync.clone());
        }
    }

    /// Read each Plex server's requests from the catalog, as its settings
    /// make it now.
    pub fn observe_catalog(&self, catalog: Arc<LiveCatalog>) {
        self.sources().catalog = Some(catalog);
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

    /// [`Metrics::sample`] every [`history::SAMPLE_EVERY`], for as long as
    /// the metrics are held anywhere else.
    pub fn spawn_sampler(self: &Arc<Self>) {
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
        self.playbacks(&mut families);
        self.conversions(&mut families);
        self.reads(&mut families);
        self.plex(&mut families);
        self.background(&mut families);
        self.telemetry(&mut families);
        self.process(&mut families);
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
        let mut samples = Vec::new();
        for s in self.sessions() {
            let user = format!("local:{}", s.user);
            samples.push(Sample::labelled(
                &[("user", &user), ("kind", "app")],
                s.app as f64,
            ));
            samples.push(Sample::labelled(
                &[("user", &user), ("kind", "web")],
                s.web as f64,
            ));
        }
        families.push(Family {
            name: "spoolframe_sessions",
            help: "Live sessions by user: app (a device's) and web (the admin API's).",
            kind: Kind::Gauge,
            samples,
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
            name: "spoolframe_plex_request_duration_seconds",
            help: "How long requests to each Plex server took.",
            kind: Kind::Histogram,
            samples: latency,
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
}
