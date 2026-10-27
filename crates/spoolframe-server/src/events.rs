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
use crate::connection_users::ConnectionUsers;
use crate::network::Connections;
use crate::playlists::PlaylistNews;
use crate::principal::Principal;
use spoolframe_types::MediaId;

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
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
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

/// The next of `news`, or `None` once it ends; news missed while the
/// teller lagged is let go, as `what` went untold.
async fn next_news<T: Clone>(
    news: &mut tokio::sync::broadcast::Receiver<T>,
    what: &str,
) -> Option<T> {
    use tokio::sync::broadcast::error::RecvError;
    loop {
        match news.recv().await {
            Ok(told) => return Some(told),
            Err(RecvError::Lagged(missed)) => tracing::debug!(missed, "{what} went untold"),
            Err(RecvError::Closed) => return None,
        }
    }
}

/// The connections told of playlist changes (PX-40) to what each user
/// they act as ([`ConnectionUsers`]) sees, by stable id.
#[derive(Debug, Clone)]
pub(crate) struct PlaylistListeners {
    listening: Arc<Mutex<HashSet<usize>>>,
    users: ConnectionUsers,
}

impl PlaylistListeners {
    pub(crate) fn new(users: ConnectionUsers) -> Self {
        Self {
            listening: Arc::default(),
            users,
        }
    }

    pub(crate) fn note(&self, id: usize) {
        self.listening.lock().insert(id);
    }

    pub(crate) fn remove(&self, id: usize) {
        self.listening.lock().remove(&id);
    }

    /// The event of `news` for each listening connection concerned.
    fn events(&self, news: &PlaylistNews) -> Vec<(usize, Event)> {
        let listening: Vec<usize> = self.listening.lock().iter().copied().collect();
        listening
            .into_iter()
            .filter_map(|id| playlist_event(news, &self.users.who(id)).map(|event| (id, event)))
            .collect()
    }
}

