//! Passkeys in the apps (P3-19), over `spoolframe/client/1`, with
//! webauthn-rs' software authenticator in the place of the app's platform
//! passkey API: relying party `spoolframe.app`, origin
//! `https://spoolframe.app`. Every server here has no `public_url`, so its
//! web UI could only offer passkeys at `localhost`.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::json;
use spoolframe_admin_api::{AdminApiClient, MeApiClient, error_code};
use spoolframe_protocol::{
    APP_ORIGIN, APP_RP_ID, Error, FinishPasskeyRegistration, FinishPasskeySignIn, PasskeyCreation,
    PasskeyInfo, PasskeyRequest, Reply, Request, Response, SessionId,
};
use spoolframe_server::auth::hash_password;
use spoolframe_types::Role;
use webauthn_authenticator_rs::AuthenticatorBackend;
use webauthn_authenticator_rs::softtoken::SoftToken;
use webauthn_rs::prelude::Url;

use crate::admin::{anonymous, creds, serve_beside, with_token};
use crate::support::{ServerOptions, TestClient, TestServer, fresh_dir, logs};

const PASSWORD: &str = "correct horse";

/// Where the web UI beside a server is reached: no public URL, so the
/// listener's own host.
const WEB: &str = "http://localhost:8080";

/// The app on one device, with the platform's passkey store: it signs as
/// a real authenticator does, and keeps each passkey's credential id and
/// user handle, as the platform does (the soft token keeps no user
/// handle, and finds a passkey only when it is named).
pub(crate) struct App {
    token: SoftToken,
    stored: Vec<(Vec<u8>, Vec<u8>)>,
}

/// How the app's platform makes or signs: for which relying party, from
/// which origin, verifying its user or not. [`Platform::APPLE`] is what
/// the platform does; the rest is mischief.
#[derive(Clone, Copy)]
struct Platform {
    rp_id: &'static str,
    origin: &'static str,
    verify_user: bool,
}

impl Platform {
    const APPLE: Self = Self {
        rp_id: APP_RP_ID,
        origin: APP_ORIGIN,
        verify_user: true,
    };

    fn user_verification(self) -> &'static str {
        if self.verify_user {
            "required"
        } else {
            "discouraged"
        }
    }
}

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

impl App {
    pub(crate) fn new() -> Self {
        Self {
            token: SoftToken::new(true).unwrap().0,
            stored: Vec::new(),
        }
    }

    /// A copy of it as it is now, passkeys and signature counter.
    fn copy(&self) -> Self {
        Self {
            token: SoftToken::from_cbor(&self.token.to_cbor().unwrap()).unwrap(),
            stored: self.stored.clone(),
        }
    }

    /// Make a passkey for `creation`, called `name`.
    pub(crate) fn create(
        &mut self,
        creation: &PasskeyCreation,
        name: &str,
    ) -> FinishPasskeyRegistration {
        self.create_as(Platform::APPLE, creation, name)
    }

    fn create_as(
        &mut self,
        platform: Platform,
        creation: &PasskeyCreation,
        name: &str,
    ) -> FinishPasskeyRegistration {
        let options = serde_json::from_value(json!({
            "rp": { "id": platform.rp_id, "name": "Spoolframe" },
            "user": {
                "id": b64(&creation.user_id),
                "name": creation.user_name,
                "displayName": creation.display_name,
            },
            "challenge": b64(&creation.challenge),
            "pubKeyCredParams": [{ "type": "public-key", "alg": -7 }],
            "excludeCredentials": creation
                .exclude_credential_ids
                .iter()
                .map(|id| json!({ "type": "public-key", "id": b64(id) }))
                .collect::<Vec<_>>(),
            "authenticatorSelection": {
                "requireResidentKey": false,
                "userVerification": platform.user_verification(),
            },
        }))
        .unwrap();
        let made = self
            .token
            .perform_register(Url::parse(platform.origin).unwrap(), options, 60_000)
            .unwrap();
        let credential_id = made.raw_id.to_vec();
        self.stored
            .push((credential_id.clone(), creation.user_id.clone()));
        FinishPasskeyRegistration {
            credential_id,
            attestation_object: made.response.attestation_object.to_vec(),
            client_data_json: made.response.client_data_json.to_vec(),
            name: name.to_owned(),
        }
    }

    /// Sign `request` with the passkey it keeps (the first that `request`
    /// allows).
    pub(crate) fn get(&mut self, request: &PasskeyRequest) -> FinishPasskeySignIn {
        self.get_as(Platform::APPLE, request)
    }

