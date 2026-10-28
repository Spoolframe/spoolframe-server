//! Tests: what this server sends a pair, and keeps of what it hears, goes
//! by the key the pair is pinned to now and by the pair's server id
//! (C8-01, C8-06). A link, a dial or an assertion of the old key's, once a
//! new key is confirmed, carries nothing; a key that answers as another
//! server is refused.

use std::sync::Arc;

use iroh::endpoint::{Connection, presets};
use iroh::{Endpoint, EndpointId, SecretKey};
use spoolframe_protocol::codec::{read_frame, read_frame_within, write_frame, write_reply};
use spoolframe_protocol::federation::{
    AssertionRefused, MAX_REQUEST, PeerInfo, PeerReply, PeerRequest, TokenGranted, TokenScope,
};
use spoolframe_protocol::rpc::{RpcRequest, RpcResponse, ServerInstance};
use spoolframe_protocol::{
    CLIENT_ALPN, Error, FEDERATION_ALPN, Reply, Request, Response, ServerInfo,
};
use spoolframe_types::{MediaId, Role, SubtitleDownloaded, SubtitleRole, SubtitleTrack};
use tokio::io::AsyncReadExt;

use super::tokens::tests::{bob_s_key, bob_wants, sharing};
use super::*;
use crate::principal::Who;
use crate::store::Store;
use crate::store::federation::NewPeer;
use crate::store::sessions::SessionUser;
use crate::store::tests::{TestStore, test_store};
use crate::store::users::LocalUserId;
use crate::test_support::{FakeCatalog, movie};

/// Alice's server's id: the pair's.
const SA: &str = "aaaaaaaaaaaaaaaa";

/// What a server answering heard, in turn.
type Heard = Arc<parking_lot::Mutex<Vec<&'static str>>>;

/// A key Alice's server answers at, as far as this server's dials meet
/// it: on both ALPNs it says hello as `server_id`, paired, and it keeps
/// what it is asked.
struct Answering {
    endpoint: Endpoint,
    heard: Heard,
}

impl Answering {
    async fn start(key: u8, server_id: &'static str) -> Self {
        Self::start_with(key, server_id, true).await
    }

    /// As [`Answering::start`], its `libraries` answered `unavailable`
    /// unless `lists`: a sync of it fails.
    async fn start_with(key: u8, server_id: &'static str, lists: bool) -> Self {
        let endpoint = Endpoint::builder(presets::Minimal)
            .secret_key(SecretKey::from_bytes(&[key; 32]))
            .relay_mode(iroh::RelayMode::Disabled)
            .alpns(vec![FEDERATION_ALPN.to_vec(), CLIENT_ALPN.to_vec()])
            .bind()
            .await
            .unwrap();
        let heard = Heard::default();
        let (accepting, kept) = (endpoint.clone(), heard.clone());
        tokio::spawn(async move {
            while let Some(incoming) = accepting.accept().await {
                if let Ok(connection) = incoming.await {
                    tokio::spawn(answer(connection, server_id, lists, kept.clone()));
                }
            }
        });
        Self { endpoint, heard }
    }

    /// Its key, hex, as a pair keeps it.
    fn key(&self) -> String {
        self.endpoint.id().to_string()
    }

    fn heard(&self) -> Vec<&'static str> {
        self.heard.lock().clone()
    }

    /// Where it answers: one key's addresses only, as a dial to a key at
    /// another's fails its handshake.
    fn addrs(&self) -> Vec<String> {
        self.endpoint
            .addr()
            .ip_addrs()
            .map(ToString::to_string)
            .collect()
    }

    /// Pair `peer` is reached here from now.
    async fn reached_for(&self, store: &Store, peer: &PeerRecord) {
        store.set_peer_addrs(&peer.id, &self.addrs()).await.unwrap();
    }
}

