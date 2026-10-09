//! Invites (P6-01): an admin makes one over the admin API, and a device
//! with nothing but its link redeems it over the client protocol, making
//! its own account. The server here is off the LAN; its link's addresses
//! are its own, as `network.announce` tells them.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use jsonrpsee::ws_client::{HeaderMap, WsClient, WsClientBuilder};
use spoolframe_admin_api::{
    AdminApiClient, InviteCreated, InviteKind, MeApiClient, NewInvite, Role, error_code, invite,
};
use spoolframe_protocol::invite::{normalize_code, parse_link};
use spoolframe_protocol::rpc::RpcRequest;
use spoolframe_protocol::{Error, RedeemInvite, Reply, Request, RequestEnvelope, Response};
use spoolframe_server::admin::{Admin, Origins, StatusSource, TrustedProxies};
use spoolframe_server::auth::{Bucket, hash_password};

use crate::admin::{anonymous, code, creds, with_token};
use crate::support::{
    self, LogCapture, ServerOptions, TestClient, TestServer, alone, mkv_capabilities, movie,
};

/// Where browsers reach the admin API beside the server.
const PUBLIC_URL: &str = "https://media.example.org";

/// A server with root as its admin, the admin API beside it over the same
/// store, and root signed in to it just now.
struct Invites {
    server: TestServer,
    addr: SocketAddr,
    root: WsClient,
    root_token: String,
}

impl Invites {
    async fn start(test: &str) -> Self {
        Self::start_with(test, ServerOptions::default()).await
    }

    async fn start_with(test: &str, options: ServerOptions) -> Self {
        let server = TestServer::start_with(support::fresh_dir(test).await, options).await;
        let phc = hash_password("correct horse").await.unwrap();
        server
            .store
            .add_user("root", None, &phc, Role::Admin)
            .await
            .unwrap();
        let status = StatusSource::new(
            server.name.clone(),
            server.server_id.clone(),
            support::transcoding(),
        );
        status.set_endpoint_id(server.server.id().to_string());
        status.set_told(server.server.told());
        let origins = Origins::Exactly(PUBLIC_URL.to_owned());
        let (addr, handle) = spoolframe_server::admin::serve(
            "127.0.0.1:0".parse().unwrap(),
            Admin {
                status: Arc::new(status),
                ..crate::admin::admin(server.auth.clone(), None)
            },
            origins,
            TrustedProxies::default(),
        )
        .await
        .unwrap();
        std::mem::forget(handle);
        let root_token = anonymous(addr)
            .await
            .login(creds("root", "correct horse"))
            .await
            .unwrap()
            .token;
        let root = with_token(addr, &root_token).await;
        Self {
            server,
            addr,
            root,
            root_token,
        }
    }

    async fn create(&self, invite: NewInvite) -> InviteCreated {
        self.root.invite_create(invite).await.unwrap()
    }

    /// A device the server has never seen, past `Hello`, able to play the
    /// stand-in MKV.
    async fn stranger(&self) -> TestClient {
        let device = self.server.client().await;
        device.hello_with(None, Some(mkv_capabilities())).await;
        device
    }
}

/// The default invite: one use, seven days, a consumer.
fn create_user() -> NewInvite {
    NewInvite {
        kind: InviteKind::CreateUser,
        note: Some("Mum's iPad".into()),
        uses: None,
        expires: None,
        role: None,
    }
}

fn redeem(code: &str, username: &str) -> Request {
    Request::RedeemInvite(RedeemInvite {
        code: code.into(),
        username: username.into(),
        display_name: Some("Mum".into()),
        password: "correct horse battery".into(),
    })
}

fn logged_in(response: Response) -> spoolframe_protocol::SessionId {
    match response {
        Response::Ok(Reply::LoggedIn {
            session,
            remembered: false,
        }) => session,
        other => panic!("not signed in: {other:?}"),
    }
}

/// A code no invite has.
const WRONG: &str = "AAAA-BBBB-CCCC";

