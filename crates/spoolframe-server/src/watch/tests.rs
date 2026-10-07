//! Watch rules, and watch state and Home (P1-14, P1-22) through [`Watch`]
//! itself: a store, the fake catalog and the stand-in ffprobe, no
//! connection. `tests/it/watch.rs` keeps the requests' wiring.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use spoolframe_catalog::Catalog;
use spoolframe_media::Prober;
use spoolframe_media::sandbox::Tool;
use spoolframe_protocol::HomeScreen;
use spoolframe_types::{Item, Marker, MarkerKind, MediaId, MediaKind, Namespace, PlaybackState};

use super::*;
use crate::clock::Clock;
use crate::principal::Principal;
use crate::store::watch::WatchUpdate;
use crate::test_support::{
    FakeCatalog, OldKey, episode, fake_ffprobe, movie, second_copy, show, write_episode,
};

#[test]
fn without_credits_an_item_is_watched_at_ninety_five_percent() {
    let rules = WatchRules::default();
    // 95% of 2 h is 114 min.
    let movie = Some(Duration::from_secs(7200));
    assert!(!rules.is_watched(Duration::from_secs(6839), movie, None));
    assert!(rules.is_watched(Duration::from_secs(6840), movie, None));
}

#[test]
fn time_left_does_not_make_an_item_watched() {
    let rules = WatchRules::default();
    // 44 min, with 4 minutes left: 91%.
    let episode = Some(Duration::from_secs(2640));
    assert!(!rules.is_watched(Duration::from_secs(2400), episode, None));
    // A 4-minute clip always has less than 5 minutes left.
    let clip = Some(Duration::from_secs(240));
    assert!(!rules.is_watched(Duration::from_secs(10), clip, None));
    assert!(!rules.is_watched(Duration::from_secs(227), clip, None));
    assert!(rules.is_watched(Duration::from_secs(228), clip, None));
}

#[test]
fn without_a_duration_nothing_is_watched_by_progress() {
    assert!(!WatchRules::default().is_watched(Duration::from_secs(99_999), None, None));
}

#[test]
fn with_credits_that_run_to_the_end_watched_is_at_their_start_exactly() {
    let rules = WatchRules::default();
    let episode = Some(Duration::from_secs(2640));
    // Short credits, from 98%: 95% (41:48) is not the point.
    let credits = Some(Duration::from_secs(2590));
    assert!(!rules.is_watched(Duration::from_secs(2589), episode, credits));
    assert!(rules.is_watched(Duration::from_secs(2590), episode, credits));
    // Long credits: before 95%.
    let long = Some(Duration::from_secs(1800));
    assert!(rules.is_watched(Duration::from_secs(1800), episode, long));
}

#[test]
fn credits_that_run_to_the_end_need_no_duration() {
    let rules = WatchRules::default();
    let credits = Some(Duration::from_secs(2400));
    assert!(rules.is_watched(Duration::from_secs(2400), None, credits));
    assert!(!rules.is_watched(Duration::from_secs(2399), None, credits));
}

fn marker(kind: MarkerKind, start_ms: u64, to_end: bool) -> Marker {
    Marker {
        kind,
        start_ms,
        end_ms: start_ms + 60_000,
        to_end,
    }
}

#[test]
fn only_credits_that_run_to_the_end_give_the_watched_point() {
    // An intro, and credits with a scene after them, do not.
    assert_eq!(
        credits_to_end(&[
            marker(MarkerKind::Intro, 30_000, false),
            marker(MarkerKind::Credits, 2_000_000, false),
        ]),
        None
    );
    assert_eq!(
        credits_to_end(&[
            marker(MarkerKind::Credits, 2_000_000, false),
            marker(MarkerKind::Credits, 2_400_000, true),
        ]),
        Some(Duration::from_secs(2400))
    );
}

/// [`Watch`] over a store of its own with alice and bob, the fake
/// catalog, and the stand-in ffprobe: the movie is probed as 7020 s long,
/// an episode's file as Brooklyn Nine-Nine S07E01 (1309.472 s).
struct Harness {
    // Declared first, so the store closes before its directory goes.
    watch: Watch,
    store: Store,
    catalog: Arc<FakeCatalog>,
    /// The store's, and so the watch's: moved on instead of sleeping.
    clock: Clock,
    media: PathBuf,
    alice: Principal,
    bob: Principal,
    _dir: spoolframe_test_dir::TestDir,
}

