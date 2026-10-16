//! Playlists in the web UI (PX-40): the admin API's `me.playlist*`
//! methods, over a real WebSocket, reaching the same `Playlists` service
//! the client protocol and MCP use. The rules themselves are tested in
//! `playlists::tests`; these check that each method acts as the session's
//! user, with that user's rights, and says each refusal with its code.

use std::net::SocketAddr;

use jsonrpsee::ws_client::WsClient;
use spoolframe_admin_api::{
    Credentials, MeApiClient, NewPlaylist, PlaylistChanges, PlaylistEditOp, PlaylistInfo,
    PlaylistKinds, PlaylistOwner, PlaylistShare, PlaylistSharing, error_code,
};
use spoolframe_types::Role;

use crate::admin::{anonymous, code, with_token};
use crate::mcp::{PASSWORD, beside, user};
use crate::support::TestServer;

const EPISODES: PlaylistKinds = PlaylistKinds {
    movies: false,
    episodes: true,
    tracks: false,
};

/// `name`'s own client, signed in with their password.
async fn as_user(addr: SocketAddr, name: &str) -> WsClient {
    let login = anonymous(addr)
        .await
        .login(Credentials {
            username: name.into(),
            password: PASSWORD.into(),
        })
        .await
        .unwrap();
    with_token(addr, &login.token).await
}

/// A server with root (an admin), alice, bob and carol (consumers): each
/// one's client, and their user ids, as playlists share by them.
struct Four {
    server: TestServer,
    addr: SocketAddr,
    root: WsClient,
    alice: WsClient,
    bob: WsClient,
    carol: WsClient,
    bob_id: String,
}

async fn four(test: &str) -> Four {
    let (server, addr) = beside(test).await;
    user(&server, "root", Role::Admin).await;
    user(&server, "alice", Role::Consumer).await;
    let bob = user(&server, "bob", Role::Consumer).await;
    user(&server, "carol", Role::Consumer).await;
    Four {
        root: as_user(addr, "root").await,
        alice: as_user(addr, "alice").await,
        bob: as_user(addr, "bob").await,
        carol: as_user(addr, "carol").await,
        bob_id: bob.to_string(),
        server,
        addr,
    }
}

fn new(name: &str, kinds: PlaylistKinds, sharing: PlaylistSharing) -> NewPlaylist {
    NewPlaylist {
        name: name.into(),
        kinds,
        sharing,
    }
}

fn add(id: &str) -> PlaylistEditOp {
    PlaylistEditOp::Add {
        id: id.into(),
        before: None,
    }
}

fn shared_with(user_id: &str, can_edit: bool) -> PlaylistSharing {
    PlaylistSharing::Shared {
        with: vec![PlaylistShare {
            user_id: user_id.into(),
            can_edit,
        }],
    }
}

/// The ids of the items in playlist `id`, as `client` sees them.
async fn items(client: &WsClient, id: &str) -> Vec<String> {
    client
        .playlist(id.into(), 0, 200)
        .await
        .unwrap()
        .entries
        .iter()
        .map(|e| e.item.id().to_string())
        .collect()
}

fn named<'a>(list: &'a [PlaylistInfo], name: &str) -> Option<&'a PlaylistInfo> {
    list.iter().find(|p| p.name == name)
}

