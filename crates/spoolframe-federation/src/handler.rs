//! The dialled server's side of `spoolframe/federation/1`.

use std::sync::Arc;
use std::time::Duration;

use iroh::EndpointId;
use iroh::endpoint::{Connection, RecvStream, SendStream, VarInt};
use iroh::protocol::{AcceptError, ProtocolHandler};
use spoolframe_protocol::codec::{read_frame_within, write_frame};
use spoolframe_protocol::federation::{
    MAX_REQUEST, PROTOCOL_VERSION, Paired, PairingRefused, PeerHello, PeerInfo, PeerReply,
    PeerRequest, Standing, reset,
};
use spoolframe_protocol::{ServerId, close};

use crate::{Live, Peers, Strangers};

/// How long a stream's request may take to arrive once it is opened.
const REQUEST_WAIT: Duration = Duration::from_secs(10);

/// How long a reply that ends the connection may take to be read before
/// the connection closes anyway.
const LAST_REPLY_WAIT: Duration = Duration::from_secs(2);

/// What the server calls itself now: a live setting.
pub type Name = Arc<dyn Fn() -> String + Send + Sync>;

/// The `spoolframe/federation/1` handler, over the server's pairs and its
/// strangers' limits.
pub struct FederationProtocol<P, S> {
    peers: Arc<P>,
    strangers: Arc<S>,
    server_id: ServerId,
    name: Name,
    live: Live,
}

impl<P, S> Clone for FederationProtocol<P, S> {
    fn clone(&self) -> Self {
        Self {
            peers: self.peers.clone(),
            strangers: self.strangers.clone(),
            server_id: self.server_id.clone(),
            name: self.name.clone(),
            live: self.live.clone(),
        }
    }
}

impl<P, S> std::fmt::Debug for FederationProtocol<P, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FederationProtocol")
            .field("server_id", &self.server_id)
            .finish_non_exhaustive()
    }
}

/// What a connection said of itself.
#[derive(Debug, Clone)]
struct Said {
    server_id: ServerId,
    hello: PeerHello,
    standing: Standing,
}

/// One connection's state: nothing until its `Hello` is answered.
#[derive(Debug, Default)]
struct State {
    /// The server id its endpoint is a live pair's under, as
    /// [`Peers::known`] said when it connected.
    known: Option<ServerId>,
    said: parking_lot::Mutex<Option<Said>>,
    /// A `Hello` is being answered: a second one is not allowed.
    greeting: std::sync::atomic::AtomicBool,
}

impl State {
    fn said(&self) -> Option<Said> {
        self.said.lock().clone()
    }

    fn paired(&self) -> bool {
        matches!(
            self.said(),
            Some(Said {
                standing: Standing::Paired,
                ..
            })
        )
    }
}

/// How a stream ended, for the connection.
enum After {
    /// Go on taking streams.
    Next,
    /// Close the connection with this code and reason, once the reply is
    /// read.
    Close(u32, &'static str),
}

impl<P: Peers, S: Strangers> FederationProtocol<P, S> {
    /// The handler for server `server_id`, called `name`, over `peers` and
    /// `strangers`; its open connections are kept in `live`.
    pub fn new(
        peers: Arc<P>,
        strangers: Arc<S>,
        server_id: ServerId,
        name: Name,
        live: Live,
    ) -> Self {
        Self {
            peers,
            strangers,
            server_id,
            name,
            live,
        }
    }

    fn me(&self) -> Paired {
        Paired {
            server_id: self.server_id.to_string(),
            name: (self.name)(),
        }
    }

