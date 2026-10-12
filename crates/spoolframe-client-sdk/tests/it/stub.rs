//! A stand-in server that can stop answering without closing anything:
//! the connection stays up and QUIC keeps acknowledging packets, but no
//! request on it is answered. That is what a client sees when the server
//! side of a connection quietly died (an app suspended by iOS comes back
//! to it), and it is worse than a lost path: nothing in QUIC times it out.

use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use iroh::endpoint::{Connection, RecvStream, SendStream, presets};
use iroh::{Endpoint, EndpointId, RelayMode, SecretKey};
use spoolframe_protocol::codec::{decode, read_frame_bytes, request_type, write_frame};
use spoolframe_protocol::streams::{bi, uni};
use spoolframe_protocol::{
    AuthMethod, CLIENT_ALPN, DeviceSignInRequest, Error, Event, PROTOCOL_VERSION, Reply, Request,
    Response, ServerInfo, SessionId, UserSummary,
};
use spoolframe_types::Role;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Default)]
struct State {
    /// Each connection's "stop answering" switch, in the order accepted.
    connections: Mutex<Vec<Arc<AtomicBool>>>,
    /// No connection, open or new, answers anything.
    frozen: AtomicBool,
    /// Requests of these types are never answered; others are.
    stalled: Mutex<HashSet<String>>,
    /// Each request's type and the connection (from 0) it came on.
    requests: Mutex<Vec<(usize, String)>>,
    invocations: Mutex<Vec<(String, spoolframe_protocol::rpc::RpcRequest)>>,
    /// The length advertised by binary replies and the bytes actually sent.
    body: Mutex<(u64, Vec<u8>)>,
    /// Binary replies' bytes go out this many at a time, this far apart:
    /// a slow link, or an encode still writing.
    trickle: Mutex<Option<(usize, std::time::Duration)>>,
    /// The addresses the `ServerAddresses` event pushes (PX-12).
    told: Mutex<Vec<SocketAddr>>,
    /// The relays the `ServerAddresses` event lists.
    relays: Mutex<Vec<String>>,
    /// A device signed in here: from then on each connection is pushed
    /// the addresses after its Hello, as the server pushes a device that
    /// kept its session.
    signed_in: AtomicBool,
    /// Each connection's events stream (from 0), opened at its Hello.
    events: Mutex<HashMap<usize, Arc<tokio::sync::Mutex<SendStream>>>>,
    /// What `ServerInfo` and `DeviceSignInStarted` give as the web UI.
    web_url: Mutex<Option<String>>,
    /// Every connection accepted, to close them all at once.
    open: Mutex<Vec<Connection>>,
    /// `Hello` is refused as the server refuses another protocol revision.
    refuse_hellos: AtomicBool,
    /// Answer `LeaveServer` with `LastAdmin` (PX-47), and `WhoAmI` that
    /// the user cannot leave.
    last_admin: AtomicBool,
    /// The protocol revision `ServerInfo` says, when not this build's.
    protocol: Mutex<Option<u32>>,
    /// The server id `ServerInfo` says, when not the one made from the key.
    server_id: Mutex<Option<String>>,
}

pub struct Stub {
    endpoint: Endpoint,
    state: Arc<State>,
    task: tokio::task::JoinHandle<()>,
}

impl Stub {
    pub async fn start() -> Self {
        let endpoint = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .alpns(vec![CLIENT_ALPN.to_vec()])
            .bind()
            .await
            .unwrap();
        Self::serving(endpoint)
    }

    /// A stub with key `key`, bound only on `addrs` (at most one of each
    /// address family), so that it answers there and nowhere else.
    pub async fn start_on(key: SecretKey, addrs: &[SocketAddr]) -> Self {
        let mut builder = Endpoint::builder(presets::Minimal)
            .secret_key(key)
            .relay_mode(RelayMode::Disabled)
            .alpns(vec![CLIENT_ALPN.to_vec()])
            .clear_ip_transports();
        for addr in addrs {
            builder = builder.bind_addr(*addr).unwrap();
        }
        Self::serving(builder.bind().await.unwrap())
    }

    fn serving(endpoint: Endpoint) -> Self {
        let state = Arc::new(State::default());
        let task = tokio::spawn(accept(endpoint.clone(), state.clone()));
        Self {
            endpoint,
            state,
            task,
        }
    }

    pub fn id(&self) -> String {
        self.endpoint.id().to_string()
    }

    pub fn addrs(&self) -> Vec<std::net::SocketAddr> {
        self.endpoint.addr().ip_addrs().copied().collect()
    }

    /// The sockets it is bound on, ports and all.
    pub fn bound(&self) -> Vec<SocketAddr> {
        self.endpoint.bound_sockets()
    }

