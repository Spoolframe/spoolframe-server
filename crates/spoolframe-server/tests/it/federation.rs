//! Federation (P8-01): two servers in this process pair over
//! `spoolframe/federation/1`, one redeeming the other's `federation`
//! invite; and what a stranger, a changed server id and a new key get.
//! Experimental: a server with `experimental.federation` off closes the
//! ALPN at once and makes no `federation` invite.

use std::sync::Arc;

use iroh::endpoint::{Connection, ConnectionError, presets};
use iroh::{Endpoint, RelayMode};
use spoolframe_federation::{DialError, Dialed};
use spoolframe_protocol::federation::{
    PROTOCOL_VERSION, PairingRefused, PeerHello, PeerRequest, RedeemPairing, Standing, Unpaired,
};
use spoolframe_protocol::invite::InviteLinkKind;
use spoolframe_protocol::{CLIENT_ALPN, Error, FEDERATION_ALPN, RedeemInvite, Request, Response};
use spoolframe_protocol::{close, fingerprint};
use spoolframe_server::admission::Admission;
use spoolframe_server::federation::{Federation, PairError};
use spoolframe_server::invites::{InviteRequest, create, server_link};
use spoolframe_server::store::invites::{InviteKind, InviteOrigin};

use crate::support::{ServerOptions, TestClient, TestServer, fresh_dir};

/// A server with federation on, called `name`.
async fn server(test: &str, name: &str) -> TestServer {
    TestServer::start_with(
        fresh_dir(test).await,
        ServerOptions {
            name: name.into(),
            federation: true,
            ..ServerOptions::default()
        },
    )
    .await
}

fn federation(server: &TestServer) -> &Arc<Federation> {
    server.federation.as_ref().expect("federation is on")
}

/// `server` walks a library at each sync: a test that changes it between
/// two syncs needs it.
fn no_walk_reuse(server: &TestServer) {
    let federation = federation(server);
    federation.set_limits(spoolframe_server::federation::Limits {
        walk_reuse: std::time::Duration::ZERO,
        ..federation.limits()
    });
}

/// A `federation` invite of `server`'s, made by alice: its link.
async fn pairing_link(server: &TestServer, uses: Option<u32>) -> String {
    let created = create(
        &server.store,
        InviteRequest {
            kind: InviteKind::Federation,
            note: None,
            uses,
            expires_secs: None,
            role: None,
        },
        "alice",
        &InviteOrigin::Web,
        server.federation.as_deref(),
    )
    .await
    .unwrap();
    let link = server_link(
        server.server.id().to_string(),
        server.name.get(),
        Some(server.server.told()),
    )
    .await;
    created.invite(link).link()
}

/// An endpoint of a server that is not one of these: what a stranger, or
/// a key that says it is a paired server's, dials from.
async fn endpoint() -> Endpoint {
    Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind()
        .await
        .unwrap()
}

fn hello(server_id: &str) -> PeerHello {
    PeerHello {
        protocol_version: PROTOCOL_VERSION,
        server_id: server_id.into(),
        name: "Somebody's server".into(),
    }
}

fn redeem(code: &str) -> RedeemPairing {
    RedeemPairing {
        code: code.into(),
        addrs: Vec::new(),
        relays: Vec::new(),
    }
}

/// The code a pairing link carries.
fn code_of(link: &str) -> String {
    spoolframe_protocol::invite::parse_link(link)
        .unwrap()
        .code
        .unwrap()
}

fn assertion(user: &str, item: &str) -> spoolframe_protocol::federation::AssertUser {
    spoolframe_protocol::federation::AssertUser {
        user: user.into(),
        display_name: "Bob".into(),
        client_endpoint: iroh::SecretKey::from_bytes(&[3; 32]).public().to_string(),
        item: item.into(),
        scope: spoolframe_protocol::federation::TokenScope::Play,
        preferences: None,
        download: None,
    }
}

/// P8-03: an assertion from an endpoint that is not a pair's is reset (and
/// counted as a stranger's stray, as `spoolframe-federation`'s tests
/// check); one from a pair naming a portable guest, or
/// no user, or an item not shared, is refused and counted against the
/// pair, and no token is minted.
#[tokio::test]
async fn assertions_sa_may_not_answer_are_refused_and_counted() {
    use spoolframe_protocol::federation::AssertionRefused;
    use spoolframe_server::store::federation_tokens::PlaySide;
    let sa = server("fed-assert-refused", "Alice's server").await;
    let bob = "0192f1c4-5b6e-7a3d-9c41-2e8f7b6a5d4c";
    let stranger = endpoint().await;
    let (dialed, _) = Dialed::connect(&stranger, sa.server.addr(), hello("bbbbbbbbbbbbbbbb"))
        .await
        .unwrap();
    assert!(matches!(
        dialed.assert_user(assertion(bob, "plex:movie:1")).await,
        Err(DialError::Broken(_))
    ));
    let link = pairing_link(&sa, None).await;
    dialed.redeem(redeem(&code_of(&link))).await.unwrap();
    let peer = sa
        .store
        .live_peer_by_endpoint(&stranger.id().to_string())
        .await
        .unwrap()
        .unwrap();
    federation(&sa)
        .share(&peer.id, &["plex:1".parse().unwrap()], "alice")
        .await
        .unwrap();
    let portable = format!("portable:{}", "ab".repeat(32));
    for (user, item, refused) in [
        (
            portable.as_str(),
            "plex:movie:1",
            AssertionRefused::InvalidUser,
        ),
        ("bob", "plex:movie:1", AssertionRefused::InvalidUser),
        (bob, "plex:track:32", AssertionRefused::NotShared),
        (bob, "plex:artist:30", AssertionRefused::InvalidScope),
        (
            bob,
            "fed:movie:cccccccccccccccc:plex:1",
            AssertionRefused::NotShared,
        ),
    ] {
        let answer = dialed.assert_user(assertion(user, item)).await;
        assert!(
            matches!(&answer, Err(DialError::NotGranted(reason)) if *reason == refused),
            "{user} {item}: {answer:?}"
        );
    }
    let counted = sa
        .store
        .federation_plays(&peer.id, PlaySide::Served)
        .await
        .unwrap();
    assert_eq!((counted.plays, counted.refused), (0, 5));
    // The pair's own user, for a shared film: a token.
    dialed
        .assert_user(assertion(bob, "plex:movie:1"))
        .await
        .unwrap();
    // Counted as a play once the token is redeemed (P8-04), not now.
    let counted = sa
        .store
        .federation_plays(&peer.id, PlaySide::Served)
        .await
        .unwrap();
    assert_eq!((counted.plays, counted.refused), (0, 5));
    dialed.close();
    sa.stop().await;
}

/// `server`'s count of the pairs' `request`s answered as `outcome`.
fn requests(server: &TestServer, request: &str, outcome: &str) -> f64 {
    server
        .metrics
        .families()
        .into_iter()
        .find(|f| f.name == "spoolframe_federation_requests_total")
        .and_then(|f| f.value(&[("request", request), ("outcome", outcome)]))
        .unwrap_or_else(|| panic!("no requests {request} {outcome}"))
}

/// A pair's requests are counted for the metrics by what they asked and
/// how they went: answered, or refused for what is not shared with it.
#[tokio::test]
async fn a_pair_s_requests_are_counted_answered_and_refused() {
    use spoolframe_protocol::federation::AssertionRefused;
    let sa = server("fed-requests-counted", "Alice's server").await;
    let paired = endpoint().await;
    let (dialed, _) = Dialed::connect(&paired, sa.server.addr(), hello("bbbbbbbbbbbbbbbb"))
        .await
        .unwrap();
    let link = pairing_link(&sa, None).await;
    dialed.redeem(redeem(&code_of(&link))).await.unwrap();
    let peer = sa
        .store
        .live_peer_by_endpoint(&paired.id().to_string())
        .await
        .unwrap()
        .unwrap();
    federation(&sa)
        .share(&peer.id, &["plex:1".parse().unwrap()], "alice")
        .await
        .unwrap();
    assert_eq!(requests(&sa, "libraries", "ok"), 0.0);
    assert_eq!(dialed.libraries().await.unwrap().len(), 1);
    assert_eq!(requests(&sa, "libraries", "ok"), 1.0);
    // An image of nothing SA sent it.
    assert!(matches!(
        dialed.artwork("plex:art:404").await,
        Err(DialError::NotShared)
    ));
    assert_eq!(requests(&sa, "artwork", "refused"), 1.0);
    assert_eq!(requests(&sa, "artwork", "ok"), 0.0);
    // A track of a library not shared with it.
    let bob = "0192f1c4-5b6e-7a3d-9c41-2e8f7b6a5d4c";
    assert!(matches!(
        dialed.assert_user(assertion(bob, "plex:track:32")).await,
        Err(DialError::NotGranted(AssertionRefused::NotShared))
    ));
    assert_eq!(requests(&sa, "assertion", "refused"), 1.0);
    dialed.close();
    sa.stop().await;
}

/// The full dance: SA's invite, redeemed by SB, and both keep the pair
/// with the other's key and server id, so both admins see the same
/// fingerprints.
#[tokio::test]
async fn two_servers_pair_with_an_invite_and_both_keep_the_pair() {
    let sa = server("fed-pair-sa", "Alice's server").await;
    let sb = server("fed-pair-sb", "Bob's server").await;
    let link = pairing_link(&sa, None).await;
    let read = spoolframe_protocol::invite::parse_link(&link).unwrap();
    assert_eq!((read.kind, read.role), (InviteLinkKind::Federation, None));

    let on_sb = federation(&sb).redeem_link(&link, "bob").await.unwrap();
    assert_eq!(on_sb.endpoint_id, sa.server.id().to_string());
    assert_eq!(on_sb.server_id, sa.server_id.as_str());
    assert_eq!(on_sb.display_name, "Alice's server");
    assert_eq!((on_sb.paired_by.as_str(), on_sb.invite_id), ("bob", None));

    let on_sa = sa
        .store
        .live_peer_by_endpoint(&sb.server.id().to_string())
        .await
        .unwrap()
        .expect("SA keeps SB's row");
    assert_eq!(on_sa.server_id, sb.server_id.as_str());
    assert_eq!(on_sa.display_name, "Bob's server");
    assert_eq!(on_sa.paired_by, "alice");
    let invites = sa.store.live_invites().await.unwrap();
    assert!(invites.is_empty(), "one use, and it is used");
    assert!(on_sa.invite_id.is_some());
    // SB told SA where it is, so SA can dial it back.
    assert!(!on_sa.addrs.is_empty());

    // What each admin reads off the other's row is the other's own key.
    assert_eq!(
        fingerprint::of_hex(&on_sb.endpoint_id),
        Some(fingerprint::fingerprint(sa.server.id().as_bytes()))
    );
    assert_eq!(
        fingerprint::of_hex(&on_sa.endpoint_id),
        Some(fingerprint::fingerprint(sb.server.id().as_bytes()))
    );

    // Paired already: the second link is not redeemed, and keeps its use.
    let again = pairing_link(&sa, None).await;
    assert!(matches!(
        federation(&sb).redeem_link(&again, "bob").await,
        Err(PairError::AlreadyPaired)
    ));
    assert_eq!(sa.store.live_invites().await.unwrap()[0].uses_left, 1);
    sa.stop().await;
    sb.stop().await;
}

#[tokio::test]
async fn a_link_that_is_not_one_to_pair_with_is_refused_before_dialling() {
    let sa = server("fed-bad-links-sa", "Alice's server").await;
    let sb = server("fed-bad-links-sb", "Bob's server").await;
    let link = pairing_link(&sa, None).await;
    let fed = federation(&sb);
    assert!(matches!(
        fed.redeem_link("https://example.com", "bob").await,
        Err(PairError::NotALink)
    ));
    let (without_code, _) = link.split_once("&cap=").unwrap();
    assert!(matches!(
        fed.redeem_link(without_code, "bob").await,
        Err(PairError::NoCode)
    ));
    let own = pairing_link(&sb, None).await;
    assert!(matches!(
        fed.redeem_link(&own, "bob").await,
        Err(PairError::Itself)
    ));
    sa.stop().await;
    sb.stop().await;
}

/// A wrong code is a wrong guess on the count a device's invite codes
/// share: after the limit even the right code is refused, over federation
/// and over the client protocol alike, from that endpoint. A stream a
/// stranger may not open is counted the same way.
#[tokio::test]
async fn wrong_codes_and_strays_count_on_the_endpoint_s_invite_guesses() {
    let sa = server("fed-guesses", "Alice's server").await;
    let link = pairing_link(&sa, Some(2)).await;
    let stranger = endpoint().await;
    let (dialed, info) = Dialed::connect(&stranger, sa.server.addr(), hello("bbbbbbbbbbbbbbbb"))
        .await
        .unwrap();
    assert_eq!(info.standing, Standing::Stranger);
    assert_eq!(info.server_id, sa.server_id.as_str());
    for _ in 0..3 {
        assert!(matches!(
            dialed.redeem(redeem("AAAA-AAAA-AAAA")).await,
            Err(DialError::Refused(PairingRefused::InviteInvalid))
        ));
    }
    // §28: a stranger reaches nothing else; the stream is reset and the
    // ask counted.
    for _ in 0..2 {
        let (mut send, mut recv) = dialed.connection().open_bi().await.unwrap();
        spoolframe_protocol::codec::write_frame(&mut send, &PeerRequest::Unpaired(Unpaired {}))
            .await
            .unwrap();
        send.finish().unwrap();
        assert!(
            spoolframe_protocol::codec::read_frame::<_, spoolframe_protocol::federation::PeerReply>(
                &mut recv
            )
            .await
            .is_err()
        );
    }
    assert!(matches!(
        dialed.redeem(redeem(&code_of(&link))).await,
        Err(DialError::Refused(PairingRefused::RateLimited { .. }))
    ));
    // The same endpoint on the client protocol: its invite guesses are
    // the same count.
    let conn = stranger
        .connect(sa.server.addr(), CLIENT_ALPN)
        .await
        .unwrap();
    let device = TestClient {
        endpoint: stranger.clone(),
        conn,
        instance: Arc::default(),
        controls: Arc::default(),
    };
    device.hello().await;
    assert!(matches!(
        device
            .call(
                None,
                Request::RedeemInvite(RedeemInvite {
                    code: "AAAA-AAAA-AAAA".into(),
                    username: "mallory".into(),
                    display_name: None,
                    password: "correct horse battery".into(),
                })
            )
            .await,
        Response::Err(Error::RateLimited { .. })
    ));
    // Another server is not held to it, and pairs.
    let sb = endpoint().await;
    let (dialed, _) = Dialed::connect(&sb, sa.server.addr(), hello("cccccccccccccccc"))
        .await
        .unwrap();
    dialed.redeem(redeem(&code_of(&link))).await.unwrap();
    sa.stop().await;
}

/// Whether `request`, sent on a stream of its own on `dialed`, is reset
/// rather than answered.
async fn reset(dialed: &Dialed, request: &PeerRequest) -> bool {
    let (mut send, mut recv) = dialed.connection().open_bi().await.unwrap();
    spoolframe_protocol::codec::write_frame(&mut send, request)
        .await
        .unwrap();
    send.finish().unwrap();
    spoolframe_protocol::codec::read_frame::<_, spoolframe_protocol::federation::PeerReply>(
        &mut recv,
    )
    .await
    .is_err()
}

/// P8-02's requests, from anyone but a live pair (§28): a stranger and a
/// held key are reset, as for anything else they may not ask.
#[tokio::test]
async fn replication_is_reset_for_a_stranger_and_a_held_key() {
    use spoolframe_protocol::federation::{ArtworkFetch, SyncLibrary};
    let sa = server("fed-replication-strangers", "Alice's server").await;
    let link = pairing_link(&sa, None).await;
    let asks = [
        PeerRequest::Libraries,
        PeerRequest::Sync(SyncLibrary {
            library: "plex:1".into(),
            since: None,
        }),
        PeerRequest::Artwork(ArtworkFetch {
            id: spoolframe_server::test_support::POSTER.into(),
        }),
    ];
    let stranger = endpoint().await;
    let (dialed, _) = Dialed::connect(&stranger, sa.server.addr(), hello("bbbbbbbbbbbbbbbb"))
        .await
        .unwrap();
    for ask in &asks {
        assert!(reset(&dialed, ask).await, "{ask:?}");
    }
    dialed.redeem(redeem(&code_of(&link))).await.unwrap();
    let held = endpoint().await;
    let (dialed, info) = Dialed::connect(&held, sa.server.addr(), hello("bbbbbbbbbbbbbbbb"))
        .await
        .unwrap();
    assert_eq!(info.standing, Standing::KeyHeld);
    for ask in &asks {
        assert!(reset(&dialed, ask).await, "{ask:?}");
    }
    sa.stop().await;
}

/// A stray is counted only within the strangers' budget: keys made by the
/// thousand cannot fill the limiter, which makes room by forgetting the
/// oldest counts, faster than sign-in attempts can.
#[tokio::test]
async fn strays_past_the_strangers_budget_are_not_counted() {
    use spoolframe_server::auth::{Bucket, MAX_FAILURES};
    let sa = TestServer::start_with(
        fresh_dir("fed-stray-budget").await,
        ServerOptions {
            name: "Alice's server".into(),
            federation: true,
            stranger_attempts: Bucket {
                capacity: MAX_FAILURES as u32 - 1,
                refill_every: std::time::Duration::from_secs(3600),
            },
            ..ServerOptions::default()
        },
    )
    .await;
    let stranger = endpoint().await;
    let (dialed, _) = Dialed::connect(&stranger, sa.server.addr(), hello("bbbbbbbbbbbbbbbb"))
        .await
        .unwrap();
    for _ in 0..MAX_FAILURES + 2 {
        let (mut send, mut recv) = dialed.connection().open_bi().await.unwrap();
        spoolframe_protocol::codec::write_frame(&mut send, &PeerRequest::Unpaired(Unpaired {}))
            .await
            .unwrap();
        send.finish().unwrap();
        let _ = spoolframe_protocol::codec::read_frame::<
            _,
            spoolframe_protocol::federation::PeerReply,
        >(&mut recv)
        .await;
    }
    let source = stranger.id().to_string();
    assert_eq!(
        sa.auth
            .attempt_limited(&format!("invite@{source}"), Some(&source)),
        None,
        "only the budget's worth of strays was counted"
    );
    sa.stop().await;
}

/// PX-14's pin pair: once paired, the same key under another server id is
/// refused every time, and its connection closed.
#[tokio::test]
async fn a_paired_key_under_another_server_id_is_refused_every_time() {
    let sa = server("fed-server-id-changed", "Alice's server").await;
    let link = pairing_link(&sa, None).await;
    let sb = endpoint().await;
    let (dialed, _) = Dialed::connect(&sb, sa.server.addr(), hello("bbbbbbbbbbbbbbbb"))
        .await
        .unwrap();
    dialed.redeem(redeem(&code_of(&link))).await.unwrap();
    dialed.close();
    let (dialed, info) = Dialed::connect(&sb, sa.server.addr(), hello("bbbbbbbbbbbbbbbb"))
        .await
        .unwrap();
    assert_eq!(info.standing, Standing::Paired);
    dialed.close();
    for _ in 0..3 {
        let refused = Dialed::connect(&sb, sa.server.addr(), hello("dddddddddddddddd")).await;
        assert!(
            matches!(
                refused,
                Err(DialError::Refused(PairingRefused::ServerIdChanged))
            ),
            "{refused:?}"
        );
    }
    let conn = sb.connect(sa.server.addr(), FEDERATION_ALPN).await.unwrap();
    let (mut send, _recv) = conn.open_bi().await.unwrap();
    spoolframe_protocol::codec::write_frame(
        &mut send,
        &PeerRequest::Hello(hello("dddddddddddddddd")),
    )
    .await
    .unwrap();
    send.finish().unwrap();
    assert!(matches!(
        conn.closed().await,
        ConnectionError::ApplicationClosed(c) if c.error_code == close::SERVER_ID_CHANGED.into()
    ));
    // The pin is as it was.
    let row = sa
        .store
        .live_peer_by_endpoint(&sb.id().to_string())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.server_id, "bbbbbbbbbbbbbbbb");
    // Kept for the Federation page, which says so with Unpair.
    let (_, refused_at) = sa.store.peer_presence(&row.id).await.unwrap();
    assert!(refused_at.is_some());
    sa.stop().await;
}

/// A known server id from a new key is held for SA's admin, and reaches
/// only `Hello`: its redemption is reset and counted, and pairs nothing.
#[tokio::test]
async fn a_new_key_for_a_paired_server_id_is_held_and_reaches_only_hello() {
    let sa = server("fed-key-held", "Alice's server").await;
    let link = pairing_link(&sa, Some(2)).await;
    let old = endpoint().await;
    let (dialed, _) = Dialed::connect(&old, sa.server.addr(), hello("bbbbbbbbbbbbbbbb"))
        .await
        .unwrap();
    dialed.redeem(redeem(&code_of(&link))).await.unwrap();
    let new = endpoint().await;
    let (dialed, info) = Dialed::connect(&new, sa.server.addr(), hello("bbbbbbbbbbbbbbbb"))
        .await
        .unwrap();
    assert_eq!(info.standing, Standing::KeyHeld);
    let row = sa
        .store
        .live_peer_by_server_id("bbbbbbbbbbbbbbbb")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.endpoint_id, old.id().to_string());
    let held = sa.store.held_peer_keys(&row.id).await.unwrap();
    assert_eq!(
        held.iter()
            .map(|k| k.endpoint_id.clone())
            .collect::<Vec<_>>(),
        [new.id().to_string()]
    );
    assert!(matches!(
        dialed.redeem(redeem(&code_of(&link))).await,
        Err(DialError::Broken(_))
    ));
    assert_eq!(sa.store.live_invites().await.unwrap()[0].uses_left, 1);
    assert!(
        sa.store
            .live_peer_by_endpoint(&new.id().to_string())
            .await
            .unwrap()
            .is_none()
    );
    sa.stop().await;
}

/// SA's paired server id, from keys made one after another: each hello
/// spends from the strangers' budget, so turning keys does not escape
/// the count each key has; and past the held keys' limit, a new key is
/// refused, not swapped in for one the admin may be looking at.
#[tokio::test]
async fn new_keys_for_a_paired_server_id_spend_the_strangers_budget_and_are_capped() {
    use spoolframe_server::auth::Bucket;
    use spoolframe_server::store::federation::MAX_HELD_KEYS;
    let sa = TestServer::start_with(
        fresh_dir("fed-key-budget").await,
        ServerOptions {
            name: "Alice's server".into(),
            federation: true,
            stranger_attempts: Bucket {
                capacity: MAX_HELD_KEYS as u32 + 2,
                refill_every: std::time::Duration::from_secs(3600),
            },
            ..ServerOptions::default()
        },
    )
    .await;
    let link = pairing_link(&sa, None).await;
    let old = endpoint().await;
    let (dialed, _) = Dialed::connect(&old, sa.server.addr(), hello("bbbbbbbbbbbbbbbb"))
        .await
        .unwrap();
    // The redemption spends one.
    dialed.redeem(redeem(&code_of(&link))).await.unwrap();
    let mut keys = Vec::new();
    for _ in 0..MAX_HELD_KEYS {
        let new = endpoint().await;
        let (_, info) = Dialed::connect(&new, sa.server.addr(), hello("bbbbbbbbbbbbbbbb"))
            .await
            .unwrap();
        assert_eq!(info.standing, Standing::KeyHeld);
        keys.push(new.id().to_string());
    }
    let late = endpoint().await;
    assert!(matches!(
        Dialed::connect(&late, sa.server.addr(), hello("bbbbbbbbbbbbbbbb")).await,
        Err(DialError::Refused(PairingRefused::KeyRefused))
    ));
    let row = sa
        .store
        .live_peer_by_server_id("bbbbbbbbbbbbbbbb")
        .await
        .unwrap()
        .unwrap();
    let held = sa.store.held_peer_keys(&row.id).await.unwrap();
    assert_eq!(
        held.iter()
            .map(|k| k.endpoint_id.clone())
            .collect::<Vec<_>>(),
        keys
    );
    // The budget is spent: the next key is refused before the store is
    // asked, even one held already.
    let again = endpoint().await;
    assert!(matches!(
        Dialed::connect(&again, sa.server.addr(), hello("bbbbbbbbbbbbbbbb")).await,
        Err(DialError::Refused(PairingRefused::RateLimited { .. }))
    ));
    sa.stop().await;
}

/// Two servers with one server id (a copied state directory) never pair.
#[tokio::test]
async fn a_server_with_this_server_s_id_does_not_pair() {
    let sb = server("fed-same-id-sb", "Bob's server").await;
    let dir = fresh_dir("fed-same-id-sa").await;
    tokio::fs::write(
        dir.join(spoolframe_server::identity::SERVER_ID_FILE),
        format!("{}\n", sb.server_id),
    )
    .await
    .unwrap();
    let sa = TestServer::start_with(
        dir,
        ServerOptions {
            name: "Alice's server".into(),
            federation: true,
            ..ServerOptions::default()
        },
    )
    .await;
    assert_eq!(sa.server_id, sb.server_id);
    let link = pairing_link(&sa, None).await;
    assert!(matches!(
        federation(&sb).redeem_link(&link, "bob").await,
        Err(PairError::Dial(DialError::Refused(
            PairingRefused::SameServerId
        )))
    ));
    assert!(sa.store.federation_peers().await.unwrap().is_empty());
    assert!(sb.store.federation_peers().await.unwrap().is_empty());
    sa.stop().await;
    sb.stop().await;
}

/// SA paired, and SB cannot keep its side: SB tells SA, so neither is
/// left half paired.
#[tokio::test]
async fn a_pair_the_redeemer_cannot_keep_is_unpaired_on_the_other_side() {
    let sa = server("fed-half-sa", "Alice's server").await;
    let sb = server("fed-half-sb", "Bob's server").await;
    let link = pairing_link(&sa, None).await;
    // SB's store refuses every new pair row.
    let pool = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::new()
            .filename(sb.dir.join(spoolframe_server::store::DB_FILE)),
    )
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER refuse_pairs BEFORE INSERT ON federation_peers \
         BEGIN SELECT RAISE(ABORT, 'refused for the test'); END",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert!(matches!(
        federation(&sb).redeem_link(&link, "bob").await,
        Err(PairError::Store(_))
    ));
    assert!(sb.store.federation_peers().await.unwrap().is_empty());
    let on_sa = sa.store.federation_peers().await.unwrap();
    assert_eq!(on_sa.len(), 1, "SA paired");
    assert!(!on_sa[0].live(), "and was told to unpair");
    sa.stop().await;
    sb.stop().await;
}

/// `endpoint`, paired with `sa` as `server_id` by a use of `link`.
async fn paired(sa: &TestServer, link: &str, server_id: &str) -> Endpoint {
    let sb = endpoint().await;
    let (dialed, _) = Dialed::connect(&sb, sa.server.addr(), hello(server_id))
        .await
        .unwrap();
    dialed.redeem(redeem(&code_of(link))).await.unwrap();
    dialed.close();
    sb
}

/// A connection from `from` to `sa`'s federation ALPN, its `hello` as
/// `server_id` answered paired; or `Err` once it is closed `BUSY`.
async fn busy(sa: &TestServer, from: &Endpoint, server_id: &str) -> Result<Connection, ()> {
    use spoolframe_protocol::codec::{read_frame, write_frame};
    use spoolframe_protocol::federation::PeerReply;
    let conn = from
        .connect(sa.server.addr(), FEDERATION_ALPN)
        .await
        .unwrap();
    let said = async {
        let (mut send, mut recv) = conn.open_bi().await.ok()?;
        write_frame(&mut send, &PeerRequest::Hello(hello(server_id)))
            .await
            .ok()?;
        send.finish().ok()?;
        read_frame::<_, PeerReply>(&mut recv).await.ok()
    };
    match said.await {
        Some(PeerReply::Info(info)) => {
            assert_eq!(info.standing, Standing::Paired);
            Ok(conn)
        }
        other => {
            let closed = conn.closed().await;
            assert!(
                matches!(
                    &closed,
                    ConnectionError::ApplicationClosed(c) if c.error_code == close::BUSY.into()
                ),
                "{other:?}, {closed:?}"
            );
            Err(())
        }
    }
}

/// F8-07: a paired server's connections are capped, per key and in all,
/// apart from the strangers' cap: one past either is closed `BUSY`, and
/// another pair, a stranger and a device still get in. Once one closes,
/// the next is let in.
#[tokio::test]
async fn a_paired_server_s_connections_are_capped_apart_from_the_strangers() {
    let sa = server("fed-pair-cap", "Alice's server").await;
    let link = pairing_link(&sa, Some(2)).await;
    let bob_s = paired(&sa, &link, "bbbbbbbbbbbbbbbb").await;
    let carol_s = paired(&sa, &link, "cccccccccccccccc").await;
    let mut held = Vec::new();
    for _ in 0..Admission::DEFAULT_MAX_PAIR_CONNECTIONS {
        held.push(busy(&sa, &bob_s, "bbbbbbbbbbbbbbbb").await.expect("let in"));
    }
    assert!(busy(&sa, &bob_s, "bbbbbbbbbbbbbbbb").await.is_err());
    // Another pair's, a stranger's and a device's are their own.
    let carol = busy(&sa, &carol_s, "cccccccccccccccc")
        .await
        .expect("let in");
    let stranger = endpoint().await;
    let (_stranger, info) = Dialed::connect(&stranger, sa.server.addr(), hello("dddddddddddddddd"))
        .await
        .unwrap();
    assert_eq!(info.standing, Standing::Stranger);
    let device = sa.client().await;
    device.hello().await;
    // One of Bob's closed, the next is let in.
    held.remove(0).close(0u32.into(), b"done");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Ok(again) = busy(&sa, &bob_s, "bbbbbbbbbbbbbbbb").await {
                break again;
            }
        }
    })
    .await
    .expect("let in once one closed");
    drop((held, carol));
    sa.stop().await;
}

