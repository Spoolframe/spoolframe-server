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
use std::collections::HashMap;
use std::sync::Arc;

use iroh::endpoint::SendStream;
use spoolframe_protocol::Event;
use spoolframe_protocol::codec::write_frame;

use crate::announce::Told;
use crate::network::Connections;

/// One connection's events stream, and the turn to write on it.
pub(crate) type EventStream = Arc<tokio::sync::Mutex<SendStream>>;

/// The events stream of each connection that said `Hello`, by the
/// connection's stable id.
#[derive(Debug, Clone, Default)]
pub(crate) struct EventStreams(Arc<Mutex<HashMap<usize, EventStream>>>);

impl EventStreams {
    pub(crate) fn insert(&self, id: usize, stream: SendStream) {
        self.0
            .lock()
            .insert(id, Arc::new(tokio::sync::Mutex::new(stream)));
    }

    pub(crate) fn remove(&self, id: usize) {
        self.0.lock().remove(&id);
    }

    /// Every stream, taken out: nothing more is written on them but what
    /// the taker writes.
    pub(crate) fn drain(&self) -> Vec<EventStream> {
        self.0.lock().drain().map(|(_, stream)| stream).collect()
    }

    fn get(&self, id: usize) -> Option<EventStream> {
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
        let Some(stream) = self.streams.get(id) else {
            return;
        };
        let this = self.clone();
        tokio::spawn(async move {
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