#[tokio::test]
async fn an_invite_makes_an_account_that_signs_in_and_plays() {
    let invites = Invites::start("invite-plays").await;
    let file = invites.server.write_media(1 << 20);
    let created = invites.create(create_user()).await;
    let device = invites.stranger().await;
    let session = logged_in(device.call(None, redeem(&created.code, "Mum")).await);

    let Response::Ok(Reply::WhoAmI(me)) = device.call(Some(&session), Request::WhoAmI).await else {
        panic!("no WhoAmI");
    };
    assert_eq!(
        (me.username.as_str(), me.display_name.as_str(), me.role),
        ("mum", "Mum", Role::Consumer)
    );
    let started = device
        .call(
            Some(&session),
            Request::StartPlayback {
                id: movie().id().clone(),
                selection: None,
                start_ms: None,
                bitmap: Default::default(),
                quality: Default::default(),
            },
        )
        .await;
    let Response::Ok(Reply::PlaybackStarted { handle, .. }) = started else {
        panic!("playback did not start: {started:?}");
    };
    let (_, bytes) = device.read(&session, &handle, 0, 4096).await;
    assert_eq!(bytes, file[..4096]);

    // Bound to the device that redeemed it (P1-12).
    let other = invites.stranger().await;
    assert_eq!(
        other.call(Some(&session), Request::WhoAmI).await,
        Response::Err(Error::Unauthenticated)
    );
    // The invite had one use.
    assert!(invites.root.invite_list().await.unwrap().is_empty());
    invites.server.stop().await;
}

#[tokio::test]
async fn the_account_has_the_invite_s_role() {
    let invites = Invites::start("invite-role").await;
    let created = invites
        .create(NewInvite {
            role: Some(Role::Admin),
            ..create_user()
        })
        .await;
    let device = invites.stranger().await;
    let session = logged_in(device.call(None, redeem(&created.code, "cabin")).await);
    let Response::Ok(Reply::WhoAmI(me)) = device.call(Some(&session), Request::WhoAmI).await else {
        panic!("no WhoAmI");
    };
    assert_eq!(me.role, Role::Admin);
    invites.server.stop().await;
}

#[tokio::test]
async fn the_link_carries_the_key_the_hints_and_the_code_and_the_list_never_the_code() {
    let invites = Invites::start("invite-link").await;
    let created = invites
        .create(NewInvite {
            uses: Some(4),
            ..create_user()
        })
        .await;
    let link = parse_link(&created.link).expect("an invite link");
    assert_eq!(link.server, invites.server.server.id().to_string());
    assert_eq!(link.name.as_deref(), Some("Home Media"));
    let told = invites.server.server.told();
    let addrs = tokio::task::spawn_blocking(move || told.addrs())
        .await
        .unwrap();
    assert_eq!(link.addrs, addrs);
    assert!(link.relays.is_empty(), "{:?}", link.relays);
    assert_eq!(link.code, normalize_code(&created.code));
    assert_eq!(link.role, Some(Role::Consumer));
    let expires_at = u64::try_from(created.invite.expires_at).unwrap();
    assert_eq!(link.expires_at, Some(expires_at));
    let now = invites.server.clock.unix_secs();
    assert!(
        (created.invite.expires_at - now - 7 * 24 * 3600).abs() <= 5,
        "{created:?}"
    );
    assert_eq!(created.code.len(), 14, "grouped in fours: {created:?}");

    let listed = invites.root.invite_list().await.unwrap();
    assert_eq!(listed, std::slice::from_ref(&created.invite));
    assert_eq!(
        (
            listed[0].uses,
            listed[0].uses_left,
            listed[0].note.as_deref()
        ),
        (4, 4, Some("Mum's iPad"))
    );
    let json = serde_json::to_string(&listed).unwrap();
    let bare = normalize_code(&created.code).unwrap();
    assert!(
        !json.contains(&created.code) && !json.contains(&bare),
        "{json}"
    );
    invites.server.stop().await;
}

#[tokio::test]
async fn the_server_link_finds_the_server_and_has_no_code() {
    let invites = Invites::start("invite-server-link").await;
    let link = parse_link(&invites.root.invite_link().await.unwrap()).expect("an invite link");
    assert_eq!(link.server, invites.server.server.id().to_string());
    assert_eq!(link.name.as_deref(), Some("Home Media"));
    assert_eq!((link.code, link.expires_at, link.role), (None, None, None));
    invites.server.stop().await;
}