/// What `connection` asks, answered as `server_id` would, and kept.
async fn answer(connection: Connection, server_id: &'static str, lists: bool, heard: Heard) {
    let client = connection.alpn() == CLIENT_ALPN;
    let key = connection.remote_id();
    while let Ok((mut send, mut recv)) = connection.accept_bi().await {
        if client {
            let Ok(_) = recv.read_u8().await else {
                return;
            };
            let Ok(request) = read_frame::<_, RpcRequest>(&mut recv).await else {
                return;
            };
            let response = match request.envelope.request {
                Request::Hello(_) => {
                    heard.lock().push("client hello");
                    Response::Ok(Reply::ServerInfo(ServerInfo {
                        name: "Alice's server".into(),
                        endpoint_id: key.to_string(),
                        server_id: server_id.into(),
                        protocol_version: spoolframe_protocol::PROTOCOL_VERSION,
                        server_version: "0".into(),
                        auth_methods: Vec::new(),
                        web_url: None,
                        experimental: Vec::new(),
                        invite_only: true,
                    }))
                }
                _ => {
                    heard.lock().push("client request");
                    Response::Err(Error::Unauthenticated)
                }
            };
            let reply = RpcResponse {
                server: ServerInstance {
                    id: "alice".into(),
                    time_ms: 0,
                },
                reply: response.into(),
            };
            let _ = write_reply(&mut send, &reply).await;
        } else {
            let Ok(request) = read_frame_within(&mut recv, MAX_REQUEST).await else {
                return;
            };
            let (said, reply) = match request {
                PeerRequest::Hello(_) => (
                    "hello",
                    PeerReply::Info(PeerInfo {
                        protocol_version: spoolframe_protocol::federation::PROTOCOL_VERSION,
                        server_id: server_id.into(),
                        name: "Alice's server".into(),
                        standing: Standing::Paired,
                    }),
                ),
                PeerRequest::Libraries if lists => (
                    "libraries",
                    PeerReply::Libraries {
                        libraries: Vec::new(),
                        name: None,
                    },
                ),
                PeerRequest::Libraries => ("libraries", PeerReply::Unavailable),
                PeerRequest::AssertUser(assertion) => (
                    "assertion",
                    PeerReply::Token(TokenGranted {
                        token: "a token".into(),
                        scope: assertion.scope,
                        expires_at: 300,
                    }),
                ),
                PeerRequest::RemoveSubtitleDownload(_) => ("subtitles", PeerReply::Done),
                // Downloaded, after a while: what a request dropped
                // meanwhile was asked for.
                PeerRequest::DownloadSubtitle(_) => {
                    heard.lock().push("subtitle asked");
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    (
                        "subtitle downloaded",
                        PeerReply::SubtitleDownloaded {
                            downloaded: a_download(),
                        },
                    )
                }
                _ => ("other", PeerReply::Unavailable),
            };
            heard.lock().push(said);
            let _ = write_frame(&mut send, &reply).await;
        }
        let _ = send.finish();
    }
}

/// A subtitle Alice's server downloaded, as it answers.
fn a_download() -> SubtitleDownloaded {
    SubtitleDownloaded {
        track: 3,
        subtitle: SubtitleTrack {
            language: Some("eng".into()),
            title: Some("OpenSubtitles".into()),
            forced: false,
            default: false,
            external: true,
            role: SubtitleRole::Full,
            timing: None,
            download: None,
        },
        account_remaining: None,
        user_remaining: Some(39),
    }
}

/// Bob's server, paired with Alice's pinned to `at`'s key and reached
/// there, attached to an endpoint of its own; and Bob, a user of its.
async fn bob_s(test: &str, at: &Answering) -> (TestStore, Federation, PeerRecord, LocalUserId) {
    let store = test_store(test).await;
    let federation = Federation::new(
        Auth::new(store.store.clone()),
        Arc::new(FakeCatalog::new("/nowhere/movie.mkv".into())),
        "mzxw6ytbojtg633c".parse().unwrap(),
        ServerName::new("Bob's server"),
        ReplicaSettings::in_dir("/nowhere/federation".into()),
    );
    let peer = store
        .add_peer(
            NewPeer {
                server_id: SA,
                endpoint_id: &at.key(),
                display_name: "Alice's server",
                addrs: &at.addrs(),
                relays: &[],
            },
            "bob",
        )
        .await
        .unwrap();
    let sb = Endpoint::builder(presets::Minimal)
        .relay_mode(iroh::RelayMode::Disabled)
        .bind()
        .await
        .unwrap();
    federation.attach_quietly(sb);
    let bob = store
        .add_user(
            "bob",
            None,
            "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA",
            Role::Consumer,
        )
        .await
        .unwrap();
    (store, federation, peer, bob)
}

/// Bob, signed in.
fn who(bob: LocalUserId) -> Who {
    Who::Local(SessionUser {
        user: bob,
        username: "bob".into(),
        display_name: "bob".into(),
        role: Role::Consumer,
        pin_required: false,
        authenticated_at: 0,
        signed_in_with: None,
        password_change_only: false,
    })
}

/// Alice's film, as Bob's server names it.
fn alien() -> MediaId {
    MediaId::federated(SA, movie().id()).unwrap()
}

