//! F8-01 over the wire: a paired server that lies about a read. SA here is
//! a stand-in on its own endpoint: its federation protocol is the real
//! handler over fakes, and its client protocol answers a proxied play as a
//! server would, but answers a read with a length far past what was asked,
//! and sends none of it. SB, a whole server, plays its film through itself
//! to a device over QUIC and to a player over HTTP.

use spoolframe_federation::{
    FederationProtocol, Live, Peers, Shares, Strangers, SyncAnswer, Unshared,
};
use spoolframe_protocol::codec::{read_frame, write_frame, write_reply};
use spoolframe_protocol::federation::PlayTokenRedeemed;
use spoolframe_protocol::federation::{
    AssertUser, AssertionRefused, RemoteItem, RemoteKind, RemoteLibraryKind, SharedLibrary,
    SyncLibrary, SyncMode, TokenGranted, TokenScope,
};
use spoolframe_protocol::media::MediaReadResponse;
use spoolframe_protocol::resource::{ResourceMetadata, ResourceRequest, ResourceResponse};
use spoolframe_protocol::rpc::{RpcReply, RpcRequest, RpcResponse, ServerInstance};
use spoolframe_protocol::streams::bi;
use spoolframe_protocol::{
    PlaybackHandle, PlaybackPlan, Reply, Selection, ServerId, ServerInfo, SessionId,
};
use spoolframe_server::store::federation::NewPeer;
use spoolframe_types::Container;
use tokio::io::AsyncReadExt;

use super::*;
use crate::support::mkv_capabilities;

const FEDERATION: &str = spoolframe_protocol::experimental::FEDERATION;

/// The stand-in SA's server id.
const SA_ID: &str = "llllllllllllllll";

/// What SA says a read is: a TiB, of a file of a PiB.
const LIE: u64 = 1 << 40;

/// Pairs with whoever dials it.
struct AnyPeer;

#[async_trait::async_trait]
impl Peers for AnyPeer {
    async fn known(&self, _endpoint: iroh::EndpointId) -> Option<ServerId> {
        Some(ServerId::parse("bbbbbbbbbbbbbbbb").unwrap())
    }

    async fn hello(
        &self,
        _endpoint: iroh::EndpointId,
        _known: Option<&ServerId>,
        _server_id: &ServerId,
    ) -> Result<Standing, PairingRefused> {
        Ok(Standing::Paired)
    }

    async fn redeem(
        &self,
        _endpoint: iroh::EndpointId,
        _server_id: &ServerId,
        _hello: &PeerHello,
        _redeem: &RedeemPairing,
    ) -> Result<(), PairingRefused> {
        Err(PairingRefused::InviteInvalid)
    }

    async fn unpaired(&self, _endpoint: iroh::EndpointId) {}
}

struct Room;

impl Strangers for Room {
    type Held = ();

    fn admit(&self, _connection: &Connection, _peer: Option<&ServerId>) -> Option<()> {
        Some(())
    }

    fn strayed(&self, _endpoint: iroh::EndpointId) {}
}

/// Movies, of one film, Alien; a token for every assertion.
struct OneFilm;

#[async_trait::async_trait]
impl Shares for OneFilm {
    async fn libraries(
        &self,
        _peer: &ServerId,
        _endpoint: iroh::EndpointId,
    ) -> Result<Vec<SharedLibrary>, Unshared> {
        Ok(vec![SharedLibrary {
            id: "plex:1".into(),
            title: "Movies".into(),
            kind: RemoteLibraryKind::Movies,
        }])
    }

    async fn sync(
        &self,
        _peer: &ServerId,
        _endpoint: iroh::EndpointId,
        _sync: &SyncLibrary,
    ) -> Result<SyncAnswer, Unshared> {
        let alien = RemoteItem {
            id: "plex:movie:1".into(),
            kind: RemoteKind::Movie,
            title: "Alien".into(),
            parent: None,
            grandparent: None,
            parent_title: None,
            grandparent_title: None,
            index: None,
            parent_index: None,
            year: Some(1979),
            date: None,
            summary: None,
            content_rating: None,
            duration_ms: Some(7_000_000),
            count: None,
            genres: Vec::new(),
            artwork: Vec::new(),
            external_ids: Vec::new(),
            hash: String::new(),
        }
        .hashed();
        Ok(SyncAnswer {
            mode: SyncMode::Snapshot,
            cursor: "c".into(),
            items: std::sync::Arc::new(vec![alien]),
            removed: Vec::new(),
        })
    }

    async fn artwork(
        &self,
        _peer: &ServerId,
        _endpoint: iroh::EndpointId,
        _id: &str,
    ) -> Result<(String, Vec<u8>), Unshared> {
        Err(Unshared::NotFound)
    }

