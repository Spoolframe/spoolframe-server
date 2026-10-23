//! OTLP export (P3-14) against in-process stand-in collectors: one over
//! HTTP/protobuf, one over gRPC, and one that never answers. Nothing here
//! reaches a real collector.

use parking_lot::Mutex;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::State;
use axum::routing::post;
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::trace_service_server::{
    TraceService, TraceServiceServer,
};
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use opentelemetry_proto::tonic::common::v1::{KeyValue, any_value};
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::Span;
use prost::Message as _;
use spoolframe_protocol::resource::{ResourceRequest, ResourceResponse};
use spoolframe_protocol::{MediaRead, PlaybackHandle, Reply, Request, Response, SessionId};
use spoolframe_server::metrics::Metrics;
use spoolframe_server::network::ServerName;
use spoolframe_server::telemetry::{
    OtelLayer, OtlpSettings, Protocol, Signal, Swap, Telemetry, Timings, Tracer, spans_layer,
};
use tracing_subscriber::layer::SubscriberExt as _;

use crate::support::{self, ServerOptions, TestClient, TestServer};

/// What a stand-in collector was sent.
#[derive(Debug, Default, Clone)]
struct Received {
    traces: Arc<Mutex<Vec<ExportTraceServiceRequest>>>,
    metrics: Arc<Mutex<Vec<ExportMetricsServiceRequest>>>,
    logs: Arc<Mutex<Vec<ExportLogsServiceRequest>>>,
}

fn protobuf_ok() -> impl axum::response::IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "application/x-protobuf")],
        Vec::<u8>::new(),
    )
}

/// An OTLP/HTTP collector on loopback.
async fn http_collector() -> (SocketAddr, Received) {
    let received = Received::default();
    let app = axum::Router::new()
        .route(
            "/v1/traces",
            post(|State(r): State<Received>, body: Bytes| async move {
                let request = ExportTraceServiceRequest::decode(body).unwrap();
                r.traces.lock().push(request);
                protobuf_ok()
            }),
        )
        .route(
            "/v1/metrics",
            post(|State(r): State<Received>, body: Bytes| async move {
                let request = ExportMetricsServiceRequest::decode(body).unwrap();
                r.metrics.lock().push(request);
                protobuf_ok()
            }),
        )
        .route(
            "/v1/logs",
            post(|State(r): State<Received>, body: Bytes| async move {
                let request = ExportLogsServiceRequest::decode(body).unwrap();
                r.logs.lock().push(request);
                protobuf_ok()
            }),
        )
        .with_state(received.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, received)
}

struct Traces(Received);

#[tonic::async_trait]
impl TraceService for Traces {
    async fn export(
        &self,
        request: tonic::Request<ExportTraceServiceRequest>,
    ) -> Result<tonic::Response<ExportTraceServiceResponse>, tonic::Status> {
        self.0.traces.lock().push(request.into_inner());
        Ok(tonic::Response::new(ExportTraceServiceResponse::default()))
    }
}

/// An OTLP/gRPC collector on loopback, for traces.
async fn grpc_collector() -> (SocketAddr, Received) {
    let received = Received::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = TraceServiceServer::new(Traces(received.clone()));
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(service)
            .serve_with_incoming(tonic::transport::server::TcpIncoming::from(listener))
            .await
            .unwrap()
    });
    (addr, received)
}

/// A collector that takes connections and never answers.
async fn silent_collector() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });
    addr
}

fn timings() -> Timings {
    Timings {
        queue: 64,
        batch: 16,
        flush_every: Duration::from_millis(100),
        metrics_every: Duration::from_millis(200),
        export_timeout: Duration::from_secs(2),
        warn_every: Duration::from_secs(60),
    }
}

/// An export for a server called Den, and the subscriber it sends from,
/// as the server's is made: used on the test's thread only, so nothing
/// else is exported.
fn telemetry(
    metrics: Arc<Metrics>,
    timings: Timings,
) -> (Telemetry, impl tracing::Subscriber + Send + Sync) {
    let (slot, handle) = tracing_subscriber::reload::Layer::new(None::<OtelLayer>);
    // Gone with the test's subscriber, once it has done its part.
    let swap: Swap = Box::new(move |layer| {
        let _ = handle.reload(layer);
    });
    let tracer = Tracer::default();
    let subscriber = tracing_subscriber::registry()
        .with(slot)
        .with(spans_layer(&tracer));
    let telemetry = Telemetry::with_timings(
        swap,
        tracer,
        metrics,
        ServerName::new("Den"),
        "f00dfeed".into(),
        timings,
    );
    (telemetry, subscriber)
}

