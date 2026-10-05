//! Watch state per user, and Home (P1-14), over a real Iroh connection.

use crate::support;

use std::time::Duration;

use spoolframe_protocol::{HomeScreen, Reply, Request, Response, SessionId, TopResult};
use spoolframe_server::principal::Principal;
use spoolframe_server::store::watch::WatchUpdate;
use spoolframe_types::{Item, MediaId, PlaybackState};
use support::{OldKey, TestClient, TestServer, episode, movie, second_copy};

async fn alice_and_bob(test: &str) -> (TestServer, TestClient, SessionId, SessionId) {
    let server = TestServer::start(test).await;
    server.add_user("alice", "correct horse", None).await;
    server.add_user("bob", "battery staple", None).await;
    let tv = server.client().await;
    tv.hello().await;
    let alice = tv.login("alice", "correct horse").await;
    let bob = tv.login("bob", "battery staple").await;
    (server, tv, alice, bob)
}

async fn progress(tv: &TestClient, who: &SessionId, id: &MediaId, secs: u64) {
    let response = tv
        .call(
            Some(who),
            Request::UpdatePlaybackProgress {
                id: id.clone(),
                position_ms: secs * 1000,
            },
        )
        .await;
    assert_eq!(response, Response::Ok(Reply::Done));
}

async fn mark_watched(tv: &TestClient, who: &SessionId, id: &MediaId) {
    let response = tv
        .call(Some(who), Request::MarkWatched { id: id.clone() })
        .await;
    assert_eq!(response, Response::Ok(Reply::Done));
}

async fn state_of(tv: &TestClient, who: &SessionId, id: &MediaId) -> PlaybackState {
    match tv.call(Some(who), Request::Item { id: id.clone() }).await {
        Response::Ok(Reply::Item {
            item: Item::Movie(m),
            ..
        }) => m.playback,
        Response::Ok(Reply::Item {
            item: Item::Episode(e),
            ..
        }) => e.playback,
        other => panic!("no item: {other:?}"),
    }
}

async fn home(tv: &TestClient, who: &SessionId) -> HomeScreen {
    match tv.call(Some(who), Request::Home).await {
        Response::Ok(Reply::Home(home)) => home,
        other => panic!("no home: {other:?}"),
    }
}

fn ids(items: &[Item]) -> Vec<String> {
    items.iter().map(|i| i.id().to_string()).collect()
}

