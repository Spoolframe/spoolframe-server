//! Sign in with Plex per user (P3-12): each user links their own Plex
//! account with plex.tv's PIN flow, against a stand-in plex.tv; the
//! config's PX-04 owner is linked as before; an admin sees and unlinks;
//! the owner's account lists its servers for adding libraries.

use std::net::SocketAddr;

use jsonrpsee::ws_client::WsClient;
use serde_json::json;
use spoolframe_admin_api::{
    AdminApiClient, MeApiClient, NewUser, PlexLink, PlexLinkState, Role, error_code,
};
use spoolframe_server::admin::Wizard;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::admin::{CODE, anonymous, code, creds, setup, with_token};
use crate::settings::{serve_in, served_with};

/// A stand-in plex.tv: PIN `pin` approved at the second check, for
/// `token`, the account `username`.
pub(crate) async fn plex_tv(pin: u64, token: &str, username: &str) -> MockServer {
    let tv = MockServer::start().await;
    mount_pin(&tv, pin, token, username).await;
    tv
}

pub(crate) async fn mount_pin(tv: &MockServer, pin: u64, token: &str, username: &str) {
    Mock::given(method("POST"))
        .and(path("/api/v2/pins"))
        // Plex's sign-in page takes only a strong PIN.
        .and(query_param("strong", "true"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": pin, "code": "q3n8vbx2kd5w7rjh0tyc4mzpf", "expiresIn": 900, "authToken": null
        })))
        .up_to_n_times(1)
        .mount(tv)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/v2/pins/{pin}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": pin, "code": "q3n8vbx2kd5w7rjh0tyc4mzpf", "authToken": null
        })))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(tv)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/v2/pins/{pin}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": pin, "code": "q3n8vbx2kd5w7rjh0tyc4mzpf", "authToken": token
        })))
        .with_priority(2)
        .mount(tv)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v2/user"))
        .and(header("X-Plex-Token", token))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "username": username, "email": format!("{username}@example.com")
        })))
        .mount(tv)
        .await;
}

pub(crate) fn reaching(tv: &MockServer) -> Wizard {
    Wizard {
        plex_tv: tv.uri().parse().unwrap(),
        plex_app: "https://app.example".parse().unwrap(),
        ..Wizard::default()
    }
}

pub(crate) async fn signed_in(addr: SocketAddr, username: &str, password: &str) -> WsClient {
    let token = anonymous(addr)
        .await
        .login(creds(username, password))
        .await
        .unwrap()
        .token;
    with_token(addr, &token).await
}

/// The first admin, `root`, and a consumer, `alice`, each signed in.
pub(crate) async fn root_and_alice(addr: SocketAddr) -> (WsClient, WsClient) {
    let token = anonymous(addr)
        .await
        .setup(setup(CODE, "root", "correct horse"))
        .await
        .unwrap()
        .token;
    let root = with_token(addr, &token).await;
    root.user_add(NewUser {
        username: "alice".into(),
        display_name: None,
        password: "alice's horse".into(),
        role: Role::Consumer,
    })
    .await
    .unwrap();
    (root, signed_in(addr, "alice", "alice's horse").await)
}

/// Link `client`'s Plex account through the PIN flow; the link.
pub(crate) async fn link(client: &WsClient) -> PlexLink {
    let pin = client.plex_link_begin().await.unwrap();
    assert!(
        pin.auth_url
            .starts_with("https://app.example/auth#?clientID="),
        "{}",
        pin.auth_url
    );
    assert!(
        pin.auth_url.contains("code=q3n8vbx2kd5w7rjh0tyc4mzpf"),
        "{}",
        pin.auth_url
    );
    assert_eq!(
        client.plex_link_check(pin.id).await.unwrap(),
        PlexLinkState::Waiting
    );
    match client.plex_link_check(pin.id).await.unwrap() {
        PlexLinkState::Linked { link } => link,
        other => panic!("not linked: {other:?}"),
    }
}

