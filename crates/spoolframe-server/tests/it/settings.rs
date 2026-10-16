//! Settings changed at run time through the admin API (P3-10): checked as
//! a file is, saved in the runtime layer, applied at once or flagged for
//! a restart.

use std::net::SocketAddr;
use std::sync::Arc;

use jsonrpsee::ws_client::WsClient;
use serde_json::json;
use spoolframe_admin_api::{
    AdminApiClient, MeApiClient, RestartMode, SettingApply, SettingLayer, error_code,
};
use spoolframe_server::admin::{
    self, Admin, CookiePolicy, Origins, SetupCode, TrustedProxies, Wizard,
};
use spoolframe_server::auth::Auth;
use spoolframe_server::catalog::LiveCatalog;
use spoolframe_server::config::ServerConfig;
use spoolframe_server::config::WatchSyncConfig;
use spoolframe_server::config::layers::{Layers, Locations};
use spoolframe_server::plex_accounts::{PlexAccounts, configured_owner};
use spoolframe_server::plex_sync::WatchSync;
use spoolframe_server::runtime::{Live, RuntimeSettings};
use spoolframe_server::store::Store;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::admin::{CODE, admin, anonymous, code, setup, state_dir, with_token};

/// The admin API with runtime settings over the provided file `file`, and
/// its first admin.
pub(crate) async fn served(
    test: &str,
    file: &str,
) -> (SocketAddr, WsClient, spoolframe_test_dir::TestDir) {
    let (addr, dir) = served_with(test, file, Wizard::default()).await;
    let token = anonymous(addr)
        .await
        .setup(setup(CODE, "root", "correct horse"))
        .await
        .unwrap()
        .token;
    (addr, with_token(addr, &token).await, dir)
}

/// The admin API with runtime settings over `file` in a new state
/// directory, its wizard reaching `wizard`; nobody set up yet.
pub(crate) async fn served_with(
    test: &str,
    file: &str,
    wizard: Wizard,
) -> (SocketAddr, spoolframe_test_dir::TestDir) {
    let dir = state_dir(test).await;
    let addr = serve_in(dir.as_ref(), file, wizard).await;
    (addr, dir)
}

/// The admin API over the state in `dir`, as a server (re)started there.
pub(crate) async fn serve_in(dir: &std::path::Path, file: &str, wizard: Wizard) -> SocketAddr {
    serve_syncing(dir, file, wizard, false).await.0
}

/// [`serve_in`], with its store and Plex watch sync, whose scheduled runs
/// are started only with `start`: otherwise runs are only the ones a test
/// asks for.
pub(crate) async fn serve_syncing(
    dir: &std::path::Path,
    file: &str,
    wizard: Wizard,
    start: bool,
) -> (SocketAddr, Store) {
    serve_syncing_with(dir, file, wizard, start, false).await
}

