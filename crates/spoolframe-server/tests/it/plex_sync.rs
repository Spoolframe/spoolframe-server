//! Watch state sync per library and per linked user (P3-12), against a
//! stand-in Plex server that keeps each account's state apart, as a PMS
//! does, and a stand-in plex.tv.

use std::net::SocketAddr;

use serde_json::json;
use spoolframe_admin_api::{
    AdminApiClient, MeApiClient, NewUser, PlexSyncReport, Role, SyncDirection, error_code,
};
use spoolframe_server::principal::Principal;
use spoolframe_server::store::Store;
use spoolframe_server::store::watch::WatchUpdate;
use wiremock::matchers::{header, method, path, path_regex, query_param};
use wiremock::{Match, Mock, MockServer, Request, Respond, ResponseTemplate};

use crate::admin::code;
use crate::plex_link::{link, mount_pin, reaching, root_and_alice, signed_in};
use crate::settings::serve_syncing;

/// One film an account has seen on the stand-in Plex server. Its key's
/// first digit is its section: `11` is in section 1.
#[derive(Clone, Copy)]
struct Seen {
    key: &'static str,
    /// When it was last viewed, Unix seconds.
    at: i64,
    /// Where it stopped, for one in progress; watched without.
    position_ms: Option<u64>,
}

const fn watched(key: &'static str, at: i64) -> Seen {
    Seen {
        key,
        at,
        position_ms: None,
    }
}

fn section(key: &str) -> &str {
    &key[..1]
}

fn metadata(key: &str) -> serde_json::Value {
    json!({
        "ratingKey": key, "type": "movie", "title": format!("Film {key}"),
        "librarySectionID": section(key).parse::<u32>().unwrap(), "duration": 3_600_000,
    })
}

/// Requests without a token: the owner's, on a PMS that lets LAN clients
/// in without one.
struct NoToken;

impl Match for NoToken {
    fn matches(&self, request: &Request) -> bool {
        !request.headers.contains_key("x-plex-token")
    }
}

/// `/library/metadata/<key>`: the film, in its section; none in section 9,
/// which is films Plex no longer has.
struct Item;

impl Respond for Item {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let key = request.url.path().rsplit('/').next().unwrap_or_default();
        if key.starts_with('9') {
            return ResponseTemplate::new(404);
        }
        ResponseTemplate::new(200)
            .set_body_json(json!({ "MediaContainer": { "Metadata": [metadata(key)] } }))
    }
}

/// A stand-in Plex server called Tower, with three movie sections; each
/// account (by the token it uses there, `None` for no token) has seen
/// what it is given.
async fn tower(accounts: &[(Option<&str>, &[Seen])]) -> MockServer {
    let pms = MockServer::start().await;
    let json = |body: serde_json::Value| ResponseTemplate::new(200).set_body_json(body);
    Mock::given(method("GET"))
        .and(path("/identity"))
        .respond_with(json(
            json!({ "MediaContainer": { "machineIdentifier": "tower-machine" } }),
        ))
        .mount(&pms)
        .await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(json(
            json!({ "MediaContainer": { "friendlyName": "Tower", "version": "1.41.0" } }),
        ))
        .mount(&pms)
        .await;
    Mock::given(method("GET"))
        .and(path("/library/sections"))
        .respond_with(json(json!({ "MediaContainer": { "Directory": [
            { "key": "1", "type": "movie", "title": "Movies" },
            { "key": "2", "type": "movie", "title": "Kids" },
            { "key": "3", "type": "movie", "title": "Docs" },
        ] } })))
        .mount(&pms)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/library/metadata/\d+$"))
        .respond_with(Item)
        .mount(&pms)
        .await;
    for at in ["/:/scrobble", "/:/unscrobble", "/:/progress"] {
        Mock::given(method("GET"))
            .and(path(at))
            .respond_with(ResponseTemplate::new(200))
            .mount(&pms)
            .await;
    }
    for &(token, seen) in accounts {
        for s in ["1", "2", "3"] {
            for (filter, in_progress) in [("unwatched", false), ("inProgress", true)] {
                let listed: Vec<serde_json::Value> = seen
                    .iter()
                    .filter(|f| section(f.key) == s && f.position_ms.is_some() == in_progress)
                    .map(|f| {
                        let mut m = metadata(f.key);
                        m["lastViewedAt"] = json!(f.at);
                        match f.position_ms {
                            Some(p) => m["viewOffset"] = json!(p),
                            None => m["viewCount"] = json!(1),
                        }
                        m
                    })
                    .collect();
                let mock = Mock::given(method("GET"))
                    .and(path(format!("/library/sections/{s}/all")))
                    .and(query_param(filter, if in_progress { "1" } else { "0" }));
                let mock = match token {
                    Some(token) => mock.and(header("X-Plex-Token", token)),
                    None => mock.and(NoToken),
                };
                mock.respond_with(json(json!({ "MediaContainer": {
                    "totalSize": listed.len(), "Metadata": listed,
                } })))
                .mount(&pms)
                .await;
            }
        }
    }
    pms
}