/// The event `news` is for a connection whose users are `who`, if any: a
/// playlist one of them sees now changed; one some saw is gone to all of
/// them; the pins of one of them changed.
fn playlist_event(news: &PlaylistNews, who: &[Principal]) -> Option<Event> {
    match news {
        PlaylistNews::Playlist {
            id,
            revision,
            before,
            after,
        } => {
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
        // For music downloads: [`tell_liked_news`].
        PlaylistNews::Liked { .. } => None,
    }
}

/// Tell each listening connection of the playlist changes it is concerned
/// with, as they are made, until the playlists' news ends.
pub(crate) async fn tell_playlist_news(
    mut news: tokio::sync::broadcast::Receiver<PlaylistNews>,
    listeners: PlaylistListeners,
    streams: EventStreams,
) {
    while let Some(told) = next_news(&mut news, "playlist changes").await {
        for (id, event) in listeners.events(&told) {
            streams.send(id, event);
        }
    }
}

/// The items each connection asked the tracks of or plays, by its stable
/// id (PX-03): who to tell when one
/// of their subtitles is timed anew. The items it asked about, it may
/// see. At most [`MAX_WATCHED`] a connection, the oldest let go.
#[derive(Debug, Clone, Default)]
pub(crate) struct SubtitleListeners(Arc<Mutex<HashMap<usize, Vec<MediaId>>>>);

/// The most items one connection is told of: a few screens of a browse.
const MAX_WATCHED: usize = 64;

impl SubtitleListeners {
    pub(crate) fn note(&self, id: usize, item: &MediaId) {
        let mut all = self.0.lock();
        let items = all.entry(id).or_default();
        items.retain(|i| i != item);
        if items.len() == MAX_WATCHED {
            items.remove(0);
        }
        items.push(item.clone());
    }

    pub(crate) fn remove(&self, id: usize) {
        self.0.lock().remove(&id);
    }

    /// The connections that asked about `item`.
    fn of(&self, item: &MediaId) -> Vec<usize> {
        self.0
            .lock()
            .iter()
            .filter(|(_, items)| items.contains(item))
            .map(|(id, _)| *id)
            .collect()
    }
}

/// Tell each connection that asked about an item of its subtitles' timing
/// changes, as they are made, until the news ends.
pub(crate) async fn tell_subtitle_news(
    mut news: tokio::sync::broadcast::Receiver<crate::subtitle_sync::TimingNews>,
    listeners: SubtitleListeners,
    streams: EventStreams,
) {
    while let Some(told) = next_news(&mut news, "subtitle timing changes").await {
        for id in listeners.of(&told.id) {
            streams.send(
                id,
                Event::SubtitleTimingChanged {
                    id: told.id.clone(),
                    track: told.track,
                    timing: told.timing.clone(),
                },
            );
        }
    }
}

/// The connections that named the experimental `federation` feature, by
/// stable id (P8-02): told when the paired servers' libraries change.
#[derive(Debug, Clone, Default)]
pub(crate) struct FederationListeners(Arc<Mutex<HashSet<usize>>>);

impl FederationListeners {
    pub(crate) fn note(&self, id: usize) {
        self.0.lock().insert(id);
    }

    pub(crate) fn remove(&self, id: usize) {
        self.0.lock().remove(&id);
    }

    fn ids(&self) -> Vec<usize> {
        self.0.lock().iter().copied().collect()
    }
}

/// Tell each connection that named the `federation` feature that the
/// libraries changed, each time `news` says so, until it ends.
pub(crate) async fn tell_libraries_news(
    mut news: tokio::sync::broadcast::Receiver<()>,
    listeners: FederationListeners,
    streams: EventStreams,
) {
    while next_news(&mut news, "library changes").await.is_some() {
        for id in listeners.ids() {
            streams.send(id, Event::LibrariesChanged);
        }
    }
}

/// The connections, by stable id, told when a download of a user they act
/// as ([`ConnectionUsers`]) moves (PX-38). Only their own: a download is
/// its user's business. The same are told of their user's likes (PX-51,
/// [`tell_liked_news`]).
#[derive(Debug, Clone)]
pub(crate) struct DownloadListeners {
    listed: Arc<Mutex<HashSet<usize>>>,
    users: ConnectionUsers,
}

impl DownloadListeners {
    pub(crate) fn new(users: ConnectionUsers) -> Self {
        Self {
            listed: Arc::default(),
            users,
        }
    }

    pub(crate) fn note(&self, id: usize) {
        self.listed.lock().insert(id);
    }

    pub(crate) fn remove(&self, id: usize) {
        self.listed.lock().remove(&id);
    }

    /// The listening connections that act as `principal`.
    fn of(&self, principal: &Principal) -> Vec<usize> {
        let mut ids = self.users.of(principal);
        let listed = self.listed.lock();
        ids.retain(|id| listed.contains(id));
        ids
    }
}

/// Tell each connection of a download's user of its moves, as the
/// downloads tell them (a progress once per percent or every 2 s), until
/// they end.
pub(crate) async fn tell_download_news(
    mut news: tokio::sync::broadcast::Receiver<crate::downloads::DownloadChange>,
    listeners: DownloadListeners,
    streams: EventStreams,
) {
    while let Some(told) = next_news(&mut news, "download changes").await {
        let Ok(principal) = told.principal.parse::<Principal>() else {
            continue;
        };
        let connections = listeners.of(&principal);
        if connections.is_empty() {
            continue;
        }
        let state = told.state.to_wire();
        for id in connections {
            streams.send(
                id,
                Event::DownloadChanged {
                    id: told.id.clone(),
                    state: state.clone(),
                },
            );
        }
    }
}

/// Tell each connection of a user when they like or unlike a track
/// (PX-51): a device that downloaded
/// Liked asks what it holds now. Until the playlists' news ends.
pub(crate) async fn tell_liked_news(
    mut news: tokio::sync::broadcast::Receiver<PlaylistNews>,
    listeners: DownloadListeners,
    streams: EventStreams,
) {
    while let Some(told) = next_news(&mut news, "liked changes").await {
        let PlaylistNews::Liked { principal } = told else {
            continue;
        };
        for id in listeners.of(&principal) {
            streams.send(id, Event::LikedChanged);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spoolframe_protocol::SessionId;

    /// PX-38: a download's moves go to its user's listening connections
    /// alone, and to none once they close.
    #[test]
    fn download_listeners_hear_of_their_own_user_s_downloads() {
        let users = ConnectionUsers::default();
        let listeners = DownloadListeners::new(users.clone());
        let (alice, bob) = (principal(1), principal(2));
        users.note(1, &session(1), alice);
        users.note(2, &session(2), bob);
        users.note(2, &session(3), alice);
        users.note(3, &session(4), alice);
        listeners.note(1);
        listeners.note(2);
        let mut of_alice = listeners.of(&alice);
        of_alice.sort();
        assert_eq!(of_alice, [1, 2], "3 is not listening");
        listeners.remove(2);
        assert!(listeners.of(&bob).is_empty());
    }

    /// Review 2026-10-20, finding 6: a connection that signed out as alice
    /// and in as bob is told neither of alice's playlists nor her pins.
    #[test]
    fn a_connection_signed_out_of_a_user_is_told_none_of_their_playlists() {
        let users = ConnectionUsers::default();
        let listeners = PlaylistListeners::new(users.clone());
        let (alice, bob) = (principal(1), principal(2));
        users.note(1, &session(1), alice);
        listeners.note(1);
        users.forget(&session(1).0);
        users.note(1, &session(2), bob);
        let hers = crate::playlists::Seen::for_tests(alice, false, &[]);
        let changed = PlaylistNews::Playlist {
            id: "p".into(),
            revision: 2,
            before: Some(hers.clone()),
            after: Some(hers),
        };
        let pins = PlaylistNews::Pins { principal: alice };
        assert_eq!(
            (listeners.events(&changed), listeners.events(&pins)),
            (vec![], vec![])
        );
    }

    fn session(n: u8) -> SessionId {
        SessionId(format!("session-{n}"))
    }

    fn item(n: u32) -> MediaId {
        format!("plex:movie:{n}").parse().unwrap()
    }

    /// PX-03: a connection is told of the items it asked about, the last
    /// [`MAX_WATCHED`] of them; one that closed, of none.
    #[test]
    fn subtitle_listeners_hear_of_the_items_they_asked_about() {
        let listeners = SubtitleListeners::default();
        listeners.note(1, &item(1));
        listeners.note(2, &item(1));
        listeners.note(2, &item(2));
        assert_eq!(listeners.of(&item(2)), [2]);
        let mut both = listeners.of(&item(1));
        both.sort();
        assert_eq!(both, [1, 2]);
        for n in 10..10 + MAX_WATCHED as u32 {
            listeners.note(1, &item(n));
        }
        assert!(listeners.of(&item(1)) == [2], "the oldest is let go");
        listeners.remove(2);
        assert!(listeners.of(&item(2)).is_empty());
    }

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
            playlist_event(&unshared, &[alice]),
            Some(Event::PlaylistChanged {
                id: "p".into(),
                revision: 4
            })
        );
        assert_eq!(
            playlist_event(&unshared, &[bob]),
            Some(Event::PlaylistGone { id: "p".into() })
        );
        assert_eq!(playlist_event(&unshared, &[carol]), None);
        // A connection Bob and Alice both use sees it still.
        assert!(matches!(
            playlist_event(&unshared, &[bob, alice]),
            Some(Event::PlaylistChanged { .. })
        ));
        let pins = PlaylistNews::Pins { principal: bob };
        assert_eq!(playlist_event(&pins, &[bob]), Some(Event::PinsChanged));
        assert_eq!(playlist_event(&pins, &[alice]), None);
    }

    /// PX-43: Liked and All are told of as any playlist is.
    #[test]
    fn a_builtin_s_change_is_told_to_its_owner() {
        let alice = principal(1);
        let mut liked = crate::playlists::Seen::for_tests(alice, false, &[]);
        liked.builtin = true;
        let changed = PlaylistNews::Playlist {
            id: "liked".into(),
            revision: 2,
            before: Some(liked.clone()),
            after: Some(liked),
        };
        assert_eq!(
            playlist_event(&changed, &[alice]),
            Some(Event::PlaylistChanged {
                id: "liked".into(),
                revision: 2
            })
        );
    }
}
