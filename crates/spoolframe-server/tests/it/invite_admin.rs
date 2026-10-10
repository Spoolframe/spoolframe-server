//! Making invites in an app (PX-44), over a real connection: an admin
//! makes, lists and revokes them as the web UI does, with where each was
//! made; a consumer and a portable guest are refused; making and revoking
//! need a recent confirmation, listing does not; and what an app makes, a
//! friend redeems.

use spoolframe_protocol::invite::{InviteLinkKind, parse_link};
use spoolframe_protocol::invite_admin::{
    CreateInvite, InviteCreated, InviteKind, InviteOrigin, InviteSummary,
};
use spoolframe_protocol::{
    ConfirmPassword, Error, RedeemInvite, Reply, Request, Response, SessionId,
};
use spoolframe_server::store::invites::{self, NewInvite};
use spoolframe_types::{Platform, Role};

use crate::support::{TestClient, TestServer};

const PASSWORD: &str = "correct horse";

/// A server with `andrey`, an admin called "Andrey", signed in on a
/// Linux app called "desk", and `alice`, a consumer.
async fn admin_at_desk(test: &str) -> (TestServer, TestClient, SessionId) {
    let server = TestServer::start(test).await;
    server.add_user("andrey", PASSWORD, Some("Andrey")).await;
    server.store.set_role("andrey", Role::Admin).await.unwrap();
    server.add_user("alice", PASSWORD, None).await;
    let desk = server.client().await;
    desk.hello_as(Some("desk")).await;
    let session = desk.login("andrey", PASSWORD).await;
    (server, desk, session)
}

fn create(kind: InviteKind) -> Request {
    Request::CreateInvite(CreateInvite {
        kind,
        note: Some("Mum".into()),
        uses: Some(2),
        expires_in_secs: Some(2 * 24 * 3600),
        role: None,
    })
}

fn created(response: Response) -> InviteCreated {
    match response {
        Response::Ok(Reply::InviteCreated(created)) => created,
        other => panic!("no invite made: {other:?}"),
    }
}

async fn list(client: &TestClient, session: &SessionId) -> Vec<InviteSummary> {
    match client.call(Some(session), Request::ListInvites).await {
        Response::Ok(Reply::Invites { invites }) => invites,
        other => panic!("no list: {other:?}"),
    }
}

fn revoke(id: &str) -> Request {
    Request::RevokeInvite { id: id.into() }
}

#[tokio::test]
async fn an_admin_makes_lists_and_revokes_an_invite_in_an_app() {
    let (server, desk, session) = admin_at_desk("invite-app-made").await;
    let made = created(
        desk.call(Some(&session), create(InviteKind::CreateUser))
            .await,
    );
    assert_eq!(made.code.len(), 14, "shown in groups: {}", made.code);
    let link = parse_link(&made.link).unwrap();
    assert_eq!(link.server, desk.conn.remote_id().to_string());
    assert_eq!(
        link.code.as_deref(),
        Some(made.code.replace('-', "").as_str())
    );
    assert_eq!(link.kind, InviteLinkKind::CreateUser);
    assert_eq!(link.role, Some(Role::Consumer));

    let listed = list(&desk, &session).await;
    assert_eq!(listed, vec![made.invite.clone()]);
    let invite = &listed[0];
    assert_eq!(invite.kind, InviteKind::CreateUser);
    assert_eq!(invite.note.as_deref(), Some("Mum"));
    assert_eq!((invite.uses, invite.uses_left), (2, 2));
    assert_eq!(invite.created_by.username.as_deref(), Some("andrey"));
    assert_eq!(invite.created_by.display_name, "Andrey");
    assert_eq!(
        invite.created_from,
        InviteOrigin::Device {
            endpoint_id: desk.endpoint.id().to_string(),
            name: Some("desk".into()),
            platform: Platform::Linux,
        }
    );
    // The web UI's list is the same store: it shows the admin's name.
    let web = server.store.live_invites().await.unwrap();
    assert_eq!(web.len(), 1);
    assert_eq!(web[0].created_by, "andrey");

    assert_eq!(
        desk.call(Some(&session), revoke(&invite.id)).await,
        Response::Ok(Reply::Done)
    );
    assert!(list(&desk, &session).await.is_empty());
    assert!(matches!(
        desk.call(Some(&session), revoke(&invite.id)).await,
        Response::Err(Error::NotFound { .. })
    ));
}

#[tokio::test]
async fn a_friend_redeems_what_an_app_made() {
    let (server, desk, session) = admin_at_desk("invite-app-redeemed").await;
    let made = created(
        desk.call(Some(&session), create(InviteKind::CreateUser))
            .await,
    );
    let friend = server.client().await;
    friend.hello().await;
    let redeemed = friend
        .call(
            None,
            Request::RedeemInvite(RedeemInvite {
                code: made.code,
                username: "mum".into(),
                display_name: None,
                password: "correct horse battery".into(),
            }),
        )
        .await;
    assert!(
        matches!(redeemed, Response::Ok(Reply::LoggedIn { .. })),
        "{redeemed:?}"
    );
    assert_eq!(list(&desk, &session).await[0].uses_left, 1);
}

