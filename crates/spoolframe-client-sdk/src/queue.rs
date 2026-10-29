//! What plays next, in two layers as Spotify has them (the Linux app's
//! queue and the iOS PlayQueue, as one):
//!
//! - **The context**: the album or list playback was started from, with
//!   shuffle and repeat.
//! - **The queue**: items added by hand ("Play next", "Add to queue"). They
//!   play after the current item and before the context goes on, in their
//!   own order, once; shuffle and repeat never touch them.
//!
//! Previous goes back through what actually played, queued items included.
//! `T` is what the app plays: an [`Item`], or its own type carrying one
//! (a playlist entry's id, say). A queue saves as JSON, and a queue the
//! Linux app saved loads as a `PlayQueue<Item>`.
//!
//! [`Item`]: spoolframe_types::Item

use std::collections::{BTreeSet, VecDeque};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use spoolframe_protocol::playlists::Repeat;
use spoolframe_types::Album;

/// How far into an item Previous starts it again rather than going back.
pub const RESTART_AFTER: Duration = Duration::from_secs(3);

/// A hand-added item, with the album it is from for its cover and name.
/// The id stays with it as the queue is reordered, so a view can name it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Queued<T> {
    pub id: u64,
    pub item: T,
    pub album: Option<Album>,
}

/// What plays: the context's item at the cursor, or one taken from the
/// queue. The names are the Linux app's, as saved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum Now<T> {
    Context,
    Queued(Box<Queued<T>>),
}

/// What [`PlayQueue::previous`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Previous {
    /// The item playing starts again: it was past [`RESTART_AFTER`].
    Restart,
    /// What played before plays again.
    Back,
    /// Nothing played before: the item starts again.
    Nothing,
}

/// A heading of the Up next list.
#[derive(Debug, Clone, PartialEq)]
pub enum QueueHeading {
    /// Over the hand-added items.
    NextInQueue,
    /// Over the context's rest, when the queue comes before it: "Next
    /// from: <album>".
    NextFrom(Album),
}

