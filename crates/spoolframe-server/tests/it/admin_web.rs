//! The admin listener's browser side (P3-01): signing in over HTTP, the
//! session cookie on the JSON-RPC socket, and the origin check.

use std::net::SocketAddr;

use jsonrpsee::core::ClientError;
use jsonrpsee::ws_client::{HeaderMap, WsClient, WsClientBuilder};
use reqwest::StatusCode;
use serde_json::{Value, json};
use spoolframe_admin_api::{AdminApiClient, MeApiClient, NewUser, Role, error_code};

use crate::admin::{CODE, anonymous, server, server_behind_a_proxy};

const PASSWORD: &str = "correct horse";

/// What a browser on the listener's own page sends.
fn page(addr: SocketAddr) -> String {
    format!("http://{addr}")
}

async fn post(
    addr: SocketAddr,
    path: &str,
    origin: Option<&str>,
    cookie: Option<&str>,
    body: Value,
) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("http://{addr}{path}"))
        .json(&body);
    if let Some(origin) = origin {
        request = request.header("origin", origin);
    }
    if let Some(cookie) = cookie {
        request = request.header("cookie", cookie);
    }
    request.send().await.unwrap()
}

/// The session cookie a response set, as a browser sends it back.
fn cookie(response: &reqwest::Response) -> String {
    let set = response
        .headers()
        .get("set-cookie")
        .expect("a cookie")
        .to_str()
        .unwrap();
    for part in ["HttpOnly", "Secure", "SameSite=Strict"] {
        assert!(set.contains(part), "{set}");
    }
    set.split(';').next().unwrap().to_owned()
}

async fn me(addr: SocketAddr, cookie: &str) -> reqwest::Response {
    reqwest::Client::new()
        .get(format!("http://{addr}/auth/me"))
        .header("cookie", cookie)
        .send()
        .await
        .unwrap()
}

/// The web UI's socket: `/jsonrpc`, the cookie, the page's origin.
async fn socket(addr: SocketAddr, cookie: &str, origin: &str) -> Result<WsClient, ClientError> {
    let mut headers = HeaderMap::new();
    headers.insert("cookie", cookie.parse().unwrap());
    headers.insert("origin", origin.parse().unwrap());
    WsClientBuilder::default()
        .set_headers(headers)
        .build(format!("ws://{addr}/jsonrpc"))
        .await
}

fn call_code(err: ClientError) -> i32 {
    match err {
        ClientError::Call(e) => e.code(),
        other => panic!("not a call error: {other}"),
    }
}

