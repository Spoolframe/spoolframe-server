//! What the player offers at a point of an item from its markers: Skip
//! Intro inside an intro, Skip Credits inside credits a scene follows,
//! Next inside credits that run to the end when something follows, and
//! when Up next shows.
//!
//! Markers are in the item's own time, which is also a converted
//! output's time (the server encodes with `-copyts`), so positions
//! compare directly.

use spoolframe_types::{Marker, MarkerKind};
use std::time::Duration;
use tokio::time::Instant;

/// How long an automatic intro skip can be undone.
pub const UNDO_WINDOW: Duration = Duration::from_secs(5);
/// A skip button goes this long before its marker ends: a seek to the
/// end from there would gain nothing.
pub const SKIP_END_MARGIN: Duration = Duration::from_secs(1);
/// Up next shows only for an item longer than this: a short clip ends
/// before it could be read.
pub const UP_NEXT_MIN_LENGTH: Duration = Duration::from_secs(120);
/// With no credits marker, Up next shows for an item's last 30 s.
pub const UP_NEXT_LEAD: Duration = Duration::from_secs(30);

/// A button over the video.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerOffer {
    /// Seek to the intro's end.
    SkipIntro { to: Duration },
    /// Seek to the end of credits that a scene follows.
    SkipCredits { to: Duration },
    /// Play what comes next: the credits run to the end.
    Next,
}

/// An item's markers, and whether something follows it (the queue, or
/// the show's next episode).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Markers {
    markers: Vec<Marker>,
    has_next: bool,
}

impl Markers {
    pub fn new(markers: Vec<Marker>, has_next: bool) -> Self {
        Self { markers, has_next }
    }

    /// Something follows the item now: the show's next episode was found
    /// after it started, or the queue changed.
    pub fn set_has_next(&mut self, has_next: bool) {
        self.has_next = has_next;
    }

    pub fn markers(&self) -> &[Marker] {
        &self.markers
    }

    /// The button at `time`, if any. A skip shows inside
    /// `[start, end - 1 s)`; credits that run to the end show until the
    /// end, as Next, and only when something follows and Up next does
    /// not show already. A kind this build does not know is passed over,
    /// so it cannot hide an intro it overlaps. A skip's target is kept
    /// inside `duration` when it is known.
    pub fn skip_at(
        &self,
        time: Duration,
        duration: Option<Duration>,
        up_next_shown: bool,
    ) -> Option<MarkerOffer> {
        let (_, marker) = self.marker_at(time)?;
        let end = Duration::from_millis(marker.end_ms);
        let to = duration.map_or(end, |d| end.min(d));
        match marker.kind {
            MarkerKind::Intro => Some(MarkerOffer::SkipIntro { to }),
            MarkerKind::Credits if marker.to_end => {
                (self.has_next && !up_next_shown).then_some(MarkerOffer::Next)
            }
            MarkerKind::Credits => Some(MarkerOffer::SkipCredits { to }),
            MarkerKind::Unknown => None,
        }
    }

    /// The marker whose button shows at `time`, with its place.
    fn marker_at(&self, time: Duration) -> Option<(usize, &Marker)> {
        self.markers.iter().enumerate().find(|(_, m)| {
            let start = Duration::from_millis(m.start_ms);
            let end = Duration::from_millis(m.end_ms).saturating_sub(SKIP_END_MARGIN);
            m.kind != MarkerKind::Unknown && start <= time && (time < end || m.to_end)
        })
    }

    /// Whether Up next shows at `time`: something follows, the item is
    /// longer than [`UP_NEXT_MIN_LENGTH`], and its credits began (the
    /// marker's start, see [`credits_start`]), else its last
    /// [`UP_NEXT_LEAD`].
    pub fn up_next_due(&self, time: Duration, duration: Option<Duration>) -> bool {
        let Some(duration) = duration.filter(|d| *d > UP_NEXT_MIN_LENGTH) else {
            return false;
        };
        let from = credits_start(&self.markers).unwrap_or(duration - UP_NEXT_LEAD);
        self.has_next && time >= from
    }
}

