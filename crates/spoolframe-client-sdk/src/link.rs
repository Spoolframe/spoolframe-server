//! Keeping one server connected (P1-15).
//!
//! A link holds a server's current connection and its state. Once
//! connected, a supervisor task watches the connection; when it is lost,
//! or the server says it is shutting down, it dials the pinned key again
//! with backoff. Sessions live on the server and are bound to this
//! device's key, not to a connection, so they keep working afterwards.

use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use iroh::EndpointId;
use iroh::endpoint::Connection;
use spoolframe_protocol::codec::read_frame;
use spoolframe_protocol::streams::uni;
use spoolframe_protocol::{Event, ServerInfo};
use tokio::io::AsyncReadExt;
use tokio::sync::watch;

use crate::client::{Client, Inner};
use crate::connection::ServerConnection;
use crate::error::SdkError;

/// The first wait after a connection is lost.
const FIRST_RETRY: Duration = Duration::from_millis(250);
/// The longest wait between two dials.
const MAX_RETRY: Duration = Duration::from_secs(10);
/// How long a call waits for a reconnect before it fails.
pub(crate) const CALL_WAIT: Duration = Duration::from_secs(10);

/// Where a server's connection stands, for the app to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// Not connected, and not trying to be.
    Disconnected,
    /// The first dial.
    Connecting,
    Connected,
    /// The connection was lost; dialling again, `attempt` from 1.
    Reconnecting {
        attempt: u32,
    },
}

/// Changes of one server's [`ConnectionState`].
#[derive(Debug, Clone)]
pub struct StateWatch(watch::Receiver<ConnectionState>);

impl StateWatch {
    pub fn current(&self) -> ConnectionState {
        *self.0.borrow()
    }

    /// The next state; `None` once the client is gone.
    pub async fn changed(&mut self) -> Option<ConnectionState> {
        self.0.changed().await.ok()?;
        Some(*self.0.borrow_and_update())
    }
}

