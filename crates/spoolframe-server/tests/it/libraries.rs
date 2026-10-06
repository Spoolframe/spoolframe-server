//! Plex libraries as a library source (P3-11): the libraries chosen from
//! one or more Plex servers, each under its Spoolframe name, served to
//! clients as one set, through the client protocol as a client sees them.
//! Each Plex server is a stand-in serving a few movies.

use std::sync::Arc;

use serde_json::json;
use spoolframe_protocol::HomeScreen;
use spoolframe_protocol::{
    Error, Reply, Request, Response, SearchRequest, SearchResults, SessionId,
};
use spoolframe_server::catalog::LiveCatalog;
use spoolframe_server::config::layers::{Layers, Locations};
use spoolframe_server::config::secrets::Secrets;
use spoolframe_server::principal::Principal;
use spoolframe_server::runtime::{Live, RuntimeSettings};
use spoolframe_server::store::watch::WatchUpdate;
use spoolframe_types::{Item, Library, LibraryKind, MediaId, SearchKind, SearchScope};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::support::{self, ServerOptions, TestClient, TestServer, mkv_capabilities};

/// A movie on a stand-in Plex server.
#[derive(Clone, Copy)]
struct Film {
    key: &'static str,
    section: u32,
    title: &'static str,
    added_at: i64,
    /// Plex's own id for the film: the same on two servers for one film.
    guid: &'static str,
    /// Its part's id, when Plex has made a preview index of it (P3-13).
    indexed_part: Option<u64>,
}

const fn film(key: &'static str, section: u32, title: &'static str, added_at: i64) -> Film {
    Film {
        key,
        section,
        title,
        added_at,
        guid: title,
        indexed_part: None,
    }
}

impl Film {
    fn metadata(&self) -> serde_json::Value {
        json!({
            "ratingKey": self.key,
            "type": "movie",
            "title": self.title,
            "addedAt": self.added_at,
            "librarySectionID": self.section,
            "guid": format!("plex://movie/{}", self.guid),
            "duration": 2_644_000,
            "Media": [{ "Part": [match self.indexed_part {
                Some(id) => json!({
                    "id": id,
                    "file": format!("/data/{}.mkv", self.title),
                    "indexes": "sd",
                }),
                None => json!({ "file": format!("/data/{}.mkv", self.title) }),
            }] }],
        })
    }
}

fn container(body: serde_json::Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({ "MediaContainer": body }))
}

/// A Plex server called `name` with these sections (key, Plex type,
/// title) and films.
async fn pms(name: &str, sections: &[(u32, &str, &str)], films: &[Film]) -> MockServer {
    pms_with(name, sections, films, None).await
}

/// As [`pms`], answering only requests with `token`, when there is one.
async fn pms_with(
    name: &str,
    sections: &[(u32, &str, &str)],
    films: &[Film],
    token: Option<&str>,
) -> MockServer {
    let pms = MockServer::start().await;
    let get = |at: String, body: serde_json::Value| {
        let mock = Mock::given(method("GET")).and(path(at));
        match token {
            Some(token) => mock.and(header("X-Plex-Token", token)),
            None => mock,
        }
        .respond_with(container(body))
    };
    if token.is_some() {
        // Without the token: what Plex answers.
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401))
            .with_priority(10)
            .mount(&pms)
            .await;
    }
    get(
        "/".into(),
        json!({ "friendlyName": name, "version": "1.41" }),
    )
    .mount(&pms)
    .await;
    let directories: Vec<_> = sections
        .iter()
        .map(|(key, kind, title)| json!({ "key": key.to_string(), "type": kind, "title": title }))
        .collect();
    get(
        "/library/sections".into(),
        json!({ "Directory": directories }),
    )
    .mount(&pms)
    .await;
    for (key, kind, title) in sections {
        let inside: Vec<_> = films
            .iter()
            .filter(|f| f.section == *key)
            .map(Film::metadata)
            .collect();
        // Every film matches any search; other kinds find nothing.
        let found = if *kind == "movie" {
            inside.clone()
        } else {
            Vec::new()
        };
        get(
            format!("/library/sections/{key}/search"),
            json!({ "totalSize": found.len(), "Metadata": found }),
        )
        .mount(&pms)
        .await;
        get(
            format!("/library/sections/{key}/all"),
            json!({ "totalSize": inside.len(), "librarySectionTitle": title, "Metadata": inside }),
        )
        .mount(&pms)
        .await;
    }
    for f in films {
        get(
            format!("/library/metadata/{}", f.key),
            json!({ "librarySectionID": f.section, "Metadata": [f.metadata()] }),
        )
        .mount(&pms)
        .await;
    }
    let mut newest: Vec<&Film> = films.iter().collect();
    newest.sort_by_key(|f| std::cmp::Reverse(f.added_at));
    let newest: Vec<_> = newest.iter().map(|f| f.metadata()).collect();
    get(
        "/library/recentlyAdded".into(),
        json!({ "Metadata": newest }),
    )
    .mount(&pms)
    .await;
    // Nobody is in these films, and anybody is in all of them.
    get("/hubs/search".into(), json!({ "Hub": [] }))
        .mount(&pms)
        .await;
    let all: Vec<_> = films.iter().map(Film::metadata).collect();
    get("/library/all".into(), json!({ "Metadata": all }))
        .mount(&pms)
        .await;
    pms
}