/// What was written to Plex: each write's endpoint, item and token.
async fn writes(pms: &MockServer) -> Vec<(String, String, Option<String>)> {
    pms.received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().starts_with("/:/"))
        .map(|r| {
            let key = r
                .url
                .query_pairs()
                .find(|(k, _)| k == "key")
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default();
            let token = r
                .headers
                .get("x-plex-token")
                .map(|v| v.to_str().unwrap().to_owned());
            (r.url.path().to_owned(), key, token)
        })
        .collect()
}

fn scrobble(key: &str, token: Option<&str>) -> (String, String, Option<String>) {
    ("/:/scrobble".into(), key.into(), token.map(Into::into))
}

/// plex.tv's list of the servers each of these accounts reaches: Tower,
/// with that account's own token there.
async fn mount_resources(tv: &MockServer, tower: &MockServer, accounts: &[(&str, &str)]) {
    for &(account, on_tower) in accounts {
        Mock::given(method("GET"))
            .and(path("/api/v2/resources"))
            .and(header("X-Plex-Token", account))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                { "name": "Tower", "provides": "server", "clientIdentifier": "tower-machine",
                  "owned": false, "accessToken": on_tower,
                  "connections": [{ "uri": tower.uri(), "local": true, "relay": false }] },
            ])))
            .mount(tv)
            .await;
    }
}

/// `username`'s principal.
async fn principal(store: &Store, username: &str) -> Principal {
    let user = store
        .password_login(username)
        .await
        .unwrap()
        .expect(username);
    Principal::LocalUser(user.id)
}

/// Mark `key` watched for `username` in Spoolframe, as of `at`.
async fn seen_here(store: &Store, username: &str, key: &str, at: i64) {
    let media_id = format!("plex:movie:{key}");
    store
        .save_watch_at(
            principal(store, username).await,
            WatchUpdate {
                media_id: &media_id,
                show_id: None,
                external_ids: &[],
                position_ms: None,
                duration_ms: Some(3_600_000),
                watched: true,
                unhide: false,
            },
            at,
        )
        .await
        .unwrap();
}

/// `username`'s state of `key` in Spoolframe: watched, and as of when.
async fn state_here(store: &Store, username: &str, key: &str) -> Option<(bool, Option<i64>, i64)> {
    store
        .all_watch(principal(store, username).await)
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.media_id == format!("plex:movie:{key}"))
        .map(|r| (r.watched, r.position_ms, r.updated_at))
}

fn of<'a>(reports: &'a [PlexSyncReport], username: &str) -> &'a PlexSyncReport {
    reports
        .iter()
        .find(|r| r.username == username)
        .unwrap_or_else(|| panic!("no report for {username}: {reports:?}"))
}

