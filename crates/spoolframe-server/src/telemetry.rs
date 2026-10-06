//! Opt-in OpenTelemetry export (P3-14): the server's spans, log records
//! and metrics, over OTLP to a collector the admin names
//! (`telemetry.otlp`), for example an OpenTelemetry Collector or Grafana
//! Alloy in front of the household's Grafana stack.
//!
//! Off unless an endpoint is set. A live setting: [`Telemetry::apply`]
//! builds the exporters and swaps the `tracing` layer that feeds them, and
//! stops the old ones in the background.
//!
//! An endpoint that is down never slows the server. Spans and log records
//! go into a bounded queue each, and are exported in batches by a task of
//! their own; when a queue is full the record is dropped and counted
//! (`spoolframe_telemetry_dropped_total`). Metrics are read from the
//! registry at each export, so nothing of theirs queues. A failed export
//! is counted and logged at WARN, at most once a minute per signal.
//!
//! What is exported names users only as `local:<uuid>`, and never a token
//! or a secret: the spans and logs are the ones the journal has, which
//! never carry them, and the headers set for the collector are never
//! logged.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use opentelemetry::metrics::MeterProvider as _;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::{InstrumentationScope, KeyValue};
use opentelemetry_otlp::{WithExportConfig as _, WithHttpConfig as _, WithTonicConfig as _};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::logs::{
    LogBatch, LogExporter as _, LogProcessor, SdkLogRecord, SdkLoggerProvider,
};
use opentelemetry_sdk::metrics::data::ResourceMetrics;
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
use opentelemetry_sdk::metrics::{SdkMeterProvider, Temporality};
use opentelemetry_sdk::trace::{
    SdkTracerProvider, Span, SpanData, SpanExporter as _, SpanProcessor,
};
use tokio::sync::{mpsc, oneshot};
use tracing::{Metadata, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{Layer, Registry};

use crate::metrics::{Family, Kind, Metrics, Value};
use crate::network::ServerName;

/// The layer the subscriber's reload slot holds while exporting.
pub type OtelLayer = Box<dyn Layer<Registry> + Send + Sync>;
/// Puts a layer in the subscriber's slot, or empties it.
pub type Swap = Box<dyn Fn(Option<OtelLayer>) + Send + Sync>;

/// What `service.name` says.
pub const SERVICE_NAME: &str = "spoolframe-server";

/// How OTLP reaches the collector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// OTLP over gRPC, usually on port 4317.
    Grpc,
    /// OTLP over HTTP with protobuf bodies, usually on port 4318:
    /// `<endpoint>/v1/traces`, `/v1/metrics` and `/v1/logs`.
    Http,
}

/// `telemetry.otlp`, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtlpSettings {
    pub endpoint: String,
    pub protocol: Protocol,
    /// Sent with every export: an API key, a tenant.
    pub headers: Vec<(String, String)>,
    pub traces: bool,
    pub metrics: bool,
    pub logs: bool,
}

/// One kind of telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Traces = 0,
    Metrics = 1,
    Logs = 2,
}

impl Signal {
    pub const ALL: [Signal; 3] = [Signal::Traces, Signal::Metrics, Signal::Logs];

    pub fn as_str(self) -> &'static str {
        match self {
            Signal::Traces => "traces",
            Signal::Metrics => "metrics",
            Signal::Logs => "logs",
        }
    }
}

/// How long and how much: fixed in the server, shortened in tests.
#[derive(Debug, Clone, Copy)]
pub struct Timings {
    /// Records each queue holds before new ones are dropped.
    pub queue: usize,
    /// The most records one export carries.
    pub batch: usize,
    /// How long records wait for a batch to fill.
    pub flush_every: Duration,
    /// How often metrics are read and exported.
    pub metrics_every: Duration,
    /// How long one export may take.
    pub export_timeout: Duration,
    /// The least time between two warnings of failed exports.
    pub warn_every: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            queue: 2048,
            batch: 512,
            flush_every: Duration::from_secs(5),
            metrics_every: Duration::from_secs(30),
            export_timeout: Duration::from_secs(10),
            warn_every: Duration::from_secs(60),
        }
    }
}