/// F8-07: every pair's connections together are capped too: with room
/// for three, Bob's two and Carol's one fill it, and Carol's second is
/// closed `BUSY` though her key has room; a stranger still gets in.
#[tokio::test]
async fn every_pair_s_connections_together_are_capped() {
    let sa = TestServer::start_with(
        fresh_dir("fed-pairs-cap").await,
        ServerOptions {
            name: "Alice's server".into(),
            federation: true,
            admission: Admission {
                max_pair_connections: 2,
                max_paired_connections: 3,
                ..Admission::default()
            },
            ..ServerOptions::default()
        },
    )
    .await;
    let link = pairing_link(&sa, Some(2)).await;
    let bob_s = paired(&sa, &link, "bbbbbbbbbbbbbbbb").await;
    let carol_s = paired(&sa, &link, "cccccccccccccccc").await;
    let _held = [
        busy(&sa, &bob_s, "bbbbbbbbbbbbbbbb").await.expect("let in"),
        busy(&sa, &bob_s, "bbbbbbbbbbbbbbbb").await.expect("let in"),
        busy(&sa, &carol_s, "cccccccccccccccc")
            .await
            .expect("let in"),
    ];
    assert!(busy(&sa, &carol_s, "cccccccccccccccc").await.is_err());
    let stranger = endpoint().await;
    let (_stranger, info) = Dialed::connect(&stranger, sa.server.addr(), hello("dddddddddddddddd"))
        .await
        .unwrap();
    assert_eq!(info.standing, Standing::Stranger);
    sa.stop().await;
}

/// Off, as in 0.14.4: the ALPN closes at once, and no `federation` invite
/// is made.
#[tokio::test]
async fn with_federation_off_the_alpn_closes_and_no_invite_is_made() {
    let sa = TestServer::start("fed-off").await;
    assert!(sa.federation.is_none());
    let refused = create(
        &sa.store,
        InviteRequest {
            kind: InviteKind::Federation,
            note: None,
            uses: None,
            expires_secs: None,
            role: None,
        },
        "alice",
        &InviteOrigin::Web,
        None,
    )
    .await;
    assert!(refused.is_err());
    assert!(sa.store.live_invites().await.unwrap().is_empty());
    let sb = endpoint().await;
    let conn = sb.connect(sa.server.addr(), FEDERATION_ALPN).await.unwrap();
    assert!(matches!(
        conn.closed().await,
        ConnectionError::ApplicationClosed(c)
            if c.error_code == close::UNSUPPORTED_PROTOCOL.into()
    ));
    sa.stop().await;
}

/// A `federation` invite pairs a server: it takes no role.
#[tokio::test]
async fn a_federation_invite_takes_no_role() {
    let sa = server("fed-no-role", "Alice's server").await;
    let refused = create(
        &sa.store,
        InviteRequest {
            kind: InviteKind::Federation,
            note: None,
            uses: None,
            expires_secs: None,
            role: Some(spoolframe_types::Role::Consumer),
        },
        "alice",
        &InviteOrigin::Web,
        sa.federation.as_deref(),
    )
    .await;
    assert!(
        matches!(&refused, Err(spoolframe_server::invites::CreateError::Invalid(m)) if m.contains("role")),
        "{refused:?}"
    );
    sa.stop().await;
}

/// P8-03's share call for the QR flow, on the client protocol: an admin of
/// SB, recently signed in, from an app that names the feature, shares
/// SB's Movies with SA, which SB just paired with. Anyone else, or an app
/// that does not name the feature, shares nothing.
#[tokio::test]
async fn an_admin_s_app_shares_libraries_with_a_paired_server() {
    use spoolframe_protocol::federation::ShareLibraries;
    use spoolframe_protocol::{Reply, experimental::FEDERATION};
    const PASSWORD: &str = "correct horse";
    let sa = server("fed-share-sa", "Alice's server").await;
    let sb = server("fed-share-sb", "Bob's server").await;
    sb.add_user("bob", PASSWORD, None).await;
    sb.store
        .set_role("bob", spoolframe_types::Role::Admin)
        .await
        .unwrap();
    sb.add_user("guest", PASSWORD, None).await;
    federation(&sb)
        .redeem_link(&pairing_link(&sa, None).await, "bob")
        .await
        .unwrap();
    let movies: spoolframe_types::LibraryId = "plex:1".parse().unwrap();
    let share = |server_id: &str, library: &spoolframe_types::LibraryId| {
        Request::ShareLibraries(ShareLibraries {
            server_id: server_id.into(),
            libraries: vec![library.clone()],
        })
    };
    let sa_id = sa.server_id.as_str();

    let app = sb.client().await;
    app.hello_naming(crate::support::mkv_capabilities(), &[FEDERATION])
        .await;
    let bob = app.login("bob", PASSWORD).await;
    assert_eq!(
        app.call(Some(&bob), share(sa_id, &movies)).await,
        Response::Ok(Reply::LibrariesShared {
            libraries: vec![movies.clone()]
        })
    );
    let peer = sb
        .store
        .live_peer_by_server_id(sa_id)
        .await
        .unwrap()
        .unwrap();
    let grants = sb.store.live_grants(&peer.id).await.unwrap();
    assert_eq!(
        grants
            .iter()
            .map(|g| (g.library.as_str(), g.granted_by.as_str()))
            .collect::<Vec<_>>(),
        [("plex:1", "bob")]
    );
    assert!(matches!(
        app.call(Some(&bob), share(sa_id, &"plex:9".parse().unwrap()))
            .await,
        Response::Err(Error::InvalidInput { .. })
    ));
    assert!(matches!(
        app.call(Some(&bob), share("aaaaaaaaaaaaaaaa", &movies))
            .await,
        Response::Err(Error::NotFound { .. })
    ));
    let guest = app.login("guest", PASSWORD).await;
    assert!(matches!(
        app.call(Some(&guest), share(sa_id, &movies)).await,
        Response::Err(Error::AdminRequired)
    ));

    // An app that does not name the feature is not heard.
    let plain = sb.client().await;
    plain.hello().await;
    let bob = plain.login("bob", PASSWORD).await;
    assert!(matches!(
        plain.call(Some(&bob), share(sa_id, &movies)).await,
        Response::Err(Error::Unsupported { .. })
    ));

    // Nor is an admin who did not sign in recently.
    sb.clock
        .advance(spoolframe_server::auth::RECENT_SIGN_IN + std::time::Duration::from_secs(1));
    let bob = app.login("bob", PASSWORD).await;
    sb.clock
        .advance(spoolframe_server::auth::RECENT_SIGN_IN + std::time::Duration::from_secs(1));
    assert!(matches!(
        app.call(Some(&bob), share(sa_id, &movies)).await,
        Response::Err(Error::RecentSignInRequired)
    ));
    sa.stop().await;
    sb.stop().await;
}

// P8-01's admin surface: the `federation_*` methods, each server with its
// admin API beside it, its admin signed in from the web UI.

mod admin {
    use std::net::SocketAddr;
    use std::time::Duration;

    use jsonrpsee::ws_client::{HeaderMap, WsClient, WsClientBuilder};
    use spoolframe_admin_api::{
        AdminApiClient, FederationApiClient, InviteKind as ApiKind, MeApiClient, NewInvite,
        PeerEntry, Role, error_code,
    };
    use spoolframe_server::admin::{Admin, Origins, StatusSource, TrustedProxies};
    use spoolframe_server::auth::hash_password;
    use spoolframe_server::relay_access::RelayAccess;

    use super::*;
    use crate::admin::{anonymous, code, creds};
    use crate::support;

    const PUBLIC_URL: &str = "https://media.example.org";
    const PASSWORD: &str = "correct horse";

    /// A server with federation on (or not), `admin` its admin, and the
    /// admin API beside it, `admin` signed in from its web UI just now.
    struct Site {
        server: TestServer,
        addr: SocketAddr,
        admin: WsClient,
    }

    impl Site {
        async fn start(test: &str, name: &str, admin: &str, federation: bool) -> Self {
            let server = TestServer::start_with(
                fresh_dir(test).await,
                ServerOptions {
                    name: name.into(),
                    federation,
                    ..ServerOptions::default()
                },
            )
            .await;
            let phc = hash_password(PASSWORD).await.unwrap();
            server
                .store
                .add_user(admin, None, &phc, Role::Admin)
                .await
                .unwrap();
            server
                .store
                .add_user("guest", None, &phc, Role::Consumer)
                .await
                .unwrap();
            let status = StatusSource::new(
                server.name.clone(),
                server.server_id.clone(),
                support::transcoding(),
            );
            status.set_endpoint_id(server.server.id().to_string());
            status.set_told(server.server.told());
            let (addr, handle) = spoolframe_server::admin::serve(
                "127.0.0.1:0".parse().unwrap(),
                Admin {
                    status: std::sync::Arc::new(status),
                    federation: server.federation.clone(),
                    ..crate::admin::admin(server.auth.clone(), None)
                },
                Origins::Exactly(PUBLIC_URL.to_owned()),
                TrustedProxies::default(),
            )
            .await
            .unwrap();
            std::mem::forget(handle);
            let admin = Self::page(addr, admin).await;
            Self {
                server,
                addr,
                admin,
            }
        }

        /// `username` signed in from the web UI's page.
        async fn page(addr: SocketAddr, username: &str) -> WsClient {
            let token = anonymous(addr)
                .await
                .login(creds(username, PASSWORD))
                .await
                .unwrap()
                .token;
            let mut headers = HeaderMap::new();
            headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
            headers.insert("origin", PUBLIC_URL.parse().unwrap());
            WsClientBuilder::default()
                .set_headers(headers)
                .build(format!("ws://{addr}"))
                .await
                .unwrap()
        }

        /// A `federation` invite made in the web UI: its link.
        async fn invite(&self) -> String {
            self.admin
                .invite_create(NewInvite {
                    kind: ApiKind::Federation,
                    note: Some("Bob's server".into()),
                    uses: None,
                    expires: None,
                    role: None,
                })
                .await
                .unwrap()
                .link
        }

        async fn peers(&self) -> Vec<PeerEntry> {
            self.admin.peer_list().await.unwrap()
        }

        /// This server's paired row for `other`, once it is unpaired:
        /// telling it goes on after the call that unpaired.
        async fn unpaired_from(&self, other: &TestServer) -> PeerEntry {
            let key = other.server.id().to_string();
            for _ in 0..100 {
                let peers = self.peers().await;
                if let Some(peer) = peers.iter().find(|p| p.endpoint_id == key)
                    && peer.revoked_at.is_some()
                {
                    return peer.clone();
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            panic!("{} still pairs with the other", self.server.name.get());
        }
    }

    async fn pair(sa: &Site, sb: &Site) -> PeerEntry {
        let link = sa.invite().await;
        sb.admin.redeem(link).await.unwrap()
    }

    /// The full dance from two admins: invite, redeem, and both see the
    /// pair with each other's own fingerprint, who paired it and how.
    #[tokio::test]
    async fn two_admins_pair_and_see_matching_fingerprints() {
        let sa = Site::start("fed-admin-pair-sa", "Alice's server", "alice", true).await;
        let sb = Site::start("fed-admin-pair-sb", "Bob's server", "bob", true).await;
        let on_sb = pair(&sa, &sb).await;
        let on_sa = sa.peers().await.remove(0);
        let sa_status = sa.admin.server_status().await.unwrap();
        let sb_status = sb.admin.server_status().await.unwrap();
        assert_eq!(Some(on_sb.fingerprint.clone()), sa_status.fingerprint);
        assert_eq!(Some(on_sa.fingerprint.clone()), sb_status.fingerprint);
        assert_eq!(
            (on_sb.name.as_str(), on_sb.paired_by.as_str()),
            ("Alice's server", "bob")
        );
        assert_eq!(
            (on_sa.name.as_str(), on_sa.paired_by.as_str()),
            ("Bob's server", "alice")
        );
        assert!(on_sa.invite_id.is_some() && on_sb.invite_id.is_none());
        assert_eq!(sb.peers().await, [on_sb]);
        sa.server.stop().await;
        sb.server.stop().await;
    }

    /// "Invite a server" in the web UI opens Invites' form: a server's
    /// invite keeps the note, uses and expiry typed there, and is listed
    /// with them. A role is refused, as the form sends none.
    #[tokio::test]
    async fn the_web_ui_s_server_invite_keeps_its_note_uses_and_expiry() {
        const DAY: u64 = 24 * 3600;
        let sa = Site::start("fed-admin-invite-form", "Alice's server", "alice", true).await;
        let made = sa
            .admin
            .invite_create(NewInvite {
                kind: ApiKind::Federation,
                note: Some("Bob's server".into()),
                uses: Some(2),
                expires: Some(DAY),
                role: None,
            })
            .await
            .unwrap()
            .invite;
        let listed = sa.admin.invite_list().await.unwrap();
        assert_eq!(listed, std::slice::from_ref(&made));
        assert_eq!(
            (made.kind, made.note.as_deref(), made.uses, made.uses_left),
            (ApiKind::Federation, Some("Bob's server"), 2, 2)
        );
        assert_eq!(made.expires_at - made.created_at, DAY as i64);
        let refused = sa
            .admin
            .invite_create(NewInvite {
                kind: ApiKind::Federation,
                note: None,
                uses: None,
                expires: None,
                role: Some(Role::Consumer),
            })
            .await
            .unwrap_err();
        assert_eq!(code(refused), error_code::INVALID_INPUT);
        assert_eq!(sa.admin.invite_list().await.unwrap().len(), 1);
        sa.server.stop().await;
    }

    /// The web UI's check before pairing: SB's admin looks SA's invite up
    /// and reads SA's name and fingerprint, and the invite keeps its use;
    /// one used by another server, or a server paired already, is refused
    /// as redeeming would refuse it. Paired, SA's own page on SB has its
    /// row and an empty week.
    #[tokio::test]
    async fn an_admin_looks_an_invite_up_before_pairing() {
        let sa = Site::start("fed-admin-look-sa", "Alice's server", "alice", true).await;
        let sb = Site::start("fed-admin-look-sb", "Bob's server", "bob", true).await;
        let sc = Site::start("fed-admin-look-sc", "Carol's server", "carol", true).await;
        let spent = sa.invite().await;
        sc.admin.redeem(spent.clone()).await.unwrap();
        let refused = sb.admin.look_up(spent).await.unwrap_err();
        let said = format!("{refused:?}");
        assert!(said.contains("invite_exhausted"), "{said}");
        assert_eq!(code(refused), error_code::PAIRING_REFUSED);

        let link = sa.invite().await;
        let found = sb.admin.look_up(link.clone()).await.unwrap();
        let sa_status = sa.admin.server_status().await.unwrap();
        assert_eq!(found.name, "Alice's server");
        assert_eq!(found.server_id, sa.server.server_id.as_str());
        assert_eq!(Some(found.fingerprint.clone()), sa_status.fingerprint);
        let open = sa.server.store.live_invites().await.unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].uses_left, 1, "looking uses nothing");

        let on_sb = sb.admin.redeem(link).await.unwrap();
        let again = sb.admin.look_up(sa.invite().await).await.unwrap_err();
        assert!(format!("{again:?}").contains("already_paired"), "{again:?}");

        let detail = sb.admin.peer_detail(on_sb.id.clone()).await.unwrap();
        assert_eq!(detail.peer.id, on_sb.id);
        assert_eq!(detail.week.len(), 7);
        assert!(
            detail
                .week
                .iter()
                .all(|d| d.plays_here == 0 && d.plays_there == 0)
        );
        assert!(detail.received.is_empty() && detail.withdrawn.is_empty());
        assert_eq!(detail.identity_refused_at, None);
        assert_eq!(
            code(sb.admin.peer_detail("nope".into()).await.unwrap_err()),
            error_code::NO_SUCH_PEER
        );
        sa.server.stop().await;
        sb.server.stop().await;
        sc.server.stop().await;
    }

    /// The QR check (2026-10-22): looking SA's invite up on SA tells its
    /// kind and SA's fingerprint before SB redeems it.
    #[tokio::test]
    async fn a_federation_invite_s_lookup_tells_the_key_it_pairs_with() {
        use spoolframe_protocol::invite_admin::InviteKind as WireKind;
        use spoolframe_protocol::{InviteLookupOutcome, LookupInvite, Reply};
        let sa = Site::start("fed-admin-lookup", "Alice's server", "alice", true).await;
        let link = sa.invite().await;
        let device = sa.server.client().await;
        device.hello_info().await;
        let looked = device
            .call(
                None,
                Request::LookupInvite(LookupInvite {
                    code: code_of(&link),
                }),
            )
            .await;
        let Response::Ok(Reply::InviteLookup {
            outcome:
                InviteLookupOutcome::Valid {
                    kind, fingerprint, ..
                },
        }) = looked
        else {
            panic!("{looked:?}");
        };
        assert_eq!(kind, WireKind::Federation);
        assert_eq!(
            fingerprint,
            Some(fingerprint::fingerprint(sa.server.server.id().as_bytes()))
        );
        sa.server.stop().await;
    }

    /// Unpairing from either side unpairs both, and a pair made again
    /// starts clean: a new row on each side.
    #[tokio::test]
    async fn unpairing_on_either_side_unpairs_both() {
        let sa = Site::start("fed-admin-unpair-sa", "Alice's server", "alice", true).await;
        let sb = Site::start("fed-admin-unpair-sb", "Bob's server", "bob", true).await;
        let first = pair(&sa, &sb).await;
        sb.admin.peer_revoke(first.id.clone()).await.unwrap();
        assert!(sb.peers().await[0].revoked_at.is_some());
        let on_sa = sa.unpaired_from(&sb.server).await;
        assert!(!on_sa.connected);

        let second = pair(&sa, &sb).await;
        assert_ne!(second.id, first.id);
        assert_eq!(sb.peers().await.len(), 2);
        let on_sa = sa
            .peers()
            .await
            .into_iter()
            .find(|p| p.revoked_at.is_none())
            .unwrap();
        sa.admin.peer_revoke(on_sa.id).await.unwrap();
        let again = sb.unpaired_from(&sa.server).await;
        assert_eq!(again.id, second.id);
        assert_eq!(
            code(sa.admin.peer_revoke(first.id).await.unwrap_err()),
            error_code::NO_SUCH_PEER
        );
        sa.server.stop().await;
        sb.server.stop().await;
    }

    /// P6-04's tiers: a stranger server is a stranger to the relay; paired,
    /// a friend; unpaired, a stranger again.
    #[tokio::test]
    async fn a_paired_server_relays_as_a_friend_and_only_while_paired() {
        use spoolframe_protocol::relay_access::Tier;
        let sa = Site::start("fed-admin-relay-sa", "Alice's server", "alice", true).await;
        let sb = Site::start("fed-admin-relay-sb", "Bob's server", "bob", true).await;
        let relay = RelayAccess::new(
            Vec::new(),
            sa.server.server.id(),
            sa.server.auth.clone(),
            sa.server.federation.clone(),
        );
        let sb_key = sb.server.server.id().to_string();
        assert_eq!(relay.tier(&sb_key, None).await, Tier::Stranger);
        let on_sb = pair(&sa, &sb).await;
        assert_eq!(relay.tier(&sb_key, None).await, Tier::Friend);
        // With federation off, a pair kept from before is no friend.
        let off = RelayAccess::new(
            Vec::new(),
            sa.server.server.id(),
            sa.server.auth.clone(),
            None,
        );
        assert_eq!(off.tier(&sb_key, None).await, Tier::Stranger);
        sb.admin.peer_revoke(on_sb.id).await.unwrap();
        sa.unpaired_from(&sb.server).await;
        assert_eq!(relay.tier(&sb_key, None).await, Tier::Stranger);
        sa.server.stop().await;
        sb.server.stop().await;
    }

    /// SB's rotated keys are held until SA's admin decides on the one
    /// they name: a key not held changes nothing; rejected, a key is
    /// refused when it says hello again; confirmed, the pair moves to
    /// that key and no other.
    #[tokio::test]
    async fn a_held_key_is_rejected_or_confirmed_by_the_admin() {
        let sa = Site::start("fed-admin-key", "Alice's server", "alice", true).await;
        let link = sa.invite().await;
        let old = endpoint().await;
        let (dialed, _) = Dialed::connect(&old, sa.server.server.addr(), hello("bbbbbbbbbbbbbbbb"))
            .await
            .unwrap();
        dialed.redeem(redeem(&code_of(&link))).await.unwrap();
        dialed.close();
        let say_hello = |from: Endpoint| {
            let addr = sa.server.server.addr();
            async move {
                let said = Dialed::connect(&from, addr, hello("bbbbbbbbbbbbbbbb")).await;
                said.map(|(dialed, info)| {
                    dialed.close();
                    info.standing
                })
            }
        };
        let (rejected_key, confirmed_key) = (endpoint().await, endpoint().await);
        assert_eq!(
            say_hello(rejected_key.clone()).await.unwrap(),
            Standing::KeyHeld
        );
        assert_eq!(
            say_hello(confirmed_key.clone()).await.unwrap(),
            Standing::KeyHeld
        );
        let peer = sa.peers().await.remove(0);
        assert_eq!(
            peer.held_keys
                .iter()
                .map(|k| (k.endpoint_id.clone(), k.fingerprint.clone()))
                .collect::<Vec<_>>(),
            [&rejected_key, &confirmed_key].map(|e| (
                e.id().to_string(),
                fingerprint::fingerprint(e.id().as_bytes())
            ))
        );

        // A key that is not held is neither confirmed nor rejected.
        let stranger = endpoint().await.id().to_string();
        for refused in [
            sa.admin
                .key_confirm(peer.id.clone(), stranger.clone())
                .await,
            sa.admin.key_reject(peer.id.clone(), stranger.clone()).await,
        ] {
            assert_eq!(code(refused.unwrap_err()), error_code::KEY_NOT_HELD);
        }
        assert_eq!(sa.peers().await.remove(0), peer);

        let rejected = sa
            .admin
            .key_reject(peer.id.clone(), rejected_key.id().to_string())
            .await
            .unwrap();
        assert_eq!(rejected.endpoint_id, old.id().to_string());
        assert!(rejected.held_keys[0].rejected_at.is_some());
        assert!(matches!(
            say_hello(rejected_key.clone()).await,
            Err(DialError::Refused(PairingRefused::KeyRefused))
        ));
        assert_eq!(
            code(
                sa.admin
                    .key_confirm(peer.id.clone(), rejected_key.id().to_string())
                    .await
                    .unwrap_err()
            ),
            error_code::KEY_NOT_HELD
        );

        let moved = sa
            .admin
            .key_confirm(peer.id.clone(), confirmed_key.id().to_string())
            .await
            .unwrap();
        assert_eq!(
            (moved.id.as_str(), moved.endpoint_id.as_str()),
            (peer.id.as_str(), confirmed_key.id().to_string().as_str())
        );
        assert!(moved.held_keys.is_empty());
        assert_eq!(
            say_hello(confirmed_key.clone()).await.unwrap(),
            Standing::Paired
        );
        sa.server.stop().await;
    }

    /// M8-06: a stranger who knows SB's server id fills the keys SA holds
    /// for it. SA's admin enters the fingerprint SB's admin reads them,
    /// and SB's rotated key is held past the stranger's, and confirmed; a
    /// key that is not it is still refused. The page says the held keys
    /// are full, and what is expected.
    #[tokio::test]
    async fn the_expected_key_is_held_past_a_stranger_s_and_confirmed() {
        use spoolframe_server::store::federation::MAX_HELD_KEYS;
        let sa = Site::start("fed-admin-key-expected", "Alice's server", "alice", true).await;
        let link = sa.invite().await;
        let old = endpoint().await;
        let (dialed, _) = Dialed::connect(&old, sa.server.server.addr(), hello("bbbbbbbbbbbbbbbb"))
            .await
            .unwrap();
        dialed.redeem(redeem(&code_of(&link))).await.unwrap();
        dialed.close();
        let say_hello = |from: Endpoint| {
            let addr = sa.server.server.addr();
            async move {
                let said = Dialed::connect(&from, addr, hello("bbbbbbbbbbbbbbbb")).await;
                said.map(|(dialed, info)| {
                    dialed.close();
                    info.standing
                })
            }
        };
        for _ in 0..MAX_HELD_KEYS {
            assert_eq!(
                say_hello(endpoint().await).await.unwrap(),
                Standing::KeyHeld
            );
        }
        let peer = sa.peers().await.remove(0);
        assert!(peer.held_keys_full);
        assert_eq!(peer.expected_key, None);

        // What is not a fingerprint, or is the pinned key's, is refused.
        let pinned = fingerprint::fingerprint(old.id().as_bytes());
        for typed in ["K7QX-3MPA", pinned.as_str()] {
            assert_eq!(
                code(
                    sa.admin
                        .key_expect(peer.id.clone(), Some(typed.into()))
                        .await
                        .unwrap_err()
                ),
                error_code::INVALID_INPUT,
                "{typed}"
            );
        }

        let rotated = endpoint().await;
        let print = fingerprint::fingerprint(rotated.id().as_bytes());
        let expecting = sa
            .admin
            .key_expect(
                peer.id.clone(),
                Some(print.to_lowercase().replace('-', " ")),
            )
            .await
            .unwrap();
        let expected = expecting.expected_key.unwrap();
        assert_eq!(
            (expected.fingerprint.as_str(), expected.expected_by.as_str()),
            (print.as_str(), "alice")
        );
        assert!(matches!(
            say_hello(endpoint().await).await,
            Err(DialError::Refused(PairingRefused::KeyRefused))
        ));
        assert_eq!(say_hello(rotated.clone()).await.unwrap(), Standing::KeyHeld);
        let detail = sa.admin.peer_detail(peer.id.clone()).await.unwrap();
        assert_eq!(
            detail.peer.held_keys.last().map(|k| k.fingerprint.as_str()),
            Some(print.as_str())
        );
        assert!(detail.peer.held_keys_full);

        let moved = sa
            .admin
            .key_confirm(peer.id.clone(), rotated.id().to_string())
            .await
            .unwrap();
        assert_eq!(moved.endpoint_id, rotated.id().to_string());
        assert_eq!(moved.expected_key, None);
        assert!(!moved.held_keys_full);
        assert_eq!(say_hello(rotated).await.unwrap(), Standing::Paired);
        sa.server.stop().await;
    }

    /// Clearing what is expected leaves nothing expected; an unpaired
    /// server expects nothing.
    #[tokio::test]
    async fn an_expected_key_is_cleared_and_an_unpaired_server_expects_none() {
        let sa = Site::start("fed-admin-key-unexpect", "Alice's server", "alice", true).await;
        let sb = Site::start("fed-admin-key-unexpect-sb", "Bob's server", "bob", true).await;
        let on_sb = pair(&sa, &sb).await;
        let print = fingerprint::fingerprint(endpoint().await.id().as_bytes());
        let expecting = sb
            .admin
            .key_expect(on_sb.id.clone(), Some(print))
            .await
            .unwrap();
        assert!(expecting.expected_key.is_some());
        let cleared = sb.admin.key_expect(on_sb.id.clone(), None).await.unwrap();
        assert_eq!(cleared.expected_key, None);
        sb.admin.peer_revoke(on_sb.id.clone()).await.unwrap();
        assert_eq!(
            code(
                sb.admin
                    .key_expect(on_sb.id, Some("QAJ7-XQG9-2MDW-AN9H".into()))
                    .await
                    .unwrap_err()
            ),
            error_code::NO_SUCH_PEER
        );
        sa.server.stop().await;
        sb.server.stop().await;
    }

    /// An admin renames a paired server, signed in long ago: its name is
    /// the alias, what it called itself is kept, and blank calls it that
    /// again. Too long, or with a control character, is refused; so is an
    /// unpaired server, and a consumer.
    #[tokio::test]
    async fn an_admin_renames_a_paired_server_and_turns_its_home_row_off() {
        let sa = Site::start("fed-admin-rename", "Alice's server", "alice", true).await;
        let sb = Site::start("fed-admin-rename-sb", "Bob's server", "bob", true).await;
        let on_sb = pair(&sa, &sb).await;
        assert_eq!(
            (
                on_sb.alias.as_deref(),
                on_sb.remote_name.as_str(),
                on_sb.home_row
            ),
            (None, "Alice's server", true)
        );
        sb.server
            .clock
            .advance(spoolframe_server::auth::RECENT_SIGN_IN + Duration::from_secs(1));
        let named = sb
            .admin
            .peer_rename(on_sb.id.clone(), Some("  Mum's  ".into()))
            .await
            .unwrap();
        assert_eq!(
            (
                named.name.as_str(),
                named.alias.as_deref(),
                named.remote_name.as_str()
            ),
            ("Mum's", Some("Mum's"), "Alice's server")
        );
        assert_eq!(sb.peers().await, [named]);
        for bad in ["x".repeat(129), "Mum's\nserver".into()] {
            assert_eq!(
                code(
                    sb.admin
                        .peer_rename(on_sb.id.clone(), Some(bad))
                        .await
                        .unwrap_err()
                ),
                error_code::INVALID_INPUT
            );
        }
        let cleared = sb
            .admin
            .peer_rename(on_sb.id.clone(), Some("   ".into()))
            .await
            .unwrap();
        assert_eq!(
            (cleared.name.as_str(), cleared.alias),
            ("Alice's server", None)
        );
        let off = sb
            .admin
            .peer_home_row(on_sb.id.clone(), false)
            .await
            .unwrap();
        assert!(!off.home_row);
        let guest = Site::page(sb.addr, "guest").await;
        assert_eq!(
            code(
                guest
                    .peer_rename(on_sb.id.clone(), Some("Mine".into()))
                    .await
                    .unwrap_err()
            ),
            error_code::FORBIDDEN
        );
        assert_eq!(
            code(
                guest
                    .peer_home_row(on_sb.id.clone(), true)
                    .await
                    .unwrap_err()
            ),
            error_code::FORBIDDEN
        );
        sb.server
            .store
            .revoke_peer(&on_sb.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            code(
                sb.admin
                    .peer_rename(on_sb.id.clone(), Some("Mum's".into()))
                    .await
                    .unwrap_err()
            ),
            error_code::NO_SUCH_PEER
        );
        assert_eq!(
            code(sb.admin.peer_home_row(on_sb.id, true).await.unwrap_err()),
            error_code::NO_SUCH_PEER
        );
        sa.server.stop().await;
        sb.server.stop().await;
    }