    async fn assert_user(
        &self,
        _peer: &ServerId,
        _endpoint: iroh::EndpointId,
        assertion: &AssertUser,
    ) -> Result<TokenGranted, AssertionRefused> {
        Ok(TokenGranted {
            token: "a token".into(),
            scope: assertion.scope,
            expires_at: i64::MAX / 2,
        })
    }
}

/// The stand-in SA's client protocol: a play as a server answers one,
/// and every read answered [`LIE`], its bytes never sent. How many reads
/// it was asked, and how many requests in all.
#[derive(Debug, Clone, Default)]
struct Lying {
    reads: Arc<std::sync::atomic::AtomicUsize>,
    asked: Arc<std::sync::atomic::AtomicUsize>,
}

impl Lying {
    fn answer(&self, request: Request) -> Response {
        self.asked.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Response::Ok(match request {
            Request::Hello(_) => Reply::ServerInfo(ServerInfo {
                name: "Alice's server".into(),
                endpoint_id: String::new(),
                server_id: SA_ID.into(),
                protocol_version: spoolframe_protocol::PROTOCOL_VERSION,
                server_version: "0.0.0".into(),
                auth_methods: Vec::new(),
                web_url: None,
                experimental: vec![FEDERATION.into()],
                features: Vec::new(),
                invite_only: true,
            }),
            Request::RedeemPlayToken(_) => Reply::PlayTokenRedeemed(PlayTokenRedeemed {
                session: SessionId("sa's session".into()),
                item: "plex:movie:1".parse().unwrap(),
                scope: TokenScope::Play,
            }),
            Request::StartPlayback { .. } => Reply::PlaybackStarted {
                handle: PlaybackHandle("sa's playback".into()),
                plan: PlaybackPlan::Direct {
                    container: Container::Matroska,
                    video: None,
                    audio: None,
                    subtitle: None,
                },
                size: 1 << 20,
                chosen: None,
                epoch: 0,
                markers: Vec::new(),
            },
            Request::Ping => Reply::Pong,
            _ => Reply::Done,
        })
    }
}

impl iroh::protocol::ProtocolHandler for Lying {
    async fn accept(&self, connection: Connection) -> Result<(), iroh::protocol::AcceptError> {
        while let Ok((mut send, mut recv)) = connection.accept_bi().await {
            let this = self.clone();
            tokio::spawn(async move {
                match recv.read_u8().await {
                    Ok(bi::RPC) => {
                        let Ok(wire) = read_frame::<_, RpcRequest>(&mut recv).await else {
                            return;
                        };
                        let response = RpcResponse {
                            server: ServerInstance {
                                id: "sa".into(),
                                time_ms: 0,
                            },
                            reply: RpcReply::from(this.answer(wire.envelope.request)),
                        };
                        let _ = write_reply(&mut send, &response).await;
                        let _ = send.finish();
                    }
                    Ok(bi::RESOURCE) => {
                        let Ok(ResourceRequest::Media { .. }) = read_frame(&mut recv).await else {
                            return;
                        };
                        this.reads.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                        let ready = ResourceResponse::Ready {
                            length: LIE,
                            metadata: ResourceMetadata::Media {
                                size: LIE << 10,
                                complete: true,
                                epoch: 0,
                                revision: 0,
                            },
                        };
                        let _ = write_frame(&mut send, &ready).await;
                        // None of it: the stream stays open until SB lets
                        // it go.
                        let _ = send.stopped().await;
                    }
                    _ => {}
                }
            });
        }
        Ok(())
    }
}

/// The stand-in SA on its own endpoint, and what its client side saw.
async fn lying_sa() -> (iroh::protocol::Router, Lying) {
    let lying = Lying::default();
    let federation = FederationProtocol::new(
        Arc::new(AnyPeer),
        Arc::new(Room),
        Arc::new(OneFilm),
        ServerId::parse(SA_ID).unwrap(),
        Arc::new(|| "Alice's server".to_owned()),
        Live::default(),
    );
    let endpoint = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .alpns(vec![FEDERATION_ALPN.to_vec(), CLIENT_ALPN.to_vec()])
        .bind()
        .await
        .unwrap();
    let router = iroh::protocol::Router::builder(endpoint)
        .accept(FEDERATION_ALPN, federation)
        .accept(CLIENT_ALPN, lying.clone())
        .spawn();
    (router, lying)
}

/// SB, with HTTP playback on, paired with the stand-in SA and synced:
/// Bob is a user of SB's.
async fn sb_paired_with(test: &str, sa: &iroh::protocol::Router) -> TestServer {
    let (sb, _) = sb_on(
        fresh_dir(test).await,
        ServerOptions {
            http_playback: spoolframe_server::http_playback::Settings {
                listen: Some("0.0.0.0:0".parse().unwrap()),
                ..Default::default()
            },
            ..ServerOptions::default()
        },
        quiet_plex().await,
    )
    .await;
    let addrs: Vec<String> = sa
        .endpoint()
        .addr()
        .ip_addrs()
        .map(|addr| addr.to_string())
        .collect();
    let peer = sb
        .store
        .add_peer(
            NewPeer {
                server_id: SA_ID,
                endpoint_id: &sa.endpoint().id().to_string(),
                display_name: "Alice's server",
                addrs: &addrs,
                relays: &[],
            },
            "bob",
        )
        .await
        .unwrap();
    federation(&sb).refresh(&peer.id).await.unwrap();
    sb.add_user("bob", "correct horse", None).await;
    sb
}