/// The export of one server, on or off.
pub struct Telemetry {
    swap: Swap,
    metrics: Arc<Metrics>,
    name: ServerName,
    endpoint_id: String,
    timings: Timings,
    running: Mutex<Option<Running>>,
}

impl std::fmt::Debug for Telemetry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Telemetry")
            .field("timings", &self.timings)
            .finish_non_exhaustive()
    }
}

/// How the export is going (P3-14), for the web UI's telemetry status.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExportStatus {
    /// When an export last went through, Unix ms.
    pub last_export_at: Option<i64>,
    /// Records dropped since dropping began, and when it began: until an
    /// export goes through again.
    pub dropped: u64,
    pub dropping_since: Option<i64>,
    /// The last failure: the collector unreachable, its answer, or none
    /// in time; and when.
    pub last_error: Option<String>,
    pub last_error_at: Option<i64>,
}

type Shared = Arc<Mutex<ExportStatus>>;

fn now_ms() -> i64 {
    crate::metrics::history::unix_ms()
}

/// An export going on.
struct Running {
    settings: OtlpSettings,
    status: Shared,
    tracer: Option<SdkTracerProvider>,
    logger: Option<SdkLoggerProvider>,
    meter: Option<SdkMeterProvider>,
    spans: Option<mpsc::Sender<Msg<SpanData>>>,
    logs: Option<mpsc::Sender<Msg<LogItem>>>,
}

type LogItem = Box<(SdkLogRecord, InstrumentationScope)>;

enum Msg<T> {
    Item(T),
    /// Export what is queued now, then say so.
    Flush(oneshot::Sender<()>),
}

impl Telemetry {
    /// Export for the server called `name`, whose key is `endpoint_id`,
    /// through `swap`, the subscriber's slot.
    pub fn new(swap: Swap, metrics: Arc<Metrics>, name: ServerName, endpoint_id: String) -> Self {
        Self::with_timings(swap, metrics, name, endpoint_id, Timings::default())
    }

    pub fn with_timings(
        swap: Swap,
        metrics: Arc<Metrics>,
        name: ServerName,
        endpoint_id: String,
        timings: Timings,
    ) -> Self {
        Self {
            swap,
            metrics,
            name,
            endpoint_id,
            timings,
            running: Mutex::default(),
        }
    }

    /// Export as `settings` say from now on, or stop with `None`. The same
    /// settings again change nothing. Within the server's runtime: the
    /// export runs on it.
    pub fn apply(&self, settings: Option<OtlpSettings>) {
        let mut running = self.running.lock().expect("never held across a panic");
        if running.as_ref().map(|r| &r.settings) == settings.as_ref() {
            return;
        }
        let old = running.take();
        let started = settings.and_then(|settings| match self.start(&settings) {
            Ok(started) => Some(started),
            Err(e) => {
                tracing::warn!(endpoint = settings.endpoint, error = %e, "cannot start the OTLP export");
                None
            }
        });
        match started {
            Some((now, layer)) => {
                tracing::info!(
                    endpoint = now.settings.endpoint,
                    protocol = ?now.settings.protocol,
                    traces = now.settings.traces,
                    metrics = now.settings.metrics,
                    logs = now.settings.logs,
                    headers = ?now.settings.headers.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
                    "exporting telemetry over OTLP"
                );
                (self.swap)(Some(layer));
                *running = Some(now);
            }
            None => (self.swap)(None),
        }
        drop(running);
        if let Some(old) = old {
            tokio::spawn(old.stop(self.timings));
        }
    }

    /// Export what is queued and stop: for a server that is stopping.
    pub async fn shutdown(&self) {
        let old = self
            .running
            .lock()
            .expect("never held across a panic")
            .take();
        (self.swap)(None);
        if let Some(old) = old {
            old.stop(self.timings).await;
        }
    }

    /// What is exported to, and how it is going; `None` while off.
    pub fn status(&self) -> Option<(OtlpSettings, ExportStatus)> {
        let running = self.running.lock().expect("never held across a panic");
        running.as_ref().map(|r| {
            (
                r.settings.clone(),
                r.status.lock().expect("never held across a panic").clone(),
            )
        })
    }