    /// The connections open now stop answering for good; new ones answer.
    /// The server is alive, its old connections are not.
    pub fn kill_open_connections(&self) {
        for conn in self.state.connections.lock().iter() {
            conn.store(true, Ordering::SeqCst);
        }
    }

    /// Close every connection open now, as a server that restarted does.
    /// New ones are taken.
    pub fn close_open_connections(&self) {
        for conn in self.state.open.lock().drain(..) {
            conn.close(0u32.into(), b"restarting");
        }
    }

    /// Refuse `LeaveServer` as the last admin's (PX-47), and say so in
    /// `WhoAmI`.
    pub fn refuse_leaving(&self) {
        self.state.last_admin.store(true, Ordering::SeqCst);
    }

    /// Refuse every `Hello` from now on, as a server refuses one of
    /// another protocol revision.
    pub fn refuse_hellos(&self) {
        self.state.refuse_hellos.store(true, Ordering::SeqCst);
    }

    /// Answer `Hello` with protocol revision `revision` from now on.
    pub fn speak_protocol(&self, revision: u32) {
        *self.state.protocol.lock() = Some(revision);
    }

    /// Answer `Hello` with server id `id` from now on: another server
    /// under this one's key.
    pub fn answer_as_server_id(&self, id: &str) {
        *self.state.server_id.lock() = Some(id.to_owned());
    }

    /// Nothing is answered, on any connection, until [`Stub::thaw`].
    pub fn freeze(&self) {
        self.state.frozen.store(true, Ordering::SeqCst);
    }

    /// Answer again; connections killed stay dead.
    pub fn thaw(&self) {
        self.state.frozen.store(false, Ordering::SeqCst);
    }

    /// Never answer a request of type `kind` (`"libraries"`), as a server
    /// that is slow at it; everything else, `Ping` too, is answered.
    pub fn stall(&self, kind: &str) {
        self.state.stalled.lock().insert(kind.to_owned());
    }

    /// How many of the connections it accepted are open still.
    pub fn open_connections(&self) -> usize {
        self.state
            .open
            .lock()
            .iter()
            .filter(|c| c.close_reason().is_none())
            .count()
    }

    /// How many connections it has accepted.
    pub fn connections(&self) -> usize {
        self.state.connections.lock().len()
    }

    /// How many requests of type `kind` arrived, answered or not.
    pub fn count(&self, kind: &str) -> usize {
        self.requests_of(kind).len()
    }

    /// Tell a signed-in device `addrs`, whether the stub is bound there
    /// or not, from the next push on.
    pub fn tell(&self, addrs: &[SocketAddr]) {
        *self.state.told.lock() = addrs.to_vec();
    }

    /// List `relays` in the `ServerAddresses` event from the next push on.
    pub fn tell_relays(&self, relays: &[&str]) {
        *self.state.relays.lock() = relays.iter().map(|r| (*r).to_owned()).collect();
    }

    /// Push the addresses to every connection that said Hello, as the
    /// server does when they change.
    pub async fn push(&self) {
        let indices: Vec<usize> = self.state.events.lock().keys().copied().collect();
        for index in indices {
            push(&self.state, index).await;
        }
    }

    /// Push a `PlaybackShare` event (PX-46) to every connection that said
    /// Hello, as the server does when a playback's share of its uplink
    /// moves.
    pub async fn push_share(&self, handle: &str, max_bitrate: Option<u64>) {
        let streams: Vec<_> = self.state.events.lock().values().cloned().collect();
        let event = Event::PlaybackShare {
            handle: spoolframe_protocol::PlaybackHandle(handle.to_owned()),
            max_bitrate,
        };
        for events in streams {
            let _ = write_frame(&mut *events.lock().await, &event).await;
        }
    }

    /// Say `url` is the server's web UI from now on.
    pub fn web_url(&self, url: &str) {
        *self.state.web_url.lock() = Some(url.to_owned());
    }

    pub fn body(&self, announced: u64, bytes: &[u8]) {
        *self.state.body.lock() = (announced, bytes.to_vec());
    }

    /// Send binary replies `piece` bytes at a time, `every` apart.
    pub fn trickle(&self, piece: usize, every: std::time::Duration) {
        *self.state.trickle.lock() = Some((piece, every));
    }

    pub fn invocations(&self, kind: &str) -> Vec<spoolframe_protocol::rpc::RpcRequest> {
        self.state
            .invocations
            .lock()
            .iter()
            .filter(|(name, _)| name == kind)
            .map(|(_, request)| request.clone())
            .collect()
    }

    /// The connection (from 0) each request of type `kind` arrived on.
    pub fn requests_of(&self, kind: &str) -> Vec<usize> {
        self.state
            .requests
            .lock()
            .iter()
            .filter(|(_, k)| k == kind)
            .map(|(conn, _)| *conn)
            .collect()
    }

