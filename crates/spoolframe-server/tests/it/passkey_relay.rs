//! PX-09 finding 2 (A-3, C-1): a passkey ceremony could be relayed. A
//! malicious server M begins a sign-in on the real server S, hands S's
//! request to a victim's app as its own, and finishes on S with what the
//! app signed: S would sign M in as the victim.
//!
//! Each server has its own relying party since PX-14, named by the server
//! id the server gives in `ServerInfo`, and M gives S's: the app then
//! signs for S's relying party, with S's passkey. Only the challenge,
//! made from the key the app pinned and dialled, which is M's, stops it.
//!
//! M here is a stand-in, not a Spoolframe server: an endpoint that says
//! Hello as itself with S's server id, relays `BeginPasskeySignIn` to S
//! from its own endpoint and keeps the `FinishPasskeySignIn` it is sent.

use std::sync::Arc;

use parking_lot::Mutex;

use iroh::endpoint::{Connection, RecvStream, SendStream, presets};
use iroh::{Endpoint, EndpointId, RelayMode};
use spoolframe_client_sdk::{Client, MemoryStorage, SdkError};
use spoolframe_protocol::codec::{decode, read_frame_bytes, write_reply};
use spoolframe_protocol::rpc::{RpcRequest, RpcResponse, ServerInstance};
use spoolframe_protocol::streams::bi;
use spoolframe_protocol::{
    AuthMethod, CLIENT_ALPN, Error, FinishPasskeySignIn, PROTOCOL_VERSION, Reply, Request,
    Response, ServerInfo,
};
use tokio::io::AsyncReadExt;

use crate::app_passkeys::App;
use crate::support::{TestClient, TestServer, mkv_capabilities};

