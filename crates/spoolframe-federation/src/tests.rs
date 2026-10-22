//! The pairing state machine over a fake store, between two endpoints on
//! loopback: what each standing may ask, and what a refusal does to the
//! stream, the connection and the stranger's count. The server's own
//! tests run the same dance between two whole servers.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use iroh::endpoint::{Connection, presets};
use iroh::protocol::Router;
use iroh::{Endpoint, EndpointId, RelayMode};
use spoolframe_protocol::federation::{
    PROTOCOL_VERSION, Paired, PairingRefused, PeerHello, PeerRequest, RedeemPairing, Standing,
    Unpaired,
};
use spoolframe_protocol::{FEDERATION_ALPN, ServerId, close};

use crate::{DialError, Dialed, FederationProtocol, Live, Peers, Strangers};

const SA_ID: &str = "aaaaaaaaaaaaaaaa";
const SB_ID: &str = "mzxw6ytbojtg633c";
const CODE: &str = "QW7K-2MFP-XR4T";

/// Pairs by endpoint, the one code that pairs, and what was asked of it.
#[derive(Default)]
struct FakePeers {
    paired: parking_lot::Mutex<HashMap<EndpointId, ServerId>>,
    held: parking_lot::Mutex<HashSet<EndpointId>>,
    unpaired: parking_lot::Mutex<Vec<EndpointId>>,
}

#[async_trait::async_trait]
impl Peers for FakePeers {
    async fn known(&self, endpoint: EndpointId) -> Option<ServerId> {
        self.paired.lock().get(&endpoint).cloned()
    }

    async fn hello(
        &self,
        endpoint: EndpointId,
        known: Option<&ServerId>,
        server_id: &ServerId,
    ) -> Result<Standing, PairingRefused> {
        let paired = self.paired.lock();
        match known {
            Some(pinned) if pinned == server_id => Ok(Standing::Paired),
            Some(_) => Err(PairingRefused::ServerIdChanged),
            None if paired.values().any(|id| id == server_id) => {
                self.held.lock().insert(endpoint);
                Ok(Standing::KeyHeld)
            }
            None => Ok(Standing::Stranger),
        }
    }

    async fn redeem(
        &self,
        endpoint: EndpointId,
        server_id: &ServerId,
        _hello: &PeerHello,
        redeem: &RedeemPairing,
    ) -> Result<(), PairingRefused> {
        if redeem.code != CODE {
            return Err(PairingRefused::InviteInvalid);
        }
        self.paired.lock().insert(endpoint, server_id.clone());
        Ok(())
    }

    async fn unpaired(&self, endpoint: EndpointId) {
        self.paired.lock().remove(&endpoint);
        self.unpaired.lock().push(endpoint);
    }
}

/// Room for everyone, unless told otherwise; strays counted.
#[derive(Default)]
struct FakeStrangers {
    full: std::sync::atomic::AtomicBool,
    strays: AtomicUsize,
}

impl Strangers for FakeStrangers {
    type Held = ();

    fn admit(&self, _connection: &Connection, peer: Option<&ServerId>) -> Option<()> {
        (peer.is_some() || !self.full.load(Ordering::Acquire)).then_some(())
    }

    fn strayed(&self, _endpoint: EndpointId) {
        self.strays.fetch_add(1, Ordering::AcqRel);
    }
}

async fn endpoint(alpns: Vec<Vec<u8>>) -> Endpoint {
    Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .alpns(alpns)
        .bind()
        .await
        .unwrap()
}

/// Server A on its own endpoint, and the fakes behind it.
struct Sa {
    router: Router,
    peers: Arc<FakePeers>,
    strangers: Arc<FakeStrangers>,
    live: Live,
}

impl Sa {
    async fn start() -> Self {
        let peers = Arc::new(FakePeers::default());
        let strangers = Arc::new(FakeStrangers::default());
        let live = Live::default();
        let handler = FederationProtocol::new(
            peers.clone(),
            strangers.clone(),
            ServerId::parse(SA_ID).unwrap(),
            Arc::new(|| "Alice's server".to_owned()),
            live.clone(),
        );
        let router = Router::builder(endpoint(vec![FEDERATION_ALPN.to_vec()]).await)
            .accept(FEDERATION_ALPN, handler)
            .spawn();
        Self {
            router,
            peers,
            strangers,
            live,
        }
    }

    fn strays(&self) -> usize {
        self.strangers.strays.load(Ordering::Acquire)
    }
}