#[tokio::test]
async fn each_user_has_their_own_progress() {
    let (server, tv, alice, bob) = alice_and_bob("two-users").await;
    let id = movie().id().clone();
    progress(&tv, &alice, &id, 600).await;
    assert_eq!(
        state_of(&tv, &alice, &id).await.position,
        Some(Duration::from_secs(600))
    );
    assert_eq!(state_of(&tv, &bob, &id).await, PlaybackState::default());
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn progress_past_the_threshold_marks_watched() {
    let (server, tv, alice, _) = alice_and_bob("threshold").await;
    let id = movie().id().clone();
    // The fake movie is 7020 s long: 95% is 6669 s.
    progress(&tv, &alice, &id, 6669).await;
    let state = state_of(&tv, &alice, &id).await;
    assert!(state.watched);
    assert_eq!(state.position, None);
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn under_a_minute_is_not_in_progress() {
    let (server, tv, alice, _) = alice_and_bob("not-started").await;
    let id = movie().id().clone();
    progress(&tv, &alice, &id, 30).await;
    assert_eq!(state_of(&tv, &alice, &id).await.position, None);
    assert!(home(&tv, &alice).await.continue_watching.is_empty());
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn fast_updates_are_coalesced() {
    let (server, tv, alice, _) = alice_and_bob("throttle").await;
    let id = movie().id().clone();
    progress(&tv, &alice, &id, 600).await;
    // Within the throttle: not written.
    progress(&tv, &alice, &id, 610).await;
    progress(&tv, &alice, &id, 620).await;
    assert_eq!(
        state_of(&tv, &alice, &id).await.position,
        Some(Duration::from_secs(600))
    );
    // Crossing into watched is always written.
    progress(&tv, &alice, &id, 6900).await;
    assert!(state_of(&tv, &alice, &id).await.watched);
    tv.close().await;
    server.stop().await;
}

/// The end of an item is throttled like the rest of it: one write at the
/// crossing into watched, not one per report after it.
#[tokio::test]
async fn fast_updates_after_the_crossing_into_watched_are_coalesced() {
    let (server, tv, alice, _) = alice_and_bob("throttle-watched").await;
    let id = movie().id().clone();
    progress(&tv, &alice, &id, 6900).await;
    progress(&tv, &alice, &id, 6910).await;
    progress(&tv, &alice, &id, 6920).await;
    let rows = server
        .store
        .all_watch(principal(&server, "alice").await)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].watched);
    assert_eq!(rows[0].position_ms, Some(6_900_000));
    tv.close().await;
    server.stop().await;
}

/// Episode 15 is 2640 s long, with credits from 2400 s to its end: without
/// them it would be watched at 95%, 2508 s.
#[tokio::test]
async fn an_item_is_watched_at_the_start_of_credits_that_run_to_its_end() {
    let (server, tv, alice, bob) = alice_and_bob("credits").await;
    let id = episode("15", "14", 2, 1).id().clone();
    progress(&tv, &alice, &id, 2399).await;
    let state = state_of(&tv, &alice, &id).await;
    assert!(!state.watched);
    assert_eq!(state.position, Some(Duration::from_secs(2399)));
    progress(&tv, &bob, &id, 2400).await;
    assert!(state_of(&tv, &bob, &id).await.watched);
    tv.close().await;
    server.stop().await;
}

/// The crossing into watched is written inside the throttle interval, and
/// is judged there by the same credits as the write before it.
#[tokio::test]
async fn the_crossing_at_the_credits_is_written_within_the_throttle() {
    let (server, tv, alice, _) = alice_and_bob("credits-throttle").await;
    let id = episode("15", "14", 2, 1).id().clone();
    progress(&tv, &alice, &id, 2380).await;
    // Before the credits: no crossing, so not written.
    progress(&tv, &alice, &id, 2390).await;
    let state = state_of(&tv, &alice, &id).await;
    assert!(!state.watched);
    assert_eq!(state.position, Some(Duration::from_secs(2380)));
    progress(&tv, &alice, &id, 2400).await;
    assert!(state_of(&tv, &alice, &id).await.watched);
    tv.close().await;
    server.stop().await;
}

/// An item with no such credits is watched at 95% and not before: episode
/// 12 is as long as episode 15, and at 2507 s it has 133 s left.
#[tokio::test]
async fn an_item_without_credits_that_run_to_its_end_is_watched_at_95_percent() {
    let (server, tv, alice, bob) = alice_and_bob("no-credits").await;
    let id = episode("12", "11", 1, 1).id().clone();
    progress(&tv, &alice, &id, 2507).await;
    let state = state_of(&tv, &alice, &id).await;
    assert!(!state.watched);
    assert_eq!(state.position, Some(Duration::from_secs(2507)));
    progress(&tv, &bob, &id, 2508).await;
    assert!(state_of(&tv, &bob, &id).await.watched);
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn continue_watching_is_newest_first() {
    let (server, tv, alice, _) = alice_and_bob("continue").await;
    let movie_id = movie().id().clone();
    let episode_id = episode("13", "11", 1, 2).id().clone();
    progress(&tv, &alice, &movie_id, 600).await;
    // updated_at has one-second resolution.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    progress(&tv, &alice, &episode_id, 300).await;
    let home = home(&tv, &alice).await;
    assert_eq!(
        ids(&home.continue_watching),
        [episode_id.to_string(), movie_id.to_string()]
    );
    let Item::Episode(e) = &home.continue_watching[0] else {
        panic!("not an episode");
    };
    assert_eq!(e.playback.position, Some(Duration::from_secs(300)));
    tv.close().await;
    server.stop().await;
}

fn season_id(key: &str) -> MediaId {
    MediaId::new(
        spoolframe_types::Namespace::Plex,
        spoolframe_types::MediaKind::Season,
        key,
    )
    .unwrap()
}

#[tokio::test]
async fn marking_a_season_watched_marks_its_episodes() {
    let (server, tv, alice, bob) = alice_and_bob("mark-season").await;
    mark_watched(&tv, &alice, &season_id("11")).await;
    for e in [episode("12", "11", 1, 1), episode("13", "11", 1, 2)] {
        assert!(state_of(&tv, &alice, e.id()).await.watched, "{}", e.id());
    }
    // Only that season, and only for alice.
    assert!(
        !state_of(&tv, &alice, episode("15", "14", 2, 1).id())
            .await
            .watched
    );
    assert!(
        !state_of(&tv, &bob, episode("12", "11", 1, 1).id())
            .await
            .watched
    );
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn marking_a_show_watched_marks_every_episode_and_unmarking_clears_them() {
    let (server, tv, alice, _) = alice_and_bob("mark-show").await;
    let show = support::show().id().clone();
    let all = [
        episode("21", "20", 0, 1),
        episode("12", "11", 1, 1),
        episode("13", "11", 1, 2),
        episode("15", "14", 2, 1),
    ];
    mark_watched(&tv, &alice, &show).await;
    for e in &all {
        assert!(state_of(&tv, &alice, e.id()).await.watched, "{}", e.id());
    }
    let response = tv
        .call(
            Some(&alice),
            Request::MarkUnwatched {
                id: season_id("11"),
            },
        )
        .await;
    assert_eq!(response, Response::Ok(Reply::Done));
    assert!(!state_of(&tv, &alice, all[1].id()).await.watched);
    assert!(!state_of(&tv, &alice, all[2].id()).await.watched);
    assert!(state_of(&tv, &alice, all[3].id()).await.watched);
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn next_up_crosses_a_season_boundary() {
    let (server, tv, alice, _) = alice_and_bob("next-up").await;
    mark_watched(&tv, &alice, episode("12", "11", 1, 1).id()).await;
    mark_watched(&tv, &alice, episode("13", "11", 1, 2).id()).await;
    let home = home(&tv, &alice).await;
    // Season 2, episode 1; specials (season 0) are skipped.
    assert_eq!(ids(&home.next_up), ["plex:episode:15"]);
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn next_up_is_after_the_latest_watched_episode() {
    let (server, tv, alice, _) = alice_and_bob("next-up-gap").await;
    // Watched E2 but not E1: next up is what follows E2.
    mark_watched(&tv, &alice, episode("13", "11", 1, 2).id()).await;
    assert_eq!(ids(&home(&tv, &alice).await.next_up), ["plex:episode:15"]);
    tv.close().await;
    server.stop().await;
}

/// A rewatch: the whole show was watched once, and the person is back at
/// the start. Next up follows what they watched last, not the finale.
#[tokio::test]
async fn next_up_follows_the_most_recently_watched_episode() {
    let (server, tv, alice, _) = alice_and_bob("next-up-rewatch").await;
    mark_watched(&tv, &alice, episode("15", "14", 2, 1).id()).await;
    // updated_at has one-second resolution.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    mark_watched(&tv, &alice, episode("12", "11", 1, 1).id()).await;
    assert_eq!(ids(&home(&tv, &alice).await.next_up), ["plex:episode:13"]);
    tv.close().await;
    server.stop().await;
}

async fn principal(server: &TestServer, username: &str) -> Principal {
    let user = server
        .store
        .password_login(username)
        .await
        .unwrap()
        .unwrap();
    Principal::LocalUser(user.id)
}

/// The same film in two libraries: two items with the same external ids.
/// Each keeps its own state.
#[tokio::test]
async fn a_second_copy_of_a_film_does_not_take_the_first_copy_s_state() {
    let (server, tv, alice, _) = alice_and_bob("two-copies").await;
    let first = movie().id().clone();
    let second = second_copy().id().clone();
    progress(&tv, &alice, &first, 600).await;
    assert_eq!(
        state_of(&tv, &alice, &second).await,
        PlaybackState::default()
    );
    assert_eq!(
        state_of(&tv, &alice, &first).await.position,
        Some(Duration::from_secs(600))
    );
    assert_eq!(
        ids(&home(&tv, &alice).await.continue_watching),
        [first.to_string()]
    );
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_second_copy_of_a_film_does_not_take_the_first_copy_s_hide() {
    let (server, tv, alice, _) = alice_and_bob("two-copies-hide").await;
    let first = movie().id().clone();
    progress(&tv, &alice, &first, 600).await;
    hide(&tv, &alice, &first).await;
    state_of(&tv, &alice, second_copy().id()).await;
    let hidden = server
        .store
        .hidden(principal(&server, "alice").await)
        .await
        .unwrap();
    assert_eq!(hidden, [first.to_string()].into());
    tv.close().await;
    server.stop().await;
}

/// The key the film had before Plex gave it a new one.
fn old_key() -> MediaId {
    "plex:movie:99".parse().unwrap()
}

/// Alice is 10 minutes into the film, under its old key.
async fn started_under_the_old_key(server: &TestServer) {
    let external_ids = [format!("test://{}", movie().id().key())];
    server
        .store
        .save_watch(
            principal(server, "alice").await,
            WatchUpdate {
                media_id: &old_key().to_string(),
                show_id: None,
                external_ids: &external_ids,
                position_ms: Some(600_000),
                duration_ms: Some(7_020_000),
                watched: false,
                unhide: true,
            },
        )
        .await
        .unwrap();
}

/// Plex rebuilt its database and the keys started over: the film's old
/// key is another film's now.
#[tokio::test]
async fn state_under_a_key_that_now_holds_another_film_moves_to_the_new_key() {
    let (server, tv, alice, _) = alice_and_bob("relink-other-film").await;
    let id = movie().id().clone();
    started_under_the_old_key(&server).await;
    server.catalog.set_old_key(
        &old_key(),
        OldKey::AnotherItem(vec!["test://another-film".into()]),
    );
    assert_eq!(
        state_of(&tv, &alice, &id).await.position,
        Some(Duration::from_secs(600))
    );
    tv.close().await;
    server.stop().await;
}

/// The old key is a show's now: the item still opens, with its state.
#[tokio::test]
async fn state_under_a_key_that_now_holds_another_kind_moves_to_the_new_key() {
    let (server, tv, alice, _) = alice_and_bob("relink-other-kind").await;
    let id = movie().id().clone();
    started_under_the_old_key(&server).await;
    server.catalog.set_old_key(&old_key(), OldKey::AnotherKind);
    assert_eq!(
        state_of(&tv, &alice, &id).await.position,
        Some(Duration::from_secs(600))
    );
    tv.close().await;
    server.stop().await;
}

/// The old key is one this catalog does not serve (the state was written
/// with another catalog, P9).
#[tokio::test]
async fn state_under_another_catalog_s_key_moves_to_the_new_key() {
    let (server, tv, alice, _) = alice_and_bob("relink-foreign").await;
    let id = movie().id().clone();
    started_under_the_old_key(&server).await;
    server.catalog.set_old_key(&old_key(), OldKey::Foreign);
    assert_eq!(
        state_of(&tv, &alice, &id).await.position,
        Some(Duration::from_secs(600))
    );
    tv.close().await;
    server.stop().await;
}

/// A catalog that cannot say what the old key holds: the item opens
/// without the state, the state stays where it is, and it moves at the
/// next look once the catalog answers.
#[tokio::test]
async fn state_stays_while_the_catalog_cannot_answer_for_the_old_key() {
    let (server, tv, alice, _) = alice_and_bob("relink-unreachable").await;
    let id = movie().id().clone();
    started_under_the_old_key(&server).await;
    server.catalog.set_old_key(&old_key(), OldKey::Unreachable);
    assert_eq!(state_of(&tv, &alice, &id).await, PlaybackState::default());
    let rows = server
        .store
        .all_watch(principal(&server, "alice").await)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].media_id, old_key().to_string());

    server.catalog.forget_old_key(&old_key());
    assert_eq!(
        state_of(&tv, &alice, &id).await.position,
        Some(Duration::from_secs(600))
    );
    tv.close().await;
    server.stop().await;
}

/// The second copy has no state, so each look at it asks whether the
/// first copy's key still holds the film: with the narrow call, and the
/// answer is kept for the next look.
#[tokio::test]
async fn looks_at_a_second_copy_ask_about_the_first_copy_once() {
    let (server, tv, alice, _) = alice_and_bob("two-copies-asked-once").await;
    let first = movie().id().clone();
    let second = second_copy().id().clone();
    progress(&tv, &alice, &first, 600).await;
    server.catalog.take_calls(&first);
    state_of(&tv, &alice, &second).await;
    state_of(&tv, &alice, &second).await;
    assert_eq!(server.catalog.take_calls(&first), ["external_ids"]);
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn pages_and_searches_carry_the_user_s_state() {
    let (server, tv, alice, bob) = alice_and_bob("filled").await;
    let id = movie().id().clone();
    progress(&tv, &alice, &id, 600).await;
    let library = support::library().id;
    let Response::Ok(Reply::Items { items, .. }) = tv
        .call(
            Some(&alice),
            Request::Items {
                library: library.clone(),
                offset: 0,
                limit: 10,
            },
        )
        .await
    else {
        panic!("no page");
    };
    let Item::Movie(m) = &items[0] else {
        panic!("not a movie")
    };
    assert_eq!(m.playback.position, Some(Duration::from_secs(600)));

    let searched = |session| {
        let tv = &tv;
        async move {
            let Response::Ok(Reply::Search(results)) = tv
                .call(Some(session), Request::Search(support::search("alien")))
                .await
            else {
                panic!("no search");
            };
            let Some(TopResult::Item(Item::Movie(m))) = results.top else {
                panic!("not a movie")
            };
            m.playback
        }
    };
    assert_eq!(
        searched(&alice).await.position,
        Some(Duration::from_secs(600))
    );
    assert_eq!(
        searched(&bob).await,
        PlaybackState::default(),
        "bob has no progress"
    );
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn home_lists_recently_added() {
    let (server, tv, alice, _) = alice_and_bob("recent").await;
    assert_eq!(
        ids(&home(&tv, &alice).await.recently_added),
        [movie().id().to_string()]
    );
    tv.close().await;
    server.stop().await;
}

async fn hide(tv: &TestClient, who: &SessionId, id: &MediaId) {
    let response = tv
        .call(
            Some(who),
            Request::HideFromContinueWatching { id: id.clone() },
        )
        .await;
    assert_eq!(response, Response::Ok(Reply::Done));
}

#[tokio::test]
async fn a_hidden_movie_leaves_only_that_user_s_home_and_keeps_its_state() {
    let (server, tv, alice, bob) = alice_and_bob("hide-movie").await;
    let id = movie().id().clone();
    progress(&tv, &alice, &id, 600).await;
    progress(&tv, &bob, &id, 600).await;
    hide(&tv, &alice, &id).await;
    assert!(home(&tv, &alice).await.continue_watching.is_empty());
    assert_eq!(
        ids(&home(&tv, &bob).await.continue_watching),
        [id.to_string()]
    );
    // Resume still resumes.
    assert_eq!(
        state_of(&tv, &alice, &id).await.position,
        Some(Duration::from_secs(600))
    );
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn playing_a_hidden_movie_brings_it_back() {
    let (server, tv, alice, _) = alice_and_bob("hide-play").await;
    let id = movie().id().clone();
    progress(&tv, &alice, &id, 600).await;
    hide(&tv, &alice, &id).await;
    // Within the write throttle of the last update: not written, but played.
    progress(&tv, &alice, &id, 610).await;
    assert_eq!(
        ids(&home(&tv, &alice).await.continue_watching),
        [id.to_string()]
    );
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_hidden_episode_hides_its_show_until_an_episode_is_played() {
    let (server, tv, alice, _) = alice_and_bob("hide-show").await;
    let watched = episode("12", "11", 1, 1).id().clone();
    let started = episode("13", "11", 1, 2).id().clone();
    mark_watched(&tv, &alice, &watched).await;
    progress(&tv, &alice, &started, 300).await;
    let before = home(&tv, &alice).await;
    assert_eq!(ids(&before.continue_watching), [started.to_string()]);
    assert!(!before.next_up.is_empty());

    hide(&tv, &alice, &started).await;
    let hidden = home(&tv, &alice).await;
    assert!(hidden.continue_watching.is_empty());
    assert!(hidden.next_up.is_empty());
    // Recently added is not Continue watching.
    assert!(!hidden.recently_added.is_empty());

    // Another episode of the show, marked: the show is back.
    mark_watched(&tv, &alice, &watched).await;
    let back = home(&tv, &alice).await;
    assert_eq!(ids(&back.continue_watching), [started.to_string()]);
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn unhide_restores_at_once() {
    let (server, tv, alice, _) = alice_and_bob("unhide").await;
    let id = movie().id().clone();
    progress(&tv, &alice, &id, 600).await;
    hide(&tv, &alice, &id).await;
    let response = tv
        .call(
            Some(&alice),
            Request::UnhideFromContinueWatching { id: id.clone() },
        )
        .await;
    assert_eq!(response, Response::Ok(Reply::Done));
    assert_eq!(
        ids(&home(&tv, &alice).await.continue_watching),
        [id.to_string()]
    );
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_hide_survives_a_restart() {
    let (server, tv, alice, _) = alice_and_bob("hide-restart").await;
    let id = movie().id().clone();
    progress(&tv, &alice, &id, 600).await;
    hide(&tv, &alice, &id).await;
    tv.close().await;
    let server = server.restart().await;
    let tv = server.client().await;
    tv.hello().await;
    let alice = tv.login("alice", "correct horse").await;
    assert!(home(&tv, &alice).await.continue_watching.is_empty());
    tv.close().await;
    server.stop().await;
}