    fn get_as(&mut self, platform: Platform, request: &PasskeyRequest) -> FinishPasskeySignIn {
        let (credential_id, user_handle) = self
            .stored
            .iter()
            .find(|(id, _)| {
                request.allow_credential_ids.is_empty() || request.allow_credential_ids.contains(id)
            })
            .cloned()
            .expect("a passkey for this request");
        let options = serde_json::from_value(json!({
            "challenge": b64(&request.challenge),
            "rpId": platform.rp_id,
            "allowCredentials": [{ "type": "public-key", "id": b64(&credential_id) }],
            "userVerification": platform.user_verification(),
        }))
        .unwrap();
        let signed = self
            .token
            .perform_auth(Url::parse(platform.origin).unwrap(), options, 60_000)
            .unwrap();
        FinishPasskeySignIn {
            credential_id: signed.raw_id.to_vec(),
            authenticator_data: signed.response.authenticator_data.to_vec(),
            client_data_json: signed.response.client_data_json.to_vec(),
            signature: signed.response.signature.to_vec(),
            user_handle,
        }
    }
}

async fn server(test: &str) -> TestServer {
    server_with(test, ServerOptions::default()).await
}

async fn server_with(test: &str, options: ServerOptions) -> TestServer {
    let server = TestServer::start_with(fresh_dir(test).await, options).await;
    assert_eq!(server.public_url, None);
    server.add_user("alice", PASSWORD, Some("Alice")).await;
    server
}

/// A device of alice's that said Hello as `name`.
async fn device(server: &TestServer, name: &str) -> TestClient {
    let device = server.client().await;
    device.hello_as(Some(name)).await;
    device
}

/// Alice signed in with her password on a device called `name`.
async fn alice_on(server: &TestServer, name: &str) -> (TestClient, SessionId) {
    let device = device(server, name).await;
    let session = device.login("alice", PASSWORD).await;
    (device, session)
}

async fn begin_registration(device: &TestClient, session: &SessionId) -> PasskeyCreation {
    match device
        .call(Some(session), Request::BeginPasskeyRegistration)
        .await
    {
        Response::Ok(Reply::PasskeyRegistrationBegun(begun)) => {
            begun.for_server(device.conn.remote_id().as_bytes())
        }
        other => panic!("not begun: {other:?}"),
    }
}

async fn finish_registration(
    device: &TestClient,
    session: &SessionId,
    made: FinishPasskeyRegistration,
) -> Response {
    device
        .call(Some(session), Request::FinishPasskeyRegistration(made))
        .await
}

/// Alice makes a passkey on `app`, in her session on `device`.
async fn register(device: &TestClient, session: &SessionId, app: &mut App, name: &str) {
    let creation = begin_registration(device, session).await;
    let made = app.create(&creation, name);
    assert_eq!(
        finish_registration(device, session, made).await,
        Response::Ok(Reply::Done)
    );
}

async fn begin_sign_in(device: &TestClient, username: Option<&str>) -> PasskeyRequest {
    let request = Request::BeginPasskeySignIn {
        username: username.map(str::to_owned),
    };
    match device.call(None, request).await {
        Response::Ok(Reply::PasskeySignInBegun(begun)) => {
            begun.for_server(device.conn.remote_id().as_bytes())
        }
        other => panic!("not begun: {other:?}"),
    }
}

async fn finish_sign_in(device: &TestClient, signed: FinishPasskeySignIn) -> Response {
    device
        .call(None, Request::FinishPasskeySignIn(signed))
        .await
}

/// A sign-in on `device` with `app`'s passkey, start to end.
async fn sign_in(device: &TestClient, app: &mut App) -> Response {
    let request = begin_sign_in(device, None).await;
    finish_sign_in(device, app.get(&request)).await
}

fn session_of(response: Response) -> SessionId {
    match response {
        Response::Ok(Reply::LoggedIn { session }) => session,
        other => panic!("not signed in: {other:?}"),
    }
}

async fn username(device: &TestClient, session: &SessionId) -> String {
    match device.call(Some(session), Request::WhoAmI).await {
        Response::Ok(Reply::WhoAmI(user)) => user.username,
        other => panic!("{other:?}"),
    }
}

const REFUSED: Response = Response::Err(Error::InvalidCredentials);

/// Alice with one app passkey, made on her phone: the phone, her session
/// on it and the app.
async fn alice_with_a_passkey(server: &TestServer) -> (TestClient, SessionId, App) {
    let (phone, session) = alice_on(server, "Alice's iPhone").await;
    let mut app = App::new();
    register(&phone, &session, &mut app, "Alice's iPhone").await;
    (phone, session, app)
}