/// [`serve_syncing`], and with `library` the Plex catalog's watch state
/// and playlists for the methods that read them (PX-40).
pub(crate) async fn serve_syncing_with(
    dir: &std::path::Path,
    file: &str,
    wizard: Wizard,
    start: bool,
    library: bool,
) -> (SocketAddr, Store) {
    let file_path = dir.join("server.ucl");
    std::fs::write(&file_path, file).unwrap();
    let layers = Layers::load(&Locations {
        state_dir: dir.to_owned(),
        config: Some(file_path),
    })
    .unwrap();
    let secrets = spoolframe_server::config::secrets::Secrets::new(dir);
    let plex =
        spoolframe_server::runtime::plex_catalog(&layers.config().unwrap(), &layers, &secrets)
            .unwrap()
            .map(Arc::new);
    let catalog = Arc::new(LiveCatalog::new(plex));
    let cookies = CookiePolicy::default();
    let settings = RuntimeSettings::new(
        layers,
        dir,
        Live {
            catalog: catalog.clone(),
            cookies: cookies.clone(),
            ..Live::default()
        },
        spoolframe_admin_api::RestartMode::InPlace,
    );
    let auth = Auth::new(Store::open(dir).await.unwrap());
    let owner = configured_owner(&settings.layers().config().unwrap(), auth.store())
        .await
        .unwrap();
    let config = settings.layers().config().unwrap();
    let accounts = Arc::new(PlexAccounts::new(
        secrets,
        auth.store().clone(),
        wizard.plex_site(),
        owner,
    ));
    let sync = Arc::new(WatchSync::new(
        catalog.clone(),
        accounts.clone(),
        config.watch.rules().unwrap().started_after,
        config
            .plex
            .as_ref()
            .map_or(WatchSyncConfig::DEFAULT_INTERVAL, |p| p.sync_interval()),
    ));
    if start {
        // Nothing saves watch state through `Watch` here: no changes.
        let (never, changes) = tokio::sync::broadcast::channel(1);
        sync.start(changes);
        // Closed, the runs would stop after the first.
        std::mem::forget(never);
    }
    let store = auth.store().clone();
    let library = library.then(|| {
        let catalog: Arc<dyn spoolframe_catalog::Catalog> = catalog.clone();
        let watch = Arc::new(spoolframe_server::watch::Watch::new(
            store.clone(),
            catalog.clone(),
            // Nothing these tests call probes a file.
            Arc::new(spoolframe_media::Prober::new(
                spoolframe_media::sandbox::Tool::unconfined("ffprobe"),
                spoolframe_media::Prober::DEFAULT_TIMEOUT,
            )),
            spoolframe_server::watch::WatchRules::default(),
        ));
        spoolframe_server::mcp::Library {
            playlists: Arc::new(spoolframe_server::playlists::Playlists::new(
                store.clone(),
                catalog.clone(),
                watch.clone(),
            )),
            catalog,
            watch,
        }
    });
    let (addr, handle) = admin::serve(
        "127.0.0.1:0".parse().unwrap(),
        Admin {
            library,
            plex: catalog,
            cookies,
            settings: Some(Arc::new(settings)),
            accounts: Some(accounts),
            plex_sync: Some(sync),
            wizard,
            ..admin(auth, Some(SetupCode::new(CODE)))
        },
        Origins::default(),
        TrustedProxies::default(),
    )
    .await
    .unwrap();
    std::mem::forget(handle);
    (addr, store)
}

/// A PMS with one movie, whose file Plex has at `/data/movies/a.mkv`.
pub(crate) async fn pms() -> MockServer {
    let pms = MockServer::start().await;
    let json = |body: serde_json::Value| ResponseTemplate::new(200).set_body_json(body);
    Mock::given(method("GET"))
        .and(path("/library/sections"))
        .respond_with(json(json!({ "MediaContainer": { "Directory": [
            { "key": "1", "type": "movie", "title": "Movies",
              "Location": [{ "path": "/data/movies" }] },
        ] } })))
        .mount(&pms)
        .await;
    Mock::given(method("GET"))
        .and(path("/library/sections/1/all"))
        .respond_with(json(
            json!({ "MediaContainer": { "totalSize": 1, "Metadata": [
            { "ratingKey": "10", "type": "movie", "title": "A",
              "Media": [{ "Part": [{ "file": "/data/movies/a.mkv" }] }] },
        ] } }),
        ))
        .mount(&pms)
        .await;
    pms
}