/// A-13: each wrong code is counted before it is checked, and after the
/// limit even the right one is refused from that device. Another device
/// is not held to it.
#[tokio::test]
async fn wrong_codes_count_as_guesses_and_are_limited() {
    let invites = Invites::start("invite-guesses").await;
    let created = invites.create(create_user()).await;
    let guesser = invites.stranger().await;
    for wrong in [WRONG, "not a code", WRONG, "QQQQ-QQQQ-QQQ", WRONG] {
        assert_eq!(
            guesser.call(None, redeem(wrong, "mallory")).await,
            Response::Err(Error::InviteInvalid),
            "{wrong}"
        );
    }
    assert!(
        matches!(
            guesser.call(None, redeem(&created.code, "mallory")).await,
            Response::Err(Error::RateLimited { .. })
        ),
        "a sixth guess was checked"
    );
    let device = invites.stranger().await;
    logged_in(device.call(None, redeem(&created.code, "mum")).await);
    invites.server.stop().await;
}

/// Redemptions are sign-ins strangers make: they share the strangers'
/// one budget (PX-10), whatever device they come from.
#[tokio::test]
async fn strangers_redemptions_draw_on_the_shared_budget() {
    let invites = Invites::start_with(
        "invite-bucket",
        ServerOptions {
            stranger_attempts: Bucket {
                capacity: 2,
                refill_every: Duration::from_secs(3600),
            },
            ..ServerOptions::default()
        },
    )
    .await;
    let created = invites.create(create_user()).await;
    for _ in 0..2 {
        let stranger = invites.stranger().await;
        assert_eq!(
            stranger.call(None, redeem(WRONG, "mallory")).await,
            Response::Err(Error::InviteInvalid)
        );
    }
    let fresh = invites.stranger().await;
    assert!(
        matches!(
            fresh.call(None, redeem(&created.code, "mum")).await,
            Response::Err(Error::RateLimited { .. })
        ),
        "a third stranger's redemption was let through"
    );
    assert_eq!(invites.server.store.user_count().await.unwrap(), 1);
    invites.server.stop().await;
}

/// Finding 48: a refused redemption changed nothing, so it is not kept.
/// The same operation sent again runs again: kept, it would be refused as
/// a conflict, its arguments being others.
#[tokio::test]
async fn a_refused_redemption_is_not_kept_as_an_operation() {
    let invites = Invites::start("invite-not-kept").await;
    let created = invites.create(create_user()).await;
    let device = invites.stranger().await;
    let operation = device.operation();
    let send = |request: Request| {
        let device = device.clone();
        let operation = operation.clone();
        async move {
            device
                .wire(&RpcRequest {
                    envelope: RequestEnvelope {
                        session: None,
                        request,
                    },
                    operation: Some(operation),
                    expected_revision: None,
                })
                .await
                .reply
                .response
        }
    };
    for refused in [
        redeem(WRONG, "mum"),
        redeem(&created.code, "root"),
        redeem(&created.code, "not a name!"),
    ] {
        assert!(
            matches!(
                send(refused).await,
                Response::Err(
                    Error::InviteInvalid | Error::UsernameTaken | Error::InvalidInput { .. }
                )
            ),
            "not refused"
        );
    }
    logged_in(send(redeem(&created.code, "mum")).await);
    invites.server.stop().await;
}

/// The ticket's race, over the wire: two devices redeem the last use at
/// once, and exactly one gets an account.
#[tokio::test]
async fn two_devices_redeeming_the_last_use_at_once_make_one_user() {
    let invites = Invites::start("invite-race").await;
    let created = invites.create(create_user()).await;
    let (a, b) = (invites.stranger().await, invites.stranger().await);
    let (first, second) = tokio::join!(
        a.call(None, redeem(&created.code, "anna")),
        b.call(None, redeem(&created.code, "bob"))
    );
    let signed_in = [&first, &second]
        .iter()
        .filter(|r| matches!(r, Response::Ok(Reply::LoggedIn { .. })))
        .count();
    assert_eq!(signed_in, 1, "{first:?} {second:?}");
    assert!(
        [&first, &second].contains(&&Response::Err(Error::InviteExhausted)),
        "{first:?} {second:?}"
    );
    // Root and one of them.
    assert_eq!(invites.server.store.user_count().await.unwrap(), 2);
    invites.server.stop().await;
}

#[tokio::test]
async fn an_expired_invite_answers_invite_expired() {
    let invites = Invites::start("invite-expired").await;
    let created = invites
        .create(NewInvite {
            expires: Some(invite::MIN_EXPIRES_SECS),
            ..create_user()
        })
        .await;
    invites.server.clock.advance(Duration::from_secs(3601));
    let device = invites.stranger().await;
    assert_eq!(
        device.call(None, redeem(&created.code, "mum")).await,
        Response::Err(Error::InviteExpired)
    );
    assert_eq!(invites.server.store.user_count().await.unwrap(), 1);
    invites.server.stop().await;
}