/// A device of Bob's.
fn device() -> EndpointId {
    SecretKey::from_bytes(&[9; 32]).public()
}

/// This server's own endpoint: what it dials the pair from.
fn endpoint_of(federation: &Federation) -> Endpoint {
    federation.attached.get().unwrap().endpoint.clone()
}

/// The audit's C8-01: once Alice's new key is confirmed, the link to her
/// old one is closed, and a refresh, an assertion and a subtitle ask after
/// it reach the old key not at all. The refresh dials the new key, and
/// what follows goes there.
#[tokio::test]
async fn a_confirmed_key_closes_the_old_one_s_link_and_the_new_one_is_dialled() {
    let (old, new) = (
        Answering::start(21, SA).await,
        Answering::start(22, SA).await,
    );
    let (store, federation, peer, bob) = bob_s("fed-pin-confirm", &old).await;
    federation.refresh(&peer.id).await.unwrap();
    assert_eq!(old.heard(), ["hello", "libraries"]);
    store.hold_peer_key(&peer.id, &new.key()).await.unwrap();
    federation
        .confirm_key(&peer.id, &new.key(), "bob")
        .await
        .unwrap();
    assert!(!federation.online(SA), "the old key's link is closed");
    let asserted = federation.assert_play(bob, &alien(), device()).await;
    assert!(
        matches!(asserted, Err(AssertError::Offline)),
        "{asserted:?}"
    );
    assert_eq!(
        federation
            .remove_subtitle_download_of(&who(bob), &alien(), "d")
            .await,
        Err(Error::OriginOffline)
    );
    new.reached_for(&store, &peer).await;
    federation.refresh(&peer.id).await.unwrap();
    federation
        .assert_play(bob, &alien(), device())
        .await
        .unwrap();
    assert_eq!(old.heard(), ["hello", "libraries"], "nothing more");
    assert_eq!(new.heard(), ["hello", "libraries", "assertion"]);
}

/// A link to a key the pair has moved from, its cleanup missed, is not
/// used: what is sent goes by the pin as it is read, each time.
#[tokio::test]
async fn a_link_to_a_key_the_pair_moved_from_carries_nothing() {
    let (old, new) = (
        Answering::start(23, SA).await,
        Answering::start(24, SA).await,
    );
    let (store, federation, peer, bob) = bob_s("fed-pin-stale", &old).await;
    federation.refresh(&peer.id).await.unwrap();
    store.hold_peer_key(&peer.id, &new.key()).await.unwrap();
    // The pin moves, and nothing of this federation's hears of it.
    store.confirm_peer_key(&peer.id, &new.key()).await.unwrap();
    let asserted = federation.assert_play(bob, &alien(), device()).await;
    assert!(
        matches!(asserted, Err(AssertError::Offline)),
        "{asserted:?}"
    );
    assert_eq!(
        federation
            .remove_subtitle_download_of(&who(bob), &alien(), "d")
            .await,
        Err(Error::OriginOffline)
    );
    assert_eq!(old.heard(), ["hello", "libraries"]);
}

/// The replica's dial of the old key, answered as the new key is
/// confirmed, keeps no link, and asks the old key nothing more.
#[tokio::test]
async fn a_dial_of_the_old_key_as_a_new_one_is_confirmed_keeps_no_link() {
    let (old, new) = (
        Answering::start(25, SA).await,
        Answering::start(26, SA).await,
    );
    let (store, federation, peer, _) = bob_s("fed-pin-dial", &old).await;
    store.hold_peer_key(&peer.id, &new.key()).await.unwrap();
    let (held, go) = federation.holds.dialing.next();
    let confirm = async {
        held.await.unwrap();
        federation
            .confirm_key(&peer.id, &new.key(), "bob")
            .await
            .unwrap();
        go.send(()).unwrap();
    };
    let (refreshed, ()) = tokio::join!(federation.refresh(&peer.id), confirm);
    assert!(
        matches!(refreshed, Err(ReplicaError::Moved)),
        "{refreshed:?}"
    );
    assert!(!federation.online(SA));
    assert_eq!(old.heard(), ["hello"], "asked nothing past its hello");
}

