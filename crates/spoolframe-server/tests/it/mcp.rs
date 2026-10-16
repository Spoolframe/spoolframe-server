//! MCP at `/mcp` (PX-42): a client with its OAuth token, calling tools
//! for the user who connected it, against a server's fake library.

use std::net::SocketAddr;

use reqwest::StatusCode;
use serde_json::{Value, json};
use spoolframe_server::admin::{self, Admin, Origins, TrustedProxies};
use spoolframe_server::auth::hash_password;
use spoolframe_server::oauth::McpSwitch;
use spoolframe_server::passkeys::Passkeys;
use spoolframe_server::principal::Principal;
use spoolframe_server::store::watch::WatchUpdate;
use spoolframe_types::Role;

use crate::oauth::{browser, connect, register};
use crate::passkeys::{ORIGIN, cookie};
use crate::support::{LogCapture, TestServer, alone};

pub(crate) const PASSWORD: &str = "correct horse";

/// What the client says it speaks.
const PROTOCOL: &str = "2025-06-18";

/// A test server with an admin listener beside it at [`ORIGIN`], MCP on,
/// its tools reading the server's library, watch state and playlists.
pub(crate) async fn beside(test: &str) -> (TestServer, SocketAddr) {
    let server = TestServer::start(test).await;
    let origins = Origins::Exactly(ORIGIN.to_owned());
    let (addr, handle) = admin::serve(
        "127.0.0.1:0".parse().unwrap(),
        Admin {
            passkeys: std::sync::Arc::new(Passkeys::new(&origins, &server.server_id)),
            mcp: McpSwitch::new(true),
            library: Some(spoolframe_server::mcp::Library {
                catalog: server.catalog.clone(),
                watch: server.watch.clone(),
                playlists: server.playlists.clone(),
            }),
            playback: Some(server.debug.clone()),
            ..crate::admin::admin(server.auth.clone(), None)
        },
        origins,
        TrustedProxies::default(),
    )
    .await
    .unwrap();
    std::mem::forget(handle);
    (server, addr)
}

/// A user called `name` with role `role`: their principal.
pub(crate) async fn user(server: &TestServer, name: &str, role: Role) -> Principal {
    let phc = hash_password(PASSWORD).await.unwrap();
    let id = server.store.add_user(name, None, &phc, role).await.unwrap();
    Principal::LocalUser(id)
}

/// `name` signs in to the web UI with their password: the cookie.
pub(crate) async fn signed_in(addr: SocketAddr, name: &str) -> String {
    let response = browser()
        .post(format!("http://{addr}/auth/login"))
        .header("origin", ORIGIN)
        .json(&json!({ "username": name, "password": PASSWORD }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    cookie(&response)
}

/// An MCP client, as it calls once connected: its token on every POST.
pub(crate) struct Mcp {
    pub(crate) addr: SocketAddr,
    pub(crate) token: String,
    pub(crate) refresh: String,
    pub(crate) client_id: String,
}

impl Mcp {
    /// A client called Claude, connected by `name` with `scopes` granted
    /// of everything it asks for.
    pub(crate) async fn connect(addr: SocketAddr, name: &str, scopes: &[&str]) -> Self {
        let cookie = signed_in(addr, name).await;
        let client_id = register(addr, "Claude").await;
        let tokens = connect(
            addr,
            &cookie,
            &client_id,
            "library:read playlists:read playlists:write admin",
            scopes,
        )
        .await;
        Self {
            addr,
            token: tokens["access_token"].as_str().unwrap().to_owned(),
            refresh: tokens["refresh_token"].as_str().unwrap().to_owned(),
            client_id,
        }
    }

    pub(crate) async fn post(&self, body: &Value) -> reqwest::Response {
        browser()
            .post(format!("http://{}/mcp", self.addr))
            .header("authorization", format!("Bearer {}", self.token))
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", PROTOCOL)
            .json(body)
            .send()
            .await
            .unwrap()
    }

    /// A JSON-RPC call's whole answer.
    pub(crate) async fn rpc(&self, method: &str, params: Value) -> Value {
        let response = self
            .post(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }))
            .await;
        assert_eq!(response.status(), StatusCode::OK, "{method}");
        response.json().await.unwrap()
    }

    /// The client trades its refresh token for a new pair: the status.
    pub(crate) async fn refreshed(&self) -> StatusCode {
        crate::oauth::token(
            self.addr,
            &[
                ("grant_type", "refresh_token"),
                ("client_id", &self.client_id),
                ("refresh_token", &self.refresh),
            ],
        )
        .await
        .status()
    }

    /// The names of the tools listed.
    pub(crate) async fn tools(&self) -> Vec<String> {
        self.rpc("tools/list", json!({})).await["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_owned())
            .collect()
    }

    /// The tool's answer: its structured result, or the error it said.
    pub(crate) async fn call(&self, tool: &str, arguments: Value) -> Result<Value, String> {
        let answer = self
            .rpc(
                "tools/call",
                json!({ "name": tool, "arguments": arguments }),
            )
            .await;
        if let Some(error) = answer.get("error") {
            return Err(error["message"].as_str().unwrap().to_owned());
        }
        let result = &answer["result"];
        if result["isError"] == true {
            return Err(result["content"][0]["text"].as_str().unwrap().to_owned());
        }
        Ok(result["structuredContent"].clone())
    }
}

/// The ids of the items in `list`.
pub(crate) fn ids(list: &Value) -> Vec<String> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap().to_owned())
        .collect()
}

