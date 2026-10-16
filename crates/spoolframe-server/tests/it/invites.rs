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
use spoolframe_protocol::{
    Error, RedeemInvite, Reply, Request, RequestEnvelope, Response, WhoAmIReply,
};
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
            password_change_required: false,
            ..
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

    let Response::Ok(Reply::WhoAmI(WhoAmIReply { user: me, .. })) =
        device.call(Some(&session), Request::WhoAmI).await
    else {
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
                display: None,
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
    let Response::Ok(Reply::WhoAmI(WhoAmIReply { user: me, .. })) =
        device.call(Some(&session), Request::WhoAmI).await
    else {
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
    assert_eq!(parse_link(&created.share_link).as_ref(), Some(&link));
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
    let both = invites.root.invite_link().await.unwrap();
    let link = parse_link(&both.link).expect("an invite link");
    // PX-45: the https form, sent as text, is the same link.
    assert!(both.link.starts_with("spoolframe://invite?"), "{both:?}");
    assert!(
        both.share_link.starts_with("https://spoolframe.app/i#"),
        "{both:?}"
    );
    assert_eq!(parse_link(&both.share_link).as_ref(), Some(&link));
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

/// `federation` is parsed and refused, saying when it arrives: nothing
/// is made, so nothing can be half-redeemed or made another kind of
/// invite instead.
#[tokio::test]
async fn federation_invites_are_refused_and_nothing_is_made() {
    let invites = Invites::start("invite-kinds").await;
    let refused = invites
        .root
        .invite_create(NewInvite {
            kind: InviteKind::Federation,
            ..create_user()
        })
        .await
        .unwrap_err();
    let jsonrpsee::core::ClientError::Call(refused) = refused else {
        panic!("not a call error");
    };
    assert_eq!(refused.code(), error_code::INVALID_INPUT);
    assert!(refused.message().contains("P8"), "{}", refused.message());
    assert!(invites.root.invite_list().await.unwrap().is_empty());
    invites.server.stop().await;
}

// P7-05: portable-account invites, redeemed by a portable identity's
// device credential into a grant, never an account.

mod portable {
    use ed25519_dalek::{Signer, SigningKey};
    use spoolframe_protocol::invite::InviteLinkKind;
    use spoolframe_protocol::portable::{
        DevicePublicKey, MAX_CREDENTIAL_VALIDITY, PortableUserId, UserDeviceCredential,
        device_proof_message,
    };
    use spoolframe_protocol::{PortableSignIn, RedeemPortableInvite};

    use super::*;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn identity(seed: u8) -> PortableUserId {
        PortableUserId::from_bytes(key(seed).verifying_key().to_bytes()).unwrap()
    }

    fn guest_invite() -> NewInvite {
        NewInvite {
            kind: InviteKind::PortableAccount,
            note: Some("Sam".into()),
            uses: None,
            expires: None,
            role: None,
        }
    }

    /// Identity 7's sign-in from device 9 on `device`, over a nonce it
    /// begins now.
    async fn sign_in(invites: &Invites, device: &TestClient) -> PortableSignIn {
        let Response::Ok(Reply::PortableSignInBegun { nonce, .. }) =
            device.call(None, Request::BeginPortableSignIn).await
        else {
            panic!("not begun");
        };
        let now = invites.server.store.clock().unix_secs();
        PortableSignIn {
            credential: UserDeviceCredential::new(
                identity(7),
                DevicePublicKey::from_bytes(key(9).verifying_key().to_bytes()).unwrap(),
                1,
                now - 60,
                now - 60 + MAX_CREDENTIAL_VALIDITY,
                |m| key(7).sign(m),
            ),
            proof: key(9).sign(&device_proof_message(
                device.conn.remote_id().as_bytes(),
                &nonce,
            )),
            states: vec![],
            display_name: Some("Sam".into()),
        }
    }

    async fn redeem_as_guest(invites: &Invites, device: &TestClient, code: &str) -> Response {
        let sign_in = sign_in(invites, device).await;
        device
            .call(
                None,
                Request::RedeemPortableInvite(RedeemPortableInvite {
                    code: code.into(),
                    sign_in,
                }),
            )
            .await
    }

    #[tokio::test]
    async fn a_guest_invite_grants_a_portable_identity_and_makes_no_account() {
        let invites = Invites::start("invite-portable").await;
        let created = invites.create(guest_invite()).await;
        let link = parse_link(&created.link).unwrap();
        assert_eq!(link.kind, InviteLinkKind::PortableAccount);
        assert_eq!(link.role, None);
        assert_eq!(created.invite.kind, InviteKind::PortableAccount);
        let users = invites.root.user_list().await.unwrap();

        let phone = invites.stranger().await;
        let session = logged_in(redeem_as_guest(&invites, &phone, &created.code).await);
        match phone.call(Some(&session), Request::WhoAmI).await {
            Response::Ok(Reply::WhoAmI(WhoAmIReply { user: me, .. })) => {
                assert_eq!(me.user_id, format!("portable:{}", identity(7)));
                assert_eq!(me.display_name, "Sam");
            }
            other => panic!("no WhoAmI: {other:?}"),
        }
        let grants = invites.root.grant_list().await.unwrap();
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].id, identity(7).to_string());
        assert_eq!(
            grants[0].invite_id.as_deref(),
            Some(created.invite.id.as_str())
        );
        assert_eq!(invites.root.user_list().await.unwrap(), users);
        // Used up: the list drops it.
        assert!(invites.root.invite_list().await.unwrap().is_empty());
        invites.server.stop().await;
    }

    #[tokio::test]
    async fn a_code_redeems_only_as_its_kind_and_keeps_its_use() {
        let invites = Invites::start("invite-portable-kind").await;
        let guest = invites.create(guest_invite()).await;
        let account = invites.create(create_user()).await;
        let device = invites.stranger().await;
        assert_eq!(
            device.call(None, redeem(&guest.code, "sam")).await,
            Response::Err(Error::WrongInviteKind)
        );
        assert_eq!(
            redeem_as_guest(&invites, &device, &account.code).await,
            Response::Err(Error::WrongInviteKind)
        );
        let left: Vec<u32> = invites
            .root
            .invite_list()
            .await
            .unwrap()
            .iter()
            .map(|i| i.uses_left)
            .collect();
        assert_eq!(left, [1, 1]);
        assert!(invites.root.grant_list().await.unwrap().is_empty());
        invites.server.stop().await;
    }

    #[tokio::test]
    async fn an_expired_or_revoked_guest_invite_is_refused_with_a_good_credential() {
        let invites = Invites::start("invite-portable-refused").await;
        let revoked = invites.create(guest_invite()).await;
        invites
            .root
            .invite_revoke(revoked.invite.id.clone())
            .await
            .unwrap();
        let expiring = invites
            .create(NewInvite {
                expires: Some(invite::MIN_EXPIRES_SECS),
                ..guest_invite()
            })
            .await;
        let device = invites.stranger().await;
        assert_eq!(
            redeem_as_guest(&invites, &device, &revoked.code).await,
            Response::Err(Error::InviteInvalid)
        );
        invites.server.clock.advance(Duration::from_secs(3601));
        assert_eq!(
            redeem_as_guest(&invites, &device, &expiring.code).await,
            Response::Err(Error::InviteExpired)
        );
        // The admin's API session idled out with the clock: the store.
        assert!(invites.server.store.grants().await.unwrap().is_empty());
        invites.server.stop().await;
    }

    /// Review, finding 2: a revoked guest cannot redeem the invite that
    /// brought them again; only a newer invite lets them back.
    #[tokio::test]
    async fn a_revoked_guest_needs_a_new_invite() {
        let invites = Invites::start("invite-portable-revoked-guest").await;
        let created = invites
            .create(NewInvite {
                uses: Some(3),
                ..guest_invite()
            })
            .await;
        let device = invites.stranger().await;
        logged_in(redeem_as_guest(&invites, &device, &created.code).await);
        invites
            .root
            .grant_revoke(identity(7).to_string())
            .await
            .unwrap();
        let device = invites.stranger().await;
        assert_eq!(
            redeem_as_guest(&invites, &device, &created.code).await,
            Response::Err(Error::InviteInvalid)
        );
        assert_eq!(invites.root.invite_list().await.unwrap()[0].uses_left, 2);
        assert!(invites.root.grant_list().await.unwrap().is_empty());
        invites.server.clock.advance(Duration::from_secs(2));
        let newer = invites.create(guest_invite()).await;
        logged_in(redeem_as_guest(&invites, &device, &newer.code).await);
        assert_eq!(invites.root.grant_list().await.unwrap().len(), 1);
        invites.server.stop().await;
    }

    /// Review: an identity tied to an account here signs in as it; no
    /// grant is made and the invite keeps its use.
    #[tokio::test]
    async fn a_tied_identity_redeeming_signs_in_as_its_account() {
        let invites = Invites::start("invite-portable-tied").await;
        invites
            .server
            .add_user("alice", "correct horse", None)
            .await;
        let alice = invites
            .server
            .store
            .list_users()
            .await
            .unwrap()
            .into_iter()
            .find(|u| u.username == "alice")
            .unwrap()
            .id;
        invites
            .server
            .store
            .tie(alice, identity(7), "root")
            .await
            .unwrap();
        let created = invites.create(guest_invite()).await;
        let device = invites.stranger().await;
        let session = logged_in(redeem_as_guest(&invites, &device, &created.code).await);
        match device.call(Some(&session), Request::WhoAmI).await {
            Response::Ok(Reply::WhoAmI(WhoAmIReply { user: me, .. })) => {
                assert_eq!(me.username, "alice")
            }
            other => panic!("no WhoAmI: {other:?}"),
        }
        assert!(invites.root.grant_list().await.unwrap().is_empty());
        assert_eq!(invites.root.invite_list().await.unwrap()[0].uses_left, 1);
        invites.server.stop().await;
    }

    #[tokio::test]
    async fn a_guest_invite_with_a_role_other_than_consumer_is_refused() {
        let invites = Invites::start("invite-portable-role").await;
        let refused = invites
            .root
            .invite_create(NewInvite {
                role: Some(Role::Admin),
                ..guest_invite()
            })
            .await
            .unwrap_err();
        assert_eq!(code(refused), error_code::INVALID_INPUT);
        invites.server.stop().await;
    }
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

/// P6-07, glm E-8: a working code with a taken username or a password
/// too short is a refusal too, and leaves a line; the code and the
/// password do not reach it.
#[tokio::test]
async fn a_redemption_refused_for_its_username_or_password_is_logged() {
    if !alone("invites::a_redemption_refused_for_its_username_or_password_is_logged") {
        return;
    }
    let invites = Invites::start("invite-logged-taken").await;
    let created = invites.create(create_user()).await;
    let device = invites.stranger().await;
    let (capture, _guard) = LogCapture::start();
    assert_eq!(
        device.call(None, redeem(&created.code, "Root")).await,
        Response::Err(Error::UsernameTaken)
    );
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
    let lines = capture.refused();
    let reasons: Vec<_> = lines.iter().map(|l| l.field("reason")).collect();
    assert_eq!(
        reasons,
        [
            Some("username taken"),
            Some("username or password not acceptable")
        ],
        "{lines:#?}"
    );
    for line in &lines {
        assert_eq!(line.field("kind"), Some("sign_in_failed"), "{line:#?}");
        assert_eq!(line.field("request"), Some("redeem_invite"), "{line:#?}");
    }
    let text = capture.text();
    assert!(!text.contains(&created.code), "the code was logged: {text}");
    assert!(!text.contains("short"), "the password was logged: {text}");
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
        // As a message carries it: the https form (PX-45).
        let link = &created.share_link;

        let info = client.read_invite(link).await.unwrap();
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
        let entry = client.connect_invite(link).await.unwrap();
        assert_eq!(entry.name, "Home Media");
        assert_eq!(
            entry.server_id.as_deref(),
            Some(invites.server.server_id.to_string().as_str())
        );
        assert!(!entry.pinned, "{entry:?}");

        let session = client
            .redeem_invite(link, "mum", Some("Mum"), PASSWORD)
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

    /// A `spoolframe://` link as one was written before PX-45, by hand
    /// here, as a QR code printed then holds it, still redeems.
    #[tokio::test]
    async fn a_spoolframe_link_from_before_share_links_still_redeems() {
        let invites = invites("sdk-invite-old-link").await;
        let created = invites.create(create_user()).await;
        let told = invites.server.server.told();
        let addrs = tokio::task::spawn_blocking(move || told.addrs())
            .await
            .unwrap();
        let mut link = format!(
            "spoolframe://invite?server={}&name=Home%20Media",
            invites.server.server.id()
        );
        for addr in addrs {
            link.push_str(&format!("&addr={addr}"));
        }
        link.push_str(&format!(
            "&exp={}&role=consumer&cap={}",
            created.invite.expires_at, created.code
        ));
        let client = new_device().await;
        let session = client
            .redeem_invite(&link, "mum", None, PASSWORD)
            .await
            .unwrap();
        assert_eq!(session.user().username, "mum");
        client.close().await;
        invites.server.stop().await;
    }

    /// P7: the user's goal, end to end. A new portable identity on the
    /// phone joins the server with a guest invite; the living-room TV,
    /// guests allowed, pairs with the phone by QR, saves the server the
    /// phone hands it, signs in as the identity and plays. The server
    /// gains a grant, and no user.
    #[tokio::test]
    async fn a_new_identity_joins_on_the_phone_and_watches_on_the_tv() {
        let invites = invites("sdk-portable-tv").await;
        let file = invites.server.write_media(1 << 20);
        let created = invites
            .create(NewInvite {
                kind: spoolframe_admin_api::InviteKind::PortableAccount,
                note: Some("Sam".into()),
                uses: None,
                expires: None,
                role: None,
            })
            .await;
        let users = invites.root.user_list().await.unwrap();
        let server = invites.server.server.id().to_string();

        let phone = new_device().await;
        let (identity, _phrase) = phone.portable().create("Sam").await.unwrap();
        let info = phone.read_invite(&created.link).await.unwrap();
        assert_eq!(
            info.kind,
            spoolframe_client_sdk::InviteLinkKind::PortableAccount
        );
        let on_phone = phone
            .redeem_invite_portable(&created.link, identity, Some("Sam"))
            .await
            .unwrap();
        assert_eq!(on_phone.user().user_id, format!("portable:{identity}"));
        assert_eq!(on_phone.user().display_name, "Sam");

        let tv = new_device().await;
        tv.portable().set_allow_guests(true).await.unwrap();
        let pairing = tv.start_pairing("Living room TV").await.unwrap();
        let offer = phone.scan_pairing(pairing.link(), identity).await.unwrap();
        offer.authorize(None).await.unwrap();
        let paired = pairing.wait().await.unwrap();
        assert_eq!(paired.identity, identity);
        let handed: Vec<_> = paired.servers.iter().map(|s| &s.endpoint_id).collect();
        assert_eq!(handed, [&server]);

        // Pinned on the TV, not verified: the guest's phone vouched.
        let pinned = tv.servers().await.unwrap();
        let entry = pinned.iter().find(|s| s.endpoint_id == server).unwrap();
        assert!(entry.pinned && !entry.verified, "{entry:?}");
        let on_tv = tv.sign_in_portable(&server, identity).await.unwrap();
        assert_eq!(on_tv.user().user_id, format!("portable:{identity}"));
        let playback = on_tv
            .start_playback(movie().id(), Some(Selection::default()))
            .await
            .unwrap();
        let bytes = on_tv.read(&playback.handle, 0, 4096).await.unwrap();
        assert_eq!(bytes, file[..4096]);

        // Signed out on the TV, it signs in again with no password.
        let profiles = tv.saved_profiles(&server).await.unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(
            profiles[0].method,
            Some(spoolframe_client_sdk::SignInMethod::Portable)
        );
        assert_eq!(profiles[0].portable, Some(identity));

        let grants = invites.root.grant_list().await.unwrap();
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].id, identity.to_string());
        assert_eq!(grants[0].live_sessions, 2);
        assert_eq!(invites.root.user_list().await.unwrap(), users);
        phone.close().await;
        tv.close().await;
        invites.server.stop().await;
    }

    /// P7-08, end to end: an identity rooted in two security keys (one in
    /// hand, one in a drawer) is made on a Linux desktop, which pairs
    /// itself and joins with a guest invite. The TV, with no camera on
    /// the desktop to scan it, pairs by a code through the server; the
    /// desktop answers with the backup key. A fresh install with only the
    /// backup key fetches the identity from the server and acts for it.
    #[tokio::test]
    async fn a_key_identity_joins_pairs_a_tv_by_code_and_recovers_with_its_backup() {
        use spoolframe_client_sdk::{RootSigner, SecurityKey, SoftSecurityKey};

        let invites = invites("sdk-p7-08").await;
        let file = invites.server.write_media(1 << 20);
        let created = invites
            .create(NewInvite {
                kind: spoolframe_admin_api::InviteKind::PortableAccount,
                note: Some("Sam".into()),
                uses: None,
                expires: None,
                role: None,
            })
            .await;
        let server = invites.server.server.id().to_string();
        let (in_hand, drawer) = (SoftSecurityKey::new(), SoftSecurityKey::new());

        let desktop = new_device().await;
        let identity = desktop
            .portable()
            .create_with_keys(&[&in_hand, &drawer], "Sam")
            .await
            .unwrap();
        desktop
            .portable()
            .pair_self(identity, RootSigner::Key(&in_hand))
            .await
            .unwrap();
        let on_desktop = desktop
            .redeem_invite_portable(&created.link, identity, Some("Sam"))
            .await
            .unwrap();
        assert_eq!(on_desktop.user().user_id, format!("portable:{identity}"));

        // The TV knows the server (a link with no code), and offers.
        let tv = new_device().await;
        tv.portable().set_allow_guests(true).await.unwrap();
        let find = invites.root.invite_link().await.unwrap().link;
        tv.save_invite_server(&find).await.unwrap();
        let offered = tv
            .offer_relay_pairing(&server, "Living room TV")
            .await
            .unwrap();
        assert!(tv.collect_relay_pairing(&offered).await.unwrap().is_none());

        // The desktop types the code (lower case, as people do), checks
        // the fingerprint, and answers with the backup key.
        let request = desktop
            .fetch_relay_pairing(&server, &offered.code().to_lowercase())
            .await
            .unwrap();
        assert_eq!(request.name(), "Living room TV");
        assert_eq!(request.fingerprint(), offered.fingerprint());
        desktop
            .answer_relay_pairing(&request, identity, RootSigner::Key(&drawer), Some("TV"))
            .await
            .unwrap();

        let answer = tv.collect_relay_pairing(&offered).await.unwrap().unwrap();
        assert_eq!(answer.identity_fingerprint(), identity.fingerprint());
        let paired = tv.accept_relay_pairing(answer).await.unwrap();
        assert_eq!(paired.identity, identity);
        let handed: Vec<_> = paired.servers.iter().map(|s| &s.endpoint_id).collect();
        assert_eq!(handed, [&server]);
        let on_tv = tv.sign_in_portable(&server, identity).await.unwrap();
        assert_eq!(on_tv.user().user_id, format!("portable:{identity}"));
        let playback = on_tv
            .start_playback(movie().id(), Some(Selection::default()))
            .await
            .unwrap();
        let bytes = on_tv.read(&playback.handle, 0, 4096).await.unwrap();
        assert_eq!(bytes, file[..4096]);

        // A fresh install with only the drawer key: the server knows the
        // identity, so it hands over its record.
        let fresh = new_device().await;
        fresh.save_invite_server(&find).await.unwrap();
        let record = fresh
            .fetch_identity_record(&server, identity)
            .await
            .unwrap();
        assert!(record.genesis.is_some(), "{record:?}");
        fresh.portable().import_record(record, "Sam").await.unwrap();
        fresh
            .portable()
            .pair_self(identity, RootSigner::Key(&drawer))
            .await
            .unwrap();
        let on_fresh = fresh.sign_in_portable(&server, identity).await.unwrap();
        assert_eq!(on_fresh.user().user_id, format!("portable:{identity}"));

        // A stranger's identity is not one the server hands over.
        let (stranger, _) = new_device().await.portable().create("Eve").await.unwrap();
        assert!(matches!(
            fresh.fetch_identity_record(&server, stranger).await,
            Err(SdkError::NotFound(_))
        ));

        // Another fresh install knows neither the id nor a record: only the
        // drawer key, whose credential carries the identity's handle.
        let bare = new_device().await;
        bare.save_invite_server(&find).await.unwrap();
        let recovered = bare
            .recover_with_key(&server, &drawer, "Sam")
            .await
            .unwrap();
        assert_eq!(recovered, identity);
        bare.portable()
            .pair_self(identity, RootSigner::Key(&drawer))
            .await
            .unwrap();
        let on_bare = bare.sign_in_portable(&server, identity).await.unwrap();
        assert_eq!(on_bare.user().user_id, format!("portable:{identity}"));
        // A key the server knows no identity of finds nothing.
        let unknown = SoftSecurityKey::new();
        unknown
            .register(spoolframe_client_sdk::PORTABLE_RP, [7; 32], "nobody")
            .await
            .unwrap();
        assert!(matches!(
            bare.recover_with_key(&server, &unknown, "Nobody").await,
            Err(SdkError::NotFound(_))
        ));
        // Once the drawer key is removed, it recovers nothing: the record
        // names its identity, but the key is not one of its roots now.
        desktop
            .portable()
            .remove_root(
                identity,
                RootSigner::Key(&in_hand),
                &drawer.credentials()[0].root(),
            )
            .await
            .unwrap();
        // Signing in carries the state to the server.
        desktop.sign_in_portable(&server, identity).await.unwrap();
        let later = new_device().await;
        later.save_invite_server(&find).await.unwrap();
        assert!(matches!(
            later.recover_with_key(&server, &drawer, "Sam").await,
            Err(SdkError::NotARoot)
        ));
        desktop.close().await;
        tv.close().await;
        fresh.close().await;
        bare.close().await;
        later.close().await;
        invites.server.stop().await;
    }

    /// P7-08, Apple: one key, one identity, two APIs. An identity made on
    /// a Linux desktop with a key over raw CTAP2 joins a server; an iPhone
    /// with nothing but that key, reached through the system WebAuthn API,
    /// finds the identity by the handle the key gives back, pairs itself
    /// and signs in. Then the reverse: an identity made on the iPhone
    /// through WebAuthn is recovered by a Linux desktop over CTAP2.
    #[tokio::test]
    async fn a_key_identity_crosses_between_ctap_and_webauthn_devices() {
        use spoolframe_client_sdk::{RootSignature, RootSigner, SoftSecurityKey};

        let invites = invites("sdk-p7-08-webauthn").await;
        let server = invites.server.server.id().to_string();
        let find = invites.root.invite_link().await.unwrap().link;
        let new_invite = || {
            invites.create(NewInvite {
                kind: spoolframe_admin_api::InviteKind::PortableAccount,
                note: Some("Sam".into()),
                uses: None,
                expires: None,
                role: None,
            })
        };

        for made_through_webauthn in [false, true] {
            let key = if made_through_webauthn {
                SoftSecurityKey::webauthn()
            } else {
                SoftSecurityKey::new()
            };
            let other_way = if made_through_webauthn {
                key.as_ctap()
            } else {
                key.as_webauthn()
            };
            let maker = new_device().await;
            let identity = maker
                .portable()
                .create_with_keys(&[&key], "Sam")
                .await
                .unwrap();
            maker
                .portable()
                .pair_self(identity, RootSigner::Key(&key))
                .await
                .unwrap();
            let created = new_invite().await;
            maker
                .redeem_invite_portable(&created.link, identity, Some("Sam"))
                .await
                .unwrap();

            let other = new_device().await;
            other.save_invite_server(&find).await.unwrap();
            let handle = maker
                .portable()
                .identity_record(identity)
                .await
                .unwrap()
                .genesis
                .unwrap()
                .handle;
            let record = other
                .fetch_identity_record_by_handle(&server, handle)
                .await
                .unwrap();
            assert_eq!(record.user, identity);
            assert_eq!(
                other
                    .recover_with_key(&server, &other_way, "Sam")
                    .await
                    .unwrap(),
                identity
            );
            let credential = other
                .portable()
                .pair_self(identity, RootSigner::Key(&other_way))
                .await
                .unwrap();
            assert_eq!(
                matches!(credential.signature, RootSignature::WebAuthn { .. }),
                !made_through_webauthn,
                "{credential:?}"
            );
            let signed_in = other.sign_in_portable(&server, identity).await.unwrap();
            assert_eq!(signed_in.user().user_id, format!("portable:{identity}"));
            maker.close().await;
            other.close().await;
        }
        invites.server.stop().await;
    }

    /// P7-08: a key added to a phrase identity carries the identity's id as
    /// its handle, so a fresh install with only that key finds the
    /// identity by handle too, and keeps its id.
    #[tokio::test]
    async fn a_phrase_identity_s_added_key_recovers_it_by_handle() {
        use spoolframe_client_sdk::{RootSigner, SoftSecurityKey};

        let invites = invites("sdk-p7-08-phrase-key").await;
        let created = invites
            .create(NewInvite {
                kind: spoolframe_admin_api::InviteKind::PortableAccount,
                note: Some("Sam".into()),
                uses: None,
                expires: None,
                role: None,
            })
            .await;
        let server = invites.server.server.id().to_string();
        let phone = new_device().await;
        let (identity, _phrase) = phone.portable().create("Sam").await.unwrap();
        phone
            .redeem_invite_portable(&created.link, identity, Some("Sam"))
            .await
            .unwrap();
        let key = SoftSecurityKey::new();
        phone
            .portable()
            .add_root(identity, RootSigner::Phrase, &key, "Sam")
            .await
            .unwrap();
        assert_eq!(
            key.credentials()[0].user_handle(),
            Some(*identity.as_bytes())
        );
        // Signing in again carries the state that adds the key.
        phone.sign_in_portable(&server, identity).await.unwrap();

        let find = invites.root.invite_link().await.unwrap().link;
        let fresh = new_device().await;
        fresh.save_invite_server(&find).await.unwrap();
        let record = fresh
            .fetch_identity_record_by_handle(&server, *identity.as_bytes())
            .await
            .unwrap();
        assert_eq!(record.user, identity);
        assert!(record.genesis.is_none(), "{record:?}");
        assert_eq!(
            fresh.recover_with_key(&server, &key, "Sam").await.unwrap(),
            identity
        );
        fresh
            .portable()
            .pair_self(identity, RootSigner::Key(&key))
            .await
            .unwrap();
        let on_fresh = fresh.sign_in_portable(&server, identity).await.unwrap();
        assert_eq!(on_fresh.user().user_id, format!("portable:{identity}"));
        phone.close().await;
        fresh.close().await;
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
        // As sent: the https form (PX-45).
        let link = invites.root.invite_link().await.unwrap().share_link;
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
        let link = invites.root.invite_link().await.unwrap().link;
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
            .save_invite_server(&first.root.invite_link().await.unwrap().link)
            .await
            .unwrap();
        client
            .save_invite_server(&second.root.invite_link().await.unwrap().link)
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