#[tokio::test]
async fn a_passkey_made_in_the_app_signs_in_on_a_server_without_a_public_url() {
    let server = server("app-passkey-life").await;
    let (phone, session) = alice_on(&server, "Alice's iPhone").await;
    let creation = begin_registration(&phone, &session).await;
    // The keychain holds the passkeys of every server, all for
    // spoolframe.app: the server's name says which this one is.
    assert_eq!(creation.user_name, "alice on Home Media");
    assert_eq!(creation.display_name, "Alice on Home Media");
    assert_eq!(creation.user_id.len(), 16, "a UUID");
    assert_eq!(creation.exclude_credential_ids, Vec::<Vec<u8>>::new());
    let mut app = App::new();
    let made = app.create(&creation, "Alice's iPhone");
    let id = made.credential_id.clone();
    assert_eq!(
        finish_registration(&phone, &session, made).await,
        Response::Ok(Reply::Done)
    );
    let kept = server.store.passkey(&b64(&id)).await.unwrap().unwrap();
    assert_eq!(
        (kept.name.as_str(), kept.rp_id.as_str()),
        ("Alice's iPhone", APP_RP_ID)
    );
    // The same authenticator is not asked for a second one.
    assert_eq!(
        begin_registration(&phone, &session)
            .await
            .exclude_credential_ids,
        std::slice::from_ref(&id)
    );

    // On the TV, with no username: whatever passkey the phone offers.
    let tv = device(&server, "Living room TV").await;
    let request = begin_sign_in(&tv, None).await;
    assert_eq!(request.allow_credential_ids, Vec::<Vec<u8>>::new());
    let tv_session = session_of(finish_sign_in(&tv, app.get(&request)).await);
    assert_eq!(username(&tv, &tv_session).await, "alice");
    // Bound to the TV, as a password sign-in's is.
    assert_eq!(
        phone.call(Some(&tv_session), Request::WhoAmI).await,
        Response::Err(Error::Unauthenticated)
    );
    assert!(
        server
            .store
            .passkey(&b64(&id))
            .await
            .unwrap()
            .unwrap()
            .last_used_at
            .is_some()
    );

    // With a username, too.
    let request = begin_sign_in(&tv, Some("alice")).await;
    session_of(finish_sign_in(&tv, app.get(&request)).await);
    server.stop().await;
}

/// Anyone can begin a sign-in, so one for a username lists none of their
/// passkeys (PX-09 C-14): the platform finds them, as without one.
#[tokio::test]
async fn a_sign_in_for_a_username_lists_none_of_their_passkeys() {
    let server = server("app-passkey-no-list").await;
    let (_phone, _session, _app) = alice_with_a_passkey(&server).await;
    let tv = device(&server, "Living room TV").await;
    for username in ["Alice", "alice", "nobody"] {
        assert_eq!(
            begin_sign_in(&tv, Some(username))
                .await
                .allow_credential_ids,
            Vec::<Vec<u8>>::new(),
            "{username}"
        );
    }
    server.stop().await;
}

/// A web passkey is not an app's, nor the other way round: a sign-in that
/// asked for alice's app passkeys takes no other.
#[tokio::test]
async fn a_sign_in_for_a_user_takes_only_their_passkeys() {
    logs::keep();
    let server = server("app-passkey-allowed").await;
    let (_phone, _session, mut alices) = alice_with_a_passkey(&server).await;
    server.add_user("bob", PASSWORD, None).await;
    let bobs_phone = device(&server, "Bob's iPhone").await;
    let bob = bobs_phone.login("bob", PASSWORD).await;
    let mut bobs = App::new();
    register(&bobs_phone, &bob, &mut bobs, "Bob's iPhone").await;

    let tv = device(&server, "Living room TV").await;
    let for_alice = begin_sign_in(&tv, Some("alice")).await;
    let by_bob = bobs.get(&PasskeyRequest {
        allow_credential_ids: Vec::new(),
        ..for_alice
    });
    assert_eq!(finish_sign_in(&tv, by_bob).await, REFUSED);
    assert!(logs::has(&[
        "Bob's iPhone",
        "the passkey is not one the sign-in asked for"
    ]));
    let request = begin_sign_in(&tv, Some("alice")).await;
    let session = session_of(finish_sign_in(&tv, alices.get(&request)).await);
    assert_eq!(username(&tv, &session).await, "alice");
    server.stop().await;
}

/// Another server's endpoint id, for challenges an app signed while it
/// talked to that server (PX-09, finding 2).
const ANOTHER_SERVER: [u8; 32] = [0x55; 32];

/// A server sends a nonce, and the challenge is made from it and the
/// server the app talks to: one made for another server, with this one's
/// nonce, finishes nothing here.
#[tokio::test]
async fn a_challenge_made_for_another_server_is_refused() {
    logs::keep();
    let server = server("app-passkey-other-server").await;
    let (phone, session) = alice_on(&server, "Alice's iPhone").await;
    let begun = match phone
        .call(Some(&session), Request::BeginPasskeyRegistration)
        .await
    {
        Response::Ok(Reply::PasskeyRegistrationBegun(begun)) => begun,
        other => panic!("not begun: {other:?}"),
    };
    let mut app = App::new();
    let made = app.create(&begun.for_server(&ANOTHER_SERVER), "Alice's iPhone");
    assert_eq!(finish_registration(&phone, &session, made).await, REFUSED);
    assert!(logs::has(&["app passkey refused", "challenge"]));

    let (_phone, _session, mut app) = alice_with_a_passkey(&server).await;
    let tv = device(&server, "Living room TV").await;
    let begun = match tv
        .call(None, Request::BeginPasskeySignIn { username: None })
        .await
    {
        Response::Ok(Reply::PasskeySignInBegun(begun)) => begun,
        other => panic!("not begun: {other:?}"),
    };
    let signed = app.get(&begun.for_server(&ANOTHER_SERVER));
    assert_eq!(finish_sign_in(&tv, signed).await, REFUSED);
    server.stop().await;
}