    /// How often metrics are exported.
    pub fn metrics_every(&self) -> Duration {
        self.timings.metrics_every
    }

    /// The resource attributes every export carries.
    pub fn resource_attributes(&self) -> Vec<(String, String)> {
        vec![
            ("service.name".into(), SERVICE_NAME.into()),
            ("service.version".into(), env!("CARGO_PKG_VERSION").into()),
            ("spoolframe.server.name".into(), self.name.get()),
            (
                "spoolframe.server.endpoint_id".into(),
                self.endpoint_id.clone(),
            ),
        ]
    }

    /// Export what is queued now, and wait for it (or the timeout).
    pub async fn flush(&self) {
        let (spans, logs) = {
            let running = self.running.lock().expect("never held across a panic");
            match running.as_ref() {
                Some(r) => (r.spans.clone(), r.logs.clone()),
                None => (None, None),
            }
        };
        flush(spans, self.timings).await;
        flush(logs, self.timings).await;
    }

    fn resource(&self) -> Resource {
        Resource::builder_empty()
            .with_attributes(
                self.resource_attributes()
                    .into_iter()
                    .map(|(k, v)| KeyValue::new(k, v)),
            )
            .build()
    }

    fn start(&self, settings: &OtlpSettings) -> Result<(Running, OtelLayer), String> {
        let resource = self.resource();
        let timings = self.timings;
        let status = Shared::default();
        let mut running = Running {
            settings: settings.clone(),
            status: status.clone(),
            tracer: None,
            logger: None,
            meter: None,
            spans: None,
            logs: None,
        };
        if settings.traces {
            let mut exporter = span_exporter(settings, timings)?;
            exporter.set_resource(&resource);
            let (tx, rx) = mpsc::channel(timings.queue);
            tokio::spawn(export_queue(
                rx,
                Out::Spans(exporter),
                Watch::new(
                    Signal::Traces,
                    self.metrics.clone(),
                    status.clone(),
                    timings,
                ),
                timings,
            ));
            running.tracer = Some(
                SdkTracerProvider::builder()
                    .with_resource(resource.clone())
                    .with_span_processor(Bounded {
                        tx: tx.clone(),
                        metrics: self.metrics.clone(),
                        status: status.clone(),
                        signal: Signal::Traces,
                    })
                    .build(),
            );
            running.spans = Some(tx);
        }
        if settings.logs {
            let mut exporter = log_exporter(settings, timings)?;
            exporter.set_resource(&resource);
            let (tx, rx) = mpsc::channel(timings.queue);
            tokio::spawn(export_queue(
                rx,
                Out::Logs(exporter),
                Watch::new(Signal::Logs, self.metrics.clone(), status.clone(), timings),
                timings,
            ));
            running.logger = Some(
                SdkLoggerProvider::builder()
                    .with_resource(resource.clone())
                    .with_log_processor(Bounded {
                        tx: tx.clone(),
                        metrics: self.metrics.clone(),
                        status: status.clone(),
                        signal: Signal::Logs,
                    })
                    .build(),
            );
            running.logs = Some(tx);
        }
        if settings.metrics {
            let exporter = Watched {
                inner: metric_exporter(settings, timings)?,
                watch: Watch::new(
                    Signal::Metrics,
                    self.metrics.clone(),
                    status.clone(),
                    timings,
                ),
            };
            let reader =
                opentelemetry_sdk::metrics::periodic_reader_with_async_runtime::PeriodicReader::builder(
                    exporter,
                    opentelemetry_sdk::runtime::Tokio,
                )
                .with_interval(timings.metrics_every)
                .build();
            let provider = SdkMeterProvider::builder()
                .with_resource(resource)
                .with_reader(reader)
                .build();
            observe(&provider, &self.metrics);
            running.meter = Some(provider);
        }
        let traces = running
            .tracer
            .as_ref()
            .map(|p| tracing_opentelemetry::layer().with_tracer(p.tracer(SERVICE_NAME)));
        let logs = running
            .logger
            .as_ref()
            .map(opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new);
        let layer: OtelLayer = Box::new(Quiet(Layer::and_then(traces, logs)));
        Ok((running, layer))
    }
}