fn settings(endpoint: String, protocol: Protocol) -> OtlpSettings {
    OtlpSettings {
        endpoint,
        protocol,
        headers: vec![("x-scope-orgid".into(), "home".into())],
        traces: true,
        metrics: true,
        logs: true,
    }
}

/// A playback start's span, and a warning in it. As the server's own:
/// another crate's span is not exported.
fn activity() {
    let span = tracing::info_span!(
        target: "spoolframe_server::client",
        "client request",
        request = "StartPlayback"
    );
    span.in_scope(|| tracing::warn!(server = "plex", "Plex server unavailable"));
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let within = Duration::from_secs(20);
    tokio::time::timeout(within, async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("not within {within:?}: {what}"));
}

/// The resource's attributes, as strings.
fn attributes(resource: Option<&Resource>) -> Vec<(String, String)> {
    let mut found: Vec<_> = resource
        .expect("a resource")
        .attributes
        .iter()
        .filter_map(|kv| match kv.value.as_ref()?.value.as_ref()? {
            any_value::Value::StringValue(s) => Some((kv.key.clone(), s.clone())),
            _ => None,
        })
        .collect();
    found.sort();
    found
}

fn expected_resource() -> Vec<(String, String)> {
    let mut expected = vec![
        ("service.name".to_owned(), "spoolframe-server".to_owned()),
        (
            "service.version".to_owned(),
            env!("CARGO_PKG_VERSION").to_owned(),
        ),
        (
            "spoolframe.server.endpoint_id".to_owned(),
            "f00dfeed".to_owned(),
        ),
        ("spoolframe.server.name".to_owned(), "Den".to_owned()),
    ];
    expected.sort();
    expected
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spans_metrics_and_logs_reach_an_http_collector_with_the_resource() {
    let (addr, received) = http_collector().await;
    let metrics = Arc::new(Metrics::default());
    let (telemetry, subscriber) = telemetry(metrics, timings());
    telemetry.apply(Some(settings(format!("http://{addr}"), Protocol::Http)));
    tracing::subscriber::with_default(subscriber, activity);
    telemetry.flush().await;
    until("a span, a log record and metrics arrive", || {
        !received.traces.lock().is_empty()
            && !received.logs.lock().is_empty()
            && !received.metrics.lock().is_empty()
    })
    .await;

    let traces = received.traces.lock().clone();
    let spans = &traces[0].resource_spans[0];
    assert_eq!(attributes(spans.resource.as_ref()), expected_resource());
    let names: Vec<_> = spans
        .scope_spans
        .iter()
        .flat_map(|s| &s.spans)
        .map(|s| s.name.as_str())
        .collect();
    assert!(names.contains(&"client request"), "{names:?}");

    let logs = received.logs.lock().clone();
    let records = &logs[0].resource_logs[0];
    assert_eq!(attributes(records.resource.as_ref()), expected_resource());
    let bodies: Vec<_> = records
        .scope_logs
        .iter()
        .flat_map(|s| &s.log_records)
        .filter_map(|r| match r.body.as_ref()?.value.as_ref()? {
            any_value::Value::StringValue(s) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    assert!(bodies.contains(&"Plex server unavailable"), "{bodies:?}");

    let exported = received.metrics.lock().clone();
    let resource_metrics = &exported[0].resource_metrics[0];
    assert_eq!(
        attributes(resource_metrics.resource.as_ref()),
        expected_resource()
    );
    let names: Vec<_> = resource_metrics
        .scope_metrics
        .iter()
        .flat_map(|s| &s.metrics)
        .map(|m| m.name.as_str())
        .collect();
    for name in [
        "spoolframe_process_uptime_seconds",
        "spoolframe_process_memory_bytes",
        "spoolframe_cache_memory_bytes",
        "spoolframe_cache_disk_bytes",
        "spoolframe_cache_requests",
        "spoolframe_cache_evictions",
        "spoolframe_plex_replies_invalidated",
        // A counter goes without `_total`, which Prometheus adds back.
        "spoolframe_playbacks_started",
        "spoolframe_telemetry_dropped",
    ] {
        assert!(names.contains(&name), "{name} not in {names:?}");
    }
    let (_, status) = telemetry.status().unwrap();
    assert!(status.last_export_at.is_some());
    assert_eq!((status.dropped, status.last_error), (0, None));
    telemetry.shutdown().await;
    assert!(telemetry.status().is_none());
}

/// PX-09 finding 20: the setup code takes over a server with no users
/// yet, so it is never sent to the collector, where whoever reads the
/// logs would see it. It stays in the server's own log.
/// A histogram goes out as one, with the bounds `/metrics` has, so the
/// collector can tell a p95 and not only a mean.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plex_request_s_time_is_exported_as_a_histogram_with_buckets() {
    use opentelemetry_proto::tonic::metrics::v1::metric::Data;
    use spoolframe_server::catalog::{Composite, LiveCatalog};

    let (addr, received) = http_collector().await;
    let metrics = Arc::new(Metrics::default());
    // Nothing listens there: the request fails at once, and is timed.
    let plex =
        spoolframe_plex::PlexCatalog::new("http://127.0.0.1:9".parse().unwrap(), None).unwrap();
    let live = Arc::new(LiveCatalog::new(Some(Arc::new(Composite::single(
        "Tower", plex,
    )))));
    metrics.observe_catalog(live.clone());
    let (telemetry, _subscriber) = telemetry(metrics, timings());
    telemetry.apply(Some(OtlpSettings {
        traces: false,
        logs: false,
        ..settings(format!("http://{addr}"), Protocol::Http)
    }));
    let _ = live.plex().unwrap().server_name().await;

    let histogram = || {
        received.metrics.lock().iter().rev().find_map(|request| {
            request
                .resource_metrics
                .iter()
                .flat_map(|r| &r.scope_metrics)
                .flat_map(|s| &s.metrics)
                .find(|m| m.name == "spoolframe_plex_request_duration_seconds")
                .and_then(|m| match &m.data {
                    Some(Data::Histogram(h)) => Some(h.clone()),
                    _ => None,
                })
        })
    };
    until("the histogram arrives", || {
        histogram().is_some_and(|h| !h.data_points.is_empty())
    })
    .await;
    let names: Vec<_> = received
        .metrics
        .lock()
        .iter()
        .flat_map(|r| &r.resource_metrics)
        .flat_map(|r| &r.scope_metrics)
        .flat_map(|s| &s.metrics)
        .map(|m| m.name.clone())
        .collect();
    assert!(
        !names
            .iter()
            .any(|n| n.ends_with("_sum") || n.ends_with("_count")),
        "{names:?}"
    );
    let point = histogram().unwrap().data_points[0].clone();
    assert_eq!(point.count, 1);
    assert_eq!(
        point.explicit_bounds,
        spoolframe_plex::stats::LATENCY_BUCKETS
    );
    assert_eq!(point.bucket_counts.len(), point.explicit_bounds.len() + 1);
    let server: Vec<_> = point
        .attributes
        .iter()
        .map(|kv| (kv.key.as_str(), format!("{:?}", kv.value)))
        .collect();
    assert_eq!(server.len(), 1, "{server:?}");
    assert_eq!(server[0].0, "server");
    assert!(server[0].1.contains("\"plex\""), "{server:?}");
    telemetry.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_setup_code_is_never_exported() {
    let (addr, received) = http_collector().await;
    let (telemetry, subscriber) = telemetry(Arc::new(Metrics::default()), timings());
    telemetry.apply(Some(settings(format!("http://{addr}"), Protocol::Http)));
    let code = spoolframe_server::admin::SetupCode::generate();
    tracing::subscriber::with_default(subscriber, || {
        spoolframe_server::admin::log_setup_code(&["http://127.0.0.1:7421/".into()], &code);
        activity();
    });
    telemetry.flush().await;
    until("the warning after it arrives", || {
        !received.logs.lock().is_empty()
    })
    .await;
    let logs = format!("{:?}", received.logs.lock());
    assert!(logs.contains("Plex server unavailable"), "{logs}");
    assert!(!logs.contains(&code.to_string()), "{logs}");
    assert!(!logs.contains("setup code"), "{logs}");
    telemetry.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spans_reach_a_grpc_collector() {
    let (addr, received) = grpc_collector().await;
    let (telemetry, subscriber) = telemetry(Arc::new(Metrics::default()), timings());
    telemetry.apply(Some(OtlpSettings {
        metrics: false,
        logs: false,
        ..settings(format!("http://{addr}"), Protocol::Grpc)
    }));
    tracing::subscriber::with_default(subscriber, activity);
    telemetry.flush().await;
    until("a span arrives", || !received.traces.lock().is_empty()).await;
    let traces = received.traces.lock().clone();
    assert_eq!(
        attributes(traces[0].resource_spans[0].resource.as_ref()),
        expected_resource()
    );
    telemetry.shutdown().await;
}

/// A collector that never answers costs a request nothing: records are
/// queued without waiting, dropped when the queue is full, and counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_collector_that_never_answers_slows_nothing_and_drops_are_counted() {
    let addr = silent_collector().await;
    let metrics = Arc::new(Metrics::default());
    let (telemetry, subscriber) = telemetry(
        metrics.clone(),
        Timings {
            queue: 4,
            batch: 2,
            ..timings()
        },
    );
    telemetry.apply(Some(OtlpSettings {
        metrics: false,
        ..settings(format!("http://{addr}"), Protocol::Http)
    }));
    let started = Instant::now();
    tracing::subscriber::with_default(subscriber, || {
        for _ in 0..200 {
            activity();
        }
    });
    // Far under one export's timeout: nothing waited on the collector.
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    assert!(metrics.telemetry_dropped_count(Signal::Traces) > 0);
    assert!(metrics.telemetry_dropped_count(Signal::Logs) > 0);
    let (_, status) = telemetry.status().unwrap();
    assert!(status.dropped > 0 && status.dropping_since.is_some());
    until("the export times out and says so", || {
        telemetry.status().unwrap().1.last_error.is_some()
    })
    .await;
    let families = metrics.families();
    let failures = families
        .iter()
        .find(|f| f.name == "spoolframe_telemetry_export_failures_total")
        .unwrap();
    assert!(failures.total() > 0.0);
}

/// Every span the collector was sent, in the order it got them.
fn spans_in(received: &Received) -> Vec<Span> {
    received
        .traces
        .lock()
        .iter()
        .flat_map(|r| &r.resource_spans)
        .flat_map(|r| &r.scope_spans)
        .flat_map(|s| &s.spans)
        .cloned()
        .collect()
}

/// The value of attribute `key`, as text.
fn attribute(attributes: &[KeyValue], key: &str) -> Option<String> {
    let value = attributes.iter().find(|kv| kv.key == key)?.value.as_ref()?;
    match value.value.as_ref()? {
        any_value::Value::StringValue(s) => Some(s.clone()),
        any_value::Value::IntValue(i) => Some(i.to_string()),
        any_value::Value::BoolValue(b) => Some(b.to_string()),
        any_value::Value::DoubleValue(d) => Some(d.to_string()),
        _ => None,
    }
}

/// The spans called `name`.
fn named<'a>(spans: &'a [Span], name: &str) -> Vec<&'a Span> {
    spans.iter().filter(|s| s.name == name).collect()
}

/// The `client request` span of request type `request`.
fn request<'a>(spans: &'a [Span], request: &str) -> &'a Span {
    named(spans, "client request")
        .into_iter()
        .find(|s| attribute(&s.attributes, "request").as_deref() == Some(request))
        .unwrap_or_else(|| panic!("no {request} request in {:?}", names(spans)))
}