/// The same for an unpairing: a dial answered as the pair ends keeps no
/// link, however the sync lock is held around it.
#[tokio::test]
async fn a_dial_answered_as_the_pair_is_unpaired_keeps_no_link() {
    let sa = Answering::start(30, SA).await;
    let (_store, federation, peer, _) = bob_s("fed-pin-dial-unpaired", &sa).await;
    let (held, go) = federation.holds.dialing.next();
    // The unpairing commits while the dial is held; its cleanup, run then,
    // finds no link to close.
    let unpair = async {
        held.await.unwrap();
        federation
            .store()
            .revoke_peer(&peer.id)
            .await
            .unwrap()
            .unwrap();
        go.send(()).unwrap();
    };
    let (dialled, ()) = tokio::join!(federation.refresh(&peer.id), unpair);
    assert!(matches!(dialled, Err(ReplicaError::Moved)), "{dialled:?}");
    assert!(!federation.online(SA));
    assert_eq!(sa.heard(), ["hello"]);
}

/// The same through the admin's unpairing, its cleanup and all: the dial
/// keeps no link, and the old pair is asked for nothing it shares.
#[tokio::test]
async fn a_dial_answered_as_an_admin_unpairs_keeps_no_link() {
    let sa = Answering::start(36, SA).await;
    let (_store, federation, peer, _) = bob_s("fed-pin-dial-unpair", &sa).await;
    let (held, go) = federation.holds.dialing.next();
    let unpair = async {
        held.await.unwrap();
        federation.unpair(&peer.id, "bob").await.unwrap().unwrap();
        go.send(()).unwrap();
    };
    let (dialled, ()) = tokio::join!(federation.refresh(&peer.id), unpair);
    assert!(matches!(dialled, Err(ReplicaError::Moved)), "{dialled:?}");
    assert!(!federation.online(SA));
    assert!(!sa.heard().contains(&"libraries"), "{:?}", sa.heard());
}

/// A dial waiting on another's, across a confirmed key, dials the new key,
/// whatever record its caller read: the pair is read again under the dial
/// lock, as the play pool's is.
#[tokio::test]
async fn a_dial_waiting_on_another_across_a_confirm_dials_the_new_key() {
    let (old, new) = (
        Answering::start(33, SA).await,
        Answering::start(34, SA).await,
    );
    let (store, federation, peer, _) = bob_s("fed-pin-dial-wait", &old).await;
    store.hold_peer_key(&peer.id, &new.key()).await.unwrap();
    let (held, go) = federation.holds.dialing.next();
    let (waiting, wait) = tokio::sync::oneshot::channel();
    let confirm = async {
        held.await.unwrap();
        // The second dial waits on the first's lock from now.
        waiting.send(()).unwrap();
        federation
            .confirm_key(&peer.id, &new.key(), "bob")
            .await
            .unwrap();
        new.reached_for(&store, &peer).await;
        go.send(()).unwrap();
    };
    let second = async {
        wait.await.unwrap();
        // The record read before the confirmation.
        federation.connect(&peer).await
    };
    let (first, (), second) = tokio::join!(federation.refresh(&peer.id), confirm, second);
    assert!(matches!(first, Err(ReplicaError::Moved)), "{first:?}");
    second.unwrap();
    assert_eq!(old.heard(), ["hello"], "dialled once");
    assert_eq!(new.heard(), ["hello"]);
    assert!(federation.online(SA));
}

/// A pair whose old key failed its dials, its wait far from out, has the
/// new key dialled at its next tend once that is confirmed: the wait was
/// the old key's.
#[tokio::test]
async fn a_confirmed_key_is_dialled_at_once_whatever_the_old_key_s_wait() {
    use super::replica::Tended;
    let (old, new) = (
        Answering::start(31, SA).await,
        Answering::start(32, SA).await,
    );
    let (store, federation, peer, _) = bob_s("fed-pin-backoff", &old).await;
    let now = store.clock().now();
    let mut state = Tended {
        endpoint: peer.endpoint_id.clone(),
        retry_at: now + std::time::Duration::from_secs(3600),
        failures: 3,
        ..Tended::at(now)
    };
    store.hold_peer_key(&peer.id, &new.key()).await.unwrap();
    federation
        .confirm_key(&peer.id, &new.key(), "bob")
        .await
        .unwrap();
    new.reached_for(&store, &peer).await;
    federation.tend(&peer, &mut state).await;
    assert_eq!(new.heard(), ["hello", "libraries"]);
    assert!(old.heard().is_empty());
    assert_eq!(state.failures, 0);
}

/// A pair whose sync fails waits before it is tended again, as one that
/// cannot be dialled does: it does not take one of the places at every
/// tick.
#[tokio::test]
async fn a_pair_whose_sync_fails_is_not_tended_again_at_the_next_tick() {
    use super::replica::Tended;
    let sa = Answering::start_with(35, SA, false).await;
    let (store, federation, peer, _) = bob_s("fed-pin-sync-fails", &sa).await;
    let mut state = Tended::at(store.clock().now());
    federation.tend(&peer, &mut state).await;
    assert_eq!(sa.heard(), ["hello", "libraries"]);
    assert_eq!(state.failures, 1);
    federation.tend(&peer, &mut state).await;
    assert_eq!(sa.heard(), ["hello", "libraries"], "not again so soon");
}