#[tokio::test]
async fn a_passkey_made_on_another_server_is_refused() {
    logs::keep();
    let home = server("app-passkey-home").await;
    let cabin = server("app-passkey-cabin").await;
    let (_phone, _session, mut app) = alice_with_a_passkey(&home).await;
    let tv = device(&cabin, "Cabin TV").await;
    assert_eq!(sign_in(&tv, &mut app).await, REFUSED);
    assert!(logs::has(&["not a passkey of this server"]));
    home.stop().await;
    cabin.stop().await;
}

/// The same username on two servers is two accounts, and the handles say
/// nothing about each other.
#[tokio::test]
async fn the_user_handle_differs_between_servers() {
    let home = server("app-passkey-handle-home").await;
    let cabin = server("app-passkey-handle-cabin").await;
    let (at_home, home_session) = alice_on(&home, "Alice's iPhone").await;
    let (at_the_cabin, cabin_session) = alice_on(&cabin, "Alice's iPhone").await;
    let home_handle = begin_registration(&at_home, &home_session).await.user_id;
    let cabin_handle = begin_registration(&at_the_cabin, &cabin_session)
        .await
        .user_id;
    assert_ne!(home_handle, cabin_handle);
    // And it is one per user on a server.
    assert_eq!(
        begin_registration(&at_home, &home_session).await.user_id,
        home_handle
    );
    home.stop().await;
    cabin.stop().await;
}

const WEB_RP: Platform = Platform {
    rp_id: "localhost",
    origin: WEB,
    verify_user: true,
};
const WRONG_ORIGIN: Platform = Platform {
    origin: "https://evil.example",
    ..Platform::APPLE
};
const WRONG_RP_ID: Platform = Platform {
    rp_id: "localhost",
    ..Platform::APPLE
};
const NO_USER_VERIFICATION: Platform = Platform {
    verify_user: false,
    ..Platform::APPLE
};

#[tokio::test]
async fn a_passkey_made_for_another_relying_party_or_origin_or_unverified_is_not_kept() {
    logs::keep();
    let server = server("app-passkey-made-wrong").await;
    let (phone, session) = alice_on(&server, "Alice's iPhone").await;
    for (platform, why) in [
        (WEB_RP, "origin does not match"),
        (WRONG_ORIGIN, "origin does not match"),
        (WRONG_RP_ID, "relying party id hash does not match"),
        (NO_USER_VERIFICATION, "user verified bit is not set"),
    ] {
        let creation = begin_registration(&phone, &session).await;
        let made = App::new().create_as(platform, &creation, "Alice's iPhone");
        assert_eq!(finish_registration(&phone, &session, made).await, REFUSED);
        assert!(logs::has(&["app passkey refused", why]), "{why}");
    }
    let alice = server.store.password_login("alice").await.unwrap().unwrap();
    assert_eq!(server.store.list_passkeys(alice.id).await.unwrap(), []);
    server.stop().await;
}

#[tokio::test]
async fn an_assertion_for_another_relying_party_or_origin_or_unverified_signs_no_one_in() {
    logs::keep();
    let server = server("app-passkey-signed-wrong").await;
    let (_phone, _session, mut app) = alice_with_a_passkey(&server).await;
    let tv = device(&server, "Living room TV").await;
    for (platform, why) in [
        (WEB_RP, "origin does not match"),
        (WRONG_ORIGIN, "origin does not match"),
        (WRONG_RP_ID, "relying party id hash does not match"),
        (NO_USER_VERIFICATION, "user verified bit is not set"),
    ] {
        let request = begin_sign_in(&tv, None).await;
        let signed = app.get_as(platform, &request);
        assert_eq!(finish_sign_in(&tv, signed).await, REFUSED);
        assert!(logs::has(&["Alice's iPhone", why]), "{why}");
    }
    // As the platform signs, it signs in.
    session_of(sign_in(&tv, &mut app).await);
    server.stop().await;
}

#[tokio::test]
async fn a_challenge_is_used_once() {
    logs::keep();
    let server = server("app-passkey-replay").await;
    let (phone, session) = alice_on(&server, "Alice's iPhone").await;
    let mut app = App::new();
    let creation = begin_registration(&phone, &session).await;
    let made = app.create(&creation, "Alice's iPhone");
    // A name that cannot be kept does not use the challenge up.
    assert!(matches!(
        finish_registration(
            &phone,
            &session,
            FinishPasskeyRegistration {
                name: " ".into(),
                ..made.clone()
            }
        )
        .await,
        Response::Err(Error::InvalidInput { .. })
    ));
    assert_eq!(
        finish_registration(&phone, &session, made.clone()).await,
        Response::Ok(Reply::Done)
    );
    assert_eq!(finish_registration(&phone, &session, made).await, REFUSED);

    let tv = device(&server, "Living room TV").await;
    let request = begin_sign_in(&tv, None).await;
    let signed = app.get(&request);
    session_of(finish_sign_in(&tv, signed.clone()).await);
    // Replayed by someone who saw it go by. Refused because the challenge
    // is spent, whether or not the passkey counts its signatures (a
    // synced one does not).
    assert_eq!(finish_sign_in(&tv, signed.clone()).await, REFUSED);
    assert!(logs::has(&[
        "Alice's iPhone",
        "the sign-in was not begun on this device, or is over"
    ]));
    // Nor does it finish a sign-in it was not signed for.
    begin_sign_in(&tv, None).await;
    assert_eq!(finish_sign_in(&tv, signed).await, REFUSED);
    server.stop().await;
}