#[tokio::test]
async fn a_revoked_invite_answers_invite_invalid_and_the_list_drops_it() {
    let invites = Invites::start("invite-revoked").await;
    let created = invites.create(create_user()).await;
    invites
        .root
        .invite_revoke(created.invite.id.clone())
        .await
        .unwrap();
    assert!(invites.root.invite_list().await.unwrap().is_empty());
    let device = invites.stranger().await;
    assert_eq!(
        device.call(None, redeem(&created.code, "mum")).await,
        Response::Err(Error::InviteInvalid)
    );
    let again = invites
        .root
        .invite_revoke(created.invite.id.clone())
        .await
        .unwrap_err();
    assert_eq!(code(again), error_code::NO_SUCH_INVITE);
    invites.server.stop().await;
}

#[tokio::test]
async fn a_used_up_invite_answers_invite_exhausted() {
    let invites = Invites::start("invite-used-up").await;
    let created = invites.create(create_user()).await;
    logged_in(
        invites
            .stranger()
            .await
            .call(None, redeem(&created.code, "anna"))
            .await,
    );
    assert_eq!(
        invites
            .stranger()
            .await
            .call(None, redeem(&created.code, "bob"))
            .await,
        Response::Err(Error::InviteExhausted)
    );
    invites.server.stop().await;
}

/// Picking a name someone has, or one that cannot be a username, uses
/// nothing up: the same invite then makes the account.
#[tokio::test]
async fn a_taken_or_invalid_username_keeps_the_invite() {
    let invites = Invites::start("invite-taken").await;
    let created = invites.create(create_user()).await;
    let device = invites.stranger().await;
    assert_eq!(
        device.call(None, redeem(&created.code, "Root")).await,
        Response::Err(Error::UsernameTaken)
    );
    assert!(matches!(
        device
            .call(None, redeem(&created.code, "not a name!"))
            .await,
        Response::Err(Error::InvalidInput { .. })
    ));
    let short = Request::RedeemInvite(RedeemInvite {
        code: created.code.clone(),
        username: "mum".into(),
        display_name: None,
        password: "short".into(),
    });
    assert!(matches!(
        device.call(None, short).await,
        Response::Err(Error::InvalidInput { .. })
    ));
    logged_in(device.call(None, redeem(&created.code, "mum")).await);
    invites.server.stop().await;
}

/// What makes an account, or stops a code, needs a recent sign-in; seeing
/// the list and the server's own link does not.
#[tokio::test]
async fn making_or_revoking_an_invite_needs_a_recent_sign_in() {
    let invites = Invites::start("invite-recent").await;
    let created = invites.create(create_user()).await;
    crate::recent_admin::age(invites.server.dir.as_ref(), "root").await;
    let refused = invites.root.invite_create(create_user()).await.unwrap_err();
    assert_eq!(code(refused), error_code::RECENT_SIGN_IN_REQUIRED);
    let refused = invites
        .root
        .invite_revoke(created.invite.id.clone())
        .await
        .unwrap_err();
    assert_eq!(code(refused), error_code::RECENT_SIGN_IN_REQUIRED);
    assert_eq!(invites.root.invite_list().await.unwrap(), [created.invite]);
    invites.root.invite_link().await.unwrap();
    invites.server.stop().await;
}

#[tokio::test]
async fn a_consumer_sees_no_invites() {
    let invites = Invites::start("invite-consumer").await;
    invites
        .server
        .add_user("alice", "battery staple", None)
        .await;
    let token = anonymous(invites.addr)
        .await
        .login(creds("alice", "battery staple"))
        .await
        .unwrap()
        .token;
    let alice = with_token(invites.addr, &token).await;
    for refused in [
        alice.invite_create(create_user()).await.map(|_| ()),
        alice.invite_list().await.map(|_| ()),
        alice.invite_link().await.map(|_| ()),
        alice.invite_revoke("x".into()).await,
    ] {
        assert_eq!(code(refused.unwrap_err()), error_code::FORBIDDEN);
    }
    invites.server.stop().await;
}

