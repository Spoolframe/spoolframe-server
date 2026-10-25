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

/// Movies, of `ITEMS` films, shared with SB alone; one poster. Its syncs
/// wait while a test holds them (X8-02).
#[derive(Default)]
struct FakeShares {
    held: Held,
}

/// Where a test holds the fake's syncs: how many are at it now, how many
/// ran to their end, and what lets them go on.
struct Held {
    holding: std::sync::atomic::AtomicBool,
    at_it: tokio::sync::watch::Sender<usize>,
    go_on: tokio::sync::Notify,
    done: AtomicUsize,
}

impl Default for Held {
    fn default() -> Self {
        Self {
            holding: std::sync::atomic::AtomicBool::new(false),
            at_it: tokio::sync::watch::Sender::new(0),
            go_on: tokio::sync::Notify::new(),
            done: AtomicUsize::new(0),
        }
    }
}

impl Held {
    /// A sync waits here while syncs are held, counted among those at it
    /// until it goes on or is dropped.
    async fn wait(&self) {
        if !self.holding.load(Ordering::Acquire) {
            return;
        }
        struct Leaving<'a>(&'a Held);
        impl Drop for Leaving<'_> {
            fn drop(&mut self) {
                self.0.at_it.send_modify(|n| *n -= 1);
            }
        }
        self.at_it.send_modify(|n| *n += 1);
        let _leaving = Leaving(self);
        self.go_on.notified().await;
        self.done.fetch_add(1, Ordering::AcqRel);
    }

    /// Until `n` syncs are at it, within 5 s.
    async fn until_at_it(&self, n: usize) {
        let mut at_it = self.at_it.subscribe();
        let waited = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            at_it.wait_for(|at| *at == n),
        )
        .await;
        assert!(
            waited.is_ok(),
            "{} syncs at it, not {n}",
            *self.at_it.borrow()
        );
    }
}

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
    async fn libraries(
        &self,
        peer: &ServerId,
        _endpoint: EndpointId,
    ) -> Result<Vec<SharedLibrary>, Unshared> {
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

    async fn sync(
        &self,
        peer: &ServerId,
        _endpoint: EndpointId,
        sync: &SyncLibrary,
    ) -> Result<SyncAnswer, Unshared> {
        self.held.wait().await;
        if peer.as_str() != SB_ID || sync.library != "plex:1" {
            return Err(Unshared::NotShared);
        }
        Ok(SyncAnswer {
            mode: SyncMode::Snapshot,
            cursor: "c1".into(),
            items: Arc::new((0..ITEMS).map(film).collect()),
            removed: Vec::new(),
        })
    }

    async fn artwork(
        &self,
        peer: &ServerId,
        _endpoint: EndpointId,
        id: &str,
    ) -> Result<(String, Vec<u8>), Unshared> {
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
        _endpoint: EndpointId,
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
    handler: FederationProtocol<FakePeers, FakeStrangers, FakeShares>,
    peers: Arc<FakePeers>,
    strangers: Arc<FakeStrangers>,
    shares: Arc<FakeShares>,
    live: Live,
    max_streams: Arc<std::sync::atomic::AtomicUsize>,
}

impl Sa {
    async fn start() -> Self {
        Self::start_with(None, crate::MAX_REQUESTS, crate::MAX_STRANGER_REQUESTS).await
    }

    /// With `per_peer` requests answered at once from one paired endpoint
    /// (its connections' streams if `None`), `total` from every pair and
    /// `strangers` from the rest (X8-02).
    async fn start_with(per_peer: Option<usize>, total: usize, strangers_total: usize) -> Self {
        let peers = Arc::new(FakePeers::default());
        let strangers = Arc::new(FakeStrangers::default());
        let shares = Arc::new(FakeShares::default());
        let live = Live::default();
        let max_streams = Arc::new(std::sync::atomic::AtomicUsize::new(crate::MAX_STREAMS));
        let handler = FederationProtocol::new(
            peers.clone(),
            strangers.clone(),
            shares.clone(),
            ServerId::parse(SA_ID).unwrap(),
            Arc::new(|| "Alice's server".to_owned()),
            live.clone(),
        )
        .with_max_streams(max_streams.clone())
        .with_request_budget(per_peer, total, strangers_total);
        let router = Router::builder(endpoint(vec![FEDERATION_ALPN.to_vec()]).await)
            .accept(FEDERATION_ALPN, handler.clone())
            .spawn();
        Self {
            router,
            handler,
            peers,
            strangers,
            shares,
            live,
            max_streams,
        }
    }

    /// `endpoint` is a live pair's, under `server_id`.
    fn pair(&self, endpoint: &Endpoint, server_id: &str) {
        self.peers
            .paired
            .lock()
            .insert(endpoint.id(), ServerId::parse(server_id).unwrap());
    }

    /// Syncs wait until the test lets them go on (X8-02).
    fn hold_syncs(&self) {
        self.shares.held.holding.store(true, Ordering::Release);
    }

    fn strays(&self) -> usize {
        self.strangers.strays.load(Ordering::Acquire)
    }

    /// Until `n` requests are in hand, within 5 s of real time.
    async fn until_in_hand(&self, n: usize) {
        let waited = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while self.handler.in_hand() != n {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(
            waited.is_ok(),
            "{} requests in hand, not {n}",
            self.handler.in_hand()
        );
    }
}

/// An endpoint that takes `window` bytes of each stream and no more until
/// it reads: a peer that stops reading a reply (Y8-04).
async fn endpoint_taking(window: u32) -> Endpoint {
    use iroh::endpoint::{QuicTransportConfig, VarInt};
    Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .transport_config(
            QuicTransportConfig::builder()
                .stream_receive_window(VarInt::from_u32(window))
                .build(),
        )
        .bind()
        .await
        .unwrap()
}

/// `request` sent on a stream of `dialed`'s, its reply left unread: the
/// stream to read it from later.
async fn ask_unread(dialed: &Dialed, request: &PeerRequest) -> iroh::endpoint::RecvStream {
    let (mut send, recv) = dialed.connection().open_bi().await.unwrap();
    spoolframe_protocol::codec::write_frame(&mut send, request)
        .await
        .unwrap();
    send.finish().unwrap();
    recv
}

/// The paused clock moved on a second at a time until no request is in
/// hand: how long that took. Never, within `wait` twice over, fails.
async fn until_given_up(sa: &Sa, what: &str, wait: std::time::Duration) -> std::time::Duration {
    let second = std::time::Duration::from_secs(1);
    let started = tokio::time::Instant::now();
    while sa.handler.in_hand() > 0 {
        tokio::time::advance(second).await;
        assert!(started.elapsed() < 2 * wait, "{what}: never given up on");
    }
    started.elapsed()
}

/// Whether `recv` was reset `STALLED`: the reply given up on.
async fn stalled(mut recv: iroh::endpoint::RecvStream) -> bool {
    use iroh::endpoint::{ReadError, ReadToEndError, VarInt};
    matches!(
        recv.read_to_end(usize::MAX).await,
        Err(ReadToEndError::Read(ReadError::Reset(code)))
            if code == VarInt::from_u32(spoolframe_protocol::federation::reset::STALLED)
    )
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

/// F8-12: an image's reply is read within the image's bound, not the
/// 64 MiB any reply may have. One announcing 64 MiB is refused as soon as
/// it says so, though it goes on sending and never ends; an image just
/// past the bound is refused; one at it loads.
#[tokio::test]
async fn an_image_s_reply_is_read_within_the_image_s_bound() {
    use spoolframe_protocol::codec::{MAX_FRAME, MAX_REPLY, read_frame_within, write_reply};
    use spoolframe_protocol::federation::{MAX_ARTWORK_BYTES, MAX_REQUEST, PeerReply};
    use tokio::io::AsyncWriteExt;
    let sa = endpoint(vec![FEDERATION_ALPN.to_vec()]).await;
    let answering = sa.clone();
    tokio::spawn(async move {
        let connection = answering.accept().await.unwrap().await.unwrap();
        while let Ok((mut send, mut recv)) = connection.accept_bi().await {
            let PeerRequest::Artwork(fetch) =
                read_frame_within(&mut recv, MAX_REQUEST).await.unwrap()
            else {
                panic!("not an image's fetch");
            };
            tokio::spawn(async move {
                let image = |len| PeerReply::Artwork {
                    content_type: "image/png".into(),
                    bytes: vec![7; len],
                };
                match fetch.id.as_str() {
                    "huge" => {
                        // 64 MiB announced, and its frames sent, slowly,
                        // past the image's bound; never the rest.
                        let _ = send.write_u64(MAX_REPLY as u64).await;
                        for _ in 0..(MAX_ARTWORK_BYTES / MAX_FRAME as usize + 2) {
                            let _ = send.write_u32(MAX_FRAME).await;
                            let _ = send.write_all(&vec![0; MAX_FRAME as usize]).await;
                        }
                        let _ = send.stopped().await;
                    }
                    "over" => {
                        let _ = write_reply(&mut send, &image(MAX_ARTWORK_BYTES + 1)).await;
                        let _ = send.finish();
                    }
                    _ => {
                        let _ = write_reply(&mut send, &image(MAX_ARTWORK_BYTES)).await;
                        let _ = send.finish();
                    }
                }
            });
        }
    });
    let sb = endpoint(vec![]).await;
    let connection = sb.connect(sa.addr(), FEDERATION_ALPN).await.unwrap();
    let dialed = Dialed::for_tests(connection);
    let quickly = std::time::Duration::from_secs(10);
    let huge = tokio::time::timeout(quickly, dialed.artwork("huge"))
        .await
        .expect("refused before the image's wait is out");
    assert!(matches!(huge, Err(DialError::Broken(_))), "{huge:?}");
    assert!(matches!(
        dialed.artwork("over").await,
        Err(DialError::OutOfTurn)
    ));
    let (_, bytes) = dialed.artwork("at").await.unwrap();
    assert_eq!(bytes.len(), MAX_ARTWORK_BYTES);
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
    streams_past(crate::MAX_STREAMS, None).await;
}

/// The server's setting moves the limit for a connection made after it
/// (P8-04): four held streams fill a connection when the setting is four.
#[tokio::test]
async fn a_connection_s_stream_limit_is_the_server_s_setting() {
    streams_past(4, Some(4)).await;
}

/// `limit` streams held fill a connection made with the server's limit
/// set to `set`, if given.
async fn streams_past(limit: usize, set: Option<usize>) {
    let sa = Sa::start().await;
    if let Some(set) = set {
        sa.max_streams.store(set, Ordering::Relaxed);
    }
    let sb = endpoint(Vec::new()).await;
    sa.peers
        .paired
        .lock()
        .insert(sb.id(), ServerId::parse(SB_ID).unwrap());
    let (dialed, _) = dial(&sa, &sb, SB_ID).await;
    // Streams that never finish their request: each holds its place for
    // the request's wait.
    let mut held = Vec::new();
    for _ in 0..limit {
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
        preferences: None,
        download: None,
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

/// An endpoint on the federation ALPN that holds every connection made to
/// it and every stream opened on it, reading nothing, with `transport` as
/// its limits.
async fn holding(transport: iroh::endpoint::QuicTransportConfig) -> Endpoint {
    let held = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .alpns(vec![FEDERATION_ALPN.to_vec()])
        .transport_config(transport)
        .bind()
        .await
        .unwrap();
    let answering = held.clone();
    tokio::spawn(async move {
        let mut connections = Vec::new();
        while let Some(incoming) = answering.accept().await {
            let Ok(connection) = incoming.await else {
                continue;
            };
            let streams = connection.clone();
            tokio::spawn(async move {
                let mut held = Vec::new();
                while let Ok(stream) = streams.accept_bi().await {
                    held.push(stream);
                }
            });
            connections.push(connection);
        }
    });
    held
}

/// `asked` ends `Broken` within a request's wait: the paused clock is
/// moved on a second at a time while it is asked, as the endpoints' own
/// work keeps the runtime too busy to move it on by itself.
async fn given_up_on(what: &str, asked: impl std::future::Future<Output = Result<(), DialError>>) {
    let second = std::time::Duration::from_secs(1);
    let started = tokio::time::Instant::now();
    tokio::pin!(asked);
    let asked = loop {
        tokio::select! {
            biased;
            asked = &mut asked => break asked,
            () = tokio::time::advance(second) => {}
        }
        assert!(
            started.elapsed() < 40 * crate::dial::REPLY_WAIT,
            "{what}: never given up on"
        );
    };
    assert!(
        matches!(asked, Err(DialError::Broken(_))),
        "{what}: {asked:?}"
    );
    assert!(
        started.elapsed() <= crate::dial::REPLY_WAIT + second,
        "{what}: {:?}",
        started.elapsed()
    );
}

/// C8-05: a paired server that takes a connection but no request on it,
/// granting no stream or no credit to write the request with, is given
/// up on within the request's wait, Hello among them: the wait covers
/// opening the stream and writing to it, not only the reply. Paused time,
/// once the connection is made.
#[tokio::test]
async fn a_peer_that_takes_no_request_is_given_up_on_in_its_wait() {
    use iroh::endpoint::{QuicTransportConfig, VarInt};
    let no_stream = QuicTransportConfig::builder()
        .max_concurrent_bidi_streams(VarInt::from_u32(0))
        .build();
    let no_credit = QuicTransportConfig::builder()
        .stream_receive_window(VarInt::from_u32(0))
        .receive_window(VarInt::from_u32(0))
        .build();
    for (what, transport) in [("no stream", no_stream), ("no credit", no_credit)] {
        let sa = holding(transport).await;
        let sb = endpoint(Vec::new()).await;
        let connection = sb.connect(sa.addr(), FEDERATION_ALPN).await.unwrap();
        let dialed = Dialed::for_tests(connection.clone());
        tokio::time::pause();
        given_up_on(what, async { dialed.libraries().await.map(drop) }).await;
        let sync = SyncLibrary {
            library: "plex:1".into(),
            since: None,
        };
        given_up_on(what, async { dialed.sync(sync).await.map(drop) }).await;
        given_up_on(what, async {
            Dialed::hello_on(connection.clone(), hello(SB_ID))
                .await
                .map(drop)
        })
        .await;
        tokio::time::resume();
    }
}

/// C8-05: a dial to a server that takes the connection and never lets its
/// Hello through ends within the dial's and the Hello's wait together. Real
/// time: the handshake is a real one, so the clock is not paused.
#[tokio::test]
async fn a_dial_whose_hello_never_goes_through_ends_in_its_wait() {
    use iroh::endpoint::{QuicTransportConfig, VarInt};
    let sa = holding(
        QuicTransportConfig::builder()
            .max_concurrent_bidi_streams(VarInt::from_u32(0))
            .build(),
    )
    .await;
    let sb = endpoint(Vec::new()).await;
    let started = std::time::Instant::now();
    let dialled = tokio::time::timeout(
        crate::dial::CONNECT_WAIT + std::time::Duration::from_secs(5),
        Dialed::connect(&sb, sa.addr(), hello(SB_ID)),
    )
    .await
    .expect("given up on in the dial's wait");
    assert!(dialled.is_err());
    assert!(started.elapsed() <= crate::dial::CONNECT_WAIT + std::time::Duration::from_secs(1));
}

/// Movies, whole.
fn movies() -> SyncLibrary {
    SyncLibrary {
        library: "plex:1".into(),
        since: None,
    }
}

/// The audit's X8-02: a connection's requests end with it. Syncs held at
/// the service from connections since closed are aborted, not left at
/// work: closing and connecting again, as often as one likes, leaves
/// nothing in hand, and what was held never runs to its end once let go.
/// Another pair is answered meanwhile.
#[tokio::test]
async fn a_closed_connection_s_requests_are_aborted_with_it() {
    const SC_ID: &str = "cccccccccccccccc";
    let sa = Sa::start().await;
    sa.max_streams.store(2, Ordering::Relaxed);
    sa.hold_syncs();
    let (sb, sc) = (endpoint(Vec::new()).await, endpoint(Vec::new()).await);
    sa.pair(&sb, SB_ID);
    sa.pair(&sc, SC_ID);
    for round in 1..=3 {
        let (dialed, _) = dial(&sa, &sb, SB_ID).await;
        let mut pages = Vec::new();
        for _ in 0..2 {
            pages.push(dialed.sync(movies()).await.unwrap());
        }
        sa.shares.held.until_at_it(2).await;
        if round == 1 {
            let (other, _) = dial(&sa, &sc, SC_ID).await;
            other.libraries().await.expect("another pair is answered");
        }
        dialed.close();
        drop(pages);
        // Aborted with the connection, not left at the hold.
        sa.shares.held.until_at_it(0).await;
    }
    sa.shares.held.go_on.notify_waiters();
    tokio::task::yield_now().await;
    assert_eq!(
        sa.shares.held.done.load(Ordering::Acquire),
        0,
        "no abandoned sync ran to its end"
    );
    assert_eq!(sa.strays(), 0);
}

/// X8-02's budgets: an endpoint's requests at once are capped across its
/// connections, and every endpoint's together; one past either is reset
/// `BUSY` and not counted against it, and a place is given back as its
/// connection closes.
#[tokio::test]
async fn requests_at_once_are_capped_per_endpoint_and_in_all() {
    const SC_ID: &str = "cccccccccccccccc";
    let sa = Sa::start_with(Some(3), 4, crate::MAX_STRANGER_REQUESTS).await;
    sa.hold_syncs();
    let (sb, sc) = (endpoint(Vec::new()).await, endpoint(Vec::new()).await);
    sa.pair(&sb, SB_ID);
    sa.pair(&sc, SC_ID);
    let (first, _) = dial(&sa, &sb, SB_ID).await;
    let (second, _) = dial(&sa, &sb, SB_ID).await;
    // SB's three, over two connections: its budget, with streams to spare
    // on each.
    let mut held = vec![
        first.sync(movies()).await.unwrap(),
        first.sync(movies()).await.unwrap(),
        second.sync(movies()).await.unwrap(),
    ];
    sa.shares.held.until_at_it(3).await;
    assert!(
        matches!(second.libraries().await, Err(DialError::Broken(_))),
        "past the endpoint's budget"
    );
    // SC takes the fourth place of all, and no more.
    let (third, _) = dial(&sa, &sc, SC_ID).await;
    held.push(third.sync(movies()).await.unwrap());
    sa.shares.held.until_at_it(4).await;
    assert!(
        matches!(third.libraries().await, Err(DialError::Broken(_))),
        "past the server's budget"
    );
    assert_eq!(sa.strays(), 0);
    // SB's second connection closes: its place is given back, and SC is
    // answered.
    second.close();
    sa.shares.held.until_at_it(3).await;
    third.libraries().await.unwrap();
    drop(held);
}

/// The review of X8-02: a stranger's streams, held open for a request
/// that never comes, draw on the strangers' own budget and never on the
/// pairs': with the strangers' full, a pair is answered still, and the
/// stranger's next stream is reset without being counted against it.
#[tokio::test]
async fn strangers_requests_have_a_budget_of_their_own() {
    let sa = Sa::start_with(None, 2, 2).await;
    let stranger = endpoint(Vec::new()).await;
    let (dialed, standing) = dial(&sa, &stranger, SB_ID).await;
    assert_eq!(standing, Standing::Stranger);
    let mut held = Vec::new();
    for _ in 0..2 {
        let (mut send, recv) = dialed.connection().open_bi().await.unwrap();
        send.write_all(&[0]).await.unwrap();
        held.push((send, recv));
    }
    assert!(matches!(
        dialed.ask(&PeerRequest::RedeemPairing(redeem(CODE))).await,
        Err(DialError::Broken(_))
    ));
    assert_eq!(sa.strays(), 0, "a stream past the budget is not a stray");
    let sb = endpoint(Vec::new()).await;
    sa.pair(&sb, SB_ID);
    let (paired, _) = dial(&sa, &sb, SB_ID).await;
    paired
        .libraries()
        .await
        .expect("the pairs' budget is their own");
    drop(held);
}

/// The review of X8-02: a pair's requests at once follow the streams
/// setting, so raising it takes effect: with one stream a connection, a
/// pair holds four connections' worth, and a fifth request is reset.
#[tokio::test]
async fn a_pair_s_requests_at_once_follow_the_streams_setting() {
    let sa = Sa::start().await;
    sa.max_streams.store(1, Ordering::Relaxed);
    sa.hold_syncs();
    let sb = endpoint(Vec::new()).await;
    sa.pair(&sb, SB_ID);
    let mut connections = Vec::new();
    let mut held = Vec::new();
    for _ in 0..crate::PEER_CONNECTIONS {
        let (dialed, _) = dial(&sa, &sb, SB_ID).await;
        held.push(dialed.sync(movies()).await.unwrap());
        connections.push(dialed);
    }
    sa.shares.held.until_at_it(crate::PEER_CONNECTIONS).await;
    // A fifth request, its hello: reset, past four connections' streams.
    let fifth = Dialed::connect(&sb, sa.router.endpoint().addr(), hello(SB_ID)).await;
    assert!(matches!(fifth, Err(DialError::Broken(_))), "{fifth:?}");
    assert_eq!(sa.strays(), 0);
    drop(held);
}

/// X8-02's deadline: a sync whose work does not end within what the asker
/// waits for its first page, less a margin, is answered `unavailable`
/// then, and its work dropped. Paused time, once the connection is made;
/// the page is read with no wait of the asker's own, so only the dialled
/// side's deadline ends it.
#[tokio::test]
async fn a_request_whose_work_outlasts_the_asker_s_wait_is_given_up_on() {
    use spoolframe_protocol::codec::{MAX_FRAME, read_frame_within, write_frame};
    use spoolframe_protocol::federation::PeerReply;
    let sa = Sa::start().await;
    sa.hold_syncs();
    let sb = endpoint(Vec::new()).await;
    sa.pair(&sb, SB_ID);
    let (dialed, _) = dial(&sa, &sb, SB_ID).await;
    let (mut send, mut recv) = dialed.connection().open_bi().await.unwrap();
    write_frame(&mut send, &PeerRequest::Sync(movies()))
        .await
        .unwrap();
    send.finish().unwrap();
    sa.shares.held.until_at_it(1).await;
    tokio::time::pause();
    let second = std::time::Duration::from_secs(1);
    let started = tokio::time::Instant::now();
    let page = read_frame_within::<_, PeerReply>(&mut recv, MAX_FRAME);
    tokio::pin!(page);
    let page = loop {
        tokio::select! {
            biased;
            page = &mut page => break page,
            () = tokio::time::advance(second) => {}
        }
        assert!(
            started.elapsed() < 2 * crate::dial::FIRST_PAGE_WAIT,
            "never given up on"
        );
    };
    tokio::time::resume();
    assert!(matches!(page, Ok(PeerReply::Unavailable)), "{page:?}");
    let given_up = crate::dial::FIRST_PAGE_WAIT - crate::handler::ANSWER_MARGIN;
    assert!(
        started.elapsed() >= given_up && started.elapsed() <= given_up + 10 * second,
        "{:?}",
        started.elapsed()
    );
    sa.shares.held.until_at_it(0).await;
}

/// Y8-04: a reply the asker takes none of, keeping the connection alive
/// but reading nothing of the stream, is given up on when the asker's own
/// wait for it is up, less the margin: its stream is reset `STALLED`, and
/// the request's place given back, so another is answered. A sync's first
/// page and an image; paused time, once the request is in hand.
#[tokio::test]
async fn a_reply_the_asker_takes_none_of_is_given_up_on_in_its_wait() {
    use spoolframe_protocol::federation::ArtworkFetch;
    let sa = Sa::start_with(Some(1), 4, crate::MAX_STRANGER_REQUESTS).await;
    // SB takes a KiB of a stream and no more: a page, or the image, never
    // fits.
    let sb = endpoint_taking(1024).await;
    sa.pair(&sb, SB_ID);
    let (dialed, _) = dial(&sa, &sb, SB_ID).await;
    let poster = PeerRequest::Artwork(ArtworkFetch {
        id: "poster".into(),
    });
    for (what, request, wait) in [
        (
            "sync",
            PeerRequest::Sync(movies()),
            crate::dial::FIRST_PAGE_WAIT,
        ),
        ("artwork", poster, crate::dial::ARTWORK_WAIT),
    ] {
        let recv = ask_unread(&dialed, &request).await;
        sa.until_in_hand(1).await;
        // SB's one place is the stalled reply's: nothing else of SB's is
        // answered meanwhile.
        assert!(
            matches!(dialed.libraries().await, Err(DialError::Broken(_))),
            "{what}: the place is held"
        );
        tokio::time::pause();
        let took = until_given_up(&sa, what, wait).await;
        tokio::time::resume();
        let second = std::time::Duration::from_secs(1);
        let given_up = wait - crate::handler::ANSWER_MARGIN;
        assert!(
            took >= given_up - second && took <= given_up + 10 * second,
            "{what}: given up on after {took:?}"
        );
        assert!(stalled(recv).await, "{what}: the stream is reset");
        dialed.libraries().await.expect("the place is given back");
    }
    assert_eq!(sa.strays(), 0);
}

/// Y8-04: a sync's later page the asker takes none of is given up on
/// within the asker's wait for a page, less the margin, not the first
/// page's long wait: SB takes the first page and no more.
#[tokio::test]
async fn a_later_page_the_asker_takes_none_of_is_given_up_on_in_a_page_s_wait() {
    let first_page: usize = (0..MAX_PAGE_ITEMS)
        .map(|n| spoolframe_protocol::codec::encoded_len(&film(n)).unwrap())
        .sum();
    let sa = Sa::start().await;
    // Room for the first page, its envelope and frame, and part of the
    // second.
    let sb = endpoint_taking(u32::try_from(first_page).unwrap() + 2048).await;
    sa.pair(&sb, SB_ID);
    let (dialed, _) = dial(&sa, &sb, SB_ID).await;
    let recv = ask_unread(&dialed, &PeerRequest::Sync(movies())).await;
    sa.until_in_hand(1).await;
    tokio::time::pause();
    let took = until_given_up(&sa, "a later page", crate::dial::PAGE_WAIT).await;
    tokio::time::resume();
    let second = std::time::Duration::from_secs(1);
    let given_up = crate::dial::PAGE_WAIT - crate::handler::ANSWER_MARGIN;
    assert!(
        took >= given_up - second && took <= given_up + 10 * second,
        "given up on after {took:?}"
    );
    assert!(stalled(recv).await, "the stream is reset");
}

/// Pages hold at most so many items, then the removed ids; the last says
/// so; an empty answer is one empty page.
#[test]
fn an_answer_is_paged_items_first_then_removals() {
    let answer = |items: usize, removed: usize| SyncAnswer {
        mode: SyncMode::Changes,
        cursor: "c".into(),
        items: Arc::new((0..items).map(film).collect()),
        removed: (0..removed).map(|n| format!("plex:movie:{n}")).collect(),
    };
    let shape = |items, removed| {
        pages(answer(items, removed))
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