fn progress<'a>(media_id: &'a str, show_id: Option<&'a str>, minutes: i64) -> WatchUpdate<'a> {
    WatchUpdate {
        media_id,
        show_id,
        external_ids: &[],
        position_ms: Some(minutes * 60_000),
        duration_ms: Some(2_640_000),
        watched: false,
        unhide: false,
    }
}

const LIBRARY_TOOLS: [&str; 5] = [
    "search",
    "get_item",
    "list_recent",
    "continue_watching",
    "watch_history",
];

/// A client connecting by the URL alone starts here: a 401 that names
/// where to find how to sign in.
#[tokio::test]
async fn without_a_live_token_mcp_says_where_to_sign_in() {
    let (_server, addr) = beside("mcp-401").await;
    let no_token = browser()
        .post(format!("http://{addr}/mcp"))
        .header("accept", "application/json, text/event-stream")
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }))
        .send()
        .await
        .unwrap();
    assert_eq!(no_token.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        no_token.headers()["www-authenticate"],
        format!("Bearer resource_metadata=\"{ORIGIN}/.well-known/oauth-protected-resource/mcp\"")
    );
    let wrong = Mcp {
        addr,
        token: "sfo_00".into(),
        refresh: String::new(),
        client_id: String::new(),
    };
    let refused = wrong
        .post(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }))
        .await;
    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
    assert!(
        refused.headers()["www-authenticate"]
            .to_str()
            .unwrap()
            .contains("error=\"invalid_token\"")
    );
}