/// The server, its settings changed as an admin's are, and a client signed
/// in as alice.
struct Served {
    server: TestServer,
    settings: RuntimeSettings,
    client: TestClient,
    session: SessionId,
}

impl Served {
    /// A server on the provided file `file`, which may name `{dir}`, its
    /// state directory.
    async fn start(test: &str, file: &str) -> Self {
        Self::start_with(test, file, ServerOptions::default()).await
    }

    /// [`Served::start`], the server otherwise run as `options` say.
    async fn start_with(test: &str, file: &str, options: ServerOptions) -> Self {
        let dir = support::fresh_dir(test).await;
        let file_path = dir.join("server.ucl");
        std::fs::write(
            &file_path,
            file.replace("{dir}", &dir.display().to_string()),
        )
        .unwrap();
        let layers = Layers::load(&Locations {
            state_dir: dir.to_path_buf(),
            config: Some(file_path),
        })
        .unwrap();
        let config = layers.config().unwrap();
        let plex = spoolframe_server::runtime::plex_catalog(&config, &layers, &Secrets::new(&dir))
            .unwrap();
        let live = Arc::new(LiveCatalog::new(plex.map(Arc::new)));
        let settings = RuntimeSettings::new(
            layers,
            &dir,
            Live {
                catalog: live.clone(),
                ..Live::default()
            },
            false,
        );
        let options = ServerOptions {
            catalog: Some(live),
            ..options
        };
        let server = TestServer::start_with(dir, options).await;
        server.add_user("alice", "correct horse", None).await;
        let client = server.client().await;
        client.hello_with(None, Some(mkv_capabilities())).await;
        let session = client.login("alice", "correct horse").await;
        Self {
            server,
            settings,
            client,
            session,
        }
    }

    /// A file Plex reports as `/data/<title>.mkv`, under `server`'s
    /// mapping: `{dir}/<server>/<title>.mkv`.
    fn write_film(&self, server: &str, title: &str, bytes: usize) {
        let file = self.server.dir.join(server).join(format!("{title}.mkv"));
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, vec![7u8; bytes]).unwrap();
    }

    async fn call(&self, request: Request) -> Response {
        self.client.call(Some(&self.session), request).await
    }

    async fn libraries(&self) -> Vec<Library> {
        match self.call(Request::Libraries).await {
            Response::Ok(Reply::Libraries { libraries }) => libraries,
            other => panic!("{other:?}"),
        }
    }

    async fn items(&self, library: &str) -> Result<Vec<String>, Error> {
        let request = Request::Items {
            library: library.parse().unwrap(),
            offset: 0,
            limit: 50,
        };
        match self.call(request).await {
            Response::Ok(Reply::Items { items, .. }) => Ok(ids(&items)),
            Response::Err(e) => Err(e),
            other => panic!("{other:?}"),
        }
    }

    async fn item(&self, id: &str) -> Result<Item, Error> {
        match self.call(Request::Item { id: media(id) }).await {
            Response::Ok(Reply::Item { item, .. }) => Ok(item),
            Response::Err(e) => Err(e),
            other => panic!("{other:?}"),
        }
    }

    /// Every movie: "See all" movies, for any text.
    async fn search(&self) -> Vec<String> {
        let request = SearchRequest {
            kind: Some(SearchKind::Movies),
            ..support::search("a")
        };
        ids(&self.search_with(request).await.groups[0].items)
    }

    async fn search_with(&self, request: SearchRequest) -> SearchResults {
        match self.call(Request::Search(request)).await {
            Response::Ok(Reply::Search(results)) => results,
            other => panic!("{other:?}"),
        }
    }

    async fn home(&self) -> HomeScreen {
        match self.call(Request::Home).await {
            Response::Ok(Reply::Home(home)) => home,
            other => panic!("{other:?}"),
        }
    }

    /// Start `id` and read it whole: the bytes of the file it plays.
    async fn play(&self, id: &str) -> u64 {
        let request = Request::StartPlayback {
            id: media(id),
            selection: None,
            start_ms: None,
            bitmap: Default::default(),
        };
        match self.call(request).await {
            Response::Ok(Reply::PlaybackStarted { size, .. }) => size,
            other => panic!("{id} does not play: {other:?}"),
        }
    }

    /// Set `libraries` as an admin does.
    fn set_libraries(&self, libraries: serde_json::Value) {
        self.settings.set("libraries", &libraries, "root").unwrap();
    }
}

fn media(id: &str) -> MediaId {
    id.parse().unwrap()
}

fn ids(items: &[Item]) -> Vec<String> {
    items.iter().map(|i| i.id().to_string()).collect()
}

fn library(id: &str, title: &str, kind: LibraryKind) -> Library {
    Library {
        id: id.parse().unwrap(),
        title: title.into(),
        kind,
    }
}

/// Tower: four libraries, three of them of movies.
const TOWER_SECTIONS: &[(u32, &str, &str)] = &[
    (1, "movie", "Movies"),
    (2, "movie", "Kids"),
    (3, "artist", "Music"),
    (4, "movie", "Documentaries"),
];

const TOWER_FILMS: &[Film] = &[
    film("11", 1, "Alien", 100),
    film("21", 2, "Bambi", 400),
    film("41", 4, "Baraka", 300),
];