#[tokio::test]
async fn a_challenge_expires() {
    let lifetime = Duration::from_secs(2);
    let options = ServerOptions {
        passkey_lifetime: lifetime,
        ..ServerOptions::default()
    };
    let server = server_with("app-passkey-expired", options).await;
    let (phone, session) = alice_on(&server, "Alice's iPhone").await;
    let mut app = App::new();
    let late = App::new().create(&begin_registration(&phone, &session).await, "Late");
    let tv = device(&server, "Living room TV").await;
    let request = begin_sign_in(&tv, None).await;
    tokio::time::sleep(lifetime + Duration::from_millis(500)).await;
    assert_eq!(finish_registration(&phone, &session, late).await, REFUSED);
    register(&phone, &session, &mut app, "Alice's iPhone").await;
    // Signed for the sign-in begun before the passkey was kept: it has the
    // passkey, and the challenge is over.
    let signed = app.get(&request);
    assert_eq!(finish_sign_in(&tv, signed).await, REFUSED);
    session_of(sign_in(&tv, &mut app).await);
    server.stop().await;
}

/// A challenge is the endpoint's that asked for it: another device cannot
/// finish the sign-in, and its attempt leaves it to the device it is for.
#[tokio::test]
async fn a_challenge_issued_to_another_device_is_refused() {
    logs::keep();
    let server = server("app-passkey-endpoint").await;
    let (_phone, _session, mut app) = alice_with_a_passkey(&server).await;
    let tv = device(&server, "Living room TV").await;
    let thief = device(&server, "Somebody else's laptop").await;
    let request = begin_sign_in(&tv, None).await;
    let signed = app.get(&request);
    assert_eq!(finish_sign_in(&thief, signed.clone()).await, REFUSED);
    // Nor with a sign-in of its own begun.
    begin_sign_in(&thief, None).await;
    assert_eq!(finish_sign_in(&thief, signed.clone()).await, REFUSED);
    let session = session_of(finish_sign_in(&tv, signed).await);
    assert_eq!(username(&tv, &session).await, "alice");
    server.stop().await;
}

/// A passkey that counts its signatures and was copied: once the server
/// has seen the original's count, the copy's is behind it.
#[tokio::test]
async fn a_sign_count_that_went_backwards_is_refused() {
    logs::keep();
    let server = server("app-passkey-counter").await;
    let (_phone, _session, mut app) = alice_with_a_passkey(&server).await;
    let mut copy = app.copy();
    let tv = device(&server, "Living room TV").await;
    for _ in 0..2 {
        session_of(sign_in(&tv, &mut app).await);
    }
    assert_eq!(sign_in(&tv, &mut copy).await, REFUSED);
    assert!(logs::has(&[
        "Alice's iPhone",
        "the signature counter went backwards"
    ]));
    session_of(sign_in(&tv, &mut app).await);
    server.stop().await;
}

/// P1-11's limit counts a passkey that fails as it counts a wrong password,
/// under its user and the device.
#[tokio::test]
async fn failed_sign_ins_are_limited() {
    let server = server("app-passkey-limit").await;
    let (_phone, _session, mut app) = alice_with_a_passkey(&server).await;
    let tv = device(&server, "Living room TV").await;
    for _ in 0..spoolframe_server::auth::MAX_FAILURES {
        let request = begin_sign_in(&tv, None).await;
        let mut signed = app.get(&request);
        signed.signature[10] ^= 0xFF;
        assert_eq!(finish_sign_in(&tv, signed).await, REFUSED);
    }
    let request = begin_sign_in(&tv, None).await;
    assert!(matches!(
        finish_sign_in(&tv, app.get(&request)).await,
        Response::Err(Error::RateLimited { .. })
    ));
    server.stop().await;
}

#[tokio::test]
async fn a_passkey_is_added_only_soon_after_signing_in() {
    let server = server("app-passkey-recent").await;
    let (phone, session) = alice_on(&server, "Alice's iPhone").await;
    age(&server, &session).await;
    assert_eq!(
        phone
            .call(Some(&session), Request::BeginPasskeyRegistration)
            .await,
        Response::Err(Error::RecentSignInRequired)
    );
    // Signed in again, it may.
    let again = phone.login("alice", PASSWORD).await;
    register(&phone, &again, &mut App::new(), "Alice's iPhone").await;
    server.stop().await;
}