impl Running {
    /// Export what is queued, then shut the providers down, each within
    /// the export timeout. The SDK's shutdown blocks, so it runs off the
    /// runtime's threads.
    async fn stop(self, timings: Timings) {
        flush(self.spans.clone(), timings).await;
        flush(self.logs.clone(), timings).await;
        let (tracer, logger, meter) = (self.tracer, self.logger, self.meter);
        let shut = tokio::task::spawn_blocking(move || {
            if let Some(meter) = meter {
                let _ = meter.shutdown();
            }
            if let Some(tracer) = tracer {
                let _ = tracer.shutdown();
            }
            if let Some(logger) = logger {
                let _ = logger.shutdown();
            }
        });
        if tokio::time::timeout(timings.export_timeout * 2, shut)
            .await
            .is_err()
        {
            tracing::warn!("the OTLP export did not stop in time; what it held is lost");
        }
    }
}

async fn flush<T>(tx: Option<mpsc::Sender<Msg<T>>>, timings: Timings) {
    let Some(tx) = tx else {
        return;
    };
    let (done, wait) = oneshot::channel();
    let asked = tokio::time::timeout(timings.export_timeout, tx.send(Msg::Flush(done))).await;
    if matches!(asked, Ok(Ok(()))) {
        let _ = tokio::time::timeout(timings.export_timeout * 2, wait).await;
    }
}

fn base(endpoint: &str) -> &str {
    endpoint.trim_end_matches('/')
}

fn metadata(headers: &[(String, String)]) -> Result<tonic::metadata::MetadataMap, String> {
    let mut map = tonic::metadata::MetadataMap::new();
    for (name, value) in headers {
        let key = tonic::metadata::MetadataKey::from_bytes(name.to_ascii_lowercase().as_bytes())
            .map_err(|_| format!("{name:?} is not a header name"))?;
        let value = value
            .parse()
            .map_err(|_| format!("the value of header {name:?} is not one a header can carry"))?;
        map.insert(key, value);
    }
    Ok(map)
}

fn header_map(headers: &[(String, String)]) -> std::collections::HashMap<String, String> {
    headers.iter().cloned().collect()
}

fn span_exporter(
    s: &OtlpSettings,
    timings: Timings,
) -> Result<opentelemetry_otlp::SpanExporter, String> {
    let built = match s.protocol {
        Protocol::Grpc => opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(base(&s.endpoint))
            .with_timeout(timings.export_timeout)
            .with_metadata(metadata(&s.headers)?)
            .build(),
        Protocol::Http => opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_endpoint(format!("{}/v1/traces", base(&s.endpoint)))
            .with_timeout(timings.export_timeout)
            .with_headers(header_map(&s.headers))
            .build(),
    };
    built.map_err(|e| e.to_string())
}

fn log_exporter(
    s: &OtlpSettings,
    timings: Timings,
) -> Result<opentelemetry_otlp::LogExporter, String> {
    let built = match s.protocol {
        Protocol::Grpc => opentelemetry_otlp::LogExporter::builder()
            .with_tonic()
            .with_endpoint(base(&s.endpoint))
            .with_timeout(timings.export_timeout)
            .with_metadata(metadata(&s.headers)?)
            .build(),
        Protocol::Http => opentelemetry_otlp::LogExporter::builder()
            .with_http()
            .with_endpoint(format!("{}/v1/logs", base(&s.endpoint)))
            .with_timeout(timings.export_timeout)
            .with_headers(header_map(&s.headers))
            .build(),
    };
    built.map_err(|e| e.to_string())
}

fn metric_exporter(
    s: &OtlpSettings,
    timings: Timings,
) -> Result<opentelemetry_otlp::MetricExporter, String> {
    let built = match s.protocol {
        Protocol::Grpc => opentelemetry_otlp::MetricExporter::builder()
            .with_tonic()
            .with_endpoint(base(&s.endpoint))
            .with_timeout(timings.export_timeout)
            .with_metadata(metadata(&s.headers)?)
            .build(),
        Protocol::Http => opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .with_endpoint(format!("{}/v1/metrics", base(&s.endpoint)))
            .with_timeout(timings.export_timeout)
            .with_headers(header_map(&s.headers))
            .build(),
    };
    built.map_err(|e| e.to_string())
}

