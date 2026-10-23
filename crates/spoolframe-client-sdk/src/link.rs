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
//!
//! The server may also close a connection for a reason it gives
//! ([`spoolframe_protocol::close`]). Signed out, the device has no session
//! there: they are ended here, and nothing dials again until a call needs
//! it. Refused as the server is full, the supervisor stops too, and so it
//! does when a dial fails in a way dialling again cannot fix: the server
//! speaks another protocol revision, or is not the server pinned. The
//! state watch says why ([`StateWatch::stopped`]).
//!
//! Any other failed dial is tried again after [`FIRST_RETRY`], doubling
//! up to [`MAX_RETRY`], each wait moved by up to [`JITTER`] either way.

use crate::link_meter::LinkMeter;
use parking_lot::Mutex;
use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Weak};
use std::time::Duration;

use iroh::EndpointId;
use iroh::endpoint::Connection;
use iroh::endpoint::RecvStream;
use spoolframe_protocol::codec::read_frame;
use spoolframe_protocol::downloads::Followed;
use spoolframe_protocol::streams::uni;
use spoolframe_protocol::{Event, ServerInfo, SessionId};
use tokio::io::AsyncReadExt;
use tokio::sync::watch;

use crate::client::{Client, Inner};
use crate::connection::{ServerConnection, closed_by_server};
use crate::error::SdkError;

/// The first wait after a connection is lost: a blip is dialled again at
/// once.
const FIRST_RETRY: Duration = Duration::from_millis(250);
/// The longest wait between two dials, before [`JITTER`]. A server that
/// stays unreachable is tried every few minutes, not every few seconds:
/// a phone left with an app open is not to dial all night.
const MAX_RETRY: Duration = Duration::from_secs(5 * 60);
/// How far each wait is moved at random, either way, as a share of it:
/// devices that lost a server together do not all dial it in step.
const JITTER: f64 = 0.2;
/// How long a call waits for a reconnect before it fails.
pub(crate) const CALL_WAIT: Duration = Duration::from_secs(10);
/// How much longer than its `Ping` a call waits for a [`Link::check`]:
/// the check drops a dead connection only after the ping's time limit.
const CHECK_MARGIN: Duration = Duration::from_millis(500);

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
    /// Through a relay server: one the server told this device of, with
    /// [`crate::ClientOptions::relays`] on (P6-03).
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

/// A link's state, and why the SDK stopped dialling, sent together so a
/// watcher never sees one without the other.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Status {
    state: ConnectionState,
    /// Set only with [`ConnectionState::Disconnected`].
    stopped: Option<SdkError>,
}

impl Status {
    fn new(state: ConnectionState) -> Self {
        Self {
            state,
            stopped: None,
        }
    }
}

/// Changes of one server's [`ConnectionState`].
#[derive(Debug, Clone)]
pub struct StateWatch(watch::Receiver<Status>);

impl StateWatch {
    pub fn current(&self) -> ConnectionState {
        self.0.borrow().state
    }

    /// The next state; `None` once the client is gone.
    pub async fn changed(&mut self) -> Option<ConnectionState> {
        self.0.changed().await.ok()?;
        Some(self.0.borrow_and_update().state)
    }

    /// Why the SDK stopped dialling this server by itself, while it is
    /// [`ConnectionState::Disconnected`] for that reason; `None` in every
    /// other state, and after [`crate::Client::forget`] or
    /// [`crate::Client::close`]. Read it when [`StateWatch::changed`]
    /// gives `Disconnected`, and show it in place of "Reconnecting…":
    ///
    /// - [`SdkError::UpdateRequired`]: the app and the server speak
    ///   different protocol revisions;
    /// - [`SdkError::IdentityChanged`], [`SdkError::ServerIdChanged`] or
    ///   [`SdkError::WrongServer`]: the server is not the one pinned;
    /// - [`SdkError::SignInRequired`]: the server signed this device out
    ///   ([`crate::Client::signed_out`]);
    /// - [`SdkError::ServerFull`]: the server holds no more devices that
    ///   are not signed in;
    /// - [`SdkError::UnknownServer`]: the server is no longer known.
    ///
    /// None of these goes away by dialling again, so the SDK does not. The
    /// next call dials once, and fails the same way until the cause is
    /// fixed. Every other failure (the server is unreachable, it did not
    /// answer in time) is dialled again with backoff, as
    /// [`ConnectionState::Reconnecting`].
    pub fn stopped(&self) -> Option<SdkError> {
        self.0.borrow().stopped.clone()
    }
}