fn names(spans: &[Span]) -> Vec<&str> {
    spans.iter().map(|s| s.name.as_str()).collect()
}

/// A span the server opens inside a library's is a root, with a duration.
/// iroh's socket span stays open as long as the endpoint does, so Tempo
/// never got it, and the traces of its `poll_send` spans, every few
/// seconds, had no root.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_the_server_s_own_spans_are_exported_and_each_has_its_root() {
    let (addr, received) = http_collector().await;
    let (telemetry, subscriber) = telemetry(Arc::new(Metrics::default()), timings());
    telemetry.apply(Some(OtlpSettings {
        metrics: false,
        logs: false,
        ..settings(format!("http://{addr}"), Protocol::Http)
    }));
    tracing::subscriber::with_default(subscriber, || {
        let socket = tracing::info_span!(target: "iroh::socket::transports", "poll_send");
        socket.in_scope(activity);
    });
    telemetry.flush().await;
    until("a span arrives", || !spans_in(&received).is_empty()).await;
    let spans = spans_in(&received);
    assert_eq!(names(&spans), ["client request"]);
    let request = &spans[0];
    assert!(request.parent_span_id.is_empty(), "{request:?}");
    assert!(
        request.end_time_unix_nano > request.start_time_unix_nano,
        "{request:?}"
    );
    // Its warning is a log record, not an event of the span.
    assert!(request.events.is_empty(), "{request:?}");
    telemetry.shutdown().await;
}

