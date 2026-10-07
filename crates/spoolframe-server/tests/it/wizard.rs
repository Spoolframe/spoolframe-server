//! The setup wizard's server side (P3-10): the setup code's page and its
//! insecure-cookies offer, progress kept over a restart, Sign in with Plex
//! against a stand-in plex.tv, a PMS probed before it is saved, and the
//! paths step's live check with its suggestions.

use std::net::SocketAddr;

use serde_json::json;
use spoolframe_admin_api::{
    AdminApiClient, MeApiClient, PathMappingEntry, PlexPinState, error_code,
};
use spoolframe_server::admin::Wizard;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::admin::{CODE, anonymous, code, setup, with_token};
use crate::settings::{pms, serve_in, served_with};

async fn post(addr: SocketAddr, path: &str, body: serde_json::Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://{addr}{path}"))
        .header("origin", format!("http://{addr}"))
        .json(&body)
        .send()
        .await
        .unwrap()
}

/// The first admin, made as the wizard's first page and step 1 make it:
/// a session for the rest.
async fn root(addr: SocketAddr) -> jsonrpsee::ws_client::WsClient {
    let token = anonymous(addr)
        .await
        .setup(setup(CODE, "root", "correct horse"))
        .await
        .unwrap()
        .token;
    with_token(addr, &token).await
}

#[tokio::test]
async fn the_setup_code_page_turns_insecure_cookies_on_for_the_admin_made_next() {
    let (addr, dir) = served_with("setup-check", "", Wizard::default()).await;
    let wrong = post(
        addr,
        "/auth/setup/check",
        json!({ "code": "AAAA-BBBB-CCCC", "insecureCookies": true }),
    )
    .await;
    assert_eq!(wrong.status(), 403);
    assert!(!dir.as_ref().join("runtime.ucl").exists());

    let right = post(
        addr,
        "/auth/setup/check",
        json!({ "code": CODE, "insecureCookies": true }),
    )
    .await;
    assert!(right.status().is_success(), "{right:?}");
    let made = post(
        addr,
        "/auth/setup",
        json!({ "code": CODE, "username": "root", "password": "correct horse" }),
    )
    .await;
    let cookie = made.headers()["set-cookie"].to_str().unwrap().to_owned();
    assert!(
        !cookie.contains("Secure") && cookie.contains("HttpOnly"),
        "{cookie}"
    );
    let saved = std::fs::read_to_string(dir.as_ref().join("runtime.ucl")).unwrap();
    assert!(saved.contains("insecure_cookies = true;"), "{saved}");

    // Set up now: the page has nothing more to check.
    let again = post(
        addr,
        "/auth/setup/check",
        json!({ "code": CODE, "insecureCookies": false }),
    )
    .await;
    assert_eq!(again.status(), 409);
}

/// Behind an HTTPS proxy the setup code cannot turn insecure cookies on
/// (PX-11 finding 32): they would only downgrade the cookie.
#[tokio::test]
async fn the_setup_code_page_cannot_turn_insecure_cookies_on_behind_https() {
    let (addr, dir) = served_with(
        "setup-check-https",
        r#"admin { public_url = "https://media.example.org"; }"#,
        Wizard::default(),
    )
    .await;
    let refused = post(
        addr,
        "/auth/setup/check",
        json!({ "code": CODE, "insecureCookies": true }),
    )
    .await;
    assert_eq!(refused.status(), 400);
    assert!(!dir.as_ref().join("runtime.ucl").exists());
    // Without them, the code is still checked as right.
    let right = post(
        addr,
        "/auth/setup/check",
        json!({ "code": CODE, "insecureCookies": false }),
    )
    .await;
    assert!(right.status().is_success(), "{right:?}");
}

#[tokio::test]
async fn a_wizard_stopped_after_the_plex_step_resumes_there_after_a_restart() {
    let pms = pms().await;
    let (addr, dir) = served_with("resume", "", Wizard::default()).await;
    let root = root(addr).await;
    let fresh = root.setup_state().await.unwrap();
    assert!(!fresh.finished);
    for step in ["admin", "server"] {
        root.setup_step_done(step.into()).await.unwrap();
    }
    root.setting_set("plex.url".into(), json!(pms.uri()))
        .await
        .unwrap();
    root.setup_step_done("plex".into()).await.unwrap();

    // A new server on the same state: the settings and the progress are
    // there, and it serves Plex at once.
    let again = serve_in(dir.as_ref(), "", Wizard::default()).await;
    let token = anonymous(again)
        .await
        .login(crate::admin::creds("root", "correct horse"))
        .await
        .unwrap()
        .token;
    let root = with_token(again, &token).await;
    let state = root.setup_state().await.unwrap();
    assert_eq!(state.done, ["admin", "server", "plex"]);
    assert!(!state.finished);
    assert_eq!(
        root.plex_check_paths(None).await.unwrap().reports[0].library,
        "Movies"
    );
    let done = root.setup_finish().await.unwrap();
    assert!(done.finished);
}