/// Each time a server signs this device out: it closed the connection
/// because the device's last session there ended ([`crate::Client::signed_out`]).
#[derive(Debug, Clone)]
pub struct SignOutWatch(watch::Receiver<u64>);

impl SignOutWatch {
    /// Returns at the next sign-out; `None` once the client is gone. A
    /// sign-out since the last call returns at once, so none goes unseen
    /// between two calls; several are told as one.
    pub async fn next(&mut self) -> Option<()> {
        self.0.changed().await.ok()?;
        self.0.borrow_and_update();
        Some(())
    }
}

pub(crate) struct Link {
    id: EndpointId,
    state: watch::Sender<Status>,
    /// How many times the server has signed this device out.
    sign_outs: watch::Sender<u64>,
    /// The sessions a sign-out ended: a call in one is
    /// [`SdkError::SignInRequired`] without a dial.
    ended: Mutex<HashSet<String>>,
    current: Mutex<Option<Arc<ServerConnection>>>,
    /// What the latest connection's Hello said, kept while it reconnects.
    info: Mutex<Option<ServerInfo>>,
    /// One first dial at a time.
    dialing: tokio::sync::Mutex<()>,
    supervisor: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Cuts the supervisor's wait between dials short ([`Link::hurry`]).
    hurry: tokio::sync::Notify,
    /// [`Link::close`]d: the client dropped it (the server was forgotten,
    /// or its relays changed). A dial on the way when it closed keeps
    /// nothing, and no supervisor starts or goes on. A watch, so the
    /// supervisor's waits end on it ([`Link::closing`]): it stops between
    /// steps, never inside one, so a write to storage it began is
    /// finished (C6).
    closed: watch::Sender<bool>,
    /// How fast media comes from the server (P6-06), across connections.
    meter: Arc<LinkMeter>,
    /// How many [`Link::check`]s of the open connection are under way.
    checks: watch::Sender<u32>,
}

/// A check of a link's open connection, from [`Link::check`]: calls wait
/// until it is dropped. A drop guard, so a cancelled `resume` does not
/// leave calls waiting.
pub(crate) struct Check(Arc<Link>);

impl Drop for Check {
    fn drop(&mut self) {
        self.0.checks.send_modify(|n| *n -= 1);
    }
}

impl Link {
    pub fn new(id: EndpointId) -> Arc<Self> {
        Arc::new(Self {
            id,
            state: watch::Sender::new(Status::new(ConnectionState::Disconnected)),
            sign_outs: watch::Sender::new(0),
            ended: Mutex::default(),
            current: Mutex::default(),
            info: Mutex::default(),
            dialing: tokio::sync::Mutex::default(),
            supervisor: Mutex::default(),
            hurry: tokio::sync::Notify::new(),
            closed: watch::Sender::new(false),
            meter: Arc::default(),
            checks: watch::Sender::new(0),
        })
    }

    /// How fast media comes from the server (P6-06).
    pub fn meter(&self) -> Arc<LinkMeter> {
        self.meter.clone()
    }

    pub fn id(&self) -> EndpointId {
        self.id
    }

    pub fn watch(&self) -> StateWatch {
        StateWatch(self.state.subscribe())
    }

    pub fn sign_outs(&self) -> SignOutWatch {
        SignOutWatch(self.sign_outs.subscribe())
    }

    /// Whether the server signed this device out of `session`.
    pub fn has_ended(&self, session: &SessionId) -> bool {
        self.ended.lock().contains(&session.0)
    }

    /// The server signed this device out of `sessions`.
    pub fn end(&self, sessions: impl IntoIterator<Item = String>) {
        self.ended.lock().extend(sessions);
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
            .clone()
            .filter(|c| c.conn.close_reason().is_none())
    }

    /// What the server said in the latest connection's Hello; `None`
    /// before the first.
    pub fn info(&self) -> Option<ServerInfo> {
        self.info.lock().clone()
    }

    fn set(&self, conn: Option<Arc<ServerConnection>>, state: ConnectionState) {
        if let Some(conn) = &conn {
            *self.info.lock() = Some(conn.info.clone());
        }
        *self.current.lock() = conn;
        self.show(state);
    }

