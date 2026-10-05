//! `GET /metrics` on the admin listener (P3-14): off by default, and when
//! on, only for the addresses `admin.metrics_from` names.

use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::StatusCode;
use spoolframe_server::admin::{self, Admin, Origins, TrustedProxies};
use spoolframe_server::auth::Auth;
use spoolframe_server::metrics::Metrics;
use spoolframe_server::store::Store;

use crate::admin::{admin, state_dir};

/// An admin listener with `metrics`, believing a proxy on loopback about
/// whom it carries when `proxied`.
async fn serve(
    test: &str,
    metrics: Option<Arc<Metrics>>,
    proxied: bool,
) -> (SocketAddr, spoolframe_test_dir::TestDir) {
    let dir = state_dir(test).await;
    let auth = Auth::new(Store::open(dir.as_ref()).await.unwrap());
    let proxies = if proxied {
        "127.0.0.1".parse().unwrap()
    } else {
        TrustedProxies::default()
    };
    let (addr, handle) = admin::serve(
        "127.0.0.1:0".parse().unwrap(),
        Admin {
            metrics,
            ..admin(auth, None)
        },
        Origins::default(),
        proxies,
    )
    .await
    .unwrap();
    std::mem::forget(handle);
    (addr, dir)
}

fn on(from: &str) -> Arc<Metrics> {
    let metrics = Arc::new(Metrics::default());
    metrics.access().set(true, from.parse().unwrap());
    metrics
}

async fn get(addr: SocketAddr, forwarded_for: Option<&str>) -> reqwest::Response {
    let mut request = reqwest::Client::new().get(format!("http://{addr}/metrics"));
    if let Some(ip) = forwarded_for {
        request = request.header("x-forwarded-for", ip);
    }
    request.send().await.unwrap()
}

#[tokio::test]
async fn metrics_are_not_found_while_off() {
    let (addr, _dir) = serve("metrics-off", Some(Arc::new(Metrics::default())), false).await;
    assert_eq!(get(addr, None).await.status(), StatusCode::NOT_FOUND);
    let (addr, _dir) = serve("metrics-none", None, false).await;
    assert_eq!(get(addr, None).await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn loopback_reads_prometheus_text() {
    let (addr, _dir) = serve("metrics-loopback", Some(on("127.0.0.0/8, ::1")), false).await;
    let response = get(addr, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"],
        "text/plain; version=0.0.4; charset=utf-8"
    );
    let text = response.text().await.unwrap();
    assert!(
        text.contains("# TYPE spoolframe_process_uptime_seconds gauge\n"),
        "{text}"
    );
}

#[tokio::test]
async fn an_address_not_listed_is_refused() {
    let (addr, _dir) = serve("metrics-elsewhere", Some(on("10.0.0.0/8")), false).await;
    assert_eq!(get(addr, None).await.status(), StatusCode::FORBIDDEN);
}

/// A proxy on this machine carries the Internet: what it forwards for is
/// the caller, not loopback.
#[tokio::test]
async fn behind_a_trusted_proxy_the_forwarded_address_is_the_caller() {
    let (addr, _dir) = serve(
        "metrics-proxied",
        Some(on("127.0.0.0/8, ::1, 198.51.100.0/24")),
        true,
    )
    .await;
    assert_eq!(
        get(addr, Some("192.0.2.7")).await.status(),
        StatusCode::FORBIDDEN
    );
    // Nobody named: nothing says who it is.
    assert_eq!(get(addr, None).await.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        get(addr, Some("198.51.100.9")).await.status(),
        StatusCode::OK
    );
}