/// A stand-in plex.tv: PIN 77, approved at the second check, for the
/// token `tok-123` of account `andrey`.
async fn plex_tv() -> MockServer {
    let tv = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/pins"))
        // Plex's sign-in page takes only a strong PIN.
        .and(query_param("strong", "true"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": 77, "code": "q3n8vbx2kd5w7rjh0tyc4mzpf", "expiresIn": 900, "authToken": null
        })))
        .mount(&tv)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v2/pins/77"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 77, "code": "q3n8vbx2kd5w7rjh0tyc4mzpf", "authToken": null
        })))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&tv)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v2/pins/77"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 77, "code": "q3n8vbx2kd5w7rjh0tyc4mzpf", "authToken": "tok-123"
        })))
        .with_priority(2)
        .mount(&tv)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v2/user"))
        .and(header("X-Plex-Token", "tok-123"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "username": "andrey" })))
        .mount(&tv)
        .await;
    tv
}

/// A PMS that answers only with the token `tok-123`.
async fn pms_with_token() -> MockServer {
    let pms = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .and(header("X-Plex-Token", "tok-123"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "MediaContainer": { "friendlyName": "Tower", "version": "1.41.0" }
        })))
        .mount(&pms)
        .await;
    Mock::given(method("GET"))
        .and(path("/library/sections"))
        .and(header("X-Plex-Token", "tok-123"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "MediaContainer": { "Directory": [] }
        })))
        .mount(&pms)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401))
        .with_priority(10)
        .mount(&pms)
        .await;
    pms
}

#[tokio::test]
async fn signing_in_with_plex_keeps_the_token_and_never_hands_it_out() {
    let tv = plex_tv().await;
    let pms = pms_with_token().await;
    let wizard = Wizard {
        plex_tv: tv.uri().parse().unwrap(),
        plex_app: "https://app.example".parse().unwrap(),
        ..Wizard::default()
    };
    let (addr, dir) = served_with("plex-pin", "", wizard).await;
    let root = root(addr).await;

    // Without a token, Plex refuses: an answer, not an error.
    let refused = root.plex_probe(pms.uri(), None).await.unwrap();
    assert!(!refused.reachable);
    assert!(refused.error.unwrap().contains("requires a token"));

    let pin = root.plex_pin_begin().await.unwrap();
    assert!(
        pin.auth_url
            .starts_with("https://app.example/auth#?clientID="),
        "{}",
        pin.auth_url
    );
    assert_eq!(pin.expires_in, Some(900));
    assert_eq!(
        root.plex_pin_check(pin.id, true).await.unwrap(),
        PlexPinState::Waiting
    );
    assert_eq!(
        root.plex_pin_check(pin.id, true).await.unwrap(),
        PlexPinState::Linked {
            account: Some("andrey".into())
        }
    );
    // Kept as the secret, and used for the account's own server, as
    // plex.tv lists it: never for a URL the account does not reach
    // (PX-09 B-2).
    let secret = std::fs::read_to_string(dir.as_ref().join("secrets/plex-token")).unwrap();
    assert_eq!(secret, "tok-123");
    let port = pms.address().port();
    Mock::given(method("GET"))
        .and(path("/api/v2/resources"))
        .and(header("X-Plex-Token", "tok-123"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "name": "Tower", "provides": "server", "clientIdentifier": "tower-machine",
              "owned": true, "accessToken": "tok-123",
              "connections": [
                { "uri": format!("http://127.0.0.1:{port}"),
                  "address": "127.0.0.1", "port": port, "local": true, "relay": false },
              ] },
        ])))
        .mount(&tv)
        .await;
    let probed = root.plex_probe(pms.uri(), None).await.unwrap();
    assert!(probed.reachable, "{probed:?}");
    assert_eq!(probed.server_name.as_deref(), Some("Tower"));
    assert_eq!(probed.version.as_deref(), Some("1.41.0"));
    let settings = serde_json::to_string(&root.settings().await.unwrap()).unwrap();
    assert!(!settings.contains("tok-123"), "{settings}");
    // Only PINs this server began.
    assert_eq!(
        code(root.plex_pin_check(78, true).await.unwrap_err()),
        error_code::INVALID_INPUT
    );
}

