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
use wiremock::matchers::{header, method, path};
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
        !cookie.contains("Secure") && !cookie.contains("HttpOnly"),
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
        root.plex_check_paths(None).await.unwrap()[0].library,
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
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": 77, "code": "4F7K", "expiresIn": 900, "authToken": null
        })))
        .mount(&tv)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v2/pins/77"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 77, "code": "4F7K", "authToken": null
        })))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&tv)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v2/pins/77"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 77, "code": "4F7K", "authToken": "tok-123"
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
    assert_eq!(pin.code, "4F7K");
    assert!(
        pin.auth_url
            .starts_with("https://app.example/auth#?clientID="),
        "{}",
        pin.auth_url
    );
    assert_eq!(pin.expires_in, Some(900));
    assert_eq!(
        root.plex_pin_check(pin.id).await.unwrap(),
        PlexPinState::Waiting
    );
    assert_eq!(
        root.plex_pin_check(pin.id).await.unwrap(),
        PlexPinState::Linked {
            account: Some("andrey".into())
        }
    );
    // Kept as the secret, and used.
    let secret = std::fs::read_to_string(dir.as_ref().join("secrets/plex-token")).unwrap();
    assert_eq!(secret, "tok-123");
    let probed = root.plex_probe(pms.uri(), None).await.unwrap();
    assert!(probed.reachable, "{probed:?}");
    assert_eq!(probed.server_name.as_deref(), Some("Tower"));
    assert_eq!(probed.version.as_deref(), Some("1.41.0"));
    let settings = serde_json::to_string(&root.settings().await.unwrap()).unwrap();
    assert!(!settings.contains("tok-123"), "{settings}");
    // Only PINs this server began.
    assert_eq!(
        code(root.plex_pin_check(78).await.unwrap_err()),
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

    let unmapped = root.plex_path_check(vec![]).await.unwrap();
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

    let mapped = root.plex_path_check(vec![suggestion]).await.unwrap();
    assert_eq!((mapped[0].checked, mapped[0].ok), (1, 1));
    assert_eq!(mapped[0].suggestion, None);
    // Checked, not saved.
    assert!(!dir.as_ref().join("runtime.ucl").exists());
    let relative = root
        .plex_path_check(vec![PathMappingEntry {
            library: None,
            from: "data".into(),
            to: "/x".into(),
        }])
        .await
        .unwrap_err();
    assert_eq!(code(relative), error_code::INVALID_INPUT);
}
