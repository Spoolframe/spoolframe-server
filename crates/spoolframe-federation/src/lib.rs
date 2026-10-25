//! Server-to-server federation (P8, ARCHITECTURE §26–§28): the
//! `spoolframe/federation/1` handler, the pairing state machine, and the
//! paging of replication (P8-02).
//!
//! AGPL, as the server is (§56); the messages are
//! [`spoolframe_protocol::federation`], which is permissive. The server
//! keeps its pairs, its strangers' limits and what it shares; this crate
//! reaches them only through [`Peers`], [`Strangers`] and [`Shares`], so
//! the state machine is the same whatever store is behind it.
//!
//! - [`FederationProtocol`] answers the ALPN: the server that is dialled.
//!   The ALPN is strict (§28): an endpoint that is not a live pair's
//!   reaches only `Hello` and redeeming a pairing invite; a known server
//!   id from a new key only `Hello`. Anything else resets the stream and
//!   counts against the endpoint as a wrong guess.
//!   A live pair also reaches replication: what is shared with it, a
//!   library's changes, and its artwork, answered by [`Shares`]; and its
//!   assertions of its users, answered with a token (P8-03).
//! - [`Dialed`] is the other side: a server redeeming another's invite,
//!   telling it that it unpaired, or replicating what it shares.
//! - [`PlayClient`] is a server's own client of another's
//!   `spoolframe/client/1`, for playing its item in proxy mode (P8-04):
//!   federation/1 carries no media.
//!
//! Experimental: the server registers the handler only while its
//! `experimental.federation` is on.

mod dial;
mod handler;
mod live;
mod play;

pub use dial::{DialError, Dialed, Pages, SYNC_WAIT};
pub use handler::{
    FederationProtocol, MAX_REQUESTS, MAX_STRANGER_REQUESTS, MAX_STREAMS, PEER_CONNECTIONS,
};
pub use live::Live;
pub use play::PlayClient;

use std::sync::Arc;
use std::time::Duration;

use iroh::endpoint::Connection;
use iroh::{Endpoint, EndpointAddr, EndpointId};
use spoolframe_protocol::ServerId;
use spoolframe_protocol::federation::{
    AssertUser, AssertionRefused, PairingRefused, PeerHello, RedeemPairing, RemoteItem,
    SharedLibrary, Standing, SubtitleDownloadAsk, SubtitleRemovalAsk, SubtitleSearchAsk,
    SyncLibrary, SyncMode, TokenGranted,
};
use spoolframe_types::{SubtitleDownloaded, SubtitleSearch};

/// How long dialling another server may take: one attempt, over every
/// hint given.
const DIAL_WAIT: Duration = Duration::from_secs(15);

/// Dial `addr` from `endpoint` on `alpn`, once, within [`DIAL_WAIT`]: the
/// federation protocol's [`Dialed`] and the client protocol's
/// [`PlayClient`] alike.
async fn dial(
    endpoint: &Endpoint,
    addr: EndpointAddr,
    alpn: &[u8],
) -> Result<Connection, DialError> {
    tokio::time::timeout(DIAL_WAIT, endpoint.connect(addr, alpn))
        .await
        .map_err(|_| DialError::Unreachable("no answer in time".into()))?
        .map_err(|e| DialError::Unreachable(e.to_string()))
}

/// The pairs this server keeps: what the handler asks of its store.
#[async_trait::async_trait]
pub trait Peers: Send + Sync + 'static {
    /// The server id `endpoint` is a live pair's under, if it is: such a
    /// connection skips the strangers' cap. Unknown when the store cannot
    /// say, which treats it as a stranger. Asked once per connection; its
    /// `hello` is given the answer.
    async fn known(&self, endpoint: EndpointId) -> Option<ServerId>;

    /// `endpoint`, the live pair under `known` if [`Peers::known`] said
    /// so, says it is server `server_id`: what it is to this server. A
    /// live pair's endpoint under another server id is
    /// [`PairingRefused::ServerIdChanged`], every time. A live pair's
    /// server id from a new endpoint is [`Standing::KeyHeld`], and the key
    /// is held for an admin to confirm.
    async fn hello(
        &self,
        endpoint: EndpointId,
        known: Option<&ServerId>,
        server_id: &ServerId,
    ) -> Result<Standing, PairingRefused>;

    /// Pair stranger `endpoint`, which said `hello` as `server_id`, by
    /// redeeming a `federation` invite: the code is guessed at as an
    /// account invite's is, on the strangers' budget. `Ok` once both are
    /// pinned and the use taken.
    async fn redeem(
        &self,
        endpoint: EndpointId,
        server_id: &ServerId,
        hello: &PeerHello,
        redeem: &RedeemPairing,
    ) -> Result<(), PairingRefused>;

    /// Live pair `endpoint` said it unpaired: revoke this side's row.
    async fn unpaired(&self, endpoint: EndpointId);
}