/// A session cookie is no MCP credential: the web UI's own pages are not
/// clients.
#[tokio::test]
async fn a_session_cookie_does_not_reach_mcp() {
    let (server, addr) = beside("mcp-cookie").await;
    user(&server, "alice", Role::Consumer).await;
    let cookie = signed_in(addr, "alice").await;
    let response = browser()
        .post(format!("http://{addr}/mcp"))
        .header("cookie", cookie)
        .header("accept", "application/json, text/event-stream")
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_client_initializes_and_lists_the_tools_of_its_scopes_only() {
    let (server, addr) = beside("mcp-list").await;
    user(&server, "alice", Role::Consumer).await;
    let mcp = Mcp::connect(addr, "alice", &["library:read"]).await;
    let init = mcp
        .rpc(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL,
                "capabilities": {},
                "clientInfo": { "name": "claude-ai", "version": "1.0" },
            }),
        )
        .await;
    assert_eq!(init["result"]["serverInfo"]["name"], "spoolframe");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    assert_eq!(mcp.tools().await, LIBRARY_TOOLS);
    // Nothing streams: no GET.
    let get = browser()
        .get(format!("http://{addr}/mcp"))
        .header("authorization", format!("Bearer {}", mcp.token))
        .header("accept", "text/event-stream")
        .send()
        .await
        .unwrap();
    assert_eq!(get.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn continue_watching_gives_the_user_s_unfinished_items_and_nobody_else_s() {
    let (server, addr) = beside("mcp-continue").await;
    let alice = user(&server, "alice", Role::Consumer).await;
    let bob = user(&server, "bob", Role::Consumer).await;
    server
        .store
        .save_watch(alice, progress("plex:episode:12", Some("plex:show:10"), 10))
        .await
        .unwrap();
    server
        .store
        .save_watch(bob, progress("plex:movie:1", None, 30))
        .await
        .unwrap();
    let mcp = Mcp::connect(addr, "alice", &["library:read"]).await;
    let answer = mcp.call("continue_watching", json!({})).await.unwrap();
    assert_eq!(ids(&answer["continueWatching"]), ["plex:episode:12"]);
    let episode = &answer["continueWatching"][0];
    assert_eq!(episode["show"], "The Expanse");
    assert_eq!(episode["state"]["positionSecs"], 600);
    assert_eq!(episode["state"]["watched"], false);
}

#[tokio::test]
async fn the_watch_history_is_the_user_s_newest_first_a_page_at_a_time() {
    let (server, addr) = beside("mcp-history").await;
    let alice = user(&server, "alice", Role::Consumer).await;
    for (id, at) in [("plex:movie:1", 100), ("plex:episode:13", 200)] {
        server
            .store
            .save_watch_at(alice, progress(id, None, 5), at)
            .await
            .unwrap();
    }
    let mcp = Mcp::connect(addr, "alice", &["library:read"]).await;
    let first = mcp
        .call("watch_history", json!({ "limit": 1 }))
        .await
        .unwrap();
    assert_eq!(ids(&first["items"]), ["plex:episode:13"]);
    assert_eq!(first["total"], 2);
    let next = mcp
        .call("watch_history", json!({ "offset": 1, "limit": 1 }))
        .await
        .unwrap();
    assert_eq!(ids(&next["items"]), ["plex:movie:1"]);
}

#[tokio::test]
async fn search_get_item_and_what_is_new_name_items_by_id() {
    let (server, addr) = beside("mcp-search").await;
    user(&server, "alice", Role::Consumer).await;
    let mcp = Mcp::connect(addr, "alice", &["library:read"]).await;
    let found = mcp
        .call("search", json!({ "query": "alien" }))
        .await
        .unwrap();
    assert_eq!(found["top"]["id"], "plex:movie:1");
    assert_eq!(found["top"]["title"], "Alien");

    let show = mcp
        .call("get_item", json!({ "id": "plex:show:10" }))
        .await
        .unwrap();
    assert_eq!(show["item"]["title"], "The Expanse");
    assert_eq!(
        ids(&show["children"]),
        ["plex:season:20", "plex:season:11", "plex:season:14"]
    );
    assert_eq!(show["playlists"], json!([]));
    let missing = mcp
        .call("get_item", json!({ "id": "plex:movie:999" }))
        .await;
    assert!(missing.unwrap_err().contains("not in the library"));
    let wrong = mcp.call("get_item", json!({ "id": "nonsense" })).await;
    assert!(wrong.unwrap_err().contains("not an item id"));

    let recent = mcp.call("list_recent", json!({})).await.unwrap();
    assert_eq!(ids(&recent["items"]), ["plex:movie:1"]);
}

/// A disabled user's client stops at its next call, and cannot refresh.
#[tokio::test]
async fn disabling_the_user_ends_their_client_s_next_call() {
    let (server, addr) = beside("mcp-disabled").await;
    user(&server, "alice", Role::Consumer).await;
    let mcp = Mcp::connect(addr, "alice", &["library:read"]).await;
    assert_eq!(mcp.tools().await.len(), LIBRARY_TOOLS.len());
    server.store.disable_user("alice").await.unwrap();
    let refused = mcp
        .post(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }))
        .await;
    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(mcp.refreshed().await, StatusCode::BAD_REQUEST);
}