#[tokio::test]
async fn a_portable_account_invite_says_so_in_its_link_and_takes_no_role() {
    let (_server, desk, session) = admin_at_desk("invite-app-portable").await;
    let made = created(
        desk.call(Some(&session), create(InviteKind::PortableAccount))
            .await,
    );
    assert_eq!(made.invite.kind, InviteKind::PortableAccount);
    let link = parse_link(&made.link).unwrap();
    assert_eq!(link.kind, InviteLinkKind::PortableAccount);
    assert_eq!(link.role, None);

    let with_role = Request::CreateInvite(CreateInvite {
        kind: InviteKind::PortableAccount,
        note: None,
        uses: None,
        expires_in_secs: None,
        role: Some(Role::Admin),
    });
    assert!(matches!(
        desk.call(Some(&session), with_role).await,
        Response::Err(Error::InvalidInput { .. })
    ));
}

#[tokio::test]
async fn what_the_web_ui_refuses_an_app_is_refused_too() {
    let (_server, desk, session) = admin_at_desk("invite-app-limits").await;
    for request in [
        create(InviteKind::Federation),
        Request::CreateInvite(CreateInvite {
            uses: Some(0),
            ..create_invite(InviteKind::CreateUser)
        }),
        Request::CreateInvite(CreateInvite {
            expires_in_secs: Some(60),
            ..create_invite(InviteKind::CreateUser)
        }),
        Request::CreateInvite(CreateInvite {
            note: Some("two\nlines".into()),
            ..create_invite(InviteKind::CreateUser)
        }),
    ] {
        let answer = desk.call(Some(&session), request.clone()).await;
        assert!(
            matches!(answer, Response::Err(Error::InvalidInput { .. })),
            "{request:?}: {answer:?}"
        );
    }
    assert!(list(&desk, &session).await.is_empty(), "nothing was made");
}

fn create_invite(kind: InviteKind) -> CreateInvite {
    match create(kind) {
        Request::CreateInvite(create) => create,
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn a_consumer_is_refused_every_invite_request() {
    let (server, _desk, _session) = admin_at_desk("invite-app-consumer").await;
    let phone = server.client().await;
    phone.hello().await;
    let alice = phone.login("alice", PASSWORD).await;
    for request in [
        create(InviteKind::CreateUser),
        Request::ListInvites,
        revoke("any"),
    ] {
        assert_eq!(
            phone.call(Some(&alice), request).await,
            Response::Err(Error::AdminRequired)
        );
    }
}

#[tokio::test]
async fn making_and_revoking_need_a_recent_confirmation_and_listing_does_not() {
    let (server, desk, session) = admin_at_desk("invite-app-recent").await;
    let made = created(
        desk.call(Some(&session), create(InviteKind::CreateUser))
            .await,
    );
    crate::recent_admin::age(&server.dir, "andrey").await;
    let not_recent = Response::Err(Error::RecentSignInRequired);
    assert_eq!(
        desk.call(Some(&session), create(InviteKind::CreateUser))
            .await,
        not_recent
    );
    assert_eq!(
        desk.call(Some(&session), revoke(&made.invite.id)).await,
        not_recent
    );
    assert_eq!(list(&desk, &session).await.len(), 1);

    // Confirming it is them lets both through again.
    assert!(matches!(
        desk.call(
            Some(&session),
            Request::ConfirmPassword(ConfirmPassword {
                password: PASSWORD.into(),
            }),
        )
        .await,
        Response::Ok(_)
    ));
    created(
        desk.call(Some(&session), create(InviteKind::CreateUser))
            .await,
    );
    assert_eq!(
        desk.call(Some(&session), revoke(&made.invite.id)).await,
        Response::Ok(Reply::Done)
    );
}

#[tokio::test]
async fn an_invite_from_the_cli_says_so_and_names_no_user() {
    let (server, desk, session) = admin_at_desk("invite-app-cli").await;
    server
        .store
        .create_invite(NewInvite {
            kind: invites::InviteKind::CreateUser,
            capability_hash: "h1",
            role: Role::Consumer,
            note: None,
            uses: 1,
            expires_at: server.store.clock().unix_secs() + 3600,
            created_by: spoolframe_admin_api::invite::MADE_BY_CLI,
            origin: &invites::InviteOrigin::Cli,
        })
        .await
        .unwrap();
    let listed = list(&desk, &session).await;
    assert_eq!(listed[0].created_from, InviteOrigin::Cli);
    assert_eq!(listed[0].created_by.username, None);
    assert_eq!(listed[0].created_by.display_name, "server CLI");
}
