//! Who a QUIC connection may hold the server's capacity for (PX-10).
//!
//! Every connection proves its `EndpointId`, and endpoint ids are free, so
//! a connection from a device with no live session (a *stranger*) gets
//! small limits: the endpoint's transport config. A device that is signed
//! in gets the normal ones, raised on its connection at accept or at the
//! sign-in that makes it known. Strangers also share a cap on how many of
//! their connections are held at once; a known device is always let in.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use iroh::EndpointId;
use iroh::endpoint::{Connection, QuicTransportConfig, VarInt};
use tokio::sync::watch;

/// Bidirectional streams a stranger may have open at once: enough for
/// `Hello`, a sign-in and its long poll side by side, and no more.
pub const STRANGER_BI_STREAMS: u32 = 4;

/// Bidirectional streams a signed-in device may have open at once: the
/// QUIC transport's default, which the server used for every peer before.
pub const KNOWN_BI_STREAMS: u32 = 100;

/// The longest request frame a stranger may send. What a stranger can do
/// is `Hello` and sign in, and the largest of those (a passkey assertion,
/// or a `Hello` with many capabilities) is a few KiB; a 1 MiB frame would
/// make each of its streams cost the server 1 MiB.
pub const STRANGER_MAX_FRAME: u32 = 64 << 10;

/// How much a peer may send on one stream before the server reads it:
/// endpoint-wide, as QUIC sets it at the handshake. A whole request frame
/// (1 MiB) and its header fit in one flight. Media goes the other way, so
/// this does not limit playback.
pub const STREAM_WINDOW: u32 = 2 << 20;

/// How much a stranger may send across all its streams before the server
/// reads it: its streams' largest frames, with room for their headers.
pub const STRANGER_WINDOW: u32 = 2 * STRANGER_BI_STREAMS * STRANGER_MAX_FRAME;

/// As [`STRANGER_WINDOW`], for a signed-in device: several full request
/// frames at once. Before PX-10 it had no limit.
pub const KNOWN_WINDOW: u32 = 16 << 20;

/// How long a connection may send nothing at all before it is dropped.
/// Peers send a keep-alive every 5 s (iroh's default), so only a peer that
/// is gone, or does not keep alive, reaches it. 30 s, iroh's own limit
/// for a path through a relay (`RELAY_PATH_MAX_IDLE_TIMEOUT`): the
/// laggiest link must not get the shortest leash, and a phone app
/// suspended for a few seconds more than 15 keeps its connection (review
/// 2026-10-11 L43). The lower of this and the device's limit applies.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a signed-out device's connection may finish the requests it
/// is answering (its `Logout`'s reply among them) before it is closed.
pub const SIGN_OUT_DRAIN: Duration = Duration::from_secs(2);

/// How often every signed-in device's connections are checked for a live
/// session, besides each time sessions end: a session can also expire.
pub const RECHECK: Duration = Duration::from_secs(60);

/// What an operator sets in `network {}`, and the caps on paired servers'
/// connections, which no setting moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Admission {
    /// The most stranger connections held at once; one more is closed with
    /// [`spoolframe_protocol::close::BUSY`].
    pub max_strangers: usize,
    /// The most `spoolframe/federation/1` connections one live pair's key
    /// holds at once (F8-07); one more is closed `BUSY`. A pair keeps one,
    /// and a second while it dials again or refreshes.
    pub max_pair_connections: usize,
    /// The most connections of every live pair's together; one more is
    /// closed `BUSY`. Apart from the strangers' cap, so pairs and devices
    /// never take each other's places.
    pub max_paired_connections: usize,
    /// The most client connections one endpoint holds at once that it is
    /// let in on by a paired server's session alone (C8-04): a device in a
    /// remote play, or the paired server playing through its own. One more
    /// is closed `BUSY`. A device keeps one, and another as it reconnects.
    pub max_remote_connections: usize,
    /// The most such connections of every endpoint's together; one more is
    /// closed `BUSY`. Apart from the strangers' and the pairs' caps.
    pub max_all_remote_connections: usize,
    /// The most such connections of one pair's sessions, every endpoint
    /// together; one more is closed `BUSY`. Half of every pair's, so one
    /// pair with many device keys leaves the others room.
    pub max_pair_remote_connections: usize,
    /// The requests answered at once allowed for each such connection: a
    /// pair's sessions get this many times their connection cap, every
    /// pair's times the cap on all. No setting moves it; a test sets it
    /// small.
    pub remote_requests_each: usize,
    /// How long a stream's kind byte and first frame may take to arrive
    /// before the stream is reset.
    pub first_frame: Duration,
    /// How long a resource stream (media, artwork) may wait for its
    /// reader to take any more before it is reset (PX-09 finding 14).
    pub stalled_read: Duration,
    /// How long a signed-out device's connection may drain:
    /// [`SIGN_OUT_DRAIN`]. No setting moves it; a test lengthens it to
    /// watch the drain without racing it.
    pub sign_out_drain: Duration,
}