async fn harness(test: &str) -> Harness {
    let dir = crate::store::tests::state_dir(&format!("watch-{test}")).await;
    let clock = Clock::default();
    let store = Store::open(dir.as_ref())
        .await
        .unwrap()
        .with_clock(clock.clone());
    let media = dir.join("media").join("movie.mkv");
    let catalog = Arc::new(FakeCatalog::new(media.clone()));
    let ffprobe = fake_ffprobe(dir.as_ref(), "mkv-forced-subs.json");
    let prober = Arc::new(Prober::new(
        Tool::unconfined(ffprobe),
        Prober::DEFAULT_TIMEOUT,
    ));
    let watch = Watch::new(
        store.clone(),
        catalog.clone(),
        prober,
        WatchRules::default(),
    );
    let mut users = Vec::new();
    for name in ["alice", "bob"] {
        let id = store
            .add_user(name, None, "phc", spoolframe_types::Role::Consumer)
            .await
            .unwrap();
        users.push(Principal::LocalUser(id));
    }
    Harness {
        watch,
        store,
        catalog,
        clock,
        media,
        alice: users[0],
        bob: users[1],
        _dir: dir,
    }
}

impl Harness {
    /// `who` played `id` up to `secs`, as a progress report says: its
    /// file written first, as a playback needs it.
    async fn progress(&self, who: Principal, id: &MediaId, secs: u64) {
        std::fs::create_dir_all(self.media.parent().unwrap()).unwrap();
        std::fs::write(&self.media, b"stand-in").unwrap();
        write_episode(&self.media, id.key());
        self.watch
            .progress(who, id, Duration::from_secs(secs))
            .await
            .unwrap();
    }

    async fn mark(&self, who: Principal, id: &MediaId, watched: bool) {
        self.watch.mark(who, id, watched).await.unwrap();
    }

    /// `who`'s state on `id` as a client looking at it gets it: re-linked,
    /// then filled in.
    async fn state_of(&self, who: Principal, id: &MediaId) -> PlaybackState {
        let mut found = self.catalog.item(id).await.unwrap();
        self.watch
            .relink(who, id, &found.external_ids)
            .await
            .unwrap();
        self.watch.fill(who, &mut [&mut found.item]).await.unwrap();
        match found.item {
            Item::Movie(m) => m.playback,
            Item::Episode(e) => e.playback,
            other => panic!("no state: {other:?}"),
        }
    }

    async fn home(&self, who: Principal) -> HomeScreen {
        self.watch.home(who).await.unwrap()
    }

    async fn hide(&self, who: Principal, id: &MediaId) {
        self.watch.hide(who, id).await.unwrap();
    }
}

fn ids(items: &[Item]) -> Vec<String> {
    items.iter().map(|i| i.id().to_string()).collect()
}

fn season_id(key: &str) -> MediaId {
    MediaId::new(Namespace::Plex, MediaKind::Season, key).unwrap()
}

#[tokio::test]
async fn progress_past_the_threshold_marks_watched() {
    let h = harness("threshold").await;
    let id = movie().id().clone();
    // The fake movie is 7020 s long: 95% is 6669 s.
    h.progress(h.alice, &id, 6669).await;
    let state = h.state_of(h.alice, &id).await;
    assert!(state.watched);
    assert_eq!(state.position, None);
}

#[tokio::test]
async fn under_a_minute_is_not_in_progress() {
    let h = harness("not-started").await;
    let id = movie().id().clone();
    h.progress(h.alice, &id, 30).await;
    assert_eq!(h.state_of(h.alice, &id).await.position, None);
    assert!(h.home(h.alice).await.continue_watching.is_empty());
}

#[tokio::test]
async fn fast_updates_are_coalesced() {
    let h = harness("throttle").await;
    let id = movie().id().clone();
    h.progress(h.alice, &id, 600).await;
    // Within the throttle: not written.
    h.progress(h.alice, &id, 610).await;
    h.progress(h.alice, &id, 620).await;
    assert_eq!(
        h.state_of(h.alice, &id).await.position,
        Some(Duration::from_secs(600))
    );
    // Crossing into watched is always written.
    h.progress(h.alice, &id, 6900).await;
    assert!(h.state_of(h.alice, &id).await.watched);
}

