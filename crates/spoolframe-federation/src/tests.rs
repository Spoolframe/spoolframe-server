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
    AssertUser, AssertionRefused, MAX_PAGE_ITEMS, MAX_PAGE_REMOVED, PROTOCOL_VERSION, Paired,
    PairingRefused, PeerHello, PeerRequest, RedeemPairing, RemoteItem, RemoteKind,
    RemoteLibraryKind, SharedLibrary, Standing, SyncLibrary, SyncMode, TokenGranted, TokenScope,
    Unpaired,
};
use spoolframe_protocol::{FEDERATION_ALPN, ServerId, close};

use crate::handler::pages;
use crate::{
    DialError, Dialed, FederationProtocol, Live, Peers, Shares, Strangers, SyncAnswer, Unshared,
};

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

/// Movies, of `ITEMS` films, shared with SB alone; one poster.
struct FakeShares;

const ITEMS: usize = MAX_PAGE_ITEMS * 2 + 5;

fn film(n: usize) -> RemoteItem {
    RemoteItem {
        id: format!("plex:movie:{n}"),
        kind: RemoteKind::Movie,
        title: format!("Film {n}"),
        parent: None,
        grandparent: None,
        parent_title: None,
        grandparent_title: None,
        index: None,
        parent_index: None,
        year: None,
        date: None,
        summary: None,
        content_rating: None,
        duration_ms: None,
        count: None,
        genres: Vec::new(),
        artwork: Vec::new(),
        external_ids: Vec::new(),
        hash: String::new(),
    }
    .hashed()
}

#[async_trait::async_trait]
impl Shares for FakeShares {
    async fn libraries(&self, peer: &ServerId) -> Result<Vec<SharedLibrary>, Unshared> {
        Ok(if peer.as_str() == SB_ID {
            vec![SharedLibrary {
                id: "plex:1".into(),
                title: "Movies".into(),
                kind: RemoteLibraryKind::Movies,
            }]
        } else {
            Vec::new()
        })
    }

    async fn sync(&self, peer: &ServerId, sync: &SyncLibrary) -> Result<SyncAnswer, Unshared> {
        if peer.as_str() != SB_ID || sync.library != "plex:1" {
            return Err(Unshared::NotShared);
        }
        Ok(SyncAnswer {
            mode: SyncMode::Snapshot,
            cursor: "c1".into(),
            items: (0..ITEMS).map(film).collect(),
            removed: Vec::new(),
        })
    }

    async fn artwork(&self, peer: &ServerId, id: &str) -> Result<(String, Vec<u8>), Unshared> {
        match (peer.as_str(), id) {
            (SB_ID, "poster") => Ok(("image/png".into(), vec![7; 2 << 20])),
            (SB_ID, _) => Err(Unshared::NotFound),
            _ => Err(Unshared::NotShared),
        }
    }