    /// One stream: one request and its reply.
    async fn stream(
        &self,
        endpoint: EndpointId,
        state: &State,
        mut send: SendStream,
        mut recv: RecvStream,
    ) -> After {
        let request =
            tokio::time::timeout(REQUEST_WAIT, read_frame_within(&mut recv, MAX_REQUEST)).await;
        let request: PeerRequest = match request {
            Ok(Ok(request)) => request,
            _ => {
                self.refuse(endpoint, state, send, recv, reset::BAD_REQUEST);
                return After::Next;
            }
        };
        let said = state.said();
        let (reply, after) = match (request, said) {
            (PeerRequest::Hello(hello), None)
                if !state
                    .greeting
                    .swap(true, std::sync::atomic::Ordering::AcqRel) =>
            {
                let hello = hello.within_bounds();
                let Ok(server_id) = ServerId::parse(&hello.server_id) else {
                    self.refuse(endpoint, state, send, recv, reset::BAD_REQUEST);
                    return After::Next;
                };
                // A server never pairs with itself, nor with one that took
                // its id: refused before the store is asked anything.
                let standing = if server_id == self.server_id {
                    Err(PairingRefused::SameServerId)
                } else {
                    self.peers
                        .hello(endpoint, state.known.as_ref(), &server_id)
                        .await
                };
                match standing {
                    Ok(standing) => {
                        tracing::debug!(
                            peer = %endpoint.fmt_short(),
                            server_id = %server_id,
                            ?standing,
                            "a server said hello"
                        );
                        *state.said.lock() = Some(Said {
                            server_id,
                            hello,
                            standing,
                        });
                        let me = self.me();
                        let info = PeerInfo {
                            protocol_version: PROTOCOL_VERSION,
                            server_id: me.server_id,
                            name: me.name,
                            standing,
                        };
                        (PeerReply::Info(info), After::Next)
                    }
                    Err(reason) => {
                        tracing::warn!(
                            peer = %endpoint.fmt_short(),
                            server_id = %server_id,
                            %reason,
                            "refusing a server"
                        );
                        let after = if reason == PairingRefused::ServerIdChanged {
                            After::Close(close::SERVER_ID_CHANGED, "paired under another server id")
                        } else {
                            After::Next
                        };
                        (PeerReply::Refused { reason }, after)
                    }
                }
            }
            (
                PeerRequest::RedeemPairing(redeem),
                Some(Said {
                    server_id,
                    hello,
                    standing: Standing::Stranger,
                }),
            ) => match self
                .peers
                .redeem(endpoint, &server_id, &hello, &redeem)
                .await
            {
                Ok(()) => {
                    tracing::info!(
                        peer = %endpoint.fmt_short(),
                        server_id = %server_id,
                        name = %hello.name,
                        "a server paired with this one"
                    );
                    *state.said.lock() = Some(Said {
                        server_id,
                        hello,
                        standing: Standing::Paired,
                    });
                    (PeerReply::Paired(self.me()), After::Next)
                }
                Err(reason) => {
                    tracing::info!(
                        peer = %endpoint.fmt_short(),
                        %reason,
                        "a server's pairing was refused"
                    );
                    (PeerReply::Refused { reason }, After::Next)
                }
            },
            (
                PeerRequest::RedeemPairing(_),
                Some(Said {
                    standing: Standing::Paired,
                    ..
                }),
            ) => (
                PeerReply::Refused {
                    reason: PairingRefused::AlreadyPaired,
                },
                After::Next,
            ),
            (
                PeerRequest::Unpaired(_),
                Some(Said {
                    server_id,
                    standing: Standing::Paired,
                    ..
                }),
            ) => {
                self.peers.unpaired(endpoint).await;
                tracing::info!(
                    peer = %endpoint.fmt_short(),
                    server_id = %server_id,
                    "a paired server unpaired"
                );
                (PeerReply::Done, After::Close(close::UNPAIRED, "unpaired"))
            }
            // Before Hello, a second Hello, a held key's or a stranger's
            // anything else, and what this build does not know.
            _ => {
                self.refuse(endpoint, state, send, recv, reset::NOT_ALLOWED);
                return After::Next;
            }
        };
        if write_frame(&mut send, &reply).await.is_ok()
            && send.finish().is_ok()
            && let After::Close(..) = after
        {
            // The reply read before the close, or the peer never sees why.
            let _ = tokio::time::timeout(LAST_REPLY_WAIT, send.stopped()).await;
        }
        after
    }

    /// Reset a stream with `code`; a request not from a live pair is
    /// counted against its endpoint.
    fn refuse(
        &self,
        endpoint: EndpointId,
        state: &State,
        mut send: SendStream,
        mut recv: RecvStream,
        code: u32,
    ) {
        let code_varint = VarInt::from_u32(code);
        let _ = send.reset(code_varint);
        let _ = recv.stop(code_varint);
        if !state.paired() {
            tracing::debug!(peer = %endpoint.fmt_short(), code, "a stranger's stream refused");
            self.strangers.strayed(endpoint);
        }
    }
}

impl<P: Peers, S: Strangers> ProtocolHandler for FederationProtocol<P, S> {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let endpoint = connection.remote_id();
        let known = self.peers.known(endpoint).await;
        let Some(held) = self.strangers.admit(&connection, known.as_ref()) else {
            tracing::info!(
                peer = %endpoint.fmt_short(),
                "refusing a server: too many connections from endpoints that are not known"
            );
            connection.close(
                VarInt::from_u32(close::BUSY),
                b"too many connections from unknown endpoints; try again later",
            );
            let _ = tokio::time::timeout(LAST_REPLY_WAIT, connection.closed()).await;
            return Ok(());
        };
        tracing::debug!(peer = %endpoint.fmt_short(), known = known.is_some(), "server connected");
        self.live.opened(&connection);
        let state = Arc::new(State {
            known,
            ..State::default()
        });
        let (closing, mut close_asked) = tokio::sync::mpsc::channel::<(u32, &'static str)>(1);
        loop {
            tokio::select! {
                accepted = connection.accept_bi() => match accepted {
                    Ok((send, recv)) => {
                        let this = self.clone();
                        let state = state.clone();
                        let closing = closing.clone();
                        tokio::spawn(async move {
                            if let After::Close(code, reason) =
                                this.stream(endpoint, &state, send, recv).await
                            {
                                let _ = closing.try_send((code, reason));
                            }
                        });
                    }
                    Err(_) => break,
                },
                Some((code, reason)) = close_asked.recv() => {
                    connection.close(VarInt::from_u32(code), reason.as_bytes());
                    break;
                }
            }
        }
        self.live.closed(&connection);
        drop(held);
        tracing::debug!(peer = %endpoint.fmt_short(), "server disconnected");
        Ok(())
    }
}
