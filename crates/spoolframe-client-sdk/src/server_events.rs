//! One watch of a server for an app's connection banner: its state, a
//! sign-out, and the app or the server needing an update, as one stream
//! ([`Client::server_events`]). An app merged [`Client::state`] and
//! [`Client::signed_out`] itself, and read an update from
//! [`StateWatch::stopped`].

use tokio::sync::watch;

use crate::describe::UpdateNeeded;
use crate::link::Status;
use crate::{Client, ConnectionState, SdkError};

/// What happened to a server's connection, as a banner shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum ServerEvent {
    /// Its connection state now.
    Connection { state: ConnectionState },
    /// The server signed this device out: its last session there ended,
    /// and every session kept for it is ended here too.
    SignedOut,
    /// The app and the server speak different protocol revisions: `who`
    /// updates. The link stopped dialling; it stays disconnected.
    UpdateRequired { who: UpdateNeeded },
}

/// One server's [`ServerEvent`]s, as they come.
#[derive(Debug, Clone)]
pub struct ServerEvents {
    status: watch::Receiver<Status>,
    /// The sign-outs counted when last told.
    sign_outs: u64,
    /// Told before anything the status says: a sign-out from before the
    /// watch began.
    pending: Option<ServerEvent>,
    /// The state event last told: one is told only when it differs, so a
    /// second stop while stopped is not another `Disconnected`.
    told: ServerEvent,
}

impl ServerEvents {
    /// A watch of `status`. A link already stopped dials no more, so what
    /// stopped it is told first, as a watch begun before it would have
    /// heard it: a sign-out (a stop for `SignInRequired`), or an update
    /// needed. The state itself is [`ServerEvents::current`].
    fn new(status: watch::Receiver<Status>) -> Self {
        let now = status.borrow().clone();
        let signed_out = now.stopped == Some(SdkError::SignInRequired);
        Self {
            sign_outs: now.sign_outs,
            pending: signed_out.then_some(ServerEvent::SignedOut),
            // An update needed is another event than this, so it is told.
            told: ServerEvent::Connection { state: now.state },
            status,
        }
    }

    /// The connection state when asked; what stopped it, if anything did,
    /// comes as the first event.
    pub fn current(&self) -> ConnectionState {
        self.status.borrow().state
    }

    /// The next event; `None` once the client is gone, after what it
    /// last sent is told. A sign-out since the last call comes at once,
    /// then the state it left if that is new; states missed while behind
    /// are skipped for the one now.
    pub async fn next(&mut self) -> Option<ServerEvent> {
        if let Some(event) = self.pending.take() {
            return Some(event);
        }
        loop {
            let now = self.status.borrow_and_update().clone();
            if now.sign_outs != self.sign_outs {
                self.sign_outs = now.sign_outs;
                return Some(ServerEvent::SignedOut);
            }
            let event = event_of(now.state, now.stopped);
            if event != self.told {
                self.told = event;
                return Some(event);
            }
            self.status.changed().await.ok()?;
        }
    }
}

/// The event a new connection `state` is, with why the link `stopped`
/// dialling: an update it needs is told as such, in place of the
/// disconnect.
fn event_of(state: ConnectionState, stopped: Option<SdkError>) -> ServerEvent {
    match stopped.as_ref().and_then(SdkError::who_updates) {
        Some(who) if state == ConnectionState::Disconnected => ServerEvent::UpdateRequired { who },
        _ => ServerEvent::Connection { state },
    }
}

impl Client {
    /// Server `endpoint_id`'s connection state, sign-outs and a needed
    /// update, as one stream.
    pub fn server_events(&self, endpoint_id: &str) -> Result<ServerEvents, SdkError> {
        Ok(ServerEvents::new(self.state(endpoint_id)?.into_status()))
    }
}