/// The audit's X8-05: a pair's wait after a failure runs from the
/// failure, not from the attempt's start. An attempt that takes longer
/// than its wait (a dial held while the clock moves on, whose sync then
/// fails) still waits its whole wait once it has ended; at the longest
/// wait, after a sync that took its whole budget, the same.
#[tokio::test]
async fn a_failure_s_wait_runs_from_the_failure_not_the_attempt_s_start() {
    use super::replica::{FIRST_RETRY, MAX_RETRY, SYNC_BUDGET, Tended};
    let sa = Answering::start_with(37, SA, false).await;
    let (store, federation, peer, _) = bob_s("fed-pin-late-failure", &sa).await;
    let clock = store.clock().clone();
    let mut state = Tended::at(clock.now());
    let mut heard = Vec::new();
    for (failures_before, took, wait) in [
        (0, 2 * FIRST_RETRY, FIRST_RETRY),
        (20, SYNC_BUDGET, MAX_RETRY),
    ] {
        state.failures = failures_before;
        state.retry_at = clock.now();
        let (held, go) = federation.holds.dialing.next();
        let late = async {
            held.await.unwrap();
            // The attempt takes `took` before its sync fails.
            clock.advance(took);
            go.send(()).unwrap();
        };
        tokio::join!(federation.tend(&peer, &mut state), late);
        heard.extend(["hello", "libraries"]);
        assert_eq!(sa.heard(), heard);
        assert_eq!(state.failures, failures_before + 1);
        let failed = clock.now();
        assert!(
            state.retry_at >= failed + wait,
            "the whole wait from the failure: {:?} short",
            (failed + wait).duration_since(state.retry_at)
        );
        // Half the wait on from the failure: not tried again.
        clock.advance(wait / 2);
        federation.tend(&peer, &mut state).await;
        assert_eq!(sa.heard(), heard, "waiting still");
        clock.advance(wait);
    }
    federation.tend(&peer, &mut state).await;
    heard.extend(["hello", "libraries"]);
    assert_eq!(sa.heard(), heard, "tried again once the wait is out");
}

/// The play pool's dial of the old key, answered as the new key is
/// confirmed, is not kept: the next play dials the new key.
#[tokio::test]
async fn a_play_connection_to_the_old_key_as_a_new_one_is_confirmed_is_not_kept() {
    let (old, new) = (
        Answering::start(27, SA).await,
        Answering::start(28, SA).await,
    );
    let (store, federation, peer, _) = bob_s("fed-pin-play", &old).await;
    let sb = endpoint_of(&federation);
    store.hold_peer_key(&peer.id, &new.key()).await.unwrap();
    let (held, go) = federation.holds.dialing.next();
    let confirm = async {
        held.await.unwrap();
        federation
            .confirm_key(&peer.id, &new.key(), "bob")
            .await
            .unwrap();
        go.send(()).unwrap();
    };
    let (dialled, ()) = tokio::join!(federation.play_client(&sb, &peer), confirm);
    assert!(dialled.is_err(), "{dialled:?}");
    assert_eq!(federation.play_connections(), 0);
    // Whatever record the caller holds, the dial goes to the pin.
    new.reached_for(&store, &peer).await;
    let held = federation.play_client(&sb, &peer).await.unwrap();
    assert_eq!(held.client.remote_id().to_string(), new.key());
    assert_eq!(old.heard(), ["client hello"]);
}

/// The audit's C8-06: the pair's key answering under another server id
/// is not the pair. Neither the replica nor the play pool keeps the
/// connection or asks anything more of it, and the Federation page is
/// told, as for an incoming hello under another id.
#[tokio::test]
async fn a_pair_s_key_answering_as_another_server_is_refused() {
    let other = Answering::start(29, "cccccccccccccccc").await;
    let (store, federation, peer, _) = bob_s("fed-pin-other", &other).await;
    let refreshed = federation.refresh(&peer.id).await;
    assert!(
        matches!(&refreshed, Err(ReplicaError::OtherServer(id)) if id == "cccccccccccccccc"),
        "{refreshed:?}"
    );
    assert!(!federation.online(SA));
    let dialled = federation
        .play_client(&endpoint_of(&federation), &peer)
        .await;
    assert!(dialled.is_err(), "{dialled:?}");
    assert_eq!(federation.play_connections(), 0);
    assert_eq!(other.heard(), ["hello", "client hello"]);
    let (_, refused_at) = store.peer_presence(&peer.id).await.unwrap();
    assert!(refused_at.is_some());
}