/// A server whose spans go to a stand-in collector. Its subscriber is the
/// test thread's, and a `#[tokio::test]` runs the server's tasks on that
/// thread; so, as a log capture, only in a test run [`support::alone`].
struct Traced {
    server: TestServer,
    telemetry: Telemetry,
    received: Received,
    _subscriber: tracing::subscriber::DefaultGuard,
}

impl Traced {
    async fn start(test: &str) -> Self {
        Self::start_with(test, ServerOptions::default()).await
    }

    async fn start_with(test: &str, options: ServerOptions) -> Self {
        let (addr, received) = http_collector().await;
        let (telemetry, subscriber) = telemetry(Arc::new(Metrics::default()), timings());
        let subscriber = tracing::subscriber::set_default(subscriber);
        telemetry.apply(Some(OtlpSettings {
            metrics: false,
            ..settings(format!("http://{addr}"), Protocol::Http)
        }));
        let server = TestServer::start_with(support::fresh_dir(test).await, options).await;
        Self {
            server,
            telemetry,
            received,
            _subscriber: subscriber,
        }
    }

    /// Alice, signed in on a client that plays the stand-in MKV.
    async fn alice(&self) -> (TestClient, SessionId) {
        self.alice_on(support::mkv_capabilities()).await
    }

    /// Alice, signed in on a client that plays what `capabilities` say.
    async fn alice_on(
        &self,
        capabilities: spoolframe_types::ClientCapabilities,
    ) -> (TestClient, SessionId) {
        self.server.add_user("alice", "correct horse", None).await;
        let client = self.server.client().await;
        client.hello_with(None, Some(capabilities)).await;
        let session = client.login("alice", "correct horse").await;
        (client, session)
    }