#[tokio::test]
async fn a_read_only_share_cannot_edit_an_editor_can_and_a_third_user_does_not_see_it() {
    let f = four("web-playlists-sharing").await;
    let heists = f
        .alice
        .playlist_create(new("Heists", EPISODES, shared_with(&f.bob_id, false)))
        .await
        .unwrap();
    assert_eq!(heists.owner, PlaylistOwner::Mine);
    let revision = f
        .alice
        .playlist_edit(
            heists.id.clone(),
            heists.revision,
            vec![add("plex:episode:12")],
        )
        .await
        .unwrap();

    // Bob sees it, shared with him, and may not change it.
    let his = f.bob.playlists().await.unwrap();
    let seen = named(&his, "Heists").expect("bob sees alice's playlist");
    assert_eq!(seen.owner, PlaylistOwner::SharedWithMe);
    assert!(!seen.can_edit && !seen.can_manage, "{seen:?}");
    // Who else it is shared with is its owner's to know.
    assert_eq!(seen.sharing, PlaylistSharing::Shared { with: Vec::new() });
    assert_eq!(items(&f.bob, &heists.id).await, ["plex:episode:12"]);
    let refused = f
        .bob
        .playlist_edit(heists.id.clone(), revision, vec![add("plex:episode:13")])
        .await
        .unwrap_err();
    assert_eq!(code(refused), error_code::PLAYLIST_READ_ONLY);

    // Made an editor, he changes its entries, and nothing else of it.
    f.alice
        .playlist_update(
            heists.id.clone(),
            PlaylistChanges {
                sharing: Some(shared_with(&f.bob_id, true)),
                ..PlaylistChanges::default()
            },
        )
        .await
        .unwrap();
    let now = f.bob.playlist(heists.id.clone(), 0, 1).await.unwrap();
    assert!(now.playlist.can_edit && !now.playlist.can_manage);
    f.bob
        .playlist_edit(
            heists.id.clone(),
            now.playlist.revision,
            vec![add("plex:episode:13")],
        )
        .await
        .unwrap();
    assert_eq!(
        items(&f.alice, &heists.id).await,
        ["plex:episode:12", "plex:episode:13"]
    );
    let rename = f
        .bob
        .playlist_update(
            heists.id.clone(),
            PlaylistChanges {
                name: Some("Bob's heists".into()),
                ..PlaylistChanges::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(code(rename), error_code::PLAYLIST_READ_ONLY);
    let delete = f.bob.playlist_delete(heists.id.clone()).await.unwrap_err();
    assert_eq!(code(delete), error_code::PLAYLIST_READ_ONLY);

    // Carol neither lists it nor opens it.
    assert!(named(&f.carol.playlists().await.unwrap(), "Heists").is_none());
    let hidden = f
        .carol
        .playlist(heists.id.clone(), 0, 10)
        .await
        .unwrap_err();
    assert_eq!(code(hidden), error_code::NO_SUCH_PLAYLIST);

    // Its owner deletes it.
    f.alice.playlist_delete(heists.id.clone()).await.unwrap();
    assert!(f.bob.playlists().await.unwrap().is_empty());
    drop(f.server);
}

#[tokio::test]
async fn an_item_of_a_kind_not_ticked_is_refused_until_its_kind_is_added() {
    let f = four("web-playlists-kinds").await;
    let made = f
        .alice
        .playlist_create(new("Specials", EPISODES, PlaylistSharing::Personal))
        .await
        .unwrap();
    let film = f
        .alice
        .playlist_edit(made.id.clone(), made.revision, vec![add("plex:movie:1")])
        .await
        .unwrap_err();
    assert_eq!(code(film), error_code::KIND_NOT_ALLOWED);

    let wider = PlaylistKinds {
        movies: true,
        ..EPISODES
    };
    let widened = f
        .alice
        .playlist_update(
            made.id.clone(),
            PlaylistChanges {
                kinds: Some(wider),
                ..PlaylistChanges::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(widened.kinds, wider);
    f.alice
        .playlist_edit(made.id.clone(), widened.revision, vec![add("plex:movie:1")])
        .await
        .unwrap();
    assert_eq!(items(&f.alice, &made.id).await, ["plex:movie:1"]);

    // Kinds never narrow.
    let narrower = f
        .alice
        .playlist_update(
            made.id.clone(),
            PlaylistChanges {
                kinds: Some(EPISODES),
                ..PlaylistChanges::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(code(narrower), error_code::INVALID_INPUT);
}

#[tokio::test]
async fn a_server_wide_playlist_is_made_by_an_admin_seen_by_everyone_and_managed_by_admins() {
    let f = four("web-playlists-server-wide").await;
    let refused = f
        .alice
        .playlist_create(new("Everyone's", EPISODES, PlaylistSharing::ServerWide))
        .await
        .unwrap_err();
    assert_eq!(code(refused), error_code::FORBIDDEN);

    let made = f
        .root
        .playlist_create(new(
            "Top Gear specials",
            EPISODES,
            PlaylistSharing::ServerWide,
        ))
        .await
        .unwrap();
    assert_eq!(made.owner, PlaylistOwner::ServerWide);
    assert!(made.can_manage);
    f.root
        .playlist_edit(made.id.clone(), made.revision, vec![add("plex:season:11")])
        .await
        .unwrap();

    for consumer in [&f.alice, &f.bob, &f.carol] {
        let list = consumer.playlists().await.unwrap();
        let seen = named(&list, "Top Gear specials").expect("a server-wide playlist is seen");
        assert_eq!(seen.owner, PlaylistOwner::ServerWide);
        assert!(!seen.can_edit && !seen.can_manage, "{seen:?}");
        assert!(!items(consumer, &made.id).await.is_empty());
    }
    let edit = f
        .alice
        .playlist_edit(made.id.clone(), 0, vec![add("plex:episode:12")])
        .await
        .unwrap_err();
    assert_eq!(code(edit), error_code::PLAYLIST_READ_ONLY);

    // Another admin manages it too.
    user(&f.server, "dana", Role::Admin).await;
    let dana = as_user(f.addr, "dana").await;
    let renamed = dana
        .playlist_update(
            made.id.clone(),
            PlaylistChanges {
                name: Some("Specials, in airing order".into()),
                ..PlaylistChanges::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(renamed.name, "Specials, in airing order");
}

#[tokio::test]
async fn the_users_a_playlist_can_be_shared_with_are_everyone_else_here_by_name() {
    let f = four("web-playlists-candidates").await;
    f.server.store.disable_user("carol").await.unwrap();
    f.server
        .store
        .grant_portable(crate::portable_sign_in::identity(7), Some("Sam"), None)
        .await
        .unwrap();
    let candidates = f.alice.playlist_share_candidates().await.unwrap();
    let names: Vec<&str> = candidates.iter().map(|c| c.display_name.as_str()).collect();
    // Not alice herself, nor carol, who is disabled.
    assert_eq!(names, ["bob", "root", "Sam"]);
    let bob = candidates.iter().find(|c| c.display_name == "bob").unwrap();
    assert_eq!(bob.user_id, f.bob_id);
    let sam = candidates.iter().find(|c| c.display_name == "Sam").unwrap();
    assert!(sam.user_id.starts_with("portable:"), "{sam:?}");

    // Any of them is one a playlist can be shared with.
    f.alice
        .playlist_create(new("For Sam", EPISODES, shared_with(&sam.user_id, false)))
        .await
        .unwrap();
}

#[tokio::test]
async fn importing_from_plex_without_a_linked_account_says_so() {
    let f = four("web-playlists-no-plex").await;
    let refused = f.alice.playlist_plex_import(true).await.unwrap_err();
    assert_eq!(code(refused), error_code::NOT_CONFIGURED);
}

#[tokio::test]
async fn playlists_need_a_session() {
    let (_server, addr) = beside("web-playlists-anonymous").await;
    let refused = anonymous(addr).await.playlists().await.unwrap_err();
    assert_eq!(code(refused), error_code::UNAUTHENTICATED);
}

/// Alice's playlists on the stand-in Plex server: Heists (two films here,
/// one Plex no longer has, and a clip), and Photos, which is not a video
/// or music playlist.
async fn mount_plex_playlists(pms: &wiremock::MockServer) {
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    let film = |key: &str| {
        json!({ "ratingKey": key, "type": "movie", "title": format!("Film {key}"),
                "librarySectionID": 1, "duration": 3_600_000 })
    };
    // The films found here have a file, as a PMS lists one; the stand-in's
    // own answer has none, and a playlist's entries look for theirs.
    for key in ["11", "12"] {
        let mut with_file = film(key);
        with_file["Media"] =
            json!([{ "Part": [{ "file": format!("/data/movies/{key}.mkv"), "size": 1 }] }]);
        Mock::given(method("GET"))
            .and(path(format!("/library/metadata/{key}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "MediaContainer": { "Metadata": [with_file] } })),
            )
            .with_priority(1)
            .mount(pms)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/playlists"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "MediaContainer": {
            "Metadata": [
                { "ratingKey": "501", "type": "playlist", "title": "Heists", "playlistType": "video", "smart": false },
                { "ratingKey": "502", "type": "playlist", "title": "Photos", "playlistType": "photo" },
            ],
        } })))
        .mount(pms)
        .await;
    Mock::given(method("GET"))
        .and(path("/playlists/501/items"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "MediaContainer": {
            "Metadata": [
                film("11"),
                { "ratingKey": "700", "type": "clip", "title": "A trailer" },
                film("91"),
                film("12"),
            ],
        } })),
        )
        .mount(pms)
        .await;
}

#[tokio::test]
async fn a_plex_import_dry_run_says_what_it_would_make_and_the_import_makes_it() {
    use crate::plex_link::{link, mount_pin, reaching, root_and_alice};
    use crate::plex_sync::{library, mount_resources, tower};
    use spoolframe_admin_api::{AdminApiClient, PlexPlaylistMade};

    let pms = tower(&[(Some("alice-on-tower"), &[])]).await;
    mount_plex_playlists(&pms).await;
    let tv = wiremock::MockServer::start().await;
    mount_pin(&tv, 77, "tok-alice", "alice").await;
    mount_resources(&tv, &pms, &[("tok-alice", "alice-on-tower")]).await;
    let file = format!(
        "plex {{ url = \"{}\"; }} libraries [ {} ]",
        pms.uri(),
        library("Movies", "1", "both")
    );
    let dir = crate::admin::state_dir("web-playlists-plex-import").await;
    let (addr, _store) =
        crate::settings::serve_syncing_with(dir.as_ref(), &file, reaching(&tv), false, true).await;
    let (root, alice) = root_and_alice(addr).await;
    link(&alice).await;

    let dry = alice.playlist_plex_import(true).await.unwrap();
    assert_eq!(
        dry.made,
        [PlexPlaylistMade {
            name: "Heists".into(),
            playlist_id: None,
            entries: 2,
        }]
    );
    let mut left_out: Vec<(&str, Option<&str>)> = dry
        .unmapped
        .iter()
        .map(|u| (u.playlist.as_str(), u.title.as_deref()))
        .collect();
    left_out.sort();
    assert_eq!(
        left_out,
        [("Heists", Some("A trailer")), ("Heists", Some("Film 91"))],
        "{:?}",
        dry.unmapped
    );
    assert!(
        alice.playlists().await.unwrap().is_empty(),
        "a dry run makes nothing"
    );

    let done = alice.playlist_plex_import(false).await.unwrap();
    let id = done.made[0]
        .playlist_id
        .clone()
        .expect("the import made it");
    let made = alice.playlists().await.unwrap();
    assert_eq!(made.len(), 1);
    assert_eq!(made[0].owner, PlaylistOwner::Mine);
    assert_eq!(made[0].sharing, PlaylistSharing::Personal);
    assert_eq!(items(&alice, &id).await, ["plex:movie:11", "plex:movie:12"]);

    // An item's details list the playlists the viewer sees that hold it:
    // root does not see alice's, and sees the server-wide one he makes.
    let films = PlaylistKinds {
        movies: true,
        ..PlaylistKinds::default()
    };
    let everyone = root
        .playlist_create(new("Films for all", films, PlaylistSharing::ServerWide))
        .await
        .unwrap();
    root.playlist_edit(
        everyone.id.clone(),
        everyone.revision,
        vec![add("plex:movie:11")],
    )
    .await
    .unwrap();
    let item = root.catalog_item("plex:movie:11".into()).await.unwrap();
    let holding: Vec<&str> = item.reply["playlists"]
        .as_array()
        .expect("the item's playlists")
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(holding, ["Films for all"]);
}

/// PX-43 in the web UI: `me.like` fills the caller's Liked, which with
/// All comes first in `me.playlists`, marked built-in, and is neither
/// renamed nor deleted; All is not edited. One user's likes are not
/// another's.
#[tokio::test]
async fn liked_and_all_are_listed_first_and_me_like_fills_liked() {
    use spoolframe_admin_api::PlaylistBuiltin;
    let f = four("web-playlists-liked").await;
    f.server.catalog.serve_music();
    f.alice
        .playlist_create(new("Aardvarks", EPISODES, PlaylistSharing::Personal))
        .await
        .unwrap();
    let liked = f.alice.like(vec!["plex:album:31".into()]).await.unwrap();
    assert_eq!(liked.builtin, Some(PlaylistBuiltin::Liked));
    let list = f.alice.playlists().await.unwrap();
    let builtins: Vec<_> = list.iter().map(|p| p.builtin).collect();
    assert_eq!(
        builtins,
        [
            Some(PlaylistBuiltin::Liked),
            Some(PlaylistBuiltin::All),
            None
        ]
    );
    let page = f.alice.playlist(liked.id.clone(), 0, 10).await.unwrap();
    assert_eq!(page.entries.len(), 2);
    assert!(page.entries.iter().all(|e| e.liked));
    assert_eq!(
        items(
            &f.bob,
            &named(&f.bob.playlists().await.unwrap(), "Liked")
                .unwrap()
                .id
        )
        .await
        .len(),
        0
    );

    let rename = f
        .alice
        .playlist_update(
            liked.id.clone(),
            PlaylistChanges {
                name: Some("Loved".into()),
                ..PlaylistChanges::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(code(rename), error_code::PLAYLIST_READ_ONLY);
    let all = &list[1];
    let delete = f.alice.playlist_delete(all.id.clone()).await.unwrap_err();
    assert_eq!(code(delete), error_code::PLAYLIST_READ_ONLY);
    let edit = f
        .alice
        .playlist_edit(
            all.id.clone(),
            all.revision,
            vec![PlaylistEditOp::Remove { entry: 1 }],
        )
        .await
        .unwrap_err();
    assert_eq!(code(edit), error_code::PLAYLIST_READ_ONLY);
    assert_eq!(items(&f.alice, &all.id).await.len(), 3);
    let film = f.alice.like(vec!["plex:movie:1".into()]).await.unwrap_err();
    assert_eq!(code(film), error_code::KIND_NOT_ALLOWED);
}

/// Alice's rated tracks on the stand-in Plex server's music section (4):
/// 41, rated first, and 42, rated since; 44, not rated; 95, which Plex
/// no longer has.
async fn mount_plex_music(pms: &wiremock::MockServer) {
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, ResponseTemplate};
    let track = |key: &str, rating: f64, rated_at: i64| {
        json!({ "ratingKey": key, "type": "track", "title": format!("Track {key}"),
                "parentRatingKey": "40", "grandparentRatingKey": "39",
                "parentTitle": "Discovery", "grandparentTitle": "Daft Punk",
                "index": 1, "parentIndex": 1, "librarySectionID": 4,
                "userRating": rating, "lastRatedAt": rated_at,
                "Media": [{ "Part": [{ "file": format!("/data/music/{key}.flac"), "size": 1 }] }] })
    };
    Mock::given(method("GET"))
        .and(path("/library/sections"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "MediaContainer": {
            "Directory": [
                { "key": "1", "type": "movie", "title": "Movies" },
                { "key": "4", "type": "artist", "title": "Music" },
            ],
        } })),
        )
        .with_priority(1)
        .mount(pms)
        .await;
    Mock::given(method("GET"))
        .and(path("/library/sections/4/all"))
        .and(query_param("type", "10"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "MediaContainer": {
            "totalSize": 4,
            "Metadata": [
                track("41", 10.0, 100),
                track("42", 8.0, 200),
                track("44", 0.0, 0),
                track("95", 6.0, 50),
            ],
        } })),
        )
        .with_priority(1)
        .mount(pms)
        .await;
    for key in ["41", "42"] {
        Mock::given(method("GET"))
            .and(path(format!("/library/metadata/{key}")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    json!({ "MediaContainer": { "Metadata": [track(key, 0.0, 0)] } }),
                ),
            )
            .with_priority(1)
            .mount(pms)
            .await;
    }
}

/// PX-43: the import likes the tracks rated in Plex, the last rated
/// first, rather than making a playlist of them; a dry run likes none.
#[tokio::test]
async fn a_plex_import_likes_the_rated_tracks() {
    use crate::plex_link::{link, mount_pin, reaching, root_and_alice};
    use crate::plex_sync::{library, mount_resources, tower};
    use spoolframe_admin_api::{PlaylistBuiltin, PlexPlaylistMade};

    let pms = tower(&[(Some("alice-on-tower"), &[])]).await;
    mount_plex_playlists(&pms).await;
    mount_plex_music(&pms).await;
    let tv = wiremock::MockServer::start().await;
    mount_pin(&tv, 77, "tok-alice", "alice").await;
    mount_resources(&tv, &pms, &[("tok-alice", "alice-on-tower")]).await;
    let file = format!(
        "plex {{ url = \"{}\"; }} libraries [ {} {{ name = \"Music\"; kind = \"music\"; plex {{ server = \"plex\"; section = \"4\"; }} }} ]",
        pms.uri(),
        library("Movies", "1", "both")
    );
    let dir = crate::admin::state_dir("web-playlists-plex-liked").await;
    let (addr, _store) =
        crate::settings::serve_syncing_with(dir.as_ref(), &file, reaching(&tv), false, true).await;
    let (root, alice) = root_and_alice(addr).await;
    link(&alice).await;

    let dry = alice.playlist_plex_import(true).await.unwrap();
    let liked = dry.made.iter().find(|m| m.name == "Liked");
    assert_eq!(
        liked,
        Some(&PlexPlaylistMade {
            name: "Liked".into(),
            playlist_id: None,
            entries: 2,
        }),
        "{dry:?}"
    );
    assert!(
        dry.unmapped
            .iter()
            .any(|u| u.playlist == "Liked" && u.title.as_deref() == Some("Track 95")),
        "{:?}",
        dry.unmapped
    );

    let done = alice.playlist_plex_import(false).await.unwrap();
    let id = done
        .made
        .iter()
        .find(|m| m.name == "Liked")
        .and_then(|m| m.playlist_id.clone())
        .expect("Liked filled");
    let list = alice.playlists().await.unwrap();
    assert_eq!(list[0].id, id);
    assert_eq!(list[0].builtin, Some(PlaylistBuiltin::Liked));
    assert_eq!(items(&alice, &id).await, ["plex:track:42", "plex:track:41"]);
    // The Heists playlist is made as before.
    assert!(named(&list, "Heists").is_some());

    // An item's page shows the caller's own heart: root liked nothing.
    use spoolframe_admin_api::AdminApiClient;
    let track = root.catalog_item("plex:track:41".into()).await.unwrap();
    assert_eq!(track.reply["liked"], "none");
    root.like(vec!["plex:track:41".into()]).await.unwrap();
    let track = root.catalog_item("plex:track:41".into()).await.unwrap();
    assert_eq!(track.reply["liked"], "all");
}