/// Once the throttle interval has passed, the next report is written.
#[tokio::test]
async fn an_update_after_the_throttle_interval_is_written() {
    let h = harness("throttle-over").await;
    let id = movie().id().clone();
    h.progress(h.alice, &id, 600).await;
    h.clock
        .advance(h.watch.rules().throttle - Duration::from_secs(1));
    h.progress(h.alice, &id, 610).await;
    assert_eq!(
        h.state_of(h.alice, &id).await.position,
        Some(Duration::from_secs(600))
    );
    h.clock.advance(Duration::from_secs(1));
    h.progress(h.alice, &id, 620).await;
    assert_eq!(
        h.state_of(h.alice, &id).await.position,
        Some(Duration::from_secs(620))
    );
}

/// The end of an item is throttled like the rest of it: one write at the
/// crossing into watched, not one per report after it.
#[tokio::test]
async fn fast_updates_after_the_crossing_into_watched_are_coalesced() {
    let h = harness("throttle-watched").await;
    let id = movie().id().clone();
    h.progress(h.alice, &id, 6900).await;
    h.progress(h.alice, &id, 6910).await;
    h.progress(h.alice, &id, 6920).await;
    let rows = h.store.all_watch(h.alice).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].watched);
    assert_eq!(rows[0].position_ms, Some(6_900_000));
}

/// Episode 15 is 2640 s long, with credits from 2400 s to its end: without
/// them it would be watched at 95%, 2508 s.
#[tokio::test]
async fn an_item_is_watched_at_the_start_of_credits_that_run_to_its_end() {
    let h = harness("credits").await;
    let id = episode("15", "14", 2, 1).id().clone();
    h.progress(h.alice, &id, 2399).await;
    let state = h.state_of(h.alice, &id).await;
    assert!(!state.watched);
    assert_eq!(state.position, Some(Duration::from_secs(2399)));
    h.progress(h.bob, &id, 2400).await;
    assert!(h.state_of(h.bob, &id).await.watched);
}

/// The crossing into watched is written inside the throttle interval, and
/// is judged there by the same credits as the write before it.
#[tokio::test]
async fn the_crossing_at_the_credits_is_written_within_the_throttle() {
    let h = harness("credits-throttle").await;
    let id = episode("15", "14", 2, 1).id().clone();
    h.progress(h.alice, &id, 2380).await;
    // Before the credits: no crossing, so not written.
    h.progress(h.alice, &id, 2390).await;
    let state = h.state_of(h.alice, &id).await;
    assert!(!state.watched);
    assert_eq!(state.position, Some(Duration::from_secs(2380)));
    h.progress(h.alice, &id, 2400).await;
    assert!(h.state_of(h.alice, &id).await.watched);
}

/// An item with no such credits is watched at 95% and not before: episode
/// 12 is probed as 1309.472 seconds: 95% falls between 1243 and 1244 seconds.
#[tokio::test]
async fn an_item_without_credits_that_run_to_its_end_is_watched_at_95_percent() {
    let h = harness("no-credits").await;
    let id = episode("12", "11", 1, 1).id().clone();
    h.progress(h.alice, &id, 1243).await;
    let state = h.state_of(h.alice, &id).await;
    assert!(!state.watched);
    assert_eq!(state.position, Some(Duration::from_secs(1243)));
    h.progress(h.bob, &id, 1244).await;
    assert!(h.state_of(h.bob, &id).await.watched);
}

#[tokio::test]
async fn continue_watching_is_newest_first() {
    let h = harness("continue").await;
    let movie_id = movie().id().clone();
    let episode_id = episode("13", "11", 1, 2).id().clone();
    h.progress(h.alice, &movie_id, 600).await;
    // updated_at has one-second resolution.
    h.clock.advance(Duration::from_secs(1));
    h.progress(h.alice, &episode_id, 300).await;
    let home = h.home(h.alice).await;
    assert_eq!(
        ids(&home.continue_watching),
        [episode_id.to_string(), movie_id.to_string()]
    );
    let Item::Episode(e) = &home.continue_watching[0] else {
        panic!("not an episode");
    };
    assert_eq!(e.playback.position, Some(Duration::from_secs(300)));
}