/// Where an item's credits start: those that run to the end, else the
/// last credits.
pub fn credits_start(markers: &[Marker]) -> Option<Duration> {
    let mut credits = markers.iter().filter(|m| m.kind == MarkerKind::Credits);
    let marker = credits
        .clone()
        .find(|m| m.to_end)
        .or_else(|| credits.next_back())?;
    Some(Duration::from_millis(marker.start_ms))
}

/// What the player does at a position ([`MarkerTracker::on_position`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerAction {
    /// Show this button, or none: sent only when it changes.
    Offer(Option<MarkerOffer>),
    /// Skip the intro by itself: seek to `to`.
    AutoSkip { to: Duration },
    /// Show or hide Up next: sent only when it changes.
    UpNext(bool),
}

/// An automatic skip: where from, and when.
#[derive(Debug, Clone, Copy)]
struct Skip {
    from: Duration,
    at: Instant,
}

/// Follows the position through an item's markers: what to show, and
/// when to skip an intro by itself. One per item played.
#[derive(Debug)]
pub struct MarkerTracker {
    markers: Markers,
    /// Skip intros without asking (a preference).
    auto_skip: bool,
    offered: Option<MarkerOffer>,
    up_next: bool,
    /// The markers already skipped by themselves: each once.
    skipped: Vec<usize>,
    last_skip: Option<Skip>,
}

impl MarkerTracker {
    pub fn new(markers: Markers, auto_skip: bool) -> Self {
        Self {
            markers,
            auto_skip,
            offered: None,
            up_next: false,
            skipped: Vec::new(),
            last_skip: None,
        }
    }

    pub fn set_auto_skip(&mut self, auto_skip: bool) {
        self.auto_skip = auto_skip;
    }

    pub fn markers_mut(&mut self) -> &mut Markers {
        &mut self.markers
    }

    /// Playback is at `at` of an item `length` long. `settled`: where a
    /// seek landed, not mid-seek or mid-restart; an intro is skipped by
    /// itself only then, and only once the length is known. Each intro
    /// is skipped by itself once.
    pub fn on_position(
        &mut self,
        at: Duration,
        length: Option<Duration>,
        settled: bool,
    ) -> Vec<MarkerAction> {
        let mut actions = Vec::new();
        let up_next = self.markers.up_next_due(at, length);
        if up_next != self.up_next {
            self.up_next = up_next;
            actions.push(MarkerAction::UpNext(up_next));
        }
        let mut offer = self.markers.skip_at(at, length, up_next);
        if let Some(MarkerOffer::SkipIntro { to }) = offer
            && self.auto_skip
            && settled
            && length.is_some_and(|l| !l.is_zero())
            && let Some((index, _)) = self.markers.marker_at(at)
            && !self.skipped.contains(&index)
        {
            self.skipped.push(index);
            self.last_skip = Some(Skip {
                from: at,
                at: Instant::now(),
            });
            offer = None;
            actions.push(MarkerAction::AutoSkip { to });
        }
        if offer != self.offered {
            self.offered = offer;
            actions.push(MarkerAction::Offer(offer));
        }
        actions
    }

    /// Whether the last automatic skip can still be undone.
    pub fn undoable(&self) -> bool {
        self.last_skip
            .is_some_and(|skip| skip.at.elapsed() < UNDO_WINDOW)
    }

