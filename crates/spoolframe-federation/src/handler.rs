//! The dialled server's side of `spoolframe/federation/1`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use iroh::EndpointId;
use iroh::endpoint::{Connection, RecvStream, SendStream, VarInt};
use iroh::protocol::{AcceptError, ProtocolHandler};
use spoolframe_protocol::codec::{read_frame_within, write_frame, write_reply};
use spoolframe_protocol::federation::{
    MAX_ARTWORK_BYTES, MAX_PAGE_ITEMS, MAX_PAGE_REMOVED, MAX_REQUEST, PROTOCOL_VERSION, Paired,
    PairingRefused, PeerHello, PeerInfo, PeerReply, PeerRequest, Standing, SyncLibrary, SyncPage,
    reset,
};
use spoolframe_protocol::{ServerId, close};

use crate::{Live, Peers, Shares, Strangers, SyncAnswer, Unshared};

/// The most a page's items take, encoded: half a frame, so a page with
/// its envelope always fits in one.
const PAGE_BUDGET: usize = spoolframe_protocol::codec::MAX_FRAME as usize / 2;

/// How long a stream's request may take to arrive once it is opened.
const REQUEST_WAIT: Duration = Duration::from_secs(10);

/// How long a reply that ends the connection may take to be read before
/// the connection closes anyway.
const LAST_REPLY_WAIT: Duration = Duration::from_secs(2);

/// The most streams one connection has answered at once (H2), unless the
/// server says otherwise ([`FederationProtocol::with_max_streams`]): a
/// stream past them is reset `BUSY`, not counted against the endpoint. A
/// sync and a few images at a time is what a pair asks.
pub const MAX_STREAMS: usize = 16;

/// What the server calls itself now: a live setting.
pub type Name = Arc<dyn Fn() -> String + Send + Sync>;

/// The `spoolframe/federation/1` handler, over the server's pairs, its
/// strangers' limits, and what it shares.
pub struct FederationProtocol<P, S, H> {
    peers: Arc<P>,
    strangers: Arc<S>,
    shares: Arc<H>,
    server_id: ServerId,
    name: Name,
    live: Live,
    /// The most streams a connection is answered on at once, read as it
    /// opens: a live setting of the server's (P8-04).
    max_streams: Arc<AtomicUsize>,
}

impl<P, S, H> Clone for FederationProtocol<P, S, H> {
    fn clone(&self) -> Self {
        Self {
            peers: self.peers.clone(),
            strangers: self.strangers.clone(),
            shares: self.shares.clone(),
            server_id: self.server_id.clone(),
            name: self.name.clone(),
            live: self.live.clone(),
            max_streams: self.max_streams.clone(),
        }
    }
}

impl<P, S, H> std::fmt::Debug for FederationProtocol<P, S, H> {
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

impl<P: Peers, S: Strangers, H: Shares> FederationProtocol<P, S, H> {
    /// The handler for server `server_id`, called `name`, over `peers`,
    /// `strangers` and `shares`; its open connections are kept in `live`.
    pub fn new(
        peers: Arc<P>,
        strangers: Arc<S>,
        shares: Arc<H>,
        server_id: ServerId,
        name: Name,
        live: Live,
    ) -> Self {
        Self {
            peers,
            strangers,
            shares,
            server_id,
            name,
            live,
            max_streams: Arc::new(AtomicUsize::new(MAX_STREAMS)),
        }
    }