    /// P8-03: SA's admin shares Movies with the paired server, sees it
    /// listed, and revokes it; a library SA does not serve is refused, and
    /// unpairing revokes whatever is still shared.
    #[tokio::test]
    async fn an_admin_shares_a_library_with_a_paired_server_and_revokes_it() {
        let sa = Site::start("fed-admin-grant-sa", "Alice's server", "alice", true).await;
        let sb = Site::start("fed-admin-grant-sb", "Bob's server", "bob", true).await;
        pair(&sa, &sb).await;
        let peer = sa.peers().await.remove(0).id;
        let grant = sa
            .admin
            .peer_grant_add(peer.clone(), "plex:1".into())
            .await
            .unwrap();
        assert_eq!(
            (
                grant.peer.as_str(),
                grant.library.as_str(),
                grant.granted_by.as_str()
            ),
            (peer.as_str(), "plex:1", "alice")
        );
        // Shared again, the grant is the one there was.
        assert_eq!(
            sa.admin
                .peer_grant_add(peer.clone(), "plex:1".into())
                .await
                .unwrap(),
            grant
        );
        assert_eq!(
            code(
                sa.admin
                    .peer_grant_add(peer.clone(), "plex:9".into())
                    .await
                    .unwrap_err()
            ),
            error_code::INVALID_INPUT
        );
        assert_eq!(
            code(
                sa.admin
                    .peer_grant_add("no-such".into(), "plex:1".into())
                    .await
                    .unwrap_err()
            ),
            error_code::NO_SUCH_PEER
        );
        assert_eq!(
            sa.admin.peer_grant_list(peer.clone()).await.unwrap(),
            std::slice::from_ref(&grant)
        );
        // Only SA shares it: SB has nothing shared with SA.
        let sa_on_sb = sb.peers().await.remove(0).id;
        assert!(
            sb.admin
                .peer_grant_list(sa_on_sb.clone())
                .await
                .unwrap()
                .is_empty()
        );
        // SB's admin syncs what SA shares now (P8-02), signed in long
        // ago: a refresh syncs with a server paired already, and reaches
        // no stranger.
        sb.server
            .clock
            .advance(spoolframe_server::auth::RECENT_SIGN_IN + Duration::from_secs(1));
        sb.admin.peer_refresh(sa_on_sb).await.unwrap();
        assert_eq!(
            code(sb.admin.peer_refresh("no-such".into()).await.unwrap_err()),
            error_code::NO_SUCH_PEER
        );

        sa.admin
            .peer_grant_revoke(peer.clone(), "plex:1".into())
            .await
            .unwrap();
        assert!(
            sa.admin
                .peer_grant_list(peer.clone())
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            code(
                sa.admin
                    .peer_grant_revoke(peer.clone(), "plex:1".into())
                    .await
                    .unwrap_err()
            ),
            error_code::NO_SUCH_GRANT
        );

        sa.admin
            .peer_grant_add(peer.clone(), "plex:1".into())
            .await
            .unwrap();
        sa.admin.peer_revoke(peer.clone()).await.unwrap();
        assert!(sa.server.store.live_grants(&peer).await.unwrap().is_empty());
        assert_eq!(
            code(sa.admin.peer_grant_list(peer).await.unwrap_err()),
            error_code::NO_SUCH_PEER
        );
        sa.server.stop().await;
        sb.server.stop().await;
    }

    /// What changes a pair needs an admin who signed in recently; a
    /// consumer is told no; listing and a refresh need no recency.
    #[tokio::test]
    async fn changes_need_a_recent_admin_and_a_consumer_gets_nothing() {
        let sa = Site::start("fed-admin-recent", "Alice's server", "alice", true).await;
        let guest = Site::page(sa.addr, "guest").await;
        assert_eq!(
            code(guest.peer_list().await.unwrap_err()),
            error_code::FORBIDDEN
        );
        assert_eq!(
            code(guest.peer_revoke("x".into()).await.unwrap_err()),
            error_code::FORBIDDEN
        );
        assert_eq!(
            code(guest.peer_grant_list("x".into()).await.unwrap_err()),
            error_code::FORBIDDEN
        );
        assert_eq!(
            code(guest.peer_refresh("x".into()).await.unwrap_err()),
            error_code::FORBIDDEN
        );
        sa.server
            .clock
            .advance(spoolframe_server::auth::RECENT_SIGN_IN + Duration::from_secs(1));
        assert!(sa.peers().await.is_empty());
        for refused in [
            sa.admin
                .redeem("spoolframe://invite?server=x".into())
                .await
                .map(|_| ()),
            sa.admin.peer_revoke("x".into()).await,
            sa.admin
                .key_confirm("x".into(), "y".into())
                .await
                .map(|_| ()),
            sa.admin
                .key_reject("x".into(), "y".into())
                .await
                .map(|_| ()),
            sa.admin
                .key_expect("x".into(), Some("QAJ7-XQG9-2MDW-AN9H".into()))
                .await
                .map(|_| ()),
            sa.admin.key_expect("x".into(), None).await.map(|_| ()),
            sa.admin
                .peer_grant_add("x".into(), "plex:1".into())
                .await
                .map(|_| ()),
            sa.admin
                .peer_grant_revoke("x".into(), "plex:1".into())
                .await,
        ] {
            assert_eq!(
                code(refused.unwrap_err()),
                error_code::RECENT_SIGN_IN_REQUIRED
            );
        }
        // A refresh only syncs again with a server paired already: past
        // the admin check, to the pair it names.
        assert_eq!(
            code(sa.admin.peer_refresh("x".into()).await.unwrap_err()),
            error_code::NO_SUCH_PEER
        );
        sa.server.stop().await;
    }

    /// Off, as in 0.14.4: every federation method says it is off, and no
    /// `federation` invite is made.
    #[tokio::test]
    async fn with_federation_off_the_methods_are_not_configured() {
        let sa = Site::start("fed-admin-off", "Alice's server", "alice", false).await;
        assert_eq!(
            code(sa.admin.peer_list().await.unwrap_err()),
            error_code::NOT_CONFIGURED
        );
        assert_eq!(
            code(sa.admin.redeem("x".into()).await.unwrap_err()),
            error_code::NOT_CONFIGURED
        );
        let refused = sa
            .admin
            .invite_create(NewInvite {
                kind: ApiKind::Federation,
                note: None,
                uses: None,
                expires: None,
                role: None,
            })
            .await
            .unwrap_err();
        assert_eq!(code(refused), error_code::INVALID_INPUT);
        sa.server.stop().await;
    }

    /// A bad link is refused before anything is dialled; another server's
    /// refusal is named by its reason.
    #[tokio::test]
    async fn a_refused_pairing_says_why() {
        let sa = Site::start("fed-admin-refused-sa", "Alice's server", "alice", true).await;
        let sb = Site::start("fed-admin-refused-sb", "Bob's server", "bob", true).await;
        assert_eq!(
            code(sb.admin.redeem("not a link".into()).await.unwrap_err()),
            error_code::INVALID_INPUT
        );
        let link = sa.invite().await;
        let (before, _) = link.split_once("&cap=").unwrap();
        let wrong = format!("{before}&cap=AAAA-AAAA-AAAA");
        let jsonrpsee::core::ClientError::Call(refused) = sb.admin.redeem(wrong).await.unwrap_err()
        else {
            panic!("not a call error");
        };
        assert_eq!(refused.code(), error_code::PAIRING_REFUSED);
        let data: serde_json::Value = serde_json::from_str(refused.data().unwrap().get()).unwrap();
        assert_eq!(data["reason"], "invite_invalid");
        assert!(sb.peers().await.is_empty() && sa.peers().await.is_empty());
        sa.server.stop().await;
        sb.server.stop().await;
    }
}

// P8-01's client-protocol side: an admin's app pairs its server by SA's
// invite, as the QR flow does, through the SDK; only an app that names
// the experimental `federation` feature may.

mod app {
    use spoolframe_client_sdk::{
        Client, ClientOptions, Device, Experimental, InviteKind as SdkKind, InviteOutcome,
        InviteSpec, MemoryStorage, SdkError, Session,
    };
    use spoolframe_types::Role;

    use super::*;
    use crate::support::mkv_capabilities;

    pub(super) const PASSWORD: &str = "correct horse";

    /// An app on loopback that asks for `experimental`, signed in to
    /// `server` as `username`.
    pub(super) async fn signed_in(
        server: &TestServer,
        username: &str,
        experimental: Experimental,
    ) -> (Client, Session) {
        let options = ClientOptions {
            lan_discovery: false,
            experimental,
            ..ClientOptions::default()
        };
        let device = Device {
            name: "phone".into(),
            client_name: "sdk-test".into(),
            client_version: "0".into(),
            platform: spoolframe_client_sdk::types::Platform::Ios,
            capabilities: mkv_capabilities(),
        };
        let client = Client::open_with_options(Arc::new(MemoryStorage::default()), device, options)
            .await
            .unwrap();
        let session = sign_in(&client, server, username).await;
        (client, session)
    }

    pub(super) async fn sign_in(client: &Client, server: &TestServer, username: &str) -> Session {
        let port = server
            .server
            .addr()
            .ip_addrs()
            .find(|addr| addr.is_ipv4())
            .expect("an IPv4 address")
            .port();
        let id = server.server.id().to_string();
        client
            .add_address(&id, vec![([127, 0, 0, 1], port).into()])
            .unwrap();
        client.connect(&id).await.unwrap();
        client.pin(&id, true).await.unwrap();
        client
            .sign_in(&id, username, PASSWORD, false)
            .await
            .unwrap()
    }

    pub(super) const FEDERATION: Experimental = Experimental {
        http_playback: false,
        federation: true,
    };

    async fn with_admin(test: &str, name: &str, admin: &str) -> TestServer {
        let server = super::server(test, name).await;
        server.add_user(admin, PASSWORD, None).await;
        server.store.set_role(admin, Role::Admin).await.unwrap();
        server.add_user("guest", PASSWORD, None).await;
        server
    }

    fn federation_invite() -> InviteSpec {
        InviteSpec {
            kind: SdkKind::Federation,
            note: None,
            uses: None,
            expires_in: None,
            role: None,
        }
    }

    /// The QR flow from the apps: SA's admin makes the invite in an app;
    /// SB's admin's app looks it up on SA (its kind, SA's fingerprint and
    /// who made it), then has SB redeem it.
    #[tokio::test]
    async fn an_admin_s_app_pairs_its_server_by_the_other_s_invite() {
        let sa = with_admin("fed-app-sa", "Alice's server", "alice").await;
        let sb = with_admin("fed-app-sb", "Bob's server", "bob").await;
        let (_on_sa, alice) = signed_in(&sa, "alice", FEDERATION).await;
        let minted = alice.create_invite(federation_invite()).await.unwrap();
        assert_eq!(minted.kind, SdkKind::Federation);

        let (phone, bob) = signed_in(&sb, "bob", FEDERATION).await;
        let info = phone.read_invite(&minted.link).await.unwrap();
        assert_eq!(info.kind, InviteLinkKind::Federation);
        let sa_print = fingerprint::fingerprint(sa.server.id().as_bytes());
        let looked = phone.look_up_invite(&minted.link).await.unwrap();
        assert_eq!(looked.server.fingerprint, sa_print);
        assert!(matches!(
            looked.outcome,
            InviteOutcome::Valid {
                kind: SdkKind::Federation,
                ..
            }
        ));

        let paired = bob.pair_server(&minted.link).await.unwrap();
        assert_eq!(paired.fingerprint, sa_print);
        assert_eq!(paired.name, "Alice's server");
        assert_eq!(paired.server_id, sa.server_id.as_str());
        let on_sb = sb
            .store
            .live_peer_by_endpoint(&sa.server.id().to_string())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(on_sb.paired_by, "bob");
        assert!(matches!(
            bob.pair_server(&minted.link).await,
            Err(SdkError::PairingRefused {
                reason: PairingRefused::AlreadyPaired
            })
        ));
        sa.stop().await;
        sb.stop().await;
    }

    /// Without the feature named, nothing of federation is offered: no
    /// pairing (not even sent), no `federation` invite.
    #[tokio::test]
    async fn an_app_that_does_not_name_the_feature_gets_none_of_it() {
        let sa = with_admin("fed-app-unnamed", "Alice's server", "alice").await;
        let (_phone, alice) = signed_in(&sa, "alice", Experimental::default()).await;
        assert!(!alice.experimental().federation);
        assert!(matches!(
            alice.create_invite(federation_invite()).await,
            Err(SdkError::InvalidInput(_))
        ));
        assert!(matches!(
            alice.pair_server("spoolframe://invite?server=x").await,
            Err(SdkError::Unsupported { .. })
        ));
        // Asked on the wire anyway, it is refused.
        let device = sa.client().await;
        device.hello().await;
        let session = device.login("alice", PASSWORD).await;
        assert!(matches!(
            device
                .call(
                    Some(&session),
                    Request::PairServer(spoolframe_protocol::federation::PairServer {
                        link: "spoolframe://invite?server=x".into(),
                    })
                )
                .await,
            Response::Err(Error::Unsupported { .. })
        ));
        sa.stop().await;
    }

    /// A server with federation off names nothing back, so an app that
    /// asks gets none of it either.
    #[tokio::test]
    async fn a_server_with_federation_off_does_not_turn_it_on() {
        let sa = TestServer::start("fed-app-off").await;
        sa.add_user("alice", PASSWORD, None).await;
        sa.store.set_role("alice", Role::Admin).await.unwrap();
        let (_phone, alice) = signed_in(&sa, "alice", FEDERATION).await;
        assert!(!alice.experimental().federation);
        assert!(matches!(
            alice.create_invite(federation_invite()).await,
            Err(SdkError::InvalidInput(_))
        ));
        sa.stop().await;
    }

    /// Admins only, as `CreateInvite`; and recently signed in.
    #[tokio::test]
    async fn only_a_recently_signed_in_admin_pairs() {
        let sb = with_admin("fed-app-admin-only", "Bob's server", "bob").await;
        let (_guest_phone, guest) = signed_in(&sb, "guest", FEDERATION).await;
        assert!(matches!(
            guest.pair_server("spoolframe://invite?server=x").await,
            Err(SdkError::AdminRequired)
        ));
        let (_phone, bob) = signed_in(&sb, "bob", FEDERATION).await;
        sb.clock
            .advance(spoolframe_server::auth::RECENT_SIGN_IN + std::time::Duration::from_secs(1));
        assert!(matches!(
            bob.pair_server("spoolframe://invite?server=x").await,
            Err(SdkError::RecentSignInRequired)
        ));
        sb.stop().await;
    }
}

// P8-02 on the sharing server: a pair's sync is answered from SA's
// catalog (the fake Plex catalog) for a library shared with it, as
// changes since its cursor or the whole library, and its artwork only for
// what it was sent.

mod sharing {
    use spoolframe_federation::Pages;
    use spoolframe_protocol::federation::{RemoteKind, SyncLibrary, SyncMode, SyncPage};
    use spoolframe_server::test_support::{POSTER, poster_of};
    use spoolframe_types::{MediaId, MediaKind, Namespace};

    use super::*;

    /// SA, and SB's key paired with it as `bbbbbbbbbbbbbbbb`: the
    /// connection, and the pair's id on SA. The endpoint is kept in the
    /// connection's place: dropped, it closes. SA walks its library at
    /// each sync, for the tests that change it between two.
    async fn paired(test: &str) -> (TestServer, Dialed, String, Endpoint) {
        let sa = server(test, "Alice's server").await;
        no_walk_reuse(&sa);
        let link = pairing_link(&sa, None).await;
        let sb = endpoint().await;
        let (dialed, _) = Dialed::connect(&sb, sa.server.addr(), hello("bbbbbbbbbbbbbbbb"))
            .await
            .unwrap();
        dialed.redeem(redeem(&code_of(&link))).await.unwrap();
        let peer = sa
            .store
            .live_peer_by_server_id("bbbbbbbbbbbbbbbb")
            .await
            .unwrap()
            .unwrap()
            .id;
        (sa, dialed, peer, sb)
    }

    async fn share(sa: &TestServer, peer: &str, library: &str) {
        federation(sa)
            .share(peer, &[library.parse().unwrap()], "alice")
            .await
            .unwrap();
    }

    /// Every page of a sync, read to the last.
    async fn read(mut pages: Pages) -> Result<Vec<SyncPage>, DialError> {
        let mut read = Vec::new();
        while let Some(page) = pages.next().await? {
            read.push(page);
        }
        Ok(read)
    }

    async fn sync(
        dialed: &Dialed,
        library: &str,
        since: Option<&str>,
    ) -> Result<(SyncMode, String, Vec<String>, Vec<String>), DialError> {
        let pages = read(
            dialed
                .sync(SyncLibrary {
                    library: library.into(),
                    since: since.map(str::to_owned),
                })
                .await?,
        )
        .await?;
        let mut items: Vec<String> = pages
            .iter()
            .flat_map(|p| p.items.iter().map(|i| i.id.clone()))
            .collect();
        items.sort();
        let removed = pages.iter().flat_map(|p| p.removed.clone()).collect();
        Ok((pages[0].mode, pages[0].cursor.clone(), items, removed))
    }

    fn id(kind: MediaKind, key: &str) -> MediaId {
        MediaId::new(Namespace::Plex, kind, key).unwrap()
    }

    /// Nothing shared, nothing answered; Movies shared, the pair gets the
    /// whole of it, then only what changed; a cursor SA does not keep gets
    /// the whole of it again.
    #[tokio::test]
    async fn a_pair_gets_a_shared_library_whole_then_its_changes() {
        let (sa, dialed, peer, _sb) = paired("fed-sharing-movies").await;
        assert!(dialed.libraries().await.unwrap().is_empty());
        assert!(matches!(
            sync(&dialed, "plex:1", None).await,
            Err(DialError::NotShared)
        ));
        share(&sa, &peer, "plex:1").await;
        let libraries = dialed.libraries().await.unwrap();
        assert_eq!(
            libraries
                .iter()
                .map(|l| (l.id.as_str(), l.title.as_str()))
                .collect::<Vec<_>>(),
            [("plex:1", "Movies")]
        );

        let pages = read(
            dialed
                .sync(SyncLibrary {
                    library: "plex:1".into(),
                    since: None,
                })
                .await
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].mode, SyncMode::Snapshot);
        let film = &pages[0].items[0];
        assert_eq!(
            (film.id.as_str(), film.kind, film.title.as_str()),
            ("plex:movie:1", RemoteKind::Movie, "Alien")
        );
        assert_eq!(film.external_ids, ["test://1"]);
        assert_eq!(film.duration_ms, Some(7_020_000));
        assert_eq!(film.hash, film.digest());
        let first = pages[0].cursor.clone();

        // Nothing changed: no items, and the cursor stands.
        let (mode, cursor, items, removed) = sync(&dialed, "plex:1", Some(&first)).await.unwrap();
        assert_eq!((mode, cursor.as_str()), (SyncMode::Changes, first.as_str()));
        assert!(items.is_empty() && removed.is_empty());

        // The film goes: it is named as removed.
        sa.catalog.remove(&id(MediaKind::Movie, "1"));
        let (mode, cursor, items, removed) = sync(&dialed, "plex:1", Some(&first)).await.unwrap();
        assert_eq!(mode, SyncMode::Changes);
        assert_ne!(cursor, first);
        assert!(items.is_empty());
        assert_eq!(removed, ["plex:movie:1"]);

        let (mode, _, items, _) = sync(&dialed, "plex:1", Some("no-such-cursor"))
            .await
            .unwrap();
        assert_eq!((mode, items.len()), (SyncMode::Snapshot, 0));
        sa.stop().await;
    }

    /// A sync within a minute of a walk of the library is answered from
    /// that walk (H2): a pair asking again and again does not make SA walk
    /// it again, and what changed since shows at a later sync.
    #[tokio::test]
    async fn a_library_is_walked_at_most_once_a_minute() {
        let (sa, dialed, peer, _sb) = paired("fed-sharing-walks").await;
        federation(&sa).set_limits(spoolframe_server::federation::Limits::default());
        let alien = id(MediaKind::Movie, "1");
        sa.catalog.not_yet(&alien);
        share(&sa, &peer, "plex:1").await;
        let (_, first, items, _) = sync(&dialed, "plex:1", None).await.unwrap();
        assert!(items.is_empty());
        sa.catalog.gain(&alien);
        let (_, cursor, items, _) = sync(&dialed, "plex:1", Some(&first)).await.unwrap();
        assert_eq!((cursor.as_str(), items.len()), (first.as_str(), 0));
        no_walk_reuse(&sa);
        let (_, _, items, _) = sync(&dialed, "plex:1", Some(&first)).await.unwrap();
        assert_eq!(items, ["plex:movie:1"]);
        sa.stop().await;
    }

    /// A pair's assertions are paced (H2): past its bucket, SA cannot
    /// answer now.
    #[tokio::test]
    async fn a_pair_asking_too_fast_is_told_to_wait() {
        use spoolframe_protocol::federation::{AssertUser, AssertionRefused, TokenScope};
        let (sa, dialed, _peer, _sb) = paired("fed-sharing-paced").await;
        let bogus = || AssertUser {
            user: "not-a-uuid".into(),
            display_name: "Bob".into(),
            client_endpoint: "nowhere".into(),
            item: "plex:movie:1".into(),
            scope: TokenScope::Play,
            preferences: None,
            download: None,
        };
        let refused = |e: DialError| match e {
            DialError::NotGranted(reason) => reason,
            e => panic!("{e:?}"),
        };
        for _ in 0..30 {
            let reason = refused(dialed.assert_user(bogus()).await.unwrap_err());
            assert_eq!(reason, AssertionRefused::InvalidUser);
        }
        let reason = refused(dialed.assert_user(bogus()).await.unwrap_err());
        assert_eq!(reason, AssertionRefused::Unavailable);
        sa.stop().await;
    }

    /// Music replicates whole: the artist, its albums and their tracks. A
    /// track the library gains comes as a change; a pair that did not
    /// take the last answer up (it names the cursor before) gets the
    /// changes since the one it has.
    #[tokio::test]
    async fn music_replicates_artists_albums_and_tracks() {
        let (sa, dialed, peer, _sb) = paired("fed-sharing-music").await;
        sa.catalog.serve_music();
        let late = id(MediaKind::Track, "35");
        sa.catalog.not_yet(&late);
        share(&sa, &peer, "plex:2").await;
        let (mode, first, items, _) = sync(&dialed, "plex:2", None).await.unwrap();
        assert_eq!(mode, SyncMode::Snapshot);
        assert_eq!(
            items,
            [
                "plex:album:31",
                "plex:album:34",
                "plex:artist:30",
                "plex:track:32",
                "plex:track:33"
            ]
        );
        sa.catalog.gain(&late);
        let (mode, second, items, _) = sync(&dialed, "plex:2", Some(&first)).await.unwrap();
        // The album's listing of its tracks changed with it.
        assert_eq!(mode, SyncMode::Changes);
        assert!(items.contains(&"plex:track:35".to_owned()), "{items:?}");
        // Asked again from the first cursor, as after a lost answer: the
        // same changes, under a cursor of their own, and the answer not
        // taken up is forgotten.
        let (mode, third, again, _) = sync(&dialed, "plex:2", Some(&first)).await.unwrap();
        assert_eq!((mode, again), (SyncMode::Changes, items));
        assert_ne!(third, second);
        let (mode, _, _, _) = sync(&dialed, "plex:2", Some(&second)).await.unwrap();
        assert_eq!(mode, SyncMode::Snapshot);
        sa.stop().await;
    }

    /// Artwork goes only for what was sent, while its library is shared;
    /// revoked, the library and its artwork are refused as not shared.
    #[tokio::test]
    async fn artwork_goes_only_for_items_sent_while_shared() {
        let (sa, dialed, peer, _sb) = paired("fed-sharing-artwork").await;
        sa.catalog.show_posters();
        share(&sa, &peer, "plex:1").await;
        let poster = poster_of("1").to_string();
        // Not sent yet.
        assert!(matches!(
            dialed.artwork(&poster).await,
            Err(DialError::NotShared)
        ));
        let (_, cursor, _, _) = sync(&dialed, "plex:1", None).await.unwrap();
        let (content_type, bytes) = dialed.artwork(&poster).await.unwrap();
        assert_eq!(content_type, "image/png");
        assert!(!bytes.is_empty());
        // An image of SA's that no item sent names.
        assert!(matches!(
            dialed.artwork(&format!("plex:{POSTER}")).await,
            Err(DialError::NotShared)
        ));

        federation(&sa)
            .unshare(&peer, "plex:1", "alice")
            .await
            .unwrap();
        assert!(dialed.libraries().await.unwrap().is_empty());
        assert!(matches!(
            sync(&dialed, "plex:1", Some(&cursor)).await,
            Err(DialError::NotShared)
        ));
        assert!(matches!(
            dialed.artwork(&poster).await,
            Err(DialError::NotShared)
        ));
        sa.stop().await;
    }
}

// P8-02 on the server a library is shared with: SB pairs with SA, SA
// shares a library, and SB syncs it into its cache and serves it from
// there under `fed:` ids, with SA online or not. SA's library is the
// fake Plex catalog; SB's own catalog is a composite with no library of
// its own, which carries the cache as one more backend.

mod replica {
    use std::time::Duration;

    use spoolframe_catalog::{Catalog, CatalogError, Page};
    use spoolframe_plex::PlexCatalog;
    use spoolframe_protocol::federation::SHARES_CHANGED_EVERY;
    use spoolframe_protocol::{Reply, SessionId};
    use spoolframe_server::catalog::{Composite, LiveCatalog, PlexSource};
    use spoolframe_server::store::watch::WatchUpdate;
    use spoolframe_server::test_support::{movie, poster_of};
    use spoolframe_types::{ArtworkId, Item, LibraryId, MediaId, MediaKind, Namespace};

    use super::*;

    /// SB on `dir`, its catalog the composite carrying its cache.
    async fn sb_in(dir: crate::support::StateDir) -> (TestServer, Arc<LiveCatalog>) {
        sb_with(dir, ServerOptions::default()).await
    }

