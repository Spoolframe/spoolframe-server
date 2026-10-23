//! Federation (P8-01): two servers in this process pair over
//! `spoolframe/federation/1`, one redeeming the other's `federation`
//! invite; and what a stranger, a changed server id and a new key get.
//! Experimental: a server with `experimental.federation` off closes the
//! ALPN at once and makes no `federation` invite.

use std::sync::Arc;

use iroh::endpoint::{ConnectionError, presets};
use iroh::{Endpoint, RelayMode};
use spoolframe_federation::{DialError, Dialed};
use spoolframe_protocol::federation::{
    PROTOCOL_VERSION, PairingRefused, PeerHello, PeerRequest, RedeemPairing, Standing, Unpaired,
};
use spoolframe_protocol::invite::InviteLinkKind;
use spoolframe_protocol::{CLIENT_ALPN, Error, FEDERATION_ALPN, RedeemInvite, Request, Response};
use spoolframe_protocol::{close, fingerprint};
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
    // The pair's own user, for a shared film: a token, and a play.
    dialed
        .assert_user(assertion(bob, "plex:movie:1"))
        .await
        .unwrap();
    let counted = sa
        .store
        .federation_plays(&peer.id, PlaySide::Served)
        .await
        .unwrap();
    assert_eq!((counted.plays, counted.refused), (1, 5));
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

    /// The QR check (2026-10-22): looking SA's invite up on SA tells its
    /// kind and SA's fingerprint before SB redeems it.
    #[tokio::test]
    async fn a_federation_invite_s_lookup_tells_the_key_it_pairs_with() {
        use spoolframe_protocol::invite_admin::InviteKind as WireKind;
        use spoolframe_protocol::{InviteLookupOutcome, LookupInvite, Reply};
        let sa = Site::start("fed-admin-lookup", "Alice's server", "alice", true).await;
        let link = sa.invite().await;
        let device = sa.server.client().await;
        device
            .hello_listing(&[spoolframe_protocol::features::INVITE_LOOKUP])
            .await;
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
        // SB's admin syncs what SA shares now (P8-02).
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
    /// consumer is told no; listing needs neither recency nor change.
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

    /// An app on loopback that asks for `experimental` (and lists invite
    /// lookup), signed in to `server` as `username`.
    pub(super) async fn signed_in(
        server: &TestServer,
        username: &str,
        experimental: Experimental,
    ) -> (Client, Session) {
        let options = ClientOptions {
            lan_discovery: false,
            experimental,
            features: spoolframe_client_sdk::Features {
                invite_lookup: true,
                ..Default::default()
            },
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

    async fn sign_in(client: &Client, server: &TestServer, username: &str) -> Session {
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
        federation(&sa).set_walk_reuse(std::time::Duration::ZERO);
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
        federation(&sa).set_walk_reuse(spoolframe_server::federation::WALK_REUSE);
        let alien = id(MediaKind::Movie, "1");
        sa.catalog.not_yet(&alien);
        share(&sa, &peer, "plex:1").await;
        let (_, first, items, _) = sync(&dialed, "plex:1", None).await.unwrap();
        assert!(items.is_empty());
        sa.catalog.gain(&alien);
        let (_, cursor, items, _) = sync(&dialed, "plex:1", Some(&first)).await.unwrap();
        assert_eq!((cursor.as_str(), items.len()), (first.as_str(), 0));
        federation(&sa).set_walk_reuse(std::time::Duration::ZERO);
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
        let plex = PlexCatalog::new(quiet_plex().await.parse().unwrap(), None).unwrap();
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
            let sa = server(&format!("{test}-sa"), "Alice's server").await;
            federation(&sa).set_walk_reuse(std::time::Duration::ZERO);
            let (sb, cache) = sb_with(fresh_dir(&format!("{test}-sb")).await, options).await;
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
            // A setting applied wakes the replica before its tick.
            federation(&self.sb).set_replica(
                std::time::Duration::from_secs(3600),
                spoolframe_server::store::federation_cache::FileLimits {
                    per_origin: 1 << 30,
                    total: 1 << 30,
                },
            );
            let files = self.files();
            until(|| {
                let files = files.clone();
                async move { kept(&files) == count }
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

    /// The ids a search for `query` finds, its top match first.
    async fn found(app: &TestClient, bob: &SessionId, query: &str) -> Vec<MediaId> {
        let results = match app
            .call(Some(bob), Request::Search(crate::support::search(query)))
            .await
        {
            Response::Ok(Reply::Search(results)) => results,
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

    /// A user holds at most TOKEN_LIMITS.per_user play tokens of SA's at
    /// once (H3): past them, SA says to try later, and mints again once
    /// one expires.
    #[tokio::test]
    async fn a_user_s_play_tokens_are_capped() {
        use spoolframe_protocol::federation::AssertionRefused;
        use spoolframe_server::federation::{AssertError, PLAY_TOKEN_LIFETIME, TOKEN_LIMITS};
        let pair = Pair::start("fed-assert-flood").await;
        pair.share("plex:1").await;
        pair.refresh().await;
        pair.sb.add_user("bob", "correct horse", None).await;
        let bob = pair.sb.store.user_by_name("bob").await.unwrap().unwrap().id;
        let device = iroh::SecretKey::from_bytes(&[3; 32]).public();
        let alien = pair.fed(movie().id());
        let sb = federation(&pair.sb);
        for _ in 0..TOKEN_LIMITS.per_user {
            sb.assert_play(bob, "Bob", &alien, device).await.unwrap();
        }
        assert!(matches!(
            sb.assert_play(bob, "Bob", &alien, device).await,
            Err(AssertError::Dial(DialError::NotGranted(
                AssertionRefused::Unavailable
            )))
        ));
        pair.sa.store.clock().advance(PLAY_TOKEN_LIFETIME);
        sb.assert_play(bob, "Bob", &alien, device).await.unwrap();
        pair.stop().await;
    }

    /// P8-03, across two servers: SB vouches for Bob, who wants SA's film
    /// on a device of his; SA mints a token for that film and device,
    /// kept hashed under Bob's remote principal, and both count the play.
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
            .assert_play(bob, "Bob", &alien, device)
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
        assert_eq!(served(PlaySide::Served).await, (1, 0));
        assert_eq!(served(PlaySide::Asserted).await, (1, 0));

        // Music is not shared; and an artist does not play (M6).
        let refused = |item: MediaId| {
            let federation = federation(&pair.sb);
            async move {
                match federation.assert_play(bob, "Bob", &item, device).await {
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
        assert_eq!(served(PlaySide::Served).await, (1, 2));
        assert_eq!(served(PlaySide::Asserted).await, (1, 2));

        federation(&pair.sa)
            .unshare(&pair.sb_on_sa, "plex:1", "alice")
            .await
            .unwrap();
        assert_eq!(pair.sa.store.federation_token(&hash).await.unwrap(), None);
        assert!(matches!(
            federation(&pair.sb)
                .assert_play(bob, "Bob", &pair.fed(movie().id()), device)
                .await,
            Err(AssertError::Dial(DialError::NotGranted(
                AssertionRefused::NotShared
            )))
        ));
        assert!(matches!(
            federation(&pair.sb)
                .assert_play(bob, "Bob", movie().id(), device)
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
        sb.assert_play(bob, "Bob", &pair.fed(movie().id()), device)
            .await
            .unwrap();
        // An artist does not play: refused, and counted on both sides.
        let artist = pair.fed(&MediaId::new(Namespace::Plex, MediaKind::Artist, "30").unwrap());
        assert!(sb.assert_play(bob, "Bob", &artist, device).await.is_err());
        let counted = |federation: &Arc<Federation>, peer: String| {
            let federation = federation.clone();
            async move {
                let (here, there) = federation.play_counts(&peer).await.unwrap();
                (here.plays, there.plays, here.refused, there.refused)
            }
        };
        assert_eq!(counted(sb, pair.sa_on_sb.clone()).await, (0, 1, 0, 1));
        assert_eq!(
            counted(federation(&pair.sa), pair.sb_on_sa.clone()).await,
            (1, 0, 1, 0)
        );
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
            features: vec![spoolframe_protocol::features::PLAY_HISTORY.into()],
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
                .assert_play(bob, "Bob", &alien, device)
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
}