#[tokio::test]
async fn a_pasted_token_is_probed_before_it_is_kept() {
    let pms = pms_with_token().await;
    let (addr, _dir) = served_with("plex-paste", "", Wizard::default()).await;
    let root = root(addr).await;
    let wrong = root
        .plex_probe(pms.uri(), Some("nope".into()))
        .await
        .unwrap();
    assert!(wrong.error.unwrap().contains("rejected the token"));
    let right = root
        .plex_probe(pms.uri(), Some("tok-123".into()))
        .await
        .unwrap();
    assert!(right.reachable);
    assert_eq!(
        code(root.plex_probe("nas:32400".into(), None).await.unwrap_err()),
        error_code::INVALID_INPUT
    );
}

#[tokio::test]
async fn the_paths_step_checks_unsaved_mappings_and_suggests_one() {
    let pms = pms().await;
    let tmp = spoolframe_test_dir::TestDir::new("spoolframe-wizard-paths").await;
    let nas = tmp.as_ref().join("nas");
    std::fs::create_dir_all(nas.join("movies")).unwrap();
    std::fs::write(nas.join("movies/a.mkv"), b"movie").unwrap();
    let wizard = Wizard {
        path_bases: Some(vec![tmp.as_ref().join("empty"), nas.clone()]),
        ..Wizard::default()
    };
    let (addr, dir) = served_with(
        "wizard-paths",
        &format!(r#"plex {{ url = "{}"; }}"#, pms.uri()),
        wizard,
    )
    .await;
    let root = root(addr).await;

    let unmapped = root.plex_path_try(vec![], None, None).await.unwrap();
    let movies = &unmapped[0];
    assert_eq!(movies.roots, ["/data/movies"]);
    assert_eq!((movies.checked, movies.ok), (1, 0));
    let suggestion = movies.suggestion.clone().unwrap();
    assert_eq!(
        suggestion,
        PathMappingEntry {
            library: Some("Movies".into()),
            from: "/data".into(),
            to: nas.display().to_string(),
        }
    );

    let mapped = root
        .plex_path_try(vec![suggestion], None, None)
        .await
        .unwrap();
    assert_eq!((mapped[0].checked, mapped[0].ok), (1, 1));
    assert_eq!(mapped[0].suggestion, None);
    // Checked, not saved.
    assert!(!dir.as_ref().join("runtime.ucl").exists());
    let relative = root
        .plex_path_try(
            vec![PathMappingEntry {
                library: None,
                from: "data".into(),
                to: "/x".into(),
            }],
            None,
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(code(relative), error_code::INVALID_INPUT);
}

/// A stand-in PMS answering GDM on loopback, saying from which port each
/// search came.
async fn gdm_stand_in() -> (SocketAddr, tokio::sync::mpsc::UnboundedReceiver<u16>) {
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let at = socket.local_addr().unwrap();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut buf = [0u8; 256];
        loop {
            let (_, from) = socket.recv_from(&mut buf).await.unwrap();
            let _ = tx.send(from.port());
            let reply = "HTTP/1.0 200 OK\r\nContent-Type: plex/media-server\r\n\
                Resource-Identifier: abc123\r\nName: Tower\r\nPort: 32400\r\n\r\n";
            socket.send_to(reply.as_bytes(), from).await.unwrap();
        }
    });
    (at, rx)
}

#[tokio::test]
async fn discovery_searches_from_the_port_the_setting_names_as_soon_as_it_changes() {
    let (gdm, mut from) = gdm_stand_in().await;
    let wizard = Wizard {
        gdm: vec![gdm],
        ..Wizard::default()
    };
    let (addr, _dir) = served_with("gdm-port", "", wizard).await;
    let root = root(addr).await;
    let port = {
        let free = tokio::net::UdpSocket::bind("0.0.0.0:0").await.unwrap();
        free.local_addr().unwrap().port()
    };
    let entry = root
        .setting_set("network.plex_discovery_port".into(), json!(port))
        .await
        .unwrap();
    assert_eq!(entry.apply, spoolframe_admin_api::SettingApply::Live);
    let found = root.plex_discover().await.unwrap();
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].name, "Tower");
    // Plex's id for it, for the wizard to tell it from the same server
    // listed by the account.
    assert_eq!(found[0].machine_id.as_deref(), Some("abc123"));
    assert_eq!(from.recv().await, Some(port));
}