fn hello(server_id: &str) -> PeerHello {
    PeerHello {
        protocol_version: PROTOCOL_VERSION,
        server_id: server_id.into(),
        name: "Bob's server".into(),
    }
}

fn redeem(code: &str) -> RedeemPairing {
    RedeemPairing {
        code: code.into(),
        addrs: Vec::new(),
        relays: Vec::new(),
    }
}

/// Server B, on `from`, says hello to A as `server_id`.
async fn dial(sa: &Sa, from: &Endpoint, server_id: &str) -> (Dialed, Standing) {
    let (dialed, info) = Dialed::connect(from, sa.router.endpoint().addr(), hello(server_id))
        .await
        .unwrap();
    assert_eq!(
        (info.server_id.as_str(), info.name.as_str()),
        (SA_ID, "Alice's server")
    );
    (dialed, info.standing)
}

#[tokio::test]
async fn a_stranger_pairs_with_the_right_code_and_only_once() {
    let sa = Sa::start().await;
    let sb = endpoint(Vec::new()).await;
    let (dialed, standing) = dial(&sa, &sb, SB_ID).await;
    assert_eq!(standing, Standing::Stranger);
    assert!(matches!(
        dialed.redeem(redeem("AAAA-AAAA-AAAA")).await,
        Err(DialError::Refused(PairingRefused::InviteInvalid))
    ));
    assert_eq!(
        dialed.redeem(redeem(CODE)).await.unwrap(),
        Paired {
            server_id: SA_ID.into(),
            name: "Alice's server".into()
        }
    );
    assert!(matches!(
        dialed.redeem(redeem(CODE)).await,
        Err(DialError::Refused(PairingRefused::AlreadyPaired))
    ));
    // A refused code is the store's to count; nothing was reset.
    assert_eq!(sa.strays(), 0);
    // Again, from the same key: paired.
    let (_again, standing) = dial(&sa, &sb, SB_ID).await;
    assert_eq!(standing, Standing::Paired);
}

/// A hello with the dialled server's own id is refused, and pairs
/// nothing: the store, which would call it a stranger, is never asked.
#[tokio::test]
async fn a_hello_with_this_server_s_own_id_is_refused() {
    let sa = Sa::start().await;
    let sb = endpoint(Vec::new()).await;
    assert!(matches!(
        Dialed::connect(&sb, sa.router.endpoint().addr(), hello(SA_ID)).await,
        Err(DialError::Refused(PairingRefused::SameServerId))
    ));
    assert!(sa.peers.paired.lock().is_empty());
}

/// §28: an unknown endpoint reaches only Hello and redemption; anything
/// else resets the stream and is counted.
#[tokio::test]
async fn a_stranger_asking_for_anything_else_is_reset_and_counted() {
    let sa = Sa::start().await;
    let sb = endpoint(Vec::new()).await;
    let (dialed, _) = dial(&sa, &sb, SB_ID).await;
    for request in [
        PeerRequest::Unpaired(Unpaired {}),
        PeerRequest::Unknown,
        PeerRequest::Hello(hello(SB_ID)),
    ] {
        assert!(
            matches!(dialed.ask(&request).await, Err(DialError::Broken(_))),
            "{request:?}"
        );
    }
    assert_eq!(sa.strays(), 3);
    assert!(sa.peers.unpaired.lock().is_empty());
}

#[tokio::test]
async fn nothing_but_hello_is_answered_before_hello() {
    let sa = Sa::start().await;
    let sb = endpoint(vec![]).await;
    let connection = sb
        .connect(sa.router.endpoint().addr(), FEDERATION_ALPN)
        .await
        .unwrap();
    let dialed = Dialed::for_tests(connection);
    assert!(matches!(
        dialed.ask(&PeerRequest::RedeemPairing(redeem(CODE))).await,
        Err(DialError::Broken(_))
    ));
    assert_eq!(sa.strays(), 1);
    assert!(sa.peers.paired.lock().is_empty());
}