/// Failed exports of one signal: counted, and warned of now and then.
struct Watch {
    signal: Signal,
    metrics: Arc<Metrics>,
    status: Shared,
    every: Duration,
    /// When the last warning was, and the failures since.
    warned: Mutex<(Option<Instant>, u64)>,
}

impl Watch {
    fn new(signal: Signal, metrics: Arc<Metrics>, status: Shared, timings: Timings) -> Self {
        Self {
            signal,
            metrics,
            status,
            every: timings.warn_every,
            warned: Mutex::new((None, 0)),
        }
    }

    fn ok(&self) {
        let mut status = self.status.lock().expect("never held across a panic");
        status.last_export_at = Some(now_ms());
        status.dropped = 0;
        status.dropping_since = None;
    }

    fn failed(&self, error: &dyn std::fmt::Display) {
        self.metrics.telemetry_failed(self.signal);
        {
            let mut status = self.status.lock().expect("never held across a panic");
            status.last_error = Some(format!("{}: {error}", self.signal.as_str()));
            status.last_error_at = Some(now_ms());
        }
        let mut warned = self.warned.lock().expect("never held across a panic");
        warned.1 += 1;
        if warned.0.is_none_or(|at| at.elapsed() >= self.every) {
            tracing::warn!(
                signal = self.signal.as_str(),
                failures = warned.1,
                %error,
                "OTLP export failed; it goes on, and says so again in a minute at most"
            );
            *warned = (Some(Instant::now()), 0);
        }
    }
}

/// Where a queue's batches go.
enum Out {
    Spans(opentelemetry_otlp::SpanExporter),
    Logs(opentelemetry_otlp::LogExporter),
}

impl Out {
    async fn spans(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        match self {
            Out::Spans(exporter) => exporter.export(batch).await,
            Out::Logs(_) => Ok(()),
        }
    }

    async fn logs(&self, batch: Vec<LogItem>) -> OTelSdkResult {
        match self {
            Out::Logs(exporter) => {
                let refs: Vec<(&SdkLogRecord, &InstrumentationScope)> =
                    batch.iter().map(|item| (&item.0, &item.1)).collect();
                exporter.export(LogBatch::new(&refs)).await
            }
            Out::Spans(_) => Ok(()),
        }
    }
}

/// What a queue holds, sent out by [`Out`].
trait Item: Sized + Send + 'static {
    fn export(out: &Out, batch: Vec<Self>) -> impl Future<Output = OTelSdkResult> + Send;
}

impl Item for SpanData {
    fn export(out: &Out, batch: Vec<Self>) -> impl Future<Output = OTelSdkResult> + Send {
        out.spans(batch)
    }
}

impl Item for LogItem {
    fn export(out: &Out, batch: Vec<Self>) -> impl Future<Output = OTelSdkResult> + Send {
        out.logs(batch)
    }
}

/// Batches from `rx`, exported until every sender is gone.
async fn export_queue<T: Item>(
    mut rx: mpsc::Receiver<Msg<T>>,
    out: Out,
    watch: Watch,
    timings: Timings,
) {
    let mut batch: Vec<T> = Vec::new();
    let mut tick = tokio::time::interval(timings.flush_every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(Msg::Item(item)) => {
                    batch.push(item);
                    if batch.len() >= timings.batch {
                        send(&out, &watch, &mut batch, timings).await;
                    }
                }
                Some(Msg::Flush(done)) => {
                    send(&out, &watch, &mut batch, timings).await;
                    let _ = done.send(());
                }
                None => {
                    send(&out, &watch, &mut batch, timings).await;
                    return;
                }
            },
            _ = tick.tick() => send(&out, &watch, &mut batch, timings).await,
        }
    }
}

async fn send<T: Item>(out: &Out, watch: &Watch, batch: &mut Vec<T>, timings: Timings) {
    if batch.is_empty() {
        return;
    }
    let taken = std::mem::take(batch);
    let timeout = timings.export_timeout;
    match tokio::time::timeout(timeout, T::export(out, taken)).await {
        Ok(Ok(())) => watch.ok(),
        Ok(Err(e)) => watch.failed(&e),
        Err(_) => watch.failed(&format!("no answer within {timeout:?}")),
    }
}

