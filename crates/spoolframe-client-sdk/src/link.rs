//! Keeping one server connected (P1-15).
//!
//! A link holds a server's current connection and its state. Once
//! connected, a supervisor task watches the connection; when it is lost,
//! or the server says it is shutting down, it dials the pinned key again
//! with backoff. Sessions live on the server and are bound to this
//! device's key, not to a connection, so they keep working afterwards.
//!
//! A connection can also die without closing: the app was suspended and
//! the server's side of it is gone, but QUIC here has not noticed yet. A
//! request that gets no answer, and then no `Pong`, drops it
//! ([`Link::drop_dead`]): it is closed, and the supervisor dials again.

use std::net::{IpAddr, SocketAddr};
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

/// The way packets go to a server now, for a player's diagnostics.
///
/// Iroh says whether its selected path is a relay or an IP address, not
/// how an IP path came about, so a hole punched through NAT and an open
/// port look the same. What the address is does tell the local network
/// apart: `Lan` is a private, link-local or loopback address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionPath {
    /// Straight to the server on the local network.
    Lan { addr: SocketAddr },
    /// Straight to a public address, across the internet: through an open
    /// port or a hole punched in NAT.
    Direct { addr: SocketAddr },
    /// Through a relay server. Relays are off in this build, so it is not
    /// seen yet.
    Relay,
    /// Not connected now, or no path selected yet.
    Unknown,
}

impl ConnectionPath {
    /// The path to `addr`, an IP path's remote address.
    pub(crate) fn of_ip(addr: SocketAddr) -> Self {
        if is_local(addr.ip()) {
            Self::Lan { addr }
        } else {
            Self::Direct { addr }
        }
    }
}

/// An address that is only reachable on this network.
fn is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_local(IpAddr::V4(v4)),
            None => v6.is_loopback() || v6.is_unique_local() || v6.is_unicast_link_local(),
        },
    }
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

    /// The path of the connection now; `Unknown` without one.
    pub fn path(&self) -> ConnectionPath {
        self.open()
            .map_or(ConnectionPath::Unknown, |conn| conn.path())
    }

    /// The connection, if it is open. Never dials.
    pub fn open(&self) -> Option<Arc<ServerConnection>> {
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

    /// Wait up to [`CALL_WAIT`] for the supervisor to reconnect. The
    /// state may still say `Connected` of a connection just closed, until
    /// the supervisor has seen it go: what counts is an open connection.
    async fn reconnected(&self) -> Result<Arc<ServerConnection>, SdkError> {
        let deadline = tokio::time::Instant::now() + CALL_WAIT;
        let mut state = self.state.subscribe();
        loop {
            if let Some(conn) = self.open() {
                return Ok(conn);
            }
            match tokio::time::timeout_at(deadline, state.changed()).await {
                Ok(Ok(())) => {}
                _ => {
                    return Err(SdkError::Unreachable(format!(
                        "{}: not back within {CALL_WAIT:?}",
                        self.id
                    )));
                }
            }
        }
    }

    /// `conn` has stopped answering: close it, so that no call uses it
    /// again and the supervisor dials anew. Nothing changes if it was
    /// already replaced.
    pub fn drop_dead(&self, conn: &Arc<ServerConnection>) {
        tracing::info!(server = %self.id, "the connection stopped answering; dropping it");
        let supervised = self.supervised();
        {
            let mut current = self.current.lock().expect("never poisoned");
            if current.as_ref().is_some_and(|c| Arc::ptr_eq(c, conn)) {
                *current = None;
                self.state.send_replace(if supervised {
                    ConnectionState::Reconnecting { attempt: 1 }
                } else {
                    ConnectionState::Disconnected
                });
            }
        }
        conn.conn.close(0u32.into(), b"no answer");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn path(addr: &str) -> ConnectionPath {
        ConnectionPath::of_ip(addr.parse().unwrap())
    }

    #[test]
    fn private_link_local_and_loopback_addresses_are_the_lan() {
        for addr in [
            "192.168.86.176:4433",
            "10.0.0.2:4433",
            "172.20.1.1:4433",
            "169.254.3.4:4433",
            "127.0.0.1:4433",
            "[fd12:3456::1]:4433",
            "[fe80::1]:4433",
            "[::1]:4433",
            "[::ffff:192.168.1.10]:4433",
        ] {
            assert!(matches!(path(addr), ConnectionPath::Lan { .. }), "{addr}");
        }
    }

    #[test]
    fn a_public_address_is_direct() {
        for addr in [
            "203.0.113.7:4433",
            "100.64.1.2:4433",
            "[2001:db8::1]:4433",
            "[::ffff:8.8.8.8]:4433",
        ] {
            assert!(
                matches!(path(addr), ConnectionPath::Direct { .. }),
                "{addr}"
            );
        }
    }
}