/// A passkey sign-in on a device that asks for alice's PIN opens a locked
/// session there, as her password does.
#[tokio::test]
async fn a_passkey_sign_in_is_locked_where_the_pin_is_asked() {
    let server = server("app-passkey-pin").await;
    let (_phone, _session, mut app) = alice_with_a_passkey(&server).await;
    let (tv, by_password) = alice_on(&server, "Living room TV").await;
    for request in [
        Request::SetPin {
            pin: "2468".into(),
            proof: None,
        },
        Request::SetPinRequired {
            device: None,
            required: true,
        },
    ] {
        assert_eq!(
            tv.call(Some(&by_password), request).await,
            Response::Ok(Reply::Done)
        );
    }
    let tv = tv.reconnect(&server).await;
    let session = session_of(sign_in(&tv, &mut app).await);
    assert_eq!(
        tv.call(Some(&session), Request::Libraries).await,
        Response::Err(Error::ProfileLocked)
    );
    assert_eq!(
        tv.call(
            Some(&session),
            Request::UnlockProfile { pin: "2468".into() }
        )
        .await,
        Response::Ok(Reply::Done)
    );
    assert!(matches!(
        tv.call(Some(&session), Request::Libraries).await,
        Response::Ok(Reply::Libraries { .. })
    ));
    server.stop().await;
}

/// My profile and an admin list app passkeys with the web's, marked, and
/// remove them as they do the web's; the last way to sign in stays.
#[tokio::test]
async fn app_passkeys_are_listed_and_removed_in_the_web_ui() {
    let server = server("app-passkey-web").await;
    let phc = hash_password(PASSWORD).await.unwrap();
    server
        .store
        .add_user("root", None, &phc, Role::Admin)
        .await
        .unwrap();
    let (_phone, _session, mut app) = alice_with_a_passkey(&server).await;
    let web = serve_beside(server.auth.clone(), WEB).await;
    let alice = with_token(
        web,
        &anonymous(web)
            .await
            .login(creds("alice", PASSWORD))
            .await
            .unwrap()
            .token,
    )
    .await;
    let root = with_token(
        web,
        &anonymous(web)
            .await
            .login(creds("root", PASSWORD))
            .await
            .unwrap()
            .token,
    )
    .await;
    let listed = alice.passkeys().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "Alice's iPhone");
    assert_eq!(listed[0].rp_id, APP_RP_ID);
    assert!(listed[0].app);
    // Not "made for another host": it works, in the apps.
    assert!(listed[0].usable);
    assert_eq!(root.user_passkeys("alice".into()).await.unwrap(), listed);

    // Alice with no password: her one passkey is her way in.
    let mut tablet = App::new();
    let (ipad, ipad_session) = alice_on(&server, "Alice's iPad").await;
    register(&ipad, &ipad_session, &mut tablet, "Alice's iPad").await;
    sql(
        &server,
        "DELETE FROM password_credentials WHERE user_id = \
         (SELECT id FROM users WHERE username = ?1)",
        "alice",
    )
    .await;
    alice.passkey_remove(listed[0].id.clone()).await.unwrap();
    let ipads = alice.passkeys().await.unwrap();
    assert_eq!(ipads.len(), 1);
    assert_eq!(
        code(alice.passkey_remove(ipads[0].id.clone()).await.unwrap_err()),
        error_code::LAST_SIGN_IN_METHOD
    );
    assert_eq!(
        code(
            root.user_remove_passkey("alice".into(), ipads[0].id.clone())
                .await
                .unwrap_err()
        ),
        error_code::LAST_SIGN_IN_METHOD
    );
    // The removed one signs no one in; the kept one does.
    let tv = device(&server, "Living room TV").await;
    assert_eq!(sign_in(&tv, &mut app).await, REFUSED);
    session_of(sign_in(&tv, &mut tablet).await);

    // With a password again, an admin removes it too. Setting it signs
    // alice out (PX-09 finding 4), so the admin's list is read.
    server
        .store
        .set_password("alice", &phc, None)
        .await
        .unwrap();
    root.user_remove_passkey("alice".into(), ipads[0].id.clone())
        .await
        .unwrap();
    assert_eq!(root.user_passkeys("alice".into()).await.unwrap(), []);
    server.stop().await;
}

/// The passkeys `session`'s user has, as the app lists them.
async fn passkeys(device: &TestClient, session: &SessionId) -> Vec<PasskeyInfo> {
    match device.call(Some(session), Request::ListPasskeys).await {
        Response::Ok(Reply::Passkeys { passkeys }) => passkeys,
        other => panic!("not listed: {other:?}"),
    }
}

/// Give `username` a passkey made in the web UI at `localhost`, as the web
/// UI keeps one: credential id `id`, called `name`.
pub(crate) async fn web_passkey(server: &TestServer, username: &str, id: &str, name: &str) {
    let user = server
        .store
        .list_users()
        .await
        .unwrap()
        .into_iter()
        .find(|u| u.username == username)
        .unwrap()
        .id;
    server
        .store
        .add_passkey(user, id, name, "{}", "localhost")
        .await
        .unwrap();
}

