//! OTLP export (P3-14) against in-process stand-in collectors: one over
//! HTTP/protobuf, one over gRPC, and one that never answers. Nothing here
//! reaches a real collector.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
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
use opentelemetry_proto::tonic::common::v1::any_value;
use opentelemetry_proto::tonic::resource::v1::Resource;
use prost::Message as _;
use spoolframe_server::metrics::Metrics;
use spoolframe_server::network::ServerName;
use spoolframe_server::telemetry::{
    OtelLayer, OtlpSettings, Protocol, Signal, Swap, Telemetry, Timings,
};
use tracing_subscriber::layer::SubscriberExt as _;

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
                r.traces.lock().unwrap().push(request);
                protobuf_ok()
            }),
        )
        .route(
            "/v1/metrics",
            post(|State(r): State<Received>, body: Bytes| async move {
                let request = ExportMetricsServiceRequest::decode(body).unwrap();
                r.metrics.lock().unwrap().push(request);
                protobuf_ok()
            }),
        )
        .route(
            "/v1/logs",
            post(|State(r): State<Received>, body: Bytes| async move {
                let request = ExportLogsServiceRequest::decode(body).unwrap();
                r.logs.lock().unwrap().push(request);
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
        self.0.traces.lock().unwrap().push(request.into_inner());
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

/// An export for a server called Den, and the subscriber whose slot it
/// fills: used on the test's thread only, so nothing else is exported.
fn telemetry(
    metrics: Arc<Metrics>,
    timings: Timings,
) -> (Telemetry, impl tracing::Subscriber + Send + Sync) {
    let (slot, handle) = tracing_subscriber::reload::Layer::new(None::<OtelLayer>);
    // Gone with the test's subscriber, once it has done its part.
    let swap: Swap = Box::new(move |layer| {
        let _ = handle.reload(layer);
    });
    let subscriber = tracing_subscriber::registry().with(slot);
    let telemetry = Telemetry::with_timings(
        swap,
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

/// A playback start's span, and a warning in it.
fn activity() {
    let span = tracing::info_span!("client request", request = "StartPlayback");
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
        !received.traces.lock().unwrap().is_empty()
            && !received.logs.lock().unwrap().is_empty()
            && !received.metrics.lock().unwrap().is_empty()
    })
    .await;

    let traces = received.traces.lock().unwrap().clone();
    let spans = &traces[0].resource_spans[0];
    assert_eq!(attributes(spans.resource.as_ref()), expected_resource());
    let names: Vec<_> = spans
        .scope_spans
        .iter()
        .flat_map(|s| &s.spans)
        .map(|s| s.name.as_str())
        .collect();
    assert!(names.contains(&"client request"), "{names:?}");

    let logs = received.logs.lock().unwrap().clone();
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

    let exported = received.metrics.lock().unwrap().clone();
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
        !received.logs.lock().unwrap().is_empty()
    })
    .await;
    let logs = format!("{:?}", received.logs.lock().unwrap());
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
    until("a span arrives", || {
        !received.traces.lock().unwrap().is_empty()
    })
    .await;
    let traces = received.traces.lock().unwrap().clone();
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
    assert!(received.traces.lock().unwrap().is_empty());
    assert!(received.logs.lock().unwrap().is_empty());
}