fn library(name: &str, section: &str, sync: &str) -> String {
    format!(
        r#"{{ name = "{name}"; kind = "movies"; plex {{ server = "plex"; section = "{section}"; sync = "{sync}"; }} }}"#
    )
}

async fn add_user(root: &jsonrpsee::ws_client::WsClient, addr: SocketAddr, name: &str) {
    root.user_add(NewUser {
        username: name.into(),
        display_name: None,
        password: format!("{name}'s horse"),
        role: Role::Consumer,
    })
    .await
    .unwrap();
    let _ = signed_in(addr, name, &format!("{name}'s horse")).await;
}

#[tokio::test]
async fn two_linked_users_each_sync_their_own_plex_account_and_an_unlinked_one_is_not() {
    let pms = tower(&[
        (Some("alice-on-tower"), &[watched("11", 1_000)]),
        (Some("bob-on-tower"), &[]),
    ])
    .await;
    let tv = MockServer::start().await;
    mount_pin(&tv, 77, "tok-alice", "alice").await;
    mount_pin(&tv, 78, "tok-bob", "bob").await;
    mount_resources(
        &tv,
        &pms,
        &[("tok-alice", "alice-on-tower"), ("tok-bob", "bob-on-tower")],
    )
    .await;
    let file = format!(
        "plex {{ url = \"{}\"; }} libraries [ {} ]",
        pms.uri(),
        library("Movies", "1", "both")
    );
    let dir = crate::admin::state_dir("sync-per-user").await;
    let (addr, store) = serve_syncing(dir.as_ref(), &file, reaching(&tv), false).await;
    let (root, alice) = root_and_alice(addr).await;
    add_user(&root, addr, "bob").await;
    add_user(&root, addr, "carol").await;
    let bob = signed_in(addr, "bob", "bob's horse").await;
    link(&alice).await;
    link(&bob).await;
    // bob and carol watched films here that Plex has not seen.
    seen_here(&store, "bob", "12", 2_000).await;
    seen_here(&store, "carol", "13", 2_000).await;

    let reports = root.plex_watch_sync(None, false).await.unwrap();
    assert_eq!(reports.len(), 2, "{reports:?}");
    let (a, b) = (of(&reports, "alice"), of(&reports, "bob"));
    assert_eq!(a.pulled, ["Film 11: watched"]);
    assert!(a.pushed.is_empty(), "{a:?}");
    assert_eq!(b.pushed, ["plex:movie:12: watched"]);
    assert!(b.pulled.is_empty(), "{b:?}");
    // Each in their own account, with their own token for Tower; carol's
    // film went nowhere.
    assert_eq!(writes(&pms).await, [scrobble("12", Some("bob-on-tower"))]);
    // alice's state is Plex's, as of Plex's time; bob's is his own.
    assert_eq!(
        state_here(&store, "alice", "11").await,
        Some((true, None, 1_000))
    );
    assert_eq!(state_here(&store, "bob", "11").await, None);

    // The newer side won, and nothing comes back: no ping-pong.
    let again = root
        .plex_watch_sync(Some("alice".into()), false)
        .await
        .unwrap();
    assert!(
        again[0].pulled.is_empty() && again[0].pushed.is_empty(),
        "{again:?}"
    );
    assert_eq!(again[0].skipped, 1);

    let overview = root.plex_sync_overview().await.unwrap();
    let user = |name: &str| overview.users.iter().find(|u| u.username == name).unwrap();
    let last = |name: &str| user(name).last_run.clone().unwrap();
    assert_eq!((last("alice").pulled, last("alice").libraries), (0, 1));
    assert_eq!(last("bob").pushed, 1);
    assert_eq!(user("carol").link, None);
    assert_eq!(user("carol").last_run, None);
    assert_eq!(overview.interval_secs, 120);
    assert_eq!(
        code(
            root.plex_watch_sync(Some("carol".into()), false)
                .await
                .unwrap_err()
        ),
        error_code::NOT_CONFIGURED
    );
}

