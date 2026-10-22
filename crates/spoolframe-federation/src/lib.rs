//! Server-to-server federation (P8, ARCHITECTURE §26–§28): the
//! `spoolframe/federation/1` handler and the pairing state machine.
//!
//! AGPL, as the server is (§56); the messages are
//! [`spoolframe_protocol::federation`], which is permissive. The server
//! keeps its pairs and its strangers' limits; this crate reaches them only
//! through [`Peers`] and [`Strangers`], so the state machine is the same
//! whatever store is behind it.
//!
//! - [`FederationProtocol`] answers the ALPN: the server that is dialled.
//!   The ALPN is strict (§28): an endpoint that is not a live pair's
//!   reaches only `Hello` and redeeming a pairing invite; a known server
//!   id from a new key only `Hello`. Anything else resets the stream and
//!   counts against the endpoint as a wrong guess.
//! - [`Dialed`] is the other side: a server redeeming another's invite,
//!   or telling it that it unpaired.
//!
//! Experimental: the server registers the handler only while its
//! `experimental.federation` is on.

mod dial;
mod handler;
mod live;

pub use dial::{DialError, Dialed};
pub use handler::FederationProtocol;
pub use live::Live;

use iroh::EndpointId;
use iroh::endpoint::Connection;
use spoolframe_protocol::ServerId;
use spoolframe_protocol::federation::{PairingRefused, PeerHello, RedeemPairing, Standing};

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

    /// Hold `connection`, if there is room: a live pair's (`peer`) always,
    /// with the normal limits; a stranger's only while the server holds
    /// fewer than its cap of connections from no one it knows.
    fn admit(&self, connection: &Connection, peer: Option<&ServerId>) -> Option<Self::Held>;

    /// `endpoint`, not a live pair's, asked for what it may not: counted
    /// as a wrong guess at an invite's code.
    fn strayed(&self, endpoint: EndpointId);
}

#[cfg(test)]
mod tests;