/// SA's film, started on SB by a device of Bob's naming `features`: SB
/// plays it from the stand-in SA. The device, its session, the handle.
async fn bob_plays(sb: &TestServer, features: &[&str]) -> (TestClient, SessionId, PlaybackHandle) {
    let device = sb.loopback_client().await;
    device.hello_naming(mkv_capabilities(), features).await;
    let bob = device.login("bob", "correct horse").await;
    let alien = MediaId::federated(SA_ID, &"plex:movie:1".parse().unwrap()).unwrap();
    let start = Request::StartPlayback {
        id: alien,
        selection: Some(Selection::default()),
        start_ms: None,
        bitmap: Default::default(),
        quality: Default::default(),
        display: None,
        capabilities: None,
        onward_share: None,
    };
    let handle = match device.call(Some(&bob), start).await {
        Response::Ok(Reply::PlaybackStarted { handle, .. }) => handle,
        other => panic!("did not start through SB: {other:?}"),
    };
    (device, bob, handle)
}

/// Both servers still answer: SB a device's ping, the stand-in SA SB's
/// sync of it.
async fn both_answer(sb: &TestServer, device: &TestClient) {
    assert_eq!(
        device.call(None, Request::Ping).await,
        Response::Ok(Reply::Pong)
    );
    let sa = federation(sb)
        .live_peer_by_server_id(SA_ID)
        .await
        .unwrap()
        .unwrap();
    federation(sb).refresh(&sa.id).await.unwrap();
}

/// F8-01, over QUIC: SA's answer of a TiB to a 4 KiB read fails the
/// device's read at once; the device is never told the length.
#[tokio::test]
async fn a_paired_server_s_lying_read_is_refused_over_quic() {
    let (sa, lying) = lying_sa().await;
    let sb = sb_paired_with("fed-lying-quic", &sa).await;
    let (device, bob, handle) = bob_plays(&sb, &[FEDERATION]).await;
    let read = tokio::time::timeout(Duration::from_secs(10), device.read(&bob, &handle, 0, 4096))
        .await
        .expect("refused at once, not after a stall");
    assert!(
        matches!(read, (MediaReadResponse::Err(_), _)),
        "{:?}",
        read.0
    );
    assert_eq!(
        lying.reads.load(std::sync::atomic::Ordering::Acquire),
        1,
        "SA was asked"
    );
    both_answer(&sb, &device).await;
    sb.stop().await;
    sa.shutdown().await.unwrap();
}

/// F8-01, over HTTP: the player's range fails at once, with no length of
/// SA's said, and SB's HTTP port still answers.
#[tokio::test]
async fn a_paired_server_s_lying_read_is_refused_over_http() {
    use spoolframe_protocol::experimental::HTTP_PLAYBACK;
    let (sa, lying) = lying_sa().await;
    let sb = sb_paired_with("fed-lying-http", &sa).await;
    let (device, bob, handle) = bob_plays(&sb, &[FEDERATION, HTTP_PLAYBACK]).await;
    let url = match device
        .call(Some(&bob), Request::HttpPlaybackUrl { handle })
        .await
    {
        Response::Ok(Reply::HttpPlaybackUrl { url }) => url,
        other => panic!("no URL: {other:?}"),
    };
    let http = reqwest::Client::new();
    let response = tokio::time::timeout(
        Duration::from_secs(10),
        http.get(&url).header("range", "bytes=0-4095").send(),
    )
    .await
    .expect("refused at once, not after a stall")
    .unwrap();
    // The headers are SB's own, from the range and the size SA gave at
    // the start; none of SA's lie is in them. The body fails, and at
    // once, as SA's answer was refused.
    let length = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    assert!(length.is_none_or(|length| length <= 4096), "{length:?}");
    let body = tokio::time::timeout(Duration::from_secs(10), response.bytes())
        .await
        .expect("refused at once, not after a stall");
    assert!(
        body.as_ref().is_err() || body.as_ref().is_ok_and(|b| b.len() < 4096),
        "{body:?}"
    );
    assert_eq!(lying.reads.load(std::sync::atomic::Ordering::Acquire), 1);
    // The port answers the next player, as it would any.
    tokio::time::timeout(Duration::from_secs(10), http.get(&url).send())
        .await
        .expect("answered")
        .unwrap();
    both_answer(&sb, &device).await;
    sb.stop().await;
    sa.shutdown().await.unwrap();
}
