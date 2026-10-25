//! The dialled server's side of `spoolframe/federation/1`.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use iroh::EndpointId;
use iroh::endpoint::{Connection, RecvStream, SendStream, VarInt};
use iroh::protocol::{AcceptError, ProtocolHandler};
use spoolframe_protocol::codec::{CodecError, read_frame_within, write_frame, write_reply};
use spoolframe_protocol::federation::{
    AssertionRefused, MAX_ARTWORK_BYTES, MAX_PAGE_ITEMS, MAX_PAGE_REMOVED, MAX_REQUEST,
    PROTOCOL_VERSION, Paired, PairingRefused, PeerHello, PeerInfo, PeerReply, PeerRequest,
    RemoteItem, Standing, SyncLibrary, SyncMode, SyncPage, reset,
};
use spoolframe_protocol::{ServerId, close};
use tokio::time::Instant;

use crate::dial::{ARTWORK_WAIT, FIRST_PAGE_WAIT, PAGE_WAIT, REPLY_WAIT, SUBTITLE_WAIT, SYNC_WAIT};
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

/// The connections one pair may hold at once, as the server caps them
/// (its `Admission::max_pair_connections`): a pair's requests at once are
/// this many connections' streams (X8-02), so raising the streams setting
/// raises them too. One past them is reset `BUSY`, as a stream past a
/// connection's is. A request's work ends with its connection, so this
/// bounds what is in hand, not what a connection closed and opened again
/// could leave behind.
pub const PEER_CONNECTIONS: usize = 4;

/// The most requests answered at once from every paired endpoint together
/// (X8-02): what the server's catalog and store are asked at one time by
/// its pairs, however many.
pub const MAX_REQUESTS: usize = 256;

/// The most requests answered at once from every endpoint that is no
/// pair's, together: a stranger's `hello` and redemption, and a held key's
/// `hello`, each a store lookup. Apart from the pairs', so strangers'
/// streams, held open for a request that never comes, take no pair's
/// place.
pub const MAX_STRANGER_REQUESTS: usize = 16;

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
    /// The requests being answered now, every connection together.
    requests: Arc<Requests>,
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
            requests: self.requests.clone(),
        }
    }
}

/// The requests being answered now (X8-02): a pair's by the endpoint each
/// came from and in all, and those of endpoints that are no pair's apart.
/// Each holds its place for the whole of its work, and gives it back as
/// its task ends, aborted with its connection or not.
#[derive(Debug)]
struct Requests {
    /// From one paired endpoint; `None` takes the caller's, derived from
    /// the streams setting and [`PEER_CONNECTIONS`] as each is taken.
    per_peer: Option<usize>,
    total: usize,
    strangers: usize,
    counts: parking_lot::Mutex<Counts>,
}

/// How many are in hand: the pairs' in all and by endpoint, and the
/// strangers'.
#[derive(Debug, Default)]
struct Counts {
    all: usize,
    by_peer: HashMap<EndpointId, usize>,
    strangers: usize,
}

impl Requests {
    fn new(per_peer: Option<usize>, total: usize, strangers: usize) -> Self {
        Self {
            per_peer,
            total,
            strangers,
            counts: parking_lot::Mutex::default(),
        }
    }

    /// A place for a request from `endpoint`, a live pair's if `paired`,
    /// if there is one: held while it is kept.
    fn take(
        self: &Arc<Self>,
        endpoint: EndpointId,
        paired: bool,
        per_peer_default: usize,
    ) -> Option<Place> {
        let mut counts = self.counts.lock();
        if !paired {
            if counts.strangers >= self.strangers {
                return None;
            }
            counts.strangers += 1;
            return Some(Place {
                requests: self.clone(),
                of: None,
            });
        }
        let per_peer = self.per_peer.unwrap_or(per_peer_default);
        let mine = counts.by_peer.get(&endpoint).copied().unwrap_or(0);
        if counts.all >= self.total || mine >= per_peer {
            return None;
        }
        counts.all += 1;
        counts.by_peer.insert(endpoint, mine + 1);
        Some(Place {
            requests: self.clone(),
            of: Some(endpoint),
        })
    }
}