#[tokio::test]
async fn from_plex_never_writes_to_plex_to_plex_never_writes_here_and_off_does_neither() {
    let pms = tower(&[(
        Some("alice-on-tower"),
        &[
            watched("11", 1_000),
            watched("21", 1_000),
            watched("31", 1_000),
        ],
    )])
    .await;
    let tv = MockServer::start().await;
    mount_pin(&tv, 77, "tok-alice", "alice").await;
    mount_resources(&tv, &pms, &[("tok-alice", "alice-on-tower")]).await;
    let file = format!(
        "plex {{ url = \"{}\"; }} libraries [ {}, {}, {} ]",
        pms.uri(),
        library("Movies", "1", "from_plex"),
        library("Kids", "2", "to_plex"),
        library("Docs", "3", "off"),
    );
    let dir = crate::admin::state_dir("sync-directions").await;
    let (addr, store) = serve_syncing(dir.as_ref(), &file, reaching(&tv), false).await;
    let (root, alice) = root_and_alice(addr).await;
    link(&alice).await;
    for key in ["12", "22", "32"] {
        seen_here(&store, "alice", key, 2_000).await;
    }

    let run = root
        .plex_watch_sync(Some("alice".into()), false)
        .await
        .unwrap()
        .remove(0);
    // Movies, from Plex: 11 taken, 12 not sent.
    assert_eq!(run.pulled, ["Film 11: watched"]);
    // Kids, to Plex: 22 sent, 21 not taken.
    assert_eq!(run.pushed, ["plex:movie:22: watched"]);
    assert_eq!(writes(&pms).await, [scrobble("22", Some("alice-on-tower"))]);
    assert_eq!(state_here(&store, "alice", "21").await, None);
    // Docs, off: neither.
    assert_eq!(state_here(&store, "alice", "31").await, None);
    assert_eq!(run.skipped, 2);

    // Movies both ways: PX-04's rules, so 12 goes to Plex now.
    root.library_set_sync("plex:1".into(), SyncDirection::Both)
        .await
        .unwrap();
    let run = root
        .plex_watch_sync(Some("alice".into()), false)
        .await
        .unwrap()
        .remove(0);
    assert!(run.pulled.is_empty(), "{run:?}");
    assert!(
        run.pushed.contains(&"plex:movie:12: watched".to_owned()),
        "{run:?}"
    );
}

/// The user's dev server, as PX-04 left it: `watch_sync { user = … }` on a
/// LAN PMS without a token, no libraries chosen. It keeps syncing as it
/// did, with nothing to do: every library of the server both ways, no
/// token sent, and plex.tv never asked.
#[tokio::test]
async fn the_owner_s_px_04_setup_keeps_syncing_as_before() {
    let pms = tower(&[(
        None,
        &[
            watched("11", 1_000),
            watched("21", 1_000),
            Seen {
                key: "31",
                at: 1_500,
                position_ms: Some(600_000),
            },
        ],
    )])
    .await;
    let tv = MockServer::start().await;
    let file = format!(
        "plex {{ url = \"{}\"; watch_sync {{ user = \"root\"; }} }}",
        pms.uri()
    );
    let dir = crate::admin::state_dir("sync-owner").await;
    let (addr, _) = serve_syncing(dir.as_ref(), &file, reaching(&tv), false).await;
    let _ = root_and_alice(addr).await;
    // The server started again, as after setting up: root is the owner.
    let (addr, store) = serve_syncing(dir.as_ref(), &file, reaching(&tv), false).await;
    let root = signed_in(addr, "root", "correct horse").await;
    seen_here(&store, "root", "12", 2_000).await;
    // Gone from Plex: nothing to send it to.
    seen_here(&store, "root", "91", 2_000).await;

    let reports = root.plex_watch_sync(None, false).await.unwrap();
    assert_eq!(reports.len(), 1, "{reports:?}");
    let run = of(&reports, "root");
    assert_eq!(
        run.pulled,
        [
            "Film 11: watched",
            "Film 21: watched",
            "Film 31: at 0:10:00"
        ]
    );
    assert_eq!(run.pushed, ["plex:movie:12: watched"]);
    assert_eq!(writes(&pms).await, [scrobble("12", None)]);
    assert_eq!(
        state_here(&store, "root", "31").await,
        Some((false, Some(600_000), 1_500))
    );
    assert!(tv.received_requests().await.unwrap().is_empty());
    let overview = root.plex_sync_overview().await.unwrap();
    let owner = overview
        .users
        .iter()
        .find(|u| u.username == "root")
        .unwrap();
    assert!(owner.link.as_ref().unwrap().by_config);
    assert_eq!(owner.last_run.as_ref().unwrap().libraries, 3);
}