/// Each call counts toward the client's calls today and its last call,
/// and is logged with the user, the client and the tool.
#[tokio::test]
async fn every_call_is_counted_and_logged() {
    if !alone("mcp::every_call_is_counted_and_logged") {
        return;
    }
    let (server, addr) = beside("mcp-audit").await;
    let alice = user(&server, "alice", Role::Consumer).await;
    let mcp = Mcp::connect(addr, "alice", &["library:read"]).await;
    let (capture, _guard) = LogCapture::start();
    mcp.call("list_recent", json!({})).await.unwrap();
    mcp.call("get_item", json!({ "id": "plex:movie:999" }))
        .await
        .unwrap_err();
    let grant = server
        .store
        .oauth_access(&spoolframe_server::oauth::hash(&mcp.token))
        .await
        .unwrap()
        .unwrap();
    let (calls, last) = server
        .store
        .oauth_calls(&grant.grant_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(calls, 2);
    assert!(last.is_some());
    let logged = capture.saying("MCP call");
    assert_eq!(logged.len(), 2);
    let Principal::LocalUser(alice) = alice else {
        unreachable!()
    };
    for (line, (tool, outcome)) in logged
        .iter()
        .zip([("list_recent", "ok"), ("get_item", "failed")])
    {
        assert_eq!(line.target, spoolframe_server::mcp::AUDIT);
        assert_eq!(line.field("tool"), Some(tool));
        assert_eq!(line.field("outcome"), Some(outcome));
        assert_eq!(line.field("client"), Some("Claude"));
        assert_eq!(line.field("user"), Some(alice.to_string().as_str()));
    }
}

const PLAYLIST_TOOLS: [&str; 9] = [
    "list_playlists",
    "get_playlist",
    "create_playlist",
    "add_to_playlist",
    "remove_from_playlist",
    "move_in_playlist",
    "pin_playlist",
    "unpin_playlist",
    "set_watched",
];

const ADMIN_TOOLS: [&str; 7] = [
    "list_users",
    "disable_user",
    "list_sessions",
    "end_session",
    "list_playbacks",
    "stop_playback",
    "read_logs",
];

/// The ticket's own: "make a playlist of every heist episode" through
/// search and add_to_playlist makes it, the user's own, as their apps see
/// it.
#[tokio::test]
async fn a_playlist_of_every_heist_episode_is_made_through_search_and_add_to_playlist() {
    let (server, addr) = beside("mcp-heist").await;
    let alice = user(&server, "alice", Role::Consumer).await;
    let mcp = Mcp::connect(addr, "alice", &["library:read", "playlists:write"]).await;
    let found = mcp
        .call(
            "search",
            json!({ "query": spoolframe_server::test_support::HEIST }),
        )
        .await
        .unwrap();
    let episodes: Vec<String> = found["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|g| ids(&g["items"]))
        .filter(|id| id.starts_with("plex:episode:"))
        .chain(found["top"]["id"].as_str().map(str::to_owned))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    assert_eq!(episodes, ["plex:episode:12", "plex:episode:15"]);

    let made = mcp
        .call(
            "create_playlist",
            json!({ "name": "Heists", "kinds": ["episodes"] }),
        )
        .await
        .unwrap();
    let id = made["id"].as_str().unwrap();
    mcp.call(
        "add_to_playlist",
        json!({ "playlist": id, "items": episodes }),
    )
    .await
    .unwrap();
    let page = mcp
        .call("get_playlist", json!({ "playlist": id }))
        .await
        .unwrap();
    let items: Vec<String> = page["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["item"]["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(items, episodes);

    // The same playlist her apps list.
    let actor = spoolframe_server::principal::Actor {
        principal: alice,
        admin: false,
    };
    let (theirs, _) = server.playlists.list(&actor).await.unwrap();
    assert_eq!(theirs.len(), 1);
    assert_eq!(theirs[0].name, "Heists");
}

/// Without playlists:write the playlist tools are not listed, and a
/// direct call is refused.
#[tokio::test]
async fn without_the_scope_the_playlist_tools_are_absent_and_refused() {
    let (server, addr) = beside("mcp-no-scope").await;
    user(&server, "alice", Role::Consumer).await;
    let mcp = Mcp::connect(addr, "alice", &["library:read"]).await;
    let tools = mcp.tools().await;
    for tool in PLAYLIST_TOOLS.iter().chain(&ADMIN_TOOLS) {
        assert!(!tools.contains(&tool.to_string()), "{tool}");
    }
    let refused = mcp
        .call("create_playlist", json!({ "name": "Heists" }))
        .await
        .unwrap_err();
    assert!(refused.contains("playlists:write"), "{refused}");
    let actor = spoolframe_server::principal::Actor {
        principal: user(&server, "bob", Role::Consumer).await,
        admin: false,
    };
    // Nothing was made, for anyone.
    assert!(server.playlists.list(&actor).await.unwrap().0.is_empty());
}

/// With playlists:read alone an app reads playlists and changes none:
/// list_playlists and get_playlist are listed, the writing tools are not,
/// and a direct call to one is refused.
#[tokio::test]
async fn with_playlists_read_the_playlists_are_read_and_not_changed() {
    let (server, addr) = beside("mcp-read-only").await;
    let alice = user(&server, "alice", Role::Consumer).await;
    let actor = spoolframe_server::principal::Actor {
        principal: alice,
        admin: false,
    };
    let made = server
        .playlists
        .create(
            &actor,
            "Heists",
            spoolframe_protocol::playlists::Kinds {
                movies: false,
                episodes: true,
                tracks: false,
            },
            spoolframe_protocol::playlists::Visibility::Personal,
        )
        .await
        .unwrap();
    let mcp = Mcp::connect(addr, "alice", &["playlists:read"]).await;
    let tools = mcp.tools().await;
    for tool in ["list_playlists", "get_playlist"] {
        assert!(tools.contains(&tool.to_string()), "{tool}: {tools:?}");
    }
    for tool in PLAYLIST_TOOLS
        .iter()
        .filter(|t| !["list_playlists", "get_playlist"].contains(t))
    {
        assert!(!tools.contains(&tool.to_string()), "{tool}");
    }
    let listed = mcp.call("list_playlists", json!({})).await.unwrap();
    assert!(listed.to_string().contains("Heists"), "{listed}");
    let refused = mcp
        .call("create_playlist", json!({ "name": "Capers" }))
        .await
        .unwrap_err();
    assert!(refused.contains("playlists:write"), "{refused}");
    let (mine, _) = server.playlists.list(&actor).await.unwrap();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].id, made.id);
}

#[tokio::test]
async fn set_watched_marks_the_user_s_item() {
    let (server, addr) = beside("mcp-watched").await;
    user(&server, "alice", Role::Consumer).await;
    let mcp = Mcp::connect(addr, "alice", &["library:read", "playlists:write"]).await;
    mcp.call(
        "set_watched",
        json!({ "id": "plex:movie:1", "watched": true }),
    )
    .await
    .unwrap();
    let item = mcp
        .call("get_item", json!({ "id": "plex:movie:1" }))
        .await
        .unwrap();
    assert_eq!(item["item"]["state"]["watched"], true);
}

/// An admin's client with the admin scope lists the admin tools; the
/// ones that cannot be undone carry MCP's destructive annotation, so the
/// client asks first.
#[tokio::test]
async fn an_admin_s_tools_mark_what_cannot_be_undone() {
    let (server, addr) = beside("mcp-admin-tools").await;
    user(&server, "root", Role::Admin).await;
    let mcp = Mcp::connect(addr, "root", &["library:read", "admin"]).await;
    let listed = mcp.rpc("tools/list", json!({})).await;
    let tools = listed["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for tool in ADMIN_TOOLS {
        assert!(names.contains(&tool), "{tool}");
    }
    for tool in PLAYLIST_TOOLS {
        assert!(!names.contains(&tool), "{tool}");
    }
    let destructive = |name: &str| {
        tools.iter().find(|t| t["name"] == name).unwrap()["annotations"]["destructiveHint"].clone()
    };
    for tool in ["disable_user", "end_session", "stop_playback"] {
        assert_eq!(destructive(tool), json!(true), "{tool}");
    }
    assert_eq!(
        tools.iter().find(|t| t["name"] == "list_users").unwrap()["annotations"]["readOnlyHint"],
        true
    );
}

/// disable_user acts as userDisable does, and is kept in the audit list
/// with who did it through which client.
#[tokio::test]
async fn an_admin_tool_acts_and_is_recorded_in_the_audit_list() {
    let (server, addr) = beside("mcp-admin-audit").await;
    let root = user(&server, "root", Role::Admin).await;
    user(&server, "bob", Role::Consumer).await;
    let bob = signed_in(addr, "bob").await;
    let mcp = Mcp::connect(addr, "root", &["admin"]).await;
    let users = mcp.call("list_users", json!({})).await.unwrap();
    assert_eq!(users["users"].as_array().unwrap().len(), 2);
    let sessions = mcp
        .call("list_sessions", json!({ "username": "bob" }))
        .await
        .unwrap();
    assert_eq!(sessions["sessions"].as_array().unwrap().len(), 1);

    let done = mcp
        .call("disable_user", json!({ "username": "bob" }))
        .await
        .unwrap();
    assert_eq!(done["sessionsEnded"], 1);
    let me = browser()
        .get(format!("http://{addr}/auth/me"))
        .header("cookie", bob)
        .send()
        .await
        .unwrap();
    assert_eq!(me.status(), StatusCode::UNAUTHORIZED);
    let missing = mcp
        .call("stop_playback", json!({ "playback": "nope" }))
        .await
        .unwrap_err();
    assert!(missing.contains("nope"), "{missing}");

    let (events, total) = server.store.audit_page(0, 10).await.unwrap();
    // Four tool calls, after the app's connection.
    assert_eq!(total, 5);
    assert_eq!(events[4].action, "app.connect");
    let Principal::LocalUser(root) = root else {
        unreachable!()
    };
    let stop = &events[0];
    assert_eq!(stop.action, "stop_playback");
    assert!(stop.outcome.starts_with("failed"), "{}", stop.outcome);
    let disable = &events[1];
    assert_eq!(disable.action, "disable_user");
    assert_eq!(disable.actor, root.to_string());
    assert_eq!(disable.client, "Claude");
    assert_eq!(disable.target.as_deref(), Some("bob"));
    assert_eq!(disable.outcome, "ok");
}

/// The admin scope covers a user while they are an admin: demoted, their
/// client's admin tools go at once.
#[tokio::test]
async fn a_demoted_admin_s_client_loses_the_admin_tools_at_once() {
    let (server, addr) = beside("mcp-demoted").await;
    user(&server, "root", Role::Admin).await;
    user(&server, "carol", Role::Admin).await;
    let mcp = Mcp::connect(addr, "carol", &["library:read", "admin"]).await;
    assert!(mcp.tools().await.contains(&"list_users".to_owned()));
    server
        .store
        .set_role("carol", Role::Consumer)
        .await
        .unwrap();
    assert_eq!(mcp.tools().await, LIBRARY_TOOLS);
    let refused = mcp.call("list_users", json!({})).await.unwrap_err();
    assert!(refused.contains("admin"), "{refused}");
}

/// `name`, signed in on the admin API's socket.
async fn api(addr: SocketAddr, name: &str) -> jsonrpsee::ws_client::WsClient {
    let cookie = signed_in(addr, name).await;
    let token = cookie.split_once('=').unwrap().1;
    crate::admin::with_token(addr, token).await
}

/// My profile › Connected apps lists the client with its scopes, where it
/// returns and its calls today; revoking it ends its next call, and its
/// refresh token with it.
#[tokio::test]
async fn revoking_the_app_in_my_profile_ends_its_next_call() {
    use spoolframe_admin_api::MeApiClient;
    let (server, addr) = beside("mcp-revoke").await;
    user(&server, "alice", Role::Consumer).await;
    let mcp = Mcp::connect(addr, "alice", &["library:read"]).await;
    mcp.call("list_recent", json!({})).await.unwrap();
    let alice = api(addr, "alice").await;
    let apps = alice.connected_apps().await.unwrap();
    assert_eq!(apps.len(), 1);
    assert_eq!(apps[0].client_name, "Claude");
    assert_eq!(apps[0].return_host, "claude.ai");
    assert_eq!(apps[0].scopes, ["library:read"]);
    assert_eq!(apps[0].calls_today, 1);
    assert!(apps[0].last_call_at.is_some());
    assert_eq!(apps[0].username, "alice");

    alice
        .revoke_connected_app(apps[0].id.clone())
        .await
        .unwrap();
    let refused = mcp
        .post(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }))
        .await;
    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(mcp.refreshed().await, StatusCode::BAD_REQUEST);
    assert!(alice.connected_apps().await.unwrap().is_empty());
}

