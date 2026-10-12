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

/// What an operator sets in `network {}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Admission {
    /// The most stranger connections held at once; one more is closed with
    /// [`spoolframe_protocol::close::BUSY`].
    pub max_strangers: usize,
    /// How long a stream's kind byte and first frame may take to arrive
    /// before the stream is reset.
    pub first_frame: Duration,
    /// How long a resource stream (media, artwork) may wait for its
    /// reader to take any more before it is reset (PX-09 finding 14).
    pub stalled_read: Duration,
}

impl Admission {
    pub const DEFAULT_MAX_STRANGERS: usize = 64;
    pub const DEFAULT_FIRST_FRAME: Duration = Duration::from_secs(10);
    /// Long enough for a player paused with a read under way: it takes
    /// what is sent as it plays on.
    pub const DEFAULT_STALLED_READ: Duration = Duration::from_secs(120);
}

impl Default for Admission {
    fn default() -> Self {
        Self {
            max_strangers: Self::DEFAULT_MAX_STRANGERS,
            first_frame: Self::DEFAULT_FIRST_FRAME,
            stalled_read: Self::DEFAULT_STALLED_READ,
        }
    }
}

/// The endpoint's transport config: a stranger's limits, which a known
/// device's connection raises ([`Peer`]). The server never accepts a
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
    /// How many of its streams are being answered.
    streams: watch::Sender<usize>,
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
            streams: watch::Sender::new(0),
        }
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

    /// The device has no live session left: let the streams being answered
    /// finish, for at most [`SIGN_OUT_DRAIN`], then close the connection
    /// with [`spoolframe_protocol::close::SIGNED_OUT`]. The device may
    /// connect again, as a stranger.
    pub(crate) fn sign_out(self: &Arc<Self>) {
        if self.closing.swap(true, Ordering::AcqRel) {
            return;
        }
        let peer = self.clone();
        tokio::spawn(async move {
            let mut streams = peer.streams.subscribe();
            let _ = tokio::time::timeout(SIGN_OUT_DRAIN, streams.wait_for(|n| *n == 0)).await;
            tracing::info!(endpoint = %peer.endpoint.fmt_short(), "closing a signed-out device's connection");
            peer.connection.close(
                VarInt::from_u32(spoolframe_protocol::close::SIGNED_OUT),
                b"signed out",
            );
        });
    }
}