/// Who may hold the server's capacity: the strangers' cap and guess count
/// anonymous client connections draw on, which federation shares (P8-01).
pub trait Strangers: Send + Sync + 'static {
    /// What holds a connection's place while it is open; dropped when it
    /// closes.
    type Held: Send + 'static;

    /// Hold `connection`, if there is room: a live pair's (`peer`) with the
    /// normal limits, within the server's caps on pairs' connections; a
    /// stranger's only while the server holds fewer than its cap of
    /// connections from no one it knows. `None` closes it `BUSY`.
    fn admit(&self, connection: &Connection, peer: Option<&ServerId>) -> Option<Self::Held>;

    /// `endpoint`, not a live pair's, asked for what it may not: counted
    /// as a wrong guess at an invite's code.
    fn strayed(&self, endpoint: EndpointId);
}

/// What a server shares with its live pairs (P8-02): what the handler
/// asks of it for a pair's replication. Each is asked only on a connection
/// that said hello as a live pair, `peer` being the server id that pair is
/// pinned under and `endpoint` the key the request came from, and checks
/// the pair's grants itself. The key is the request's proof (X8-01): a
/// pair whose key was confirmed anew while the connection was open is
/// found by its server id still, and the request from the old key must
/// be refused, as a connection that outlived its standing.
#[async_trait::async_trait]
pub trait Shares: Send + Sync + 'static {
    /// The libraries shared with `peer` now.
    async fn libraries(
        &self,
        peer: &ServerId,
        endpoint: EndpointId,
    ) -> Result<Vec<SharedLibrary>, Unshared>;

    /// Library `sync.library`'s changes since `sync.since`, or the whole
    /// of it when that cursor is not known: the handler pages it out.
    async fn sync(
        &self,
        peer: &ServerId,
        endpoint: EndpointId,
        sync: &SyncLibrary,
    ) -> Result<SyncAnswer, Unshared>;

    /// Image `id` of an item in a library shared with `peer`: its media
    /// type and bytes.
    async fn artwork(
        &self,
        peer: &ServerId,
        endpoint: EndpointId,
        id: &str,
    ) -> Result<(String, Vec<u8>), Unshared>;

    /// `peer` vouches for one of its users, who wants an item of this
    /// server's (P8-03): a token for it, or why not. The assertion
    /// arrived on `peer`'s connection from `endpoint`, which is its proof:
    /// one from a key that is no longer the pair's mints nothing.
    async fn assert_user(
        &self,
        peer: &ServerId,
        endpoint: EndpointId,
        assertion: &AssertUser,
    ) -> Result<TokenGranted, AssertionRefused>;

    /// One of `peer`'s users searches subtitles for an item of this
    /// server's (P8-07): its results, or the client protocol's refusal.
    /// Not answered unless the server says otherwise.
    async fn search_subtitles(
        &self,
        _peer: &ServerId,
        _endpoint: EndpointId,
        _ask: &SubtitleSearchAsk,
    ) -> Result<SubtitleSearch, spoolframe_protocol::Error> {
        Err(not_answered())
    }

    /// One of `peer`'s users downloads a subtitle result (P8-07), counted
    /// under `peer`.
    async fn download_subtitle(
        &self,
        _peer: &ServerId,
        _endpoint: EndpointId,
        _ask: &SubtitleDownloadAsk,
    ) -> Result<SubtitleDownloaded, spoolframe_protocol::Error> {
        Err(not_answered())
    }

    /// One of `peer`'s users removes a downloaded subtitle (P8-07).
    async fn remove_subtitle_download(
        &self,
        _peer: &ServerId,
        _endpoint: EndpointId,
        _ask: &SubtitleRemovalAsk,
    ) -> Result<(), spoolframe_protocol::Error> {
        Err(not_answered())
    }
}

/// What a server that answers no subtitle ask says.
fn not_answered() -> spoolframe_protocol::Error {
    spoolframe_protocol::Error::Unsupported {
        request: "subtitles for a paired server".into(),
    }
}

/// A whole answer to a [`SyncLibrary`], before it is paged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncAnswer {
    pub mode: SyncMode,
    /// The cursor the asker names next time.
    pub cursor: String,
    /// Added or changed items; every item, for a snapshot. Shared: a
    /// snapshot is the server's walk of the library, one copy for every
    /// request of it in hand.
    pub items: Arc<Vec<RemoteItem>>,
    /// Ids of items gone since the cursor.
    pub removed: Vec<String>,
}

/// Why a pair's replication request was not answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Unshared {
    /// The library, or the item an image is of, is not shared with the
    /// pair (any more).
    #[error("not shared with that server")]
    NotShared,
    /// Shared, but not there now.
    #[error("not found")]
    NotFound,
    /// Cannot be answered now: the catalog, or the store, failed.
    #[error("cannot answer now")]
    Unavailable,
}

#[cfg(test)]
mod tests;