/// Z8-07: a pair's key answering under an id with a newline in it is
/// refused, and the log's line for it stays one line: the id is escaped.
#[tokio::test]
async fn a_refused_server_id_forges_no_log_line() {
    use tracing_subscriber::layer::SubscriberExt as _;
    #[derive(Clone, Default)]
    struct Buffer(Arc<parking_lot::Mutex<Vec<u8>>>);
    impl std::io::Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let buffer = Buffer::default();
    let written = buffer.clone();
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(move || written.clone()),
    );
    let _logged = tracing::subscriber::set_default(subscriber);
    let forged = Answering::start(30, "bad\nWARN forged: an admin paired a stranger").await;
    // Kept to the end: dropping the store removes its directory, and a
    // connection opened after that finds no database file.
    let (_store, federation, peer, _) = bob_s("fed-pin-forged", &forged).await;
    let refreshed = federation.refresh(&peer.id).await;
    let text = String::from_utf8(buffer.0.lock().clone()).unwrap();
    // Failed once on CI with no refusal logged: say how, if it fails so.
    assert!(
        matches!(refreshed, Err(ReplicaError::OtherServer(_))),
        "{refreshed:?}\n{text}"
    );
    let refused: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("answered under another server id"))
        .collect();
    assert_eq!(refused.len(), 1, "{text}");
    assert!(refused[0].contains(r"bad\nWARN forged"), "{text}");
    assert!(
        !text.lines().any(|l| l.starts_with("WARN forged")),
        "{text}"
    );
}

/// Bob's server says hello to Alice's as `server_id`.
fn bob_s_hello(server_id: &ServerId) -> PeerHello {
    PeerHello {
        protocol_version: PROTOCOL_VERSION,
        server_id: server_id.to_string(),
        name: "Bob's server".into(),
    }
}

/// An endpoint keyed `key`, with no ALPN of its own: a dialler's.
async fn keyed(key: u8) -> Endpoint {
    Endpoint::builder(presets::Minimal)
        .secret_key(SecretKey::from_bytes(&[key; 32]))
        .relay_mode(iroh::RelayMode::Disabled)
        .bind()
        .await
        .unwrap()
}

/// Alice's server answering `federation/1` on an endpoint of its own.
async fn answering(federation: &Arc<Federation>) -> iroh::protocol::Router {
    let sa = Endpoint::builder(presets::Minimal)
        .relay_mode(iroh::RelayMode::Disabled)
        .bind()
        .await
        .unwrap();
    federation.attach_quietly(sa.clone());
    iroh::protocol::Router::builder(sa)
        .accept(FEDERATION_ALPN, federation.protocol())
        .spawn()
}

/// The review of X8-02: Bob's server unpairs, and its connection closes
/// while Alice's revokes the pair and cleans up after it. The cleanup
/// runs to its end all the same: it is on a task of its own, not the
/// connection's, which the close aborts.
#[tokio::test]
async fn an_unpairing_whose_connection_closes_is_cleaned_up_after_to_the_end() {
    let (store, federation, bob_s) = sharing("fed-pin-unpaired-close").await;
    let federation = Arc::new(federation);
    let router = answering(&federation).await;
    let bob = keyed(11).await;
    let (dialed, info) = Dialed::connect(&bob, router.endpoint().addr(), bob_s_hello(&bob_s))
        .await
        .unwrap();
    assert_eq!(info.standing, Standing::Paired);
    let (revoked, go_on) = federation.holds.unpairing.next();
    let connection = dialed.connection().clone();
    let closing = async {
        revoked.await.unwrap();
        connection.close(0u32.into(), b"gone");
        let (ended, go) = federation.holds.unpairing.next();
        go_on.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), ended)
            .await
            .expect("cleaned up after to the end")
            .unwrap();
        go.send(()).unwrap();
    };
    let (unpaired, ()) = tokio::join!(dialed.unpaired(), closing);
    assert!(unpaired.is_err(), "the connection closed first");
    assert!(
        store
            .live_peer_by_server_id(bob_s.as_str())
            .await
            .unwrap()
            .is_none()
    );
    router.shutdown().await.unwrap();
}