#[tokio::test]
async fn marking_a_season_watched_marks_its_episodes() {
    let h = harness("mark-season").await;
    h.mark(h.alice, &season_id("11"), true).await;
    for e in [episode("12", "11", 1, 1), episode("13", "11", 1, 2)] {
        assert!(h.state_of(h.alice, e.id()).await.watched, "{}", e.id());
    }
    // Only that season, and only for alice.
    assert!(
        !h.state_of(h.alice, episode("15", "14", 2, 1).id())
            .await
            .watched
    );
    assert!(
        !h.state_of(h.bob, episode("12", "11", 1, 1).id())
            .await
            .watched
    );
}

#[tokio::test]
async fn marking_a_show_watched_marks_every_episode_and_unmarking_clears_them() {
    let h = harness("mark-show").await;
    let all = [
        episode("21", "20", 0, 1),
        episode("12", "11", 1, 1),
        episode("13", "11", 1, 2),
        episode("15", "14", 2, 1),
    ];
    h.mark(h.alice, show().id(), true).await;
    for e in &all {
        assert!(h.state_of(h.alice, e.id()).await.watched, "{}", e.id());
    }
    h.mark(h.alice, &season_id("11"), false).await;
    assert!(!h.state_of(h.alice, all[1].id()).await.watched);
    assert!(!h.state_of(h.alice, all[2].id()).await.watched);
    assert!(h.state_of(h.alice, all[3].id()).await.watched);
}

#[tokio::test]
async fn next_up_crosses_a_season_boundary() {
    let h = harness("next-up").await;
    h.mark(h.alice, episode("12", "11", 1, 1).id(), true).await;
    h.mark(h.alice, episode("13", "11", 1, 2).id(), true).await;
    // Season 2, episode 1; specials (season 0) are skipped.
    assert_eq!(ids(&h.home(h.alice).await.next_up), ["plex:episode:15"]);
}

#[tokio::test]
async fn next_up_is_after_the_latest_watched_episode() {
    let h = harness("next-up-gap").await;
    // Watched E2 but not E1: next up is what follows E2.
    h.mark(h.alice, episode("13", "11", 1, 2).id(), true).await;
    assert_eq!(ids(&h.home(h.alice).await.next_up), ["plex:episode:15"]);
}

/// A panic while the next-up cache is locked (PX-10) does not break Home
/// for everyone after it: the lock does not poison.
#[tokio::test]
async fn next_up_works_after_a_panic_while_its_cache_was_locked() {
    let h = harness("next-up-after-panic").await;
    h.mark(h.alice, episode("12", "11", 1, 1).id(), true).await;
    let panicked = std::thread::scope(|s| {
        s.spawn(|| {
            let _held = h.watch.next_up.lock();
            panic!("a bug while the next-up cache is held");
        })
        .join()
        .is_err()
    });
    assert!(panicked);
    assert_eq!(ids(&h.home(h.alice).await.next_up), ["plex:episode:13"]);
}

/// A rewatch: the whole show was watched once, and the person is back at
/// the start. Next up follows what they watched last, not the finale.
#[tokio::test]
async fn next_up_follows_the_most_recently_watched_episode() {
    let h = harness("next-up-rewatch").await;
    h.mark(h.alice, episode("15", "14", 2, 1).id(), true).await;
    // updated_at has one-second resolution.
    h.clock.advance(Duration::from_secs(1));
    h.mark(h.alice, episode("12", "11", 1, 1).id(), true).await;
    assert_eq!(ids(&h.home(h.alice).await.next_up), ["plex:episode:13"]);
}

/// The same film in two libraries: two items with the same external ids.
/// Each keeps its own state.
#[tokio::test]
async fn a_second_copy_of_a_film_does_not_take_the_first_copy_s_state() {
    let h = harness("two-copies").await;
    let first = movie().id().clone();
    let second = second_copy().id().clone();
    h.progress(h.alice, &first, 600).await;
    assert_eq!(h.state_of(h.alice, &second).await, PlaybackState::default());
    assert_eq!(
        h.state_of(h.alice, &first).await.position,
        Some(Duration::from_secs(600))
    );
    assert_eq!(
        ids(&h.home(h.alice).await.continue_watching),
        [first.to_string()]
    );
}