impl Admission {
    pub const DEFAULT_MAX_STRANGERS: usize = 64;
    pub const DEFAULT_MAX_PAIR_CONNECTIONS: usize = 4;
    pub const DEFAULT_MAX_PAIRED_CONNECTIONS: usize = 64;
    pub const DEFAULT_MAX_REMOTE_CONNECTIONS: usize = 4;
    /// Far above the plays of every pair's users at once that go straight
    /// to this server.
    pub const DEFAULT_MAX_ALL_REMOTE_CONNECTIONS: usize = 256;
    pub const DEFAULT_MAX_PAIR_REMOTE_CONNECTIONS: usize =
        Self::DEFAULT_MAX_ALL_REMOTE_CONNECTIONS / 2;
    pub const DEFAULT_REMOTE_REQUESTS_EACH: usize = 4;
    pub const DEFAULT_FIRST_FRAME: Duration = Duration::from_secs(10);
    /// Long enough for a player paused with a read under way: it takes
    /// what is sent as it plays on.
    pub const DEFAULT_STALLED_READ: Duration = Duration::from_secs(120);
}

impl Default for Admission {
    fn default() -> Self {
        Self {
            max_strangers: Self::DEFAULT_MAX_STRANGERS,
            max_pair_connections: Self::DEFAULT_MAX_PAIR_CONNECTIONS,
            max_paired_connections: Self::DEFAULT_MAX_PAIRED_CONNECTIONS,
            max_remote_connections: Self::DEFAULT_MAX_REMOTE_CONNECTIONS,
            max_all_remote_connections: Self::DEFAULT_MAX_ALL_REMOTE_CONNECTIONS,
            max_pair_remote_connections: Self::DEFAULT_MAX_PAIR_REMOTE_CONNECTIONS,
            remote_requests_each: Self::DEFAULT_REMOTE_REQUESTS_EACH,
            first_frame: Self::DEFAULT_FIRST_FRAME,
            stalled_read: Self::DEFAULT_STALLED_READ,
            sign_out_drain: SIGN_OUT_DRAIN,
        }
    }
}

/// The endpoint's transport config: a stranger's limits, which a known
/// device's connection raises (`Peer`). The server never accepts a
/// unidirectional stream (events go the other way), so none may be opened.
/// Each path's congestion controller comes from `congestion`
/// (`network.congestion_control`).
pub fn transport_config(
    congestion: std::sync::Arc<dyn iroh::endpoint::ControllerFactory + Send + Sync + 'static>,
) -> QuicTransportConfig {
    QuicTransportConfig::builder()
        .congestion_controller_factory(congestion)
        .max_concurrent_uni_streams(VarInt::from_u32(0))
        .max_concurrent_bidi_streams(VarInt::from_u32(STRANGER_BI_STREAMS))
        .stream_receive_window(VarInt::from_u32(STREAM_WINDOW))
        .receive_window(VarInt::from_u32(STRANGER_WINDOW))
        .max_idle_timeout(Some(
            IDLE_TIMEOUT
                .try_into()
                .expect("the idle timeout fits a QUIC varint"),
        ))
        .build()
}

/// One client connection, as admission sees it.
#[derive(Debug)]
pub(crate) struct Peer {
    pub endpoint: EndpointId,
    connection: Connection,
    known: AtomicBool,
    /// Its device has a local user's live session (P7-06): only such a
    /// device's sign-ins skip the strangers' budget. A portable guest's
    /// session raises its limits, not this.
    local: AtomicBool,
    /// Signed in at some point while open: a sign-in on its device, or a
    /// session used on it. Unlike `known`, not set for a device that only
    /// has a session: what the log names at INFO.
    signed_in_here: AtomicBool,
    /// Who signed in on it, or whose session it used: what the log names
    /// when it closes.
    users: parking_lot::Mutex<std::collections::BTreeSet<String>>,
    /// Signed out: closing once its streams are answered.
    closing: AtomicBool,
    /// Past the remote sessions' caps: closing `BUSY` once its streams are
    /// answered, and taking no new one.
    refused: AtomicBool,
    /// How many of its streams are being answered.
    streams: watch::Sender<usize>,
    /// Its events stream, once open: finished, and acknowledged, before a
    /// sign-out closes it.
    events: std::sync::OnceLock<crate::events::EventStream>,
    /// A device's, or a server's on `spoolframe/federation/1`.
    kind: PeerKind,
    /// Let in by paired servers' sessions alone (C8-04), those pairs'
    /// rows, sorted; empty for any other: counted under
    /// [`Admission::max_remote_connections`] and each of its pairs' shares
    /// until its device signs in here.
    remote: parking_lot::Mutex<Vec<String>>,
}