/// `portable-account` and `federation` are parsed and refused, saying
/// when they arrive: nothing is made, so nothing can be half-redeemed or
/// made a `create-user` invite instead.
#[tokio::test]
async fn the_kinds_of_later_phases_are_refused_and_nothing_is_made() {
    let invites = Invites::start("invite-kinds").await;
    for kind in [InviteKind::PortableAccount, InviteKind::Federation] {
        let refused = invites
            .root
            .invite_create(NewInvite {
                kind,
                ..create_user()
            })
            .await
            .unwrap_err();
        let jsonrpsee::core::ClientError::Call(refused) = refused else {
            panic!("not a call error");
        };
        assert_eq!(refused.code(), error_code::INVALID_INPUT);
        assert!(
            refused.message().contains("P7") && refused.message().contains("P8"),
            "{}",
            refused.message()
        );
    }
    assert!(invites.root.invite_list().await.unwrap().is_empty());
    invites.server.stop().await;
}

#[tokio::test]
async fn out_of_range_invites_are_refused() {
    let invites = Invites::start("invite-ranges").await;
    for wrong in [
        NewInvite {
            uses: Some(0),
            ..create_user()
        },
        NewInvite {
            uses: Some(invite::MAX_USES + 1),
            ..create_user()
        },
        NewInvite {
            expires: Some(invite::MIN_EXPIRES_SECS - 1),
            ..create_user()
        },
        NewInvite {
            expires: Some(invite::MAX_EXPIRES_SECS + 1),
            ..create_user()
        },
        NewInvite {
            note: Some("x".repeat(invite::MAX_NOTE_CHARS + 1)),
            ..create_user()
        },
        NewInvite {
            role: Some(Role::Unknown),
            ..create_user()
        },
    ] {
        let refused = invites.root.invite_create(wrong.clone()).await.unwrap_err();
        assert_eq!(code(refused), error_code::INVALID_INPUT, "{wrong:?}");
    }
    assert!(invites.root.invite_list().await.unwrap().is_empty());
    invites.server.stop().await;
}

/// The design's "Made by": the admin for one made in the web UI, "server
/// CLI" for one made by a caller with no page (the CLI).
#[tokio::test]
async fn made_by_is_the_admin_from_a_page_and_server_cli_otherwise() {
    let invites = Invites::start("invite-made-by").await;
    let from_cli = invites.create(create_user()).await;
    assert_eq!(from_cli.invite.created_by, invite::MADE_BY_CLI);
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {}", invites.root_token).parse().unwrap(),
    );
    headers.insert("origin", PUBLIC_URL.parse().unwrap());
    let page = WsClientBuilder::default()
        .set_headers(headers)
        .build(format!("ws://{}", invites.addr))
        .await
        .unwrap();
    let from_page = page.invite_create(create_user()).await.unwrap();
    assert_eq!(from_page.invite.created_by, "root");
    invites.server.stop().await;
}

/// A refused redemption is logged as a refused sign-in, and neither the
/// code nor the password is in the log.
#[tokio::test]
async fn a_refused_redemption_is_logged_without_its_secrets() {
    if !alone("invites::a_refused_redemption_is_logged_without_its_secrets") {
        return;
    }
    let invites = Invites::start("invite-logged").await;
    let device = invites.stranger().await;
    let (capture, _guard) = LogCapture::start();
    assert_eq!(
        device.call(None, redeem(WRONG, "mallory")).await,
        Response::Err(Error::InviteInvalid)
    );
    let lines = capture.refused();
    assert_eq!(lines.len(), 1, "{lines:#?}");
    for (name, value) in [
        ("kind", "sign_in_failed"),
        ("channel", "quic"),
        ("request", "redeem_invite"),
        ("reason", "wrong or revoked invite code"),
    ] {
        assert_eq!(lines[0].field(name), Some(value), "{name} in {lines:#?}");
    }
    let text = capture.text();
    assert!(!text.contains("AAAA"), "the code was logged: {text}");
    assert!(
        !text.contains("correct horse battery"),
        "the password was logged: {text}"
    );
    invites.server.stop().await;
}

// The SDK (P6-01): a device that has nothing but the link. Its server
// tells loopback besides its LAN addresses, so the link reaches it on a
// machine with no network.

mod sdk {
    use std::sync::Arc;
    use std::time::{Duration, UNIX_EPOCH};

    use spoolframe_admin_api::{AdminApiClient, NewInvite, Role};
    use spoolframe_client_sdk::{Client, Device, MemoryStorage, SdkError, Selection};
    use spoolframe_server::announce::{Added, Announce};

    use super::{Invites, create_user};
    use crate::support::{self, ServerOptions, mkv_capabilities, movie};