/// The audit's X8-01: Bob's server's old key connects to Alice's as her
/// admin confirms its new key, its pair looked up before the confirm and
/// the connection going on after it. Kept among the live connections
/// before the lookup, it is one the confirm closes: its hello is never
/// answered, and it is asked nothing. The new key is the pair.
#[tokio::test]
async fn an_old_key_connecting_as_the_new_one_is_confirmed_gets_no_standing() {
    let (store, federation, bob_s) = sharing("fed-pin-register").await;
    let federation = Arc::new(federation);
    let router = answering(&federation).await;
    let addr = router.endpoint().addr();
    let peer = store
        .live_peer_by_server_id(bob_s.as_str())
        .await
        .unwrap()
        .unwrap();
    // The pair's key, as `sharing` pinned it, and its new one.
    let (old, new) = (keyed(11).await, keyed(12).await);
    assert_eq!(old.id(), bob_s_key());
    store
        .hold_peer_key(&peer.id, &new.id().to_string())
        .await
        .unwrap();
    let (held, go) = federation.holds.knowing.next();
    let confirm = async {
        held.await.unwrap();
        federation
            .confirm_key(&peer.id, &new.id().to_string(), "alice")
            .await
            .unwrap();
        go.send(()).unwrap();
    };
    let (dialled, ()) = tokio::join!(
        Dialed::connect(&old, addr.clone(), bob_s_hello(&bob_s)),
        confirm
    );
    assert!(dialled.is_err(), "{:?}", dialled.map(|(_, info)| info));
    assert!(!federation.connected(&old.id().to_string()));
    let (dialled, info) = Dialed::connect(&new, addr, bob_s_hello(&bob_s))
        .await
        .unwrap();
    assert_eq!(info.standing, Standing::Paired);
    assert_eq!(dialled.libraries().await.unwrap().len(), 1);
    router.shutdown().await.unwrap();
}

/// X8-01 at the service: once the new key is confirmed, what the old key
/// asks on a connection it still holds is refused, as what no pair asks
/// is, and what the new key asks is answered. A sync of the old key's,
/// checked before the confirm and walked after it, sends nothing and
/// keeps nothing as sent.
#[tokio::test]
async fn a_request_from_a_key_the_pair_moved_from_is_refused_at_the_service() {
    use spoolframe_federation::{Shares, Unshared};
    use spoolframe_protocol::federation::{SubtitleSearchAsk, SyncLibrary};
    let (store, federation, bob_s) = sharing("fed-pin-service").await;
    let peer = store
        .live_peer_by_server_id(bob_s.as_str())
        .await
        .unwrap()
        .unwrap();
    let old = bob_s_key();
    let new = SecretKey::from_bytes(&[12; 32]).public();
    let sync = SyncLibrary {
        library: "plex:1".into(),
        since: None,
    };
    assert_eq!(
        Shares::libraries(&federation, &bob_s, old)
            .await
            .unwrap()
            .len(),
        1
    );
    store
        .hold_peer_key(&peer.id, &new.to_string())
        .await
        .unwrap();
    let (held, go) = federation.holds.answering.next();
    let confirm = async {
        held.await.unwrap();
        federation
            .confirm_key(&peer.id, &new.to_string(), "alice")
            .await
            .unwrap();
        go.send(()).unwrap();
    };
    let (synced, ()) = tokio::join!(Shares::sync(&federation, &bob_s, old, &sync), confirm);
    assert_eq!(synced.map(|a| a.items.len()), Err(Unshared::NotShared));
    assert_eq!(
        Shares::libraries(&federation, &bob_s, old)
            .await
            .map(|l| l.len()),
        Err(Unshared::NotShared)
    );
    assert_eq!(
        Shares::sync(&federation, &bob_s, old, &sync)
            .await
            .map(|a| a.mode),
        Err(Unshared::NotShared)
    );
    assert_eq!(
        Shares::artwork(&federation, &bob_s, old, "poster")
            .await
            .map(|(kind, _)| kind),
        Err(Unshared::NotShared)
    );
    let ask = SubtitleSearchAsk {
        user: uuid::Uuid::from_u128(7).to_string(),
        subtitles: Default::default(),
        item: movie().id().to_string(),
        language: None,
    };
    let searched = Shares::search_subtitles(&federation, &bob_s, old, &ask).await;
    assert!(
        matches!(searched, Err(Error::NotFound { .. })),
        "{searched:?}"
    );
    assert_eq!(
        Shares::libraries(&federation, &bob_s, new)
            .await
            .unwrap()
            .len(),
        1
    );
    Shares::sync(&federation, &bob_s, new, &sync).await.unwrap();
}

