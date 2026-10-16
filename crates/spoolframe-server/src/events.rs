//! Each connection's events stream (ARCHITECTURE §33): the server opens
//! it after `Hello` and writes `Event` frames on it for the life of the
//! connection.
//!
//! Writers take turns on one stream, so two frames never interleave, and
//! each stream has its own turn: a device that reads nothing holds up only
//! its own events.
//!
//! The server's addresses (PX-12) are pushed to a connection when it is
//! signed in, and to every signed-in connection when the endpoint's
//! addresses change. Each push reads the addresses while it holds the
//! stream's turn, so of two pushes on one stream the later one reads later:
//! the last frame a device gets is never older than the one before it.

use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use iroh::endpoint::SendStream;
use spoolframe_protocol::Event;
use spoolframe_protocol::codec::write_frame;

use crate::announce::Told;
use crate::network::Connections;
use crate::playlists::PlaylistNews;
use crate::principal::Principal;

/// One connection's events stream, and the turn to write on it.
pub(crate) type EventStream = Arc<tokio::sync::Mutex<SendStream>>;

/// The events stream of each connection that said `Hello`, by the
/// connection's stable id.
#[derive(Debug, Clone, Default)]
pub(crate) struct EventStreams(Arc<Mutex<HashMap<usize, Pushing>>>);

/// A connection's events stream, and how many events wait for it.
type Pushing = (EventStream, Arc<Backlog>);

/// The most events waiting for one connection's stream: a client that
/// never reads its events stream holds every write behind flow control,
/// and each waiting event is a task. Past it, events are dropped.
const MAX_WAITING: usize = 32;

/// How many events wait for one stream.
#[derive(Debug, Default)]
pub(crate) struct Backlog(std::sync::atomic::AtomicUsize);

impl Backlog {
    /// A place in the backlog, or none when [`MAX_WAITING`] already wait.
    fn enter(self: &Arc<Self>) -> Option<Waiting> {
        use std::sync::atomic::Ordering;
        self.0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < MAX_WAITING).then_some(n + 1)
            })
            .ok()
            .map(|_| Waiting(Arc::clone(self)))
    }
}

/// One event's place in a [`Backlog`], given back when it is written.
struct Waiting(Arc<Backlog>);