/// An admin sees and revokes anyone's; anyone else sees and revokes only
/// their own.
#[tokio::test]
async fn an_admin_revokes_another_user_s_app_and_a_user_cannot() {
    use spoolframe_admin_api::{AdminApiClient, MeApiClient, error_code};
    let (server, addr) = beside("mcp-admin-revoke").await;
    user(&server, "root", Role::Admin).await;
    user(&server, "alice", Role::Consumer).await;
    user(&server, "bob", Role::Consumer).await;
    let alices = Mcp::connect(addr, "alice", &["library:read"]).await;
    let bob = api(addr, "bob").await;
    assert_eq!(
        crate::admin::code(bob.all_connected_apps().await.unwrap_err()),
        error_code::FORBIDDEN
    );
    let root = api(addr, "root").await;
    let all = root.all_connected_apps().await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].username, "alice");
    assert_eq!(
        crate::admin::code(
            bob.revoke_connected_app(all[0].id.clone())
                .await
                .unwrap_err()
        ),
        error_code::NO_SUCH_APP
    );
    assert_eq!(alices.tools().await.len(), LIBRARY_TOOLS.len());
    root.revoke_any_connected_app(all[0].id.clone())
        .await
        .unwrap();
    let refused = alices
        .post(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }))
        .await;
    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);

    // The audit list has the connection and the revoke, by whom.
    let page = root.audit_list(0, 10).await.unwrap();
    let actions: Vec<(&str, &str, &str)> = page
        .events
        .iter()
        .map(|e| (e.action.as_str(), e.actor.as_str(), e.client.as_str()))
        .collect();
    assert_eq!(
        actions,
        [
            ("app.revoke", "root", "web"),
            ("app.connect", "alice", "web")
        ]
    );
    assert_eq!(page.events[0].target.as_deref(), Some("Claude (alice)"));
    assert_eq!(
        crate::admin::code(bob.audit_list(0, 10).await.unwrap_err()),
        error_code::FORBIDDEN
    );
}

