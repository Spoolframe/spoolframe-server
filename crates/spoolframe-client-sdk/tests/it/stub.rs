//! A stand-in server that can stop answering without closing anything:
//! the connection stays up and QUIC keeps acknowledging packets, but no
//! request on it is answered. That is what a client sees when the server
//! side of a connection quietly died (an app suspended by iOS comes back
//! to it), and it is worse than a lost path: nothing in QUIC times it out.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use iroh::endpoint::{Connection, RecvStream, SendStream, presets};
use iroh::{Endpoint, EndpointId, RelayMode, SecretKey};
use spoolframe_protocol::codec::{decode, read_frame_bytes, request_type, write_frame};
use spoolframe_protocol::streams::bi;
use spoolframe_protocol::{
    AuthMethod, CLIENT_ALPN, DeviceSignInRequest, Error, PROTOCOL_VERSION, Reply, Request,
    Response, ServerInfo, SessionId, UserSummary,
};
use spoolframe_types::Role;
use tokio::io::AsyncReadExt;

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
    /// What `ServerAddresses` answers a session with (PX-12).
    told: Mutex<Vec<SocketAddr>>,
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
        for conn in self.state.connections.lock().unwrap().iter() {
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
        self.state.stalled.lock().unwrap().insert(kind.to_owned());
    }

    /// How many connections it has accepted.
    pub fn connections(&self) -> usize {
        self.state.connections.lock().unwrap().len()
    }

    /// How many requests of type `kind` arrived, answered or not.
    pub fn count(&self, kind: &str) -> usize {
        self.requests_of(kind).len()
    }

    /// Answer a session's `ServerAddresses` with `addrs`, whether the stub
    /// is bound there or not; a stranger's is `Unauthenticated`.
    pub fn tell(&self, addrs: &[SocketAddr]) {
        *self.state.told.lock().unwrap() = addrs.to_vec();
    }

    pub fn body(&self, announced: u64, bytes: &[u8]) {
        *self.state.body.lock().unwrap() = (announced, bytes.to_vec());
    }

    pub fn invocations(&self, kind: &str) -> Vec<spoolframe_protocol::rpc::RpcRequest> {
        self.state
            .invocations
            .lock()
            .unwrap()
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
            .unwrap()
            .iter()
            .filter(|(_, k)| k == kind)
            .map(|(conn, _)| *conn)
            .collect()
    }

    pub async fn stop(self) {
        self.task.abort();
        self.endpoint.close().await;
    }
}

async fn accept(endpoint: Endpoint, state: Arc<State>) {
    while let Some(incoming) = endpoint.accept().await {
        let Ok(conn) = incoming.await else {
            continue;
        };
        let dead = Arc::new(AtomicBool::new(false));
        let index = {
            let mut connections = state.connections.lock().unwrap();
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
        let (length, bytes) = state.body.lock().unwrap().clone();
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
    state.requests.lock().unwrap().push((index, kind.clone()));
    state
        .invocations
        .lock()
        .unwrap()
        .push((kind.clone(), wire.clone()));
    let silent = dead.load(Ordering::SeqCst)
        || state.frozen.load(Ordering::SeqCst)
        || state.stalled.lock().unwrap().contains(&kind);
    if silent {
        // The stream stays open and unanswered for as long as the
        // connection lasts.
        conn.closed().await;
        return;
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

fn answer(
    id: EndpointId,
    state: &State,
    envelope: spoolframe_protocol::RequestEnvelope,
) -> Response {
    let reply = match envelope.request {
        Request::ServerAddresses if envelope.session.is_none() => {
            return Response::Err(Error::Unauthenticated);
        }
        Request::ServerAddresses => Reply::ServerAddresses {
            addrs: state.told.lock().unwrap().clone(),
        },
        Request::Hello(_) => Reply::ServerInfo(ServerInfo {
            name: "Stub".into(),
            endpoint_id: id.to_string(),
            protocol_version: PROTOCOL_VERSION,
            server_version: "0".into(),
            auth_methods: vec![AuthMethod::Password],
            web_url: None,
        }),
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