#[tokio::test]
async fn a_second_copy_of_a_film_does_not_take_the_first_copy_s_hide() {
    let h = harness("two-copies-hide").await;
    let first = movie().id().clone();
    h.progress(h.alice, &first, 600).await;
    h.hide(h.alice, &first).await;
    h.state_of(h.alice, second_copy().id()).await;
    let hidden = h.store.hidden(h.alice).await.unwrap();
    assert_eq!(hidden, [first.to_string()].into());
}

/// The key the film had before Plex gave it a new one.
fn old_key() -> MediaId {
    "plex:movie:99".parse().unwrap()
}

/// Alice is 10 minutes into the film, under its old key.
async fn started_under_the_old_key(h: &Harness) {
    let external_ids = [format!("test://{}", movie().id().key())];
    h.store
        .save_watch(
            h.alice,
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
    let h = harness("relink-other-film").await;
    let id = movie().id().clone();
    started_under_the_old_key(&h).await;
    h.catalog.set_old_key(
        &old_key(),
        OldKey::AnotherItem(vec!["test://another-film".into()]),
    );
    assert_eq!(
        h.state_of(h.alice, &id).await.position,
        Some(Duration::from_secs(600))
    );
}

/// The old key is a show's now: the item still opens, with its state.
#[tokio::test]
async fn state_under_a_key_that_now_holds_another_kind_moves_to_the_new_key() {
    let h = harness("relink-other-kind").await;
    let id = movie().id().clone();
    started_under_the_old_key(&h).await;
    h.catalog.set_old_key(&old_key(), OldKey::AnotherKind);
    assert_eq!(
        h.state_of(h.alice, &id).await.position,
        Some(Duration::from_secs(600))
    );
}

/// The old key is one this catalog does not serve (the state was written
/// with another catalog, P9).
#[tokio::test]
async fn state_under_another_catalog_s_key_moves_to_the_new_key() {
    let h = harness("relink-foreign").await;
    let id = movie().id().clone();
    started_under_the_old_key(&h).await;
    h.catalog.set_old_key(&old_key(), OldKey::Foreign);
    assert_eq!(
        h.state_of(h.alice, &id).await.position,
        Some(Duration::from_secs(600))
    );
}

/// A catalog that cannot say what the old key holds: the item opens
/// without the state, the state stays where it is, and it moves at the
/// next look once the catalog answers.
#[tokio::test]
async fn state_stays_while_the_catalog_cannot_answer_for_the_old_key() {
    let h = harness("relink-unreachable").await;
    let id = movie().id().clone();
    started_under_the_old_key(&h).await;
    h.catalog.set_old_key(&old_key(), OldKey::Unreachable);
    assert_eq!(h.state_of(h.alice, &id).await, PlaybackState::default());
    let rows = h.store.all_watch(h.alice).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].media_id, old_key().to_string());

    h.catalog.forget_old_key(&old_key());
    assert_eq!(
        h.state_of(h.alice, &id).await.position,
        Some(Duration::from_secs(600))
    );
}

/// The second copy has no state, so each look at it asks whether the
/// first copy's key still holds the film: with the narrow call, and the
/// answer is kept for the next look.
#[tokio::test]
async fn looks_at_a_second_copy_ask_about_the_first_copy_once() {
    let h = harness("two-copies-asked-once").await;
    let first = movie().id().clone();
    let second = second_copy().id().clone();
    h.progress(h.alice, &first, 600).await;
    h.catalog.take_calls(&first);
    h.state_of(h.alice, &second).await;
    h.state_of(h.alice, &second).await;
    assert_eq!(h.catalog.take_calls(&first), ["external_ids"]);
}