    /// A Plex server with nothing in it, which answers everything with an
    /// empty list: SB's own, so Home and search have a Plex to ask.
    async fn quiet_plex() -> String {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let pms = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "MediaContainer": { "size": 0, "totalSize": 0, "Metadata": [], "Hub": [] }
            })))
            .mount(&pms)
            .await;
        let uri = pms.uri();
        // Kept for the rest of the test run: a test's servers stop before
        // it ends, and the mock outlives them.
        std::mem::forget(pms);
        uri
    }

    /// As [`sb_in`], run as `options` say.
    async fn sb_with(
        dir: crate::support::StateDir,
        options: ServerOptions,
    ) -> (TestServer, Arc<LiveCatalog>) {
        sb_on(dir, options, quiet_plex().await).await
    }

    /// A Plex server with one film, Alien (`plex:movie:500`, TMDB 348),
    /// in its one section, found by any search for films: SB's own copy of
    /// the film SA shares (P8-05). Anything else is answered empty.
    async fn alien_plex() -> String {
        alien_plex_of(1979).await
    }

    /// [`alien_plex`] with its Alien of `year`.
    async fn alien_plex_of(year: u16) -> String {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let pms = MockServer::start().await;
        let alien = serde_json::json!({
            "ratingKey": "500",
            "key": "/library/metadata/500",
            "type": "movie",
            "title": "Alien",
            "year": year,
            "duration": 7_020_000,
            "librarySectionID": "1",
            "guid": "plex://movie/5d7768",
            "Guid": [{ "id": "tmdb://348" }]
        });
        let container = |metadata: serde_json::Value| {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "MediaContainer": { "size": 1, "totalSize": 1, "Metadata": metadata }
            }))
        };
        Mock::given(method("GET"))
            .and(path("/library/sections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "MediaContainer": { "Directory": [{ "key": "1", "type": "movie", "title": "Films" }] }
            })))
            .mount(&pms)
            .await;
        Mock::given(method("GET"))
            .and(path("/library/sections/1/search"))
            .and(query_param("type", "1"))
            .respond_with(container(serde_json::json!([alien.clone()])))
            .mount(&pms)
            .await;
        Mock::given(method("GET"))
            .and(path("/library/metadata/500"))
            .respond_with(container(serde_json::json!([alien.clone()])))
            .mount(&pms)
            .await;
        // Plex finds an item by its own guid alone.
        Mock::given(method("GET"))
            .and(path("/library/sections/1/all"))
            .and(query_param("guid", "plex://movie/5d7768"))
            .respond_with(container(serde_json::json!([alien])))
            .mount(&pms)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "MediaContainer": { "size": 0, "totalSize": 0, "Metadata": [], "Hub": [] }
            })))
            .mount(&pms)
            .await;
        let uri = pms.uri();
        std::mem::forget(pms);
        uri
    }

    /// As [`sb_with`], its own Plex server `plex`.
    async fn sb_on(
        dir: crate::support::StateDir,
        options: ServerOptions,
        plex: String,
    ) -> (TestServer, Arc<LiveCatalog>) {
        let plex = PlexCatalog::new(plex.parse().unwrap(), None).unwrap();
        let composite = Composite::new(
            vec![PlexSource {
                id: spoolframe_server::config::FIRST_PLEX_SERVER.to_owned(),
                name: "Bob's Plex".into(),
                catalog: Arc::new(plex),
            }],
            Some(Vec::new()),
        );
        let live = Arc::new(LiveCatalog::new(Some(Arc::new(composite))));
        let sb = TestServer::start_with(
            dir,
            ServerOptions {
                name: "Bob's server".into(),
                federation: true,
                catalog: Some(live.clone()),
                ..options
            },
        )
        .await;
        (sb, live)
    }

    /// SA and SB paired, SB having redeemed SA's invite: the pair's id on
    /// SA (SB's row) and on SB (SA's row).
    struct Pair {
        sa: TestServer,
        sb: TestServer,
        cache: Arc<LiveCatalog>,
        sb_on_sa: String,
        sa_on_sb: String,
    }

    impl Pair {
        async fn start(test: &str) -> Self {
            Self::start_with(test, ServerOptions::default()).await
        }

        /// With SB run as `options` say.
        async fn start_with(test: &str, options: ServerOptions) -> Self {
            Self::start_both(test, ServerOptions::default(), options).await
        }

        /// With SA run as `sa_options` say, and SB as `options`.
        async fn start_both(test: &str, sa_options: ServerOptions, options: ServerOptions) -> Self {
            let sa = TestServer::start_with(
                fresh_dir(&format!("{test}-sa")).await,
                ServerOptions {
                    name: "Alice's server".into(),
                    federation: true,
                    ..sa_options
                },
            )
            .await;
            Self::with_sa(test, sa, options).await
        }

        /// `sa` paired with an SB run as `options` say.
        async fn with_sa(test: &str, sa: TestServer, options: ServerOptions) -> Self {
            Self::with_sa_on(test, sa, options, quiet_plex().await).await
        }

        /// SA and an SB whose own Plex server is `plex`.
        async fn with_plex(test: &str, plex: String) -> Self {
            let sa = TestServer::start_with(
                fresh_dir(&format!("{test}-sa")).await,
                ServerOptions {
                    name: "Alice's server".into(),
                    federation: true,
                    ..ServerOptions::default()
                },
            )
            .await;
            Self::with_sa_on(test, sa, ServerOptions::default(), plex).await
        }

        /// `sa` paired with an SB run as `options` say, on Plex `plex`.
        async fn with_sa_on(
            test: &str,
            sa: TestServer,
            options: ServerOptions,
            plex: String,
        ) -> Self {
            no_walk_reuse(&sa);
            let (sb, cache) = sb_on(fresh_dir(&format!("{test}-sb")).await, options, plex).await;
            let sa_on_sb = federation(&sb)
                .redeem_link(&pairing_link(&sa, None).await, "bob")
                .await
                .unwrap()
                .id;
            let sb_on_sa = sa
                .store
                .live_peer_by_endpoint(&sb.server.id().to_string())
                .await
                .unwrap()
                .unwrap()
                .id;
            Self {
                sa,
                sb,
                cache,
                sb_on_sa,
                sa_on_sb,
            }
        }

        async fn share(&self, library: &str) {
            federation(&self.sa)
                .share(&self.sb_on_sa, &[library.parse().unwrap()], "alice")
                .await
                .unwrap();
        }

        async fn refresh(&self) {
            federation(&self.sb).refresh(&self.sa_on_sb).await.unwrap();
        }

        fn origin(&self) -> &str {
            self.sa.server_id.as_str()
        }

        /// SA's `id` as SB serves it.
        /// SB's replica woken, and its posters fetched: `count` files.
        async fn posters_fetched(&self, count: usize) {
            self.wake_replica();
            let files = self.files();
            until(|| {
                let files = files.clone();
                async move { kept(&files) == count }
            })
            .await;
        }

        /// SB's replica woken before its tick: a setting applied does it.
        fn wake_replica(&self) {
            federation(&self.sb).set_replica(
                std::time::Duration::from_secs(3600),
                spoolframe_server::store::federation_cache::FileLimits {
                    per_origin: 1 << 30,
                    total: 1 << 30,
                },
            );
        }

        /// SB's replica woken: SA is asked for no image meanwhile, of the
        /// `asked` it was asked for so far. A sweep asks within a few
        /// milliseconds of the wake; a second is far past it.
        async fn not_asked_again(&self, asked: usize) {
            self.wake_replica();
            tokio::time::sleep(Duration::from_secs(1)).await;
            assert_eq!(self.sa.catalog.artwork_asks(), asked, "asked again");
        }

        /// SB's replica woken until SA was asked for `asked` images.
        async fn asked(&self, asked: usize) {
            until(|| async {
                self.wake_replica();
                self.sa.catalog.artwork_asks() == asked
            })
            .await;
        }

        /// Where SB keeps SA's images.
        fn files(&self) -> std::path::PathBuf {
            self.sb
                .dir
                .join("cache")
                .join("federation")
                .join(self.origin())
        }

        fn fed(&self, id: &MediaId) -> MediaId {
            MediaId::federated(self.origin(), id).unwrap()
        }

        fn fed_library(&self, id: &str) -> LibraryId {
            LibraryId::federated(self.origin(), &id.parse().unwrap()).unwrap()
        }

        fn fed_artwork(&self, id: &ArtworkId) -> ArtworkId {
            ArtworkId::federated(self.origin(), id).unwrap()
        }

        /// The titles of the first page of SA's `library` in SB's cache.
        async fn titles(&self, library: &str) -> Result<Vec<String>, CatalogError> {
            let page = self
                .cache
                .items(
                    &self.fed_library(library),
                    Page {
                        offset: 0,
                        limit: 50,
                    },
                )
                .await?;
            Ok(page.items.iter().map(title).collect())
        }

        async fn stop(self) {
            self.sa.stop().await;
            self.sb.stop().await;
        }
    }

    fn title(item: &Item) -> String {
        match item {
            Item::Movie(m) => m.title.clone(),
            Item::Show(s) => s.title.clone(),
            Item::Season(s) => s.title.clone(),
            Item::Episode(e) => e.title.clone(),
            Item::Artist(a) => a.name.clone(),
            Item::Album(a) => a.title.clone(),
            Item::Track(t) => t.title.clone(),
        }
    }

    fn id(kind: MediaKind, key: &str) -> MediaId {
        MediaId::new(Namespace::Plex, kind, key).unwrap()
    }

    /// Wait for `done`, at most ten seconds.
    /// How many files are in `dir`.
    fn kept(dir: &std::path::Path) -> usize {
        std::fs::read_dir(dir).map_or(0, |d| d.filter(|f| f.is_ok()).count())
    }

    async fn until<F, Fut>(mut done: F)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        for _ in 0..100 {
            if done().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("not in ten seconds");
    }

    /// A shared library is sorted and filtered on SB, in its cache, by
    /// what SA sent: the film's own record's genres and range among it.
    #[tokio::test]
    async fn a_shared_library_is_sorted_and_filtered_in_the_cache() {
        use spoolframe_types::{DynamicRange, ItemFilter, ItemSort, SortKey};
        let pair = Pair::start("fed-replica-browse").await;
        pair.share("plex:1").await;
        pair.refresh().await;
        let app = pair.sb.client().await;
        let hello = spoolframe_protocol::Hello {
            protocol_version: spoolframe_protocol::PROTOCOL_VERSION,
            client_name: "test".into(),
            client_version: "0".into(),
            platform: spoolframe_types::Platform::Linux,
            device_name: None,
            capabilities: None,
            experimental: vec![spoolframe_protocol::experimental::FEDERATION.into()],
        };
        app.call(None, Request::Hello(hello)).await;
        pair.sb.add_user("bob", "correct horse", None).await;
        let bob = app.login("bob", "correct horse").await;
        let library = pair.fed_library("plex:1");
        let values = app
            .call(
                Some(&bob),
                Request::ItemFilterValues {
                    library: library.clone(),
                },
            )
            .await;
        let Response::Ok(Reply::ItemFilterValues { values }) = values else {
            panic!("{values:?}");
        };
        let genres: Vec<&str> = values.genres.iter().map(|g| g.value.as_str()).collect();
        assert_eq!(genres, ["Horror", "Science Fiction"]);
        assert!(values.sorts.contains(&SortKey::Size));
        let page = |filter: ItemFilter| Request::Items {
            library: library.clone(),
            offset: 0,
            limit: 10,
            sort: Some(ItemSort {
                key: SortKey::Added,
                descending: true,
            }),
            filter: Some(filter),
        };
        let hdr = app
            .call(
                Some(&bob),
                page(ItemFilter {
                    dynamic_ranges: vec![DynamicRange::Hdr],
                    watched: vec![spoolframe_types::WatchedState::Unwatched],
                    ..ItemFilter::default()
                }),
            )
            .await;
        let Response::Ok(Reply::Items { items, total, .. }) = hdr else {
            panic!("{hdr:?}");
        };
        assert_eq!(
            (
                items.iter().map(|i| i.id().clone()).collect::<Vec<_>>(),
                total
            ),
            (vec![pair.fed(movie().id())], 1)
        );
        let comedy = app
            .call(
                Some(&bob),
                page(ItemFilter {
                    genres: vec!["Comedy".into()],
                    ..ItemFilter::default()
                }),
            )
            .await;
        let Response::Ok(Reply::Items { items, total, .. }) = comedy else {
            panic!("{comedy:?}");
        };
        assert_eq!((items.len(), total), (0, 0));
        pair.stop().await;
    }

    /// SB's admin browses what SA shares from the web UI: the library
    /// under SA's name, online; its page sorted and filtered by the same
    /// query; the film as cached; and still all of it once SA is gone
    /// offline, marked so.
    #[tokio::test]
    async fn an_admin_browses_a_paired_server_s_libraries_from_the_cache() {
        use spoolframe_admin_api::{
            AdminApiClient, DynamicRange, ItemFilter, ItemSort, MeApiClient, PairedQuery, SortKey,
            error_code,
        };
        use spoolframe_server::admin::{Admin, Origins, TrustedProxies};
        let pair = Pair::start("fed-admin-browse").await;
        pair.share("plex:1").await;
        pair.refresh().await;
        pair.sb.add_user("bob", "correct horse", None).await;
        pair.sb
            .store
            .set_role("bob", spoolframe_types::Role::Admin)
            .await
            .unwrap();
        let (addr, handle) = spoolframe_server::admin::serve(
            "127.0.0.1:0".parse().unwrap(),
            Admin {
                federation: pair.sb.federation.clone(),
                watch: Some(pair.sb.watch.clone()),
                plex: pair.cache.clone(),
                ..crate::admin::admin(pair.sb.auth.clone(), None)
            },
            Origins::default(),
            TrustedProxies::default(),
        )
        .await
        .unwrap();
        std::mem::forget(handle);
        let token = crate::admin::anonymous(addr)
            .await
            .login(crate::admin::creds("bob", "correct horse"))
            .await
            .unwrap()
            .token;
        let bob = crate::admin::with_token(addr, &token).await;

        let servers = bob.catalog_paired_libraries().await.unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(
            (servers[0].name.as_str(), servers[0].online),
            ("Alice's server", true)
        );
        let library = pair.fed_library("plex:1").to_string();
        assert_eq!(servers[0].libraries[0].id, library);
        let values = bob.catalog_filter_values(library.clone()).await.unwrap();
        assert!(values.sorts.contains(&SortKey::Size));
        let hdr = PairedQuery {
            library: library.clone(),
            offset: 0,
            limit: 50,
            sort: Some(ItemSort {
                key: SortKey::Added,
                descending: true,
            }),
            item_filter: Some(ItemFilter {
                dynamic_ranges: vec![DynamicRange::Hdr],
                ..ItemFilter::default()
            }),
        };
        let page = bob.catalog_paired_items(hdr.clone()).await.unwrap();
        let alien = pair.fed(movie().id());
        assert_eq!(
            (
                page.items
                    .iter()
                    .map(|i| i.id().clone())
                    .collect::<Vec<_>>(),
                page.total
            ),
            (vec![alien.clone()], 1)
        );
        let item = bob.catalog_paired_item(alien.to_string()).await.unwrap();
        assert_eq!(item.item.id(), &alien);
        assert_eq!(item.server_name, "Alice's server");
        assert_eq!(item.library.map(|l| l.id), Some(library.clone()));
        // A page asked past the limit, and an id of this server's own.
        let too_many = PairedQuery {
            limit: 201,
            ..hdr.clone()
        };
        assert_eq!(
            crate::admin::code(bob.catalog_paired_items(too_many).await.unwrap_err()),
            error_code::INVALID_INPUT
        );
        assert_eq!(
            crate::admin::code(
                bob.catalog_paired_item(movie().id().to_string())
                    .await
                    .unwrap_err()
            ),
            error_code::INVALID_INPUT
        );
        // SA gone offline: the cache still answers, marked offline.
        let origin = pair.origin().to_owned();
        pair.sa.stop().await;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while federation(&pair.sb).online(&origin) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let servers = bob.catalog_paired_libraries().await.unwrap();
        assert!(!servers[0].online);
        let page = bob.catalog_paired_items(hdr).await.unwrap();
        assert_eq!((page.total, page.online), (1, false));
        pair.sb.stop().await;
    }

    /// The whole of it: SA shares Movies, SB syncs and serves the film
    /// from its cache under its `fed:` id, with its external ids and the
    /// poster fetched as it came. Nothing of it is in SB's own libraries.
    #[tokio::test]
    async fn a_shared_library_is_cached_and_served_from_the_cache() {
        let pair = Pair::start("fed-replica-movies").await;
        pair.sa.catalog.show_posters();
        pair.share("plex:1").await;
        pair.refresh().await;
        assert!(federation(&pair.sb).online(pair.origin()));
        assert_eq!(pair.titles("plex:1").await.unwrap(), ["Alien"]);
        let alien = pair.fed(movie().id());
        let found = pair.cache.item(&alien).await.unwrap();
        assert_eq!(found.item.id(), &alien);
        assert_eq!(found.external_ids, ["test://1"]);
        let Item::Movie(film) = &found.item else {
            panic!("not a film");
        };
        assert_eq!(film.year, Some(1979));
        let poster = pair.fed_artwork(&poster_of("1"));
        assert_eq!(film.artwork[0].id, poster);
        let image = pair.cache.artwork(&poster).await.unwrap();
        assert_eq!(image.content_type, "image/png");
        // Its file gone, it is fetched from SA again when asked for.
        let files = pair.sb.dir.join("cache").join("federation");
        assert!(files.join(pair.origin()).is_dir());
        std::fs::remove_dir_all(&files).unwrap();
        assert_eq!(pair.cache.artwork(&poster).await.unwrap(), image);
        assert!(files.join(pair.origin()).is_dir());
        assert_eq!(
            pair.cache.library_of(&alien).await.unwrap(),
            Some(pair.fed_library("plex:1"))
        );
        // SB's own libraries are as they were: none. The client protocol
        // adds the cache's, and only for an app that names the feature.
        assert!(pair.cache.libraries().await.unwrap().is_empty());
        pair.stop().await;
    }

    /// A sync is measured on both sides: SB counts it, with its time and
    /// when, and SA counts the requests it answered for it; SB's census
    /// finds SA paired and online.
    #[tokio::test]
    async fn a_sync_moves_the_federation_s_metrics() {
        let pair = Pair::start("fed-replica-metrics").await;
        pair.share("plex:1").await;
        let number = |server: &TestServer, name: &str, labels: &[(&str, &str)]| {
            server
                .metrics
                .families()
                .into_iter()
                .find(|f| f.name == name)
                .and_then(|f| f.value(labels))
                .unwrap_or(0.0)
        };
        let ok = [("outcome", "ok")];
        let synced = number(&pair.sb, "spoolframe_federation_syncs_total", &ok);
        let answered = number(
            &pair.sa,
            "spoolframe_federation_requests_total",
            &[("request", "sync"), ("outcome", "ok")],
        );
        pair.refresh().await;
        // The replica may sync too meanwhile: at least the one asked for.
        assert!(number(&pair.sb, "spoolframe_federation_syncs_total", &ok) > synced);
        assert!(
            number(
                &pair.sa,
                "spoolframe_federation_requests_total",
                &[("request", "sync"), ("outcome", "ok")],
            ) > answered
        );
        assert!(
            number(
                &pair.sb,
                "spoolframe_federation_last_sync_timestamp_seconds",
                &[]
            ) > 0.0
        );
        let timed = pair
            .sb
            .metrics
            .families()
            .into_iter()
            .find(|f| f.name == "spoolframe_federation_sync_duration_seconds")
            .unwrap();
        assert!(
            timed
                .samples
                .iter()
                .any(|s| s.labels == [("outcome", "ok".to_owned())]),
            "{timed:?}"
        );
        pair.sb.metrics.refresh().await;
        assert_eq!(number(&pair.sb, "spoolframe_federation_peers", &[]), 1.0);
        assert_eq!(
            number(&pair.sb, "spoolframe_federation_peers_online", &[]),
            1.0
        );
        pair.stop().await;
    }

    /// Bob on SB, signed in on an app that names the experimental feature
    /// (`federated`) or one that does not.
    async fn bob_on(pair: &Pair, federated: bool) -> (TestClient, SessionId) {
        if pair.sb.store.user_by_name("bob").await.unwrap().is_none() {
            pair.sb.add_user("bob", "correct horse", None).await;
        }
        let app = pair.sb.client().await;
        let experimental: &[&str] = if federated {
            &[spoolframe_protocol::experimental::FEDERATION]
        } else {
            &[]
        };
        app.hello_naming(crate::support::mkv_capabilities(), experimental)
            .await;
        let bob = app.login("bob", "correct horse").await;
        (app, bob)
    }

    /// Bob is 40 minutes into `id` on SB.
    async fn bob_watched(pair: &Pair, id: &MediaId) {
        let bob = pair.sb.store.user_by_name("bob").await.unwrap().unwrap().id;
        pair.sb
            .store
            .save_watch(
                spoolframe_server::principal::Principal::LocalUser(bob),
                WatchUpdate {
                    media_id: &id.to_string(),
                    show_id: None,
                    external_ids: &[],
                    position_ms: Some(2_400_000),
                    duration_ms: Some(7_020_000),
                    watched: false,
                    unhide: true,
                },
            )
            .await
            .unwrap();
    }

    async fn libraries(app: &TestClient, bob: &SessionId) -> Vec<spoolframe_types::Library> {
        match app.call(Some(bob), Request::Libraries).await {
            Response::Ok(Reply::Libraries { libraries }) => libraries,
            other => panic!("{other:?}"),
        }
    }

    async fn home(app: &TestClient, bob: &SessionId) -> spoolframe_protocol::HomeScreen {
        match app.call(Some(bob), Request::Home).await {
            Response::Ok(Reply::Home(home)) => home,
            other => panic!("{other:?}"),
        }
    }

    /// The ids a search for `query` finds, its top match first, by an app
    /// that does not name the feature: plain results.
    async fn found_old(app: &TestClient, bob: &SessionId, query: &str) -> Vec<MediaId> {
        found(app, bob, query).await
    }

    /// The ids a search for `query` finds, its top match first: the hits'
    /// items for an app that names the feature.
    async fn found(app: &TestClient, bob: &SessionId, query: &str) -> Vec<MediaId> {
        let results = match app
            .call(Some(bob), Request::Search(crate::support::search(query)))
            .await
        {
            Response::Ok(Reply::Search(results)) => results,
            Response::Ok(Reply::SearchHits(hits)) => hits.map(|hit| hit.item),
            other => panic!("{other:?}"),
        };
        let top = match results.top {
            Some(spoolframe_protocol::TopResult::Item(item)) => Some(item.id().clone()),
            _ => None,
        };
        top.into_iter()
            .chain(
                results
                    .groups
                    .iter()
                    .flat_map(|g| g.items.iter().map(|i| i.id().clone())),
            )
            .collect()
    }

    fn ids(items: &[Item]) -> Vec<MediaId> {
        items.iter().map(|i| i.id().clone()).collect()
    }

    /// P8-02's client exposure: an app that names the feature lists SA's
    /// library with its server, finds SA's film, has it in Continue
    /// watching and in SA's own Home row, which Bob turns off and on.
    #[tokio::test]
    async fn an_app_naming_the_feature_browses_and_finds_what_sa_shares() {
        let pair = Pair::start("fed-exposure").await;
        pair.share("plex:1").await;
        pair.refresh().await;
        let alien = pair.fed(movie().id());
        let (app, bob) = bob_on(&pair, true).await;
        bob_watched(&pair, &alien).await;
        let server = spoolframe_types::LibraryServer {
            server_id: pair.origin().to_owned(),
            name: "Alice's server".into(),
            online: true,
        };
        let listed = libraries(&app, &bob).await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, pair.fed_library("plex:1"));
        assert_eq!(listed[0].server.as_ref(), Some(&server));
        let screen = home(&app, &bob).await;
        assert_eq!(ids(&screen.continue_watching), std::slice::from_ref(&alien));
        assert!(screen.unavailable.is_empty());
        assert_eq!(screen.servers.len(), 1);
        assert_eq!(screen.servers[0].server, server);
        assert!(screen.servers[0].shown);
        assert_eq!(ids(&screen.servers[0].items), std::slice::from_ref(&alien));
        assert_eq!(
            found(&app, &bob, "alien").await,
            std::slice::from_ref(&alien)
        );
        let turned = |shown: bool| Request::SetServerRow {
            server_id: pair.origin().to_owned(),
            shown,
        };
        assert_eq!(
            app.call(Some(&bob), turned(false)).await,
            Response::Ok(Reply::Done)
        );
        let screen = home(&app, &bob).await;
        assert!(!screen.servers[0].shown);
        assert!(screen.servers[0].items.is_empty());
        assert_eq!(
            ids(&screen.continue_watching),
            std::slice::from_ref(&alien),
            "a server's items stay in Continue watching"
        );
        app.call(Some(&bob), turned(true)).await;
        assert!(home(&app, &bob).await.servers[0].shown);
        assert!(matches!(
            app.call(
                Some(&bob),
                Request::SetServerRow {
                    server_id: "bbbbbbbbbbbbbbbb".into(),
                    shown: false,
                },
            )
            .await,
            Response::Err(Error::NotFound { .. })
        ));
        pair.stop().await;
    }

    /// SB's admin's name for SA is what Bob's app calls it, in the list
    /// and on Home; with SA's row turned off for everyone, Home has none,
    /// and SA's library is still listed.
    #[tokio::test]
    async fn an_admin_s_name_and_home_switch_for_sa_reach_the_app() {
        let pair = Pair::start("fed-exposure-alias").await;
        pair.share("plex:1").await;
        pair.refresh().await;
        let (app, bob) = bob_on(&pair, true).await;
        let federation = pair.sb.federation.as_ref().unwrap();
        federation
            .rename(&pair.sa_on_sb, Some("Mum's"), "bob")
            .await
            .unwrap();
        let listed = libraries(&app, &bob).await;
        assert_eq!(listed[0].server.as_ref().unwrap().name, "Mum's");
        let screen = home(&app, &bob).await;
        assert_eq!(screen.servers.len(), 1);
        assert_eq!(screen.servers[0].server.name, "Mum's");
        federation
            .show_home_row(&pair.sa_on_sb, false, "bob")
            .await
            .unwrap();
        assert!(home(&app, &bob).await.servers.is_empty());
        assert_eq!(libraries(&app, &bob).await.len(), 1);
        federation
            .show_home_row(&pair.sa_on_sb, true, "bob")
            .await
            .unwrap();
        federation
            .rename(&pair.sa_on_sb, None, "bob")
            .await
            .unwrap();
        let screen = home(&app, &bob).await;
        assert_eq!(screen.servers.len(), 1);
        assert_eq!(screen.servers[0].server.name, "Alice's server");
        pair.stop().await;
    }

    /// An app that does not name the feature sees SB as if it had no
    /// pairs: no library, no Home row, no hit, no Continue watching card
    /// of SA's, and no `fed:` id anywhere; it may not set a server's row.
    #[tokio::test]
    async fn an_app_not_naming_the_feature_sees_nothing_of_sa() {
        let pair = Pair::start("fed-exposure-old").await;
        pair.share("plex:1").await;
        pair.refresh().await;
        let (app, bob) = bob_on(&pair, false).await;
        bob_watched(&pair, &pair.fed(movie().id())).await;
        assert!(libraries(&app, &bob).await.is_empty());
        let screen = home(&app, &bob).await;
        assert_eq!(screen, spoolframe_protocol::HomeScreen::default());
        assert!(found(&app, &bob, "alien").await.is_empty());
        assert!(matches!(
            app.call(
                Some(&bob),
                Request::SetServerRow {
                    server_id: pair.origin().to_owned(),
                    shown: false,
                },
            )
            .await,
            Response::Err(Error::Unsupported { .. })
        ));
        // Nor by id, whatever the request (L1).
        let alien = pair.fed(movie().id());
        for request in [
            Request::Item { id: alien.clone() },
            Request::MarkWatched { id: alien.clone() },
            Request::Items {
                library: pair.fed_library("plex:1"),
                offset: 0,
                limit: 10,
                sort: None,
                filter: None,
            },
        ] {
            assert!(
                matches!(
                    app.call(Some(&bob), request.clone()).await,
                    Response::Err(Error::NotFound { .. })
                ),
                "{request:?}"
            );
        }
        pair.stop().await;
    }

    /// F8-11: nor by an image's id, which is a resource, not a request: an
    /// app that does not name the feature is told `NotFound` and sent no
    /// bytes of SA's poster, as it is, or resized; one that names it gets
    /// the image.
    #[tokio::test]
    async fn an_app_not_naming_the_feature_gets_no_image_of_sa_s() {
        use spoolframe_protocol::ArtworkResponse;
        use spoolframe_protocol::ArtworkSize;
        let pair = Pair::start("fed-exposure-old-image").await;
        pair.sa.catalog.show_posters();
        pair.share("plex:1").await;
        pair.refresh().await;
        let poster = pair.fed_artwork(&poster_of("1"));
        let (app, bob) = bob_on(&pair, true).await;
        let (old, bob_old) = bob_on(&pair, false).await;
        for size in [ArtworkSize::Original, ArtworkSize::Small] {
            let (response, bytes) = app.artwork(&bob, poster.clone(), size).await;
            assert!(
                matches!(response, ArtworkResponse::Ok(_)),
                "{size:?}: {response:?}"
            );
            assert!(!bytes.is_empty());
            let (response, bytes) = old.artwork(&bob_old, poster.clone(), size).await;
            assert!(
                matches!(response, ArtworkResponse::Err(Error::NotFound { .. })),
                "{size:?}: {response:?}"
            );
            assert!(bytes.is_empty());
        }
        pair.stop().await;
    }

    /// A portable account on SB is not SB's own: federation is for SB's
    /// local accounts alone. Signed in on an app that names the feature,
    /// it sees SB as if it had no pairs: no library, Home row, hit or
    /// image of SA's, `NotFound` for any `fed:` id and no server row to
    /// set; Bob, an account here, on the same kind of app sees all of it.
    #[tokio::test]
    async fn a_portable_account_sees_nothing_of_sa() {
        use spoolframe_protocol::{ArtworkResponse, ArtworkSize, HomeScreen};
        let pair = Pair::start("fed-exposure-portable").await;
        pair.sa.catalog.show_posters();
        pair.share("plex:1").await;
        pair.refresh().await;
        let alien = pair.fed(movie().id());
        let library = pair.fed_library("plex:1");
        let poster = pair.fed_artwork(&poster_of("1"));

        let (app, bob) = bob_on(&pair, true).await;
        let listed = libraries(&app, &bob).await;
        assert_eq!(
            listed.iter().map(|l| l.id.clone()).collect::<Vec<_>>(),
            std::slice::from_ref(&library)
        );
        assert_eq!(home(&app, &bob).await.servers.len(), 1);
        assert_eq!(
            found(&app, &bob, "alien").await,
            std::slice::from_ref(&alien)
        );
        let (response, _) = app.artwork(&bob, poster.clone(), ArtworkSize::Small).await;
        assert!(matches!(response, ArtworkResponse::Ok(_)), "{response:?}");

        pair.sb
            .store
            .grant_portable(crate::portable_sign_in::identity(7), Some("Sam"), None)
            .await
            .unwrap();
        let tv = pair.sb.client().await;
        tv.hello_naming(
            crate::support::mkv_capabilities(),
            &[spoolframe_protocol::experimental::FEDERATION],
        )
        .await;
        let sam =
            crate::portable_sign_in::session(crate::portable_sign_in::sign_in(&tv, 7, 9).await);
        assert!(libraries(&tv, &sam).await.is_empty());
        assert_eq!(home(&tv, &sam).await, HomeScreen::default());
        assert!(found(&tv, &sam, "alien").await.is_empty());
        assert!(matches!(
            tv.call(
                Some(&sam),
                Request::SetServerRow {
                    server_id: pair.origin().to_owned(),
                    shown: false,
                },
            )
            .await,
            Response::Err(Error::Unsupported { .. })
        ));
        for request in [
            Request::Item { id: alien.clone() },
            Request::MarkWatched { id: alien.clone() },
            Request::Items {
                library: library.clone(),
                offset: 0,
                limit: 10,
                sort: None,
                filter: None,
            },
        ] {
            assert!(
                matches!(
                    tv.call(Some(&sam), request.clone()).await,
                    Response::Err(Error::NotFound { .. })
                ),
                "{request:?}"
            );
        }
        for size in [ArtworkSize::Original, ArtworkSize::Small] {
            let (response, bytes) = tv.artwork(&sam, poster.clone(), size).await;
            assert!(
                matches!(response, ArtworkResponse::Err(Error::NotFound { .. })),
                "{size:?}: {response:?}"
            );
            assert!(bytes.is_empty());
        }
        pair.stop().await;
    }

    /// Unpaired, SA's film Bob was watching stays in his Continue
    /// watching, listed as no longer available, for an app that names the
    /// feature; and SA's library and row are gone.
    #[tokio::test]
    async fn after_unpairing_continue_watching_says_sa_s_film_is_no_longer_available() {
        let pair = Pair::start("fed-exposure-unpaired").await;
        pair.share("plex:1").await;
        pair.refresh().await;
        let alien = pair.fed(movie().id());
        let (app, bob) = bob_on(&pair, true).await;
        bob_watched(&pair, &alien).await;
        federation(&pair.sb)
            .unpair(&pair.sa_on_sb, "bob")
            .await
            .unwrap()
            .unwrap();
        let screen = home(&app, &bob).await;
        assert_eq!(ids(&screen.continue_watching), std::slice::from_ref(&alien));
        assert_eq!(screen.unavailable, [alien]);
        assert!(screen.servers.is_empty());
        assert!(libraries(&app, &bob).await.is_empty());
        let (old, bob_old) = bob_on(&pair, false).await;
        assert!(home(&old, &bob_old).await.continue_watching.is_empty());
        pair.stop().await;
    }

    /// An app that names the feature is told `LibrariesChanged` when SA
    /// stops sharing a library, and when SA goes offline.
    #[tokio::test]
    async fn an_app_naming_the_feature_is_told_when_the_libraries_change() {
        use spoolframe_protocol::Event;
        use spoolframe_protocol::codec::read_frame;
        use spoolframe_protocol::streams::uni;
        use tokio::io::AsyncReadExt;
        let pair = Pair::start("fed-exposure-told").await;
        pair.share("plex:1").await;
        pair.refresh().await;
        let (app, bob) = bob_on(&pair, true).await;
        let mut events = tokio::time::timeout(Duration::from_secs(5), app.conn.accept_uni())
            .await
            .expect("no events stream within 5 s")
            .unwrap();
        assert_eq!(events.read_u8().await.unwrap(), uni::EVENTS);
        // Any signed-in request makes the connection one that is told.
        libraries(&app, &bob).await;
        let mut next = async || loop {
            let event =
                tokio::time::timeout(Duration::from_secs(10), read_frame::<_, Event>(&mut events))
                    .await
                    .expect("no event within 10 s")
                    .unwrap();
            if !matches!(event, Event::ServerAddresses { .. }) {
                return event;
            }
        };
        federation(&pair.sa)
            .unshare(&pair.sb_on_sa, "plex:1", "alice")
            .await
            .unwrap();
        pair.refresh().await;
        assert_eq!(next().await, Event::LibrariesChanged);
        pair.sa.stop().await;
        assert_eq!(next().await, Event::LibrariesChanged);
        pair.sb.stop().await;
    }

    /// Each origin's artwork takes at most `federation.cache_max_mb`:
    /// past it, the least recently used file goes, and an image whose file
    /// went is fetched again when it is asked for.
    #[tokio::test]
    async fn artwork_past_the_cap_goes_and_is_fetched_again() {
        let pair = Pair::start_with(
            "fed-replica-cap",
            ServerOptions {
                federation_cache_max_bytes: 1,
                ..ServerOptions::default()
            },
        )
        .await;
        pair.sa.catalog.show_posters();
        pair.share("plex:1").await;
        pair.refresh().await;
        let files = pair
            .sb
            .dir
            .join("cache")
            .join("federation")
            .join(pair.origin());
        let kept = |dir: &std::path::Path| {
            std::fs::read_dir(dir).map_or(0, |d| d.filter(|f| f.is_ok()).count())
        };
        assert_eq!(kept(&files), 0, "the poster is over the cap");
        let poster = pair.fed_artwork(&poster_of("1"));
        assert!(pair.cache.artwork(&poster).await.is_ok());
        assert_eq!(kept(&files), 0);
        pair.stop().await;
    }

    /// A sync's posters are fetched by the replica's next tick, outside
    /// the sync (M5).
    #[tokio::test]
    async fn posters_come_at_the_replica_s_next_tick() {
        let pair = Pair::start("fed-replica-posters").await;
        pair.sa.catalog.show_posters();
        pair.share("plex:1").await;
        pair.refresh().await;
        pair.posters_fetched(1).await;
        pair.stop().await;
    }

    /// SA cannot answer for a poster (its PMS fails it: `unavailable`).
    /// SB does not ask for it again at each tick, as it did every 15 s:
    /// it waits 30 s, then twice as long after each failure in a row, and
    /// asks for it again once it waited.
    #[tokio::test]
    async fn a_poster_sa_cannot_answer_is_asked_for_less_and_less_often() {
        let pair = Pair::start("fed-replica-poster-backoff").await;
        pair.sa.catalog.show_posters();
        pair.sa.catalog.fail_images();
        pair.share("plex:1").await;
        pair.refresh().await;
        // A sync the share set off may come before the refresh's, and a
        // sync asks for a poster that failed again: once the asks after
        // both are in, they stop.
        until(|| async {
            pair.wake_replica();
            pair.sa.catalog.artwork_asks() > 0
        })
        .await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        let asked = pair.sa.catalog.artwork_asks();
        pair.not_asked_again(asked).await;

        // Each ask failed: the wait after the last is 30 s doubled for
        // each failure before it. Half of it is not enough, all of it is.
        let clock = pair.sb.store.clock();
        let mut wait = Duration::from_secs(30 << (asked - 1));
        for asked in asked..asked + 2 {
            clock.advance(wait / 2);
            pair.not_asked_again(asked).await;
            clock.advance(wait / 2);
            pair.asked(asked + 1).await;
            wait *= 2;
        }
        assert_eq!(kept(&pair.files()), 0);
        pair.stop().await;
    }

    /// An image that is not one is neither kept nor served (L7), whatever
    /// type it says it is.
    #[tokio::test]
    async fn only_an_image_is_kept_of_a_paired_server_s_artwork() {
        let pair = Pair::start("fed-replica-not-images").await;
        pair.sa.catalog.show_posters();
        pair.sa.catalog.bogus_images("text/html");
        pair.share("plex:1").await;
        pair.refresh().await;
        let poster = pair.fed_artwork(&poster_of("1"));
        for content_type in ["text/html", "image/png"] {
            pair.sa.catalog.bogus_images(content_type);
            assert!(pair.cache.artwork(&poster).await.is_err(), "{content_type}");
        }
        assert_eq!(kept(&pair.files()), 0);
        pair.stop().await;
    }

    /// SA gains a film: SB has it at its next sync, without anything
    /// else done.
    #[tokio::test]
    async fn an_item_sa_adds_is_cached_at_the_next_sync() {
        let pair = Pair::start("fed-replica-adds").await;
        let alien = id(MediaKind::Movie, "1");
        pair.sa.catalog.not_yet(&alien);
        pair.share("plex:1").await;
        pair.refresh().await;
        assert!(pair.titles("plex:1").await.unwrap().is_empty());
        pair.sa.catalog.gain(&alien);
        pair.refresh().await;
        assert_eq!(pair.titles("plex:1").await.unwrap(), ["Alien"]);
        pair.stop().await;
    }

    /// SA revokes the grant: SB's next sync drops the library, and the
    /// film and its artwork with it.
    #[tokio::test]
    async fn a_revoked_grant_drops_the_cached_library() {
        let pair = Pair::start("fed-replica-revoke").await;
        pair.sa.catalog.show_posters();
        pair.share("plex:1").await;
        pair.refresh().await;
        let alien = pair.fed(movie().id());
        assert!(pair.cache.item(&alien).await.is_ok());
        federation(&pair.sa)
            .unshare(&pair.sb_on_sa, "plex:1", "alice")
            .await
            .unwrap();
        pair.refresh().await;
        assert!(matches!(
            pair.cache.item(&alien).await,
            Err(CatalogError::NotFound(_))
        ));
        assert!(matches!(
            pair.titles("plex:1").await,
            Err(CatalogError::NotFound(_))
        ));
        assert!(
            pair.cache
                .authorize_artwork(&pair.fed_artwork(&poster_of("1")))
                .await
                .is_err()
        );
        pair.stop().await;
    }

    /// With SA offline, SB still browses what it cached, its poster too,
    /// and says SA is offline.
    #[tokio::test]
    async fn with_sa_offline_the_cache_is_still_served() {
        let pair = Pair::start("fed-replica-offline").await;
        pair.sa.catalog.show_posters();
        pair.share("plex:1").await;
        pair.refresh().await;
        pair.posters_fetched(1).await;
        let Pair { sa, sb, cache, .. } = pair;
        let origin = sa.server_id.to_string();
        sa.stop().await;
        until(|| async { !federation(&sb).online(&origin) }).await;
        let alien = MediaId::federated(&origin, movie().id()).unwrap();
        assert_eq!(title(&cache.item(&alien).await.unwrap().item), "Alien");
        let poster = ArtworkId::federated(&origin, &poster_of("1")).unwrap();
        assert!(cache.artwork(&poster).await.is_ok());
        sb.stop().await;
    }

    /// A shared music library replicates whole: the artist, its albums,
    /// their tracks, and the library's tracks at once.
    #[tokio::test]
    async fn a_music_library_replicates_artists_albums_and_tracks() {
        let pair = Pair::start("fed-replica-music").await;
        pair.sa.catalog.serve_music();
        pair.share("plex:2").await;
        pair.refresh().await;
        assert_eq!(pair.titles("plex:2").await.unwrap(), ["Daft Punk"]);
        let artist = pair
            .cache
            .item(&pair.fed(&id(MediaKind::Artist, "30")))
            .await
            .unwrap();
        assert_eq!(
            artist.children.iter().map(title).collect::<Vec<_>>(),
            ["Discovery", "Homework"]
        );
        let album = pair
            .cache
            .item(&pair.fed(&id(MediaKind::Album, "31")))
            .await
            .unwrap();
        let Item::Track(first) = &album.children[0] else {
            panic!("not a track");
        };
        // In the album's order, with its ids the origin's.
        assert_eq!(first.track, Some(1));
        assert_eq!(first.album_id, pair.fed(&id(MediaKind::Album, "31")));
        assert_eq!(first.artist_id, pair.fed(&id(MediaKind::Artist, "30")));
        let tracks = pair
            .cache
            .tracks(&pair.fed_library("plex:2"))
            .await
            .unwrap();
        assert_eq!(tracks.len(), 3);
        pair.stop().await;
    }

    /// A user holds at most `Limits::tokens.per_user` play tokens of SA's at
    /// once (H3): past them, SA says to try later, and mints again once
    /// one expires.
    #[tokio::test]
    async fn a_user_s_play_tokens_are_capped() {
        use spoolframe_protocol::federation::AssertionRefused;
        use spoolframe_server::federation::{AssertError, Limits, PLAY_TOKEN_LIFETIME};
        let pair = Pair::start("fed-assert-flood").await;
        pair.share("plex:1").await;
        pair.refresh().await;
        pair.sb.add_user("bob", "correct horse", None).await;
        let bob = pair.sb.store.user_by_name("bob").await.unwrap().unwrap().id;
        let device = iroh::SecretKey::from_bytes(&[3; 32]).public();
        let alien = pair.fed(movie().id());
        let sb = federation(&pair.sb);
        for _ in 0..Limits::default().tokens.per_user {
            sb.assert_play(bob, &alien, device).await.unwrap();
        }
        assert!(matches!(
            sb.assert_play(bob, &alien, device).await,
            Err(AssertError::Dial(DialError::NotGranted(
                AssertionRefused::Unavailable
            )))
        ));
        pair.sa.store.clock().advance(PLAY_TOKEN_LIFETIME);
        sb.assert_play(bob, &alien, device).await.unwrap();
        pair.stop().await;
    }

    /// P8-03, across two servers: SB vouches for Bob, who wants SA's film
    /// on a device of his; SA mints a token for that film and device,
    /// kept hashed under Bob's remote principal; SB counts the play, and SA
    /// will once the token is redeemed (P8-04).
    /// A film not shared gets no token, and both count the refusal. The
    /// grant revoked, the token is gone; and SB never vouches for a
    /// portable guest.
    #[tokio::test]
    async fn sb_vouches_for_bob_and_sa_mints_a_play_token() {
        use spoolframe_protocol::federation::{AssertionRefused, TokenScope};
        use spoolframe_server::auth::SessionToken;
        use spoolframe_server::federation::{AssertError, PLAY_TOKEN_LIFETIME};
        use spoolframe_server::store::federation_tokens::PlaySide;
        let pair = Pair::start("fed-assert").await;
        pair.share("plex:1").await;
        pair.refresh().await;
        pair.sb.add_user("bob", "correct horse", None).await;
        let bob = pair.sb.store.user_by_name("bob").await.unwrap().unwrap().id;
        let device = iroh::SecretKey::from_bytes(&[3; 32]).public();
        let alien = pair.fed(movie().id());
        let granted = federation(&pair.sb)
            .assert_play(bob, &alien, device)
            .await
            .unwrap();
        assert_eq!(granted.scope, TokenScope::Play);
        let now = pair.sa.store.clock().unix_secs();
        let lifetime = PLAY_TOKEN_LIFETIME.as_secs() as i64;
        assert!((now..=now + lifetime).contains(&granted.expires_at));
        let hash = SessionToken(granted.token.clone()).hash();
        let kept = pair
            .sa
            .store
            .federation_token(&hash)
            .await
            .unwrap()
            .expect("kept by its hash");
        assert_eq!(
            kept.principal,
            format!("remote:{}:{bob}", pair.sb.server_id)
        );
        assert_eq!(
            (
                kept.item.as_str(),
                kept.library.as_str(),
                kept.device.as_str(),
                kept.peer.as_str(),
                kept.scope.as_str(),
            ),
            (
                "plex:movie:1",
                "plex:1",
                device.to_string().as_str(),
                pair.sb_on_sa.as_str(),
                "play",
            )
        );
        // Plays and refusals: SA's of SB's users, SB's of its users on SA.
        async fn counted(pair: &Pair, side: PlaySide) -> (u64, u64) {
            let (store, peer) = match side {
                PlaySide::Served => (&pair.sa.store, &pair.sb_on_sa),
                PlaySide::Asserted => (&pair.sb.store, &pair.sa_on_sb),
            };
            let counted = store.federation_plays(peer, side).await.unwrap();
            (counted.plays, counted.refused)
        }
        let served = |side| counted(&pair, side);
        // SA counts the play once the token is redeemed (P8-04).
        assert_eq!(served(PlaySide::Served).await, (0, 0));
        assert_eq!(served(PlaySide::Asserted).await, (1, 0));

        // Music is not shared; and an artist does not play (M6).
        let refused = |item: MediaId| {
            let federation = federation(&pair.sb);
            async move {
                match federation.assert_play(bob, &item, device).await {
                    Err(AssertError::Dial(DialError::NotGranted(reason))) => reason,
                    other => panic!("{other:?}"),
                }
            }
        };
        assert_eq!(
            refused(pair.fed(&id(MediaKind::Track, "32"))).await,
            AssertionRefused::NotShared
        );
        assert_eq!(
            refused(pair.fed(&id(MediaKind::Artist, "30"))).await,
            AssertionRefused::InvalidScope
        );
        assert_eq!(served(PlaySide::Served).await, (0, 2));
        assert_eq!(served(PlaySide::Asserted).await, (1, 2));

        federation(&pair.sa)
            .unshare(&pair.sb_on_sa, "plex:1", "alice")
            .await
            .unwrap();
        assert_eq!(pair.sa.store.federation_token(&hash).await.unwrap(), None);
        assert!(matches!(
            federation(&pair.sb)
                .assert_play(bob, &pair.fed(movie().id()), device)
                .await,
            Err(AssertError::Dial(DialError::NotGranted(
                AssertionRefused::NotShared
            )))
        ));
        assert!(matches!(
            federation(&pair.sb)
                .assert_play(bob, movie().id(), device)
                .await,
            Err(AssertError::NotFederated(_))
        ));
        pair.stop().await;
    }

    /// What the admin's pair list says of a pair (P8-02, P8-03): when what
    /// it shares was last synced, and its plays and refusals each way, one
    /// number per server.
    #[tokio::test]
    async fn a_pair_says_its_last_sync_and_its_plays_each_way() {
        let pair = Pair::start("fed-pair-state").await;
        let sb = federation(&pair.sb);
        assert_eq!(sb.last_sync(pair.origin()).await.unwrap(), None);
        pair.share("plex:1").await;
        pair.refresh().await;
        let synced = sb.last_sync(pair.origin()).await.unwrap();
        let now = pair.sb.store.clock().unix_secs();
        assert!(synced.is_some_and(|at| at <= now), "{synced:?}");
        pair.sb.add_user("bob", "correct horse", None).await;
        let bob = pair.sb.store.user_by_name("bob").await.unwrap().unwrap().id;
        let device = iroh::SecretKey::from_bytes(&[3; 32]).public();
        sb.assert_play(bob, &pair.fed(movie().id()), device)
            .await
            .unwrap();
        // An artist does not play: refused, and counted on both sides.
        let artist = pair.fed(&MediaId::new(Namespace::Plex, MediaKind::Artist, "30").unwrap());
        assert!(sb.assert_play(bob, &artist, device).await.is_err());
        let counted = |federation: &Arc<Federation>, peer: String| {
            let federation = federation.clone();
            async move {
                let (here, there) = federation.play_counts(&peer).await.unwrap();
                (here.plays, there.plays, here.refused, there.refused)
            }
        };
        assert_eq!(counted(sb, pair.sa_on_sb.clone()).await, (0, 1, 0, 1));
        // SA counts its play once the token is redeemed (P8-04).
        assert_eq!(
            counted(federation(&pair.sa), pair.sb_on_sa.clone()).await,
            (0, 0, 1, 0)
        );
        pair.stop().await;
    }

    /// What a paired server's own page says beyond its row: each library
    /// it shares as kept here, with its items and revision; the week's
    /// plays by the day; and a library it stopped sharing, until it shares
    /// it again.
    #[tokio::test]
    async fn a_pair_s_page_says_what_it_shares_its_week_and_what_it_withdrew() {
        let pair = Pair::start("fed-pair-page").await;
        let sb = federation(&pair.sb);
        let page = async || {
            let peer = sb.peer(&pair.sa_on_sb).await.unwrap().unwrap();
            sb.peer_page(&peer).await.unwrap()
        };
        pair.share("plex:1").await;
        pair.refresh().await;
        // Nothing changed since: the revision stands.
        pair.refresh().await;
        pair.sb.add_user("bob", "correct horse", None).await;
        let bob = pair.sb.store.user_by_name("bob").await.unwrap().unwrap().id;
        let device = iroh::SecretKey::from_bytes(&[3; 32]).public();
        sb.assert_play(bob, &pair.fed(movie().id()), device)
            .await
            .unwrap();
        let shown = page().await;
        assert_eq!(shown.received.len(), 1, "{shown:?}");
        let movies = &shown.received[0];
        assert_eq!((movies.library.as_str(), movies.revision), ("plex:1", 1));
        assert!(movies.items > 0 && movies.synced_at.is_some(), "{movies:?}");
        assert_eq!(shown.week.len(), 7);
        let today = shown.week[6];
        assert_eq!((today.there, today.here), (1, 0), "{:?}", shown.week);
        assert_eq!(shown.week[6].day - shown.week[0].day, 6 * 86_400);
        assert!(shown.withdrawn.is_empty());

        federation(&pair.sa)
            .unshare(&pair.sb_on_sa, "plex:1", "alice")
            .await
            .unwrap();
        pair.refresh().await;
        let shown = page().await;
        assert!(shown.received.is_empty(), "{shown:?}");
        assert_eq!(shown.withdrawn.len(), 1);
        assert_eq!(shown.withdrawn[0].0, "plex:1");
        pair.share("plex:1").await;
        pair.refresh().await;
        assert!(page().await.withdrawn.is_empty());
        pair.stop().await;
    }

    /// SB's own Alien (`plex:movie:500`).
    fn sb_alien() -> MediaId {
        id(MediaKind::Movie, "500")
    }

    /// SA and SB both with Alien, TMDB 348 and Plex's guid: SB's own from
    /// its Plex, SA's shared and cached on SB.
    async fn both_have_alien(test: &str) -> Pair {
        let pair = Pair::with_plex(test, alien_plex().await).await;
        pair.sa
            .catalog
            .give_ids(movie().id(), &["plex://movie/5d7768", "tmdb://348"]);
        pair.share("plex:1").await;
        pair.refresh().await;
        pair
    }

    /// What a search for `query` finds, as hits (P8-05).
    async fn hits(
        app: &TestClient,
        bob: &SessionId,
        query: &str,
    ) -> spoolframe_protocol::SearchResults<spoolframe_protocol::SearchHit> {
        match app
            .call(Some(bob), Request::Search(crate::support::search(query)))
            .await
        {
            Response::Ok(Reply::SearchHits(hits)) => hits,
            other => panic!("{other:?}"),
        }
    }

    /// Every hit of `results`, its top first.
    fn every_hit(
        results: &spoolframe_protocol::SearchResults<spoolframe_protocol::SearchHit>,
    ) -> Vec<&spoolframe_protocol::SearchHit> {
        let top = match &results.top {
            Some(spoolframe_protocol::TopResult::Item(hit)) => Some(hit),
            _ => None,
        };
        top.into_iter()
            .chain(results.groups.iter().flat_map(|g| &g.items))
            .collect()
    }

    /// An item's copies, as an app that names the feature is told them.
    async fn copies(
        app: &TestClient,
        bob: &SessionId,
        id: &MediaId,
    ) -> spoolframe_protocol::Copies {
        match app.call(Some(bob), Request::Item { id: id.clone() }).await {
            Response::Ok(Reply::Item {
                copies: Some(copies),
                ..
            }) => copies,
            other => panic!("{other:?}"),
        }
    }

    fn alices(pair: &Pair) -> spoolframe_types::LibraryServer {
        spoolframe_types::LibraryServer {
            server_id: pair.origin().to_owned(),
            name: "Alice's server".into(),
            online: true,
        }
    }

    /// P8-05: the film on both SB and SA (one TMDB id) is one hit, SB's,
    /// with "also on" SA, and so is its Item page. An app that does not
    /// name the feature sees what it saw before: SB's film alone, as plain
    /// results, and no copies.
    #[tokio::test]
    async fn a_film_on_sb_and_sa_is_one_hit_from_sb_also_on_sa() {
        let pair = both_have_alien("fed-merged").await;
        let sa_alien = pair.fed(movie().id());
        let (app, bob) = bob_on(&pair, true).await;
        let found = hits(&app, &bob, "alien").await;
        let every = every_hit(&found);
        assert_eq!(every.len(), 1, "{found:?}");
        let hit = every[0];
        assert_eq!(hit.item.id(), &sb_alien());
        assert_eq!(hit.origin, None);
        let sa = spoolframe_protocol::TitleCopy {
            id: sa_alien.clone(),
            server: Some(alices(&pair)),
        };
        assert_eq!(hit.also_on, std::slice::from_ref(&sa));
        assert_eq!(hit.play_from, sb_alien());
        let item = copies(&app, &bob, &sb_alien()).await;
        assert_eq!((item.origin, item.also_on), (None, vec![sa]));
        // SA's copy names SB's as its other.
        let theirs = copies(&app, &bob, &sa_alien).await;
        assert_eq!(theirs.origin, Some(alices(&pair)));
        assert_eq!(
            theirs.also_on,
            [spoolframe_protocol::TitleCopy {
                id: sb_alien(),
                server: None,
            }]
        );
        let (old, bob_old) = bob_on(&pair, false).await;
        assert_eq!(found_old(&old, &bob_old, "alien").await, [sb_alien()]);
        assert!(matches!(
            old.call(Some(&bob_old), Request::Item { id: sb_alien() })
                .await,
            Response::Ok(Reply::Item { copies: None, .. })
        ));
        pair.stop().await;
    }

    /// P8-05: SA's ids are its word only. Its Alien of 1979 says TMDB 348
    /// and Plex's guid, as SB's Alien of 2004 does: two hits, and neither
    /// Item page names the other as a copy.
    #[tokio::test]
    async fn a_paired_film_of_another_year_is_not_merged_whatever_its_ids() {
        let pair = Pair::with_plex("fed-merged-year", alien_plex_of(2004).await).await;
        pair.sa
            .catalog
            .give_ids(movie().id(), &["plex://movie/5d7768", "tmdb://348"]);
        pair.share("plex:1").await;
        pair.refresh().await;
        let sa_alien = pair.fed(movie().id());
        let (app, bob) = bob_on(&pair, true).await;
        let found = hits(&app, &bob, "alien").await;
        let every = every_hit(&found);
        assert_eq!(every.len(), 2, "{found:?}");
        assert!(every.iter().all(|hit| hit.also_on.is_empty()), "{found:?}");
        assert!(copies(&app, &bob, &sb_alien()).await.also_on.is_empty());
        assert!(copies(&app, &bob, &sa_alien).await.also_on.is_empty());
        pair.stop().await;
    }

    /// P8-05: Play from SA is remembered for Bob, by the title: SB's hit
    /// and either copy's Item page play SA's, in a new session too, until
    /// he chooses SB again. A server not paired is not one to play from.
    #[tokio::test]
    async fn play_from_sa_is_remembered_for_the_title() {
        let pair = both_have_alien("fed-play-from").await;
        let sa_alien = pair.fed(movie().id());
        let (app, bob) = bob_on(&pair, true).await;
        let play_from = |from: Option<&str>| Request::SetPlayFrom {
            title: sb_alien(),
            from: from.map(str::to_owned),
        };
        assert_eq!(
            app.call(Some(&bob), play_from(Some(pair.origin()))).await,
            Response::Ok(Reply::Done)
        );
        assert_eq!(copies(&app, &bob, &sb_alien()).await.play_from, sa_alien);
        let (again, bob_again) = bob_on(&pair, true).await;
        assert_eq!(
            every_hit(&hits(&again, &bob_again, "alien").await)[0].play_from,
            sa_alien
        );
        assert_eq!(
            copies(&again, &bob_again, &sa_alien).await.play_from,
            sa_alien
        );
        assert!(matches!(
            app.call(Some(&bob), play_from(Some("bbbbbbbbbbbbbbbb")))
                .await,
            Response::Err(Error::NotFound { .. })
        ));
        app.call(Some(&bob), play_from(None)).await;
        assert_eq!(copies(&app, &bob, &sa_alien).await.play_from, sb_alien());
        let (old, bob_old) = bob_on(&pair, false).await;
        assert!(matches!(
            old.call(Some(&bob_old), play_from(None)).await,
            Response::Err(Error::Unsupported { .. })
        ));
        pair.stop().await;
    }

    /// P8-05, progress per title (P8-03): after 40 minutes of SA's copy,
    /// the merged hit, SB's, shows 40 minutes.
    #[tokio::test]
    async fn after_40_minutes_of_sa_s_copy_the_merged_hit_shows_40_minutes() {
        let pair = both_have_alien("fed-merged-progress").await;
        let (app, bob) = bob_on(&pair, true).await;
        assert_eq!(
            app.call(
                Some(&bob),
                Request::UpdateWatchProgress {
                    id: pair.fed(movie().id()),
                    position_ms: 2_400_000,
                },
            )
            .await,
            Response::Ok(Reply::Done)
        );
        let found = hits(&app, &bob, "alien").await;
        let hit = every_hit(&found)[0];
        assert_eq!(hit.item.id(), &sb_alien());
        let Item::Movie(film) = &hit.item else {
            panic!("not a film");
        };
        assert_eq!(film.playback.position, Some(Duration::from_secs(2400)));
        pair.stop().await;
    }

    /// P8-03: Bob's app reports where he got to in SA's film to SB, by
    /// item. SB keeps it in his watch state, under the `fed:` id, and his
    /// play in its history; SA keeps nothing. Only a paired server's item
    /// is reported so, and only by an app that names the feature; once
    /// unpaired, the film is no longer available to report on.
    #[tokio::test]
    async fn progress_on_sa_s_film_is_kept_on_sb_alone() {
        let pair = Pair::start("fed-progress").await;
        pair.share("plex:1").await;
        pair.refresh().await;
        let alien = pair.fed(movie().id());
        let (app, bob) = bob_on(&pair, true).await;
        let report = |id: &MediaId, position_ms: u64| Request::UpdateWatchProgress {
            id: id.clone(),
            position_ms,
        };
        assert_eq!(
            app.call(Some(&bob), report(&alien, 2_400_000)).await,
            Response::Ok(Reply::Done)
        );
        let screen = home(&app, &bob).await;
        let Some(Item::Movie(film)) = screen.continue_watching.first() else {
            panic!("{screen:?}");
        };
        assert_eq!(
            (&film.id, film.playback.position),
            (&alien, Some(Duration::from_secs(2400)))
        );
        let bob_id = pair.sb.store.user_by_name("bob").await.unwrap().unwrap().id;
        let principal = spoolframe_server::principal::Principal::LocalUser(bob_id);
        // Its play's row is opened in the background.
        until(|| async { !pair.sb.store.all_plays(principal).await.unwrap().is_empty() }).await;
        let plays = pair.sb.store.all_plays(principal).await.unwrap();
        assert_eq!(plays.len(), 1);
        assert_eq!(plays[0].media_id, alien.to_string());
        assert!(
            pair.sa
                .store
                .in_progress_anyone(10)
                .await
                .unwrap()
                .is_empty(),
            "SA keeps no watch state for SB's users"
        );
        assert!(matches!(
            app.call(Some(&bob), report(movie().id(), 1)).await,
            Response::Err(Error::InvalidInput { .. })
        ));
        // Without the feature, a paired server's id is not one at all.
        let (old, bob_old) = bob_on(&pair, false).await;
        assert!(matches!(
            old.call(Some(&bob_old), report(&alien, 1)).await,
            Response::Err(Error::NotFound { .. })
        ));
        federation(&pair.sb)
            .unpair(&pair.sa_on_sb, "bob")
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            app.call(Some(&bob), report(&alien, 2_500_000)).await,
            Response::Err(Error::NotFound { .. })
        ));
        pair.stop().await;
    }

    /// P8-03: SA's film in Bob's playlist on SB: listed and playable while
    /// SA is paired, to an app that names the feature; kept and listed as
    /// no longer available once SA is unpaired. An app that does not name
    /// the feature lists none of it, and may not add it.
    #[tokio::test]
    async fn a_playlist_holds_sa_s_film_and_keeps_it_after_unpairing() {
        use spoolframe_protocol::playlists::{Kinds, PlaylistEdit, Visibility};
        let pair = Pair::start("fed-playlist").await;
        pair.share("plex:1").await;
        pair.refresh().await;
        let alien = pair.fed(movie().id());
        let (app, bob) = bob_on(&pair, true).await;
        let made = match app
            .call(
                Some(&bob),
                Request::CreatePlaylist {
                    name: "Space".into(),
                    kinds: Kinds {
                        movies: true,
                        episodes: false,
                        tracks: false,
                    },
                    visibility: Visibility::Personal,
                },
            )
            .await
        {
            Response::Ok(Reply::PlaylistInfo(made)) => made,
            other => panic!("{other:?}"),
        };
        async fn add(
            client: &TestClient,
            session: &SessionId,
            playlist: &str,
            id: &MediaId,
        ) -> Response {
            let edit = Request::EditPlaylist {
                id: playlist.into(),
                revision: 0,
                edits: vec![PlaylistEdit::Add {
                    id: id.clone(),
                    before: None,
                }],
            };
            client.call(Some(session), edit).await
        }
        async fn page(
            client: &TestClient,
            session: &SessionId,
            playlist: &str,
        ) -> spoolframe_protocol::playlists::PlaylistPage {
            let get = Request::GetPlaylist {
                id: playlist.into(),
                offset: 0,
                limit: 10,
            };
            match client.call(Some(session), get).await {
                Response::Ok(Reply::Playlist(page)) => page,
                other => panic!("{other:?}"),
            }
        }
        let (old, bob_old) = bob_on(&pair, false).await;
        let refused = add(&old, &bob_old, &made.id, &alien).await;
        assert!(
            matches!(refused, Response::Err(Error::NotFound { .. })),
            "{refused:?}"
        );
        let added = add(&app, &bob, &made.id, &alien).await;
        assert!(
            matches!(added, Response::Ok(Reply::PlaylistEdited { .. })),
            "{added:?}"
        );
        let listed = page(&app, &bob, &made.id).await;
        assert_eq!(listed.entries.len(), 1);
        assert_eq!(listed.entries[0].item.id(), &alien);
        assert!(!listed.entries[0].unavailable);
        let unseen = page(&old, &bob_old, &made.id).await;
        assert!(unseen.entries.is_empty() && unseen.total == 0, "{unseen:?}");
        federation(&pair.sb)
            .unpair(&pair.sa_on_sb, "bob")
            .await
            .unwrap()
            .unwrap();
        let kept = page(&app, &bob, &made.id).await;
        assert_eq!(kept.entries.len(), 1);
        assert_eq!(kept.entries[0].item.id(), &alien);
        assert!(kept.entries[0].unavailable);
        assert_eq!(kept.playable, 0);
        pair.stop().await;
    }

    /// P8-03: Bob's play of SA's film stays in his Recently played on SB
    /// once SA is unpaired, listed as no longer available, to an app that
    /// names the feature.
    #[tokio::test]
    async fn recently_played_keeps_sa_s_film_after_unpairing() {
        use spoolframe_server::store::plays::{NewPlay, PlayUpdate, PlayedKind};
        let pair = Pair::start("fed-history").await;
        pair.share("plex:1").await;
        pair.refresh().await;
        let alien = pair.fed(movie().id());
        pair.sb.add_user("bob", "correct horse", None).await;
        let bob_id = pair.sb.store.user_by_name("bob").await.unwrap().unwrap().id;
        let principal = spoolframe_server::principal::Principal::LocalUser(bob_id);
        let media_id = alien.to_string();
        let play = pair
            .sb
            .store
            .open_play(
                principal,
                NewPlay {
                    media_id: &media_id,
                    kind: PlayedKind::Movie,
                    show_id: None,
                    album_id: None,
                    artist_id: None,
                    external_ids: &[],
                    duration_ms: Some(7_020_000),
                    endpoint_id: None,
                    device_name: None,
                    playback_id: None,
                },
            )
            .await
            .unwrap();
        pair.sb
            .store
            .update_play(
                play,
                PlayUpdate {
                    played_ms: 7_000_000,
                    counted: true,
                    over_http: false,
                    close: true,
                },
            )
            .await
            .unwrap();
        federation(&pair.sb)
            .unpair(&pair.sa_on_sb, "bob")
            .await
            .unwrap()
            .unwrap();
        let app = pair.sb.client().await;
        let hello = spoolframe_protocol::Hello {
            protocol_version: spoolframe_protocol::PROTOCOL_VERSION,
            client_name: "test".into(),
            client_version: "0".into(),
            platform: spoolframe_types::Platform::Linux,
            device_name: None,
            capabilities: None,
            experimental: vec![spoolframe_protocol::experimental::FEDERATION.into()],
        };
        app.call(None, Request::Hello(hello)).await;
        let bob = app.login("bob", "correct horse").await;
        let played = app
            .call(
                Some(&bob),
                Request::RecentlyPlayed {
                    kinds: None,
                    offset: 0,
                    limit: 10,
                },
            )
            .await;
        let Response::Ok(Reply::Items {
            items,
            total,
            unavailable,
        }) = played
        else {
            panic!("{played:?}");
        };
        assert_eq!(ids(&items), std::slice::from_ref(&alien));
        assert_eq!(total, 1);
        assert_eq!(unavailable, [alien]);
        pair.stop().await;
    }

    /// The SDK's side of P8-02 and P8-03: SA's admin shares a library
    /// from an app; Bob's app on SB lists it with its server, turns SA's
    /// Home row off, reports progress on SA's film to SB, and is told when
    /// the libraries change.
    #[tokio::test]
    async fn the_sdk_shares_lists_and_reports_progress() {
        use super::app::{FEDERATION, PASSWORD, signed_in};
        let pair = Pair::start("fed-sdk").await;
        pair.sa.add_user("alice", PASSWORD, None).await;
        pair.sa
            .store
            .set_role("alice", spoolframe_types::Role::Admin)
            .await
            .unwrap();
        pair.sb.add_user("bob", PASSWORD, None).await;
        let (_on_sa, alice) = signed_in(&pair.sa, "alice", FEDERATION).await;
        let library: LibraryId = "plex:1".parse().unwrap();
        assert_eq!(
            alice
                .share_libraries(pair.sb.server_id.as_str(), std::slice::from_ref(&library))
                .await
                .unwrap(),
            [library]
        );
        pair.refresh().await;
        let (phone, bob) = signed_in(&pair.sb, "bob", FEDERATION).await;
        let mut told = phone.libraries_events();
        let listed = bob.libraries().await.unwrap();
        assert_eq!(listed.len(), 1);
        let server = listed[0]
            .server
            .clone()
            .expect("SA's library has its server");
        assert_eq!(
            (
                server.server_id.as_str(),
                server.name.as_str(),
                server.online
            ),
            (pair.origin(), "Alice's server", true)
        );
        bob.set_server_row(pair.origin(), false).await.unwrap();
        assert!(!bob.home().await.unwrap().servers[0].shown);
        let alien = pair.fed(movie().id());
        bob.update_watch_progress(&alien, Duration::from_secs(600))
            .await
            .unwrap();
        let screen = bob.home().await.unwrap();
        assert_eq!(ids(&screen.continue_watching), std::slice::from_ref(&alien));
        assert!(matches!(
            bob.update_watch_progress(movie().id(), Duration::from_secs(1))
                .await,
            Err(spoolframe_client_sdk::SdkError::InvalidInput(_))
        ));
        federation(&pair.sa)
            .unshare(&pair.sb_on_sa, "plex:1", "alice")
            .await
            .unwrap();
        pair.refresh().await;
        let event = tokio::time::timeout(Duration::from_secs(10), told.recv())
            .await
            .expect("told within 10 s")
            .unwrap();
        assert_eq!(event.server, pair.sb.server.id().to_string());
        assert!(bob.libraries().await.unwrap().is_empty());
        pair.stop().await;
    }

    /// P8-05 in the SDK: Bob's app finds the film on SB and SA as one hit,
    /// chooses to play it from SA, and the item's copies say so; its plain
    /// search gives the hits' items.
    #[tokio::test]
    async fn the_sdk_finds_one_hit_and_plays_it_from_sa() {
        use super::app::{FEDERATION, PASSWORD, signed_in};
        let pair = both_have_alien("fed-sdk-merged").await;
        pair.sb.add_user("bob", PASSWORD, None).await;
        let (_phone, bob) = signed_in(&pair.sb, "bob", FEDERATION).await;
        let found = bob
            .search_hits(crate::support::search("alien"))
            .await
            .unwrap();
        let every = every_hit(&found);
        assert_eq!(every.len(), 1);
        let sa_alien = pair.fed(movie().id());
        assert_eq!(every[0].also_on[0].id, sa_alien);
        bob.set_play_from(&sb_alien(), Some(pair.origin()))
            .await
            .unwrap();
        let detail = bob.item(&sb_alien()).await.unwrap();
        assert_eq!(detail.copies.map(|c| c.play_from), Some(sa_alien));
        let plain = bob.search(crate::support::search("alien")).await.unwrap();
        assert!(matches!(
            plain.top,
            Some(spoolframe_protocol::TopResult::Item(ref item)) if item.id() == &sb_alien()
        ));
        pair.stop().await;
    }

    /// Unpairing deletes the pair's tokens on SA; one past its expiry is
    /// pruned.
    #[tokio::test]
    async fn unpairing_and_expiry_end_sa_s_tokens() {
        use spoolframe_server::auth::SessionToken;
        use spoolframe_server::federation::PLAY_TOKEN_LIFETIME;
        let pair = Pair::start("fed-assert-end").await;
        pair.share("plex:1").await;
        pair.refresh().await;
        pair.sb.add_user("bob", "correct horse", None).await;
        let bob = pair.sb.store.user_by_name("bob").await.unwrap().unwrap().id;
        let device = iroh::SecretKey::from_bytes(&[3; 32]).public();
        let alien = pair.fed(movie().id());
        let play = || async {
            let granted = federation(&pair.sb)
                .assert_play(bob, &alien, device)
                .await
                .unwrap();
            SessionToken(granted.token.clone()).hash()
        };
        let first = play().await;
        pair.sa
            .store
            .clock()
            .advance(PLAY_TOKEN_LIFETIME + std::time::Duration::from_secs(1));
        assert_eq!(pair.sa.store.prune_federation_tokens().await.unwrap(), 1);
        assert_eq!(pair.sa.store.federation_token(&first).await.unwrap(), None);
        let second = play().await;
        assert!(
            pair.sa
                .store
                .federation_token(&second)
                .await
                .unwrap()
                .is_some()
        );
        federation(&pair.sa)
            .unpair(&pair.sb_on_sa, "alice")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pair.sa.store.federation_token(&second).await.unwrap(), None);
        pair.stop().await;
    }

    /// Unpairing on SB keeps only the film a user of SB watched, flagged
    /// unavailable and without artwork: not browsed, but still known for
    /// Continue watching. Everything else of SA's goes.
    #[tokio::test]
    async fn unpairing_keeps_only_what_a_user_holds_and_flags_it() {
        let pair = Pair::start("fed-replica-unpair").await;
        pair.sa.catalog.show_posters();
        pair.sa.catalog.serve_music();
        pair.share("plex:1").await;
        pair.share("plex:2").await;
        pair.refresh().await;
        let alien = pair.fed(movie().id());
        let artist = pair.fed(&id(MediaKind::Artist, "30"));
        pair.sb.add_user("bob", "correct horse", None).await;
        let bob = pair.sb.store.user_by_name("bob").await.unwrap().unwrap().id;
        pair.sb
            .store
            .save_watch(
                spoolframe_server::principal::Principal::LocalUser(bob),
                WatchUpdate {
                    media_id: &alien.to_string(),
                    show_id: None,
                    external_ids: &[],
                    position_ms: Some(2_400_000),
                    duration_ms: Some(7_020_000),
                    watched: false,
                    unhide: true,
                },
            )
            .await
            .unwrap();
        federation(&pair.sb)
            .unpair(&pair.sa_on_sb, "bob")
            .await
            .unwrap()
            .unwrap();
        assert!(!federation(&pair.sb).online(pair.origin()));
        assert!(matches!(
            pair.cache.item(&alien).await,
            Err(CatalogError::NotServed(_))
        ));
        let (kept, reach) = pair.cache.item_kept(&alien).await.unwrap();
        assert_eq!(
            (title(&kept.item), reach),
            ("Alien".to_owned(), spoolframe_catalog::Reach::Gone)
        );
        assert!(matches!(
            pair.cache.item_kept(&artist).await,
            Err(CatalogError::NotFound(_))
        ));
        assert!(
            pair.cache
                .authorize_artwork(&pair.fed_artwork(&poster_of("1")))
                .await
                .is_err()
        );
        assert!(
            pair.sb
                .store
                .cached_libraries(None)
                .await
                .unwrap()
                .is_empty()
        );
        pair.stop().await;
    }

    /// SB syncs on connecting, without being asked: started again after
    /// SA shared a library, it has it soon after.
    #[tokio::test]
    async fn sb_syncs_on_connecting() {
        let pair = Pair::start("fed-replica-connect").await;
        pair.share("plex:1").await;
        let Pair { sa, sb, .. } = pair;
        let dir = sb.dir.clone();
        sb.stop().await;
        let (sb, cache) = sb_in(dir).await;
        let alien = MediaId::federated(sa.server_id.as_str(), movie().id()).unwrap();
        until(|| async { cache.item(&alien).await.is_ok() }).await;
        assert!(federation(&sb).online(sa.server_id.as_str()));
        sa.stop().await;
        sb.stop().await;
    }

    /// The admin's Refresh: an admin syncs a pair now; another id is no
    /// pair.
    #[tokio::test]
    async fn the_admin_refresh_syncs_now() {
        let pair = Pair::start("fed-replica-refresh").await;
        pair.share("plex:1").await;
        assert!(matches!(
            federation(&pair.sb).refresh("no-such").await,
            Err(spoolframe_server::federation::ReplicaError::NoSuchPeer(_))
        ));
        pair.refresh().await;
        assert_eq!(pair.titles("plex:1").await.unwrap(), ["Alien"]);
        pair.stop().await;
    }

    impl Pair {
        /// SA shares Movies and SB synced it; each holds its connection
        /// to the other, and SB's own sync on connecting is over: from
        /// here SB syncs again only when it is told to, or in an hour.
        async fn settled(&self) {
            self.share("plex:1").await;
            self.refresh().await;
            let sb = self.sb.server_id.as_str();
            // SA dials SB at its replica's next tick: woken, at once.
            federation(&self.sa).set_replica(
                std::time::Duration::from_secs(3600),
                spoolframe_server::store::federation_cache::FileLimits {
                    per_origin: 1 << 30,
                    total: 1 << 30,
                },
            );
            until(|| async {
                federation(&self.sa).online(sb) && federation(&self.sb).online(self.origin())
            })
            .await;
            // The replica's sync on connecting, a few milliseconds after
            // the pairing: a second is far past it.
            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        /// When SB last synced SA's `library` whole, by SB's clock; `None`
        /// while it is not cached.
        async fn synced_at(&self, library: &str) -> Option<i64> {
            self.sb
                .store
                .cached_libraries(Some(self.origin()))
                .await
                .unwrap()
                .into_iter()
                .find(|l| l.library == library)
                .and_then(|l| l.synced_at)
        }

        /// SB's clock moved on by `by`.
        fn later(&self, by: Duration) {
            self.sb.clock.advance(by);
        }
    }

    /// A grant added on SA reaches SB at once: SA tells SB on the
    /// connection it holds, and SB syncs SA without waiting for its hour
    /// or a Refresh. A grant revoked takes the library away as soon.
    #[tokio::test]
    async fn a_grant_added_or_revoked_reaches_the_paired_server_at_once() {
        let pair = Pair::start("fed-replica-grant-notice").await;
        pair.sa.catalog.serve_music();
        pair.settled().await;
        assert_eq!(pair.synced_at("plex:2").await, None);

        pair.share("plex:2").await;
        until(|| async { pair.synced_at("plex:2").await.is_some() }).await;

        // Past the least time between two syncs a notice sets off.
        pair.later(SHARES_CHANGED_EVERY + Duration::from_secs(1));
        federation(&pair.sa)
            .unshare(&pair.sb_on_sa, "plex:2", "alice")
            .await
            .unwrap()
            .unwrap();
        until(|| async { pair.synced_at("plex:2").await.is_none() }).await;
        assert!(pair.synced_at("plex:1").await.is_some());
        pair.stop().await;
    }

    /// Notices are coalesced: one sets off a sync at once, those within
    /// [`SHARES_CHANGED_EVERY`] after it wait for its end and are one
    /// sync then, however many came.
    #[tokio::test]
    async fn notices_of_grants_changed_are_coalesced() {
        let pair = Pair::start("fed-replica-grant-notices").await;
        pair.settled().await;
        let first = pair.synced_at("plex:1").await.unwrap();

        // Shared again, the grant is kept, and SB is told all the same.
        pair.later(Duration::from_secs(2));
        pair.share("plex:1").await;
        until(|| async { pair.synced_at("plex:1").await > Some(first) }).await;
        let second = pair.synced_at("plex:1").await.unwrap();

        pair.later(Duration::from_secs(2));
        for _ in 0..3 {
            pair.share("plex:1").await;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(pair.synced_at("plex:1").await, Some(second), "not yet");

        pair.later(SHARES_CHANGED_EVERY);
        pair.wake_replica();
        until(|| async { pair.synced_at("plex:1").await > Some(second) }).await;
        let third = pair.synced_at("plex:1").await.unwrap();

        // The three were one sync: none follows.
        pair.later(SHARES_CHANGED_EVERY + Duration::from_secs(1));
        pair.wake_replica();
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(pair.synced_at("plex:1").await, Some(third));
        pair.stop().await;
    }

    /// F8-10: MCP never names the feature, so a paired server's item is
    /// not in its library, by its `fed:` id either: `get_item` and
    /// `set_watched` of it are refused as of an item that is not there,
    /// though the cache has the item.
    #[tokio::test]
    async fn mcp_reaches_no_paired_server_s_item_by_its_id() {
        use crate::mcp::{Mcp, serve_beside, user};
        let pair = Pair::start("fed-replica-mcp").await;
        pair.share("plex:1").await;
        pair.refresh().await;
        let alien = pair.fed(movie().id());
        assert!(pair.cache.item(&alien).await.is_ok(), "cached");
        let addr = serve_beside(&pair.sb, pair.cache.clone()).await;
        user(&pair.sb, "bob", spoolframe_types::Role::Consumer).await;
        let mcp = Mcp::connect(addr, "bob", &["library:read", "playlists:write"]).await;
        let id = alien.to_string();
        let got = mcp.call("get_item", serde_json::json!({ "id": id })).await;
        assert!(
            got.as_ref().unwrap_err().contains("not in the library"),
            "{got:?}"
        );
        let marked = mcp
            .call(
                "set_watched",
                serde_json::json!({ "id": id, "watched": true }),
            )
            .await;
        assert!(
            marked.as_ref().unwrap_err().contains("not in the library"),
            "{marked:?}"
        );
        pair.stop().await;
    }

    /// The media path (P8-04).
    mod media {
        use spoolframe_protocol::federation::{PlayTokenRedeemed, RedeemPlayToken};
        use spoolframe_protocol::{
            ArtworkSize, MediaReadResponse, PlaybackHandle, Request, Response, Selection,
        };
        use spoolframe_server::store::federation_tokens::PlaySide;
        use spoolframe_server::store::users::LocalUserId;

        use super::*;
        use crate::support::{TestClient, mkv_capabilities};

        const FEDERATION: &str = spoolframe_protocol::experimental::FEDERATION;

        /// SA's film shared with SB, synced, its file written on SA, and
        /// Bob a user of SB's.
        async fn shared(pair: &Pair) -> (Vec<u8>, LocalUserId) {
            pair.share("plex:1").await;
            pair.refresh().await;
            let file = pair.sa.write_media(1 << 20);
            pair.sb.add_user("bob", "correct horse", None).await;
            let bob = pair.sb.store.user_by_name("bob").await.unwrap().unwrap().id;
            (file, bob)
        }

        /// A device of Bob's on SA, naming the feature.
        async fn device(pair: &Pair) -> TestClient {
            let device = pair.sa.client().await;
            device.hello_naming(mkv_capabilities(), &[FEDERATION]).await;
            device
        }

        /// SB vouches for Bob on `device`: SA's token for its film.
        async fn token(pair: &Pair, bob: LocalUserId, device: &TestClient) -> String {
            federation(&pair.sb)
                .assert_play(bob, &pair.fed(movie().id()), device.endpoint.id())
                .await
                .unwrap()
                .token
        }

        async fn redeem(device: &TestClient, token: &str) -> Response {
            device
                .call(
                    None,
                    Request::RedeemPlayToken(RedeemPlayToken {
                        token: token.to_owned(),
                    }),
                )
                .await
        }

        async fn redeemed(device: &TestClient, token: &str) -> PlayTokenRedeemed {
            match redeem(device, token).await {
                Response::Ok(Reply::PlayTokenRedeemed(redeemed)) => redeemed,
                other => panic!("not redeemed: {other:?}"),
            }
        }

        fn start(id: &MediaId) -> Request {
            Request::StartPlayback {
                id: id.clone(),
                selection: Some(Selection::default()),
                start_ms: None,
                bitmap: Default::default(),
                quality: Default::default(),
                display: None,
                capabilities: None,
                onward_share: None,
            }
        }

        async fn started(device: &TestClient, session: &SessionId) -> PlaybackHandle {
            match device.call(Some(session), start(movie().id())).await {
                Response::Ok(Reply::PlaybackStarted { handle, .. }) => handle,
                other => panic!("did not start: {other:?}"),
            }
        }

        fn unauthenticated(response: Response) -> bool {
            matches!(
                response,
                Response::Err(spoolframe_protocol::Error::Unauthenticated)
            )
        }

        /// Bob's device redeems SA's token on SA itself and plays SA's
        /// film from there: the bytes are the file's, SA counts a direct
        /// play, and keeps no watch state for Bob. The token works once.
        #[tokio::test]
        async fn bob_s_device_redeems_sa_s_token_and_plays_the_film() {
            let pair = Pair::start("fed-media-direct").await;
            let (file, bob) = shared(&pair).await;
            let device = device(&pair).await;
            let token = token(&pair, bob, &device).await;
            let opened = redeemed(&device, &token).await;
            assert_eq!(&opened.item, movie().id());
            let handle = started(&device, &opened.session).await;
            let (read, bytes) = device.read(&opened.session, &handle, 0, 4096).await;
            assert!(matches!(read, MediaReadResponse::Ok(_)), "{read:?}");
            assert_eq!(bytes, file[..4096]);
            let progress = Request::UpdatePlaybackProgress {
                handle: handle.clone(),
                sequence: 1,
                position_ms: 120_000,
            };
            assert!(matches!(
                device.call(Some(&opened.session), progress).await,
                Response::Ok(Reply::Done)
            ));
            let principal = format!("remote:{}:{bob}", pair.sb.server_id)
                .parse()
                .unwrap();
            let states = pair
                .sa
                .store
                .watch_states(principal, &[movie().id().to_string()])
                .await
                .unwrap();
            assert!(states.is_empty(), "SA keeps nothing for Bob: {states:?}");
            // Counted off the session's way, once it is written.
            until(|| async {
                let served = pair
                    .sa
                    .store
                    .federation_plays(&pair.sb_on_sa, PlaySide::Served)
                    .await
                    .unwrap();
                (served.plays, served.direct(), served.proxied) == (1, 1, 0)
            })
            .await;
            assert!(unauthenticated(redeem(&device, &token).await), "used");
            pair.stop().await;
        }

        /// The session reaches its film's playback alone: not the
        /// libraries, nor the item's page, nor another item, nor artwork.
        /// A token works only from the device it was minted for.
        #[tokio::test]
        async fn a_play_token_opens_its_film_s_playback_and_nothing_else() {
            let pair = Pair::start("fed-media-scope").await;
            let (_, bob) = shared(&pair).await;
            let (mine, other) = (device(&pair).await, device(&pair).await);
            let token = token(&pair, bob, &mine).await;
            assert!(
                unauthenticated(redeem(&other, &token).await),
                "another device"
            );
            let session = redeemed(&mine, &token).await.session;
            let episode = MediaId::new(Namespace::Plex, MediaKind::Episode, "12").unwrap();
            for request in [
                Request::Libraries,
                Request::Item {
                    id: movie().id().clone(),
                },
                Request::Home,
                start(&episode),
            ] {
                let name = format!("{request:?}");
                assert!(
                    unauthenticated(mine.call(Some(&session), request).await),
                    "{name}"
                );
            }
            let (artwork, _) = mine
                .artwork(&session, poster_of("1"), ArtworkSize::Thumb)
                .await;
            assert!(
                matches!(
                    artwork,
                    spoolframe_protocol::ArtworkResponse::Err(
                        spoolframe_protocol::Error::Unauthenticated
                    )
                ),
                "{artwork:?}"
            );
            started(&mine, &session).await;
            pair.stop().await;
        }

        /// SA's PMS moving the film to a library not shared with SB, with
        /// no catalog change SA hears of, still ends the session as it
        /// next starts a playback (F8-04).
        #[tokio::test]
        async fn a_session_whose_film_moved_starts_no_playback() {
            let pair = Pair::start("fed-media-moved").await;
            let (_, bob) = shared(&pair).await;
            let device = device(&pair).await;
            let token = token(&pair, bob, &device).await;
            let session = redeemed(&device, &token).await.session;
            pair.sa.catalog.move_to(
                movie().id(),
                spoolframe_server::test_support::music_library().id,
            );
            assert!(unauthenticated(
                device.call(Some(&session), start(movie().id())).await
            ));
            assert_eq!(pair.sa.auth.remote_sessions().count(|_| true), 0);
            pair.stop().await;
        }

        /// The audit's C8-03: the moved film's play session, used from a
        /// connection whose hello does not name the feature, starts no
        /// playback either, and a download token's session prepares no
        /// download of it: SA checks where the film is itself, whatever
        /// the client names.
        #[tokio::test]
        async fn a_moved_film_s_session_does_nothing_from_a_hello_without_the_feature() {
            use spoolframe_protocol::downloads::DownloadSource;
            use spoolframe_protocol::downloads::DownloadTracks;
            use spoolframe_protocol::federation::TokenScope;
            use spoolframe_server::principal::Who;
            use spoolframe_server::store::sessions::SessionUser;
            let pair = Pair::start("fed-media-moved-plain").await;
            let (_, bob) = shared(&pair).await;
            let device = device(&pair).await;
            let play = redeemed(&device, &token(&pair, bob, &device).await)
                .await
                .session;
            let sb = federation(&pair.sb);
            let sa = sb.peer(&pair.sa_on_sb).await.unwrap().unwrap();
            let who = Who::Local(SessionUser {
                user: bob,
                username: "bob".into(),
                display_name: "bob".into(),
                role: spoolframe_types::Role::Consumer,
                pin_required: false,
                authenticated_at: 0,
                signed_in_with: None,
                password_change_only: false,
            });
            let granted = sb
                .assert_download_for(
                    &who,
                    &sa,
                    &pair.fed(movie().id()),
                    device.endpoint.id(),
                    None,
                )
                .await
                .unwrap();
            let download = redeemed(&device, &granted.token).await;
            assert_eq!(download.scope, TokenScope::Download);
            pair.sa.catalog.move_to(
                movie().id(),
                spoolframe_server::test_support::music_library().id,
            );
            let plain = pair.sa.client_from(&device.endpoint).await;
            plain.hello_naming(mkv_capabilities(), &[]).await;
            assert!(unauthenticated(
                plain.call(Some(&play), start(movie().id())).await
            ));
            let prepare = Request::PrepareDownload {
                id: movie().id().clone(),
                source: DownloadSource::Original,
                tracks: DownloadTracks::All,
                device: None,
            };
            assert!(unauthenticated(
                plain.call(Some(&download.session), prepare).await
            ));
            assert_eq!(pair.sa.auth.remote_sessions().count(|_| true), 0);
            assert!(pair.sa.store.downloads().await.unwrap().is_empty());
            pair.stop().await;
        }

        /// The audit's X8-03: a play session of a film moved to a library
        /// not shared with SB changes none of the film's subtitle timing,
        /// which is served to everyone, and ends; from a hello naming the
        /// feature and from one that does not. The timing request is the
        /// session's first after the move: a start first would end the
        /// session by itself (F8-04) and prove nothing. A real film with a
        /// real subtitle file beside it, so that with the check missing
        /// the offset is set and the timing row written.
        #[tokio::test]
        async fn a_moved_film_s_session_changes_no_subtitle_timing() {
            let pair = Pair::start_both(
                "fed-media-moved-subtitles",
                ServerOptions {
                    real_media: true,
                    ..ServerOptions::default()
                },
                ServerOptions::default(),
            )
            .await;
            pair.share("plex:1").await;
            pair.refresh().await;
            pair.sa.generate_media(4, "libx264", "aac");
            let sidecar = pair.sa.media.with_extension("en.srt");
            let text = "1\n00:00:01,000 --> 00:00:02,000\nA line\n\n";
            std::fs::write(&sidecar, text).unwrap();
            pair.sb.add_user("bob", "correct horse", None).await;
            let bob = pair.sb.store.user_by_name("bob").await.unwrap().unwrap().id;
            let hello = |experimental: &[&str]| spoolframe_protocol::Hello {
                protocol_version: spoolframe_protocol::PROTOCOL_VERSION,
                client_name: "test".into(),
                client_version: "0".into(),
                platform: spoolframe_types::Platform::Linux,
                device_name: None,
                capabilities: Some(mkv_capabilities()),
                experimental: experimental.iter().map(|n| (*n).to_owned()).collect(),
            };
            let timing = [
                (
                    "sync",
                    Request::SyncSubtitle {
                        id: movie().id().clone(),
                        track: 0,
                    },
                ),
                (
                    "original",
                    Request::UseOriginalSubtitleTiming {
                        id: movie().id().clone(),
                        track: 0,
                    },
                ),
                (
                    "offset",
                    Request::SetSubtitleOffset {
                        id: movie().id().clone(),
                        track: 0,
                        offset_ms: 1_500,
                    },
                ),
            ];
            // One session for each request, each redeemed before the move
            // (a token for a moved film opens nothing), from a device
            // naming the feature; the plain connection is the same
            // device's, saying hello without it.
            let mut sessions = Vec::new();
            for (name, request) in timing {
                for federated in [true, false] {
                    let device = pair.sa.client().await;
                    device.hello_naming(mkv_capabilities(), &[FEDERATION]).await;
                    let session = redeemed(&device, &token(&pair, bob, &device).await)
                        .await
                        .session;
                    let asking = pair.sa.client_from(&device.endpoint).await;
                    let named: &[&str] = if federated { &[FEDERATION] } else { &[] };
                    assert!(matches!(
                        asking.call(None, Request::Hello(hello(named))).await,
                        Response::Ok(Reply::ServerInfo(_))
                    ));
                    sessions.push((
                        format!("{name}, federated {federated}"),
                        asking,
                        session,
                        request.clone(),
                    ));
                }
            }
            pair.sa.catalog.move_to(
                movie().id(),
                spoolframe_server::test_support::music_library().id,
            );
            for (name, asking, session, request) in sessions {
                let response = asking.call(Some(&session), request).await;
                let refused = matches!(
                    response,
                    Response::Err(spoolframe_protocol::Error::Unauthenticated)
                );
                assert!(refused, "{name}: {response:?}");
            }
            assert_eq!(pair.sa.auth.remote_sessions().count(|_| true), 0);
            assert_eq!(std::fs::read_to_string(&sidecar).unwrap(), text);
            assert!(
                pair.sa
                    .store
                    .subtitle_timing(sidecar.to_str().unwrap())
                    .await
                    .unwrap()
                    .is_none(),
                "no timing row"
            );
            pair.stop().await;
        }

        /// SA's film, real, with a subtitle file beside it, shared with SB
        /// and synced; Bob a user of SB's. The file's path and text.
        async fn subtitled_film(pair: &Pair) -> (std::path::PathBuf, &'static str, LocalUserId) {
            pair.share("plex:1").await;
            pair.refresh().await;
            pair.sa.generate_media(4, "libx264", "aac");
            let sidecar = pair.sa.media.with_extension("en.srt");
            let text = "1\n00:00:01,000 --> 00:00:02,000\nA line\n\n";
            std::fs::write(&sidecar, text).unwrap();
            pair.sb.add_user("bob", "correct horse", None).await;
            let bob = pair.sb.store.user_by_name("bob").await.unwrap().unwrap().id;
            (sidecar, text, bob)
        }

        /// A new connection from `device`'s endpoint, let in by its remote
        /// session, saying hello with the feature and subtitle sync.
        async fn timing_connection(pair: &Pair, device: &TestClient) -> TestClient {
            let asking = pair.sa.client_from(&device.endpoint).await;
            let hello = spoolframe_protocol::Hello {
                protocol_version: spoolframe_protocol::PROTOCOL_VERSION,
                client_name: "test".into(),
                client_version: "0".into(),
                platform: spoolframe_types::Platform::Linux,
                device_name: None,
                capabilities: Some(mkv_capabilities()),
                experimental: vec![FEDERATION.into()],
            };
            assert!(matches!(
                asking.call(None, Request::Hello(hello)).await,
                Response::Ok(Reply::ServerInfo(_))
            ));
            asking
        }

        /// `request` under `session` on a stream of `client`'s, its reply
        /// left unread: the stream to read it from later.
        async fn ask_unread(
            client: &TestClient,
            session: &SessionId,
            request: Request,
        ) -> iroh::endpoint::RecvStream {
            use spoolframe_protocol::codec::write_frame;
            use spoolframe_protocol::streams::bi;
            use tokio::io::AsyncWriteExt as _;
            let (mut send, recv) = client.conn.open_bi().await.unwrap();
            send.write_u8(bi::RPC).await.unwrap();
            write_frame(
                &mut send,
                &spoolframe_protocol::rpc::RpcRequest {
                    envelope: spoolframe_protocol::RequestEnvelope {
                        session: Some(session.clone()),
                        request,
                    },
                    operation: Some(client.operation()),
                    expected_revision: None,
                },
            )
            .await
            .unwrap();
            send.finish().unwrap();
            recv
        }

        /// Whether `recv` was reset `BUSY`: past the requests of paired
        /// servers' sessions answered at once (Y8-02).
        async fn reset_busy(mut recv: iroh::endpoint::RecvStream) -> bool {
            use iroh::endpoint::{ReadError, ReadToEndError, VarInt};
            matches!(
                recv.read_to_end(1 << 20).await,
                Err(ReadToEndError::Read(ReadError::Reset(code)))
                    if code == VarInt::from_u32(spoolframe_protocol::streams::reset::BUSY)
            )
        }

        fn offset() -> Request {
            Request::SetSubtitleOffset {
                id: movie().id().clone(),
                track: 0,
                offset_ms: 1_500,
            }
        }

        /// Until `n` requests of paired servers' sessions are in hand on
        /// SA, within 5 s: a hello's stream holds its place until the
        /// client has taken the reply, a moment after the client read it.
        async fn in_hand(pair: &Pair, n: usize) {
            let mut in_hand = pair.sa.server.connections().remote_requests_in_hand();
            tokio::time::timeout(Duration::from_secs(5), in_hand.wait_for(|at| *at == n))
                .await
                .expect("in time")
                .expect("the server is up");
        }

        /// The third Codex audit's Y8-02, as reviewed: a remote session's
        /// request runs to its end, whole, though its connection closes,
        /// holding its place meanwhile. Timing changes held at the writer
        /// from connections since closed keep the pair's places, so closing
        /// and connecting again admits no more past the budget; let go,
        /// each runs to its end once. A local user is answered meanwhile.
        #[tokio::test]
        async fn a_remote_session_s_abandoned_requests_hold_their_places_to_their_end() {
            let pair = Pair::start_both(
                "fed-media-remote-requests-end",
                ServerOptions {
                    real_media: true,
                    admission: Admission {
                        max_pair_remote_connections: 5,
                        remote_requests_each: 1,
                        ..Admission::default()
                    },
                    ..ServerOptions::default()
                },
                ServerOptions::default(),
            )
            .await;
            let (sidecar, _, bob) = subtitled_film(&pair).await;
            let device = device(&pair).await;
            let session = redeemed(&device, &token(&pair, bob, &device).await)
                .await
                .session;
            let held = &pair.sa.subtitle_timings.held;
            held.hold();
            // Four of the pair's budget of five, over two connections.
            let connections = [
                timing_connection(&pair, &device).await,
                timing_connection(&pair, &device).await,
            ];
            let mut streams = Vec::new();
            for connection in &connections {
                for _ in 0..2 {
                    streams.push(ask_unread(connection, &session, offset()).await);
                }
            }
            held.until_at_it(4).await;
            for connection in &connections {
                connection.conn.close(0u32.into(), b"done");
            }
            // Closed, the four stay at the writer with their places: the
            // next connection's hello takes the fifth and gives it back, a
            // fifth change takes it for good, and a sixth is past the budget.
            let again = timing_connection(&pair, &device).await;
            in_hand(&pair, 4).await;
            held.until_at_it(4).await;
            streams.push(ask_unread(&again, &session, offset()).await);
            held.until_at_it(5).await;
            assert!(
                reset_busy(ask_unread(&again, &session, offset()).await).await,
                "the abandoned changes hold the pair's places"
            );
            pair.sa.add_user("carol", "correct horse", None).await;
            let local = pair.sa.client().await;
            local.hello().await;
            let carol = local.login("carol", "correct horse").await;
            assert!(
                matches!(
                    local.call(Some(&carol), Request::WhoAmI).await,
                    Response::Ok(Reply::WhoAmI(_))
                ),
                "a local user is answered"
            );
            // Let go, each runs to its end, whole: the offset is set.
            held.let_go();
            in_hand(&pair, 0).await;
            drop(streams);
            assert_eq!(held.done(), 5);
            let row = pair
                .sa
                .store
                .subtitle_timing(sidecar.to_str().unwrap())
                .await
                .unwrap()
                .expect("the change was written");
            assert_eq!(row.offset_ms, 1_500);
            pair.stop().await;
        }

        /// Y8-02's budget: one pair's sessions are answered at most their
        /// connections' share of requests at once, resource streams
        /// included; one more is reset `BUSY`, while another pair's device
        /// and a local user are answered.
        #[tokio::test]
        async fn a_pair_s_remote_sessions_requests_at_once_are_capped() {
            use spoolframe_protocol::streams::bi;
            use tokio::io::AsyncWriteExt as _;
            let pair = Pair::start_both(
                "fed-media-remote-requests-pair",
                ServerOptions {
                    real_media: true,
                    admission: Admission {
                        max_pair_remote_connections: 2,
                        remote_requests_each: 1,
                        ..Admission::default()
                    },
                    ..ServerOptions::default()
                },
                ServerOptions::default(),
            )
            .await;
            let (_, _, bob) = subtitled_film(&pair).await;
            let device = device(&pair).await;
            let session = redeemed(&device, &token(&pair, bob, &device).await)
                .await
                .session;
            let held = &pair.sa.subtitle_timings.held;
            held.hold();
            // Two connections' worth: SB's budget, on one connection.
            let asking = timing_connection(&pair, &device).await;
            in_hand(&pair, 0).await;
            let mut streams = Vec::new();
            for _ in 0..2 {
                streams.push(ask_unread(&asking, &session, offset()).await);
            }
            held.until_at_it(2).await;
            assert!(
                reset_busy(ask_unread(&asking, &session, offset()).await).await,
                "past the pair's budget"
            );
            // A resource stream takes a place too: reset before it is read.
            let (mut send, recv) = asking.conn.open_bi().await.unwrap();
            send.write_u8(bi::RESOURCE).await.unwrap();
            assert!(reset_busy(recv).await, "a resource stream past the budget");
            drop(send);
            // A local user is answered.
            pair.sa.add_user("carol", "correct horse", None).await;
            let local = pair.sa.client().await;
            local.hello().await;
            let carol = local.login("carol", "correct horse").await;
            assert!(matches!(
                local.call(Some(&carol), Request::WhoAmI).await,
                Response::Ok(Reply::WhoAmI(_))
            ));
            // Dave's server, paired with SA too, and its user's device: its
            // own budget.
            let (dave_s, _) = sb_on(
                fresh_dir("fed-media-remote-requests-pair-dave").await,
                ServerOptions::default(),
                quiet_plex().await,
            )
            .await;
            let sa_on_dave = federation(&dave_s)
                .redeem_link(&pairing_link(&pair.sa, None).await, "dave")
                .await
                .unwrap();
            let dave_on_sa = pair
                .sa
                .store
                .live_peer_by_endpoint(&dave_s.server.id().to_string())
                .await
                .unwrap()
                .unwrap();
            federation(&pair.sa)
                .share(&dave_on_sa.id, &["plex:1".parse().unwrap()], "alice")
                .await
                .unwrap();
            federation(&dave_s).refresh(&sa_on_dave.id).await.unwrap();
            dave_s.add_user("dave", "correct horse", None).await;
            let dave = dave_s.store.user_by_name("dave").await.unwrap().unwrap().id;
            let other = self::device(&pair).await;
            let token = federation(&dave_s)
                .assert_play(dave, &pair.fed(movie().id()), other.endpoint.id())
                .await
                .unwrap()
                .token;
            let other_session = redeemed(&other, &token).await.session;
            let other_asking = timing_connection(&pair, &other).await;
            in_hand(&pair, 2).await;
            streams.push(ask_unread(&other_asking, &other_session, offset()).await);
            held.until_at_it(3).await;
            // Let go, every held change runs to its end.
            held.let_go();
            in_hand(&pair, 0).await;
            drop(streams);
            dave_s.stop().await;
            pair.stop().await;
        }

        /// Y8-02's budget: every pair's sessions together are answered at
        /// most every such connection's share of requests at once; one more
        /// is reset `BUSY`, while a local user is answered.
        #[tokio::test]
        async fn remote_sessions_requests_at_once_are_capped_in_all() {
            let pair = Pair::start_both(
                "fed-media-remote-requests-all",
                ServerOptions {
                    real_media: true,
                    admission: Admission {
                        max_all_remote_connections: 2,
                        remote_requests_each: 1,
                        ..Admission::default()
                    },
                    ..ServerOptions::default()
                },
                ServerOptions::default(),
            )
            .await;
            let (_, _, bob) = subtitled_film(&pair).await;
            let device = device(&pair).await;
            let session = redeemed(&device, &token(&pair, bob, &device).await)
                .await
                .session;
            let held = &pair.sa.subtitle_timings.held;
            held.hold();
            // Two connections' worth, the one redeemed on among them
            // (Z8-01), asked on the other.
            let asking = timing_connection(&pair, &device).await;
            in_hand(&pair, 0).await;
            let first = [
                ask_unread(&asking, &session, offset()).await,
                ask_unread(&asking, &session, offset()).await,
            ];
            held.until_at_it(2).await;
            assert!(
                reset_busy(ask_unread(&asking, &session, offset()).await).await,
                "past every pair's budget"
            );
            pair.sa.add_user("carol", "correct horse", None).await;
            let local = pair.sa.client().await;
            local.hello().await;
            let carol = local.login("carol", "correct horse").await;
            assert!(matches!(
                local.call(Some(&carol), Request::WhoAmI).await,
                Response::Ok(Reply::WhoAmI(_))
            ));
            held.let_go();
            in_hand(&pair, 0).await;
            drop(first);
            pair.stop().await;
        }

        /// The session is gone and its playback ended: a device that comes
        /// back, on a new connection, reads nothing of it.
        async fn ended(
            pair: &Pair,
            device: &TestClient,
            session: &SessionId,
            handle: &PlaybackHandle,
        ) {
            until(|| async {
                pair.sa
                    .auth
                    .session_owner(&session.0)
                    .await
                    .unwrap()
                    .is_none()
            })
            .await;
            let back = pair.sa.client_from(&device.endpoint).await;
            back.hello_naming(mkv_capabilities(), &[FEDERATION]).await;
            let (read, _) = back.read(session, handle, 0, 4096).await;
            assert!(matches!(read, MediaReadResponse::Err(_)), "{read:?}");
            assert!(unauthenticated(
                back.call(Some(session), start(movie().id())).await
            ));
        }

        /// SA revokes the grant mid-play: the session and its playback end
        /// at once, and the device, signed in no more, is let go. Unpairing
        /// does the same.
        #[tokio::test]
        async fn a_revoked_grant_or_an_unpairing_ends_the_remote_play() {
            let pair = Pair::start("fed-media-revoke").await;
            let (_, bob) = shared(&pair).await;
            let device = device(&pair).await;
            let session = redeemed(&device, &token(&pair, bob, &device).await)
                .await
                .session;
            let handle = started(&device, &session).await;
            federation(&pair.sa)
                .unshare(&pair.sb_on_sa, "plex:1", "alice")
                .await
                .unwrap();
            ended(&pair, &device, &session, &handle).await;

            pair.share("plex:1").await;
            let device = pair.sa.client_from(&device.endpoint).await;
            device.hello_naming(mkv_capabilities(), &[FEDERATION]).await;
            let session = redeemed(&device, &token(&pair, bob, &device).await)
                .await
                .session;
            let handle = started(&device, &session).await;
            federation(&pair.sa)
                .unpair(&pair.sb_on_sa, "alice")
                .await
                .unwrap();
            ended(&pair, &device, &session, &handle).await;
            pair.stop().await;
        }

        /// Bob's app plays SA's film straight from SA (P8-04): it asks SB
        /// where SA is, reaches SA, and plays there under the token SB got
        /// for this device. The bytes are the file's, SA holds the
        /// playback, and SB opened none: it carried no media. Bob's progress
        /// goes to SB.
        #[tokio::test]
        async fn bob_s_app_plays_sa_s_film_from_sa_and_sb_carries_none_of_it() {
            use spoolframe_client_sdk::{PlaybackOptions, RemoteMode};
            let pair = Pair::start("fed-media-sdk-direct").await;
            let (file, _) = shared(&pair).await;
            let (client, bob) =
                super::super::app::signed_in(&pair.sb, "bob", super::super::app::FEDERATION).await;
            let alien = pair.fed(movie().id());
            let hints = bob.remote_hints(&alien).await.unwrap();
            assert_eq!(hints.endpoint_id, pair.sa.server.id().to_string());
            assert!(hints.online);
            let remote = bob
                .start_remote_playback(
                    &alien,
                    Some(Selection::default()),
                    PlaybackOptions::default(),
                )
                .await
                .unwrap();
            assert_eq!(remote.mode, RemoteMode::Direct);
            assert_eq!(&remote.item, movie().id());
            assert_eq!(remote.session.server(), pair.sa.server.id().to_string());
            let bytes = remote
                .session
                .read(&remote.playback.handle, 0, 4096)
                .await
                .unwrap();
            assert_eq!(bytes, file[..4096]);
            bob.update_watch_progress(&alien, std::time::Duration::from_secs(60))
                .await
                .unwrap();
            assert_eq!(pair.sa.debug.list().open.len(), 1, "SA plays it");
            let on_sb = pair.sb.debug.list();
            assert!(
                on_sb.open.is_empty() && on_sb.ended.is_empty(),
                "SB carried none of it"
            );
            client.close().await;
            pair.stop().await;
        }

        /// SA reachable only through its own relay, which lets through its
        /// owners and friends (P8-04): Bob's app, which knows SA by that
        /// relay alone, plays SA's film directly, the play token getting it
        /// through the relay. The relay's question is answered as the ticket
        /// says: the token's holder is a friend, a wrong token a stranger,
        /// and once redeemed the session keeps the device a friend.
        #[tokio::test]
        async fn bob_s_app_plays_through_sa_s_own_relay_with_the_play_token() {
            use iroh::SecretKey;
            use spoolframe_client_sdk::{
                Client, ClientOptions, Device, MemoryStorage, PlaybackOptions, RemoteMode,
            };
            use spoolframe_protocol::codec::{read_frame, write_frame};
            use spoolframe_protocol::relay_access::{
                AccessAnswer, AccessQuery, RELAY_ACCESS_ALPN, Tier, play_relay_token,
            };
            use spoolframe_server::own_relay::{self, Access, RelayConfig, Serve};

            let key = SecretKey::generate();
            let http = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                .unwrap()
                .local_addr()
                .unwrap();
            let url = url::Url::parse(&format!("http://{http}")).unwrap();
            let sa = TestServer::start_with(
                fresh_dir("fed-media-relay-sa").await,
                ServerOptions {
                    name: "Alice's server".into(),
                    federation: true,
                    relays: vec![spoolframe_server::network::Relay::Custom(url.clone())],
                    own_relays: vec![key.public()],
                    ..ServerOptions::default()
                },
            )
            .await;
            let config = RelayConfig {
                server: sa.server.addr(),
                access: Access {
                    owner: true,
                    friends: true,
                    everyone: false,
                },
                serve: Serve::Proxy,
                http,
                quic: None,
            };
            let relay = own_relay::spawn(&config, key.clone(), &std::env::temp_dir())
                .await
                .unwrap();
            let home = iroh::RelayUrl::from(url.clone());
            until(|| async { sa.server.addr().relay_urls().any(|u| *u == home) }).await;
            // The relay is on loopback, which no pair's may be (M8-07):
            // SB, which keeps and dials it, allows it.
            let pair = Pair::with_sa(
                "fed-media-relay",
                sa,
                ServerOptions {
                    inner_relays: true,
                    ..ServerOptions::default()
                },
            )
            .await;
            let (file, bob) = shared(&pair).await;
            // SB knows SA by its relay alone, as a phone on cellular would.
            let pool = sqlx::SqlitePool::connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(pair.sb.dir.join(spoolframe_server::store::DB_FILE)),
            )
            .await
            .unwrap();
            sqlx::query("UPDATE federation_peers SET addrs = '[]', relays = json_array(?1)")
                .bind(url.as_str().trim_end_matches('/'))
                .execute(&pool)
                .await
                .unwrap();

            // The relay's question, asked as the relay asks it.
            let asker = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
                .secret_key(key)
                .relay_mode(iroh::RelayMode::Disabled)
                .alpns(vec![RELAY_ACCESS_ALPN.to_vec()])
                .bind()
                .await
                .unwrap();
            let asked = asker
                .connect(pair.sa.server.addr(), RELAY_ACCESS_ALPN)
                .await
                .unwrap();
            let tier = |endpoint: iroh::EndpointId, invite: Option<String>| {
                let asked = asked.clone();
                async move {
                    let (mut send, mut recv) = asked.open_bi().await.unwrap();
                    let query = AccessQuery {
                        endpoint: endpoint.to_string(),
                        invite,
                    };
                    write_frame(&mut send, &query).await.unwrap();
                    send.finish().unwrap();
                    let answer: AccessAnswer = read_frame(&mut recv).await.unwrap();
                    answer.tier
                }
            };
            let probe = device(&pair).await;
            let token = token(&pair, bob, &probe).await;
            let probe_id = probe.endpoint.id();
            assert_eq!(
                tier(
                    probe_id,
                    Some(play_relay_token(&token, probe_id.as_bytes()))
                )
                .await,
                Tier::Friend
            );
            assert_eq!(
                tier(
                    probe_id,
                    Some(play_relay_token("not it", probe_id.as_bytes()))
                )
                .await,
                Tier::Stranger
            );
            assert_eq!(tier(probe_id, None).await, Tier::Stranger);
            redeemed(&probe, &token).await;
            assert_eq!(tier(probe_id, None).await, Tier::Friend, "its session");

            let options = ClientOptions {
                lan_discovery: false,
                relays: true,
                experimental: super::super::app::FEDERATION,
                ..ClientOptions::default()
            };
            let phone = Client::open_with_options(
                Arc::new(MemoryStorage::default()),
                Device {
                    name: "phone".into(),
                    client_name: "sdk-test".into(),
                    client_version: "0".into(),
                    platform: spoolframe_client_sdk::types::Platform::Ios,
                    capabilities: mkv_capabilities(),
                },
                options,
            )
            .await
            .unwrap();
            let bob_app = super::super::app::sign_in(&phone, &pair.sb, "bob").await;
            let alien = pair.fed(movie().id());
            let remote = bob_app
                .start_remote_playback(
                    &alien,
                    Some(Selection::default()),
                    PlaybackOptions::default(),
                )
                .await
                .unwrap();
            assert_eq!(remote.mode, RemoteMode::Direct);
            let bytes = remote
                .session
                .read(&remote.playback.handle, 0, 4096)
                .await
                .unwrap();
            assert_eq!(bytes, file[..4096]);
            phone.close().await;
            pair.stop().await;
            drop(relay);
        }

        /// Bob's track preferences are SB's: SA has none of his. His play
        /// of SA's film with no selection chooses its subtitles as SB would,
        /// by the preferences SB sent with his token: French ones.
        #[tokio::test]
        async fn sa_chooses_bob_s_tracks_by_his_preferences_on_sb() {
            use spoolframe_types::{SubtitleMode, TrackPreferences};
            let pair = Pair::start("fed-media-preferences").await;
            let (_, bob) = shared(&pair).await;
            let mut french = TrackPreferences::default();
            french.subtitles.mode = SubtitleMode::Always;
            french.subtitles.languages = vec!["fre".into()];
            pair.sb
                .store
                .set_track_preferences(
                    spoolframe_server::principal::Principal::LocalUser(bob),
                    &french,
                )
                .await
                .unwrap();
            let device = device(&pair).await;
            let session = redeemed(&device, &token(&pair, bob, &device).await)
                .await
                .session;
            let mut request = start(movie().id());
            if let Request::StartPlayback { selection, .. } = &mut request {
                *selection = None;
            }
            let chosen = match device.call(Some(&session), request).await {
                Response::Ok(Reply::PlaybackStarted {
                    chosen: Some(chosen),
                    ..
                }) => chosen,
                other => panic!("did not start: {other:?}"),
            };
            // SA's own view of the film's tracks, as a user of its own.
            pair.sa.add_user("alice", "correct horse", None).await;
            let alice_s = pair.sa.client().await;
            alice_s.hello_with(None, Some(mkv_capabilities())).await;
            let alice = alice_s.login("alice", "correct horse").await;
            let Response::Ok(Reply::Tracks(tracks)) = alice_s
                .call(
                    Some(&alice),
                    Request::Tracks {
                        id: movie().id().clone(),
                    },
                )
                .await
            else {
                panic!("no tracks");
            };
            let subtitle = chosen.selection.subtitle.expect("a subtitle chosen");
            assert_eq!(
                tracks.subtitles[subtitle as usize].language.as_deref(),
                Some("fre"),
                "{chosen:?}"
            );
            pair.stop().await;
        }

        /// A device in a play its token opened is no stranger to SA: with
        /// the strangers' places all taken, it comes back mid-play.
        #[tokio::test]
        async fn a_device_in_a_remote_play_reconnects_past_the_strangers_cap() {
            let pair = Pair::start_both(
                "fed-media-reconnect",
                ServerOptions {
                    admission: spoolframe_server::admission::Admission {
                        max_strangers: 1,
                        ..Default::default()
                    },
                    ..ServerOptions::default()
                },
                ServerOptions::default(),
            )
            .await;
            let (_, bob) = shared(&pair).await;
            let device = device(&pair).await;
            let session = redeemed(&device, &token(&pair, bob, &device).await)
                .await
                .session;
            started(&device, &session).await;
            let held = pair.sa.server.connections().snapshot().len();
            device.conn.close(0u32.into(), b"later");
            until(|| async { pair.sa.server.connections().snapshot().len() < held }).await;
            let stranger = pair.sa.client().await;
            stranger.hello().await;
            let back = device.reconnect(&pair.sa).await;
            assert_eq!(
                back.call(None, Request::Ping).await,
                Response::Ok(Reply::Pong)
            );
            pair.stop().await;
        }

        /// A new connection from `from`'s endpoint to SA's client protocol,
        /// its Hello answered; or `Err` once it is closed `BUSY`.
        async fn connection(pair: &Pair, from: &Endpoint) -> Result<TestClient, ()> {
            let client = pair.sa.client_from(from).await;
            tokio::select! {
                biased;
                closed = client.conn.closed() => {
                    assert!(
                        matches!(
                            &closed,
                            ConnectionError::ApplicationClosed(c)
                                if c.error_code == close::BUSY.into()
                        ),
                        "{closed:?}"
                    );
                    Err(())
                }
                () = client.hello() => Ok(client),
            }
        }

        /// C8-04: a device known to SA only by a paired server's session
        /// holds at most so many client connections at once: one more is
        /// closed `BUSY`, while another such device and a local one still
        /// get in. Once one closes, the next is let in.
        #[tokio::test]
        async fn a_device_known_by_a_remote_session_alone_is_capped_in_connections() {
            let pair = Pair::start("fed-media-remote-cap").await;
            let (_, bob) = shared(&pair).await;
            let device = device(&pair).await;
            redeemed(&device, &token(&pair, bob, &device).await).await;
            // The connection it redeemed on came as a stranger's and is
            // the remote session's now (Z8-01), counted as those after.
            let mut held = vec![device.clone()];
            for _ in 1..Admission::DEFAULT_MAX_REMOTE_CONNECTIONS {
                held.push(connection(&pair, &device.endpoint).await.expect("let in"));
            }
            assert!(connection(&pair, &device.endpoint).await.is_err());
            // Another device in a remote play, and a local user's device,
            // are their own.
            let other = self::device(&pair).await;
            redeemed(&other, &token(&pair, bob, &other).await).await;
            connection(&pair, &other.endpoint).await.expect("let in");
            pair.sa.add_user("carol", "correct horse", None).await;
            let local = pair.sa.client().await;
            local.hello().await;
            local.login("carol", "correct horse").await;
            connection(&pair, &local.endpoint).await.expect("let in");
            // One of the device's closed, the next is let in.
            held.remove(1).conn.close(0u32.into(), b"done");
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Ok(again) = connection(&pair, &device.endpoint).await {
                        break again;
                    }
                }
            })
            .await
            .expect("let in once one closed");
            drop(held);
            pair.stop().await;
        }

        /// Z8-01: redeeming another token on a device at its cap keeps its
        /// connections counted, the one it redeemed on among them: one
        /// more is still closed `BUSY`, where the redemption let every one
        /// of them go uncounted.
        #[tokio::test]
        async fn redeeming_again_keeps_a_device_s_connections_counted() {
            let pair = Pair::start("fed-media-remote-redeem-again").await;
            let (_, bob) = shared(&pair).await;
            let device = device(&pair).await;
            redeemed(&device, &token(&pair, bob, &device).await).await;
            let mut held = vec![device.clone()];
            for _ in 1..Admission::DEFAULT_MAX_REMOTE_CONNECTIONS {
                held.push(connection(&pair, &device.endpoint).await.expect("let in"));
            }
            assert!(connection(&pair, &device.endpoint).await.is_err());
            redeemed(&device, &token(&pair, bob, &device).await).await;
            assert!(
                connection(&pair, &device.endpoint).await.is_err(),
                "still at its cap"
            );
            drop(held);
            pair.stop().await;
        }

        /// The review of Z8-01: a token redeemed on a stranger's connection
        /// the remote caps have no room for is answered, then the
        /// connection is closed `BUSY`, as one more past the caps is, where
        /// it stayed a stranger's holding the session, outside the remote
        /// requests' budget. The device's next connection is refused too,
        /// and one with room is not closed.
        #[tokio::test]
        async fn a_token_redeemed_past_the_remote_caps_closes_its_connection_busy() {
            let pair = Pair::start_both(
                "fed-media-remote-redeem-past",
                ServerOptions {
                    admission: Admission {
                        max_all_remote_connections: 1,
                        ..Admission::default()
                    },
                    ..ServerOptions::default()
                },
                ServerOptions::default(),
            )
            .await;
            let (_, bob) = shared(&pair).await;
            let first = device(&pair).await;
            redeemed(&first, &token(&pair, bob, &first).await).await;
            let second = device(&pair).await;
            redeemed(&second, &token(&pair, bob, &second).await).await;
            let closed = tokio::time::timeout(Duration::from_secs(5), second.conn.closed())
                .await
                .expect("closed once answered");
            assert!(
                matches!(
                    &closed,
                    ConnectionError::ApplicationClosed(c) if c.error_code == close::BUSY.into()
                ),
                "{closed:?}"
            );
            assert!(connection(&pair, &second.endpoint).await.is_err());
            assert_eq!(
                first.call(None, Request::Ping).await,
                Response::Ok(Reply::Pong),
                "the one with room is held"
            );
            pair.stop().await;
        }

        /// C8-04: every such device's connections together are capped too:
        /// with room for two, two devices' fill it, and a third device's is
        /// closed `BUSY` though its own key has room; a local device still
        /// gets in.
        #[tokio::test]
        async fn every_remote_session_device_s_connections_together_are_capped() {
            let pair = Pair::start_both(
                "fed-media-remote-all",
                ServerOptions {
                    admission: Admission {
                        max_all_remote_connections: 2,
                        ..Admission::default()
                    },
                    ..ServerOptions::default()
                },
                ServerOptions::default(),
            )
            .await;
            let (_, bob) = shared(&pair).await;
            // Each device's connection it redeemed on is counted (Z8-01):
            // the first two fill the room, and the third's is closed
            // `BUSY` once answered.
            let mut devices = Vec::new();
            for _ in 0..3 {
                let device = device(&pair).await;
                redeemed(&device, &token(&pair, bob, &device).await).await;
                devices.push(device);
            }
            assert!(connection(&pair, &devices[0].endpoint).await.is_err());
            assert!(connection(&pair, &devices[2].endpoint).await.is_err());
            let local = pair.sa.client().await;
            local.hello().await;
            pair.stop().await;
        }

        /// Z8-01, D8-02: a device in a session of SB's, then of Carol's
        /// server's too, counts in both pairs' shares: with a share of two
        /// each, Carol's server's next device has room for the connection
        /// it redeemed on alone, and one more is closed `BUSY`, where the
        /// second redemption left the first device counted in neither.
        #[tokio::test]
        async fn a_device_in_two_pairs_sessions_counts_in_both_shares() {
            let pair = Pair::start_both(
                "fed-media-remote-two-pairs",
                ServerOptions {
                    admission: Admission {
                        max_pair_remote_connections: 2,
                        ..Admission::default()
                    },
                    ..ServerOptions::default()
                },
                ServerOptions::default(),
            )
            .await;
            let (_, bob) = shared(&pair).await;
            let (carol_s, _) = sb_on(
                fresh_dir("fed-media-remote-two-pairs-carol").await,
                ServerOptions::default(),
                quiet_plex().await,
            )
            .await;
            let sa_on_carol = federation(&carol_s)
                .redeem_link(&pairing_link(&pair.sa, None).await, "carol")
                .await
                .unwrap();
            let carol_on_sa = pair
                .sa
                .store
                .live_peer_by_endpoint(&carol_s.server.id().to_string())
                .await
                .unwrap()
                .unwrap();
            federation(&pair.sa)
                .share(&carol_on_sa.id, &["plex:1".parse().unwrap()], "alice")
                .await
                .unwrap();
            federation(&carol_s).refresh(&sa_on_carol.id).await.unwrap();
            carol_s.add_user("carol", "correct horse", None).await;
            let carol = carol_s
                .store
                .user_by_name("carol")
                .await
                .unwrap()
                .unwrap()
                .id;
            let carol_s_token = |device: &TestClient| {
                let carol_s = &carol_s;
                let pair = &pair;
                let endpoint = device.endpoint.id();
                async move {
                    federation(carol_s)
                        .assert_play(carol, &pair.fed(movie().id()), endpoint)
                        .await
                        .unwrap()
                        .token
                }
            };
            let both = device(&pair).await;
            redeemed(&both, &token(&pair, bob, &both).await).await;
            redeemed(&both, &carol_s_token(&both).await).await;
            let next = device(&pair).await;
            redeemed(&next, &carol_s_token(&next).await).await;
            assert!(
                connection(&pair, &next.endpoint).await.is_err(),
                "Carol's server's share is the first device's too"
            );
            carol_s.stop().await;
            pair.stop().await;
        }

        /// W4: a device held under SB's session that redeems Carol's
        /// server's token takes a place of Carol's share, so where it is
        /// full its connection is closed `BUSY` once answered, as a new one
        /// is, and Carol's share is not spent past.
        #[tokio::test]
        async fn a_device_joining_a_full_pair_s_session_is_closed_busy() {
            let pair = Pair::start_both(
                "fed-media-remote-join-full",
                ServerOptions {
                    admission: Admission {
                        max_pair_remote_connections: 2,
                        ..Admission::default()
                    },
                    ..ServerOptions::default()
                },
                ServerOptions::default(),
            )
            .await;
            let (_, bob) = shared(&pair).await;
            let (carol_s, _) = sb_on(
                fresh_dir("fed-media-remote-join-full-carol").await,
                ServerOptions::default(),
                quiet_plex().await,
            )
            .await;
            let sa_on_carol = federation(&carol_s)
                .redeem_link(&pairing_link(&pair.sa, None).await, "carol")
                .await
                .unwrap();
            let carol_on_sa = pair
                .sa
                .store
                .live_peer_by_endpoint(&carol_s.server.id().to_string())
                .await
                .unwrap()
                .unwrap();
            federation(&pair.sa)
                .share(&carol_on_sa.id, &["plex:1".parse().unwrap()], "alice")
                .await
                .unwrap();
            federation(&carol_s).refresh(&sa_on_carol.id).await.unwrap();
            carol_s.add_user("carol", "correct horse", None).await;
            let carol = carol_s
                .store
                .user_by_name("carol")
                .await
                .unwrap()
                .unwrap()
                .id;
            let carol_s_token = |device: &TestClient| {
                let carol_s = &carol_s;
                let pair = &pair;
                let endpoint = device.endpoint.id();
                async move {
                    federation(carol_s)
                        .assert_play(carol, &pair.fed(movie().id()), endpoint)
                        .await
                        .unwrap()
                        .token
                }
            };
            // Carol's share is full with two devices'.
            let mut full = Vec::new();
            for _ in 0..2 {
                let device = device(&pair).await;
                redeemed(&device, &carol_s_token(&device).await).await;
                full.push(device);
            }
            let both = device(&pair).await;
            redeemed(&both, &token(&pair, bob, &both).await).await;
            redeemed(&both, &carol_s_token(&both).await).await;
            let closed = tokio::time::timeout(Duration::from_secs(5), both.conn.closed())
                .await
                .expect("closed once answered");
            assert!(
                matches!(
                    &closed,
                    ConnectionError::ApplicationClosed(c) if c.error_code == close::BUSY.into()
                ),
                "{closed:?}"
            );
            assert_eq!(
                full[0].call(None, Request::Ping).await,
                Response::Ok(Reply::Pong),
                "the ones in Carol's share are held"
            );
            carol_s.stop().await;
            pair.stop().await;
        }

        /// C8-04: one pair's devices hold at most their pair's share of
        /// those connections, every key of theirs together: with a share
        /// of two, SB's third device is closed `BUSY` though its key and
        /// the server have room, and Carol's server's device still gets in.
        #[tokio::test]
        async fn one_pair_s_devices_hold_only_their_pair_s_share_of_connections() {
            let pair = Pair::start_both(
                "fed-media-remote-pair",
                ServerOptions {
                    admission: Admission {
                        max_pair_remote_connections: 2,
                        ..Admission::default()
                    },
                    ..ServerOptions::default()
                },
                ServerOptions::default(),
            )
            .await;
            let (_, bob) = shared(&pair).await;
            // The connections the first two redeemed on fill the share
            // (Z8-01).
            let mut devices = Vec::new();
            for _ in 0..3 {
                let device = device(&pair).await;
                redeemed(&device, &token(&pair, bob, &device).await).await;
                devices.push(device);
            }
            assert!(connection(&pair, &devices[2].endpoint).await.is_err());
            // Carol's server, paired with SA too, and its user's device.
            let (carol_s, _) = sb_on(
                fresh_dir("fed-media-remote-pair-carol").await,
                ServerOptions::default(),
                quiet_plex().await,
            )
            .await;
            let sa_on_carol = federation(&carol_s)
                .redeem_link(&pairing_link(&pair.sa, None).await, "carol")
                .await
                .unwrap();
            let carol_on_sa = pair
                .sa
                .store
                .live_peer_by_endpoint(&carol_s.server.id().to_string())
                .await
                .unwrap()
                .unwrap();
            federation(&pair.sa)
                .share(&carol_on_sa.id, &["plex:1".parse().unwrap()], "alice")
                .await
                .unwrap();
            federation(&carol_s).refresh(&sa_on_carol.id).await.unwrap();
            carol_s.add_user("carol", "correct horse", None).await;
            let carol = carol_s
                .store
                .user_by_name("carol")
                .await
                .unwrap()
                .unwrap()
                .id;
            let device = device(&pair).await;
            let token = federation(&carol_s)
                .assert_play(carol, &pair.fed(movie().id()), device.endpoint.id())
                .await
                .unwrap()
                .token;
            redeemed(&device, &token).await;
            connection(&pair, &device.endpoint)
                .await
                .expect("another pair's share");
            carol_s.stop().await;
            pair.stop().await;
        }

        /// With SA offline to SB, a grant is refused at once.
        #[tokio::test]
        async fn with_sa_offline_a_grant_is_refused_at_once() {
            use spoolframe_client_sdk::SdkError;
            let pair = Pair::start("fed-media-offline").await;
            shared(&pair).await;
            let (client, bob) =
                super::super::app::signed_in(&pair.sb, "bob", super::super::app::FEDERATION).await;
            let alien = pair.fed(movie().id());
            let Pair { sa, sb, .. } = pair;
            let origin = sa.server_id.to_string();
            sa.stop().await;
            until(|| async { !federation(&sb).online(&origin) }).await;
            assert!(!bob.remote_hints(&alien).await.unwrap().online);
            assert_eq!(
                bob.remote_grant(&alien).await.unwrap_err(),
                SdkError::OriginOffline
            );
            client.close().await;
            sb.stop().await;
        }

        /// A remote user off SA's LAN is a guest WAN stream (PX-46): with
        /// one stream allowed, a second device's play is refused as full.
        #[tokio::test]
        async fn a_remote_play_off_the_lan_is_a_guest_wan_stream() {
            let loopback: Vec<spoolframe_server::announce::Network> = ["127.0.0.0/8", "::1/128"]
                .iter()
                .map(|n| n.parse().unwrap())
                .collect();
            let pair = Pair::start_both(
                "fed-media-wan",
                ServerOptions {
                    uplink: spoolframe_server::uplink::UplinkSettings {
                        max_streams: Some(1),
                        wan_prefixes: loopback,
                        ..Default::default()
                    },
                    ..ServerOptions::default()
                },
                ServerOptions::default(),
            )
            .await;
            let (_, bob) = shared(&pair).await;
            let mut sessions = Vec::new();
            for _ in 0..2 {
                let device = pair.sa.loopback_client().await;
                device.hello_naming(mkv_capabilities(), &[FEDERATION]).await;
                let session = redeemed(&device, &token(&pair, bob, &device).await)
                    .await
                    .session;
                sessions.push((device, session));
            }
            let (first, first_session) = &sessions[0];
            started(first, first_session).await;
            let (second, second_session) = &sessions[1];
            let refused = second.call(Some(second_session), start(movie().id())).await;
            assert!(
                matches!(
                    refused,
                    Response::Err(spoolframe_protocol::Error::WanStreamsFull { max: 1 })
                ),
                "{refused:?}"
            );
            pair.stop().await;
        }

        /// SB's record of SA names no address and no relay: a device
        /// asking SB where SA is gets no way there. SB itself still holds
        /// its connection to SA.
        async fn forget_sa_s_addresses(pair: &Pair) {
            let pool = sqlx::SqlitePool::connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(pair.sb.dir.join(spoolframe_server::store::DB_FILE)),
            )
            .await
            .unwrap();
            sqlx::query("UPDATE federation_peers SET addrs = '[]', relays = '[]'")
                .execute(&pool)
                .await
                .unwrap();
        }

        /// A device of Bob's on SB, naming the feature: `name`, signed in.
        async fn on_sb(pair: &Pair, name: &str) -> (TestClient, SessionId) {
            if pair.sb.store.user_by_name(name).await.unwrap().is_none() {
                pair.sb.add_user(name, "correct horse", None).await;
            }
            let device = pair.sb.client().await;
            device.hello_naming(mkv_capabilities(), &[FEDERATION]).await;
            let session = device.login(name, "correct horse").await;
            (device, session)
        }

        /// SA's film, started on SB by its `fed:` id: played from SA
        /// through SB.
        async fn proxied(pair: &Pair, device: &TestClient, session: &SessionId) -> PlaybackHandle {
            match device
                .call(Some(session), start(&pair.fed(movie().id())))
                .await
            {
                Response::Ok(Reply::PlaybackStarted { handle, .. }) => handle,
                other => panic!("did not start through SB: {other:?}"),
            }
        }

        /// The files under `dir`, but its database's and what is under
        /// `skip`.
        fn files_in(dir: &std::path::Path, skip: &std::path::Path) -> Vec<std::path::PathBuf> {
            let mut found = Vec::new();
            let mut left = vec![dir.to_path_buf()];
            while let Some(at) = left.pop() {
                for entry in std::fs::read_dir(&at).unwrap().flatten() {
                    let path = entry.path();
                    let name = path.file_name().unwrap().to_string_lossy().into_owned();
                    if path.starts_with(skip) || name.contains(".db") {
                        continue;
                    }
                    if path.is_dir() {
                        left.push(path);
                    } else {
                        found.push(path);
                    }
                }
            }
            found.sort();
            found
        }

        /// SA out of the device's reach (P8-04): SB's record of SA names
        /// no way there, so Bob's app cannot dial SA, and plays SA's film
        /// through SB instead. SB plays it from SA in its own name, and on
        /// to the app: the bytes are the file's, SA counts a proxied play,
        /// and the app's stop ends SA's playback too.
        #[tokio::test]
        async fn out_of_reach_of_sa_bob_s_app_plays_sa_s_film_through_sb() {
            use spoolframe_client_sdk::{PlaybackOptions, RemoteMode};
            let pair = Pair::start("fed-media-proxy").await;
            let (file, _) = shared(&pair).await;
            forget_sa_s_addresses(&pair).await;
            let (client, bob) =
                super::super::app::signed_in(&pair.sb, "bob", super::super::app::FEDERATION).await;
            let alien = pair.fed(movie().id());
            let remote = bob
                .start_remote_playback(
                    &alien,
                    Some(Selection::default()),
                    PlaybackOptions::default(),
                )
                .await
                .unwrap();
            assert_eq!(remote.mode, RemoteMode::Proxied);
            assert_eq!(remote.item, alien);
            assert_eq!(remote.session.server(), pair.sb.server.id().to_string());
            let bytes = remote
                .session
                .read(&remote.playback.handle, 1000, 4096)
                .await
                .unwrap();
            assert_eq!(bytes, file[1000..5096]);
            assert_eq!(pair.sa.debug.list().open.len(), 1, "SA plays it");
            assert_eq!(pair.sb.debug.list().open.len(), 1, "SB plays it on");
            // Counted beside the redemption.
            until(|| async {
                let served = pair
                    .sa
                    .store
                    .federation_plays(&pair.sb_on_sa, PlaySide::Served)
                    .await
                    .unwrap();
                (served.plays, served.direct(), served.proxied) == (1, 0, 1)
            })
            .await;
            let asserted = pair
                .sb
                .store
                .federation_plays(&pair.sa_on_sb, PlaySide::Asserted)
                .await
                .unwrap();
            assert_eq!((asserted.plays, asserted.proxied), (1, 1), "SB carried it");
            // The metrics: SB plays one on, has relayed the read's bytes,
            // and counts a play of SA's items; SA counts one of its own.
            let number = |server: &TestServer, name: &str, labels: &[(&str, &str)]| {
                server
                    .metrics
                    .families()
                    .into_iter()
                    .find(|f| f.name == name)
                    .and_then(|f| f.value(labels))
                    .unwrap()
            };
            assert_eq!(
                number(&pair.sb, "spoolframe_federation_proxied_playbacks", &[]),
                1.0
            );
            assert!(number(&pair.sb, "spoolframe_federation_proxied_bytes_total", &[]) >= 4096.0);
            assert_eq!(
                number(
                    &pair.sb,
                    "spoolframe_federation_plays_total",
                    &[("items", "theirs")]
                ),
                1.0
            );
            assert_eq!(
                number(
                    &pair.sa,
                    "spoolframe_federation_plays_total",
                    &[("items", "ours")]
                ),
                1.0
            );
            remote
                .session
                .stop_playback(&remote.playback.handle)
                .await
                .unwrap();
            until(|| async { pair.sa.debug.list().open.is_empty() }).await;
            // Stopped, it is no longer played on.
            until(|| async {
                number(&pair.sb, "spoolframe_federation_proxied_playbacks", &[]) == 0.0
            })
            .await;
            client.close().await;
            pair.stop().await;
        }

        /// Reads at the protocol's largest through SB arrive whole, read
        /// after read (the end-to-end test found them lost about half the
        /// time): SB passes SA's answer on in whole parts, not in each
        /// piece as it came off SA's stream.
        #[tokio::test]
        async fn the_largest_reads_through_sb_arrive_whole() {
            use spoolframe_client_sdk::PlaybackOptions;
            let pair = Pair::start("fed-media-proxy-large").await;
            pair.share("plex:1").await;
            pair.refresh().await;
            let file = pair.sa.write_media(20 << 20);
            pair.sb.add_user("bob", "correct horse", None).await;
            forget_sa_s_addresses(&pair).await;
            let (client, bob) =
                super::super::app::signed_in(&pair.sb, "bob", super::super::app::FEDERATION).await;
            let remote = bob
                .start_remote_playback(
                    &pair.fed(movie().id()),
                    Some(Selection::default()),
                    PlaybackOptions::default(),
                )
                .await
                .unwrap();
            let most = spoolframe_protocol::MAX_READ;
            for round in 0..10u64 {
                let offset = (round * 1_000_003) % (file.len() as u64 - most);
                let bytes = remote
                    .session
                    .read(&remote.playback.handle, offset, most)
                    .await
                    .unwrap_or_else(|e| panic!("read {round} failed: {e}"));
                let at = offset as usize;
                assert!(
                    bytes == file[at..at + most as usize],
                    "read {round} differs"
                );
            }
            client.close().await;
            pair.stop().await;
        }

        /// SB goes away mid-play: SA's playback through it ends with the
        /// connection, as a device's that is gone does, and neither
        /// server is left with anything on disk for it. SB, back, has
        /// nothing of it to play on.
        #[tokio::test]
        async fn sb_restarting_mid_proxy_ends_the_play_and_leaves_nothing() {
            let pair = Pair::start_both(
                "fed-media-proxy-restart",
                ServerOptions {
                    disconnect_grace: std::time::Duration::from_secs(1),
                    ..ServerOptions::default()
                },
                ServerOptions::default(),
            )
            .await;
            let (file, _) = shared(&pair).await;
            let skip = pair.files();
            let before = files_in(&pair.sb.dir, &skip);
            let (device, bob) = on_sb(&pair, "bob").await;
            let handle = proxied(&pair, &device, &bob).await;
            let (read, bytes) = device.read(&bob, &handle, 0, 4096).await;
            assert!(matches!(read, MediaReadResponse::Ok(_)), "{read:?}");
            assert_eq!(bytes, file[..4096]);
            assert_eq!(pair.sa.debug.list().open.len(), 1);
            let Pair { sa, sb, .. } = pair;
            let dir = sb.dir.clone();
            sb.stop().await;
            until(|| async { sa.debug.list().open.is_empty() }).await;
            assert_eq!(files_in(&dir, &skip), before, "SB wrote nothing");
            let (sb, _) = sb_with(dir, ServerOptions::default()).await;
            assert!(sb.debug.list().open.is_empty());
            let back = sb.client_from(&device.endpoint).await;
            back.hello_naming(mkv_capabilities(), &[FEDERATION]).await;
            let (read, _) = back.read(&bob, &handle, 0, 4096).await;
            assert!(matches!(read, MediaReadResponse::Err(_)), "{read:?}");
            sa.stop().await;
            sb.stop().await;
        }

        /// A token SB holds for its own endpoint (proxy mode) opens its
        /// one film on SA as a device's does: not another item, nor the
        /// libraries. SA counts it as proxied.
        #[tokio::test]
        async fn a_proxy_s_play_token_opens_its_film_and_nothing_else() {
            let pair = Pair::start("fed-media-proxy-scope").await;
            let (_, bob) = shared(&pair).await;
            let token = federation(&pair.sb)
                .assert_play(bob, &pair.fed(movie().id()), pair.sb.server.id())
                .await
                .unwrap()
                .token;
            let sb_device = pair.sa.client_from(pair.sb.server.endpoint()).await;
            // As SB's own client takes SA's events (a server's endpoint
            // takes no stream of its clients').
            sb_device
                .conn
                .set_max_concurrent_uni_streams(iroh::endpoint::VarInt::from_u32(4));
            sb_device
                .hello_naming(mkv_capabilities(), &[FEDERATION])
                .await;
            let session = redeemed(&sb_device, &token).await.session;
            let episode = MediaId::new(Namespace::Plex, MediaKind::Episode, "12").unwrap();
            for request in [Request::Libraries, start(&episode)] {
                let name = format!("{request:?}");
                assert!(
                    unauthenticated(sb_device.call(Some(&session), request).await),
                    "{name}"
                );
            }
            started(&sb_device, &session).await;
            until(|| async {
                let served = pair
                    .sa
                    .store
                    .federation_plays(&pair.sb_on_sa, PlaySide::Served)
                    .await
                    .unwrap();
                (served.direct(), served.proxied) == (0, 1)
            })
            .await;
            pair.stop().await;
        }

        /// Two of SB's users play SA's film through SB at once: one's
        /// start does not end the other's play, and each reads its own.
        #[tokio::test]
        async fn two_of_sb_s_users_play_through_sb_at_once() {
            let pair = Pair::start("fed-media-proxy-two").await;
            let (file, _) = shared(&pair).await;
            let (bob_s, bob) = on_sb(&pair, "bob").await;
            let (carol_s, carol) = on_sb(&pair, "carol").await;
            let bob_plays = proxied(&pair, &bob_s, &bob).await;
            let carol_plays = proxied(&pair, &carol_s, &carol).await;
            for (device, session, handle) in
                [(&bob_s, &bob, &bob_plays), (&carol_s, &carol, &carol_plays)]
            {
                let (read, bytes) = device.read(session, handle, 0, 4096).await;
                assert!(matches!(read, MediaReadResponse::Ok(_)), "{read:?}");
                assert_eq!(bytes, file[..4096]);
            }
            assert_eq!(pair.sa.debug.list().open.len(), 2);
            assert!(pair.sa.debug.list().ended.is_empty());
            // A re-plan passes through to SA's playback, and the read
            // after it is at SB's revision.
            let changed = bob_s
                .call(
                    Some(&bob),
                    Request::ChangeQuality {
                        handle: bob_plays.clone(),
                        quality: Default::default(),
                        bitmap: Default::default(),
                        position_ms: 1_000,
                        display: None,
                        onward_share: None,
                    },
                )
                .await;
            assert!(
                matches!(changed, Response::Ok(Reply::PlaybackChanged { .. })),
                "{changed:?}"
            );
            let (read, _) = bob_s.read(&bob, &bob_plays, 0, 4096).await;
            assert!(matches!(read, MediaReadResponse::Ok(_)), "{read:?}");
            // Both play over SB's one connection to SA: Bob's stop ends
            // his session there, and Carol's play goes on.
            let stopped = bob_s
                .call(
                    Some(&bob),
                    Request::StopPlayback {
                        handle: bob_plays.clone(),
                    },
                )
                .await;
            assert_eq!(stopped, Response::Ok(Reply::Done));
            until(|| async { pair.sa.debug.list().open.len() == 1 }).await;
            let (read, bytes) = carol_s.read(&carol, &carol_plays, 4096, 4096).await;
            assert!(matches!(read, MediaReadResponse::Ok(_)), "{read:?}");
            assert_eq!(bytes, file[4096..8192]);
            pair.stop().await;
        }

        /// Bob's app reports progress on SA's film to SB while SB plays it
        /// through for him: the playback keeps his play, and the report
        /// opens no second one.
        #[tokio::test]
        async fn a_report_beside_a_proxied_play_opens_no_second_play() {
            let pair = Pair::start("fed-media-proxy-report").await;
            let (_, bob_id) = shared(&pair).await;
            let (device, bob) = on_sb(&pair, "bob").await;
            let alien = pair.fed(movie().id());
            proxied(&pair, &device, &bob).await;
            let principal = spoolframe_server::principal::Principal::LocalUser(bob_id);
            until(|| async { !pair.sb.store.all_plays(principal).await.unwrap().is_empty() }).await;
            let report = device
                .call(
                    Some(&bob),
                    Request::UpdateWatchProgress {
                        id: alien.clone(),
                        position_ms: 60_000,
                    },
                )
                .await;
            assert_eq!(report, Response::Ok(Reply::Done));
            // A second play's row is written in the background: given the
            // time, there is none.
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let plays = pair.sb.store.all_plays(principal).await.unwrap();
            assert_eq!(plays.len(), 1, "{plays:?}");
            pair.stop().await;
        }

        /// SB keeps its connection to SA only while a play needs it, and a
        /// while after: once its plays stop it is closed after the idle
        /// time. An unpairing closes it at once, and ends the plays on it.
        #[tokio::test]
        async fn sb_s_connection_to_sa_goes_once_no_play_needs_it() {
            let pair = Pair::start("fed-media-proxy-idle").await;
            shared(&pair).await;
            let sb = federation(&pair.sb);
            sb.set_play_idle(std::time::Duration::from_millis(200));
            let (device, bob) = on_sb(&pair, "bob").await;
            let handle = proxied(&pair, &device, &bob).await;
            assert_eq!(sb.play_connections(), 1);
            let stopped = device
                .call(Some(&bob), Request::StopPlayback { handle })
                .await;
            assert_eq!(stopped, Response::Ok(Reply::Done));
            until(|| async { sb.play_connections() == 0 }).await;
            until(|| async { pair.sa.debug.list().open.is_empty() }).await;
            proxied(&pair, &device, &bob).await;
            assert_eq!(sb.play_connections(), 1);
            sb.unpair(&pair.sa_on_sb, "admin").await.unwrap();
            assert_eq!(sb.play_connections(), 0);
            until(|| async { pair.sb.debug.list().open.is_empty() }).await;
            pair.stop().await;
        }

        /// SB's admin confirming a new key for SA closes SB's connection
        /// to SA's old one at once (F8-03), and the plays on it end: the
        /// next play dials the new key.
        #[tokio::test]
        async fn a_confirmed_key_closes_the_connection_to_the_old_one() {
            let pair = Pair::start("fed-media-proxy-rotated").await;
            shared(&pair).await;
            let sb = federation(&pair.sb);
            let (device, bob) = on_sb(&pair, "bob").await;
            proxied(&pair, &device, &bob).await;
            assert_eq!(sb.play_connections(), 1);
            let new = iroh::SecretKey::from_bytes(&[12; 32]).public().to_string();
            pair.sb
                .store
                .hold_peer_key(&pair.sa_on_sb, &new)
                .await
                .unwrap();
            sb.confirm_key(&pair.sa_on_sb, &new, "admin").await.unwrap();
            assert_eq!(sb.play_connections(), 0);
            until(|| async { pair.sb.debug.list().open.is_empty() }).await;
            pair.stop().await;
        }

        /// SA's admin stops a playback SB plays on: SB's ends at once, as
        /// stopped, so the device is told not to start it again.
        #[tokio::test]
        async fn sa_s_admin_stop_ends_the_play_on_sb_as_stopped() {
            let pair = Pair::start("fed-media-proxy-admin-stop").await;
            shared(&pair).await;
            let (device, bob) = on_sb(&pair, "bob").await;
            let handle = proxied(&pair, &device, &bob).await;
            let id = pair.sa.debug.playbacks.snapshots()[0].id.clone();
            assert!(pair.sa.debug.playbacks.stop(&id));
            until(|| async { pair.sb.debug.list().open.is_empty() }).await;
            let (read, _) = device.read(&bob, &handle, 0, 4096).await;
            assert!(
                matches!(read, MediaReadResponse::Err(Error::PlaybackStopped)),
                "{read:?}"
            );
            pair.stop().await;
        }

        /// A re-plan SB passes through is kept on SB as SA made it: SB's
        /// Playback page shows the plan SA plays, not the start's.
        #[tokio::test]
        async fn a_re_plan_through_sb_is_kept_on_sb_as_sa_made_it() {
            let pair = Pair::start("fed-media-proxy-replan").await;
            shared(&pair).await;
            let (device, bob) = on_sb(&pair, "bob").await;
            let handle = proxied(&pair, &device, &bob).await;
            let changed = device
                .call(
                    Some(&bob),
                    Request::ChangeSubtitle {
                        handle,
                        subtitle: Some(1),
                        bitmap: Default::default(),
                        position_ms: 1_000,
                        display: None,
                        onward_share: None,
                    },
                )
                .await;
            assert!(
                matches!(changed, Response::Ok(Reply::PlaybackChanged { .. })),
                "{changed:?}"
            );
            let (sa, sb) = (pair.sa.debug.list(), pair.sb.debug.list());
            assert_eq!(
                (&sb.open[0].plan, &sb.open[0].line),
                (&sa.open[0].plan, &sa.open[0].line)
            );
            pair.stop().await;
        }

        /// A device granted a token that could not reach SA with it plays
        /// the item through SB: one play, as SA counts it, not two on SB.
        #[tokio::test]
        async fn a_device_s_fallback_to_sb_is_counted_as_one_play() {
            let pair = Pair::start("fed-media-proxy-fallback").await;
            shared(&pair).await;
            let (device, bob) = on_sb(&pair, "bob").await;
            let alien = pair.fed(movie().id());
            let granted = device
                .call(
                    Some(&bob),
                    Request::RemoteGrant {
                        id: alien.clone(),
                        download: None,
                    },
                )
                .await;
            assert!(
                matches!(granted, Response::Ok(Reply::RemoteGranted(_))),
                "{granted:?}"
            );
            proxied(&pair, &device, &bob).await;
            until(|| async {
                pair.sa
                    .store
                    .federation_plays(&pair.sb_on_sa, PlaySide::Served)
                    .await
                    .unwrap()
                    .plays
                    == 1
            })
            .await;
            let asserted = pair
                .sb
                .store
                .federation_plays(&pair.sa_on_sb, PlaySide::Asserted)
                .await
                .unwrap();
            assert_eq!(asserted.plays, 1);
            pair.stop().await;
        }

        /// An HTTP player (a cast target) cannot present a token to SA: its
        /// play is SB's, through SB, and the bytes it fetches from SB's
        /// HTTP port are SA's file's.
        #[tokio::test]
        async fn an_http_player_plays_sa_s_film_through_sb() {
            use spoolframe_protocol::experimental::HTTP_PLAYBACK;
            let pair = Pair::start_with(
                "fed-media-proxy-http",
                ServerOptions {
                    http_playback: spoolframe_server::http_playback::Settings {
                        listen: Some("0.0.0.0:0".parse().unwrap()),
                        ..Default::default()
                    },
                    ..ServerOptions::default()
                },
            )
            .await;
            let (file, _) = shared(&pair).await;
            let device = pair.sb.loopback_client().await;
            device
                .hello_naming(mkv_capabilities(), &[FEDERATION, HTTP_PLAYBACK])
                .await;
            let bob = device.login("bob", "correct horse").await;
            let handle = proxied(&pair, &device, &bob).await;
            let url = match device
                .call(
                    Some(&bob),
                    Request::HttpPlaybackUrl {
                        handle: handle.clone(),
                    },
                )
                .await
            {
                Response::Ok(Reply::HttpPlaybackUrl { url }) => url,
                other => panic!("no URL: {other:?}"),
            };
            let response = reqwest::Client::new()
                .get(&url)
                .header("range", "bytes=1000-5095")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), 206);
            assert_eq!(response.bytes().await.unwrap(), file[1000..5096]);
            pair.stop().await;
        }
    }

    mod downloads;
    mod lying;
    mod stuck;
    mod subtitles;
}