/// The one user's Plex link secret under the state directory, if any:
/// `secrets/plex-account-<user id>`, the id a UUID (P3-20).
pub(crate) fn secret(dir: &std::path::Path) -> Option<String> {
    let Ok(entries) = std::fs::read_dir(dir.join("secrets")) else {
        return None;
    };
    let mut found: Vec<_> = entries
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("plex-account-")
        })
        .collect();
    assert!(found.len() <= 1, "one user links here: {found:?}");
    let path = found.pop()?;
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    let id = name.strip_prefix("plex-account-").unwrap();
    assert!(
        id.len() == 36 && id.split('-').count() == 5,
        "named by a UUID: {name}"
    );
    Some(std::fs::read_to_string(path).unwrap())
}

/// `forwardUrl` in Plex's sign-in page `auth_url`, if it has one.
pub(crate) fn forward_url(auth_url: &str) -> Option<String> {
    let (_, fragment) = auth_url.split_once("#?")?;
    url::form_urlencoded::parse(fragment.as_bytes())
        .find(|(k, _)| k == "forwardUrl")
        .map(|(_, v)| v.into_owned())
}

/// Signed in as `token` from a browser that has the web UI open at its
/// own address: the socket's `Origin` is the page's.
pub(crate) async fn from_browser(addr: SocketAddr, token: &str) -> WsClient {
    let mut headers = jsonrpsee::ws_client::HeaderMap::new();
    headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
    headers.insert("origin", format!("http://{addr}").parse().unwrap());
    jsonrpsee::ws_client::WsClientBuilder::default()
        .set_headers(headers)
        .build(format!("ws://{addr}"))
        .await
        .unwrap()
}

/// Plex's sign-in ends on the web UI's page that the browser has open,
/// rather than on "We were unable to complete this request"; the CLI,
/// which has no page, gets none.
#[tokio::test]
async fn plex_s_sign_in_goes_back_to_the_web_ui_the_browser_has_open() {
    let tv = plex_tv(77, "tok-root", "root").await;
    mount_pin(&tv, 78, "tok-root", "root").await;
    mount_pin(&tv, 79, "tok-root", "root").await;
    let (addr, _dir) = served_with("plex-forward", "", reaching(&tv)).await;
    let token = anonymous(addr)
        .await
        .setup(setup(CODE, "root", "correct horse"))
        .await
        .unwrap()
        .token;

    let browser = from_browser(addr, &token).await;
    let pin = browser.plex_link_begin().await.unwrap();
    assert_eq!(
        forward_url(&pin.auth_url).as_deref(),
        Some(format!("http://{addr}/auth/plex").as_str()),
        "{}",
        pin.auth_url
    );
    // The wizard's Sign in with Plex (P3-10) as well.
    let pin = browser.plex_pin_begin().await.unwrap();
    assert_eq!(
        forward_url(&pin.auth_url).as_deref(),
        Some(format!("http://{addr}/auth/plex").as_str()),
        "{}",
        pin.auth_url
    );

    let cli = with_token(addr, &token).await;
    let pin = cli.plex_link_begin().await.unwrap();
    assert_eq!(forward_url(&pin.auth_url), None, "{}", pin.auth_url);
}