/// The one app passkey in `listed`. Passkeys made in the same second
/// are listed by id, so the tests never go by position.
pub(crate) fn app_one(listed: &[PasskeyInfo]) -> &PasskeyInfo {
    let mut apps = listed.iter().filter(|p| p.app);
    let one = apps.next().expect("an app passkey");
    assert!(apps.next().is_none(), "{listed:?}");
    one
}

/// The one web UI passkey in `listed`.
pub(crate) fn web_one(listed: &[PasskeyInfo]) -> &PasskeyInfo {
    let mut webs = listed.iter().filter(|p| !p.app);
    let one = webs.next().expect("a web passkey");
    assert!(webs.next().is_none(), "{listed:?}");
    one
}

/// Bob, with a password and an app passkey of his own.
async fn bob_with_a_passkey(server: &TestServer) -> String {
    server.add_user("bob", PASSWORD, None).await;
    let phone = device(server, "Bob's phone").await;
    let session = phone.login("bob", PASSWORD).await;
    register(&phone, &session, &mut App::new(), "Bob's phone").await;
    let listed = passkeys(&phone, &session).await;
    assert_eq!(listed.len(), 1);
    listed[0].id.clone()
}

/// What the app is told of a passkey `id` its user does not have.
fn not_found(id: &str) -> Response {
    Response::Err(Error::NotFound {
        what: format!("passkey {id}"),
    })
}

/// The app lists every passkey of its user's, the web UI's too, and tells
/// which it can sign in with.
#[tokio::test]
async fn the_app_lists_its_user_s_passkeys_the_web_ui_s_too() {
    let server = server("app-passkey-list").await;
    let (phone, session, mut app) = alice_with_a_passkey(&server).await;
    web_passkey(&server, "alice", "d2ViLTE", "YubiKey").await;
    bob_with_a_passkey(&server).await;

    let listed = passkeys(&phone, &session).await;
    assert_eq!(listed.len(), 2, "{listed:?}");
    let (made, web) = (app_one(&listed), web_one(&listed));
    // Named as the web UI names it: base64url of the platform's id.
    assert_eq!(made.id, b64(&app.stored[0].0));
    assert_eq!(made.name, "Alice's iPhone");
    assert_eq!(made.rp_id, APP_RP_ID);
    assert!(made.app);
    assert!(made.usable_here);
    assert_eq!(made.last_used_at, None);
    assert!(made.created_at > 0);
    assert_eq!(web.id, "d2ViLTE");
    assert_eq!(web.name, "YubiKey");
    assert_eq!(web.rp_id, "localhost");
    assert!(!web.app);
    assert!(!web.usable_here);

    // The ones the web UI lists, in its order.
    let ids = |list: &[PasskeyInfo]| list.iter().map(|p| p.id.clone()).collect::<Vec<_>>();
    let entries = server
        .store
        .list_passkeys(
            server
                .store
                .list_users()
                .await
                .unwrap()
                .into_iter()
                .find(|u| u.username == "alice")
                .unwrap()
                .id,
        )
        .await
        .unwrap();
    assert_eq!(
        ids(&listed),
        entries
            .into_iter()
            .map(|p| p.credential_id)
            .collect::<Vec<_>>()
    );

    // A sign-in with it shows when.
    let tv = device(&server, "Living room TV").await;
    let on_tv = session_of(sign_in(&tv, &mut app).await);
    let listed = passkeys(&tv, &on_tv).await;
    assert!(app_one(&listed).last_used_at.is_some(), "{listed:?}");
    assert_eq!(web_one(&listed).last_used_at, None);
    server.stop().await;
}

#[tokio::test]
async fn the_app_renames_a_passkey_as_the_web_ui_does() {
    let server = server("app-passkey-rename").await;
    let (phone, session, _app) = alice_with_a_passkey(&server).await;
    let id = passkeys(&phone, &session).await[0].id.clone();
    let rename = |name: &str| Request::RenamePasskey {
        id: id.clone(),
        name: name.to_owned(),
    };
    assert_eq!(
        phone.call(Some(&session), rename("  Kitchen iPad ")).await,
        Response::Ok(Reply::Done)
    );
    assert_eq!(passkeys(&phone, &session).await[0].name, "Kitchen iPad");
    // The web UI's rule: 1 to 64 characters, trimmed.
    for bad in ["", "   ", &"x".repeat(65)] {
        assert!(
            matches!(
                phone.call(Some(&session), rename(bad)).await,
                Response::Err(Error::InvalidInput { .. })
            ),
            "{bad:?}"
        );
    }
    assert_eq!(passkeys(&phone, &session).await[0].name, "Kitchen iPad");
    server.stop().await;
}