/// The answer is kept for [`SAME_ITEM_CACHE`] only: a real re-key is
/// seen at most that long after it.
#[tokio::test]
async fn a_look_at_a_second_copy_after_the_cache_asks_again() {
    let h = harness("two-copies-asked-again").await;
    let first = movie().id().clone();
    let second = second_copy().id().clone();
    h.progress(h.alice, &first, 600).await;
    h.state_of(h.alice, &second).await;
    h.catalog.take_calls(&first);
    h.clock.advance(SAME_ITEM_CACHE);
    h.state_of(h.alice, &second).await;
    assert_eq!(h.catalog.take_calls(&first), ["external_ids"]);
}

#[tokio::test]
async fn home_lists_recently_added() {
    let h = harness("recent").await;
    assert_eq!(
        ids(&h.home(h.alice).await.recently_added),
        [movie().id().to_string()]
    );
}

#[tokio::test]
async fn a_hidden_movie_leaves_only_that_user_s_home_and_keeps_its_state() {
    let h = harness("hide-movie").await;
    let id = movie().id().clone();
    h.progress(h.alice, &id, 600).await;
    h.progress(h.bob, &id, 600).await;
    h.hide(h.alice, &id).await;
    assert!(h.home(h.alice).await.continue_watching.is_empty());
    assert_eq!(
        ids(&h.home(h.bob).await.continue_watching),
        [id.to_string()]
    );
    // Resume still resumes.
    assert_eq!(
        h.state_of(h.alice, &id).await.position,
        Some(Duration::from_secs(600))
    );
}

#[tokio::test]
async fn playing_a_hidden_movie_brings_it_back() {
    let h = harness("hide-play").await;
    let id = movie().id().clone();
    h.progress(h.alice, &id, 600).await;
    h.hide(h.alice, &id).await;
    // Within the write throttle of the last update: not written, but played.
    h.progress(h.alice, &id, 610).await;
    assert_eq!(
        ids(&h.home(h.alice).await.continue_watching),
        [id.to_string()]
    );
}

/// Removing an episode removes that card only: playing another episode of
/// the show does not bring it back, as playing it does.
#[tokio::test]
async fn a_removed_episode_stays_removed_when_another_episode_is_played() {
    let h = harness("hide-episode").await;
    let watched = episode("12", "11", 1, 1).id().clone();
    let started = episode("13", "11", 1, 2).id().clone();
    h.mark(h.alice, &watched, true).await;
    h.progress(h.alice, &started, 300).await;
    h.hide(h.alice, &started).await;
    let hidden = h.home(h.alice).await;
    assert!(hidden.continue_watching.is_empty());
    // Recently added is not Continue watching.
    assert!(!hidden.recently_added.is_empty());

    h.mark(h.alice, &watched, true).await;
    assert!(h.home(h.alice).await.continue_watching.is_empty());
}

/// Lanterns S1E8 and Banshee S3E9, on the user's server: the episode after
/// the one watched is started, and was in both rows.
#[tokio::test]
async fn an_episode_in_progress_is_in_continue_watching_and_not_in_next_up() {
    let h = harness("one-row-started").await;
    h.mark(h.alice, episode("12", "11", 1, 1).id(), true).await;
    let started = episode("13", "11", 1, 2).id().clone();
    h.progress(h.alice, &started, 300).await;
    let home = h.home(h.alice).await;
    assert_eq!(ids(&home.continue_watching), [started.to_string()]);
    assert!(home.next_up.is_empty(), "{:?}", ids(&home.next_up));
}

/// Vigil, on the user's server: S2E3 in progress, and Next up showed S1E2,
/// the episode after an older one watched.
#[tokio::test]
async fn a_show_with_a_later_episode_in_progress_has_nothing_in_next_up() {
    let h = harness("one-row-later").await;
    h.mark(h.alice, episode("12", "11", 1, 1).id(), true).await;
    h.progress(h.alice, episode("15", "14", 2, 1).id(), 300)
        .await;
    assert!(h.home(h.alice).await.next_up.is_empty());
}

#[tokio::test]
async fn finishing_the_episode_in_progress_puts_the_next_one_in_next_up() {
    let h = harness("one-row-finished").await;
    h.mark(h.alice, episode("12", "11", 1, 1).id(), true).await;
    let started = episode("13", "11", 1, 2).id().clone();
    h.progress(h.alice, &started, 300).await;
    assert!(h.home(h.alice).await.next_up.is_empty());
    // Probed as 1309.472 s: watched at 95%.
    h.progress(h.alice, &started, 1300).await;
    let home = h.home(h.alice).await;
    assert!(home.continue_watching.is_empty());
    assert_eq!(ids(&home.next_up), ["plex:episode:15"]);
}