    /// Undo the automatic skip made in the last [`UNDO_WINDOW`]: where to
    /// seek back to. The intro is not skipped again.
    pub fn undo(&mut self) -> Option<Duration> {
        let skip = self.last_skip.take()?;
        (skip.at.elapsed() < UNDO_WINDOW).then_some(skip.from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker(kind: MarkerKind, start_ms: u64, end_ms: u64, to_end: bool) -> Marker {
        Marker {
            kind,
            start_ms,
            end_ms,
            to_end,
        }
    }

    fn episode() -> Vec<Marker> {
        vec![
            marker(MarkerKind::Intro, 60_000, 90_000, false),
            marker(MarkerKind::Credits, 1_200_000, 1_260_000, false),
            marker(MarkerKind::Credits, 2_600_000, 2_700_001, true),
        ]
    }

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    fn offer(markers: &[Marker], at: Duration, has_next: bool) -> Option<MarkerOffer> {
        Markers::new(markers.to_vec(), has_next).skip_at(at, None, false)
    }

    const LENGTH: Option<Duration> = Some(Duration::from_secs(2_700));

    #[test]
    fn inside_the_intro_it_offers_to_skip_to_its_end() {
        assert_eq!(
            offer(&episode(), secs(70), true),
            Some(MarkerOffer::SkipIntro { to: secs(90) })
        );
    }

    #[test]
    fn outside_every_marker_there_is_nothing() {
        assert_eq!(offer(&episode(), secs(100), true), None);
    }

    #[test]
    fn the_intros_end_is_already_past_it() {
        assert_eq!(offer(&episode(), secs(90), true), None);
    }

    #[test]
    fn the_skip_goes_a_second_before_the_marker_ends() {
        assert_eq!(
            offer(&episode(), Duration::from_millis(88_999), true),
            Some(MarkerOffer::SkipIntro { to: secs(90) })
        );
        assert_eq!(offer(&episode(), secs(89), true), None);
    }

    #[test]
    fn credits_with_a_scene_after_offer_to_skip_them() {
        assert_eq!(
            offer(&episode(), secs(1_210), true),
            Some(MarkerOffer::SkipCredits { to: secs(1_260) })
        );
    }

    #[test]
    fn credits_to_the_end_offer_the_next_item() {
        assert_eq!(
            offer(&episode(), secs(2_650), true),
            Some(MarkerOffer::Next)
        );
    }

    #[test]
    fn credits_to_the_end_with_nothing_after_offer_nothing() {
        assert_eq!(offer(&episode(), secs(2_650), false), None);
    }

    #[test]
    fn credits_to_the_end_offer_no_next_while_up_next_shows() {
        let markers = Markers::new(episode(), true);
        assert_eq!(markers.skip_at(secs(2_650), LENGTH, true), None);
    }

    #[test]
    fn to_end_holds_past_a_marker_end_short_of_the_duration() {
        // Plex's end can be off the duration either way: go by to_end.
        let markers = vec![marker(MarkerKind::Credits, 100_000, 150_000, true)];
        assert_eq!(offer(&markers, secs(151), true), Some(MarkerOffer::Next));
    }

    #[test]
    fn a_skip_past_the_length_lands_on_the_end() {
        let markers = Markers::new(
            vec![marker(MarkerKind::Credits, 100_000, 150_000, false)],
            false,
        );
        assert_eq!(
            markers.skip_at(secs(110), Some(secs(140)), false),
            Some(MarkerOffer::SkipCredits { to: secs(140) })
        );
    }

    #[test]
    fn an_unknown_kind_does_not_hide_an_intro_inside_it() {
        let markers = vec![
            marker(MarkerKind::Unknown, 0, 120_000, false),
            marker(MarkerKind::Intro, 60_000, 90_000, false),
        ];
        assert_eq!(
            offer(&markers, secs(70), true),
            Some(MarkerOffer::SkipIntro { to: secs(90) })
        );
    }

    #[test]
    fn an_unknown_kind_shows_nothing() {
        let markers = vec![marker(MarkerKind::Unknown, 0, 10_000, false)];
        assert_eq!(offer(&markers, secs(5), true), None);
    }

    #[test]
    fn the_credits_start_at_those_that_run_to_the_end() {
        assert_eq!(credits_start(&episode()), Some(secs(2_600)));
    }

    #[test]
    fn with_no_credits_to_the_end_the_last_credits_count() {
        let markers = &episode()[..2];
        assert_eq!(credits_start(markers), Some(secs(1_200)));
        assert_eq!(credits_start(&episode()[..1]), None);
    }

    #[test]
    fn up_next_shows_from_the_credits() {
        let markers = Markers::new(episode(), true);
        assert!(!markers.up_next_due(secs(2_599), LENGTH));
        assert!(markers.up_next_due(secs(2_600), LENGTH));
    }

    #[test]
    fn up_next_with_no_credits_shows_for_the_last_thirty_seconds() {
        let markers = Markers::new(Vec::new(), true);
        assert!(!markers.up_next_due(secs(2_669), LENGTH));
        assert!(markers.up_next_due(secs(2_670), LENGTH));
    }

    #[test]
    fn up_next_needs_something_after_and_an_item_over_two_minutes() {
        assert!(!Markers::new(Vec::new(), false).up_next_due(secs(2_690), LENGTH));
        let short = Markers::new(Vec::new(), true);
        assert!(!short.up_next_due(secs(119), Some(secs(120))));
        assert!(!short.up_next_due(secs(119), None));
    }

    fn tracker(auto_skip: bool) -> MarkerTracker {
        MarkerTracker::new(Markers::new(episode(), true), auto_skip)
    }

    #[test]
    fn the_offer_is_sent_only_when_it_changes() {
        let mut t = tracker(false);
        assert_eq!(
            t.on_position(secs(61), LENGTH, true),
            [MarkerAction::Offer(Some(MarkerOffer::SkipIntro {
                to: secs(90)
            }))]
        );
        assert_eq!(t.on_position(secs(62), LENGTH, true), []);
        assert_eq!(
            t.on_position(secs(95), LENGTH, true),
            [MarkerAction::Offer(None)]
        );
    }

    #[test]
    fn up_next_takes_the_place_of_the_next_button() {
        let mut t = tracker(false);
        assert_eq!(
            t.on_position(secs(2_650), LENGTH, true),
            [MarkerAction::UpNext(true)]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_automatic_intro_skip_happens_once_and_undoes() {
        let mut t = tracker(true);
        assert_eq!(
            t.on_position(secs(61), LENGTH, true),
            [MarkerAction::AutoSkip { to: secs(90) }]
        );
        assert!(t.undoable());
        tokio::time::advance(secs(2)).await;
        assert_eq!(t.undo(), Some(secs(61)));
        // Back in the intro: offered, not skipped again.
        assert_eq!(
            t.on_position(secs(61), LENGTH, true),
            [MarkerAction::Offer(Some(MarkerOffer::SkipIntro {
                to: secs(90)
            }))]
        );
        assert_eq!(t.undo(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn an_automatic_skip_cannot_be_undone_after_five_seconds() {
        let mut t = tracker(true);
        t.on_position(secs(61), LENGTH, true);
        tokio::time::advance(UNDO_WINDOW).await;
        assert!(!t.undoable());
        assert_eq!(t.undo(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn no_automatic_skip_mid_seek_or_before_the_length_is_known() {
        let mut t = tracker(true);
        let skipped = |actions: Vec<MarkerAction>| {
            actions
                .iter()
                .any(|a| matches!(a, MarkerAction::AutoSkip { .. }))
        };
        assert!(!skipped(t.on_position(secs(61), LENGTH, false)));
        assert!(!skipped(t.on_position(secs(62), None, true)));
        assert_eq!(
            t.on_position(secs(63), LENGTH, true),
            [
                MarkerAction::AutoSkip { to: secs(90) },
                MarkerAction::Offer(None)
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn credits_are_never_skipped_by_themselves() {
        let mut t = tracker(true);
        assert_eq!(
            t.on_position(secs(1_210), LENGTH, true),
            [MarkerAction::Offer(Some(MarkerOffer::SkipCredits {
                to: secs(1_260)
            }))]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn each_intro_is_skipped_by_itself_once() {
        let markers = vec![
            marker(MarkerKind::Intro, 0, 30_000, false),
            marker(MarkerKind::Intro, 600_000, 630_000, false),
        ];
        let mut t = MarkerTracker::new(Markers::new(markers, false), true);
        assert_eq!(
            t.on_position(secs(1), LENGTH, true),
            [MarkerAction::AutoSkip { to: secs(30) }]
        );
        assert_eq!(
            t.on_position(secs(601), LENGTH, true),
            [MarkerAction::AutoSkip { to: secs(630) }]
        );
    }
}