    /// Film 1 is shared with SB; nothing else, nor with anyone else.
    async fn assert_user(
        &self,
        peer: &ServerId,
        assertion: &AssertUser,
    ) -> Result<TokenGranted, AssertionRefused> {
        if peer.as_str() != SB_ID || assertion.item != "plex:movie:1" {
            return Err(AssertionRefused::NotShared);
        }
        Ok(TokenGranted {
            token: format!("token for {}", assertion.user),
            scope: assertion.scope,
            expires_at: 300,
        })
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
            Arc::new(FakeShares),
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

/// A reply is read within its request's bound: a whole frame for the
/// libraries, a request's 16 KiB for the rest, a hello's above all, which
/// a server not paired yet answers.
#[tokio::test]
async fn a_reply_is_read_within_its_request_s_bound() {
    use spoolframe_protocol::codec::{read_frame_within, write_frame};
    use spoolframe_protocol::federation::{MAX_REQUEST, PeerReply};
    let big = endpoint(vec![FEDERATION_ALPN.to_vec()]).await;
    let answering = big.clone();
    tokio::spawn(async move {
        let connection = answering.accept().await.unwrap().await.unwrap();
        while let Ok((mut send, mut recv)) = connection.accept_bi().await {
            let _: PeerRequest = read_frame_within(&mut recv, MAX_REQUEST).await.unwrap();
            // Past a request's bound, within a frame's.
            let libraries = (0..200)
                .map(|n| SharedLibrary {
                    id: format!("plex:{n}"),
                    title: "x".repeat(200),
                    kind: RemoteLibraryKind::Movies,
                })
                .collect();
            write_frame(&mut send, &PeerReply::Libraries { libraries })
                .await
                .unwrap();
            send.finish().unwrap();
        }
    });
    let sb = endpoint(vec![]).await;
    let connection = sb.connect(big.addr(), FEDERATION_ALPN).await.unwrap();
    let dialed = Dialed::for_tests(connection);
    assert!(matches!(
        dialed.ask(&PeerRequest::Hello(hello(SB_ID))).await,
        Err(DialError::Broken(_))
    ));
    assert!(!dialed.libraries().await.unwrap().is_empty());
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

/// At most MAX_STREAMS streams of a connection are answered at once
/// (H2): one past them is reset, and not counted; once one ends, the next
/// is answered.
#[tokio::test]
async fn a_connection_s_streams_past_the_limit_are_reset() {
    let sa = Sa::start().await;
    let sb = endpoint(Vec::new()).await;
    sa.peers
        .paired
        .lock()
        .insert(sb.id(), ServerId::parse(SB_ID).unwrap());
    let (dialed, _) = dial(&sa, &sb, SB_ID).await;
    // Streams that never finish their request: each holds its place for
    // the request's wait.
    let mut held = Vec::new();
    for _ in 0..crate::handler::MAX_STREAMS {
        let (mut send, recv) = dialed.connection().open_bi().await.unwrap();
        send.write_all(&[0]).await.unwrap();
        held.push((send, recv));
    }
    assert!(matches!(
        dialed.libraries().await,
        Err(DialError::Broken(_))
    ));
    assert_eq!(sa.strays(), 0);
    for (mut send, _) in held {
        send.reset(0u32.into()).unwrap();
    }
    let answered = async {
        loop {
            if dialed.libraries().await.is_ok() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), answered)
        .await
        .expect("answered once the held streams end");
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

/// A live pair replicates: what is shared, a library over several pages
/// on one stream, and an image larger than a frame. What is not shared
/// is refused as such.
#[tokio::test]
async fn a_paired_server_replicates_what_is_shared_with_it() {
    let sa = Sa::start().await;
    let sb = endpoint(Vec::new()).await;
    sa.peers
        .paired
        .lock()
        .insert(sb.id(), ServerId::parse(SB_ID).unwrap());
    let (dialed, _) = dial(&sa, &sb, SB_ID).await;
    let libraries = dialed.libraries().await.unwrap();
    assert_eq!(libraries[0].id, "plex:1");

    let mut pages = dialed
        .sync(SyncLibrary {
            library: "plex:1".into(),
            since: None,
        })
        .await
        .unwrap();
    let mut items = Vec::new();
    let mut count = 0;
    while let Some(page) = pages.next().await.unwrap() {
        assert_eq!(
            (page.mode, page.cursor.as_str()),
            (SyncMode::Snapshot, "c1")
        );
        items.extend(page.items);
        count += 1;
    }
    assert_eq!(count, 3);
    assert_eq!(items, (0..ITEMS).map(film).collect::<Vec<_>>());

    let (content_type, bytes) = dialed.artwork("poster").await.unwrap();
    assert_eq!((content_type.as_str(), bytes.len()), ("image/png", 2 << 20));
    assert!(matches!(
        dialed.artwork("other").await,
        Err(DialError::NotFound)
    ));
    let not_shared = dialed
        .sync(SyncLibrary {
            library: "plex:2".into(),
            since: None,
        })
        .await
        .unwrap()
        .next()
        .await;
    assert!(
        matches!(not_shared, Err(DialError::NotShared)),
        "{not_shared:?}"
    );
    assert_eq!(sa.strays(), 0);
}

fn assertion(item: &str) -> AssertUser {
    AssertUser {
        user: "0192f1c4-5b6e-7a3d-9c41-2e8f7b6a5d4c".into(),
        display_name: "Bob".into(),
        client_endpoint: "ab".repeat(32),
        item: item.into(),
        scope: TokenScope::Play,
    }
}

/// P8-03: a paired server's assertion is answered with a token, or why
/// not; a stranger's is reset and counted.
#[tokio::test]
async fn a_paired_server_s_assertion_gets_a_token_and_a_stranger_s_is_reset() {
    let sa = Sa::start().await;
    let sb = endpoint(Vec::new()).await;
    let (stranger, _) = dial(&sa, &sb, SB_ID).await;
    assert!(matches!(
        stranger.assert_user(assertion("plex:movie:1")).await,
        Err(DialError::Broken(_))
    ));
    assert_eq!(sa.strays(), 1);
    sa.peers
        .paired
        .lock()
        .insert(sb.id(), ServerId::parse(SB_ID).unwrap());
    let (dialed, _) = dial(&sa, &sb, SB_ID).await;
    let granted = dialed.assert_user(assertion("plex:movie:1")).await.unwrap();
    assert_eq!(
        (granted.token.as_str(), granted.scope),
        (
            "token for 0192f1c4-5b6e-7a3d-9c41-2e8f7b6a5d4c",
            TokenScope::Play
        )
    );
    assert!(matches!(
        dialed.assert_user(assertion("plex:movie:2")).await,
        Err(DialError::NotGranted(AssertionRefused::NotShared))
    ));
    assert_eq!(sa.strays(), 1);
}

/// §28 for replication: a stranger is reset and counted, whatever it
/// asks of it.
#[tokio::test]
async fn a_stranger_is_reset_on_replication() {
    let sa = Sa::start().await;
    let sb = endpoint(Vec::new()).await;
    let (dialed, _) = dial(&sa, &sb, SB_ID).await;
    assert!(matches!(
        dialed.libraries().await,
        Err(DialError::Broken(_))
    ));
    let mut pages = dialed
        .sync(SyncLibrary {
            library: "plex:1".into(),
            since: None,
        })
        .await
        .unwrap();
    assert!(matches!(pages.next().await, Err(DialError::Broken(_))));
    assert!(matches!(
        dialed.artwork("poster").await,
        Err(DialError::Broken(_))
    ));
    assert_eq!(sa.strays(), 3);
}

/// Pages hold at most so many items, then the removed ids; the last says
/// so; an empty answer is one empty page.
#[test]
fn an_answer_is_paged_items_first_then_removals() {
    let answer = |items: usize, removed: usize| SyncAnswer {
        mode: SyncMode::Changes,
        cursor: "c".into(),
        items: (0..items).map(film).collect(),
        removed: (0..removed).map(|n| format!("plex:movie:{n}")).collect(),
    };
    let shape = |items, removed| {
        pages(answer(items, removed))
            .iter()
            .map(|p| (p.items.len(), p.removed.len(), p.more))
            .collect::<Vec<_>>()
    };
    assert_eq!(shape(0, 0), [(0, 0, false)]);
    assert_eq!(shape(MAX_PAGE_ITEMS, 0), [(MAX_PAGE_ITEMS, 0, false)]);
    assert_eq!(
        shape(MAX_PAGE_ITEMS + 1, MAX_PAGE_REMOVED + 1),
        [
            (MAX_PAGE_ITEMS, 0, true),
            (1, MAX_PAGE_REMOVED, true),
            (0, 1, false)
        ]
    );
}
