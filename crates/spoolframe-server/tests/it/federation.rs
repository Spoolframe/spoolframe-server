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