pub(crate) struct Link {
    id: EndpointId,
    state: watch::Sender<ConnectionState>,
    current: Mutex<Option<Arc<ServerConnection>>>,
    /// What the latest connection's Hello said, kept while it reconnects.
    info: Mutex<Option<ServerInfo>>,
    /// One first dial at a time.
    dialing: tokio::sync::Mutex<()>,
    supervisor: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Link {
    pub fn new(id: EndpointId) -> Arc<Self> {
        Arc::new(Self {
            id,
            state: watch::Sender::new(ConnectionState::Disconnected),
            current: Mutex::default(),
            info: Mutex::default(),
            dialing: tokio::sync::Mutex::default(),
            supervisor: Mutex::default(),
        })
    }

    pub fn watch(&self) -> StateWatch {
        StateWatch(self.state.subscribe())
    }

    /// The connection, if it is open.
    fn open(&self) -> Option<Arc<ServerConnection>> {
        self.current
            .lock()
            .expect("never poisoned")
            .clone()
            .filter(|c| c.conn.close_reason().is_none())
    }

    /// What the server said in the latest connection's Hello; `None`
    /// before the first.
    pub fn info(&self) -> Option<ServerInfo> {
        self.info.lock().expect("never poisoned").clone()
    }

    fn set(&self, conn: Option<Arc<ServerConnection>>, state: ConnectionState) {
        if let Some(conn) = &conn {
            *self.info.lock().expect("never poisoned") = Some(conn.info.clone());
        }
        *self.current.lock().expect("never poisoned") = conn;
        self.state.send_replace(state);
    }

    /// The open connection: as it is, after the supervisor's reconnect, or
    /// from a first dial made here.
    pub async fn connection(
        self: &Arc<Self>,
        client: &Client,
    ) -> Result<Arc<ServerConnection>, SdkError> {
        if let Some(conn) = self.open() {
            return Ok(conn);
        }
        if self.supervised() {
            return self.reconnected().await;
        }
        let _dialing = self.dialing.lock().await;
        if let Some(conn) = self.open() {
            return Ok(conn);
        }
        self.state.send_replace(ConnectionState::Connecting);
        match client.dial(self.id).await {
            Ok(conn) => {
                let conn = Arc::new(conn);
                self.set(Some(conn.clone()), ConnectionState::Connected);
                let task = tokio::spawn(supervise(
                    self.clone(),
                    Arc::downgrade(&client.inner),
                    conn.clone(),
                ));
                *self.supervisor.lock().expect("never poisoned") = Some(task);
                Ok(conn)
            }
            Err(e) => {
                self.state.send_replace(ConnectionState::Disconnected);
                Err(e)
            }
        }
    }

    fn supervised(&self) -> bool {
        self.supervisor
            .lock()
            .expect("never poisoned")
            .as_ref()
            .is_some_and(|t| !t.is_finished())
    }

    /// Wait up to [`CALL_WAIT`] for the supervisor to reconnect.
    async fn reconnected(&self) -> Result<Arc<ServerConnection>, SdkError> {
        let mut state = self.state.subscribe();
        let wait = state.wait_for(|s| *s == ConnectionState::Connected);
        match tokio::time::timeout(CALL_WAIT, wait).await {
            Ok(Ok(_)) => self
                .open()
                .ok_or_else(|| SdkError::Unreachable(format!("{}: lost again", self.id))),
            _ => Err(SdkError::Unreachable(format!(
                "{}: not back within {CALL_WAIT:?}",
                self.id
            ))),
        }
    }

    /// Stop: no more reconnects, and the connection closed.
    pub fn close(&self) {
        if let Some(task) = self.supervisor.lock().expect("never poisoned").take() {
            task.abort();
        }
        if let Some(conn) = self.current.lock().expect("never poisoned").take() {
            conn.conn.close(0u32.into(), b"closed");
        }
        self.state.send_replace(ConnectionState::Disconnected);
    }
}

/// Watch `conn`; when it is lost, dial again until connected, and watch
/// that one. Ends when the client is gone or forgets the server.
async fn supervise(link: Arc<Link>, client: Weak<Inner>, mut conn: Arc<ServerConnection>) {
    loop {
        until_lost(&conn.conn).await;
        tracing::info!(server = %link.id, "connection lost; reconnecting");
        let mut delay = FIRST_RETRY;
        let mut attempt = 1;
        conn = loop {
            link.set(None, ConnectionState::Reconnecting { attempt });
            let Some(inner) = client.upgrade() else {
                return;
            };
            match (Client { inner }).dial(link.id).await {
                Ok(conn) => break Arc::new(conn),
                Err(SdkError::UnknownServer(_)) => {
                    link.set(None, ConnectionState::Disconnected);
                    return;
                }
                Err(e) => {
                    tracing::debug!(server = %link.id, attempt, error = %e, "reconnect failed")
                }
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(MAX_RETRY);
            attempt += 1;
        };
        tracing::info!(server = %link.id, attempt, "reconnected");
        link.set(Some(conn.clone()), ConnectionState::Connected);
    }
}

/// Returns when `conn` closes or the server says it is shutting down: then
/// it is about to close, and dialling can start.
async fn until_lost(conn: &Connection) {
    tokio::select! {
        _ = conn.closed() => {}
        () = shutting_down(conn) => {}
    }
}

/// Returns on the server's `ShuttingDown` event. If the events stream
/// cannot be read, waits for the connection to close instead.
async fn shutting_down(conn: &Connection) {
    let events = async {
        let mut recv = conn.accept_uni().await.ok()?;
        if recv.read_u8().await.ok()? != uni::EVENTS {
            return None;
        }
        loop {
            match read_frame::<_, Event>(&mut recv).await.ok()? {
                Event::ShuttingDown => return Some(()),
                Event::Unknown => {}
            }
        }
    };
    if events.await.is_none() {
        std::future::pending::<()>().await;
    }
}
