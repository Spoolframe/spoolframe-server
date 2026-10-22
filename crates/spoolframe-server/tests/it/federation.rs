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