    /// Every span exported so far, once what was queued has gone.
    async fn spans(&self) -> Vec<Span> {
        self.telemetry.flush().await;
        spans_in(&self.received)
    }

    /// Wait until the spans exported are `what`: a span that ends after
    /// its client has its answer (a stream waits for the client to have
    /// it all) may not have ended yet.
    async fn until_spans(&self, what: &str, done: impl Fn(&[Span]) -> bool) {
        until(what, || done(&spans_in(&self.received))).await;
    }
}

async fn start_playback(client: &TestClient, session: &SessionId) -> PlaybackHandle {
    let started = client
        .call(
            Some(session),
            Request::StartPlayback {
                id: support::movie().id().clone(),
                selection: None,
                start_ms: None,
                bitmap: Default::default(),
                quality: Default::default(),
                display: None,
                capabilities: None,
                onward_share: None,
            },
        )
        .await;
    match started {
        Response::Ok(Reply::PlaybackStarted { handle, .. }) => handle,
        other => panic!("playback did not start: {other:?}"),
    }
}

/// A request that changes something runs in a task of its own: what it
/// does is in its request's trace all the same, not a trace of its own.
/// Its log lines say which span they were logged in, and are not events
/// of the span: they name the user and the device.
#[tokio::test]
async fn what_a_mutation_does_is_in_its_request_s_span() {
    if !support::alone("telemetry::what_a_mutation_does_is_in_its_request_s_span") {
        return;
    }
    let traced = Traced::start("trace-mutation").await;
    traced.server.write_media(4096);
    let (client, session) = traced.alice().await;
    start_playback(&client, &session).await;
    let spans = traced.spans().await;
    let start = request(&spans, "start_playback");
    assert!(start.events.is_empty(), "{:?}", start.events);
    until("the log record arrives", || {
        !traced.received.logs.lock().is_empty()
    })
    .await;
    let logs = traced.received.logs.lock().clone();
    let started = logs
        .iter()
        .flat_map(|r| &r.resource_logs)
        .flat_map(|r| &r.scope_logs)
        .flat_map(|s| &s.log_records)
        .find(|r| {
            matches!(
                r.body.as_ref().and_then(|b| b.value.as_ref()),
                Some(any_value::Value::StringValue(s)) if s == "playback started"
            )
        })
        .expect("the playback's log line");
    assert_eq!(
        (&started.trace_id, &started.span_id),
        (&start.trace_id, &start.span_id)
    );
}

/// Whether `span` is about the playback whose `playback` span is
/// `playback`: its id, and a link to that span.
fn about(span: &Span, playback: &Span) -> bool {
    attribute(&span.attributes, "playback.id") == attribute(&playback.attributes, "playback.id")
        && span
            .links
            .iter()
            .any(|l| l.trace_id == playback.trace_id && l.span_id == playback.span_id)
}

/// A playback is told in Tempo by its `playback` span, a root that ends
/// as the start does, and every later request about it links to it.
#[tokio::test]
async fn a_playback_s_requests_link_to_its_start() {
    if !support::alone("telemetry::a_playback_s_requests_link_to_its_start") {
        return;
    }
    let traced = Traced::start("trace-playback").await;
    traced.server.write_media(4096);
    let (client, session) = traced.alice().await;
    let handle = start_playback(&client, &session).await;
    let reported = client
        .call(
            Some(&session),
            Request::UpdatePlaybackProgress {
                handle: handle.clone(),
                sequence: 1,
                position_ms: 1000,
            },
        )
        .await;
    assert_eq!(reported, Response::Ok(Reply::Done));
    let stopped = client
        .call(Some(&session), Request::StopPlayback { handle })
        .await;
    assert_eq!(stopped, Response::Ok(Reply::Done));

    let spans = traced.spans().await;
    let playbacks = named(&spans, "playback");
    assert_eq!(playbacks.len(), 1, "{:?}", names(&spans));
    let playback = playbacks[0];
    assert!(playback.parent_span_id.is_empty(), "{playback:?}");
    let id = attribute(&playback.attributes, "playback.id").expect("its id");
    assert_eq!(id.len(), 8, "{id}");
    for (key, value) in [
        ("playback.plan", "direct"),
        ("playback.start_ms", "0"),
        ("outcome", "ok"),
    ] {
        assert_eq!(
            attribute(&playback.attributes, key).as_deref(),
            Some(value),
            "{key}"
        );
    }
    // The playback debug page names it so too.
    assert_eq!(traced.server.debug.playbacks.ended()[0].last.id, id);
    for request_type in [
        "start_playback",
        "update_playback_progress",
        "stop_playback",
    ] {
        assert!(
            about(request(&spans, request_type), playback),
            "{request_type}: {:?}",
            request(&spans, request_type)
        );
    }
}