/// PX-14's pin pair: a paired key under another server id is refused
/// every time, and its connection closed.
#[tokio::test]
async fn a_paired_key_under_another_server_id_is_refused_every_time() {
    let sa = Sa::start().await;
    let sb = endpoint(Vec::new()).await;
    sa.peers
        .paired
        .lock()
        .insert(sb.id(), ServerId::parse(SB_ID).unwrap());
    for _ in 0..2 {
        let refused =
            Dialed::connect(&sb, sa.router.endpoint().addr(), hello("bbbbbbbbbbbbbbbb")).await;
        assert!(
            matches!(
                refused,
                Err(DialError::Refused(PairingRefused::ServerIdChanged))
            ),
            "{refused:?}"
        );
    }
    let connection = sb
        .connect(sa.router.endpoint().addr(), FEDERATION_ALPN)
        .await
        .unwrap();
    let dialed = Dialed::for_tests(connection.clone());
    let _ = dialed
        .ask(&PeerRequest::Hello(hello("bbbbbbbbbbbbbbbb")))
        .await;
    assert!(matches!(
        connection.closed().await,
        iroh::endpoint::ConnectionError::ApplicationClosed(c)
            if c.error_code == close::SERVER_ID_CHANGED.into()
    ));
}

/// A paired server id from a new key is held, and reaches only Hello.
#[tokio::test]
async fn a_new_key_for_a_paired_server_reaches_only_hello() {
    let sa = Sa::start().await;
    let old = endpoint(Vec::new()).await;
    sa.peers
        .paired
        .lock()
        .insert(old.id(), ServerId::parse(SB_ID).unwrap());
    let new = endpoint(Vec::new()).await;
    let (dialed, standing) = dial(&sa, &new, SB_ID).await;
    assert_eq!(standing, Standing::KeyHeld);
    assert!(sa.peers.held.lock().contains(&new.id()));
    for request in [
        PeerRequest::RedeemPairing(redeem(CODE)),
        PeerRequest::Unpaired(Unpaired {}),
    ] {
        assert!(matches!(
            dialed.ask(&request).await,
            Err(DialError::Broken(_))
        ));
    }
    assert_eq!(sa.strays(), 2);
    assert!(!sa.peers.paired.lock().contains_key(&new.id()));
}

#[tokio::test]
async fn a_paired_server_unpairs_and_its_connection_closes() {
    let sa = Sa::start().await;
    let sb = endpoint(Vec::new()).await;
    sa.peers
        .paired
        .lock()
        .insert(sb.id(), ServerId::parse(SB_ID).unwrap());
    let (dialed, standing) = dial(&sa, &sb, SB_ID).await;
    assert_eq!(standing, Standing::Paired);
    assert!(sa.live.has(&sb.id()));
    // A paired server's request this build does not know is reset, not
    // counted against it.
    assert!(dialed.ask(&PeerRequest::Unknown).await.is_err());
    assert_eq!(sa.strays(), 0);
    dialed.unpaired().await.unwrap();
    assert_eq!(*sa.peers.unpaired.lock(), [sb.id()]);
    let connection = dialed.connection().clone();
    assert!(matches!(
        connection.closed().await,
        iroh::endpoint::ConnectionError::ApplicationClosed(c)
            if c.error_code == close::UNPAIRED.into()
    ));
}

/// The strangers' cap: a stranger is closed as busy; a paired server is
/// let in.
#[tokio::test]
async fn with_no_room_for_strangers_only_a_paired_server_gets_in() {
    let sa = Sa::start().await;
    sa.strangers.full.store(true, Ordering::Release);
    let stranger = endpoint(Vec::new()).await;
    let refused = Dialed::connect(&stranger, sa.router.endpoint().addr(), hello(SB_ID)).await;
    assert!(matches!(refused, Err(DialError::Broken(_))), "{refused:?}");
    let sb = endpoint(Vec::new()).await;
    sa.peers
        .paired
        .lock()
        .insert(sb.id(), ServerId::parse(SB_ID).unwrap());
    let (_dialed, standing) = dial(&sa, &sb, SB_ID).await;
    assert_eq!(standing, Standing::Paired);
}

/// The live list closes a server's connections on demand: unpairing from
/// this side, or a rejected key.
#[tokio::test]
async fn live_closes_a_server_s_connections() {
    let sa = Sa::start().await;
    let sb = endpoint(Vec::new()).await;
    let (dialed, _) = dial(&sa, &sb, SB_ID).await;
    assert_eq!(sa.live.close(&sb.id(), close::UNPAIRED, "unpaired"), 1);
    assert!(matches!(
        dialed.connection().closed().await,
        iroh::endpoint::ConnectionError::ApplicationClosed(c)
            if c.error_code == close::UNPAIRED.into()
    ));
    assert!(!sa.live.has(&sb.id()));
}