/// One request's place among [`Requests`], given back when dropped: a
/// pair's endpoint's, or a stranger's (`None`).
#[derive(Debug)]
struct Place {
    requests: Arc<Requests>,
    of: Option<EndpointId>,
}

impl Drop for Place {
    fn drop(&mut self) {
        let mut counts = self.requests.counts.lock();
        let Some(endpoint) = self.of else {
            counts.strangers = counts.strangers.saturating_sub(1);
            return;
        };
        counts.all = counts.all.saturating_sub(1);
        if let Some(mine) = counts.by_peer.get_mut(&endpoint) {
            *mine = mine.saturating_sub(1);
            if *mine == 0 {
                counts.by_peer.remove(&endpoint);
            }
        }
    }
}

/// How much sooner than the asker's wait for a reply its work is given up
/// on: the asker is told `unavailable`, rather than left to its wait.
pub(crate) const ANSWER_MARGIN: Duration = Duration::from_secs(5);

/// How long the asker of `request` waits for its reply, as the dialler
/// does: what the dialled side has, less [`ANSWER_MARGIN`], for the
/// request's work and the sending of its reply together. A sync's is its
/// first page's; the pages after have [`PAGE_WAIT`] each.
fn reply_wait(request: &PeerRequest) -> Duration {
    match request {
        PeerRequest::Sync(_) => FIRST_PAGE_WAIT,
        PeerRequest::Artwork(_) => ARTWORK_WAIT,
        PeerRequest::SearchSubtitles(_)
        | PeerRequest::DownloadSubtitle(_)
        | PeerRequest::RemoveSubtitleDownload(_) => SUBTITLE_WAIT,
        _ => REPLY_WAIT,
    }
}

/// `work` by `by`, the asker's wait for its reply less [`ANSWER_MARGIN`]
/// from when the request was read; else `late`. A request whose work
/// takes longer than the asker waits is given up on (X8-02), so one held
/// on a lock, or a walk of the catalog, does not hold its place for ever.
async fn within<T>(
    peer: &ServerId,
    what: &'static str,
    by: Instant,
    work: impl Future<Output = T>,
    late: T,
) -> T {
    match tokio::time::timeout_at(by, work).await {
        Ok(answered) => answered,
        Err(_) => {
            tracing::warn!(
                server_id = %peer,
                what,
                "a paired server's request took longer than its wait: given up on"
            );
            late
        }
    }
}

/// When a reply is to be written by, its work done by `by`: `by`, or the
/// margin from now for a reply made late (`unavailable` after the work was
/// given up on), which the asker still waits that long for.
fn reply_by(by: Instant) -> Instant {
    by.max(Instant::now() + ANSWER_MARGIN)
}