/// Disabling the user from the admin API ends their app's next call.
#[tokio::test]
async fn disabling_the_user_from_the_admin_api_ends_their_app_s_next_call() {
    use spoolframe_admin_api::AdminApiClient;
    let (server, addr) = beside("mcp-user-disable").await;
    user(&server, "root", Role::Admin).await;
    user(&server, "alice", Role::Consumer).await;
    let mcp = Mcp::connect(addr, "alice", &["library:read"]).await;
    let root = api(addr, "root").await;
    root.user_disable("alice".into()).await.unwrap();
    let refused = mcp
        .post(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }))
        .await;
    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
}

/// The audit list shows an admin's tool call through their client.
#[tokio::test]
async fn the_audit_list_shows_an_admin_tool_call() {
    use spoolframe_admin_api::AdminApiClient;
    let (server, addr) = beside("mcp-audit-list").await;
    user(&server, "root", Role::Admin).await;
    user(&server, "bob", Role::Consumer).await;
    let mcp = Mcp::connect(addr, "root", &["admin"]).await;
    mcp.call("disable_user", json!({ "username": "bob" }))
        .await
        .unwrap();
    let root = api(addr, "root").await;
    let page = root.audit_list(0, 1).await.unwrap();
    assert_eq!(page.total, 2);
    let event = &page.events[0];
    assert_eq!(
        (
            event.action.as_str(),
            event.actor.as_str(),
            event.client.as_str(),
            event.target.as_deref(),
            event.outcome.as_str()
        ),
        ("disable_user", "root", "Claude", Some("bob"), "ok")
    );
}