impl crate::Session {
    /// This session's server as [`Client::servers`] lists it: its name,
    /// whether it is saved and heard on the LAN. `None` once forgotten
    /// and out of earshot.
    pub async fn server_entry(&self) -> Result<Option<crate::ServerEntry>, SdkError> {
        self.client.server(&self.server()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_disconnect_for_an_update_says_who_updates() {
        let newer = || {
            Some(SdkError::UpdateRequired {
                server_protocol: Some(spoolframe_protocol::PROTOCOL_VERSION + 1),
            })
        };
        assert_eq!(
            [
                event_of(ConnectionState::Disconnected, newer()),
                event_of(ConnectionState::Disconnected, Some(SdkError::ServerFull)),
                event_of(ConnectionState::Connected, None),
            ],
            [
                ServerEvent::UpdateRequired {
                    who: UpdateNeeded::App
                },
                ServerEvent::Connection {
                    state: ConnectionState::Disconnected
                },
                ServerEvent::Connection {
                    state: ConnectionState::Connected
                },
            ]
        );
    }

    /// A sign-out the link sent just before it went is still told, then
    /// the state it left, then the end.
    #[tokio::test]
    async fn a_sign_out_before_the_link_went_is_told() {
        let link = crate::link::Link::new(iroh::SecretKey::generate().public());
        link.show(ConnectionState::Connected);
        let mut events = ServerEvents::new(link.watch().into_status());
        link.sign_out();
        drop(link);
        assert_eq!(
            [
                events.next().await,
                events.next().await,
                events.next().await
            ],
            [
                Some(ServerEvent::SignedOut),
                Some(ServerEvent::Connection {
                    state: ConnectionState::Disconnected
                }),
                None
            ]
        );
    }

    /// A watch begun after the link stopped for an update tells it at
    /// once: the link dials no more, so nothing else would come.
    #[tokio::test]
    async fn a_watch_after_an_update_stop_tells_it_first() {
        let link = crate::link::Link::new(iroh::SecretKey::generate().public());
        link.stop(SdkError::UpdateRequired {
            server_protocol: Some(spoolframe_protocol::PROTOCOL_VERSION + 1),
        });
        let mut events = ServerEvents::new(link.watch().into_status());
        drop(link);
        assert_eq!(
            [events.next().await, events.next().await],
            [
                Some(ServerEvent::UpdateRequired {
                    who: UpdateNeeded::App
                }),
                None
            ]
        );
    }

    /// A watch begun after a sign-out tells it; the state it left is
    /// `current()`, not a change.
    #[tokio::test]
    async fn a_watch_after_a_sign_out_tells_it_first() {
        let link = crate::link::Link::new(iroh::SecretKey::generate().public());
        link.sign_out();
        let mut events = ServerEvents::new(link.watch().into_status());
        drop(link);
        assert_eq!(
            [events.next().await, events.next().await],
            [Some(ServerEvent::SignedOut), None]
        );
    }

    /// A stop while already stopped says nothing new: not another
    /// Disconnected, and after a sign-out only the sign-out.
    #[tokio::test]
    async fn a_stop_of_a_stopped_link_tells_only_what_is_new() {
        let link = crate::link::Link::new(iroh::SecretKey::generate().public());
        let mut events = ServerEvents::new(link.watch().into_status());
        let state = |state| Some(ServerEvent::Connection { state });
        link.show(ConnectionState::Connected);
        assert_eq!(events.next().await, state(ConnectionState::Connected));
        link.stop(SdkError::ServerFull);
        assert_eq!(events.next().await, state(ConnectionState::Disconnected));
        link.stop(SdkError::UnknownServer("gone".into()));
        link.sign_out();
        drop(link);
        assert_eq!(
            [events.next().await, events.next().await],
            [Some(ServerEvent::SignedOut), None]
        );
    }

    /// A state is told once, however often the link repeats it.
    #[tokio::test]
    async fn a_state_is_told_once() {
        let link = crate::link::Link::new(iroh::SecretKey::generate().public());
        let mut events = ServerEvents::new(link.watch().into_status());
        link.show(ConnectionState::Connecting);
        link.show(ConnectionState::Connecting);
        drop(link);
        assert_eq!(
            [events.next().await, events.next().await],
            [
                Some(ServerEvent::Connection {
                    state: ConnectionState::Connecting
                }),
                None
            ]
        );
    }
}