#[tokio::test]
async fn a_user_links_their_plex_account_and_unlinks_it_and_the_token_is_never_handed_out() {
    let tv = plex_tv(77, "tok-alice-account", "alice").await;
    let (addr, dir) = served_with("plex-link", "", reaching(&tv)).await;
    let (root, alice) = root_and_alice(addr).await;

    assert_eq!(alice.plex_link().await.unwrap().link, None);
    let linked = link(&alice).await;
    assert_eq!(linked.username.as_deref(), Some("alice"));
    assert_eq!(linked.email.as_deref(), Some("alice@example.com"));
    assert!(!linked.by_config);
    // The token is her secret, and nowhere else.
    let kept = secret(dir.as_ref()).expect("kept");
    assert!(kept.contains("tok-alice-account"));
    assert!(!dir.as_ref().join("runtime.ucl").exists());

    // The admin sees who is linked to which account.
    let overview = root.plex_sync_overview().await.unwrap();
    let shown = serde_json::to_string(&overview).unwrap();
    assert!(!shown.contains("tok-alice-account"), "{shown}");
    let by_name = |name: &str| {
        overview
            .users
            .iter()
            .find(|u| u.username == name)
            .unwrap()
            .link
            .clone()
    };
    assert_eq!(by_name("alice"), Some(linked.clone()));
    assert_eq!(by_name("root"), None);
    let mine = serde_json::to_string(&alice.plex_link().await.unwrap()).unwrap();
    assert!(!mine.contains("tok-alice-account"), "{mine}");

    alice.plex_unlink().await.unwrap();
    assert_eq!(secret(dir.as_ref()), None);
    assert_eq!(alice.plex_link().await.unwrap().link, None);
}

#[tokio::test]
async fn a_pin_is_checked_only_by_whoever_began_it() {
    let tv = plex_tv(77, "tok-alice-account", "alice").await;
    let (addr, _dir) = served_with("plex-link-own-pin", "", reaching(&tv)).await;
    let (root, alice) = root_and_alice(addr).await;
    let pin = alice.plex_link_begin().await.unwrap();
    assert_eq!(
        code(root.plex_link_check(pin.id).await.unwrap_err()),
        error_code::INVALID_INPUT
    );
    assert_eq!(root.plex_sync_overview().await.unwrap().users[0].link, None);
}

#[tokio::test]
async fn an_admin_unlinks_a_user_but_cannot_link_one() {
    let tv = plex_tv(77, "tok-alice-account", "alice").await;
    let (addr, dir) = served_with("plex-link-admin", "", reaching(&tv)).await;
    let (root, alice) = root_and_alice(addr).await;
    link(&alice).await;
    root.user_plex_unlink("alice".into()).await.unwrap();
    assert_eq!(secret(dir.as_ref()), None);
    assert_eq!(alice.plex_link().await.unwrap().link, None);
    // Unlinking is the admin's; nothing here links someone else.
    assert_eq!(
        code(alice.user_plex_unlink("root".into()).await.unwrap_err()),
        error_code::FORBIDDEN
    );
}

#[tokio::test]
async fn plex_tv_out_of_reach_is_unavailable_and_links_nothing() {
    // A port nothing listens on.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let wizard = Wizard {
        plex_tv: format!("http://127.0.0.1:{port}").parse().unwrap(),
        ..Wizard::default()
    };
    let (addr, dir) = served_with("plex-link-down", "", wizard).await;
    let (_root, alice) = root_and_alice(addr).await;
    assert_eq!(
        code(alice.plex_link_begin().await.unwrap_err()),
        error_code::UNAVAILABLE
    );
    assert_eq!(secret(dir.as_ref()), None);
}

/// PX-04's `watch_sync { user = … }` is the owner's link: shown as made
/// by the config, and not unlinked here.
#[tokio::test]
async fn the_config_s_watch_sync_user_is_linked_as_the_owner() {
    let file = r#"plex { url = "http://127.0.0.1:9"; watch_sync { user = "root"; } }"#;
    let (addr, dir) = served_with("plex-link-owner", file, Wizard::default()).await;
    let (_, _) = root_and_alice(addr).await;
    // The owner is found as the server starts, as `watch_sync` always was.
    let again = serve_in(dir.as_ref(), file, Wizard::default()).await;
    let root = signed_in(again, "root", "correct horse").await;
    let owner = root.plex_sync_overview().await.unwrap();
    let root_link = owner
        .users
        .iter()
        .find(|u| u.username == "root")
        .and_then(|u| u.link.clone())
        .expect("linked by the config");
    assert!(root_link.by_config);
    assert!(
        owner
            .users
            .iter()
            .any(|u| u.username == "alice" && u.link.is_none())
    );
    assert_eq!(
        code(root.plex_unlink().await.unwrap_err()),
        error_code::NOT_CONFIGURED
    );
    assert!(root.plex_link().await.unwrap().link.is_some());
}