/// A processor that only queues: never waits, drops (and counts) what a
/// full queue has no room for.
#[derive(Debug)]
struct Bounded<T> {
    tx: mpsc::Sender<Msg<T>>,
    metrics: Arc<Metrics>,
    status: Shared,
    signal: Signal,
}

impl<T> Bounded<T> {
    fn queue(&self, item: T) {
        if self.tx.try_send(Msg::Item(item)).is_err() {
            self.metrics.telemetry_dropped(self.signal);
            let mut status = self.status.lock().expect("never held across a panic");
            status.dropped += 1;
            status.dropping_since.get_or_insert_with(now_ms);
        }
    }
}

impl SpanProcessor for Bounded<SpanData> {
    fn on_start(&self, _span: &mut Span, _cx: &opentelemetry::Context) {}

    fn on_end(&self, span: SpanData) {
        self.queue(span);
    }

    /// The queue is flushed by [`Telemetry::flush`] and at shutdown, from
    /// the runtime: never by blocking a thread here.
    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        Ok(())
    }
}

impl LogProcessor for Bounded<LogItem> {
    fn emit(&self, record: &mut SdkLogRecord, scope: &InstrumentationScope) {
        self.queue(Box::new((record.clone(), scope.clone())));
    }

    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        Ok(())
    }
}

/// The metric exporter, its failures watched.
struct Watched {
    inner: opentelemetry_otlp::MetricExporter,
    watch: Watch,
}

impl std::fmt::Debug for Watched {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watched").finish_non_exhaustive()
    }
}

impl PushMetricExporter for Watched {
    async fn export(&self, metrics: &ResourceMetrics) -> OTelSdkResult {
        let exported = self.inner.export(metrics).await;
        match &exported {
            Ok(()) => self.watch.ok(),
            Err(e) => self.watch.failed(e),
        }
        exported
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn temporality(&self) -> Temporality {
        self.inner.temporality()
    }
}

/// The registry's families, read once per collection rather than once per
/// instrument.
struct Families {
    metrics: Arc<Metrics>,
    read: Mutex<Option<(Instant, Arc<Vec<Family>>)>>,
}

impl Families {
    fn now(&self) -> Arc<Vec<Family>> {
        let mut read = self.read.lock().expect("never held across a panic");
        match &*read {
            Some((at, families)) if at.elapsed() < Duration::from_secs(1) => families.clone(),
            _ => {
                let families = Arc::new(self.metrics.families());
                *read = Some((Instant::now(), families.clone()));
                families
            }
        }
    }

    fn samples(&self, name: &str) -> Vec<(f64, Vec<KeyValue>)> {
        let families = self.now();
        let Some(family) = families.iter().find(|f| f.name == name) else {
            return Vec::new();
        };
        family
            .samples
            .iter()
            .filter_map(|s| match &s.value {
                Value::Number(v) => Some((*v, attributes(&s.labels))),
                Value::Histogram(_) => None,
            })
            .collect()
    }