    /// Say `state`, with no reason to have stopped.
    fn show(&self, state: ConnectionState) {
        self.state.send_replace(Status::new(state));
    }

    /// Stop with no connection: `Disconnected`, because of `reason`.
    fn stop(&self, reason: SdkError) {
        *self.current.lock() = None;
        self.state.send_replace(Status {
            state: ConnectionState::Disconnected,
            stopped: Some(reason),
        });
    }

    /// The open connection: as it is, after the supervisor's reconnect, or
    /// from a first dial made here.
    pub async fn connection(
        self: &Arc<Self>,
        client: &Client,
    ) -> Result<Arc<ServerConnection>, SdkError> {
        // The app comes back and sends a request as it calls `resume`:
        // the connection may look open and be dead. Wait for the check,
        // not the request's whole time limit on a dead connection.
        let limit = client.inner.timeouts.ping + CHECK_MARGIN;
        let _ = tokio::time::timeout(limit, self.checks.subscribe().wait_for(|n| *n == 0)).await;
        if let Some(conn) = self.open() {
            return Ok(conn);
        }
        if self.supervised() {
            // A call waits CALL_WAIT; the supervisor's wait may be minutes.
            self.hurry();
            if let Some(conn) = self.reconnected().await? {
                return Ok(conn);
            }
        }
        let _dialing = self.dialing.lock().await;
        if let Some(conn) = self.open() {
            return Ok(conn);
        }
        if self.is_closed() {
            return Err(self.closed_error());
        }
        self.show(ConnectionState::Connecting);
        match client.dial(self.id).await {
            Ok(conn) => {
                let conn = Arc::new(conn);
                // Checked under the supervisor's lock, which `close` takes
                // after it sets `closed`: either this sees it closed, or
                // `close` finds the supervisor and the connection set.
                let mut supervisor = self.supervisor.lock();
                if self.is_closed() {
                    drop(supervisor);
                    conn.conn.close(0u32.into(), b"closed");
                    return Err(self.closed_error());
                }
                self.set(Some(conn.clone()), ConnectionState::Connected);
                *supervisor = Some(tokio::spawn(supervise(
                    self.clone(),
                    Arc::downgrade(&client.inner),
                    conn.clone(),
                )));
                Ok(conn)
            }
            Err(e) if gives_up(&e) => {
                self.stop(e.clone());
                Err(e)
            }
            Err(e) => {
                self.show(ConnectionState::Disconnected);
                Err(e)
            }
        }
    }

    /// A call, or the app coming back, wants this server now: should the
    /// supervisor be waiting between dials, it dials at once instead of
    /// sleeping out a wait of up to [`MAX_RETRY`]. Should it be dialling,
    /// it dials again at once if that one fails.
    pub(crate) fn hurry(&self) {
        self.hurry.notify_one();
    }

    fn is_closed(&self) -> bool {
        *self.closed.borrow()
    }

    /// Returns once the link is [`Link::close`]d; at once if it is.
    async fn closing(&self) {
        // The sender is the link's own, so it outlives this wait.
        let _ = self.closed.subscribe().wait_for(|closed| *closed).await;
    }

    /// What a dial on a closed link gives: the client no longer keeps
    /// this server.
    fn closed_error(&self) -> SdkError {
        SdkError::UnknownServer(self.id.to_string())
    }

    pub(crate) fn supervised(&self) -> bool {
        self.supervisor
            .lock()
            .as_ref()
            .is_some_and(|t| !t.is_finished())
    }

