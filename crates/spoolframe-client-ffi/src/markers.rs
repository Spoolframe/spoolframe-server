//! An item's markers for a player the app drives itself: Media3 and
//! AVPlayer seek on their own, and the app holds them, so neither the
//! SDK's `PlaybackController` (which owns the player through a driver,
//! on one thread) nor its `LocalController` fits. [`SpoolMarkers`] is the
//! SDK's `MarkerTracker` with a seek gate: the app feeds it the player's
//! positions and does what it says. Its calls are cheap and never block,
//! so a tick can make them on the main thread.

use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use spoolframe_client_sdk::types::Marker;
use spoolframe_client_sdk::{
    ManagedPlayback, MarkerAction, MarkerOffer, MarkerOptions, MarkerTracker, Markers, SeekGate,
    UNDO_WINDOW,
};

use crate::playback::SpoolPlayback;

/// Where the player is while it seeks.
enum Gate {
    /// A file on this device: there is no playback to hold a gate.
    Own(Mutex<SeekGate>),
    /// The playback's, which `seek_started` on it and `report_position`
    /// share. Weak: a forgotten object must not keep the playback open.
    Playback(Weak<SpoolPlayback>),
}

impl Gate {
    /// The playback, while it is open.
    fn managed(playback: &Weak<SpoolPlayback>) -> Option<ManagedPlayback> {
        playback.upgrade()?.managed().ok()
    }

    /// Whether the player can no longer seek: its playback is over.
    fn over(&self) -> bool {
        match self {
            Self::Own(_) => false,
            Self::Playback(playback) => Self::managed(playback).is_none(),
        }
    }

    fn position(&self, at: Duration) -> (Duration, bool) {
        match self {
            Self::Own(gate) => gate.lock().expect("never poisoned").position(at),
            Self::Playback(playback) => {
                Self::managed(playback).map_or((at, false), |managed| managed.gated(at))
            }
        }
    }

    fn started(&self, target: Duration) {
        match self {
            Self::Own(gate) => gate.lock().expect("never poisoned").started(target),
            Self::Playback(playback) => {
                if let Some(managed) = Self::managed(playback) {
                    managed.seek_started(target);
                }
            }
        }
    }

    fn done(&self) {
        match self {
            Self::Own(gate) => gate.lock().expect("never poisoned").done(),
            Self::Playback(playback) => {
                if let Some(managed) = Self::managed(playback) {
                    managed.seek_done();
                }
            }
        }
    }
}

struct State {
    tracker: MarkerTracker,
    /// The button showing: what `skip` skips.
    offered: Option<MarkerOffer>,
}

/// Follows the position through an item's markers: what to show, and
/// when to skip an intro by itself. One per item played; its tick is
/// `on_position`. Made by `SpoolPlayback.follow_markers`, or `new` for a
/// file on this device. The object seeks nothing: where it gives a target
/// (`MarkerAction::AutoSkip`, `skip`, `undo_skip`) the app seeks the
/// player there, and it counts as a seek in flight from then.
#[derive(uniffi::Object)]
pub struct SpoolMarkers {
    state: Mutex<State>,
    gate: Gate,
}

impl SpoolMarkers {
    pub(crate) fn of_playback(
        playback: &Arc<SpoolPlayback>,
        markers: Vec<Marker>,
        has_next: bool,
        auto_skip: bool,
        up_next_card: bool,
    ) -> Self {
        let gate = Gate::Playback(Arc::downgrade(playback));
        Self::with_gate(markers, has_next, auto_skip, up_next_card, gate)
    }