/// The malicious server.
struct Relay {
    endpoint: Endpoint,
    /// M's own connection to S, from M's endpoint.
    upstream: Arc<TestClient>,
    /// What the victim's app signed, as M was sent it.
    kept: Arc<Mutex<Option<FinishPasskeySignIn>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Relay {
    async fn start(target: &TestServer) -> Self {
        let endpoint = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .alpns(vec![CLIENT_ALPN.to_vec()])
            .bind()
            .await
            .unwrap();
        let conn = endpoint
            .connect(target.server.addr(), CLIENT_ALPN)
            .await
            .unwrap();
        let upstream = Arc::new(TestClient {
            endpoint: endpoint.clone(),
            conn,
            instance: Arc::default(),
            controls: Arc::default(),
        });
        upstream.hello().await;
        let kept = Arc::default();
        let task = tokio::spawn(accept(
            endpoint.clone(),
            target.server_id.to_string(),
            upstream.clone(),
            Arc::clone(&kept),
        ));
        Self {
            endpoint,
            upstream,
            kept,
            task,
        }
    }

    fn id(&self) -> String {
        self.endpoint.id().to_string()
    }

    async fn stop(self) {
        self.task.abort();
        self.endpoint.close().await;
    }
}

async fn accept(
    endpoint: Endpoint,
    server_id: String,
    upstream: Arc<TestClient>,
    kept: Arc<Mutex<Option<FinishPasskeySignIn>>>,
) {
    while let Some(incoming) = endpoint.accept().await {
        let Ok(conn) = incoming.await else {
            continue;
        };
        tokio::spawn(serve(
            conn,
            endpoint.id(),
            server_id.clone(),
            upstream.clone(),
            Arc::clone(&kept),
        ));
    }
}

async fn serve(
    conn: Connection,
    id: EndpointId,
    server_id: String,
    upstream: Arc<TestClient>,
    kept: Arc<Mutex<Option<FinishPasskeySignIn>>>,
) {
    while let Ok((send, recv)) = conn.accept_bi().await {
        tokio::spawn(stream(
            id,
            server_id.clone(),
            upstream.clone(),
            Arc::clone(&kept),
            send,
            recv,
        ));
    }
}

async fn stream(
    id: EndpointId,
    server_id: String,
    upstream: Arc<TestClient>,
    kept: Arc<Mutex<Option<FinishPasskeySignIn>>>,
    mut send: SendStream,
    mut recv: RecvStream,
) {
    if recv.read_u8().await.ok() != Some(bi::RPC) {
        return;
    }
    let Ok(bytes) = read_frame_bytes(&mut recv).await else {
        return;
    };
    let wire: RpcRequest = decode(&bytes).unwrap();
    let response = match wire.envelope.request {
        Request::Hello(_) => Response::Ok(Reply::ServerInfo(ServerInfo {
            name: "Home Media".into(),
            endpoint_id: id.to_string(),
            // S's, not one of its own.
            server_id,
            protocol_version: PROTOCOL_VERSION,
            server_version: "0".into(),
            auth_methods: vec![AuthMethod::Password],
            web_url: None,
            experimental: Vec::new(),
        })),
        Request::Ping => Response::Ok(Reply::Pong),
        // S's own answer, as M's.
        begin @ Request::BeginPasskeySignIn { .. } => upstream.call(None, begin).await,
        Request::FinishPasskeySignIn { signed, .. } => {
            *kept.lock() = Some(signed);
            Response::Err(Error::InvalidCredentials)
        }
        _ => Response::Err(Error::Unsupported {
            request: "not in the relay".into(),
        }),
    };
    let response = RpcResponse {
        server: ServerInstance {
            id: id.to_string(),
            time_ms: 1_000_000,
        },
        reply: response.into(),
    };
    if write_reply(&mut send, &response).await.is_ok() {
        let _ = send.finish();
    }
    let _ = send.stopped().await;
}

fn device() -> spoolframe_client_sdk::Device {
    spoolframe_client_sdk::Device {
        name: "Alice's iPhone".into(),
        client_name: "sdk-test".into(),
        client_version: "0".into(),
        platform: spoolframe_client_sdk::types::Platform::Ios,
        capabilities: mkv_capabilities(),
    }
}

/// A client that knows where `id` is, at `addrs`, and has pinned it.
async fn pinned(id: &str, addrs: Vec<std::net::SocketAddr>) -> Client {
    let client = crate::support::sdk_client(Arc::new(MemoryStorage::default()), device())
        .await
        .unwrap();
    client.add_address(id, addrs).unwrap();
    client.connect(id).await.unwrap();
    client.pin(id, true).await.unwrap();
    client
}

/// Alice's app has a passkey on S. Connected to M, it signs what M relays
/// from S; S must not sign M in with it.
#[tokio::test]
async fn a_passkey_sign_in_relayed_from_another_server_signs_no_one_in() {
    let target = TestServer::start("passkey-relay").await;
    target
        .add_user("alice", "correct horse", Some("Alice"))
        .await;
    let s = target.server.id().to_string();
    let addrs = |endpoint: &Endpoint| endpoint.addr().ip_addrs().copied().collect();

    // Her passkey on S, made through the SDK as the app makes one.
    let phone = pinned(&s, target.server.addr().ip_addrs().copied().collect()).await;
    let alice = phone
        .sign_in(&s, "alice", "correct horse", false)
        .await
        .unwrap();
    let creation = alice.begin_passkey_registration().await.unwrap();
    let mut app = App::new();
    alice
        .finish_passkey_registration(app.create(&creation, "Alice's iPhone"))
        .await
        .unwrap();

    // The app, on M.
    let relay = Relay::start(&target).await;
    let victim = pinned(&relay.id(), addrs(&relay.endpoint)).await;
    let request = victim
        .begin_passkey_sign_in(&relay.id(), Some("alice"))
        .await
        .unwrap();
    // S's relying party, from the id M claimed: the app finds S's passkey.
    assert_eq!(request.rp_id, target.server_id.rp_id());
    let signed = app.get(&request);
    assert_eq!(
        victim
            .finish_passkey_sign_in(&relay.id(), signed, false)
            .await
            .unwrap_err(),
        SdkError::InvalidCredentials
    );
    let relayed = relay.kept.lock().take().expect("M kept it");

    // M finishes on S, from the endpoint that began it.
    assert_eq!(
        relay
            .upstream
            .call(
                None,
                Request::FinishPasskeySignIn {
                    signed: relayed,
                    remember: false,
                }
            )
            .await,
        Response::Err(Error::InvalidCredentials)
    );
    victim.close().await;
    phone.close().await;
    relay.stop().await;
    target.stop().await;
}