/// Whose connection a [`Peer`] is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PeerKind {
    /// A device's, on the client protocol.
    Device,
    /// A server's on `spoolframe/federation/1` (P8-01), with the server
    /// id it is a live pair's under, if it is one. It shares the
    /// strangers' cap, and has no session to sign out of.
    Federation(Option<spoolframe_protocol::ServerId>),
}

/// One stream of a [`Peer`] being answered; it is counted until dropped.
pub(crate) struct Answering(Arc<Peer>);

impl Drop for Answering {
    fn drop(&mut self) {
        self.0.streams.send_modify(|n| *n -= 1);
    }
}

impl Peer {
    pub(crate) fn new(connection: &Connection) -> Self {
        Self {
            endpoint: connection.remote_id(),
            connection: connection.clone(),
            known: AtomicBool::new(false),
            local: AtomicBool::new(false),
            signed_in_here: AtomicBool::new(false),
            users: parking_lot::Mutex::default(),
            closing: AtomicBool::new(false),
            refused: AtomicBool::new(false),
            streams: watch::Sender::new(0),
            events: std::sync::OnceLock::new(),
            kind: PeerKind::Device,
            remote: parking_lot::Mutex::default(),
        }
    }

    /// A device's connection let in by pairs `pairs`' sessions alone
    /// (C8-04): one in a remote play, or that server's own.
    pub(crate) fn remote(connection: &Connection, pairs: Vec<String>) -> Self {
        let peer = Self::new(connection);
        *peer.remote.lock() = pairs;
        peer
    }

    /// The pairs whose sessions it is held under, by the remote sessions'
    /// caps; empty for any other.
    pub(crate) fn remote_pairs(&self) -> Vec<String> {
        self.remote.lock().clone()
    }

    /// Held by pairs `pairs`' sessions from now on: those its endpoint
    /// holds now (Z8-01).
    pub(crate) fn set_remote_pairs(&self, pairs: Vec<String>) {
        *self.remote.lock() = pairs;
    }

    /// Its device signed in here: a session of this server's own, no
    /// longer only a paired server's.
    pub(crate) fn note_own_session(&self) {
        self.remote.lock().clear();
    }

    /// A server's connection on `spoolframe/federation/1` (P8-01), from a
    /// live pair's endpoint under `peer`, or a stranger's.
    pub(crate) fn federation(
        connection: &Connection,
        peer: Option<spoolframe_protocol::ServerId>,
    ) -> Self {
        Self {
            kind: PeerKind::Federation(peer),
            ..Self::new(connection)
        }
    }

    /// Whose connection it is.
    pub(crate) fn kind(&self) -> &PeerKind {
        &self.kind
    }

    /// Whether its connection goes through a relay now (P6-03); `None`
    /// before iroh has selected a path.
    pub(crate) fn relayed(&self) -> Option<bool> {
        self.connection
            .paths()
            .iter()
            .find(|path| path.is_selected())
            .map(|path| matches!(path.remote_addr(), iroh::TransportAddr::Relay(_)))
    }

    /// Whether the device is signed in: its connection has the normal
    /// limits.
    pub fn known(&self) -> bool {
        self.known.load(Ordering::Acquire)
    }

    /// Whether the device is signed in and not signed out since: what may
    /// be pushed the server's addresses (PX-12).
    pub fn signed_in(&self) -> bool {
        self.known() && !self.closing.load(Ordering::Acquire)
    }

    /// Whether a local user is signed in on the device, and it has not
    /// signed out since: its sign-ins skip the strangers' budget.
    pub fn signed_in_locally(&self) -> bool {
        self.signed_in() && self.local.load(Ordering::Acquire)
    }

    /// A local user's session is live on the device.
    pub(crate) fn note_local(&self) {
        self.local.store(true, Ordering::Release);
    }