#[tokio::test]
async fn a_path_mapping_set_at_run_time_is_used_at_once() {
    let pms = pms().await;
    let (_addr, root, dir) = served(
        "live-mapping",
        &format!(r#"plex {{ url = "{}"; }}"#, pms.uri()),
    )
    .await;
    let media = dir.as_ref().join("media");
    std::fs::create_dir_all(&media).unwrap();
    std::fs::write(media.join("a.mkv"), b"movie").unwrap();

    let before = root.plex_check_paths(None).await.unwrap().reports;
    assert_eq!((before[0].ok, before[0].no_mapping.count), (0, 1));

    let entry = root
        .setting_set(
            "plex.path_mapping".into(),
            json!([{ "from": "/data/movies", "to": media }]),
        )
        .await
        .unwrap();
    assert_eq!(entry.apply, SettingApply::Live);
    assert!(!entry.restart_pending);
    assert_eq!(entry.layer, Some(SettingLayer::Runtime));
    let after = root.plex_check_paths(None).await.unwrap().reports;
    assert_eq!((after[0].ok, after[0].no_mapping.count), (1, 0));
    let saved = std::fs::read_to_string(dir.as_ref().join("runtime.ucl")).unwrap();
    assert!(saved.contains("/data/movies"), "{saved}");
}

/// The web UI's "Every library" row (P3-18): a mapping with no library
/// goes as the admin API's type serializes it, `"library": null`.
#[tokio::test]
async fn a_mapping_for_every_library_is_saved_as_the_web_ui_sends_it() {
    let pms = pms().await;
    let (_addr, root, dir) = served(
        "every-library-mapping",
        &format!(r#"plex {{ url = "{}"; }}"#, pms.uri()),
    )
    .await;
    let media = dir.as_ref().join("media");
    std::fs::create_dir_all(&media).unwrap();
    std::fs::write(media.join("a.mkv"), b"movie").unwrap();
    let mapping = spoolframe_admin_api::PathMappingEntry {
        library: None,
        from: "/data/movies".into(),
        to: media.display().to_string(),
    };
    root.setting_set(
        "plex.path_mapping".into(),
        serde_json::to_value(vec![mapping]).unwrap(),
    )
    .await
    .unwrap();
    let after = root.plex_check_paths(None).await.unwrap().reports;
    assert_eq!((after[0].ok, after[0].no_mapping.count), (1, 0));
}

#[tokio::test]
async fn the_last_check_of_every_file_is_kept_with_its_time_until_the_mappings_change() {
    let pms = pms().await;
    let (_addr, root, dir) = served(
        "last-path-check",
        &format!(r#"plex {{ url = "{}"; }}"#, pms.uri()),
    )
    .await;
    let now = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    };
    assert_eq!(root.plex_last_path_check().await.unwrap(), None);

    let started = now();
    let run = root.plex_check_paths(None).await.unwrap();
    assert!(
        (started..=now()).contains(&run.at),
        "{} not in {started}..",
        run.at
    );
    assert_eq!(run.reports[0].library, "Movies");
    assert_eq!(root.plex_last_path_check().await.unwrap(), Some(run));

    // A mapping changed: the check said what the old one found.
    let media = dir.as_ref().join("media");
    root.setting_set(
        "plex.path_mapping".into(),
        json!([{ "from": "/data/movies", "to": media }]),
    )
    .await
    .unwrap();
    assert_eq!(root.plex_last_path_check().await.unwrap(), None);
}

#[tokio::test]
async fn a_restart_setting_is_saved_and_flagged() {
    let (_addr, root, _dir) = served("restart-setting", "").await;
    let entry = root
        .setting_set("admin.listen".into(), json!("127.0.0.1:7499"))
        .await
        .unwrap();
    assert_eq!(entry.apply, SettingApply::Restart);
    assert!(entry.restart_pending);
    let all = root.settings().await.unwrap();
    let listed = all
        .entries
        .iter()
        .find(|e| e.key == "admin.listen")
        .unwrap();
    assert!(listed.restart_pending);
}

/// Without a supervisor the server starts itself again in place, so a
/// restart is offered, and said to be done that way.
#[tokio::test]
async fn an_unsupervised_server_offers_a_restart_in_place() {
    let (_addr, root, _dir) = served("restart-in-place", "").await;
    let all = root.settings().await.unwrap();
    assert!(all.can_restart);
    assert_eq!(all.restart_mode, RestartMode::InPlace);
    root.restart().await.unwrap();
}

#[tokio::test]
async fn an_invalid_value_is_refused_with_the_message_a_file_would_get() {
    let (_addr, root, _dir) = served("invalid-setting", "").await;
    let file = ServerConfig::parse(&format!(
        "schema = {:?}; watch {{ watched_percent = 0; }}",
        ServerConfig::SCHEMA
    ))
    .unwrap_err()
    .to_string();
    match root
        .setting_set("watch.watched_percent".into(), json!(0))
        .await
        .unwrap_err()
    {
        jsonrpsee::core::ClientError::Call(e) => {
            assert_eq!(e.code(), error_code::INVALID_INPUT);
            assert_eq!(e.message(), file);
        }
        other => panic!("{other}"),
    }
    let unknown = root
        .setting_set("plex.urll".into(), json!("http://nas:32400"))
        .await
        .unwrap_err();
    assert_eq!(code(unknown), error_code::INVALID_INPUT);
}

#[tokio::test]
async fn a_locked_file_s_keys_are_shown_locked_and_refused() {
    let (_addr, root, _dir) = served("locked-setting", r#"locked = true; name = "Nimble";"#).await;
    let all = root.settings().await.unwrap();
    let name = all.entries.iter().find(|e| e.key == "name").unwrap();
    assert!(name.locked);
    assert_eq!(name.layer, Some(SettingLayer::File));
    assert!(all.config_file.is_some());
    let refused = root
        .setting_set("name".into(), json!("Den"))
        .await
        .unwrap_err();
    assert_eq!(code(refused), error_code::SETTING_LOCKED);
    // What the file leaves out is not locked.
    let port = all
        .entries
        .iter()
        .find(|e| e.key == "network.port")
        .unwrap();
    assert!(!port.locked);
}

#[tokio::test]
async fn plex_set_up_at_run_time_answers_at_once() {
    let pms = pms().await;
    let (_addr, root, _dir) = served("plex-at-run-time", "").await;
    assert_eq!(
        code(root.plex_check_paths(None).await.unwrap_err()),
        error_code::NOT_CONFIGURED
    );
    root.setting_set("plex.url".into(), json!(pms.uri()))
        .await
        .unwrap();
    let reports = root.plex_check_paths(None).await.unwrap().reports;
    assert_eq!(reports[0].library, "Movies");
}

#[tokio::test]
async fn a_consumer_cannot_see_or_change_settings() {
    let (addr, root, _dir) = served("settings-forbidden", "").await;
    root.user_add(spoolframe_admin_api::NewUser {
        username: "alice".into(),
        display_name: None,
        password: "battery staple".into(),
        role: spoolframe_admin_api::Role::Consumer,
    })
    .await
    .unwrap();
    let login = anonymous(addr)
        .await
        .login(crate::admin::creds("alice", "battery staple"))
        .await
        .unwrap();
    let alice = with_token(addr, &login.token).await;
    assert_eq!(
        code(alice.settings().await.unwrap_err()),
        error_code::FORBIDDEN
    );
    assert_eq!(
        code(
            alice
                .setting_set("name".into(), json!("Mine"))
                .await
                .unwrap_err()
        ),
        error_code::FORBIDDEN
    );
}

/// The `Set-Cookie` of a password sign-in from the listener's own page.
async fn sign_in_cookie(addr: SocketAddr) -> String {
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/auth/login"))
        .header("origin", format!("http://{addr}"))
        .json(&json!({ "username": "root", "password": "correct horse" }))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success(), "{response:?}");
    response.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn insecure_cookies_turned_on_apply_to_the_next_sign_in() {
    let (addr, root, _dir) = served("insecure-cookies", "").await;
    assert!(sign_in_cookie(addr).await.contains("Secure"));
    root.setting_set("admin.insecure_cookies".into(), json!(true))
        .await
        .unwrap();
    let cookie = sign_in_cookie(addr).await;
    assert!(
        !cookie.contains("Secure") && cookie.contains("HttpOnly"),
        "{cookie}"
    );
    let site: serde_json::Value = reqwest::get(format!("http://{addr}/auth/site"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(site["insecureCookies"], true);
}

/// Behind an HTTPS proxy, insecure cookies only downgrade the cookie
/// (PX-11 finding 32): refused, whichever of the two settings comes
/// second, and nothing is saved.
#[tokio::test]
async fn insecure_cookies_are_refused_behind_an_https_public_url() {
    let (_addr, root, dir) = served(
        "insecure-cookies-https",
        r#"admin { public_url = "https://media.example.org"; }"#,
    )
    .await;
    let refused = root
        .setting_set("admin.insecure_cookies".into(), json!(true))
        .await
        .unwrap_err();
    assert_eq!(code(refused), error_code::INVALID_INPUT);
    assert!(!dir.as_ref().join("runtime.ucl").exists());

    let (_addr, root, _dir) = served("insecure-cookies-then-https", "").await;
    root.setting_set("admin.insecure_cookies".into(), json!(true))
        .await
        .unwrap();
    let refused = root
        .setting_set(
            "admin.public_url".into(),
            json!("https://media.example.org"),
        )
        .await
        .unwrap_err();
    assert_eq!(code(refused), error_code::INVALID_INPUT);
    // Plain HTTP behind the proxy is what insecure cookies are for.
    root.setting_set("admin.public_url".into(), json!("http://10.0.0.6:7421"))
        .await
        .unwrap();
}

/// A change that would put an unnamed proxy in front of a loopback
/// listener is refused, so the server is never left unable to start
/// (PX-11 finding 38, the second review's M-4); naming the proxy first
/// lets it through.
#[tokio::test]
async fn a_public_url_on_a_loopback_listener_needs_its_proxy_named() {
    let (_addr, root, dir) = served(
        "unnamed-proxy-change",
        r#"admin { listen = "127.0.0.1:7421"; }"#,
    )
    .await;
    let refused = root
        .setting_set(
            "admin.public_url".into(),
            json!("https://media.example.org"),
        )
        .await
        .unwrap_err();
    assert_eq!(code(refused), error_code::INVALID_INPUT);
    assert!(!dir.as_ref().join("runtime.ucl").exists());

    root.setting_set("admin.trusted_proxies".into(), json!("127.0.0.1"))
        .await
        .unwrap();
    root.setting_set(
        "admin.public_url".into(),
        json!("https://media.example.org"),
    )
    .await
    .unwrap();
}

/// A sign-in provider's client secret set in the web UI (P3-04): kept as a
/// secret file, readable by the server's user only, and never shown; a
/// provider whose config names a file reads that file instead.
#[tokio::test]
async fn a_sign_in_provider_s_client_secret_is_kept_as_a_secret() {
    use std::os::unix::fs::PermissionsExt;
    let file = r#"
        admin { public_url = "https://media.example.org"; }
        auth { oidc [
            { name = "Pocket ID"; issuer = "https://id.example.org"; client_id = "spoolframe"; },
            { name = "Filed"; issuer = "https://other.example.org"; client_id = "x";
              client_secret_file = "/run/secrets/filed"; },
        ] }
    "#;
    let (_addr, root, dir) = served("sso-secret", file).await;
    root.sso_set_client_secret("Pocket ID".into(), Some(" s3cr3t-client \n".into()))
        .await
        .unwrap();
    let kept = dir.as_ref().join("secrets").join("oidc-pocket-id");
    assert_eq!(std::fs::read_to_string(&kept).unwrap(), "s3cr3t-client");
    assert_eq!(
        std::fs::metadata(&kept).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let all = serde_json::to_string(&root.settings().await.unwrap()).unwrap();
    assert!(!all.contains("s3cr3t"), "{all}");
    assert!(all.contains("auth.oidc"), "{all}");

    let locked = root
        .sso_set_client_secret("Filed".into(), Some("x".into()))
        .await;
    assert_eq!(code(locked.unwrap_err()), error_code::SETTING_LOCKED);
    let unknown = root
        .sso_set_client_secret("Nobody".into(), Some("x".into()))
        .await;
    assert_eq!(code(unknown.unwrap_err()), error_code::NOT_CONFIGURED);
    root.sso_set_client_secret("Pocket ID".into(), None)
        .await
        .unwrap();
    assert!(!kept.exists());
}