    fn device() -> Device {
        Device {
            name: "Mum's iPad".into(),
            client_name: "sdk-test".into(),
            client_version: "0".into(),
            platform: spoolframe_client_sdk::types::Platform::Ios,
            capabilities: mkv_capabilities(),
        }
    }

    /// A device that has never seen a server: no LAN discovery, no
    /// address given.
    async fn new_device() -> Client {
        support::sdk_client(Arc::new(MemoryStorage::default()), device())
            .await
            .unwrap()
    }

    fn reached_on_loopback() -> ServerOptions {
        ServerOptions {
            announce: Announce {
                add: vec![Added::Ip([127, 0, 0, 1].into())],
                ..Announce::default()
            },
            ..ServerOptions::default()
        }
    }

    async fn invites(test: &str) -> Invites {
        Invites::start_with(test, reached_on_loopback()).await
    }

    const PASSWORD: &str = "correct horse battery";

    /// The ticket's first check: a client with no prior knowledge redeems
    /// over the network, makes its user, and plays.
    #[tokio::test]
    async fn a_device_with_nothing_but_the_link_joins_and_plays() {
        let invites = invites("sdk-invite-plays").await;
        let file = invites.server.write_media(1 << 20);
        let created = invites.create(create_user()).await;
        let id = invites.server.server.id().to_string();
        let client = new_device().await;

        let info = client.read_invite(&created.link).await.unwrap();
        assert_eq!(info.endpoint_id, id);
        assert_eq!(
            info.fingerprint,
            spoolframe_protocol::fingerprint::of_hex(&id).unwrap()
        );
        assert_eq!(info.name.as_deref(), Some("Home Media"));
        assert!(info.has_code && !info.saved, "{info:?}");
        assert_eq!(info.role, Some(Role::Consumer));
        let expires = UNIX_EPOCH + Duration::from_secs(created.invite.expires_at as u64);
        assert_eq!(info.expires_at, Some(expires));

        // "Is this the server you were invited to?": who answers, before
        // anything is saved.
        let entry = client.connect_invite(&created.link).await.unwrap();
        assert_eq!(entry.name, "Home Media");
        assert_eq!(
            entry.server_id.as_deref(),
            Some(invites.server.server_id.to_string().as_str())
        );
        assert!(!entry.pinned, "{entry:?}");

        let session = client
            .redeem_invite(&created.link, "mum", Some("Mum"), PASSWORD)
            .await
            .unwrap();
        assert_eq!(session.user().username, "mum");
        assert_eq!(session.user().display_name, "Mum");

        // Pinned from the link, no trust on first use: the key and the
        // server id it answered with.
        let saved = client.servers().await.unwrap();
        let entry = saved.iter().find(|s| s.endpoint_id == id).unwrap();
        assert!(entry.pinned && entry.verified, "{entry:?}");
        assert_eq!(
            entry.server_id.as_deref(),
            Some(invites.server.server_id.to_string().as_str())
        );

        let playback = session
            .start_playback(movie().id(), Some(Selection::default()))
            .await
            .unwrap();
        let bytes = session.read(&playback.handle, 0, 4096).await.unwrap();
        assert_eq!(bytes, file[..4096]);

        // The user is kept: the app opened again finds the profile.
        assert_eq!(client.profiles(&id).await.unwrap().len(), 1);
        client.close().await;
        invites.server.stop().await;
    }

    /// A link without a code only finds and saves the server; the user
    /// signs in with an account they have.
    #[tokio::test]
    async fn a_link_without_a_code_saves_the_server_to_sign_in_to() {
        let invites = invites("sdk-invite-no-code").await;
        invites
            .server
            .add_user("alice", "battery staple", None)
            .await;
        let link = invites.root.invite_link().await.unwrap();
        let client = new_device().await;
        let info = client.read_invite(&link).await.unwrap();
        assert!(!info.has_code, "{info:?}");
        let entry = client.save_invite_server(&link).await.unwrap();
        assert!(entry.pinned && entry.verified, "{entry:?}");
        assert!(matches!(
            client.redeem_invite(&link, "mum", None, PASSWORD).await,
            Err(SdkError::InvalidInput(_))
        ));
        let alice = client
            .sign_in(&entry.endpoint_id, "alice", "battery staple", false)
            .await
            .unwrap();
        assert_eq!(alice.user().username, "alice");
        client.close().await;
        invites.server.stop().await;
    }