/// Removing one asks for a recent sign-in, as adding one does.
#[tokio::test]
async fn the_app_removes_a_passkey_only_soon_after_signing_in() {
    let server = server("app-passkey-remove").await;
    let (phone, session, mut app) = alice_with_a_passkey(&server).await;
    web_passkey(&server, "alice", "d2ViLTE", "YubiKey").await;
    let id = app_one(&passkeys(&phone, &session).await).id.clone();
    let remove = || Request::RemovePasskey { id: id.clone() };

    age(&server, &session).await;
    assert_eq!(
        phone.call(Some(&session), remove()).await,
        Response::Err(Error::RecentSignInRequired)
    );
    assert_eq!(passkeys(&phone, &session).await.len(), 2);

    let again = phone.login("alice", PASSWORD).await;
    assert_eq!(
        phone.call(Some(&again), remove()).await,
        Response::Ok(Reply::Done)
    );
    let left = passkeys(&phone, &again).await;
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].id, "d2ViLTE");
    // It signs no one in now.
    let tv = device(&server, "Living room TV").await;
    assert_eq!(sign_in(&tv, &mut app).await, REFUSED);
    // Gone is gone.
    assert_eq!(phone.call(Some(&again), remove()).await, not_found(&id));
    server.stop().await;
}

/// The web UI's rule: a user with no password keeps a passkey.
#[tokio::test]
async fn the_app_does_not_remove_the_last_way_to_sign_in() {
    let server = server("app-passkey-last").await;
    let (phone, session, mut app) = alice_with_a_passkey(&server).await;
    sql(
        &server,
        "DELETE FROM password_credentials WHERE user_id = \
         (SELECT id FROM users WHERE username = ?1)",
        "alice",
    )
    .await;
    let id = passkeys(&phone, &session).await[0].id.clone();
    assert_eq!(
        phone
            .call(Some(&session), Request::RemovePasskey { id: id.clone() })
            .await,
        Response::Err(Error::LastSignInMethod)
    );
    assert_eq!(passkeys(&phone, &session).await.len(), 1);
    let tv = device(&server, "Living room TV").await;
    session_of(sign_in(&tv, &mut app).await);
    server.stop().await;
}

/// Another user's passkey is not there to rename or remove, as one that
/// does not exist is not.
#[tokio::test]
async fn another_user_s_passkey_is_not_found() {
    let server = server("app-passkey-theirs").await;
    let (phone, session, _app) = alice_with_a_passkey(&server).await;
    let bobs = bob_with_a_passkey(&server).await;
    for id in [bobs.as_str(), "bm9wZQ"] {
        assert_eq!(
            phone
                .call(
                    Some(&session),
                    Request::RenamePasskey {
                        id: id.to_owned(),
                        name: "Mine now".into(),
                    },
                )
                .await,
            not_found(id)
        );
        assert_eq!(
            phone
                .call(Some(&session), Request::RemovePasskey { id: id.to_owned() })
                .await,
            not_found(id)
        );
    }
    let bob = device(&server, "Bob's laptop").await;
    let on_laptop = bob.login("bob", PASSWORD).await;
    let left = passkeys(&bob, &on_laptop).await;
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].name, "Bob's phone");
    server.stop().await;
}

/// A locked profile's passkeys are not shown or changed till it is
/// unlocked, as the rest of its session's requests.
#[tokio::test]
async fn a_locked_profile_s_passkeys_wait_for_its_pin() {
    let server = server("app-passkey-locked").await;
    let (phone, session, _app) = alice_with_a_passkey(&server).await;
    let id = passkeys(&phone, &session).await[0].id.clone();
    for request in [
        Request::SetPin {
            pin: "2468".into(),
            proof: None,
        },
        Request::SetPinRequired {
            device: None,
            required: true,
        },
    ] {
        assert_eq!(
            phone.call(Some(&session), request).await,
            Response::Ok(Reply::Done)
        );
    }
    let phone = phone.reconnect(&server).await;
    for request in [
        Request::ListPasskeys,
        Request::RenamePasskey {
            id: id.clone(),
            name: "Locked".into(),
        },
        Request::RemovePasskey { id: id.clone() },
    ] {
        assert_eq!(
            phone.call(Some(&session), request).await,
            Response::Err(Error::ProfileLocked)
        );
    }
    assert_eq!(
        phone
            .call(
                Some(&session),
                Request::UnlockProfile { pin: "2468".into() }
            )
            .await,
        Response::Ok(Reply::Done)
    );
    assert_eq!(passkeys(&phone, &session).await[0].name, "Alice's iPhone");
    server.stop().await;
}

fn code(err: jsonrpsee::core::ClientError) -> i32 {
    match err {
        jsonrpsee::core::ClientError::Call(e) => e.code(),
        other => panic!("not a call error: {other}"),
    }
}

/// Make `session`'s sign-in an hour older.
async fn age(server: &TestServer, session: &SessionId) {
    let hash = spoolframe_server::auth::SessionToken(session.0.clone()).hash();
    sql(
        server,
        "UPDATE sessions SET authenticated_at = authenticated_at - 3600 WHERE token_hash = ?1",
        &hash,
    )
    .await;
}

/// Run `query` on the server's database, as a hand edit would.
pub(crate) async fn sql(server: &TestServer, query: &'static str, bind: &str) {
    let pool = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::new().filename(server.dir.join("spoolframe.db")),
    )
    .await
    .unwrap();
    sqlx::query(query).bind(bind).execute(&pool).await.unwrap();
    pool.close().await;
}