/// A row of the Up next list ([`PlayQueue::rows`]).
#[derive(Debug, Clone, PartialEq)]
pub enum QueueRow<T> {
    Heading(QueueHeading),
    /// The context's item at `pos` in its play order, played or to come.
    Context {
        pos: usize,
        item: T,
        played: bool,
    },
    /// What plays, with its album.
    Playing {
        item: T,
        album: Option<Album>,
    },
    Queued(Queued<T>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
pub struct PlayQueue<T> {
    /// The context's items, in their own (album) order.
    context: Vec<T>,
    /// The album the context is, if it is one.
    album: Option<Album>,
    /// The context's play order, as positions in `context`.
    order: Vec<usize>,
    /// Where the context is in `order`: the context item playing, or the
    /// last one played while a queued item plays. `None` before any.
    at: Option<usize>,
    queued: VecDeque<Queued<T>>,
    now: Option<Now<T>>,
    shuffle: bool,
    repeat: Repeat,
    next_id: u64,
    /// The context's items taken out of Up next: they stay out when
    /// shuffle is turned off.
    #[serde(default)]
    removed: BTreeSet<usize>,
    /// What played before, with the context's cursor at the time, for
    /// Previous. Not saved: a restored queue starts its history afresh.
    #[serde(skip)]
    history: Vec<(Now<T>, Option<usize>)>,
}

impl<T: Clone> PlayQueue<T> {
    /// Plays `context` (of `album`) from `start`, with `queued` waiting,
    /// as carried over from the playback before ([`Self::carry`]). An
    /// empty context plays the queue.
    pub fn new(
        context: Vec<T>,
        start: usize,
        album: Option<Album>,
        queued: Vec<Queued<T>>,
    ) -> Self {
        let mut queue = Self {
            context: Vec::new(),
            album: None,
            order: Vec::new(),
            at: None,
            next_id: queued.iter().map(|q| q.id).max().unwrap_or(0),
            queued: queued.into(),
            now: None,
            shuffle: false,
            repeat: Repeat::Off,
            removed: BTreeSet::new(),
            history: Vec::new(),
        };
        queue.set_context(context, start, album);
        queue
    }

    /// Replaces the context, playing it from `start`, and keeps the queue,
    /// as Spotify does. History of the old context is let go, and shuffle
    /// is off.
    pub fn set_context(&mut self, context: Vec<T>, start: usize, album: Option<Album>) {
        self.order = (0..context.len()).collect();
        self.context = context;
        self.album = album;
        self.shuffle = false;
        self.removed.clear();
        self.history.clear();
        if start < self.context.len() {
            self.at = Some(start);
            self.now = Some(Now::Context);
        } else {
            self.at = None;
            self.now = None;
            self.take_queued();
        }
    }

    /// Adds `items` to the end of the context: what follows it, such as
    /// a show's next episode, or more of a playlist the server plays.
    pub fn extend_context(&mut self, items: Vec<T>) {
        for item in items {
            self.order.push(self.context.len());
            self.context.push(item);
        }
    }

    /// What is to come of the context, anew: a playlist's order after a
    /// change of shuffle or repeat, as the server gives it. What played
    /// and what plays stay; `shuffled` is shuffle as the server has it.
    pub fn replace_ahead(&mut self, items: Vec<T>, shuffled: bool) {
        let keep = self.first_ahead().min(self.order.len());
        self.order.truncate(keep);
        self.extend_context(items);
        self.shuffle = shuffled;
    }

    pub fn current(&self) -> Option<&T> {
        match self.now.as_ref()? {
            Now::Context => self.context.get(*self.order.get(self.at?)?),
            Now::Queued(queued) => Some(&queued.item),
        }
    }

    /// The album of what plays: the context's, or the queued item's own.
    pub fn current_album(&self) -> Option<&Album> {
        match self.now.as_ref()? {
            Now::Context => self.album.as_ref(),
            Now::Queued(queued) => queued.album.as_ref(),
        }
    }

    /// The context's album, for "Next from: <album>".
    pub fn album(&self) -> Option<&Album> {
        self.album.as_ref()
    }

    /// Whether what plays was taken from the queue.
    pub fn current_is_queued(&self) -> bool {
        matches!(self.now, Some(Now::Queued(_)))
    }

    /// The hand-added items still to play, in order.
    pub fn queued(&self) -> impl Iterator<Item = &Queued<T>> {
        self.queued.iter()
    }

    /// The first play-order position still to come.
    fn first_ahead(&self) -> usize {
        self.at.map_or(0, |at| at + 1)
    }

    /// The context's items still to play, with their positions in the
    /// play order (for [`Self::play_context_at`]). Repeat does not wrap
    /// the listing round.
    pub fn context_ahead(&self) -> Vec<(usize, &T)> {
        (self.first_ahead()..self.order.len())
            .map(|pos| (pos, &self.context[self.order[pos]]))
            .collect()
    }

    /// The context's items already played, in play order.
    pub fn context_behind(&self) -> Vec<(usize, &T)> {
        let Some(at) = self.at else {
            return Vec::new();
        };
        let end = if self.current_is_queued() { at + 1 } else { at };
        (0..end.min(self.order.len()))
            .map(|pos| (pos, &self.context[self.order[pos]]))
            .collect()
    }

    /// How many of the context's items are still to come.
    pub fn ahead_count(&self) -> usize {
        self.order.len().saturating_sub(self.first_ahead())
    }

    /// Where in the context, 1-based, of how many: "track 3 of 9"; `None`
    /// for a queued item.
    pub fn context_position(&self) -> Option<(usize, usize)> {
        match self.now {
            Some(Now::Context) => Some((self.at? + 1, self.order.len())),
            _ => None,
        }
    }

    pub fn shuffle(&self) -> bool {
        self.shuffle
    }

    pub fn repeat(&self) -> Repeat {
        self.repeat
    }

    /// Repeat as `repeat`; one this build does not know is off.
    pub fn set_repeat(&mut self, repeat: Repeat) {
        self.repeat = match repeat {
            Repeat::Unknown => Repeat::Off,
            known => known,
        };
    }

    /// The Repeat button's click: off, the whole context, the one item,
    /// off again.
    pub fn cycle_repeat(&mut self) {
        self.repeat = match self.repeat {
            Repeat::Off | Repeat::Unknown => Repeat::All,
            Repeat::All => Repeat::One,
            Repeat::One => Repeat::Off,
        };
    }

    /// "Play next": `items` (of `album`) to the front of the queue, in
    /// their order.
    pub fn play_next(&mut self, items: Vec<T>, album: Option<Album>) {
        for item in items.into_iter().rev() {
            let queued = self.entry(item, album.clone());
            self.queued.push_front(queued);
        }
        if self.now.is_none() {
            self.take_queued();
        }
    }

    /// "Add to queue": `items` (of `album`) to the end of the queue.
    pub fn add(&mut self, items: Vec<T>, album: Option<Album>) {
        for item in items {
            let queued = self.entry(item, album.clone());
            self.queued.push_back(queued);
        }
        if self.now.is_none() {
            self.take_queued();
        }
    }

    fn entry(&mut self, item: T, album: Option<Album>) -> Queued<T> {
        self.next_id += 1;
        Queued {
            id: self.next_id,
            item,
            album,
        }
    }

    /// Takes the queued item `id` out of the queue; false if it is not
    /// there.
    pub fn remove(&mut self, id: u64) -> bool {
        let before = self.queued.len();
        self.queued.retain(|q| q.id != id);
        self.queued.len() != before
    }

    /// Moves the queued item `id` to `index` within the queue.
    pub fn move_to(&mut self, id: u64, index: usize) {
        let Some(from) = self.queued.iter().position(|q| q.id == id) else {
            return;
        };
        if let Some(queued) = self.queued.remove(from) {
            let index = index.min(self.queued.len());
            self.queued.insert(index, queued);
        }
    }

    /// Takes the context's item at play-order position `pos` (from
    /// [`Self::context_ahead`]) out of what is to come. What played and
    /// what plays cannot be.
    pub fn remove_ahead(&mut self, pos: usize) -> bool {
        if pos < self.first_ahead() || pos >= self.order.len() {
            return false;
        }
        self.removed.insert(self.order.remove(pos));
        true
    }

    /// Moves the context's item at `pos` to play-order position `to`,
    /// both among the items still to come.
    pub fn move_ahead(&mut self, pos: usize, to: usize) {
        let first = self.first_ahead();
        if pos < first || pos >= self.order.len() {
            return;
        }
        let item = self.order.remove(pos);
        self.order.insert(to.max(first).min(self.order.len()), item);
    }

    pub fn clear(&mut self) {
        self.queued.clear();
    }

    /// The hand-added items, to carry into the next playback.
    pub fn carry(&self) -> Vec<Queued<T>> {
        self.queued.iter().cloned().collect()
    }

    pub fn has_next(&self) -> bool {
        !self.queued.is_empty() || self.context_next().is_some()
    }

    /// Whether the context goes on after what plays now.
    pub fn context_has_next(&self) -> bool {
        self.context_next().is_some()
    }

    /// The first queued item if there is one, else the context's next.
    /// False, with nothing changed, at the end of both. `auto` is an item
    /// ending by itself: with repeat one it plays again; a user's Next
    /// goes on.
    pub fn next(&mut self, auto: bool) -> bool {
        if auto && self.repeat == Repeat::One && self.now.is_some() {
            return true;
        }
        if !self.queued.is_empty() {
            self.remember();
            self.take_queued();
            true
        } else {
            self.next_in_context()
        }
    }

    /// The context's next item, passing over the queue (video, which does
    /// not play the queue).
    pub fn next_in_context(&mut self) -> bool {
        let Some(next) = self.context_next() else {
            return false;
        };
        self.remember();
        self.at = Some(next);
        self.now = Some(Now::Context);
        true
    }

    fn context_next(&self) -> Option<usize> {
        let next = self.first_ahead();
        if next < self.order.len() {
            Some(next)
        } else if self.repeat == Repeat::All && !self.order.is_empty() {
            Some(0)
        } else {
            None
        }
    }

    fn remember(&mut self) {
        if let Some(now) = self.now.take() {
            self.history.push((now, self.at));
        }
    }

    fn take_queued(&mut self) {
        if let Some(queued) = self.queued.pop_front() {
            self.now = Some(Now::Queued(Box::new(queued)));
        }
    }

    pub fn has_previous(&self) -> bool {
        !self.history.is_empty()
    }

    /// Previous, with what plays `position` in: past [`RESTART_AFTER`] it
    /// starts again; else back to what played before. A queued item
    /// stepped back from returns to the front of the queue, so Next plays
    /// it again.
    pub fn previous(&mut self, position: Duration) -> Previous {
        if position > RESTART_AFTER && self.now.is_some() {
            return Previous::Restart;
        }
        let Some((now, at)) = self.history.pop() else {
            return Previous::Nothing;
        };
        if let Some(Now::Queued(queued)) = self.now.take() {
            self.queued.push_front(*queued);
        }
        self.now = Some(now);
        self.at = at;
        Previous::Back
    }

    /// Plays the queued item `id`; it leaves the queue. False if it is not
    /// there.
    pub fn play_queued(&mut self, id: u64) -> bool {
        let Some(from) = self.queued.iter().position(|q| q.id == id) else {
            return false;
        };
        let Some(queued) = self.queued.remove(from) else {
            return false;
        };
        self.remember();
        self.now = Some(Now::Queued(Box::new(queued)));
        true
    }

    /// Jumps the context to `pos` in its play order, keeping the queue.
    pub fn play_context_at(&mut self, pos: usize) -> bool {
        if pos >= self.order.len() {
            return false;
        }
        self.remember();
        self.at = Some(pos);
        self.now = Some(Now::Context);
        true
    }

    /// Shuffles the context still to come, or puts it back in order,
    /// keeping its current item where it is. `random(n)` is a number below
    /// `n`. The queue is never shuffled.
    pub fn set_shuffle(&mut self, on: bool, random: impl FnMut(usize) -> usize) {
        if on == self.shuffle {
            return;
        }
        self.shuffle = on;
        let current = self.at.and_then(|at| self.order.get(at).copied());
        if on {
            self.order = shuffled_from(&self.order, self.first_ahead(), random);
        } else {
            self.order = (0..self.context.len())
                .filter(|i| !self.removed.contains(i))
                .collect();
            self.at = current.and_then(|c| self.order.iter().position(|&i| i == c));
        }
        // History holds cursors into the old order.
        self.history.clear();
    }

    /// The Up next list: the context's played items, what plays, then
    /// (when there are any) the hand-added items under "Next in queue"
    /// and the context's rest under "Next from: <album>".
    pub fn rows(&self) -> Vec<QueueRow<T>> {
        let context = |(pos, item): (usize, &T), played| QueueRow::Context {
            pos,
            item: item.clone(),
            played,
        };
        let mut rows: Vec<QueueRow<T>> = self
            .context_behind()
            .into_iter()
            .map(|c| context(c, true))
            .collect();
        if let Some(item) = self.current() {
            let album = match self.current_is_queued() {
                true => self.current_album(),
                false => self.album.as_ref(),
            };
            rows.push(QueueRow::Playing {
                item: item.clone(),
                album: album.cloned(),
            });
        }
        let ahead = self.context_ahead();
        if !self.queued.is_empty() {
            rows.push(QueueRow::Heading(QueueHeading::NextInQueue));
            rows.extend(self.queued.iter().cloned().map(QueueRow::Queued));
            if let (false, Some(album)) = (ahead.is_empty(), &self.album) {
                rows.push(QueueRow::Heading(QueueHeading::NextFrom(album.clone())));
            }
        }
        rows.extend(ahead.into_iter().map(|c| context(c, false)));
        rows
    }
}

/// `order` with everything from `keep` on shuffled (Fisher–Yates);
/// `random(n)` is a number below `n`. What has played and what plays stay
/// where they are.
fn shuffled_from(
    order: &[usize],
    keep: usize,
    mut random: impl FnMut(usize) -> usize,
) -> Vec<usize> {
    let mut order = order.to_vec();
    let start = keep.min(order.len());
    let rest = &mut order[start..];
    for i in (1..rest.len()).rev() {
        rest.swap(i, random(i + 1));
    }
    order
}

#[cfg(test)]
mod tests {
    use spoolframe_types::{Item, MediaId, MediaKind, Namespace, PlaybackState, Track};

    use super::*;

    type Queue = PlayQueue<Item>;

    fn id(kind: MediaKind, key: &str) -> MediaId {
        MediaId::new(Namespace::Plex, kind, key).unwrap()
    }

    fn song(name: &str) -> Item {
        Item::Track(Track {
            id: id(MediaKind::Track, name),
            album_id: id(MediaKind::Album, "1"),
            artist_id: id(MediaKind::Artist, "1"),
            title: name.into(),
            track: None,
            disc: None,
            duration: None,
            playback: PlaybackState::default(),
            codec: None,
            bit_depth: None,
            sample_rate: None,
            album_track_count: None,
            artist_name: None,
            album_title: None,
            artwork: Vec::new(),
        })
    }

    fn songs(names: &str) -> Vec<Item> {
        names.split(' ').map(song).collect()
    }

    fn album(title: &str) -> Album {
        Album {
            id: id(MediaKind::Album, title),
            artist_id: id(MediaKind::Artist, "1"),
            artist_name: None,
            title: title.into(),
            year: None,
            artwork: Vec::new(),
            track_count: None,
        }
    }

    /// `names` playing as a context from its first item, nothing queued.
    fn context(names: &str) -> Queue {
        Queue::new(songs(names), 0, None, Vec::new())
    }

    fn title(item: Option<&Item>) -> String {
        item.map_or("-".into(), |i| i.title().to_owned())
    }

    /// The titles played by pressing Next until the end, the current one
    /// first.
    fn played(queue: &mut Queue) -> String {
        let mut out = vec![title(queue.current())];
        while queue.next(false) {
            out.push(title(queue.current()));
            assert!(out.len() < 50, "next never ends");
        }
        out.join(" ")
    }

    /// A shuffle that always swaps with the first: deterministic, and
    /// not the identity.
    fn first(_: usize) -> usize {
        0
    }

    #[test]
    fn the_context_plays_in_order() {
        assert_eq!(played(&mut context("a b c")), "a b c");
    }

    #[test]
    fn play_next_plays_after_the_current_item_then_the_context_goes_on() {
        let mut queue = context("a b c");
        queue.play_next(songs("x"), None);
        assert_eq!(played(&mut queue), "a x b c");
    }

    #[test]
    fn play_next_goes_before_what_was_added_to_the_queue() {
        let mut queue = context("a b");
        queue.add(songs("x"), None);
        queue.play_next(songs("y z"), None);
        assert_eq!(played(&mut queue), "a y z x b");
    }

    #[test]
    fn add_to_queue_goes_to_the_end_of_the_queue() {
        let mut queue = context("a b");
        queue.add(songs("x"), None);
        queue.add(songs("y"), None);
        assert_eq!(played(&mut queue), "a x y b");
    }

    #[test]
    fn previous_goes_back_through_what_played_queued_items_included() {
        let mut queue = context("a b c");
        queue.add(songs("x"), None);
        queue.next(false); // x
        queue.next(false); // b
        assert_eq!(queue.previous(Duration::ZERO), Previous::Back);
        assert_eq!(title(queue.current()), "x");
    }

    #[test]
    fn previous_then_next_returns_to_the_context_where_it_was() {
        let mut queue = context("a b c");
        queue.add(songs("x"), None);
        queue.next(false); // x
        queue.next(false); // b
        queue.previous(Duration::ZERO); // x
        queue.previous(Duration::ZERO); // a
        assert_eq!(played(&mut queue), "a x b c");
    }

    #[test]
    fn previous_at_the_start_does_nothing() {
        let mut queue = Queue::new(songs("a b"), 1, None, Vec::new());
        assert_eq!(queue.previous(Duration::ZERO), Previous::Nothing);
    }

    #[test]
    fn previous_past_three_seconds_starts_the_item_again() {
        let mut queue = context("a b");
        queue.next(false);
        assert_eq!(queue.previous(Duration::from_secs(4)), Previous::Restart);
        assert_eq!(title(queue.current()), "b");
        assert_eq!(queue.previous(Duration::from_secs(3)), Previous::Back);
        assert_eq!(title(queue.current()), "a");
    }

    #[test]
    fn shuffle_never_touches_queued_items() {
        let mut queue = context("a b c d");
        queue.add(songs("x y"), None);
        queue.set_shuffle(true, first);
        let rest = played(&mut queue);
        assert!(rest.starts_with("a x y "), "{rest}");
    }

    #[test]
    fn shuffle_keeps_the_current_item_and_shuffles_the_rest_of_the_context() {
        let mut queue = context("a b c d");
        queue.set_shuffle(true, first);
        assert_eq!(played(&mut queue), "a c d b");
    }

    #[test]
    fn shuffle_off_puts_the_context_back_in_order_from_the_current_item() {
        let mut queue = context("a b c d");
        queue.set_shuffle(true, first);
        queue.next(false); // c
        queue.set_shuffle(false, first);
        assert_eq!(played(&mut queue), "c d");
    }

    #[test]
    fn repeat_goes_round_off_all_one() {
        let mut queue = context("a");
        let mut seen = vec![queue.repeat()];
        for _ in 0..3 {
            queue.cycle_repeat();
            seen.push(queue.repeat());
        }
        assert_eq!(seen, [Repeat::Off, Repeat::All, Repeat::One, Repeat::Off]);
        queue.set_repeat(Repeat::Unknown);
        assert_eq!(queue.repeat(), Repeat::Off);
    }

    #[test]
    fn repeat_all_wraps_the_context_but_queued_items_play_once() {
        let mut queue = context("a b");
        queue.add(songs("x"), None);
        queue.cycle_repeat();
        let order: Vec<_> = (0..5)
            .map(|_| {
                queue.next(false);
                title(queue.current())
            })
            .collect();
        assert_eq!(order.join(" "), "x b a b a");
    }

    #[test]
    fn repeat_one_plays_the_item_again_at_its_end_but_next_goes_on() {
        let mut queue = context("a b");
        queue.set_repeat(Repeat::One);
        assert!(queue.next(true));
        assert_eq!(title(queue.current()), "a");
        assert!(queue.next(false));
        assert_eq!(title(queue.current()), "b");
    }

    #[test]
    fn a_shuffled_context_then_a_queued_album_in_order() {
        // Shuffle on for the first album, the second added to the queue.
        // Queued items come first, in order; then the first album's rest,
        // shuffled.
        let mut queue = context("a b c d");
        queue.set_shuffle(true, first);
        queue.add(songs("p q r"), Some(album("second")));
        assert_eq!(played(&mut queue), "a p q r c d b");
    }

    #[test]
    fn a_new_context_keeps_the_queue() {
        let mut queue = context("a b");
        queue.add(songs("x"), None);
        queue.set_context(songs("p q"), 0, None);
        assert_eq!(played(&mut queue), "p x q");
    }

    #[test]
    fn a_carried_queue_plays_after_the_new_contexts_first_item() {
        let mut old = context("a");
        old.add(songs("x y"), None);
        let mut queue = Queue::new(songs("p q"), 0, None, old.carry());
        assert_eq!(played(&mut queue), "p x y q");
    }

    #[test]
    fn items_added_after_a_carry_get_new_ids() {
        let mut old = context("a");
        old.add(songs("x"), None);
        let mut queue = Queue::new(songs("p"), 0, None, old.carry());
        queue.add(songs("y"), None);
        let ids: Vec<u64> = queue.queued().map(|q| q.id).collect();
        assert_ne!(ids[0], ids[1]);
    }

    #[test]
    fn an_empty_context_plays_the_queue() {
        let mut queue = Queue::new(Vec::new(), 0, None, Vec::new());
        queue.add(songs("x y"), None);
        assert_eq!(played(&mut queue), "x y");
    }

    #[test]
    fn an_extended_context_goes_on_into_what_was_added() {
        let mut queue = context("a");
        queue.extend_context(songs("b"));
        assert_eq!(played(&mut queue), "a b");
    }

    #[test]
    fn what_is_ahead_can_be_replaced_and_what_played_stays() {
        let mut queue = context("a b c");
        queue.next(false); // b
        queue.replace_ahead(songs("z y"), true);
        assert!(queue.shuffle());
        assert_eq!(played(&mut queue), "b z y");
        queue.previous(Duration::ZERO);
        queue.previous(Duration::ZERO);
        assert_eq!(title(queue.current()), "b");
    }

    #[test]
    fn next_in_context_passes_over_the_queue() {
        let mut queue = context("a b");
        queue.add(songs("x"), None);
        queue.next_in_context();
        assert_eq!(title(queue.current()), "b");
    }

    #[test]
    fn a_queued_item_brings_its_own_album() {
        let mut queue = Queue::new(songs("a"), 0, Some(album("first")), Vec::new());
        queue.add(songs("x"), Some(album("second")));
        queue.next(false);
        assert_eq!(
            queue.current_album().map(|a| a.title.as_str()),
            Some("second")
        );
    }

    #[test]
    fn a_queued_item_can_be_moved() {
        let mut queue = context("a");
        queue.add(songs("x y z"), None);
        let z = queue.queued().last().unwrap().id;
        queue.move_to(z, 0);
        assert_eq!(played(&mut queue), "a z x y");
    }

    #[test]
    fn a_queued_item_can_be_removed() {
        let mut queue = context("a");
        queue.add(songs("x y"), None);
        let x = queue.queued().next().unwrap().id;
        assert!(queue.remove(x));
        assert_eq!(played(&mut queue), "a y");
    }

    #[test]
    fn an_item_taken_out_ahead_stays_out_when_shuffle_goes_off() {
        let mut queue = context("a b c d");
        queue.set_shuffle(true, first); // a c d b
        assert!(queue.remove_ahead(1)); // c
        assert!(!queue.remove_ahead(0), "what plays stays");
        queue.set_shuffle(false, first);
        assert_eq!(played(&mut queue), "a b d");
    }

    #[test]
    fn an_item_ahead_can_be_moved_among_those_to_come() {
        let mut queue = context("a b c d");
        queue.move_ahead(3, 0);
        assert_eq!(played(&mut queue), "a d b c");
    }

    #[test]
    fn clear_empties_the_queue_and_keeps_the_context() {
        let mut queue = context("a b");
        queue.add(songs("x y"), None);
        queue.clear();
        assert_eq!(played(&mut queue), "a b");
    }

    #[test]
    fn playing_a_queued_item_takes_it_out_of_the_queue() {
        let mut queue = context("a b");
        queue.add(songs("x y"), None);
        let y = queue.queued().last().unwrap().id;
        queue.play_queued(y);
        assert_eq!(played(&mut queue), "y x b");
    }

    #[test]
    fn playing_a_context_item_jumps_there_and_keeps_the_queue() {
        let mut queue = context("a b c d");
        queue.add(songs("x"), None);
        queue.play_context_at(2);
        assert_eq!(played(&mut queue), "c x d");
        assert_eq!(queue.context_position(), Some((4, 4)));
    }

    #[test]
    fn the_context_ahead_lists_what_the_context_plays_next() {
        let mut queue = context("a b c");
        queue.add(songs("x"), None);
        queue.next(false); // x: the context is still at a
        let ahead: Vec<_> = queue
            .context_ahead()
            .into_iter()
            .map(|(pos, item)| format!("{pos}:{}", title(Some(item))))
            .collect();
        assert_eq!(ahead.join(" "), "1:b 2:c");
        assert_eq!(queue.ahead_count(), 2);
    }

    fn row(row: &QueueRow<Item>) -> String {
        match row {
            QueueRow::Heading(QueueHeading::NextInQueue) => "[queue]".into(),
            QueueRow::Heading(QueueHeading::NextFrom(a)) => format!("[from {}]", a.title),
            QueueRow::Context { item, played, .. } => {
                format!("{}{}", title(Some(item)), if *played { "'" } else { "" })
            }
            QueueRow::Playing { item, .. } => format!("({})", title(Some(item))),
            QueueRow::Queued(q) => format!("+{}", title(Some(&q.item))),
        }
    }

    #[test]
    fn up_next_lists_played_playing_queued_then_the_albums_rest() {
        let mut queue = Queue::new(songs("a b c"), 1, Some(album("Low Tide")), Vec::new());
        queue.add(songs("x"), None);
        let rows: Vec<_> = queue.rows().iter().map(row).collect();
        assert_eq!(rows.join(" "), "a' (b) [queue] +x [from Low Tide] c");
        queue.clear();
        let rows: Vec<_> = queue.rows().iter().map(row).collect();
        assert_eq!(rows.join(" "), "a' (b) c");
    }

    #[test]
    fn a_saved_queue_comes_back_the_same() {
        let mut queue = Queue::new(songs("a b c"), 1, None, Vec::new());
        queue.add(songs("x"), None);
        queue.set_shuffle(true, first);
        let saved = serde_json::to_string(&queue).unwrap();
        let mut restored: Queue = serde_json::from_str(&saved).unwrap();
        assert_eq!(played(&mut restored), played(&mut queue));
    }

    /// A queue as the Linux app saved it before the SDK had one: its own
    /// `Repeat` names ("All"), and no `removed`.
    #[test]
    fn a_queue_the_linux_app_saved_still_loads() {
        let track = |name: &str| serde_json::to_value(song(name)).unwrap();
        let saved = serde_json::json!({
            "context": [track("a"), track("b"), track("c")],
            "album": null,
            "order": [0, 2, 1],
            "at": 0,
            "queued": [{"id": 1, "item": track("x"), "album": null}],
            "now": "Context",
            "shuffle": true,
            "repeat": "All",
            "next_id": 1,
        });
        let mut queue: Queue = serde_json::from_value(saved).unwrap();
        assert_eq!(queue.repeat(), Repeat::All);
        assert!(queue.shuffle());
        let order: Vec<_> = (0..4)
            .map(|_| {
                queue.next(false);
                title(queue.current())
            })
            .collect();
        assert_eq!(order.join(" "), "x c b a");
    }
}
