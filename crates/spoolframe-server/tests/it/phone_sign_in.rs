//! A phone signing a TV in with a sign-in it offers (feature
//! `spoolframe:phone-sign-in`): the phone reached the TV itself and asks
//! the server for a sign-in for the TV's key; the TV redeems it with
//! `AwaitDeviceSignIn`, and only then is its session made. The exchange
//! between phone and TV is the SDK's, tested there.

use crate::support;

use spoolframe_protocol::{Error, Feature, Reply, Request, Response, SessionId, WhoAmIReply};
use spoolframe_types::Platform;
use support::{ServerOptions, TestClient, TestServer, fresh_dir};

const PASSWORD: &str = "correct horse";

async fn server(test: &str, options: ServerOptions) -> TestServer {
    let server = TestServer::start_with(fresh_dir(test).await, options).await;
    server.add_user("alice", PASSWORD, None).await;
    server
}

/// Alice's phone, whose Hello named the feature, signed in.
async fn phone(server: &TestServer) -> (TestClient, SessionId) {
    let phone = server.client().await;
    phone
        .hello_with_features(Platform::Ios, Feature::PhoneSignIn.into())
        .await;
    let session = phone.login("alice", PASSWORD).await;
    (phone, session)
}

/// A TV that said Hello, and nothing else: it never began a sign-in.
async fn tv(server: &TestServer) -> TestClient {
    let tv = server.client().await;
    tv.hello_from(Platform::Tvos, Some("Living room TV"), None)
        .await;
    tv
}

fn offer(tv: &TestClient, remember: bool) -> Request {
    Request::OfferDeviceSignIn {
        device: tv.endpoint.id().to_string(),
        device_name: "Living room TV".into(),
        platform: Platform::Tvos,
        remember,
    }
}

/// The request id the phone hands the TV.
async fn offered(phone: &TestClient, session: &SessionId, request: Request) -> String {
    match phone.call(Some(session), request).await {
        Response::Ok(Reply::DeviceSignInOffered {
            request,
            expires_in_ms,
        }) => {
            assert!(expires_in_ms > 0);
            request
        }
        other => panic!("not offered: {other:?}"),
    }
}

/// The TV redeems `request`: its session, and whether it is remembered.
async fn redeem(tv: &TestClient, request: &str) -> Response {
    tv.call(
        None,
        Request::AwaitDeviceSignIn {
            request: request.into(),
        },
    )
    .await
}