/// More items in progress than Continue watching shows: a show whose
/// episode in progress is past the row is still kept out of Next up.
#[tokio::test]
async fn a_show_in_progress_past_the_row_is_kept_out_of_next_up() {
    let h = harness("one-row-past-the-row").await;
    h.mark(h.alice, episode("12", "11", 1, 1).id(), true).await;
    h.progress(h.alice, episode("15", "14", 2, 1).id(), 300)
        .await;
    // updated_at has one-second resolution.
    h.clock.advance(Duration::from_secs(1));
    for n in 0..ROW {
        let media_id = format!("plex:movie:{}", 100 + n);
        h.store
            .save_watch(
                h.alice,
                WatchUpdate {
                    media_id: &media_id,
                    show_id: None,
                    external_ids: &[],
                    position_ms: Some(600_000),
                    duration_ms: Some(7_000_000),
                    watched: false,
                    unhide: true,
                },
            )
            .await
            .unwrap();
    }
    assert!(h.home(h.alice).await.next_up.is_empty());
}

/// "Remove" on a Next up card removes that episode, not the show; once it
/// is watched, the one after it is next up.
#[tokio::test]
async fn removing_a_next_up_episode_removes_only_that_episode() {
    let h = harness("hide-next-up").await;
    h.mark(h.alice, episode("12", "11", 1, 1).id(), true).await;
    let next = episode("13", "11", 1, 2).id().clone();
    assert_eq!(ids(&h.home(h.alice).await.next_up), [next.to_string()]);
    h.hide(h.alice, &next).await;
    assert!(h.home(h.alice).await.next_up.is_empty());
    assert_eq!(
        h.store.hidden(h.alice).await.unwrap(),
        [next.to_string()].into()
    );

    h.mark(h.alice, &next, true).await;
    assert_eq!(ids(&h.home(h.alice).await.next_up), ["plex:episode:15"]);
}

/// "Remove" on a Continue watching episode removes that card: another
/// episode of the show in progress stays, and playing the removed one
/// brings it back.
#[tokio::test]
async fn removing_an_episode_in_progress_leaves_the_show_s_others() {
    let h = harness("hide-in-progress").await;
    let first = episode("13", "11", 1, 2).id().clone();
    let second = episode("15", "14", 2, 1).id().clone();
    h.progress(h.alice, &first, 300).await;
    // updated_at has one-second resolution.
    h.clock.advance(Duration::from_secs(1));
    h.progress(h.alice, &second, 300).await;
    h.hide(h.alice, &first).await;
    assert_eq!(
        ids(&h.home(h.alice).await.continue_watching),
        [second.to_string()]
    );

    // Within the write throttle: not written, but played.
    h.progress(h.alice, &first, 310).await;
    assert_eq!(
        ids(&h.home(h.alice).await.continue_watching),
        [second.to_string(), first.to_string()]
    );
}

/// Only a card that shows keeps its show out of Next up.
#[tokio::test]
async fn a_removed_episode_in_progress_does_not_keep_its_show_out_of_next_up() {
    let h = harness("hide-in-progress-next-up").await;
    let started = episode("15", "14", 2, 1).id().clone();
    h.progress(h.alice, &started, 300).await;
    h.hide(h.alice, &started).await;
    h.clock.advance(Duration::from_secs(1));
    h.mark(h.alice, episode("12", "11", 1, 1).id(), true).await;
    let home = h.home(h.alice).await;
    assert!(home.continue_watching.is_empty());
    assert_eq!(ids(&home.next_up), ["plex:episode:13"]);
}

#[tokio::test]
async fn unhide_restores_at_once() {
    let h = harness("unhide").await;
    let id = movie().id().clone();
    h.progress(h.alice, &id, 600).await;
    h.hide(h.alice, &id).await;
    h.watch.unhide(h.alice, &id).await.unwrap();
    assert_eq!(
        ids(&h.home(h.alice).await.continue_watching),
        [id.to_string()]
    );
}