impl Drop for Waiting {
    fn drop(&mut self) {
        self.0.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

impl EventStreams {
    pub(crate) fn insert(&self, id: usize, stream: SendStream) {
        self.0.lock().insert(
            id,
            (Arc::new(tokio::sync::Mutex::new(stream)), Arc::default()),
        );
    }

    pub(crate) fn remove(&self, id: usize) {
        self.0.lock().remove(&id);
    }

    /// Every stream, taken out: nothing more is written on them but what
    /// the taker writes.
    pub(crate) fn drain(&self) -> Vec<EventStream> {
        self.0
            .lock()
            .drain()
            .map(|(_, (stream, _))| stream)
            .collect()
    }

    /// Write `event` on connection `id`'s stream, in the background; a
    /// connection gone gets nothing.
    /// Past [`MAX_WAITING`] events waiting for it, the event is dropped.
    pub(crate) fn send(&self, id: usize, event: Event) {
        let Some((stream, backlog)) = self.get(id) else {
            return;
        };
        let Some(waiting) = backlog.enter() else {
            tracing::debug!(
                max = MAX_WAITING,
                "a client takes no events; one more is dropped"
            );
            return;
        };
        tokio::spawn(async move {
            let _waiting = waiting;
            let mut stream = stream.lock().await;
            if let Err(e) = write_frame(&mut *stream, &event).await {
                tracing::debug!(error = %e, "cannot push an event");
            }
        });
    }

    fn get(&self, id: usize) -> Option<Pushing> {
        self.0.lock().get(&id).cloned()
    }

    fn ids(&self) -> Vec<usize> {
        self.0.lock().keys().copied().collect()
    }
}

/// What tells signed-in connections where the server is (PX-12).
#[derive(Debug, Clone)]
pub(crate) struct Addresses {
    pub streams: EventStreams,
    pub connections: Connections,
    pub told: Told,
}

impl Addresses {
    /// Push the addresses to connection `id`, in the background, if it has
    /// an events stream and is signed in when its turn comes. A connection
    /// signed out, or gone, gets nothing.
    pub(crate) fn tell(&self, id: usize) {
        let Some((stream, backlog)) = self.streams.get(id) else {
            return;
        };
        // The addresses are pushed whole each time: one dropped while the
        // client takes no events is told again at the next change.
        let Some(waiting) = backlog.enter() else {
            return;
        };
        let this = self.clone();
        tokio::spawn(async move {
            let _waiting = waiting;
            let mut stream = stream.lock().await;
            if !this.connections.is_signed_in(id) {
                return;
            }
            let told = this.told.clone();
            let Ok(event) = tokio::task::spawn_blocking(move || told.event()).await else {
                return;
            };
            if let Err(e) = write_frame(&mut *stream, &event).await {
                tracing::debug!(error = %e, "cannot push the server's addresses");
            }
        });
    }

    /// Push them to every signed-in connection.
    fn tell_everyone(&self) {
        for id in self.streams.ids() {
            self.tell(id);
        }
    }

    /// Push them to every signed-in connection each time what is told
    /// changes: an interface came up or went down. Runs until the endpoint
    /// closes.
    pub(crate) async fn tell_changes(self) {
        let mut changes = self.told.changes();
        let mut last = self.current().await;
        while changes.recv().await.is_some() {
            let now = self.current().await;
            if now != last {
                tracing::info!(?now, "the server's addresses changed; telling the devices");
                last = now;
                self.tell_everyone();
            }
        }
    }

    async fn current(&self) -> Option<Event> {
        let told = self.told.clone();
        tokio::task::spawn_blocking(move || told.event()).await.ok()
    }
}

/// The users each connection that listed `spoolframe:playlists` has
/// made requests as, by its stable id: who to tell of playlist changes
/// (PX-40), and whether it listed `spoolframe:liked` too (PX-43): one
/// that did not is not told of Liked and All, which it does not list. A
/// connection carries any number of sessions; each one used is noted,
/// and all go when it closes.
#[derive(Debug, Clone, Default)]
pub(crate) struct PlaylistListeners(Arc<Mutex<HashMap<usize, Listener>>>);

#[derive(Debug, Default)]
struct Listener {
    who: HashSet<Principal>,
    liked: bool,
}

impl PlaylistListeners {
    pub(crate) fn note(&self, id: usize, principal: Principal, liked: bool) {
        let mut all = self.0.lock();
        let listener = all.entry(id).or_default();
        listener.who.insert(principal);
        listener.liked = liked;
    }

    pub(crate) fn remove(&self, id: usize) {
        self.0.lock().remove(&id);
    }

    fn each(&self) -> Vec<(usize, Vec<Principal>, bool)> {
        self.0
            .lock()
            .iter()
            .map(|(id, l)| (*id, l.who.iter().copied().collect(), l.liked))
            .collect()
    }
}

/// The event `news` is for a connection whose users are `who`, if any: a
/// playlist one of them sees now changed; one some saw is gone to all of
/// them; the pins of one of them changed. A built-in playlist's news is
/// only for a connection that takes them (`liked`).
fn playlist_event(news: &PlaylistNews, who: &[Principal], liked: bool) -> Option<Event> {
    match news {
        PlaylistNews::Playlist {
            id,
            revision,
            before,
            after,
        } => {
            let builtin = [before, after]
                .iter()
                .any(|seen| seen.as_ref().is_some_and(|s| s.builtin));
            if builtin && !liked {
                return None;
            }
            let sees = |seen: &Option<crate::playlists::Seen>| {
                seen.as_ref()
                    .is_some_and(|seen| who.iter().any(|p| seen.sees(*p)))
            };
            if sees(after) {
                Some(Event::PlaylistChanged {
                    id: id.clone(),
                    revision: *revision,
                })
            } else if sees(before) {
                Some(Event::PlaylistGone { id: id.clone() })
            } else {
                None
            }
        }
        PlaylistNews::Pins { principal } => who.contains(principal).then_some(Event::PinsChanged),
    }
}

/// Tell each listening connection of the playlist changes it is concerned
/// with, as they are made, until the playlists' news ends.
pub(crate) async fn tell_playlist_news(
    mut news: tokio::sync::broadcast::Receiver<PlaylistNews>,
    listeners: PlaylistListeners,
    streams: EventStreams,
) {
    use tokio::sync::broadcast::error::RecvError;
    loop {
        let told = match news.recv().await {
            Ok(told) => told,
            Err(RecvError::Lagged(missed)) => {
                tracing::debug!(missed, "playlist changes went untold");
                continue;
            }
            Err(RecvError::Closed) => return,
        };
        for (id, who, liked) in listeners.each() {
            if let Some(event) = playlist_event(&told, &who, liked) {
                streams.send(id, event);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stream with [`MAX_WAITING`] events waiting takes no more until
    /// one is written.
    #[test]
    fn a_backlog_holds_at_most_max_waiting() {
        let backlog = Arc::new(Backlog::default());
        let held: Vec<Waiting> = (0..MAX_WAITING)
            .map(|_| backlog.enter().expect("room"))
            .collect();
        assert!(backlog.enter().is_none());
        drop(held);
        assert!(backlog.enter().is_some());
    }

    fn principal(n: u128) -> Principal {
        Principal::LocalUser(crate::store::users::LocalUserId::from_uuid(
            uuid::Uuid::from_u128(n),
        ))
    }

    #[test]
    fn a_playlist_change_is_told_to_who_sees_it_and_its_loss_to_who_saw_it() {
        let (alice, bob, carol) = (principal(1), principal(2), principal(3));
        let shared = crate::playlists::Seen::for_tests(alice, false, &[bob]);
        let personal = crate::playlists::Seen::for_tests(alice, false, &[]);
        let unshared = PlaylistNews::Playlist {
            id: "p".into(),
            revision: 4,
            before: Some(shared),
            after: Some(personal),
        };
        assert_eq!(
            playlist_event(&unshared, &[alice], false),
            Some(Event::PlaylistChanged {
                id: "p".into(),
                revision: 4
            })
        );
        assert_eq!(
            playlist_event(&unshared, &[bob], false),
            Some(Event::PlaylistGone { id: "p".into() })
        );
        assert_eq!(playlist_event(&unshared, &[carol], false), None);
        // A connection Bob and Alice both use sees it still.
        assert!(matches!(
            playlist_event(&unshared, &[bob, alice], false),
            Some(Event::PlaylistChanged { .. })
        ));
        let pins = PlaylistNews::Pins { principal: bob };
        assert_eq!(
            playlist_event(&pins, &[bob], false),
            Some(Event::PinsChanged)
        );
        assert_eq!(playlist_event(&pins, &[alice], false), None);
    }

    /// PX-43: a connection that did not list `spoolframe:liked` is not
    /// told of Liked or All, which it never lists.
    #[test]
    fn a_builtin_s_change_is_told_only_to_who_takes_builtins() {
        let alice = principal(1);
        let mut liked = crate::playlists::Seen::for_tests(alice, false, &[]);
        liked.builtin = true;
        let changed = PlaylistNews::Playlist {
            id: "liked".into(),
            revision: 2,
            before: Some(liked.clone()),
            after: Some(liked),
        };
        assert_eq!(playlist_event(&changed, &[alice], false), None);
        assert_eq!(
            playlist_event(&changed, &[alice], true),
            Some(Event::PlaylistChanged {
                id: "liked".into(),
                revision: 2
            })
        );
    }
}