/// Each read of a playback's media is a `segment read` span, about the
/// playback: what was asked, what was sent, and how it went.
#[tokio::test]
async fn a_media_read_is_a_span_about_its_playback() {
    if !support::alone("telemetry::a_media_read_is_a_span_about_its_playback") {
        return;
    }
    let traced = Traced::start("trace-read").await;
    traced.server.write_media(4096);
    let (client, session) = traced.alice().await;
    let handle = start_playback(&client, &session).await;
    let (read, bytes) = client.read(&session, &handle, 1000, 500).await;
    assert!(
        matches!(read, spoolframe_protocol::MediaReadResponse::Ok(_)),
        "{read:?}"
    );
    assert_eq!(bytes.len(), 500);
    // A read at a revision the playback is not at is refused.
    let (refused, _) = client
        .resource(&ResourceRequest::Media {
            request: MediaRead {
                session: session.clone(),
                handle: handle.clone(),
                offset: 0,
                length: 500,
                epoch: 0,
                segment: None,
                variant: 0,
            },
            revision: 7,
        })
        .await;
    assert!(
        matches!(refused, ResourceResponse::Error { .. }),
        "{refused:?}"
    );

    traced
        .until_spans("both reads' spans", |spans| {
            named(spans, "segment read").len() == 2
        })
        .await;
    let spans = traced.spans().await;
    let playback = named(&spans, "playback")[0];
    let reads = named(&spans, "segment read");
    assert_eq!(reads.len(), 2, "{:?}", names(&spans));
    let read = reads[0];
    assert!(about(read, playback), "{read:?}");
    for (key, value) in [
        ("read.offset", "1000"),
        ("read.length", "500"),
        ("read.bytes", "500"),
        ("conversion.epoch", "0"),
        ("outcome", "ok"),
    ] {
        assert_eq!(
            attribute(&read.attributes, key).as_deref(),
            Some(value),
            "{key}"
        );
    }
    // A range of the file, not a segment.
    assert_eq!(attribute(&read.attributes, "segment.index"), None);
    assert!(about(reads[1], playback), "{:?}", reads[1]);
    assert_eq!(
        attribute(&reads[1].attributes, "outcome").as_deref(),
        Some("revision_conflict")
    );
}

/// The names of `span`'s events.
fn events(span: &Span) -> Vec<&str> {
    span.events.iter().map(|e| e.name.as_str()).collect()
}