/// A PMS that answers only with `token`, as `name`.
async fn pms_wanting(name: &str, token: &str) -> MockServer {
    let pms = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .and(header("X-Plex-Token", token))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "MediaContainer": { "friendlyName": name, "version": "1.41.0" }
        })))
        .mount(&pms)
        .await;
    Mock::given(method("GET"))
        .and(path("/library/sections"))
        .and(header("X-Plex-Token", token))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "MediaContainer": { "Directory": [] }
        })))
        .mount(&pms)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401))
        .with_priority(10)
        .mount(&pms)
        .await;
    pms
}

#[tokio::test]
async fn after_signing_in_with_plex_the_wizard_offers_the_account_s_servers_with_their_tokens() {
    let tv = plex_tv().await;
    // Tower is the account's own, on the LAN; plex.tv names it by its
    // plex.direct address, and says which address that stands for.
    let tower = pms_wanting("Tower", "tok-tower").await;
    let port = tower.address().port();
    Mock::given(method("GET"))
        .and(path("/api/v2/resources"))
        .and(header("X-Plex-Token", "tok-123"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "name": "Tower", "provides": "server", "clientIdentifier": "tower-machine",
              "owned": true, "accessToken": "tok-tower",
              "connections": [
                { "uri": format!("https://127-0-0-1.abc.plex.direct:{port}"),
                  "address": "127.0.0.1", "port": port, "local": true, "relay": false },
              ] },
            { "name": "Cabin", "provides": "server", "clientIdentifier": "cabin-machine",
              "owned": false, "accessToken": "tok-cabin",
              "connections": [
                { "uri": "https://1-2-3-4.cabin.plex.direct:32400",
                  "address": "1.2.3.4", "port": 32400, "local": false, "relay": false },
              ] },
        ])))
        .mount(&tv)
        .await;
    let wizard = Wizard {
        plex_tv: tv.uri().parse().unwrap(),
        plex_app: "https://app.example".parse().unwrap(),
        ..Wizard::default()
    };
    let (addr, dir) = served_with("plex-pin-servers", "", wizard).await;
    let root = root(addr).await;
    // Nobody has signed in with Plex, and nobody has linked an account.
    assert_eq!(
        code(root.plex_account_servers().await.unwrap_err()),
        error_code::NOT_CONFIGURED
    );

    let pin = root.plex_pin_begin().await.unwrap();
    root.plex_pin_check(pin.id, true).await.unwrap();
    assert!(matches!(
        root.plex_pin_check(pin.id, true).await.unwrap(),
        PlexPinState::Linked { .. }
    ));
    let servers = root.plex_account_servers().await.unwrap();
    let shown = serde_json::to_string(&servers).unwrap();
    assert!(
        !shown.contains("tok-tower") && !shown.contains("tok-cabin"),
        "{shown}"
    );
    assert_eq!(servers.len(), 2, "{servers:?}");
    assert_eq!(servers[0].machine_id, "tower-machine");
    assert!(servers[0].local && servers[0].owned);
    assert_eq!(servers[1].name, "Cabin");
    assert!(!servers[1].local, "a remote one is marked");

    // At the address discovery finds it by, Tower answers with its own
    // token from plex.tv, not the account's that plex.token holds now.
    let lan = format!("http://127.0.0.1:{port}");
    let probed = root.plex_probe(lan.clone(), None).await.unwrap();
    assert!(probed.reachable, "{probed:?}");
    // Chosen: that token is the one kept.
    root.plex_account_token_use(lan).await.unwrap();
    let secret = std::fs::read_to_string(dir.as_ref().join("secrets/plex-token")).unwrap();
    assert_eq!(secret, "tok-tower");
    // The account does not reach this one.
    assert_eq!(
        code(
            root.plex_account_token_use("http://10.9.9.9:32400".into())
                .await
                .unwrap_err()
        ),
        error_code::INVALID_INPUT
    );
}