/// The config for Tower, and Cabin at `cabin`, with these libraries.
fn two_servers(cabin: &str, libraries: &str) -> String {
    format!(
        r#"
        plex {{
          url = "{{tower}}";
          name = "Tower";
          path_mapping [ {{ from = "/data"; to = "{{dir}}/tower"; }} ]
          servers [
            {{ id = "cabin"; name = "Cabin"; url = "{cabin}";
               path_mapping [ {{ from = "/data"; to = "{{dir}}/cabin"; }} ] }}
          ]
        }}
        libraries {libraries}
        "#
    )
}

const FILMS_AND_CABIN: &str = r#"[
  { name = "Films"; kind = "movies"; plex { server = "plex"; section = "1"; } },
  { name = "Cabin movies"; kind = "movies"; plex { server = "cabin"; section = "1"; } },
]"#;

fn films_and_cabin() -> serde_json::Value {
    json!([
        { "name": "Films", "kind": "movies", "plex": { "server": "plex", "section": "1" } },
        { "name": "Cabin movies", "kind": "movies", "plex": { "server": "cabin", "section": "1" } },
    ])
}

#[tokio::test]
async fn only_the_chosen_libraries_are_listed_under_their_spoolframe_names() {
    let tower = pms("Tower", TOWER_SECTIONS, TOWER_FILMS).await;
    let file = format!(
        r#"
        plex {{ url = "{}"; }}
        libraries [
          {{ name = "Films"; kind = "movies"; plex {{ server = "plex"; section = "1"; }} }},
          {{ name = "Docs"; kind = "movies"; plex {{ server = "plex"; section = "4"; }} }},
        ]
        "#,
        tower.uri()
    );
    let served = Served::start("two-of-four", &file).await;
    assert_eq!(
        served.libraries().await,
        [
            library("plex:1", "Films", LibraryKind::Movies),
            library("plex:4", "Docs", LibraryKind::Movies),
        ]
    );
    assert_eq!(served.items("plex:4").await.unwrap(), ["plex:movie:41"]);
    // Kids is not a library here: not listed, not found, not searched.
    assert!(matches!(
        served.items("plex:2").await,
        Err(Error::NotFound { .. })
    ));
    assert!(matches!(
        served.item("plex:movie:21").await,
        Err(Error::NotFound { .. })
    ));
    assert_eq!(served.search().await, ["plex:movie:11", "plex:movie:41"]);
    assert_eq!(
        ids(&served.home().await.recently_added),
        ["plex:movie:41", "plex:movie:11"]
    );
}

#[tokio::test]
async fn a_library_from_a_second_plex_server_mixes_into_home_and_search_and_plays() {
    let tower = pms("Tower", TOWER_SECTIONS, TOWER_FILMS).await;
    // The same key as a film on Tower: the server tells them apart.
    let cabin = pms(
        "Cabin",
        &[(1, "movie", "Movies")],
        &[film("11", 1, "Arrival", 200)],
    )
    .await;
    let file = two_servers(&cabin.uri(), FILMS_AND_CABIN).replace("{tower}", &tower.uri());
    let served = Served::start("second-server", &file).await;
    assert_eq!(
        served.libraries().await,
        [
            library("plex:1", "Films", LibraryKind::Movies),
            library("plex:cabin:1", "Cabin movies", LibraryKind::Movies),
        ]
    );
    assert_eq!(
        served.items("plex:cabin:1").await.unwrap(),
        ["plex:movie:cabin:11"]
    );
    let Item::Movie(arrival) = served.item("plex:movie:cabin:11").await.unwrap() else {
        panic!("not a movie");
    };
    assert_eq!(arrival.title, "Arrival");
    assert_eq!(
        served.search().await,
        ["plex:movie:11", "plex:movie:cabin:11"]
    );
    // Newest first, whichever server: Arrival (200) after Bambi is left
    // out with Kids, and before Alien (100).
    assert_eq!(
        ids(&served.home().await.recently_added),
        ["plex:movie:cabin:11", "plex:movie:11"]
    );
    served.write_film("tower", "Alien", 1000);
    served.write_film("cabin", "Arrival", 2000);
    assert_eq!(served.play("plex:movie:11").await, 1000);
    assert_eq!(served.play("plex:movie:cabin:11").await, 2000);
}

/// How many searches `pms` was asked.
async fn searches(pms: &MockServer) -> usize {
    pms.received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().ends_with("/search"))
        .count()
}