    /// Stop, and return once the addresses it was bound on are free, so a
    /// stub started next on the same address binds there.
    ///
    /// iroh closes its UDP sockets only when the last handle on the
    /// endpoint is gone, and `close` does not wait for that: the accept
    /// task holds a clone until tokio drops it, each events stream in
    /// `state` holds its connection and with it the socket, and the
    /// tasks still serving streams drop theirs only when next polled.
    pub async fn stop(self) {
        let bound = self.endpoint.bound_sockets();
        self.task.abort();
        let _ = self.task.await;
        self.endpoint.close().await;
        self.state.events.lock().clear();
        self.state.open.lock().clear();
        drop(self.endpoint);
        drop(self.state);
        until_free(&bound).await;
    }
}

/// Wait until nothing holds a UDP socket on any of `addrs`.
async fn until_free(addrs: &[SocketAddr]) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    for addr in addrs {
        while let Err(err) = std::net::UdpSocket::bind(addr) {
            assert!(
                err.kind() == std::io::ErrorKind::AddrInUse
                    && tokio::time::Instant::now() < deadline,
                "{addr} was never freed: {err}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}

async fn accept(endpoint: Endpoint, state: Arc<State>) {
    while let Some(incoming) = endpoint.accept().await {
        let Ok(conn) = incoming.await else {
            continue;
        };
        let dead = Arc::new(AtomicBool::new(false));
        let index = {
            let mut connections = state.connections.lock();
            connections.push(dead.clone());
            connections.len() - 1
        };
        state.open.lock().push(conn.clone());
        let id = endpoint.id();
        tokio::spawn(serve(conn, id, index, dead, state.clone()));
    }
}

async fn serve(
    conn: Connection,
    id: EndpointId,
    index: usize,
    dead: Arc<AtomicBool>,
    state: Arc<State>,
) {
    while let Ok((send, recv)) = conn.accept_bi().await {
        tokio::spawn(stream(
            conn.clone(),
            id,
            index,
            dead.clone(),
            state.clone(),
            send,
            recv,
        ));
    }
}

async fn stream(
    conn: Connection,
    id: EndpointId,
    index: usize,
    dead: Arc<AtomicBool>,
    state: Arc<State>,
    mut send: SendStream,
    mut recv: RecvStream,
) {
    let Ok(stream_kind) = recv.read_u8().await else {
        return;
    };
    let Ok(bytes) = read_frame_bytes(&mut recv).await else {
        return;
    };
    if stream_kind == bi::RESOURCE {
        use spoolframe_protocol::resource::{ResourceMetadata, ResourceRequest, ResourceResponse};
        let request: ResourceRequest = decode(&bytes).unwrap();
        let (length, bytes) = state.body.lock().clone();
        let metadata = match request {
            ResourceRequest::Media { revision, .. } => ResourceMetadata::Media {
                size: length,
                complete: true,
                epoch: 0,
                revision,
            },
            ResourceRequest::Artwork { .. } => ResourceMetadata::Artwork {
                content_type: "image/png".into(),
                etag: "stub".into(),
            },
            _ => panic!("unexpected resource"),
        };
        let sent = write_frame(&mut send, &ResourceResponse::Ready { length, metadata }).await;
        let trickle = *state.trickle.lock();
        let written = match (sent, trickle) {
            (Err(_), _) => false,
            (Ok(_), None) => send.write_all(&bytes).await.is_ok(),
            (Ok(_), Some((piece, every))) => {
                let mut ok = true;
                for (i, part) in bytes.chunks(piece.max(1)).enumerate() {
                    if i > 0 {
                        tokio::time::sleep(every).await;
                    }
                    if send.write_all(part).await.is_err() {
                        ok = false;
                        break;
                    }
                }
                ok
            }
        };
        if written {
            let _ = send.finish();
        }
        let _ = send.stopped().await;
        return;
    }
    assert_eq!(stream_kind, bi::RPC);
    let wire: spoolframe_protocol::rpc::RpcRequest = decode(&bytes).unwrap();
    let kind = request_type(&spoolframe_protocol::codec::encode(&wire.envelope).unwrap())
        .unwrap_or_default();
    state.requests.lock().push((index, kind.clone()));
    state.invocations.lock().push((kind.clone(), wire.clone()));
    let silent = dead.load(Ordering::SeqCst)
        || state.frozen.load(Ordering::SeqCst)
        || state.stalled.lock().contains(&kind);
    if silent {
        // The stream stays open and unanswered for as long as the
        // connection lasts.
        conn.closed().await;
        return;
    }
    // Before the reply, as the server opens the events stream in Hello.
    match kind.as_str() {
        "hello" => {
            if let Ok(mut events) = conn.open_uni().await
                && events.write_u8(uni::EVENTS).await.is_ok()
            {
                state
                    .events
                    .lock()
                    .insert(index, Arc::new(tokio::sync::Mutex::new(events)));
                if state.signed_in.load(Ordering::SeqCst) {
                    push(&state, index).await;
                }
            }
        }
        "login_password" => {
            state.signed_in.store(true, Ordering::SeqCst);
            push(&state, index).await;
        }
        _ => {}
    }
    let response = spoolframe_protocol::rpc::RpcResponse {
        server: spoolframe_protocol::rpc::ServerInstance {
            id: id.to_string(),
            time_ms: 1_000_000,
        },
        reply: answer(id, &state, wire.envelope).into(),
    };
    if spoolframe_protocol::codec::write_reply(&mut send, &response)
        .await
        .is_ok()
    {
        let _ = send.finish();
    }
    // Keep the stream until the client has read it.
    let _ = send.stopped().await;
}

/// Push the `ServerAddresses` event on connection `index`'s events stream.
async fn push(state: &State, index: usize) {
    let Some(events) = state.events.lock().get(&index).cloned() else {
        return;
    };
    let event = Event::ServerAddresses {
        addrs: state.told.lock().clone(),
        relays: state.relays.lock().clone(),
        lookup: None,
    };
    let _ = write_frame(&mut *events.lock().await, &event).await;
}

fn answer(
    id: EndpointId,
    state: &State,
    envelope: spoolframe_protocol::RequestEnvelope,
) -> Response {
    let web_url = state.web_url.lock().clone();
    // Alice's session is "stub-session"; anyone else's names them, so two
    // profiles on one device are two sessions.
    let username = envelope
        .session
        .as_ref()
        .and_then(|s| s.0.strip_prefix("stub-session-"))
        .unwrap_or("alice")
        .to_owned();
    // Made from its key: two stubs are two servers (PX-14).
    let server_id = state.server_id.lock().clone().unwrap_or_else(|| {
        spoolframe_protocol::ServerId::from_bytes(
            id.as_bytes()[..10].try_into().expect("a key is 32 bytes"),
        )
        .to_string()
    });
    let reply = match envelope.request {
        // What the server answers a Hello of another revision.
        Request::Hello(_) if state.refuse_hellos.load(Ordering::SeqCst) => {
            return Response::Err(Error::Unsupported {
                request: "protocol revision; update the SDK".into(),
            });
        }
        Request::Hello(_) => Reply::ServerInfo(ServerInfo {
            name: "Stub".into(),
            endpoint_id: id.to_string(),
            server_id,
            protocol_version: state.protocol.lock().unwrap_or(PROTOCOL_VERSION),
            server_version: "0".into(),
            auth_methods: vec![AuthMethod::Password],
            web_url,
        }),
        Request::BeginDeviceSignIn { .. } => Reply::DeviceSignInStarted {
            request: "stub-request".into(),
            code: "BCDFGHJK".into(),
            expires_in_ms: 60_000,
            web_url,
        },
        Request::Ping => Reply::Pong,
        Request::PlaybackRevision { .. } => Reply::PlaybackRevision { revision: 0 },
        // "one-time" stands for a password an admin reset to.
        Request::LoginPassword(login) => Reply::LoggedIn {
            session: SessionId(if login.username == "alice" {
                "stub-session".into()
            } else {
                format!("stub-session-{}", login.username)
            }),
            remembered: false,
            password_change_required: login.password == "one-time",
        },
        Request::ChangePassword(_) => Reply::Done,
        Request::LeaveServer if state.last_admin.load(Ordering::SeqCst) => {
            return Response::Err(Error::LastAdmin);
        }
        Request::LeaveServer => Reply::Done,
        Request::WhoAmI => Reply::WhoAmI(spoolframe_protocol::WhoAmIReply {
            user: UserSummary {
                user_id: format!("local:{username}"),
                display_name: username.clone(),
                username,
                role: Role::Consumer,
                locked: false,
                pin: None,
                portable: None,
            },
            can_leave: !state.last_admin.load(Ordering::SeqCst),
        }),
        Request::Libraries => Reply::Libraries {
            libraries: Vec::new(),
        },
        Request::LookupDeviceSignIn { .. } => Reply::DeviceSignInRequest(DeviceSignInRequest {
            device_name: Some("Living room TV".into()),
            client_name: "spoolframe-tvos".into(),
            platform: spoolframe_types::Platform::Tvos,
            requested_at: 0,
            remember: false,
            requested_from: None,
        }),
        Request::ApproveDeviceSignIn { .. } => Reply::Done,
        _ => {
            return Response::Err(Error::Unsupported {
                request: "not in the stub".into(),
            });
        }
    };
    Response::Ok(reply)
}