#[tokio::test]
async fn signing_in_with_plex_to_add_another_server_takes_its_own_token_and_keeps_the_first_s() {
    let tv = plex_tv().await;
    let tower = pms_wanting("Tower", "tok-first").await;
    // Cabin is a server shared with the account: only its own token from
    // plex.tv opens it, not the account's and not Tower's.
    let cabin = pms_wanting("Cabin", "tok-cabin").await;
    let port = cabin.address().port();
    Mock::given(method("GET"))
        .and(path("/api/v2/resources"))
        .and(header("X-Plex-Token", "tok-123"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "name": "Cabin", "provides": "server", "clientIdentifier": "cabin-machine",
              "owned": false, "accessToken": "tok-cabin",
              "connections": [
                { "uri": format!("https://127-0-0-1.cabin.plex.direct:{port}"),
                  "address": "127.0.0.1", "port": port, "local": true, "relay": false },
              ] },
        ])))
        .mount(&tv)
        .await;
    let wizard = Wizard {
        plex_tv: tv.uri().parse().unwrap(),
        plex_app: "https://app.example".parse().unwrap(),
        ..Wizard::default()
    };
    let (addr, dir) = served_with(
        "plex-pin-another",
        &format!(r#"plex {{ url = "{}"; }}"#, tower.uri()),
        wizard,
    )
    .await;
    let root = root(addr).await;
    root.setting_set("plex.token".into(), json!("tok-first"))
        .await
        .unwrap();

    let pin = root.plex_pin_begin().await.unwrap();
    root.plex_pin_check(pin.id, false).await.unwrap();
    assert_eq!(
        root.plex_pin_check(pin.id, false).await.unwrap(),
        PlexPinState::Linked {
            account: Some("andrey".into())
        }
    );
    // The first server keeps its token.
    let first = std::fs::read_to_string(dir.as_ref().join("secrets/plex-token")).unwrap();
    assert_eq!(first, "tok-first");
    assert!(root.plex_status().await.unwrap().reachable);

    // The account's servers are offered, and the one chosen is probed and
    // added with the token plex.tv has for it.
    let servers = root.plex_account_servers().await.unwrap();
    assert_eq!(servers.len(), 1, "{servers:?}");
    assert_eq!(servers[0].name, "Cabin");
    let lan = format!("http://127.0.0.1:{port}");
    let probed = root.plex_probe(lan.clone(), None).await.unwrap();
    assert!(probed.reachable, "{probed:?}");
    let added = root.plex_server_add(lan, None).await.unwrap();
    assert_eq!(added.name, "Cabin");
    assert!(added.status.reachable, "{:?}", added.status);
    let kept = std::fs::read_to_string(
        dir.as_ref()
            .join(format!("secrets/plex-token-{}", added.id)),
    )
    .unwrap();
    assert_eq!(kept, "tok-cabin");
    let first = std::fs::read_to_string(dir.as_ref().join("secrets/plex-token")).unwrap();
    assert_eq!(first, "tok-first");
}

/// A stand-in PMS that answers anyone, token or not, and keeps what it
/// was sent: a server found on the LAN, which may be anyone's.
async fn stranger() -> MockServer {
    let pms = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "MediaContainer": { "friendlyName": "Stranger", "version": "1.41.0" }
        })))
        .mount(&pms)
        .await;
    Mock::given(method("GET"))
        .and(path("/library/sections"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "MediaContainer": { "Directory": [] }
        })))
        .mount(&pms)
        .await;
    pms
}

/// The tokens `pms` was sent, in order.
async fn tokens_sent(pms: &MockServer) -> Vec<String> {
    pms.received_requests()
        .await
        .unwrap()
        .iter()
        .filter_map(|r| r.headers.get("x-plex-token"))
        .map(|v| v.to_str().unwrap().to_owned())
        .collect()
}

/// PX-09 finding 5 (B-2): probing or adding a server no one gave a token
/// for sends none, not the first server's; the first server is still
/// probed with its own.
#[tokio::test]
async fn a_server_without_a_token_of_its_own_is_never_sent_another_s() {
    let tower = pms_wanting("Tower", "tok-first").await;
    let (addr, dir) = served_with(
        "plex-no-token-fallback",
        &format!(r#"plex {{ url = "{}"; }}"#, tower.uri()),
        Wizard::default(),
    )
    .await;
    let root = root(addr).await;
    root.setting_set("plex.token".into(), json!("tok-first"))
        .await
        .unwrap();
    let stranger = stranger().await;

    root.plex_probe(stranger.uri(), None).await.unwrap();
    assert_eq!(tokens_sent(&stranger).await, Vec::<String>::new());
    let added = root.plex_server_add(stranger.uri(), None).await.unwrap();
    assert_eq!(tokens_sent(&stranger).await, Vec::<String>::new());
    assert!(
        !dir.as_ref()
            .join(format!("secrets/plex-token-{}", added.id))
            .exists()
    );

    // The first server, by its own URL, with its own token.
    let first = root.plex_probe(tower.uri(), None).await.unwrap();
    assert!(first.reachable, "{first:?}");
}