    /// The most streams a connection is answered on at once, as
    /// `max_streams` holds it when the connection opens (P8-04).
    #[must_use]
    pub fn with_max_streams(mut self, max_streams: Arc<AtomicUsize>) -> Self {
        self.max_streams = max_streams;
        self
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
            (
                PeerRequest::Libraries,
                Some(Said {
                    server_id,
                    standing: Standing::Paired,
                    ..
                }),
            ) => match self.shares.libraries(&server_id).await {
                Ok(libraries) => (PeerReply::Libraries { libraries }, After::Next),
                Err(unshared) => (refusal(unshared), After::Next),
            },
            (
                PeerRequest::Sync(sync),
                Some(Said {
                    server_id,
                    standing: Standing::Paired,
                    ..
                }),
            ) => {
                self.sync(&server_id, &sync, send).await;
                return After::Next;
            }
            (
                PeerRequest::Artwork(fetch),
                Some(Said {
                    server_id,
                    standing: Standing::Paired,
                    ..
                }),
            ) => {
                let reply = match self.shares.artwork(&server_id, &fetch.id).await {
                    Ok((_, bytes)) if bytes.len() > MAX_ARTWORK_BYTES => PeerReply::NotFound,
                    Ok((content_type, bytes)) => PeerReply::Artwork {
                        content_type,
                        bytes,
                    },
                    Err(unshared) => refusal(unshared),
                };
                if write_reply(&mut send, &reply).await.is_ok() {
                    let _ = send.finish();
                }
                return After::Next;
            }
            (
                PeerRequest::AssertUser(assertion),
                Some(Said {
                    server_id,
                    standing: Standing::Paired,
                    ..
                }),
            ) => {
                let assertion = assertion.within_bounds();
                let reply = match self.shares.assert_user(&server_id, &assertion).await {
                    Ok(granted) => PeerReply::Token(granted),
                    Err(reason) => {
                        tracing::info!(
                            server_id = %server_id,
                            %reason,
                            "a paired server's assertion was refused"
                        );
                        PeerReply::AssertionRefused { reason }
                    }
                };
                (reply, After::Next)
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

    /// Answer pair `peer`'s `sync` on `send`: its pages, or why not.
    async fn sync(&self, peer: &ServerId, sync: &SyncLibrary, mut send: SendStream) {
        let pages = match self.shares.sync(peer, sync).await {
            Ok(answer) => {
                tracing::debug!(
                    server_id = %peer,
                    library = sync.library,
                    mode = ?answer.mode,
                    items = answer.items.len(),
                    removed = answer.removed.len(),
                    "answering a paired server's sync"
                );
                pages(answer).into_iter().map(PeerReply::Page).collect()
            }
            Err(unshared) => vec![refusal(unshared)],
        };
        for page in &pages {
            if let Err(e) = write_frame(&mut send, page).await {
                tracing::debug!(server_id = %peer, error = %e, "a sync's page was not sent");
                return;
            }
        }
        let _ = send.finish();
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

/// What a pair is told for `unshared`.
fn refusal(unshared: Unshared) -> PeerReply {
    match unshared {
        Unshared::NotShared => PeerReply::NotShared,
        Unshared::NotFound => PeerReply::NotFound,
        Unshared::Unavailable => PeerReply::Unavailable,
    }
}

/// `answer` as pages, in order: its items, at most [`MAX_PAGE_ITEMS`] and
/// [`PAGE_BUDGET`] bytes of them a page, then the ids of those gone, at
/// most [`MAX_PAGE_REMOVED`] a page. Never none: an answer with nothing
/// in it is one empty page. Only the last says no more follow.
pub(crate) fn pages(answer: SyncAnswer) -> Vec<SyncPage> {
    let page = |items, removed| SyncPage {
        mode: answer.mode,
        cursor: answer.cursor.clone(),
        items,
        removed,
        more: true,
    };
    let mut pages = Vec::new();
    let mut items = Vec::new();
    let mut size = 0;
    for item in answer.items {
        let len = spoolframe_protocol::codec::encode(&item).map_or(PAGE_BUDGET, |b| b.len());
        if !items.is_empty() && (items.len() == MAX_PAGE_ITEMS || size + len > PAGE_BUDGET) {
            pages.push(page(std::mem::take(&mut items), Vec::new()));
            size = 0;
        }
        size += len;
        items.push(item);
    }
    let mut removed = answer.removed.into_iter().peekable();
    loop {
        let gone: Vec<String> = removed.by_ref().take(MAX_PAGE_REMOVED).collect();
        let last = removed.peek().is_none();
        pages.push(page(std::mem::take(&mut items), gone));
        if last {
            break;
        }
    }
    if let Some(last) = pages.last_mut() {
        last.more = false;
    }
    pages
}

impl<P: Peers, S: Strangers, H: Shares> ProtocolHandler for FederationProtocol<P, S, H> {
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
        let streams = Arc::new(tokio::sync::Semaphore::new(
            self.max_streams.load(Ordering::Relaxed).max(1),
        ));
        loop {
            tokio::select! {
                accepted = connection.accept_bi() => match accepted {
                    Ok((mut send, mut recv)) => {
                        let Ok(permit) = streams.clone().try_acquire_owned() else {
                            let busy = VarInt::from_u32(reset::BUSY);
                            let _ = send.reset(busy);
                            let _ = recv.stop(busy);
                            tracing::debug!(
                                peer = %endpoint.fmt_short(),
                                "a server's stream past the limit is reset"
                            );
                            continue;
                        };
                        let this = self.clone();
                        let state = state.clone();
                        let closing = closing.clone();
                        tokio::spawn(async move {
                            let _permit = permit;
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