/// A Plex server that cannot be reached skips its libraries with a note,
/// and the others sync.
#[tokio::test]
async fn a_server_out_of_reach_is_a_note_and_the_others_sync() {
    let pms = tower(&[(Some("alice-on-tower"), &[watched("11", 1_000)])]).await;
    let tv = MockServer::start().await;
    mount_pin(&tv, 77, "tok-alice", "alice").await;
    mount_resources(&tv, &pms, &[("tok-alice", "alice-on-tower")]).await;
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let file = format!(
        r#"plex {{ url = "{}"; servers [ {{ id = "cabin"; name = "Cabin"; url = "http://127.0.0.1:{closed}"; }} ] }}
           libraries [ {}, {{ name = "Cabin movies"; kind = "movies"; plex {{ server = "cabin"; section = "1"; sync = "both"; }} }} ]"#,
        pms.uri(),
        library("Movies", "1", "both"),
    );
    let dir = crate::admin::state_dir("sync-cabin-down").await;
    let (addr, _store) = serve_syncing(dir.as_ref(), &file, reaching(&tv), false).await;
    let (root, alice) = root_and_alice(addr).await;
    link(&alice).await;
    let run = root
        .plex_watch_sync(Some("alice".into()), false)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(run.pulled, ["Film 11: watched"]);
    assert_eq!(run.notes, ["Cabin movies skipped: Cabin can't be reached"]);
    let overview = root.plex_sync_overview().await.unwrap();
    let alice = overview.users.iter().find(|u| u.username == "alice");
    assert_eq!(alice.unwrap().last_run.as_ref().unwrap().notes, run.notes);
}

/// A link starts the user's runs at once; unlinking stops them.
#[tokio::test]
async fn a_new_link_is_synced_at_once_and_an_unlinked_user_no_more() {
    let pms = tower(&[(Some("alice-on-tower"), &[watched("11", 1_000)])]).await;
    let tv = MockServer::start().await;
    mount_pin(&tv, 77, "tok-alice", "alice").await;
    mount_resources(&tv, &pms, &[("tok-alice", "alice-on-tower")]).await;
    let file = format!(
        "plex {{ url = \"{}\"; }} libraries [ {} ]",
        pms.uri(),
        library("Movies", "1", "both")
    );
    let dir = crate::admin::state_dir("sync-scheduled").await;
    let (addr, store) = serve_syncing(dir.as_ref(), &file, reaching(&tv), true).await;
    let (_root, alice) = root_and_alice(addr).await;
    link(&alice).await;
    let mut synced = None;
    for _ in 0..100 {
        synced = alice.plex_link().await.unwrap().last_run;
        if synced.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(synced.expect("synced after linking").pulled, 1);
    assert_eq!(
        state_here(&store, "alice", "11").await,
        Some((true, None, 1_000))
    );
    alice.plex_unlink().await.unwrap();
    let mut user = alice.plex_link().await.unwrap();
    for _ in 0..100 {
        if user.last_run.is_none() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        user = alice.plex_link().await.unwrap();
    }
    assert_eq!((user.link, user.last_run), (None, None));
}