/// Each ffmpeg run of a conversion is a `conversion` span, from its spawn
/// to its exit or its kill: one replaced at a seek says where to, and the
/// server's stop ends the one running. Real ffmpeg, on generated media
/// whose AC-3 an Apple-like device has converted.
#[tokio::test]
async fn each_ffmpeg_run_is_a_conversion_span_ended_by_its_exit_or_kill() {
    if !support::alone("telemetry::each_ffmpeg_run_is_a_conversion_span_ended_by_its_exit_or_kill")
    {
        return;
    }
    let traced = Traced::start_with(
        "trace-conversion",
        ServerOptions {
            real_media: true,
            read_ahead: Duration::from_secs(2),
            ..ServerOptions::default()
        },
    )
    .await;
    traced.server.generate_media(60, "libx264", "ac3");
    let (client, session) = traced.alice_on(support::hls_capabilities()).await;
    let handle = start_playback(&client, &session).await;
    // Its first segment, which the read waits for.
    let (read, bytes) = client
        .media(MediaRead {
            session: session.clone(),
            handle: handle.clone(),
            offset: 0,
            length: spoolframe_protocol::MAX_READ,
            epoch: 0,
            segment: Some(1),
            variant: 0,
        })
        .await;
    assert!(
        matches!(read, spoolframe_protocol::MediaReadResponse::Ok(_)),
        "{read:?}"
    );
    assert!(!bytes.is_empty());
    let seeked = client
        .call(
            Some(&session),
            Request::SeekPlayback {
                handle: handle.clone(),
                position_ms: 30_000,
            },
        )
        .await;
    assert!(
        matches!(seeked, Response::Ok(Reply::PlaybackSeeked { .. })),
        "{seeked:?}"
    );
    // As the server does as it stops.
    traced.server.jobs.stop_all();
    traced
        .until_spans("both runs' spans", |spans| {
            named(spans, "conversion").len() == 2
        })
        .await;

    let spans = traced.spans().await;
    let playback = named(&spans, "playback")[0];
    let runs = named(&spans, "conversion");
    let run = |reason: &str| {
        *runs
            .iter()
            .find(|r| attribute(&r.attributes, "conversion.reason").as_deref() == Some(reason))
            .unwrap_or_else(|| panic!("no {reason} run in {runs:?}"))
    };
    let (first, second) = (run("start"), run("seek"));
    for span in [first, second] {
        assert!(about(span, playback), "{span:?}");
        assert!(span.parent_span_id.is_empty(), "{span:?}");
        for (key, value) in [
            ("conversion.delivery", "hls"),
            ("conversion.plan", "transcode"),
            ("conversion.load", "audio"),
            ("conversion.encoder", "copy"),
            ("conversion.hardware", "cpu"),
            ("conversion.decode", "none"),
            ("conversion.filters", "none"),
            ("outcome", "killed"),
        ] {
            assert_eq!(
                attribute(&span.attributes, key).as_deref(),
                Some(value),
                "{key}: {span:?}"
            );
        }
        for key in [
            "conversion.encoded_ms",
            "conversion.running_ms",
            "conversion.paused_ms",
        ] {
            assert!(
                attribute(&span.attributes, key).is_some(),
                "{key}: {span:?}"
            );
        }
    }
    assert_eq!(
        attribute(&first.attributes, "conversion.killed_by").as_deref(),
        Some("seek")
    );
    assert!(events(first).contains(&"first segment ready"), "{first:?}");
    let replaced = first
        .events
        .iter()
        .find(|e| e.name == "replaced")
        .expect("said it was replaced");
    let at: u64 = attribute(&replaced.attributes, "position_ms")
        .unwrap()
        .parse()
        .unwrap();
    assert!((25_000..=30_000).contains(&at), "{at}");
    assert_eq!(
        attribute(&second.attributes, "conversion.killed_by").as_deref(),
        Some("server_stopping")
    );
    assert_eq!(
        attribute(&second.attributes, "conversion.start_ms"),
        Some(at.to_string())
    );

    // The segment's read waited for the encode, and says so.
    let read = named(&spans, "segment read")[0];
    assert_eq!(
        attribute(&read.attributes, "segment.index").as_deref(),
        Some("1")
    );
    assert!(about(read, playback), "{read:?}");
    assert!(
        attribute(&read.attributes, "read.waited_for_encode").is_some(),
        "{read:?}"
    );
    assert!(
        attribute(&read.attributes, "read.wait_ms").is_some(),
        "{read:?}"
    );
}

/// A run that ffmpeg finishes ends its span then, with its exit code and
/// how fast it encoded, though its playback holds it on.
#[tokio::test]
async fn a_finished_run_s_span_ends_at_its_exit() {
    if !support::alone("telemetry::a_finished_run_s_span_ends_at_its_exit") {
        return;
    }
    let traced = Traced::start_with(
        "trace-conversion-exit",
        ServerOptions {
            real_media: true,
            read_ahead: Duration::from_secs(3600),
            ..ServerOptions::default()
        },
    )
    .await;
    traced.server.generate_media(8, "libx264", "ac3");
    let (client, session) = traced.alice_on(support::progressive_capabilities()).await;
    let _handle = start_playback(&client, &session).await;
    traced
        .until_spans("the run's span", |spans| {
            !named(spans, "conversion").is_empty()
        })
        .await;
    let spans = traced.spans().await;
    let run = named(&spans, "conversion")[0];
    assert_eq!(attribute(&run.attributes, "outcome").as_deref(), Some("ok"));
    assert_eq!(
        attribute(&run.attributes, "conversion.exit_code").as_deref(),
        Some("0")
    );
    assert_eq!(
        attribute(&run.attributes, "conversion.delivery").as_deref(),
        Some("progressive")
    );
    let exited = run
        .events
        .iter()
        .find(|e| e.name == "exited")
        .expect("its exit");
    assert_eq!(
        attribute(&exited.attributes, "exit_code").as_deref(),
        Some("0")
    );
    assert!(events(run).contains(&"first segment ready"), "{run:?}");
    let speed: f64 = attribute(&run.attributes, "conversion.speed")
        .expect("its speed")
        .parse()
        .unwrap();
    assert!(speed > 0.0, "{speed}");
}