    /// Wait up to [`CALL_WAIT`] for the supervisor to reconnect. The
    /// state may still say `Connected` of a connection just closed, until
    /// the supervisor has seen it go: what counts is an open connection.
    /// `None` when the supervisor stopped instead (the device was signed
    /// out, or the server is full): the caller dials.
    async fn reconnected(&self) -> Result<Option<Arc<ServerConnection>>, SdkError> {
        let deadline = tokio::time::Instant::now() + CALL_WAIT;
        let mut state = self.state.subscribe();
        loop {
            if let Some(conn) = self.open() {
                return Ok(Some(conn));
            }
            if state.borrow_and_update().state == ConnectionState::Disconnected {
                return Ok(None);
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

    /// `resume` is checking whether the open connection answers: calls
    /// wait in [`Link::connection`] until the returned guard is dropped.
    pub(crate) fn check(self: &Arc<Self>) -> Check {
        self.checks.send_modify(|n| *n += 1);
        Check(self.clone())
    }

    /// `conn` has stopped answering: close it, so that no call uses it
    /// again and the supervisor dials anew. Nothing changes if it was
    /// already replaced.
    pub fn drop_dead(&self, conn: &Arc<ServerConnection>) {
        tracing::info!(server = %self.id, "the connection stopped answering; dropping it");
        let supervised = self.supervised();
        {
            let mut current = self.current.lock();
            if current.as_ref().is_some_and(|c| Arc::ptr_eq(c, conn)) {
                *current = None;
                self.show(if supervised {
                    ConnectionState::Reconnecting { attempt: 1 }
                } else {
                    ConnectionState::Disconnected
                });
            }
        }
        conn.conn.close(0u32.into(), b"no answer");
    }

    /// Stop: no more reconnects, and the connection closed. For good: a
    /// dial on the way keeps nothing.
    ///
    /// The supervisor is not aborted (C6): aborting it in the middle of
    /// saving what the server told could cut a storage write short. It
    /// sees the link closed at its next wait, which ends on it, and
    /// returns. Every step between two waits ends by itself: a dial
    /// within the client's timeouts, a storage write when the storage is
    /// done. So no abort is kept even as a fallback.
    pub fn close(&self) {
        self.closed.send_replace(true);
        // Detached, not aborted: it stops on its own.
        drop(self.supervisor.lock().take());
        if let Some(conn) = self.current.lock().take() {
            conn.conn.close(0u32.into(), b"closed");
        }
        self.show(ConnectionState::Disconnected);
    }
}

/// Watch `conn`; when it is lost, dial again until connected, and watch
/// that one. Ends when the client is gone or forgets the server.
async fn supervise(link: Arc<Link>, client: Weak<Inner>, mut conn: Arc<ServerConnection>) {
    loop {
        if link.is_closed() {
            return;
        }
        until_lost(&link, &conn.conn, &client).await;
        if link.is_closed() {
            return;
        }
        let reason = conn.conn.close_reason();
        if reason.as_ref().and_then(closed_by_server) == Some(SdkError::SignInRequired) {
            signed_out(&link, &client).await;
            return;
        }
        tracing::info!(server = %link.id, "connection lost; reconnecting");
        let dialled = redial(&link, || {
            let client = client.upgrade();
            let id = link.id;
            async move { Some((Client { inner: client? }).dial(id).await) }
        })
        .await;
        let Some((dialled, attempt)) = dialled else {
            return;
        };
        conn = Arc::new(dialled);
        if link.is_closed() {
            conn.conn.close(0u32.into(), b"closed");
            return;
        }
        tracing::info!(server = %link.id, attempt, "reconnected");
        link.set(Some(conn.clone()), ConnectionState::Connected);
    }
}

/// Dial with `dial` until it connects, waiting longer after each failure
/// ([`Backoff`]): the connection and the attempt it took. `None` when the
/// client is gone (`dial` gives `None`), or when the SDK gave up on an
/// error dialling again would not fix: then the link is `Disconnected`,
/// and says why ([`StateWatch::stopped`]).
async fn redial<T, Fut>(link: &Link, mut dial: impl FnMut() -> Fut) -> Option<(T, u32)>
where
    Fut: Future<Output = Option<Result<T, SdkError>>>,
{
    let mut backoff = Backoff::default();
    let mut attempt = 1;
    loop {
        if link.is_closed() {
            return None;
        }
        link.set(None, ConnectionState::Reconnecting { attempt });
        match dial().await? {
            Ok(conn) => return Some((conn, attempt)),
            Err(e) if gives_up(&e) => {
                tracing::info!(server = %link.id, attempt, error = %e, "not reconnecting");
                link.stop(e);
                return None;
            }
            Err(e) => {
                tracing::debug!(server = %link.id, attempt, error = %e, "reconnect failed");
            }
        }
        // A call coming in cuts the wait short: the server may be back,
        // and the call waits only CALL_WAIT.
        tokio::select! {
            () = tokio::time::sleep(backoff.next_wait()) => {}
            () = link.hurry.notified() => {
                tracing::debug!(server = %link.id, attempt, "hurried; dialling now");
            }
            () = link.closing() => return None,
        }
        attempt += 1;
    }
}

/// Whether the supervisor stops on `error` instead of dialling again: the
/// same dial would fail the same way, and only the user, the app or the
/// server's admin can change that.
pub(crate) fn gives_up(error: &SdkError) -> bool {
    matches!(
        error,
        SdkError::UnknownServer(_)
            // Let in again only once signed in.
            | SdkError::ServerFull
            | SdkError::UpdateRequired { .. }
            // Not the server pinned: the user decides, never a retry.
            | SdkError::IdentityChanged { .. }
            | SdkError::ServerIdChanged { .. }
            | SdkError::WrongServer { .. }
            | SdkError::SignInRequired
    )
}

/// The waits between dials: [`FIRST_RETRY`], doubling up to [`MAX_RETRY`],
/// each moved by up to [`JITTER`] either way.
#[derive(Debug)]
struct Backoff {
    next: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self { next: FIRST_RETRY }
    }
}

impl Backoff {
    fn next_wait(&mut self) -> Duration {
        let wait = self
            .next
            .mul_f64(1.0 + JITTER * (2.0 * unit_random() - 1.0));
        self.next = (self.next * 2).min(MAX_RETRY);
        wait
    }
}

/// A number in `[0, 1)`, random enough to spread waits: std's hasher keys
/// are random per process and per call, so no RNG crate is needed.
fn unit_random() -> f64 {
    use std::hash::{BuildHasher, RandomState};
    let bits = RandomState::new().hash_one(std::time::SystemTime::now());
    // The top 53 bits: as many as an f64 holds exactly.
    (bits >> 11) as f64 / (1u64 << 53) as f64
}

/// The server closed the connection because this device's last session
/// there ended. It sends that only once no session is left for the
/// device, so every session kept for the server is ended here, before the
/// app is told ([`crate::Client::signed_out`]).
async fn signed_out(link: &Link, client: &Weak<Inner>) {
    tracing::info!(server = %link.id, "signed out by the server; its sessions here are ended");
    if let Some(inner) = client.upgrade() {
        match (Client { inner }).end_every_session(link.id).await {
            Ok(tokens) => link.end(tokens),
            Err(e) => tracing::warn!(server = %link.id, error = %e, "cannot end the sessions kept"),
        }
    }
    link.stop(SdkError::SignInRequired);
    link.sign_outs.send_modify(|n| *n += 1);
}

/// Returns when `conn` closes, the server says it is shutting down (then
/// it is about to close, and dialling can start), or the link is closed.
/// Meanwhile the server's other events on it are applied. Its addresses
/// (PX-12), pushed once the device is signed in and when they change,
/// replace those it told before.
///
/// Only reading the next event is cut short by a close: an event read is
/// applied in full first, so saving the addresses is never stopped
/// halfway (C6). If the events stream cannot be read, waits for the
/// connection to close instead.
async fn until_lost(link: &Link, conn: &Connection, client: &Weak<Inner>) {
    let stop = async {
        tokio::select! {
            _ = conn.closed() => {}
            () = link.closing() => {}
        }
    };
    tokio::pin!(stop);
    let mut recv = tokio::select! {
        () = &mut stop => return,
        recv = events(conn) => match recv {
            Some(recv) => recv,
            None => return stop.await,
        },
    };
    loop {
        let event = tokio::select! {
            () = &mut stop => return,
            event = read_frame::<_, Event>(&mut recv) => event,
        };
        let Ok(event) = event else {
            return stop.await;
        };
        if apply(event, link.id, client).await.is_break() {
            return;
        }
    }
}

/// The server's events stream on `conn`; `None` if it cannot be read.
async fn events(conn: &Connection) -> Option<RecvStream> {
    let mut recv = conn.accept_uni().await.ok()?;
    (recv.read_u8().await.ok()? == uni::EVENTS).then_some(recv)
}

/// Pass a playlist change from server `id` on; nobody listening is fine.
fn tell_playlists(client: &Weak<Inner>, id: EndpointId, change: crate::PlaylistChange) {
    if let Some(inner) = client.upgrade() {
        let _ = inner.playlist_events.send(crate::PlaylistEvent {
            server: id.to_string(),
            change,
        });
    }
}

/// Tell the download manager that `followed` changed on server `id`.
fn tell_music(client: &Weak<Inner>, id: EndpointId, followed: Followed) {
    if let Some(inner) = client.upgrade() {
        inner.downloads.followed_changed(id, followed);
    }
}

/// Apply `event` from server `id`; `Break` on `ShuttingDown`.
async fn apply(event: Event, id: EndpointId, client: &Weak<Inner>) -> std::ops::ControlFlow<()> {
    match event {
        Event::ShuttingDown => return std::ops::ControlFlow::Break(()),
        // PX-40: for whoever shows playlists; PX-51: a playlist
        // downloaded is asked about again.
        Event::PlaylistChanged {
            id: playlist,
            revision,
        } => {
            tell_music(
                client,
                id,
                Followed::Playlist {
                    id: playlist.clone(),
                },
            );
            tell_playlists(
                client,
                id,
                crate::PlaylistChange::Changed {
                    id: playlist,
                    revision,
                },
            );
        }
        Event::PlaylistGone { id: playlist } => {
            tell_music(
                client,
                id,
                Followed::Playlist {
                    id: playlist.clone(),
                },
            );
            tell_playlists(client, id, crate::PlaylistChange::Gone { id: playlist });
        }
        Event::PinsChanged => tell_playlists(client, id, crate::PlaylistChange::PinsChanged),
        // PX-51: Liked downloaded is asked about again.
        Event::LikedChanged => tell_music(client, id, Followed::Liked),
        Event::ServerAddresses {
            addrs,
            relays,
            lookup,
        } => {
            if let Some(inner) = client.upgrade()
                && let Err(e) = (Client { inner }).told(id, addrs, relays, lookup).await
            {
                tracing::warn!(server = %id, error = %e, "cannot save the addresses the server told");
            }
        }
        // Followed by the managed playback of `handle` (PX-46).
        Event::PlaybackShare {
            handle,
            max_bitrate,
        } => {
            tracing::debug!(server = %id, ?max_bitrate, "a playback's share changed");
            if let Some(inner) = client.upgrade() {
                let _ = inner.shares.send(crate::PlaybackShare {
                    handle,
                    max_bitrate,
                });
            }
        }
        // P8-04: a paired server's item's playback ended where it plays;
        // the next use of it says so, as it would without this.
        Event::PlaybackEnded { error, .. } => {
            tracing::info!(server = %id, %error, "a paired server's item's playback ended");
        }
        // PX-03: for whoever shows or plays the track.
        Event::SubtitleTimingChanged {
            id: item,
            track,
            timing,
        } => {
            if let Some(inner) = client.upgrade() {
                let _ = inner.subtitle_events.send(crate::SubtitleTimingEvent {
                    server: id.to_string(),
                    id: item,
                    track,
                    timing,
                });
            }
        }
        // PX-38: the download manager asks again at once.
        Event::DownloadChanged { id: download, .. } => {
            if let Some(inner) = client.upgrade() {
                inner.downloads.server_event(id, download);
            }
        }
        // Experimental (P8-02): for whoever lists the libraries.
        Event::LibrariesChanged => {
            if let Some(inner) = client.upgrade() {
                let _ = inner.library_events.send(crate::LibrariesChanged {
                    server: id.to_string(),
                });
            }
        }
        Event::Unknown => {}
    }
    std::ops::ControlFlow::Continue(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link() -> Arc<Link> {
        Link::new(iroh::SecretKey::from_bytes(&[7; 32]).public())
    }

    /// When each dial was made, `dial` failing with `error` until the
    /// `succeed_at`th, which connects.
    async fn dials(link: &Link, error: SdkError, succeed_at: usize) -> Vec<tokio::time::Instant> {
        let made = Mutex::new(Vec::new());
        redial(link, || {
            let n = {
                let mut made = made.lock();
                made.push(tokio::time::Instant::now());
                made.len()
            };
            let error = error.clone();
            async move { Some(if n >= succeed_at { Ok(()) } else { Err(error) }) }
        })
        .await;
        made.into_inner()
    }

    /// The waits between dials, under tokio's paused clock: each is the
    /// time the supervisor slept.
    fn waits(made: &[tokio::time::Instant]) -> Vec<Duration> {
        made.windows(2).map(|w| w[1] - w[0]).collect()
    }

    #[tokio::test(start_paused = true)]
    async fn an_unreachable_server_is_dialled_less_and_less_often_up_to_five_minutes() {
        let made = dials(&link(), SdkError::Unreachable("down".into()), 40).await;
        let waits = waits(&made);
        let five_minutes = Duration::from_secs(300);
        let mut nominal = Duration::from_millis(250);
        for (n, wait) in waits.iter().enumerate() {
            assert!(
                *wait >= nominal.mul_f64(0.8) && *wait <= nominal.mul_f64(1.2),
                "wait {n} was {wait:?}, not {nominal:?} \u{b1}20%: {waits:?}"
            );
            nominal = (nominal * 2).min(five_minutes);
        }
        // A blip is still dialled again quickly...
        assert!(waits[0] <= Duration::from_millis(300), "{waits:?}");
        // ...and a server that stays down is left alone for minutes.
        assert!(
            waits.iter().any(|w| *w >= Duration::from_secs(240)),
            "the waits never grew past {:?}",
            waits.iter().max()
        );
    }

    /// After a long outage the supervisor sleeps for minutes between
    /// dials. A call that comes then hurries it: the server, back by now,
    /// is dialled at once, well within the call's wait.
    #[tokio::test(start_paused = true)]
    async fn a_call_during_a_long_wait_dials_at_once() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let link = link();
        let up = Arc::new(AtomicBool::new(false));
        let made = Arc::new(Mutex::new(Vec::new()));
        let supervisor = tokio::spawn({
            let (link, up, made) = (link.clone(), up.clone(), made.clone());
            async move {
                redial(&link, || {
                    made.lock().push(tokio::time::Instant::now());
                    let up = up.load(Ordering::SeqCst);
                    async move {
                        Some(if up {
                            Ok(())
                        } else {
                            Err(SdkError::Unreachable("down".into()))
                        })
                    }
                })
                .await
            }
        });
        // Down for long enough that the wait after the 12th dial is
        // minutes (250 ms doubled 11 times, up to five minutes).
        while made.lock().len() < 12 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let waits = waits(&made.lock());
        assert!(
            waits.last().is_some_and(|w| *w >= Duration::from_secs(100)),
            "{waits:?}"
        );

        up.store(true, Ordering::SeqCst);
        let called = tokio::time::Instant::now();
        link.hurry();
        let dialled = tokio::time::timeout(CALL_WAIT, supervisor)
            .await
            .expect("the supervisor slept on past the call's wait")
            .unwrap();
        assert_eq!(dialled.map(|(_, attempt)| attempt), Some(13));
        assert!(
            called.elapsed() < Duration::from_secs(1),
            "{:?}",
            called.elapsed()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_waits_are_spread_so_devices_do_not_dial_in_step() {
        let made = dials(&link(), SdkError::Unreachable("down".into()), 40).await;
        let capped: Vec<Duration> = waits(&made)
            .into_iter()
            .filter(|w| *w >= Duration::from_secs(240))
            .collect();
        assert!(capped.len() > 10, "{capped:?}");
        let first = capped[0];
        assert!(capped.iter().any(|w| *w != first), "{capped:?}");
    }

    /// What dialling again cannot fix stops the supervisor at once, with
    /// the reason on the state watch.
    #[tokio::test(start_paused = true)]
    async fn a_permanent_error_is_dialled_once_and_shown() {
        for error in [
            SdkError::UpdateRequired {
                server_protocol: None,
            },
            SdkError::IdentityChanged {
                saved: "a".into(),
                now: "b".into(),
            },
            SdkError::ServerIdChanged {
                endpoint_id: "a".into(),
                saved: "b".into(),
                now: "c".into(),
            },
            SdkError::WrongServer {
                expected: "a".into(),
                actual: "b".into(),
            },
            SdkError::ServerFull,
            SdkError::UnknownServer("a".into()),
        ] {
            let link = link();
            let watch = link.watch();
            let made = dials(&link, error.clone(), 5).await;
            assert_eq!(made.len(), 1, "{error:?} was dialled again");
            assert_eq!(watch.current(), ConnectionState::Disconnected, "{error:?}");
            assert_eq!(watch.stopped(), Some(error));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_reconnect_clears_the_reason() {
        let link = link();
        let watch = link.watch();
        link.stop(SdkError::ServerFull);
        dials(&link, SdkError::Unreachable("down".into()), 2).await;
        assert_eq!(watch.stopped(), None);
    }

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