    /// Refused, nothing is saved; "Save server only" then pins it.
    #[tokio::test]
    async fn a_refused_redemption_saves_nothing_until_save_server_only() {
        let invites = invites("sdk-invite-refused").await;
        let created = invites.create(create_user()).await;
        invites
            .root
            .invite_revoke(created.invite.id.clone())
            .await
            .unwrap();
        let client = new_device().await;
        assert_eq!(
            client
                .redeem_invite(&created.link, "mum", None, PASSWORD)
                .await
                .unwrap_err(),
            SdkError::InviteInvalid
        );
        assert!(client.servers().await.unwrap().iter().all(|s| !s.pinned));
        let entry = client.save_invite_server(&created.link).await.unwrap();
        assert!(entry.pinned, "{entry:?}");
        let info = client.read_invite(&created.link).await.unwrap();
        assert!(info.saved, "{info:?}");
        client.close().await;
        invites.server.stop().await;
    }

    #[tokio::test]
    async fn each_refusal_is_its_own_error() {
        let invites = invites("sdk-invite-errors").await;
        let used = invites.create(create_user()).await;
        let client = new_device().await;
        client
            .redeem_invite(&used.link, "anna", None, PASSWORD)
            .await
            .unwrap();
        let other = new_device().await;
        assert_eq!(
            other
                .redeem_invite(&used.link, "bob", None, PASSWORD)
                .await
                .unwrap_err(),
            SdkError::InviteExhausted
        );
        let fresh = invites
            .create(NewInvite {
                uses: Some(2),
                ..create_user()
            })
            .await;
        assert_eq!(
            other
                .redeem_invite(&fresh.link, "anna", None, PASSWORD)
                .await
                .unwrap_err(),
            SdkError::UsernameTaken
        );
        let short = invites
            .create(NewInvite {
                expires: Some(spoolframe_admin_api::invite::MIN_EXPIRES_SECS),
                ..create_user()
            })
            .await;
        invites.server.clock.advance(Duration::from_secs(3601));
        assert_eq!(
            other
                .redeem_invite(&short.link, "bob", None, PASSWORD)
                .await
                .unwrap_err(),
            SdkError::InviteExpired
        );
        assert!(matches!(
            other.read_invite("https://example.com/invite").await,
            Err(SdkError::NotAnInvite)
        ));
        client.close().await;
        other.close().await;
        invites.server.stop().await;
    }

    /// An invite for a server already saved adds its hints to the
    /// addresses known; nothing else of the pin changes.
    #[tokio::test]
    async fn an_invite_for_a_saved_server_adds_its_hints() {
        let invites = invites("sdk-invite-hints").await;
        let link = invites.root.invite_link().await.unwrap();
        let client = new_device().await;
        let first = client.save_invite_server(&link).await.unwrap();
        let mut hinted = spoolframe_protocol::invite::parse_link(&link).unwrap();
        let elsewhere: std::net::SocketAddr = "203.0.113.9:7420".parse().unwrap();
        hinted.addrs.push(elsewhere);
        let again = client.save_invite_server(&hinted.link()).await.unwrap();
        assert_eq!(again.pinned_at, first.pinned_at);
        assert_eq!(again.server_id, first.server_id);
        assert!(again.addrs.contains(&elsewhere), "{again:?}");
        let saved = client.servers().await.unwrap();
        assert!(saved.iter().any(|s| s.addrs.contains(&elsewhere)));
        client.close().await;
        invites.server.stop().await;
    }

    /// P1-13: another key under a saved server's name is another server,
    /// saved beside it, not a change of the first one's identity.
    #[tokio::test]
    async fn another_key_under_a_saved_name_is_another_server() {
        let first = invites("sdk-invite-first").await;
        let second = invites("sdk-invite-second").await;
        assert_eq!(first.server.name.get(), second.server.name.get());
        let client = new_device().await;
        client
            .save_invite_server(&first.root.invite_link().await.unwrap())
            .await
            .unwrap();
        client
            .save_invite_server(&second.root.invite_link().await.unwrap())
            .await
            .unwrap();
        let saved: Vec<_> = client
            .servers()
            .await
            .unwrap()
            .into_iter()
            .filter(|s| s.pinned)
            .collect();
        assert_eq!(saved.len(), 2, "{saved:?}");
        client.close().await;
        first.server.stop().await;
        second.server.stop().await;
    }
}