#[tokio::test]
async fn a_search_counts_every_server_s_results_and_a_library_asks_only_its_own() {
    let tower = pms("Tower", TOWER_SECTIONS, TOWER_FILMS).await;
    let cabin = pms(
        "Cabin",
        &[(1, "movie", "Movies")],
        &[
            film("11", 1, "Arrival", 200),
            film("12", 1, "Annie Hall", 300),
        ],
    )
    .await;
    let file = two_servers(&cabin.uri(), FILMS_AND_CABIN).replace("{tower}", &tower.uri());
    let served = Served::start("search-servers", &file).await;

    let everything = served
        .search_with(SearchRequest {
            scope: SearchScope::Movies,
            ..support::search("a")
        })
        .await;
    // Alien on Tower, two on Cabin.
    assert_eq!(everything.counts.movies, Some(3));

    let (tower_before, cabin_before) = (searches(&tower).await, searches(&cabin).await);
    let in_cabin = served
        .search_with(SearchRequest {
            scope: SearchScope::Movies,
            kind: Some(SearchKind::Movies),
            library: Some("plex:cabin:1".parse().unwrap()),
            ..support::search("a")
        })
        .await;
    assert_eq!(
        ids(&in_cabin.groups[0].items),
        ["plex:movie:cabin:11", "plex:movie:cabin:12"]
    );
    assert_eq!(searches(&tower).await, tower_before);
    assert!(searches(&cabin).await > cabin_before);

    // A person is on one server: their items are that server's.
    let person = Request::PersonItems {
        person: "plex:cabin:actor-7".parse().unwrap(),
        offset: 0,
        limit: 10,
    };
    match served.call(person).await {
        Response::Ok(Reply::Items { items, total }) => {
            assert_eq!(ids(&items), ["plex:movie:cabin:11", "plex:movie:cabin:12"]);
            assert_eq!(total, 2);
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn with_one_plex_server_stopped_only_its_libraries_are_unavailable() {
    let tower = pms("Tower", TOWER_SECTIONS, TOWER_FILMS).await;
    // Nothing listens there: a Plex server that is stopped.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let cabin = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let file = two_servers(&cabin, FILMS_AND_CABIN).replace("{tower}", &tower.uri());
    let served = Served::start("server-down", &file).await;
    // Still listed, under its name, and unavailable when opened.
    assert_eq!(served.libraries().await.len(), 2);
    assert!(
        matches!(
            served.items("plex:cabin:1").await,
            Err(Error::Unavailable { .. })
        ),
        "{:?}",
        served.items("plex:cabin:1").await
    );
    assert_eq!(served.items("plex:1").await.unwrap(), ["plex:movie:11"]);
    assert_eq!(served.search().await, ["plex:movie:11"]);
    // Alice was halfway through a film of each when Cabin stopped: Home
    // still shows Tower's.
    let alice = served
        .server
        .store
        .password_login("alice")
        .await
        .unwrap()
        .unwrap();
    for id in ["plex:movie:cabin:11", "plex:movie:11"] {
        let update = WatchUpdate {
            media_id: id,
            show_id: None,
            external_ids: &[],
            position_ms: Some(600_000),
            duration_ms: Some(2_644_000),
            watched: false,
            unhide: true,
        };
        let principal = Principal::LocalUser(alice.id);
        served
            .server
            .store
            .save_watch(principal, update)
            .await
            .unwrap();
    }
    let home = served.home().await;
    assert_eq!(ids(&home.continue_watching), ["plex:movie:11"]);
    assert_eq!(ids(&home.recently_added), ["plex:movie:11"]);
}

#[tokio::test]
async fn removing_a_library_and_adding_it_back_keeps_its_watch_state() {
    let mut copy = film("11", 1, "Arrival", 200);
    copy.guid = "arrival";
    // Tower has the same film, in Films: a copy, with state of its own.
    let mut tower_copy = film("12", 1, "Arrival copy", 50);
    tower_copy.guid = "arrival";
    let mut films = TOWER_FILMS.to_vec();
    films.push(tower_copy);
    let tower = pms("Tower", TOWER_SECTIONS, &films).await;
    let cabin = pms("Cabin", &[(1, "movie", "Movies")], &[copy]).await;
    let file = two_servers(&cabin.uri(), FILMS_AND_CABIN).replace("{tower}", &tower.uri());
    let served = Served::start("remove-and-add", &file).await;
    served.write_film("cabin", "Arrival", 10);
    served.write_film("tower", "Arrival copy", 10);
    let Response::Ok(Reply::PlaybackStarted { handle, .. }) = served
        .call(Request::StartPlayback {
            id: media("plex:movie:cabin:11"),
            selection: Some(Default::default()),
            start_ms: None,
            bitmap: Default::default(),
        })
        .await
    else {
        panic!("progress needs a playback")
    };
    let progress = Request::UpdatePlaybackProgress {
        handle,
        sequence: 1,
        position_ms: 600_000,
    };
    assert_eq!(served.call(progress).await, Response::Ok(Reply::Done));
    let before = served.item("plex:movie:cabin:11").await.unwrap();
    let Item::Movie(m) = &before else { panic!() };
    assert!(m.playback.position.is_some());
    assert_eq!(
        ids(&served.home().await.continue_watching),
        ["plex:movie:cabin:11"]
    );

    // Removed: gone for clients.
    served.set_libraries(json!([
        { "name": "Films", "kind": "movies", "plex": { "server": "plex", "section": "1" } },
    ]));
    assert_eq!(served.libraries().await.len(), 1);
    assert!(matches!(
        served.item("plex:movie:cabin:11").await,
        Err(Error::NotFound { .. })
    ));
    assert!(served.home().await.continue_watching.is_empty());
    // Watching the copy meanwhile does not take the removed one's state.
    let watched = Request::MarkWatched {
        id: media("plex:movie:12"),
    };
    assert_eq!(served.call(watched).await, Response::Ok(Reply::Done));

    // Added back: where it was.
    served.set_libraries(films_and_cabin());
    assert_eq!(served.item("plex:movie:cabin:11").await.unwrap(), before);
    assert_eq!(
        ids(&served.home().await.continue_watching),
        ["plex:movie:cabin:11"]
    );
}

#[tokio::test]
async fn a_server_configured_before_libraries_serves_all_of_its_plex_libraries() {
    // The user's dev.ucl and the NixOS module's file have this shape.
    let tower = pms("Tower", TOWER_SECTIONS, TOWER_FILMS).await;
    let file = format!(
        r#"plex {{ url = "{}"; path_mapping = [ {{ from = "/data"; to = "/mnt"; }} ]; }}"#,
        tower.uri()
    );
    let served = Served::start("before-libraries", &file).await;
    assert_eq!(
        served.libraries().await,
        [
            library("plex:1", "Movies", LibraryKind::Movies),
            library("plex:2", "Kids", LibraryKind::Movies),
            library("plex:3", "Music", LibraryKind::Music),
            library("plex:4", "Documentaries", LibraryKind::Movies),
        ]
    );
    assert_eq!(served.items("plex:2").await.unwrap(), ["plex:movie:21"]);
}

// The admin API's side (P3-11): what Server › Libraries, the add library
// flow and the wizard change.

use spoolframe_admin_api::{AdminApiClient, error_code};

use crate::admin::code;
use crate::settings::served as admin_served;

fn ids_of(overview: &spoolframe_admin_api::LibrariesOverview) -> Vec<(String, String)> {
    overview
        .libraries
        .iter()
        .map(|l| (l.id.clone(), l.name.clone()))
        .collect()
}

fn pair(id: &str, name: &str) -> (String, String) {
    (id.to_owned(), name.to_owned())
}

/// `/data` mapped to a folder under `dir` holding `films`' files: a
/// library is added only when its files are on this machine.
fn files_here(
    dir: &std::path::Path,
    films: &[&str],
) -> Vec<spoolframe_admin_api::PathMappingEntry> {
    let media = dir.join("media");
    std::fs::create_dir_all(&media).unwrap();
    for title in films {
        std::fs::write(media.join(format!("{title}.mkv")), b"film").unwrap();
    }
    vec![spoolframe_admin_api::PathMappingEntry {
        library: None,
        from: "/data".into(),
        to: media.display().to_string(),
    }]
}

#[tokio::test]
async fn an_admin_chooses_renames_orders_and_removes_libraries() {
    let tower = pms("Tower", TOWER_SECTIONS, TOWER_FILMS).await;
    let file = format!(r#"plex {{ url = "{}"; }}"#, tower.uri());
    let (_addr, root, dir) = admin_served("choose-libraries", &file).await;

    // Not chosen yet: every library of Tower, under Plex's names.
    let overview = root.libraries().await.unwrap();
    assert!(!overview.chosen);
    assert_eq!(
        ids_of(&overview),
        [
            pair("plex:1", "Movies"),
            pair("plex:2", "Kids"),
            pair("plex:3", "Music"),
            pair("plex:4", "Documentaries"),
        ]
    );
    let kids = &overview.libraries[1];
    assert_eq!(
        (kids.items, kids.plex_title.as_deref()),
        (Some(1), Some("Kids"))
    );
    assert_eq!(overview.servers[0].name, "Tower");

    // The first change chooses them all, then makes itself.
    root.library_remove("plex:2".into()).await.unwrap();
    root.library_rename("plex:4".into(), "Docs".into())
        .await
        .unwrap();
    root.library_move("plex:4".into(), 0).await.unwrap();
    let overview = root.libraries().await.unwrap();
    assert!(overview.chosen);
    assert_eq!(
        ids_of(&overview),
        [
            pair("plex:4", "Docs"),
            pair("plex:1", "Movies"),
            pair("plex:3", "Music"),
        ]
    );
    let saved = std::fs::read_to_string(dir.as_ref().join("runtime.ucl")).unwrap();
    assert!(saved.contains("Docs"), "{saved}");

    // Kids comes back under Plex's name, last; once is enough.
    root.plex_server_set_paths("plex".into(), files_here(dir.as_ref(), &["Bambi"]))
        .await
        .unwrap();
    let id = root
        .library_add("plex".into(), "2".into(), None)
        .await
        .unwrap();
    assert_eq!(id, "plex:2");
    let again = root.library_add("plex".into(), "2".into(), None).await;
    assert_eq!(code(again.unwrap_err()), error_code::INVALID_INPUT);
    let overview = root.libraries().await.unwrap();
    assert_eq!(overview.libraries.last().unwrap().name, "Kids");
    // Plex's own sections say which library each is.
    let marked: Vec<_> = overview.servers[0]
        .status
        .libraries
        .iter()
        .map(|s| s.library.clone())
        .collect();
    assert_eq!(
        marked,
        [
            Some("plex:1".into()),
            Some("plex:2".into()),
            Some("plex:3".into()),
            Some("plex:4".into()),
        ]
    );
}

/// Each library says which way it syncs (P3-12): PX-04's config syncs
/// the first server's libraries both ways until an admin says otherwise.
#[tokio::test]
async fn each_library_s_watch_sync_is_shown_and_chosen() {
    use spoolframe_admin_api::SyncDirection;
    let tower = pms("Tower", TOWER_SECTIONS, TOWER_FILMS).await;
    let file = format!(
        r#"plex {{ url = "{}"; watch_sync {{ user = "andoriyu"; }} }}"#,
        tower.uri()
    );
    let (_addr, root, dir) = admin_served("library-sync", &file).await;
    let syncs = |overview: &spoolframe_admin_api::LibrariesOverview| -> Vec<SyncDirection> {
        overview.libraries.iter().map(|l| l.sync).collect()
    };
    assert_eq!(
        syncs(&root.libraries().await.unwrap()),
        [SyncDirection::Both; 4]
    );
    root.library_set_sync("plex:2".into(), SyncDirection::FromPlex)
        .await
        .unwrap();
    assert_eq!(
        syncs(&root.libraries().await.unwrap()),
        [
            SyncDirection::Both,
            SyncDirection::FromPlex,
            SyncDirection::Both,
            SyncDirection::Both
        ]
    );
    let saved = std::fs::read_to_string(dir.as_ref().join("runtime.ucl")).unwrap();
    assert!(saved.contains("sync = \"from_plex\";"), "{saved}");
    // Only the one chosen says so: the others keep following the default.
    assert_eq!(saved.matches("sync =").count(), 1, "{saved}");

    // Without PX-04's owner, nothing syncs until chosen.
    let plain = format!(r#"plex {{ url = "{}"; }}"#, tower.uri());
    let (_addr, root, _dir) = admin_served("library-sync-off", &plain).await;
    assert_eq!(
        syncs(&root.libraries().await.unwrap()),
        [SyncDirection::Off; 4]
    );
}

#[tokio::test]
async fn a_second_plex_server_is_added_with_the_first_one_s_token_and_removed_with_its_libraries() {
    let tower = pms_with("Tower", TOWER_SECTIONS, TOWER_FILMS, Some("t0k")).await;
    let cabin = pms_with(
        "Cabin",
        &[(1, "movie", "Movies"), (2, "photo", "Photos")],
        &[film("11", 1, "Arrival", 200)],
        Some("t0k"),
    )
    .await;
    let file = format!(r#"plex {{ url = "{}"; }}"#, tower.uri());
    let (_addr, root, dir) = admin_served("add-server", &file).await;
    root.setting_set("plex.token".into(), json!("t0k"))
        .await
        .unwrap();

    let wrong = root.plex_server_add(cabin.uri(), Some("nope".into())).await;
    assert_eq!(code(wrong.unwrap_err()), error_code::UNAVAILABLE);
    // The same Plex account's token works on its other servers.
    let added = root.plex_server_add(cabin.uri(), None).await.unwrap();
    assert_eq!((added.id.as_str(), added.name.as_str()), ("cabin", "Cabin"));
    assert!(added.status.reachable && added.token_set);
    assert_eq!(added.status.unsupported[0].title, "Photos");
    let secret = dir.as_ref().join("secrets").join("plex-token-cabin");
    assert_eq!(std::fs::read_to_string(&secret).unwrap().trim(), "t0k");
    let twice = root.plex_server_add(cabin.uri(), None).await;
    assert_eq!(code(twice.unwrap_err()), error_code::INVALID_INPUT);

    root.plex_server_set_paths("cabin".into(), files_here(dir.as_ref(), &["Arrival"]))
        .await
        .unwrap();
    let id = root
        .library_add("cabin".into(), "1".into(), Some("Cabin movies".into()))
        .await
        .unwrap();
    assert_eq!(id, "plex:cabin:1");
    let photos = root.library_add("cabin".into(), "2".into(), None).await;
    assert_eq!(code(photos.unwrap_err()), error_code::INVALID_INPUT);
    let overview = root.libraries().await.unwrap();
    let library = overview.libraries.iter().find(|l| l.id == id).unwrap();
    assert_eq!(library.server_name, "Cabin");
    assert_eq!(library.items, Some(1));

    let first = root.plex_server_remove("plex".into()).await;
    assert_eq!(code(first.unwrap_err()), error_code::INVALID_INPUT);
    root.plex_server_remove("cabin".into()).await.unwrap();
    let overview = root.libraries().await.unwrap();
    assert_eq!(overview.servers.len(), 1);
    assert!(overview.libraries.iter().all(|l| l.server == "plex"));
    assert!(!secret.exists());
}

/// Whether alice has watched `id`, as a client is told.
async fn watched(served: &Served, id: &str) -> bool {
    match served.item(id).await.unwrap() {
        Item::Movie(m) => m.playback.watched,
        other => panic!("not a movie: {other:?}"),
    }
}

#[tokio::test]
async fn the_first_plex_server_is_removed_only_when_alone_and_its_watch_state_is_kept() {
    let tower = pms("Tower", TOWER_SECTIONS, TOWER_FILMS).await;
    let cabin = pms(
        "Cabin",
        &[(1, "movie", "Movies")],
        &[film("11", 1, "Arrival", 200)],
    )
    .await;
    // Set up as the wizard does: nothing in the file, the server added at
    // run time.
    let served = Served::start("remove-first", "").await;
    let settings = &served.settings;
    let first = settings
        .plex_server_add(&tower.uri(), None, "root")
        .await
        .unwrap();
    assert_eq!(first, "plex");
    let watch = Request::MarkWatched {
        id: media("plex:movie:11"),
    };
    assert_eq!(served.call(watch).await, Response::Ok(Reply::Done));
    assert!(watched(&served, "plex:movie:11").await);

    // Another server's items would answer to the first one's ids if it
    // became the first: refused while there is one, with the reason.
    settings
        .plex_server_add(&cabin.uri(), None, "root")
        .await
        .unwrap();
    let refused = settings.plex_server_remove("plex", "root").unwrap_err();
    assert!(refused.to_string().contains("Cabin"), "{refused}");
    assert!(watched(&served, "plex:movie:11").await);

    // Alone, it goes, and the server is not set up again.
    settings.plex_server_remove("cabin", "root").unwrap();
    settings.plex_server_remove("plex", "root").unwrap();
    assert!(matches!(
        served.call(Request::Libraries).await,
        Response::Err(Error::NotSetUp)
    ));
    let runtime = std::fs::read_to_string(served.server.dir.join("runtime.ucl")).unwrap();
    assert!(!runtime.contains("plex"), "{runtime}");
    // Its watch state is kept, under the ids its items had.
    let alice = served
        .server
        .store
        .password_login("alice")
        .await
        .unwrap()
        .unwrap();
    let kept = served
        .server
        .store
        .all_watch(Principal::LocalUser(alice.id))
        .await
        .unwrap();
    assert_eq!(
        kept.iter()
            .map(|r| (r.media_id.as_str(), r.watched))
            .collect::<Vec<_>>(),
        [("plex:movie:11", true)]
    );

    // Added back, it is where it was.
    settings
        .plex_server_add(&tower.uri(), None, "root")
        .await
        .unwrap();
    assert!(watched(&served, "plex:movie:11").await);
}

#[tokio::test]
async fn the_first_plex_server_a_config_file_names_is_not_removed_at_run_time() {
    let tower = pms("Tower", TOWER_SECTIONS, TOWER_FILMS).await;
    let file = format!(r#"plex {{ url = "{}"; }}"#, tower.uri());
    let served = Served::start("remove-first-file", &file).await;
    let refused = served
        .settings
        .plex_server_remove("plex", "root")
        .unwrap_err();
    assert!(refused.to_string().contains("config file"), "{refused}");
    assert_eq!(served.libraries().await.len(), 4);
}

#[tokio::test]
async fn a_plex_server_that_is_down_is_an_answer_in_the_overview() {
    let tower = pms("Tower", TOWER_SECTIONS, TOWER_FILMS).await;
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let cabin = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let file = two_servers(&cabin, FILMS_AND_CABIN)
        .replace("{tower}", &tower.uri())
        .replace("{dir}", "/nonexistent");
    let (_addr, root, _dir) = admin_served("server-down-overview", &file).await;
    let overview = root.libraries().await.unwrap();
    let cabin = &overview.servers[1];
    assert!(!cabin.status.reachable);
    assert_eq!(cabin.name, "Cabin");
    let [films, cabin_movies] = overview.libraries.as_slice() else {
        panic!("{overview:?}");
    };
    assert_eq!((films.items, films.error.as_ref()), (Some(1), None));
    assert_eq!(cabin_movies.name, "Cabin movies");
    assert!(cabin_movies.error.is_some() && cabin_movies.items.is_none());
}

#[tokio::test]
async fn a_down_server_s_libraries_keep_the_plex_titles_it_last_gave_after_a_restart() {
    let tower = pms("Tower", TOWER_SECTIONS, TOWER_FILMS).await;
    let cabin = pms("Cabin", &[(1, "movie", "Cabin films")], &[]).await;
    let file = two_servers(&cabin.uri(), FILMS_AND_CABIN)
        .replace("{tower}", &tower.uri())
        .replace("{dir}", "/nonexistent");
    let (_addr, root, dir) = admin_served("titles-kept", &file).await;
    let titles = |o: &spoolframe_admin_api::LibrariesOverview| {
        o.libraries
            .iter()
            .map(|l| l.plex_title.clone())
            .collect::<Vec<_>>()
    };
    let up = root.libraries().await.unwrap();
    assert_eq!(
        titles(&up),
        [Some("Movies".to_owned()), Some("Cabin films".to_owned())]
    );

    // Cabin stops answering, and the server starts again.
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .with_priority(1)
        .mount(&cabin)
        .await;
    let addr = crate::settings::serve_in(dir.as_ref(), &file, Default::default()).await;
    let anonymous = crate::admin::anonymous(addr).await;
    let login = spoolframe_admin_api::MeApiClient::login(
        &anonymous,
        crate::admin::creds("root", "correct horse"),
    )
    .await
    .unwrap();
    let root = crate::admin::with_token(addr, &login.token).await;
    let down = root.libraries().await.unwrap();
    assert!(!down.servers[1].status.reachable);
    assert_eq!(
        titles(&down),
        [Some("Movies".to_owned()), Some("Cabin films".to_owned())]
    );
}

#[tokio::test]
async fn libraries_and_servers_a_locked_file_sets_are_not_changed() {
    let tower = pms("Tower", TOWER_SECTIONS, TOWER_FILMS).await;
    let cabin = pms("Cabin", &[(1, "movie", "Movies")], &[]).await;
    let file = format!(
        r#"locked = true;
        plex {{ url = "{}"; servers [ {{ id = "attic"; url = "http://127.0.0.1:9"; }} ] }}
        libraries [ {{ name = "Films"; kind = "movies"; plex {{ server = "plex"; section = "1"; }} }} ]"#,
        tower.uri()
    );
    let (_addr, root, dir) = admin_served("locked-libraries", &file).await;
    assert!(root.libraries().await.unwrap().locked);
    let renamed = root.library_rename("plex:1".into(), "Mine".into()).await;
    assert_eq!(code(renamed.unwrap_err()), error_code::SETTING_LOCKED);
    let added = root.plex_server_add(cabin.uri(), Some("t0k".into())).await;
    assert_eq!(code(added.unwrap_err()), error_code::SETTING_LOCKED);
    // Nor is its token kept.
    assert!(
        !dir.as_ref()
            .join("secrets")
            .join("plex-token-cabin")
            .exists()
    );
}

#[tokio::test]
async fn a_library_about_to_be_added_has_its_paths_checked_first() {
    let tower = pms("Tower", TOWER_SECTIONS, TOWER_FILMS).await;
    let file = format!(
        r#"plex {{ url = "{}"; }}
        libraries [ {{ name = "Films"; kind = "movies"; plex {{ server = "plex"; section = "1"; }} }} ]"#,
        tower.uri()
    );
    let (_addr, root, dir) = admin_served("check-before-adding", &file).await;
    let media = dir.as_ref().join("media");
    std::fs::create_dir_all(&media).unwrap();
    std::fs::write(media.join("Baraka.mkv"), b"film").unwrap();
    let mapping = spoolframe_admin_api::PathMappingEntry {
        library: None,
        from: "/data".into(),
        to: media.display().to_string(),
    };
    // Documentaries (4) is not a library yet.
    let checked = root
        .plex_path_check(vec![mapping.clone()], None, Some(vec!["4".into()]))
        .await
        .unwrap();
    let found: Vec<_> = checked
        .iter()
        .map(|c| (c.library.as_str(), c.ok, c.checked))
        .collect();
    assert_eq!(found, [("Documentaries", 1, 1)]);
    // Without sections: the libraries served.
    let served = root
        .plex_path_check(vec![mapping], None, None)
        .await
        .unwrap();
    assert_eq!(served[0].library, "Movies");
}

/// Spoolframe plays the files itself: a library none of whose sample
/// files is on this machine is refused, saying which folder to mount,
/// and added once its files are found here.
#[tokio::test]
async fn a_library_whose_files_are_not_on_this_machine_is_not_added() {
    let tower = pms("Tower", TOWER_SECTIONS, TOWER_FILMS).await;
    let file = format!(
        r#"plex {{ url = "{}"; }}
        libraries [ {{ name = "Films"; kind = "movies"; plex {{ server = "plex"; section = "1"; }} }} ]"#,
        tower.uri()
    );
    let (_addr, root, dir) = admin_served("not-on-this-machine", &file).await;

    // Documentaries (4) is Baraka, at /data/Baraka.mkv: not here.
    let refused = root
        .library_add("plex".into(), "4".into(), None)
        .await
        .unwrap_err();
    let jsonrpsee::core::ClientError::Call(e) = refused else {
        panic!("not a call error: {refused}");
    };
    assert_eq!(
        (e.code(), e.message()),
        (
            error_code::INVALID_INPUT,
            "Documentaries can't be added: its sample file is not on this machine. \
             Mount its folder here, or map it to where it is."
        )
    );
    assert_eq!(root.libraries().await.unwrap().libraries.len(), 1);

    // Mapped to where it is here: added.
    root.plex_server_set_paths("plex".into(), files_here(dir.as_ref(), &["Baraka"]))
        .await
        .unwrap();
    let id = root
        .library_add("plex".into(), "4".into(), None)
        .await
        .unwrap();
    assert_eq!(id, "plex:4");
}

/// Plex's own preview index of a part (P3-13), on the Plex server the
/// film is on: read from there, and nothing is decoded (there is no
/// ffmpeg to decode with).
#[tokio::test]
async fn a_film_s_previews_come_from_its_own_plex_server_s_index() {
    let tower = pms("Tower", TOWER_SECTIONS, TOWER_FILMS).await;
    let arrival = Film {
        indexed_part: Some(7001),
        ..film("11", 1, "Arrival", 200)
    };
    let cabin = pms("Cabin", &[(1, "movie", "Movies")], &[arrival]).await;
    let bif = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../spoolframe-plex/tests/fixtures/index-sd-trimmed.bif"
    ))
    .unwrap();
    Mock::given(method("GET"))
        .and(path("/library/parts/7001/indexes/sd"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(bif, "application/octet-stream"))
        .expect(1)
        .mount(&cabin)
        .await;
    let file = two_servers(&cabin.uri(), FILMS_AND_CABIN).replace("{tower}", &tower.uri());
    let options = ServerOptions {
        previews: spoolframe_server::previews::PreviewSettings {
            when: spoolframe_server::previews::When::OnPlay,
            interval: std::time::Duration::from_secs(10),
            cache_limit_bytes: 1 << 30,
            workers: 1,
        },
        previews_ffmpeg: "/nonexistent/ffmpeg".into(),
        ..ServerOptions::default()
    };
    let served = Served::start_with("previews-from-plex", &file, options).await;
    served.write_film("cabin", "Arrival", 1000);
    let request = || Request::GetPreviews {
        id: media("plex:movie:cabin:11"),
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let previews = loop {
        match served.call(request()).await {
            Response::Ok(Reply::Previews(previews)) => break previews,
            Response::Err(Error::PreviewsNotReady { .. }) => {}
            other => panic!("{other:?}"),
        }
        assert!(std::time::Instant::now() < deadline, "no previews");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    // The index's frames are 320 × 132, and 4 s long: one tile at 10 s.
    assert_eq!((previews.tile_width, previews.tile_height), (320, 132));
    assert_eq!(previews.count, 1);
}
