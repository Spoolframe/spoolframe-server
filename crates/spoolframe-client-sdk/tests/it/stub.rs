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

    /// Say `url` is the server's web UI from now on.
    pub fn web_url(&self, url: &str) {
        *self.state.web_url.lock() = Some(url.to_owned());
    }

    pub fn body(&self, announced: u64, bytes: &[u8]) {
        *self.state.body.lock() = (announced, bytes.to_vec());
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
        if sent.is_ok() && send.write_all(&bytes).await.is_ok() {
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
    };
    let _ = write_frame(&mut *events.lock().await, &event).await;
}

fn answer(
    id: EndpointId,
    state: &State,
    envelope: spoolframe_protocol::RequestEnvelope,
) -> Response {
    let web_url = state.web_url.lock().clone();
    // Made from its key: two stubs are two servers (PX-14).
    let server_id = spoolframe_protocol::ServerId::from_bytes(
        id.as_bytes()[..10].try_into().expect("a key is 32 bytes"),
    )
    .to_string();
    let reply = match envelope.request {
        Request::Hello(_) => Reply::ServerInfo(ServerInfo {
            name: "Stub".into(),
            endpoint_id: id.to_string(),
            server_id,
            protocol_version: PROTOCOL_VERSION,
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
        Request::LoginPassword(_) => Reply::LoggedIn {
            session: SessionId("stub-session".into()),
            remembered: false,
        },
        Request::WhoAmI => Reply::WhoAmI(UserSummary {
            user_id: "local:1".into(),
            username: "alice".into(),
            display_name: "Alice".into(),
            role: Role::Consumer,
            locked: false,
            pin: None,
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