    fn with_gate(
        markers: Vec<Marker>,
        has_next: bool,
        auto_skip: bool,
        up_next_card: bool,
        gate: Gate,
    ) -> Self {
        let tracker = MarkerTracker::with_options(
            Markers::new(markers, has_next),
            auto_skip,
            MarkerOptions { up_next_card },
        );
        Self {
            state: Mutex::new(State {
                tracker,
                offered: None,
            }),
            gate,
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("never poisoned")
    }
}

#[uniffi::export]
impl SpoolMarkers {
    /// A file on this device, played with no `SpoolPlayback`; a playback
    /// has `SpoolPlayback.follow_markers`. `markers`: the item's.
    /// `has_next`: something follows it (`set_has_next` when found later).
    /// `auto_skip`: intros skip by themselves. `up_next_card`: the app
    /// shows an Up next card, which takes Next's place once due; without
    /// one, Next stays through the credits and `UpNext` never comes.
    #[uniffi::constructor]
    pub fn new(markers: Vec<Marker>, has_next: bool, auto_skip: bool, up_next_card: bool) -> Self {
        let gate = Gate::Own(Mutex::default());
        Self::with_gate(markers, has_next, auto_skip, up_next_card, gate)
    }

    /// The player is at `at` of an item `length` long, as the player says
    /// it (`None` until it knows): what to do, each only when it changes.
    /// On `AutoSkip` seek the player to `to`, and show Undo for
    /// `undo_window()`. An intro is skipped by itself only where a seek
    /// has landed and the length is known.
    pub fn on_position(&self, at: Duration, length: Option<Duration>) -> Vec<MarkerAction> {
        let (at, settled) = self.gate.position(at);
        let mut state = self.state();
        let actions = state.tracker.on_position(at, length, settled);
        for action in &actions {
            match action {
                MarkerAction::AutoSkip { to } => self.gate.started(*to),
                MarkerAction::Offer(offer) => state.offered = *offer,
                MarkerAction::UpNext(_) => {}
            }
        }
        actions
    }

    /// The viewer pressed the skip button showing: where to seek the
    /// player. `None` with no skip showing; `Next` is the app's own to do.
    /// A pressed skip cannot be undone.
    pub fn skip(&self) -> Option<Duration> {
        if self.gate.over() {
            return None;
        }
        let to = match self.state().offered? {
            MarkerOffer::SkipIntro { to } | MarkerOffer::SkipCredits { to } => to,
            MarkerOffer::Next => return None,
        };
        self.gate.started(to);
        Some(to)
    }

    /// Whether the intro skipped by itself can still be undone (`undo_skip`):
    /// for a view that comes after the skip, or a tick hiding Undo.
    pub fn undoable(&self) -> bool {
        !self.gate.over() && self.state().tracker.undoable()
    }

    /// Undo the intro skipped by itself in the last `undo_window()`: where
    /// to seek the player back to, at once known for the scrubber. `None`
    /// with none to undo. The intro is then offered, not skipped again.
    pub fn undo_skip(&self) -> Option<Duration> {
        // A playback that is over seeks no more: no target to promise.
        if self.gate.over() {
            return None;
        }
        let back = self.state().tracker.undo()?;
        self.gate.started(back);
        Some(back)
    }

    /// Where the player is for a scrubber, the player at `at`: a seek's
    /// target until it lands.
    pub fn position(&self, at: Duration) -> Duration {
        self.gate.position(at).0
    }

    /// The player's own seek to `target` begins (its scrubber): until it
    /// lands its positions are the old place's. A resume counts: without
    /// it, a tick before the resume lands can skip an intro at the start.
    pub fn seek_started(&self, target: Duration) {
        self.gate.started(target);
    }

    /// The player says its seek is done: its positions count again.
    pub fn seek_done(&self) {
        self.gate.done();
    }

    /// Whether something follows the item: the credits' button is Next.
    pub fn set_has_next(&self, has_next: bool) {
        self.state().tracker.markers_mut().set_has_next(has_next);
    }

    /// Skip intros by themselves, or not.
    pub fn set_auto_skip(&self, auto_skip: bool) {
        self.state().tracker.set_auto_skip(auto_skip);
    }
}

/// How long an intro skipped by itself can be undone: how long Undo shows.
#[uniffi::export]
pub fn undo_window() -> Duration {
    UNDO_WINDOW
}

#[cfg(test)]
#[path = "markers_tests.rs"]
mod tests;