/// A PMS that answers `/` only with `token`.
async fn pms_for(name: &str, token: &str) -> MockServer {
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
async fn the_linked_account_lists_its_servers_and_a_server_is_added_with_its_token_for_it() {
    let tv = plex_tv(77, "tok-root-account", "andrey").await;
    let tower = pms_for("Tower", "tok-root-account").await;
    let cabin = pms_for("Cabin", "root-on-cabin").await;
    Mock::given(method("GET"))
        .and(path("/api/v2/resources"))
        .and(header("X-Plex-Token", "tok-root-account"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "name": "Tower", "provides": "server", "clientIdentifier": "tower-machine",
              "owned": true, "accessToken": "tok-root-account",
              "connections": [{ "uri": tower.uri(), "local": true, "relay": false }] },
            { "name": "Cabin", "provides": "server", "clientIdentifier": "cabin-machine",
              "owned": false, "accessToken": "root-on-cabin",
              "connections": [{ "uri": cabin.uri(), "local": false, "relay": false }] },
        ])))
        .mount(&tv)
        .await;
    let file = format!(r#"plex {{ url = "{}"; }}"#, tower.uri());
    let (addr, dir) = served_with("plex-link-servers", &file, reaching(&tv)).await;
    let (root, _alice) = root_and_alice(addr).await;
    assert_eq!(
        code(root.plex_account_servers().await.unwrap_err()),
        error_code::NOT_CONFIGURED
    );
    link(&root).await;

    let servers = root.plex_account_servers().await.unwrap();
    let shown = serde_json::to_string(&servers).unwrap();
    assert!(!shown.contains("root-on-cabin"), "{shown}");
    assert_eq!(servers[0].added.as_deref(), Some("plex"));
    assert_eq!(servers[1].name, "Cabin");
    assert_eq!(servers[1].added, None);
    assert_eq!(servers[1].url.as_deref(), Some(cabin.uri().as_str()));

    // Probed as the add flow does before adding it, and added, without a
    // token: the account's own for Cabin, which is the only one Cabin lets
    // in.
    let probed = root.plex_probe(cabin.uri(), None).await.unwrap();
    assert!(probed.reachable, "{probed:?}");
    let added = root
        .plex_server_add(servers[1].url.clone().unwrap(), None)
        .await
        .unwrap();
    assert!(added.status.reachable, "{added:?}");
    assert_eq!(
        std::fs::read_to_string(
            dir.as_ref()
                .join(format!("secrets/plex-token-{}", added.id))
        )
        .unwrap(),
        "root-on-cabin"
    );
}

/// PX-09 finding 15 (B-8): every `plexLinkBegin` asked plex.tv for a PIN
/// and kept it, without end, so one consumer could have the server flood
/// plex.tv. A user begins at most a few links in 15 minutes; past them
/// plex.tv is not asked.
#[tokio::test]
async fn a_user_begins_only_a_few_links_at_a_time() {
    let tv = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/pins"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": 9, "code": "q3n8vbx2kd5w7rjh0tyc4mzpf", "expiresIn": 900, "authToken": null
        })))
        .mount(&tv)
        .await;
    let (addr, _dir) = served_with("plex-link-begins", "", reaching(&tv)).await;
    let (root, alice) = root_and_alice(addr).await;
    for _ in 0..spoolframe_server::plex_accounts::MAX_LINK_BEGINS {
        alice.plex_link_begin().await.unwrap();
    }
    let err = alice.plex_link_begin().await.unwrap_err();
    assert_eq!(code(err), error_code::RATE_LIMITED);
    assert_eq!(
        tv.received_requests().await.unwrap().len(),
        spoolframe_server::plex_accounts::MAX_LINK_BEGINS
    );
    // Another user's are their own.
    root.plex_link_begin().await.unwrap();
}