async fn tv_listed(phone: &TestClient, session: &SessionId) -> bool {
    match phone.call(Some(session), Request::ListDevices).await {
        Response::Ok(Reply::Devices { devices }) => devices
            .iter()
            .any(|d| d.name.as_deref() == Some("Living room TV")),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn an_offered_tv_gets_a_session_of_its_own() {
    let server = server("psi-offer", ServerOptions::default()).await;
    let (phone, phone_session) = phone(&server).await;
    let tv = tv(&server).await;
    let request = offered(&phone, &phone_session, offer(&tv, false)).await;
    // Nothing is made until the TV asks.
    assert!(!tv_listed(&phone, &phone_session).await);
    let session = match redeem(&tv, &request).await {
        Response::Ok(Reply::LoggedIn { session, .. }) => session,
        other => panic!("not signed in: {other:?}"),
    };
    match tv.call(Some(&session), Request::WhoAmI).await {
        Response::Ok(Reply::WhoAmI(WhoAmIReply { user, .. })) => assert_eq!(user.username, "alice"),
        other => panic!("{other:?}"),
    }
    // Bound to the TV: useless from the phone.
    assert_eq!(
        phone.call(Some(&session), Request::WhoAmI).await,
        Response::Err(Error::Unauthenticated)
    );
    assert!(tv_listed(&phone, &phone_session).await);
    // Redeemed once.
    assert_eq!(
        redeem(&tv, &request).await,
        Response::Err(Error::DeviceSignInExpired)
    );
    tv.close().await;
    phone.close().await;
    server.stop().await;
}

#[tokio::test]
async fn only_the_tv_it_was_offered_to_may_redeem_it() {
    let server = server("psi-other", ServerOptions::default()).await;
    let (phone, phone_session) = phone(&server).await;
    let tv = tv(&server).await;
    let request = offered(&phone, &phone_session, offer(&tv, false)).await;
    let stranger = server.client().await;
    stranger.hello().await;
    assert_eq!(
        redeem(&stranger, &request).await,
        Response::Err(Error::DeviceSignInExpired)
    );
    // Nor the phone that offered it.
    assert_eq!(
        redeem(&phone, &request).await,
        Response::Err(Error::DeviceSignInExpired)
    );
    // The TV still can.
    assert!(matches!(
        redeem(&tv, &request).await,
        Response::Ok(Reply::LoggedIn { .. })
    ));
    stranger.close().await;
    tv.close().await;
    phone.close().await;
    server.stop().await;
}

#[tokio::test]
async fn an_offer_not_redeemed_in_time_expires_with_no_session() {
    let server = server("psi-expired", ServerOptions::default()).await;
    let (phone, phone_session) = phone(&server).await;
    let tv = tv(&server).await;
    let request = offered(&phone, &phone_session, offer(&tv, false)).await;
    server.clock.advance(server.auth.sign_ins().lifetime());
    assert_eq!(
        redeem(&tv, &request).await,
        Response::Err(Error::DeviceSignInExpired)
    );
    assert!(!tv_listed(&phone, &phone_session).await);
    tv.close().await;
    phone.close().await;
    server.stop().await;
}

/// The offer holds only while the session that made it does: one signed
/// out since signs no TV in.
#[tokio::test]
async fn an_offer_from_a_session_signed_out_since_is_not_redeemed() {
    let server = server("psi-signed-out", ServerOptions::default()).await;
    let (phone, phone_session) = phone(&server).await;
    let tv = tv(&server).await;
    let request = offered(&phone, &phone_session, offer(&tv, false)).await;
    assert_eq!(
        phone.call(Some(&phone_session), Request::Logout).await,
        Response::Ok(Reply::LoggedOut)
    );
    assert_eq!(
        redeem(&tv, &request).await,
        Response::Err(Error::DeviceSignInExpired)
    );
    let (phone, phone_session) = self::phone(&server).await;
    assert!(!tv_listed(&phone, &phone_session).await);
    tv.close().await;
    phone.close().await;
    server.stop().await;
}

#[tokio::test]
async fn an_old_session_cannot_offer() {
    let server = server("psi-recent", ServerOptions::default()).await;
    let (phone, session) = phone(&server).await;
    let tv = tv(&server).await;
    let hash = spoolframe_server::auth::SessionToken(session.0.clone()).hash();
    crate::app_passkeys::sql(
        &server,
        "UPDATE sessions SET authenticated_at = authenticated_at - 3600 WHERE token_hash = ?1",
        &hash,
    )
    .await;
    assert_eq!(
        phone.call(Some(&session), offer(&tv, false)).await,
        Response::Err(Error::RecentSignInRequired)
    );
    // Confirmed again, it may.
    server.store.confirm_session(&hash).await.unwrap();
    offered(&phone, &session, offer(&tv, false)).await;
    tv.close().await;
    phone.close().await;
    server.stop().await;
}

/// A local account's session signed in with a portable identity tied to
/// it offers as any other, and the TV's session rests on the same.
#[tokio::test]
async fn a_portable_credential_session_can_offer() {
    let server = server("psi-portable", ServerOptions::default()).await;
    let (phone, session) = phone(&server).await;
    let tv = tv(&server).await;
    let hash = spoolframe_server::auth::SessionToken(session.0.clone()).hash();
    crate::app_passkeys::sql(
        &server,
        "UPDATE sessions SET signed_in_with = 'portable_credential' WHERE token_hash = ?1",
        &hash,
    )
    .await;
    let request = offered(&phone, &session, offer(&tv, false)).await;
    let tv_session = match redeem(&tv, &request).await {
        Response::Ok(Reply::LoggedIn { session, .. }) => session,
        other => panic!("not signed in: {other:?}"),
    };
    let tv_hash = spoolframe_server::auth::SessionToken(tv_session.0.clone()).hash();
    let pool = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::new().filename(server.dir.join("spoolframe.db")),
    )
    .await
    .unwrap();
    let (with,): (Option<String>,) =
        sqlx::query_as("SELECT signed_in_with FROM sessions WHERE token_hash = ?1")
            .bind(&tv_hash)
            .fetch_one(&pool)
            .await
            .unwrap();
    pool.close().await;
    assert_eq!(with.as_deref(), Some("portable_credential"));
    tv.close().await;
    phone.close().await;
    server.stop().await;
}

/// The phone decides whether the TV is remembered, as an approver does.
#[tokio::test]
async fn remember_is_the_phone_s_choice() {
    let server = server("psi-remember", ServerOptions::default()).await;
    let (phone, session) = phone(&server).await;
    for remember in [false, true] {
        let tv = self::tv(&server).await;
        let request = offered(&phone, &session, offer(&tv, remember)).await;
        match redeem(&tv, &request).await {
            Response::Ok(Reply::LoggedIn { remembered, .. }) => {
                assert_eq!(remembered, remember)
            }
            other => panic!("not signed in: {other:?}"),
        }
        tv.close().await;
    }
    phone.close().await;
    server.stop().await;
}

#[tokio::test]
async fn an_offer_is_refused_where_the_feature_is_off() {
    let server = server("psi-off", ServerOptions::default()).await;
    let phone = server.client().await;
    phone.hello_as(Some("Alice's iPhone")).await;
    let session = phone.login("alice", PASSWORD).await;
    let tv = tv(&server).await;
    assert!(matches!(
        phone.call(Some(&session), offer(&tv, false)).await,
        Response::Err(Error::Unsupported { .. })
    ));
    tv.close().await;
    phone.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_phone_cannot_offer_to_itself_or_to_no_key() {
    let server = server("psi-self", ServerOptions::default()).await;
    let (phone, session) = phone(&server).await;
    for device in [phone.endpoint.id().to_string(), "not a key".to_owned()] {
        assert!(matches!(
            phone
                .call(
                    Some(&session),
                    Request::OfferDeviceSignIn {
                        device,
                        device_name: "TV".into(),
                        platform: Platform::Tvos,
                        remember: false,
                    },
                )
                .await,
            Response::Err(Error::InvalidInput { .. })
        ));
    }
    // No session, no offer.
    let tv = tv(&server).await;
    assert_eq!(
        phone.call(None, offer(&tv, false)).await,
        Response::Err(Error::Unauthenticated)
    );
    tv.close().await;
    phone.close().await;
    server.stop().await;
}

/// A TV holds few offers pending, and they are counted apart from its
/// own codes: offers it never redeemed leave it free to begin its own
/// sign-in.
#[tokio::test]
async fn a_tv_holds_few_offers() {
    let server = server("psi-cap", ServerOptions::default()).await;
    let (phone, session) = phone(&server).await;
    let tv = tv(&server).await;
    for _ in 0..spoolframe_server::device_sign_in::MAX_OFFERS_PER_DEVICE {
        offered(&phone, &session, offer(&tv, false)).await;
    }
    assert!(matches!(
        phone.call(Some(&session), offer(&tv, false)).await,
        Response::Err(Error::RateLimited { .. })
    ));
    assert!(matches!(
        tv.call(None, Request::BeginDeviceSignIn { remember: false })
            .await,
        Response::Ok(Reply::DeviceSignInStarted { .. })
    ));
    tv.close().await;
    phone.close().await;
    server.stop().await;
}

/// One user holds few offers pending, whichever TVs they are for: the
/// most a phone hands a TV at once. Another user still offers.
#[tokio::test]
async fn a_user_holds_few_offers() {
    let server = server("psi-user-cap", ServerOptions::default()).await;
    server.add_user("bob", PASSWORD, None).await;
    let (phone, session) = phone(&server).await;
    let offer_to = |n: u8| Request::OfferDeviceSignIn {
        device: iroh::SecretKey::from_bytes(&[n; 32]).public().to_string(),
        device_name: format!("TV {n}"),
        platform: Platform::Tvos,
        remember: false,
    };
    let cap = spoolframe_server::device_sign_in::MAX_OFFERS_PER_USER;
    assert_eq!(cap, spoolframe_protocol::phone_sign_in::MAX_OFFERS);
    for n in 1..=cap as u8 {
        offered(&phone, &session, offer_to(n)).await;
    }
    assert!(matches!(
        phone.call(Some(&session), offer_to(100)).await,
        Response::Err(Error::RateLimited { .. })
    ));
    let bob = server.client().await;
    bob.hello_with_features(Platform::Ios, Feature::PhoneSignIn.into())
        .await;
    let bob_session = bob.login("bob", PASSWORD).await;
    offered(&bob, &bob_session, offer_to(100)).await;
    bob.close().await;
    phone.close().await;
    server.stop().await;
}
