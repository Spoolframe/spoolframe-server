//! What an app gets from [`SpoolMarkers`] fed its player's positions.
//! These run with no Tokio runtime where they can, as a foreign thread
//! calls them.

use super::*;
use spoolframe_client_sdk::types::MarkerKind;

const LENGTH: Option<Duration> = Some(Duration::from_secs(2_700));

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

pub(crate) fn episode() -> Vec<Marker> {
    let marker = |kind, start, end, to_end| Marker {
        kind,
        start: secs(start),
        end: secs(end),
        to_end,
    };
    vec![
        marker(MarkerKind::Intro, 60, 90, false),
        marker(MarkerKind::Credits, 2_600, 2_700, true),
    ]
}

fn skip_intro() -> MarkerAction {
    MarkerAction::Offer(Some(MarkerOffer::SkipIntro { to: secs(90) }))
}

#[test]
fn inside_an_intro_it_offers_the_skip_once() {
    let markers = SpoolMarkers::new(episode(), true, false, true);
    assert_eq!(markers.on_position(secs(61), LENGTH), [skip_intro()]);
    assert_eq!(markers.on_position(secs(62), LENGTH), []);
}

#[test]
fn a_skip_gives_the_button_s_target() {
    let markers = SpoolMarkers::new(episode(), true, false, true);
    markers.on_position(secs(61), LENGTH);
    assert_eq!(markers.skip(), Some(secs(90)));
}

#[test]
fn a_skip_counts_as_a_seek_until_it_lands() {
    let markers = SpoolMarkers::new(episode(), true, false, true);
    markers.on_position(secs(61), LENGTH);
    markers.skip();
    assert_eq!(markers.position(secs(61)), secs(90));
    assert_eq!(markers.position(secs(90)), secs(90));
    assert_eq!(markers.position(secs(91)), secs(91));
}

#[test]
fn once_the_button_has_gone_a_skip_gives_nothing() {
    let markers = SpoolMarkers::new(episode(), true, false, true);
    markers.on_position(secs(61), LENGTH);
    assert_eq!(
        markers.on_position(secs(95), LENGTH),
        [MarkerAction::Offer(None)]
    );
    assert_eq!(markers.skip(), None);
}

#[test]
fn next_is_no_seek_so_a_skip_gives_nothing() {
    let markers = SpoolMarkers::new(episode(), true, false, false);
    assert_eq!(
        markers.on_position(secs(2_650), LENGTH),
        [MarkerAction::Offer(Some(MarkerOffer::Next))]
    );
    assert_eq!(markers.skip(), None);
}

#[test]
fn an_up_next_card_takes_next_s_place() {
    let markers = SpoolMarkers::new(episode(), true, false, true);
    assert_eq!(
        markers.on_position(secs(2_650), LENGTH),
        [MarkerAction::UpNext(true)]
    );
}

#[test]
fn something_found_to_follow_offers_next() {
    let markers = SpoolMarkers::new(episode(), false, false, false);
    assert_eq!(markers.on_position(secs(2_650), LENGTH), []);
    markers.set_has_next(true);
    assert_eq!(
        markers.on_position(secs(2_651), LENGTH),
        [MarkerAction::Offer(Some(MarkerOffer::Next))]
    );
}

#[test]
fn an_automatic_skip_undoes_to_where_it_was() {
    let markers = SpoolMarkers::new(episode(), true, true, true);
    assert_eq!(
        markers.on_position(secs(61), LENGTH),
        [MarkerAction::AutoSkip { to: secs(90) }]
    );
    assert!(markers.undoable());
    assert_eq!(markers.undo_skip(), Some(secs(61)));
    assert!(!markers.undoable());
    assert_eq!(markers.undo_skip(), None);
}

#[test]
fn an_undone_intro_is_offered_not_skipped_again() {
    let markers = SpoolMarkers::new(episode(), true, true, true);
    markers.on_position(secs(61), LENGTH);
    markers.undo_skip();
    markers.seek_done();
    assert_eq!(markers.on_position(secs(61), LENGTH), [skip_intro()]);
}

#[test]
fn a_pressed_skip_is_not_undoable() {
    let markers = SpoolMarkers::new(episode(), true, false, true);
    markers.on_position(secs(61), LENGTH);
    assert_eq!(markers.skip(), Some(secs(90)));
    assert!(!markers.undoable());
    assert_eq!(markers.undo_skip(), None);
}

/// The scrubber goes to the target at once: the player's next tick is
/// still the old place's.
#[test]
fn an_automatic_skip_and_its_undo_count_as_seeks() {
    let markers = SpoolMarkers::new(episode(), true, true, true);
    markers.on_position(secs(61), LENGTH);
    assert_eq!(markers.position(secs(61)), secs(90));
    markers.undo_skip();
    assert_eq!(markers.position(secs(90)), secs(61));
}

#[test]
fn no_automatic_skip_until_the_length_is_known() {
    let markers = SpoolMarkers::new(episode(), true, true, true);
    assert_eq!(markers.on_position(secs(61), None), [skip_intro()]);
    assert_eq!(
        markers.on_position(secs(62), LENGTH),
        [
            MarkerAction::AutoSkip { to: secs(90) },
            MarkerAction::Offer(None)
        ]
    );
}

/// Mid-seek the player's position is the old place's: nothing is skipped
/// from where the player is leaving.
#[test]
fn no_automatic_skip_while_a_seek_is_in_flight() {
    let markers = SpoolMarkers::new(episode(), true, true, true);
    markers.seek_started(secs(70));
    assert_eq!(markers.on_position(secs(10), LENGTH), [skip_intro()]);
    markers.seek_done();
    assert_eq!(
        markers.on_position(secs(70), LENGTH),
        [
            MarkerAction::AutoSkip { to: secs(90) },
            MarkerAction::Offer(None)
        ]
    );
}

#[test]
fn automatic_skips_can_be_turned_on_mid_item() {
    let markers = SpoolMarkers::new(episode(), true, false, true);
    markers.set_auto_skip(true);
    assert_eq!(
        markers.on_position(secs(61), LENGTH),
        [MarkerAction::AutoSkip { to: secs(90) }]
    );
}

#[test]
fn the_undo_window_is_the_sdk_s() {
    assert_eq!(undo_window(), spoolframe_client_sdk::UNDO_WINDOW);
}

#[tokio::test(start_paused = true)]
async fn an_automatic_skip_cannot_be_undone_after_its_window() {
    let markers = SpoolMarkers::new(episode(), true, true, true);
    markers.on_position(secs(61), LENGTH);
    assert!(markers.undoable());
    tokio::time::advance(spoolframe_client_sdk::UNDO_WINDOW).await;
    assert!(!markers.undoable());
    assert_eq!(markers.undo_skip(), None);
}