/// An assertion on the old key's connection, minted as the new key is
/// confirmed, mints nothing: a token is kept only while the key that
/// asserted it is the pair's. The new key's assertions mint.
#[tokio::test]
async fn an_assertion_of_the_old_key_as_a_new_one_is_confirmed_mints_nothing() {
    let (store, federation, bob_s) = sharing("fed-pin-mint").await;
    let peer = store
        .live_peer_by_server_id(bob_s.as_str())
        .await
        .unwrap()
        .unwrap();
    let new = SecretKey::from_bytes(&[12; 32]).public();
    store
        .hold_peer_key(&peer.id, &new.to_string())
        .await
        .unwrap();
    let wants = || bob_wants(device().to_string(), TokenScope::Play, 7);
    let (held, go) = federation.holds.minting.next();
    let confirm = async {
        held.await.unwrap();
        federation
            .confirm_key(&peer.id, &new.to_string(), "alice")
            .await
            .unwrap();
        go.send(()).unwrap();
    };
    let racing = wants();
    let (minted, ()) = tokio::join!(federation.mint(&bob_s, bob_s_key(), &racing), confirm);
    assert_eq!(minted.map(|g| g.scope), Err(AssertionRefused::NotShared));
    assert!(
        federation
            .mint(&bob_s, bob_s_key(), &wants())
            .await
            .is_err()
    );
    federation.mint(&bob_s, new, &wants()).await.unwrap();
}

/// This server's subtitle downloads, holding each user to `per_user` a
/// day, with no site of its own: its users download on the pair's.
fn subtitle_downloads(
    store: &Store,
    dir: &std::path::Path,
    per_user: u32,
) -> Arc<crate::subtitle_downloads::SubtitleDownloads> {
    use spoolframe_media::sandbox::Tool;

    use crate::subtitle_sync::{FfmpegSpeech, SubtitleTimings, SyncSettings};
    let timings = SubtitleTimings::new(
        store.clone(),
        Tool::unconfined("/nonexistent/ffmpeg"),
        Arc::new(FfmpegSpeech(Tool::unconfined("/nonexistent/ffmpeg"))),
        &SyncSettings {
            at_once: 1,
            dir: dir.join("subtitles"),
            speech_dir: dir.join("speech"),
        },
    )
    .unwrap();
    let downloads = crate::subtitle_downloads::SubtitleDownloads::new(
        store.clone(),
        Arc::new(FakeCatalog::new("/nowhere/movie.mkv".into())),
        Arc::new(spoolframe_media::Prober::new(
            Tool::unconfined("/nonexistent/ffprobe"),
            std::time::Duration::from_secs(10),
        )),
        Arc::new(timings),
        dir.join("subtitles").join("downloaded"),
        crate::subtitle_downloads::Settings {
            sites: Vec::new(),
            daily_limit_per_user: per_user,
            daily_limit_per_peer: 0,
        },
    )
    .unwrap();
    Arc::new(downloads)
}

/// Bob's download of Alice's film's subtitle, its request dropped once
/// Alice's server is asked, is counted here as Alice's server makes it:
/// work of its own carries the ask and the count, where the count went
/// with the request and Bob's next got past his limit.
#[tokio::test]
async fn a_download_asked_of_the_pair_counts_here_though_its_request_is_dropped() {
    let alice = Answering::start(31, SA).await;
    let (store, federation, peer, bob) = bob_s("fed-subs-dropped", &alice).await;
    let dir = spoolframe_test_dir::TestDir::new("spoolframe-fed-subs-dropped").await;
    federation.attach_subtitle_downloads(subtitle_downloads(&store, dir.path(), 1));
    federation.refresh(&peer.id).await.unwrap();
    let (bob_now, item) = (who(bob), alien());
    let mut download =
        Box::pin(federation.download_subtitle_of(&bob_now, &item, "opensubtitles:101"));
    tokio::select! {
        biased;
        done = &mut download => panic!("done before the pair was asked: {done:?}"),
        () = async {
            while !alice.heard().contains(&"subtitle asked") {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        } => {}
    }
    drop(download);
    let principal = crate::principal::Principal::LocalUser(bob).to_string();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while store
            .subtitle_downloads_today(&principal)
            .await
            .unwrap()
            .is_empty()
        {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("counted here once the pair answered");
    assert!(matches!(
        federation
            .download_subtitle_of(&who(bob), &alien(), "opensubtitles:102")
            .await,
        Err(Error::QuotaExhausted { .. })
    ));
}