    /// The connection was signed in: its device signed in while it was
    /// open, or a session was used on it; by `user`, when it is known.
    pub(crate) fn note_signed_in(&self, user: Option<&str>) {
        self.signed_in_here.store(true, Ordering::Release);
        if let Some(user) = user {
            let mut users = self.users.lock();
            // Every request names its user: allocate only for a new one.
            if !users.contains(user) {
                users.insert(user.to_owned());
            }
        }
    }

    /// Who was signed in on the connection, comma-separated: for the log.
    pub fn users(&self) -> String {
        let users = self.users.lock();
        users
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Whether the connection was signed in at any point
    /// ([`Peer::note_signed_in`]), signed out since or not.
    pub fn was_signed_in(&self) -> bool {
        self.signed_in_here.load(Ordering::Acquire)
    }

    /// The longest request frame its streams may carry.
    pub fn max_frame(&self) -> u32 {
        if self.known() {
            spoolframe_protocol::codec::MAX_FRAME
        } else {
            STRANGER_MAX_FRAME
        }
    }

    /// Give the connection the normal limits, once.
    pub(crate) fn raise(&self) {
        if !self.known.swap(true, Ordering::AcqRel) {
            self.connection
                .set_max_concurrent_bi_streams(VarInt::from_u32(KNOWN_BI_STREAMS));
            self.connection
                .set_receive_window(VarInt::from_u32(KNOWN_WINDOW));
        }
    }

    /// Count a stream as being answered until the guard is dropped.
    pub(crate) fn answering(self: &Arc<Self>) -> Answering {
        self.streams.send_modify(|n| *n += 1);
        Answering(self.clone())
    }

    /// Its events stream is `events`.
    pub(crate) fn events_opened(&self, events: crate::events::EventStream) {
        let _ = self.events.set(events);
    }

    /// The device has no live session left: let the streams being answered
    /// finish, for at most `drain` ([`Admission::sign_out_drain`]), then
    /// its events stream, for at most `drain` more, then close the
    /// connection with [`spoolframe_protocol::close::SIGNED_OUT`]. The
    /// device may connect again, as a stranger.
    pub(crate) fn sign_out(self: &Arc<Self>, drain: Duration) {
        if self.closing.swap(true, Ordering::AcqRel) {
            return;
        }
        let peer = self.clone();
        tokio::spawn(async move {
            let mut streams = peer.streams.subscribe();
            let _ = tokio::time::timeout(drain, streams.wait_for(|n| *n == 0)).await;
            // An event written just before (a `ProfileRemoved` for the
            // change that signed it out, held as a stream being answered
            // until written) would be lost with what the device had not
            // acknowledged: the stream is never finished otherwise, so
            // nothing else says when it arrived. Finished after the writes
            // waiting for it, it is acknowledged once they are.
            if let Some(events) = peer.events.get() {
                let delivered = async {
                    let mut events = events.lock().await;
                    if events.finish().is_ok() {
                        let _ = events.stopped().await;
                    }
                };
                let _ = tokio::time::timeout(drain, delivered).await;
            }
            tracing::info!(endpoint = %peer.endpoint.fmt_short(), "closing a signed-out device's connection");
            peer.connection.close(
                VarInt::from_u32(spoolframe_protocol::close::SIGNED_OUT),
                b"signed out",
            );
        });
    }

    /// Its device redeemed a paired server's token on it, a stranger's
    /// connection, and the remote sessions' caps have no room for it
    /// (Z8-01): it takes no new stream, and once those being answered are
    /// (the redemption's reply among them), for at most `drain`, it is
    /// closed [`spoolframe_protocol::close::BUSY`], as a connection past
    /// those caps is. Held on, it would hold the session outside them.
    pub(crate) fn refuse(self: &Arc<Self>, drain: Duration) {
        if self.refused.swap(true, Ordering::AcqRel) {
            return;
        }
        let peer = self.clone();
        tokio::spawn(async move {
            let mut streams = peer.streams.subscribe();
            let _ = tokio::time::timeout(drain, streams.wait_for(|n| *n == 0)).await;
            tracing::info!(
                endpoint = %peer.endpoint.fmt_short(),
                "closing a connection a paired server's token was redeemed on: too many in its sessions"
            );
            peer.connection.close(
                VarInt::from_u32(spoolframe_protocol::close::BUSY),
                b"too many connections in a paired server's sessions; try again later",
            );
        });
    }

    /// Whether it is refused past the remote sessions' caps
    /// ([`Peer::refuse`]).
    pub(crate) fn refused(&self) -> bool {
        self.refused.load(Ordering::Acquire)
    }
}