    /// A histogram's sum or count, as a number per labelled series.
    fn histogram(&self, name: &str, sum: bool) -> Vec<(f64, Vec<KeyValue>)> {
        let families = self.now();
        let Some(family) = families.iter().find(|f| f.name == name) else {
            return Vec::new();
        };
        family
            .samples
            .iter()
            .filter_map(|s| match &s.value {
                Value::Histogram(h) => Some((
                    if sum { h.sum } else { h.count as f64 },
                    attributes(&s.labels),
                )),
                Value::Number(_) => None,
            })
            .collect()
    }
}

fn attributes(labels: &[(&'static str, String)]) -> Vec<KeyValue> {
    labels
        .iter()
        .map(|(k, v)| KeyValue::new(*k, v.clone()))
        .collect()
}

/// A counter's name as OTLP has it: without `_total`, which the
/// translation to Prometheus adds back to a monotonic sum.
fn otel_name(name: &'static str) -> &'static str {
    name.strip_suffix("_total").unwrap_or(name)
}

/// Every family of the registry as an instrument of `provider`'s, read
/// at each collection.
fn observe(provider: &SdkMeterProvider, metrics: &Arc<Metrics>) {
    let meter = provider.meter(SERVICE_NAME);
    let families = Arc::new(Families {
        metrics: metrics.clone(),
        read: Mutex::default(),
    });
    for family in metrics.families() {
        let name = family.name;
        let help = family.help;
        let read = families.clone();
        match family.kind {
            Kind::Counter => {
                meter
                    .f64_observable_counter(otel_name(name))
                    .with_description(help)
                    .with_callback(move |o| {
                        for (v, attrs) in read.samples(name) {
                            o.observe(v, &attrs);
                        }
                    })
                    .build();
            }
            Kind::Gauge => {
                meter
                    .f64_observable_gauge(name)
                    .with_description(help)
                    .with_callback(move |o| {
                        for (v, attrs) in read.samples(name) {
                            o.observe(v, &attrs);
                        }
                    })
                    .build();
            }
            // No observable histogram in OpenTelemetry: its sum and count,
            // from which a mean is had.
            Kind::Histogram => {
                for sum in [true, false] {
                    let read = families.clone();
                    let leaked: &'static str = if sum {
                        histogram_sum(name)
                    } else {
                        histogram_count(name)
                    };
                    meter
                        .f64_observable_counter(leaked)
                        .with_description(help)
                        .with_callback(move |o| {
                            for (v, attrs) in read.histogram(name, sum) {
                                o.observe(v, &attrs);
                            }
                        })
                        .build();
                }
            }
        }
    }
}

fn histogram_sum(name: &str) -> &'static str {
    match name {
        "spoolframe_plex_request_duration_seconds" => {
            "spoolframe_plex_request_duration_seconds_sum"
        }
        _ => "spoolframe_histogram_sum",
    }
}

fn histogram_count(name: &str) -> &'static str {
    match name {
        "spoolframe_plex_request_duration_seconds" => {
            "spoolframe_plex_request_duration_seconds_count"
        }
        _ => "spoolframe_histogram_count",
    }
}

/// The libraries the export itself runs on: what they log and trace is
/// never exported, or every export would make another.
const QUIET: [&str; 9] = [
    "opentelemetry",
    "opentelemetry_sdk",
    "opentelemetry_otlp",
    "tracing_opentelemetry",
    "tonic",
    "h2",
    "hyper",
    "hyper_util",
    "reqwest",
];

/// The target of log lines for this host's own log only, never exported:
/// the setup code (PX-09 finding 20), which takes over a server with no
/// users, so whoever reads the collector's logs must not see it.
pub const LOCAL_ONLY: &str = "spoolframe::local_only";

fn loud(meta: &Metadata<'_>) -> bool {
    let target = meta.target();
    target != LOCAL_ONLY
        && !QUIET.iter().any(|quiet| {
            target
                .strip_prefix(quiet)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
        })
}

/// A layer that never sees the export's own libraries, nor
/// [`LOCAL_ONLY`] lines.
struct Quiet<L>(L);

impl<S, L> Layer<S> for Quiet<L>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    L: Layer<S>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        if loud(attrs.metadata()) {
            self.0.on_new_span(attrs, id, ctx);
        }
    }

    fn on_record(
        &self,
        span: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        ctx: Context<'_, S>,
    ) {
        self.0.on_record(span, values, ctx);
    }

    fn on_follows_from(
        &self,
        span: &tracing::span::Id,
        follows: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        self.0.on_follows_from(span, follows, ctx);
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        if loud(event.metadata()) {
            self.0.on_event(event, ctx);
        }
    }

    fn on_enter(&self, id: &tracing::span::Id, ctx: Context<'_, S>) {
        self.0.on_enter(id, ctx);
    }

    fn on_exit(&self, id: &tracing::span::Id, ctx: Context<'_, S>) {
        self.0.on_exit(id, ctx);
    }

    fn on_close(&self, id: tracing::span::Id, ctx: Context<'_, S>) {
        self.0.on_close(id, ctx);
    }

    fn on_id_change(&self, old: &tracing::span::Id, new: &tracing::span::Id, ctx: Context<'_, S>) {
        self.0.on_id_change(old, new, ctx);
    }
}