/// Whether a reply's write, `written` by its deadline or not, went: one
/// the asker took none of in time is given up on, its stream reset
/// `STALLED`, so a peer that stops reading holds neither the reply nor the
/// request's place past its own wait for it.
fn sent(
    peer: &str,
    what: &'static str,
    written: Result<Result<(), CodecError>, tokio::time::error::Elapsed>,
    send: &mut SendStream,
) -> bool {
    match written {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            tracing::debug!(peer, what, error = %e, "a reply was not sent");
            false
        }
        Err(_) => {
            tracing::warn!(
                peer,
                what,
                "a peer took none of a reply in time: given up on"
            );
            let _ = send.reset(VarInt::from_u32(reset::STALLED));
            false
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
        let max_streams = Arc::new(AtomicUsize::new(MAX_STREAMS));
        Self {
            peers,
            strangers,
            shares,
            server_id,
            name,
            live,
            requests: Arc::new(Requests::new(None, MAX_REQUESTS, MAX_STRANGER_REQUESTS)),
            max_streams,
        }
    }

    /// The most streams a connection is answered on at once, as
    /// `max_streams` holds it when the connection opens (P8-04); a pair's
    /// requests at once follow it.
    #[must_use]
    pub fn with_max_streams(mut self, max_streams: Arc<AtomicUsize>) -> Self {
        self.max_streams = max_streams;
        self
    }

    /// The most requests answered at once from one paired endpoint (its
    /// connections' streams unless given), from every pair together and
    /// from the strangers, instead of [`PEER_CONNECTIONS`]' streams,
    /// [`MAX_REQUESTS`] and [`MAX_STRANGER_REQUESTS`]: for tests, which
    /// set them small.
    #[must_use]
    pub fn with_request_budget(
        mut self,
        per_peer: Option<usize>,
        total: usize,
        strangers: usize,
    ) -> Self {
        self.requests = Arc::new(Requests::new(per_peer, total, strangers));
        self
    }

    /// How many requests are being answered now, every endpoint's: for
    /// tests.
    #[cfg(test)]
    pub(crate) fn in_hand(&self) -> usize {
        let counts = self.requests.counts.lock();
        counts.all + counts.strangers
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
        // One deadline for the request's work and its reply's sending: the
        // asker's wait, less the margin, from here.
        let by = Instant::now() + reply_wait(&request).saturating_sub(ANSWER_MARGIN);
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
                // On a task of its own, not the connection's: the pair is
                // revoked and cleaned up after to the end though the
                // connection closes meanwhile, which aborts this one.
                let peers = self.peers.clone();
                let _ = tokio::spawn(async move { peers.unpaired(endpoint).await }).await;
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
            ) => {
                let libraries = within(
                    &server_id,
                    "libraries",
                    by,
                    self.shares.libraries(&server_id, endpoint),
                    Err(Unshared::Unavailable),
                )
                .await;
                match libraries {
                    Ok(libraries) => (PeerReply::Libraries { libraries }, After::Next),
                    Err(unshared) => (refusal(unshared), After::Next),
                }
            }
            (
                PeerRequest::Sync(sync),
                Some(Said {
                    server_id,
                    standing: Standing::Paired,
                    ..
                }),
            ) => {
                self.sync(&server_id, endpoint, &sync, by, send).await;
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
                let artwork = within(
                    &server_id,
                    "artwork",
                    by,
                    self.shares.artwork(&server_id, endpoint, &fetch.id),
                    Err(Unshared::Unavailable),
                )
                .await;
                let reply = match artwork {
                    Ok((_, bytes)) if bytes.len() > MAX_ARTWORK_BYTES => PeerReply::NotFound,
                    Ok((content_type, bytes)) => PeerReply::Artwork {
                        content_type,
                        bytes,
                    },
                    Err(unshared) => refusal(unshared),
                };
                let written =
                    tokio::time::timeout_at(reply_by(by), write_reply(&mut send, &reply)).await;
                if sent(server_id.as_str(), "artwork", written, &mut send) {
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
                let granted = within(
                    &server_id,
                    "assertion",
                    by,
                    self.shares.assert_user(&server_id, endpoint, &assertion),
                    Err(AssertionRefused::Unavailable),
                )
                .await;
                let reply = match granted {
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
            // P8-07: subtitles for a pair's users, answered as the client
            // protocol answers them.
            (
                request @ (PeerRequest::SearchSubtitles(_)
                | PeerRequest::DownloadSubtitle(_)
                | PeerRequest::RemoveSubtitleDownload(_)),
                Some(Said {
                    server_id,
                    standing: Standing::Paired,
                    ..
                }),
            ) => {
                let failed = |error| PeerReply::Failed { error };
                let late = || spoolframe_protocol::Error::Unavailable {
                    message: "the server cannot answer now".into(),
                };
                let reply = match request {
                    PeerRequest::SearchSubtitles(ask) => within(
                        &server_id,
                        "subtitle search",
                        by,
                        self.shares.search_subtitles(&server_id, endpoint, &ask),
                        Err(late()),
                    )
                    .await
                    .map_or_else(failed, |search| PeerReply::SubtitleResults { search }),
                    PeerRequest::DownloadSubtitle(ask) => within(
                        &server_id,
                        "subtitle download",
                        by,
                        self.shares.download_subtitle(&server_id, endpoint, &ask),
                        Err(late()),
                    )
                    .await
                    .map_or_else(failed, |downloaded| PeerReply::SubtitleDownloaded {
                        downloaded,
                    }),
                    PeerRequest::RemoveSubtitleDownload(ask) => within(
                        &server_id,
                        "subtitle removal",
                        by,
                        self.shares
                            .remove_subtitle_download(&server_id, endpoint, &ask),
                        Err(late()),
                    )
                    .await
                    .map_or_else(failed, |()| PeerReply::Done),
                    _ => unreachable!("matched above"),
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
        let peer = endpoint.fmt_short().to_string();
        let written = tokio::time::timeout_at(reply_by(by), write_frame(&mut send, &reply)).await;
        if sent(&peer, "reply", written, &mut send)
            && send.finish().is_ok()
            && let After::Close(..) = after
        {
            // The reply read before the close, or the peer never sees why.
            let _ = tokio::time::timeout(LAST_REPLY_WAIT, send.stopped()).await;
        }
        after
    }

    /// Answer pair `peer`'s `sync`, from `endpoint`, on `send`: its pages,
    /// or why not. The walk and the first page by `by`; each page after
    /// within the asker's wait for it, and the whole answer within the
    /// asker's budget for a sync. A page is made as it is sent and dropped
    /// after.
    async fn sync(
        &self,
        peer: &ServerId,
        endpoint: EndpointId,
        sync: &SyncLibrary,
        by: Instant,
        mut send: SendStream,
    ) {
        // Within what the asker waits for the first page: the whole
        // library is walked before it.
        let answer = within(
            peer,
            "sync",
            by,
            self.shares.sync(peer, endpoint, sync),
            Err(Unshared::Unavailable),
        )
        .await;
        let whole = Instant::now() + SYNC_WAIT;
        let mut by = by;
        let pages = match answer {
            Ok(answer) => {
                tracing::debug!(
                    server_id = %peer,
                    library = sync.library,
                    mode = ?answer.mode,
                    items = answer.items.len(),
                    removed = answer.removed.len(),
                    "answering a paired server's sync"
                );
                pages(answer)
            }
            Err(unshared) => {
                let written = tokio::time::timeout_at(
                    reply_by(by),
                    write_frame(&mut send, &refusal(unshared)),
                )
                .await;
                if sent(peer.as_str(), "sync", written, &mut send) {
                    let _ = send.finish();
                }
                return;
            }
        };
        for page in pages {
            let reply = PeerReply::Page(page);
            let written =
                tokio::time::timeout_at(by.min(whole), write_frame(&mut send, &reply)).await;
            if !sent(peer.as_str(), "sync", written, &mut send) {
                return;
            }
            drop(reply);
            by = Instant::now() + PAGE_WAIT.saturating_sub(ANSWER_MARGIN);
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
/// in it is one empty page. Only the last says no more follow. Each page
/// is made as it is asked for, so a page is held only until it is sent.
pub(crate) fn pages(answer: SyncAnswer) -> Paged {
    Paged {
        mode: answer.mode,
        cursor: answer.cursor,
        items: answer.items,
        at: 0,
        removed: answer.removed.into_iter().peekable(),
        done: false,
    }
}

/// The pages of an answer, made in turn: see [`pages`].
pub(crate) struct Paged {
    mode: SyncMode,
    cursor: String,
    items: Arc<Vec<RemoteItem>>,
    /// The first item not paged yet.
    at: usize,
    removed: std::iter::Peekable<std::vec::IntoIter<String>>,
    done: bool,
}

impl Iterator for Paged {
    type Item = SyncPage;

    fn next(&mut self) -> Option<SyncPage> {
        if self.done {
            return None;
        }
        let mut items = Vec::new();
        let mut size = 0;
        while let Some(item) = self.items.get(self.at) {
            let len = spoolframe_protocol::codec::encoded_len(item).unwrap_or(PAGE_BUDGET);
            if !items.is_empty() && (items.len() == MAX_PAGE_ITEMS || size + len > PAGE_BUDGET) {
                break;
            }
            size += len;
            items.push(item.clone());
            self.at += 1;
        }
        // The ids gone follow the last page of items, on it.
        let more_items = self.at < self.items.len();
        let removed: Vec<String> = if more_items {
            Vec::new()
        } else {
            self.removed.by_ref().take(MAX_PAGE_REMOVED).collect()
        };
        let more = more_items || self.removed.peek().is_some();
        self.done = !more;
        Some(SyncPage {
            mode: self.mode,
            cursor: self.cursor.clone(),
            items,
            removed,
            more,
        })
    }
}

impl<P: Peers, S: Strangers, H: Shares> ProtocolHandler for FederationProtocol<P, S, H> {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let endpoint = connection.remote_id();
        // Kept among the live connections before its pair is looked up
        // (X8-01): a key confirmed after the lookup finds this connection
        // to close; one confirmed before is what the lookup reads.
        self.live.opened(&connection);
        let known = self.peers.known(endpoint).await;
        let Some(held) = self.strangers.admit(&connection, known.as_ref()) else {
            // A stranger past the strangers' cap, or a pair past its own.
            tracing::info!(
                peer = %endpoint.fmt_short(),
                known = known.is_some(),
                "refusing a server: too many connections"
            );
            self.live.closed(&connection);
            connection.close(
                VarInt::from_u32(close::BUSY),
                b"too many connections; try again later",
            );
            let _ = tokio::time::timeout(LAST_REPLY_WAIT, connection.closed()).await;
            return Ok(());
        };
        tracing::debug!(peer = %endpoint.fmt_short(), known = known.is_some(), "server connected");
        let state = Arc::new(State {
            known,
            ..State::default()
        });
        let (closing, mut close_asked) = tokio::sync::mpsc::channel::<(u32, &'static str)>(1);
        let streams = Arc::new(tokio::sync::Semaphore::new(
            self.max_streams.load(Ordering::Relaxed).max(1),
        ));
        // The connection's requests are its own (X8-02): aborted with it,
        // so one waiting on a lock or walking the catalog outlives it not.
        let mut requests = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = connection.accept_bi() => match accepted {
                    Ok((send, recv)) => {
                        let reset_busy = |mut send: SendStream, mut recv: RecvStream, what| {
                            let busy = VarInt::from_u32(reset::BUSY);
                            let _ = send.reset(busy);
                            let _ = recv.stop(busy);
                            tracing::debug!(peer = %endpoint.fmt_short(), what, "a server's stream is reset");
                        };
                        let Ok(permit) = streams.clone().try_acquire_owned() else {
                            reset_busy(send, recv, "past the connection's streams");
                            continue;
                        };
                        // A pair's from its first stream: its standing is known
                        // as it connects, or once it says hello or pairs.
                        let paired = state.known.is_some() || state.paired();
                        let per_peer = self.max_streams.load(Ordering::Relaxed) * PEER_CONNECTIONS;
                        let Some(place) = self.requests.take(endpoint, paired, per_peer) else {
                            reset_busy(send, recv, "past the requests answered at once");
                            continue;
                        };
                        let this = self.clone();
                        let state = state.clone();
                        let closing = closing.clone();
                        requests.spawn(async move {
                            let (_permit, _place) = (permit, place);
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
                // Those that ended, so the set holds only what is in hand.
                Some(_) = requests.join_next(), if !requests.is_empty() => {}
            }
        }
        // Aborted and joined before the connection's place is given back:
        // nothing of it is at work, or holds a request's place, once
        // another may take the place.
        requests.abort_all();
        while requests.join_next().await.is_some() {}
        self.live.closed(&connection);
        drop(held);
        tracing::debug!(peer = %endpoint.fmt_short(), "server disconnected");
        Ok(())
    }
}