/// The first admin, set up from a browser: their cookie.
async fn set_up(addr: SocketAddr) -> String {
    let response = post(
        addr,
        "/auth/setup",
        Some(&page(addr)),
        None,
        json!({ "code": CODE, "username": "root", "password": PASSWORD }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    cookie(&response)
}

async fn setup_open(addr: SocketAddr) -> bool {
    let body: Value = reqwest::get(format!("http://{addr}/auth/setup"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    body["open"].as_bool().unwrap()
}

#[tokio::test]
async fn a_browser_sets_up_the_first_admin_and_uses_the_socket_with_its_cookie() {
    let (addr, _dir) = server("web-setup").await;
    assert!(setup_open(addr).await);
    let root = set_up(addr).await;
    assert!(!setup_open(addr).await);
    let who: Value = me(addr, &root).await.json().await.unwrap();
    assert_eq!(who["username"], "root");
    assert_eq!(who["role"], "admin");

    let api = socket(addr, &root, &page(addr)).await.unwrap();
    assert_eq!(api.user_list().await.unwrap().len(), 1);

    // Once there is a user, setup is refused.
    let again = post(
        addr,
        "/auth/setup",
        Some(&page(addr)),
        None,
        json!({ "code": CODE, "username": "other", "password": PASSWORD }),
    )
    .await;
    assert_eq!(again.status(), StatusCode::CONFLICT);
    let body: Value = again.json().await.unwrap();
    assert_eq!(body["code"], error_code::ALREADY_SET_UP);
}

#[tokio::test]
async fn setup_needs_the_code_from_the_log() {
    let (addr, _dir) = server("web-setup-code").await;
    let response = post(
        addr,
        "/auth/setup",
        Some(&page(addr)),
        None,
        json!({ "code": "AAAA-AAAA-AAAA", "username": "root", "password": PASSWORD }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(response.headers().get("set-cookie").is_none());
}

#[tokio::test]
async fn logging_in_and_out_from_a_browser() {
    let (addr, _dir) = server("web-login").await;
    let root = set_up(addr).await;
    let wrong = post(
        addr,
        "/auth/login",
        Some(&page(addr)),
        None,
        json!({ "username": "root", "password": "nope nope" }),
    )
    .await;
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

    let login = post(
        addr,
        "/auth/login",
        Some(&page(addr)),
        None,
        json!({ "username": "root", "password": PASSWORD }),
    )
    .await;
    assert_eq!(login.status(), StatusCode::OK);
    let session = cookie(&login);
    assert_eq!(me(addr, &session).await.status(), StatusCode::OK);

    let out = post(
        addr,
        "/auth/logout",
        Some(&page(addr)),
        Some(&session),
        json!({}),
    )
    .await;
    assert_eq!(out.status(), StatusCode::NO_CONTENT);
    assert!(
        out.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );
    // Revoked: the old cookie is worth nothing, on HTTP or the socket.
    assert_eq!(me(addr, &session).await.status(), StatusCode::UNAUTHORIZED);
    let api = socket(addr, &session, &page(addr)).await.unwrap();
    assert_eq!(
        call_code(api.who_am_i().await.unwrap_err()),
        error_code::UNAUTHENTICATED
    );
    // Only that session: setup's is still live.
    assert_eq!(me(addr, &root).await.status(), StatusCode::OK);
}

#[tokio::test]
async fn another_site_cannot_sign_in_or_open_the_socket() {
    let (addr, _dir) = server("web-origin").await;
    let root = set_up(addr).await;
    let evil = Some("https://evil.example");
    let login = post(
        addr,
        "/auth/login",
        evil,
        None,
        json!({ "username": "root", "password": PASSWORD }),
    )
    .await;
    assert_eq!(login.status(), StatusCode::FORBIDDEN);
    let logout = post(addr, "/auth/logout", evil, Some(&root), json!({})).await;
    assert_eq!(logout.status(), StatusCode::FORBIDDEN);
    assert!(socket(addr, &root, "https://evil.example").await.is_err());
    // Still signed in: the forged logout did nothing.
    assert_eq!(me(addr, &root).await.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_consumer_s_cookie_gets_no_admin_methods() {
    let (addr, _dir) = server("web-consumer").await;
    let root = set_up(addr).await;
    let api = socket(addr, &root, &page(addr)).await.unwrap();
    api.user_add(NewUser {
        username: "bob".into(),
        display_name: None,
        password: PASSWORD.into(),
        role: Role::Consumer,
    })
    .await
    .unwrap();
    let login = post(
        addr,
        "/auth/login",
        Some(&page(addr)),
        None,
        json!({ "username": "bob", "password": PASSWORD }),
    )
    .await;
    let bob = cookie(&login);
    let who: Value = me(addr, &bob).await.json().await.unwrap();
    assert_eq!(who["role"], "consumer");
    let api = socket(addr, &bob, &page(addr)).await.unwrap();
    assert_eq!(
        call_code(api.user_list().await.unwrap_err()),
        error_code::FORBIDDEN
    );
}

#[tokio::test]
async fn without_a_cookie_there_is_no_one() {
    let (addr, _dir) = server("web-anonymous").await;
    let response = reqwest::get(format!("http://{addr}/auth/me"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    // The CLI's socket, at the root, without an Origin, still answers.
    let cli = anonymous(addr).await;
    assert_eq!(
        call_code(cli.who_am_i().await.unwrap_err()),
        error_code::UNAUTHENTICATED
    );
}

/// A login as the proxy passes it on from the caller at `from`.
async fn login_from(addr: SocketAddr, from: &str, username: &str, password: &str) -> StatusCode {
    reqwest::Client::new()
        .post(format!("http://{addr}/auth/login"))
        .header("x-forwarded-for", from)
        .json(&json!({ "username": username, "password": password }))
        .send()
        .await
        .unwrap()
        .status()
}

/// Behind a trusted proxy every connection comes from the proxy's address.
/// The failure limit counts the caller the proxy names: one caller's
/// failures do not lock out the others behind it.
#[tokio::test]
async fn behind_a_trusted_proxy_the_failure_limit_counts_each_caller() {
    let (addr, _dir) = server_behind_a_proxy("web-proxy-source").await;
    set_up(addr).await;
    for n in 0..5 {
        let status = login_from(addr, "203.0.113.1", &format!("guess{n}"), "wrong").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    assert_eq!(
        login_from(addr, "203.0.113.1", "root", PASSWORD).await,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        login_from(addr, "203.0.113.2", "root", PASSWORD).await,
        StatusCode::OK
    );
}