/// A `client request` says how it went, the path its connection takes and
/// what kind of app asked, and never whose session it was nor what it
/// asked for (P3-14).
#[tokio::test]
async fn a_client_request_says_how_it_went_and_from_what_never_who() {
    if !support::alone("telemetry::a_client_request_says_how_it_went_and_from_what_never_who") {
        return;
    }
    let traced = Traced::start("trace-request").await;
    traced.server.write_media(4096);
    let (client, session) = traced.alice().await;
    let handle = start_playback(&client, &session).await;
    // One that fails: a seek in a playback with no conversion.
    let seeked = client
        .call(
            Some(&session),
            Request::SeekPlayback {
                handle: handle.clone(),
                position_ms: 987_654_321,
            },
        )
        .await;
    assert!(matches!(seeked, Response::Err(_)), "{seeked:?}");

    let spans = traced.spans().await;
    let start = request(&spans, "start_playback");
    for (key, value) in [
        ("outcome", "ok"),
        ("client.platform", "linux"),
        ("client.name", "test"),
        (
            "protocol.version",
            &spoolframe_protocol::PROTOCOL_VERSION.to_string(),
        ),
    ] {
        assert_eq!(
            attribute(&start.attributes, key).as_deref(),
            Some(value),
            "{key}: {start:?}"
        );
    }
    let path = attribute(&start.attributes, "connection.path").expect("its path");
    assert!(
        ["lan", "direct", "relay", "unknown"].contains(&path.as_str()),
        "{path}"
    );
    // Its Hello's span says what said it, once it has.
    assert_eq!(
        attribute(&request(&spans, "hello").attributes, "client.platform").as_deref(),
        Some("linux")
    );
    assert_eq!(
        attribute(&request(&spans, "seek_playback").attributes, "outcome").as_deref(),
        Some("not_found")
    );
    // Nothing exported names the user, the session, the handle or what was
    // asked.
    let exported = format!("{spans:?}");
    for secret in [
        session.0.as_str(),
        handle.0.as_str(),
        "alice",
        "correct horse",
        "987654321",
    ] {
        assert!(!exported.contains(secret), "{secret} in {exported}");
    }
}

/// A request that reads the catalog says on its span whether the kept
/// replies had what it read: the first read of an item asks Plex, the
/// next finds it kept. No span is made for the read itself.
#[tokio::test]
async fn a_catalog_read_says_whether_the_kept_replies_had_it() {
    if !support::alone("telemetry::a_catalog_read_says_whether_the_kept_replies_had_it") {
        return;
    }
    use spoolframe_plex::PlexCatalog;
    use spoolframe_server::catalog::{Composite, LiveCatalog};
    use spoolframe_types::{MediaId, MediaKind, Namespace};
    let pms = crate::watch::pms_with_a_show().await;
    let plex = PlexCatalog::new(pms.uri().parse().unwrap(), None).unwrap();
    let live = LiveCatalog::new(Some(Arc::new(Composite::single("fulcrum-ng", plex))));
    let traced = Traced::start_with(
        "trace-cache-hit",
        ServerOptions {
            catalog: Some(Arc::new(live)),
            ..ServerOptions::default()
        },
    )
    .await;
    let (client, session) = traced.alice().await;
    let episode = MediaId::new(Namespace::Plex, MediaKind::Episode, "211").unwrap();
    for _ in 0..2 {
        let read = client
            .call(
                Some(&session),
                Request::Item {
                    id: episode.clone(),
                },
            )
            .await;
        assert!(matches!(read, Response::Ok(Reply::Item { .. })), "{read:?}");
    }
    traced
        .until_spans("both item requests", |spans| {
            named(spans, "client request")
                .iter()
                .filter(|s| attribute(&s.attributes, "request").as_deref() == Some("item"))
                .count()
                == 2
        })
        .await;
    let spans = traced.spans().await;
    let hits: Vec<_> = named(&spans, "client request")
        .into_iter()
        .filter(|s| attribute(&s.attributes, "request").as_deref() == Some("item"))
        .map(|s| attribute(&s.attributes, "cache.hit"))
        .collect();
    assert_eq!(hits, [Some("false".into()), Some("true".into())]);
    assert!(named(&spans, "cache read").is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_no_endpoint_nothing_is_exported_and_turning_it_off_stops_it() {
    let (addr, received) = http_collector().await;
    let (telemetry, subscriber) = telemetry(Arc::new(Metrics::default()), timings());
    let dispatch = tracing::Dispatch::new(subscriber);
    telemetry.apply(None);
    assert!(telemetry.status().is_none());
    tracing::dispatcher::with_default(&dispatch, activity);
    // On, then off again: a live setting both ways.
    telemetry.apply(Some(settings(format!("http://{addr}"), Protocol::Http)));
    telemetry.apply(None);
    tracing::dispatcher::with_default(&dispatch, activity);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(received.traces.lock().is_empty());
    assert!(received.logs.lock().is_empty());
}
