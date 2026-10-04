//! Passkeys (P3-05) over the admin API, with webauthn-rs' software
//! authenticator in the browser's place.

use std::net::SocketAddr;

use jsonrpsee::ws_client::WsClient;
use serde_json::{Value, json};
use spoolframe_admin_api::{AdminApiClient, MeApiClient, NewUser, Role, error_code};
use webauthn_authenticator_rs::AuthenticatorBackend;
use webauthn_authenticator_rs::softtoken::SoftToken;
use webauthn_rs::prelude::{CreationChallengeResponse, RequestChallengeResponse, Url};
use webauthn_rs_proto::UserVerificationPolicy;

use crate::admin::{CODE, anonymous, code, creds, server_at, server_at_behind, setup, with_token};
use crate::support::logs;

/// Where browsers reach the test servers: the relying party.
pub(crate) const ORIGIN: &str = "https://media.example.org";
const RP_ID: &str = "media.example.org";

const PASSWORD: &str = "correct horse";

/// An authenticator that stores its passkeys, as a phone, a laptop or a
/// password manager does, and the browser in front of it.
///
/// webauthn-rs' `SoftToken` signs as a real one does, but stores nothing a
/// sign-in without a username could find. So this keeps what a real one
/// keeps beside each passkey, its credential id and user handle, and
/// answers the soft token's refusal to store one by not asking it to.
pub(crate) struct Authenticator {
    token: SoftToken,
    /// Each passkey's credential id and user handle, base64url.
    stored: Vec<(Value, Value)>,
}

impl Authenticator {
    /// One that verifies its user (a fingerprint, a PIN) when asked to.
    pub(crate) fn new() -> Self {
        Self {
            token: SoftToken::new(true).unwrap().0,
            stored: Vec::new(),
        }
    }

    /// What `navigator.credentials.create` answers on a page at `origin`
    /// for the server's `options`.
    pub(crate) fn create(&mut self, origin: &str, options: &Value) -> Value {
        self.create_with(origin, options, |_| {})
    }

    /// As [`Authenticator::create`], by a browser that changes the options
    /// first.
    fn create_with(
        &mut self,
        origin: &str,
        options: &Value,
        change: impl FnOnce(&mut CreationChallengeResponse),
    ) -> Value {
        let mut options: CreationChallengeResponse =
            serde_json::from_value(options.clone()).unwrap();
        let handle = options.public_key.user.id.clone();
        let selection = options
            .public_key
            .authenticator_selection
            .as_mut()
            .expect("the server says what authenticator it wants");
        selection.require_resident_key = false;
        change(&mut options);
        let made = self
            .token
            .perform_register(Url::parse(origin).unwrap(), options.public_key, 60_000)
            .unwrap();
        let made = serde_json::to_value(made).unwrap();
        self.stored
            .push((made["rawId"].clone(), serde_json::to_value(handle).unwrap()));
        made
    }

    /// What `navigator.credentials.get` answers on a page at `origin` for
    /// the server's `options`: signed by the passkey it stores.
    pub(crate) fn get(&mut self, origin: &str, options: &Value) -> Value {
        self.get_with(origin, options, |_| {})
    }

    /// As [`Authenticator::get`], by a browser that changes the options
    /// first.
    fn get_with(
        &mut self,
        origin: &str,
        options: &Value,
        change: impl FnOnce(&mut RequestChallengeResponse),
    ) -> Value {
        let mut options: RequestChallengeResponse =
            serde_json::from_value(options.clone()).unwrap();
        // The server names no credential; the authenticator finds its own.
        options.public_key.allow_credentials = self
            .stored
            .iter()
            .map(|(id, _)| {
                serde_json::from_value(json!({ "type": "public-key", "id": id })).unwrap()
            })
            .collect();
        change(&mut options);
        let signed = self
            .token
            .perform_auth(Url::parse(origin).unwrap(), options.public_key, 60_000)
            .unwrap();
        let mut signed = serde_json::to_value(signed).unwrap();
        let (_, handle) = self
            .stored
            .iter()
            .find(|(id, _)| *id == signed["rawId"])
            .expect("signed by a passkey it stores");
        signed["response"]["userHandle"] = handle.clone();
        signed
    }

    /// A copy of it as it is now, passkeys and signature counter: what
    /// cloning an authenticator would give.
    fn copy(&self) -> Self {
        Self {
            token: SoftToken::from_cbor(&self.token.to_cbor().unwrap()).unwrap(),
            stored: self.stored.clone(),
        }
    }
}

/// A server at [`ORIGIN`] with the admin `root` and the consumer `alice`,
/// each signed in.
async fn server(test: &str) -> (SocketAddr, WsClient, WsClient, spoolframe_test_dir::TestDir) {
    let (addr, dir) = server_at(test, ORIGIN).await;
    let root = anonymous(addr)
        .await
        .setup(setup(CODE, "root", PASSWORD))
        .await
        .unwrap();
    let root = with_token(addr, &root.token).await;
    root.user_add(NewUser {
        username: "alice".into(),
        display_name: Some("Alice".into()),
        password: PASSWORD.into(),
        role: Role::Consumer,
    })
    .await
    .unwrap();
    let alice = sign_in(addr, "alice").await;
    (addr, root, alice, dir)
}

async fn sign_in(addr: SocketAddr, username: &str) -> WsClient {
    let login = anonymous(addr)
        .await
        .login(creds(username, PASSWORD))
        .await
        .unwrap();
    with_token(addr, &login.token).await
}

/// A refusal's code and what it says.
fn refusal(err: jsonrpsee::core::ClientError) -> (i32, String) {
    match err {
        jsonrpsee::core::ClientError::Call(e) => (e.code(), e.message().to_owned()),
        other => panic!("not a call error: {other}"),
    }
}

/// `user` adds a passkey on `authenticator`, called `name`: its id.
pub(crate) async fn add_passkey(
    user: &WsClient,
    authenticator: &mut Authenticator,
    name: &str,
) -> String {
    let options = user.passkey_add_begin().await.unwrap();
    let made = authenticator.create(ORIGIN, &options);
    user.passkey_add_finish(name.into(), made).await.unwrap().id
}

#[tokio::test]
async fn a_passkey_is_added_listed_renamed_and_removed() {
    let (_addr, _root, alice, _dir) = server("passkey-life").await;
    assert_eq!(alice.passkeys().await.unwrap(), []);
    let host = alice.passkey_host().await.unwrap();
    assert_eq!((host.rp_id.as_str(), host.origin.as_str()), (RP_ID, ORIGIN));

    let mut laptop = Authenticator::new();
    let id = add_passkey(&alice, &mut laptop, " MacBook ").await;
    let listed = alice.passkeys().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, id);
    assert_eq!(listed[0].name, "MacBook");
    assert_eq!(listed[0].rp_id, RP_ID);
    assert!(listed[0].usable);
    assert_eq!(listed[0].last_used_at, None);

    alice
        .passkey_rename(id.clone(), "Work laptop".into())
        .await
        .unwrap();
    assert_eq!(alice.passkeys().await.unwrap()[0].name, "Work laptop");

    alice.passkey_remove(id.clone()).await.unwrap();
    assert_eq!(alice.passkeys().await.unwrap(), []);
    assert_eq!(
        code(alice.passkey_remove(id).await.unwrap_err()),
        error_code::NO_SUCH_PASSKEY
    );
}

/// The user's two cases: a passkey the device itself stores, and one on a
/// phone reached through the browser's QR code. Both need a passkey that
/// is stored (discoverable) and verifies its user, and a server that does
/// not say where the authenticator must be or how it is reached.
#[tokio::test]
async fn registration_asks_for_a_stored_passkey_on_any_authenticator() {
    let (_addr, _root, alice, _dir) = server("passkey-options").await;
    let options = alice.passkey_add_begin().await.unwrap();
    let key = &options["publicKey"];
    assert_eq!(key["rp"]["id"], RP_ID);
    assert_eq!(key["user"]["name"], "alice");
    assert_eq!(key["user"]["displayName"], "Alice");
    let selection = key["authenticatorSelection"].as_object().unwrap();
    assert_eq!(selection["residentKey"], "required");
    assert_eq!(selection["requireResidentKey"], true);
    assert_eq!(selection["userVerification"], "required");
    assert!(!selection.contains_key("authenticatorAttachment"), "{key}");
    assert!(key.get("hints").is_none(), "{key}");
    assert_eq!(key["excludeCredentials"], json!([]));

    // A second one excludes the first by its id alone: no transports.
    let mut laptop = Authenticator::new();
    let made = laptop.create(ORIGIN, &options);
    let first = alice
        .passkey_add_finish("MacBook".into(), made)
        .await
        .unwrap();
    let again = alice.passkey_add_begin().await.unwrap();
    assert_eq!(
        again["publicKey"]["excludeCredentials"],
        json!([{ "type": "public-key", "id": first.id }])
    );
    assert_eq!(
        again["publicKey"]["user"]["id"], key["user"]["id"],
        "one user handle per user"
    );
}

#[tokio::test]
async fn a_passkey_made_without_user_verification_is_refused() {
    let (_addr, _root, alice, _dir) = server("passkey-no-uv").await;
    let options = alice.passkey_add_begin().await.unwrap();
    let mut key = Authenticator::new();
    // A browser that did not ask the authenticator to verify its user.
    let made = key.create_with(ORIGIN, &options, |options| {
        let selection = options.public_key.authenticator_selection.as_mut().unwrap();
        selection.user_verification = UserVerificationPolicy::Discouraged_DO_NOT_USE;
    });
    let (code, why) = refusal(
        alice
            .passkey_add_finish("Key".into(), made)
            .await
            .unwrap_err(),
    );
    assert_eq!(code, error_code::INVALID_INPUT);
    assert!(why.contains("user verified bit is not set"), "{why}");
    assert_eq!(alice.passkeys().await.unwrap(), []);
}

#[tokio::test]
async fn a_passkey_made_on_another_site_s_page_is_refused() {
    let (_addr, _root, alice, _dir) = server("passkey-origin").await;
    let options = alice.passkey_add_begin().await.unwrap();
    let made = Authenticator::new().create("https://evil.example", &options);
    let (code, why) = refusal(
        alice
            .passkey_add_finish("Key".into(), made)
            .await
            .unwrap_err(),
    );
    assert_eq!(code, error_code::INVALID_INPUT);
    assert!(why.contains("origin does not match"), "{why}");
    assert_eq!(alice.passkeys().await.unwrap(), []);
}

#[tokio::test]
async fn a_passkey_made_for_another_relying_party_is_refused() {
    let (_addr, _root, alice, _dir) = server("passkey-rp").await;
    let options = alice.passkey_add_begin().await.unwrap();
    // On the right page, for the wider domain.
    let made = Authenticator::new().create_with(ORIGIN, &options, |options| {
        options.public_key.rp.id = "example.org".into();
    });
    let (code, why) = refusal(
        alice
            .passkey_add_finish("Key".into(), made)
            .await
            .unwrap_err(),
    );
    assert_eq!(code, error_code::INVALID_INPUT);
    assert!(
        why.contains("relying party id hash does not match"),
        "{why}"
    );
    assert_eq!(alice.passkeys().await.unwrap(), []);
}

#[tokio::test]
async fn a_registration_is_finished_once_by_the_session_that_began_it() {
    let (addr, _root, alice, _dir) = server("passkey-ceremony").await;
    let options = alice.passkey_add_begin().await.unwrap();
    let mut laptop = Authenticator::new();
    let made = laptop.create(ORIGIN, &options);

    // Alice's other session did not begin it.
    let elsewhere = sign_in(addr, "alice").await;
    let not_begun = elsewhere
        .passkey_add_finish("MacBook".into(), made.clone())
        .await
        .unwrap_err();
    assert_eq!(
        refusal(not_begun),
        (
            error_code::INVALID_INPUT,
            "that passkey request is over; begin again".to_owned()
        )
    );

    // A name that cannot be kept does not use the challenge up.
    let unnamed = alice
        .passkey_add_finish("  ".into(), made.clone())
        .await
        .unwrap_err();
    assert_eq!(code(unnamed), error_code::INVALID_INPUT);
    alice
        .passkey_add_finish("MacBook".into(), made.clone())
        .await
        .unwrap();
    let replayed = alice
        .passkey_add_finish("MacBook again".into(), made)
        .await
        .unwrap_err();
    assert_eq!(code(replayed), error_code::INVALID_INPUT);
    assert_eq!(alice.passkeys().await.unwrap().len(), 1);
}

#[tokio::test]
async fn nobody_renames_or_removes_another_user_s_passkey() {
    let (addr, root, alice, _dir) = server("passkey-own").await;
    let id = add_passkey(&alice, &mut Authenticator::new(), "MacBook").await;
    root.user_add(NewUser {
        username: "bob".into(),
        display_name: None,
        password: PASSWORD.into(),
        role: Role::Consumer,
    })
    .await
    .unwrap();
    let bob = sign_in(addr, "bob").await;
    assert_eq!(bob.passkeys().await.unwrap(), []);
    assert_eq!(
        code(
            bob.passkey_rename(id.clone(), "Mine".into())
                .await
                .unwrap_err()
        ),
        error_code::NO_SUCH_PASSKEY
    );
    assert_eq!(
        code(bob.passkey_remove(id.clone()).await.unwrap_err()),
        error_code::NO_SUCH_PASSKEY
    );
    // Nor reads them, or removes one, as an admin would.
    assert_eq!(
        code(bob.user_passkeys("alice".into()).await.unwrap_err()),
        error_code::FORBIDDEN
    );
    assert_eq!(
        code(
            bob.user_remove_passkey("alice".into(), id)
                .await
                .unwrap_err()
        ),
        error_code::FORBIDDEN
    );
    assert_eq!(alice.passkeys().await.unwrap()[0].name, "MacBook");
}

/// The users page's sign-in column: a password, a passkey, or both.
#[tokio::test]
async fn the_user_list_says_who_has_a_passkey() {
    let (_addr, root, alice, _dir) = server("passkey-listed").await;
    let has_passkey = |users: &[spoolframe_admin_api::UserInfo], name: &str| {
        users
            .iter()
            .find(|u| u.username == name)
            .unwrap()
            .has_passkey
    };
    assert!(!has_passkey(&root.user_list().await.unwrap(), "alice"));
    let id = add_passkey(&alice, &mut Authenticator::new(), "MacBook").await;
    let users = root.user_list().await.unwrap();
    assert!(has_passkey(&users, "alice"));
    assert!(!has_passkey(&users, "root"));
    alice.passkey_remove(id).await.unwrap();
    assert!(!has_passkey(&root.user_list().await.unwrap(), "alice"));
}

#[tokio::test]
async fn an_admin_sees_and_removes_a_user_s_passkeys() {
    let (_addr, root, alice, _dir) = server("passkey-admin").await;
    let id = add_passkey(&alice, &mut Authenticator::new(), "MacBook").await;
    let seen = root.user_passkeys("alice".into()).await.unwrap();
    assert_eq!(seen, alice.passkeys().await.unwrap());
    assert_eq!(root.user_passkeys("root".into()).await.unwrap(), []);
    assert_eq!(
        code(root.user_passkeys("nobody".into()).await.unwrap_err()),
        error_code::NO_SUCH_USER
    );
    root.user_remove_passkey("alice".into(), id.clone())
        .await
        .unwrap();
    assert_eq!(alice.passkeys().await.unwrap(), []);
    assert_eq!(
        code(
            root.user_remove_passkey("alice".into(), id)
                .await
                .unwrap_err()
        ),
        error_code::NO_SUCH_PASSKEY
    );
}

/// WebAuthn needs a secure context. Behind a proxy that is plain HTTP on
/// a real host no browser has one, and the server says so rather than
/// hand out options that fail in the browser.
#[tokio::test]
async fn a_server_on_plain_http_has_no_passkeys_and_says_why() {
    let (addr, _dir) = server_at("passkey-http", "http://media.lan").await;
    let root = anonymous(addr)
        .await
        .setup(setup(CODE, "root", PASSWORD))
        .await
        .unwrap();
    let root = with_token(addr, &root.token).await;
    for refused in [
        root.passkey_host().await.map(|_| ()).unwrap_err(),
        root.passkey_add_begin().await.map(|_| ()).unwrap_err(),
    ] {
        match refused {
            jsonrpsee::core::ClientError::Call(e) => {
                assert_eq!(e.code(), error_code::NOT_CONFIGURED);
                assert!(e.message().contains("HTTPS"), "{}", e.message());
            }
            other => panic!("not a call error: {other}"),
        }
    }
    assert_eq!(root.passkeys().await.unwrap(), []);
}

/// A browser's POST to the page's own server.
pub(crate) async fn post(addr: SocketAddr, path: &str, body: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://{addr}{path}"))
        .header("origin", ORIGIN)
        .json(&body)
        .send()
        .await
        .unwrap()
}

/// Begin a passkey sign-in from a browser: the ceremony and the options.
async fn begin(addr: SocketAddr) -> (Value, Value) {
    let response = post(addr, "/auth/passkey/begin", json!({})).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    (body["ceremony"].clone(), body["options"].clone())
}

async fn finish(addr: SocketAddr, ceremony: &Value, signed: Value) -> reqwest::Response {
    post(
        addr,
        "/auth/passkey/finish",
        json!({ "ceremony": ceremony, "credential": signed }),
    )
    .await
}

/// A whole sign-in with `authenticator`, as the browser does it.
pub(crate) async fn sign_in_with(
    addr: SocketAddr,
    authenticator: &mut Authenticator,
) -> reqwest::Response {
    let (ceremony, options) = begin(addr).await;
    finish(addr, &ceremony, authenticator.get(ORIGIN, &options)).await
}

/// That `response` refuses a passkey, with the one answer every refusal
/// has.
async fn assert_refused(response: reqwest::Response) {
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert!(response.headers().get("set-cookie").is_none());
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["code"], error_code::PASSKEY_REFUSED);
    assert_eq!(body["message"], "that passkey did not sign in");
}

/// The session cookie `response` set, as a browser sends it back.
pub(crate) fn cookie(response: &reqwest::Response) -> String {
    let set = response.headers()["set-cookie"].to_str().unwrap();
    for part in ["HttpOnly", "Secure", "SameSite=Strict"] {
        assert!(set.contains(part), "{set}");
    }
    set.split(';').next().unwrap().to_owned()
}

pub(crate) async fn who(addr: SocketAddr, cookie: &str) -> Value {
    reqwest::Client::new()
        .get(format!("http://{addr}/auth/me"))
        .header("cookie", cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_passkey_signs_its_user_in_without_a_username() {
    let (addr, _root, alice, _dir) = server("passkey-sign-in").await;
    let mut phone = Authenticator::new();
    add_passkey(&alice, &mut phone, "iPhone").await;

    let signed_in = sign_in_with(addr, &mut phone).await;
    assert_eq!(signed_in.status(), reqwest::StatusCode::OK);
    // The cookie and the answer a password login gets.
    let session = cookie(&signed_in);
    let body: Value = signed_in.json().await.unwrap();
    assert_eq!(body, json!({ "username": "alice", "role": "consumer" }));
    assert_eq!(who(addr, &session).await["username"], "alice");
    assert!(alice.passkeys().await.unwrap()[0].last_used_at.is_some());

    // An ordinary session: listed with the others, and ended as they are.
    let token = session.split_once('=').unwrap().1;
    let by_passkey = with_token(addr, token).await;
    assert_eq!(by_passkey.sessions().await.unwrap().len(), 2);
    by_passkey.logout().await.unwrap();
    assert_eq!(
        who(addr, &session).await["code"],
        error_code::UNAUTHENTICATED
    );
}

/// The second of the user's cases, a phone signing a computer's browser in
/// through the browser's QR code, is the browser's doing: the server must
/// only not stand in its way. No credential is listed (so none carries a
/// transport hint), nothing says where the authenticator is, and the
/// browser shows its own prompt rather than autofill.
#[tokio::test]
async fn sign_in_lists_no_credentials_and_restricts_nothing() {
    let (addr, _root, alice, _dir) = server("passkey-sign-in-options").await;
    add_passkey(&alice, &mut Authenticator::new(), "iPhone").await;
    let (ceremony, options) = begin(addr).await;
    assert_eq!(ceremony.as_str().unwrap().len(), 64);
    let key = options["publicKey"].as_object().unwrap();
    assert_eq!(key["rpId"], RP_ID);
    assert_eq!(key["allowCredentials"], json!([]));
    assert_eq!(key["userVerification"], "required");
    assert!(!key.contains_key("hints"), "{options}");
    assert!(options.get("mediation").is_none(), "{options}");
    // Each request has a challenge of its own.
    let (other, again) = begin(addr).await;
    assert_ne!(other, ceremony);
    assert_ne!(again["publicKey"]["challenge"], key["challenge"]);
}

#[tokio::test]
async fn a_sign_in_request_is_used_once() {
    logs::keep();
    let (addr, _root, alice, _dir) = server("passkey-once").await;
    let mut phone = Authenticator::new();
    add_passkey(&alice, &mut phone, "Phone of the replay test").await;
    let (ceremony, options) = begin(addr).await;
    let signed = phone.get(ORIGIN, &options);
    let first = finish(addr, &ceremony, signed.clone()).await;
    assert_eq!(first.status(), reqwest::StatusCode::OK);
    // Replayed by someone who saw it go by. Refused because the request
    // is spent, whether or not the passkey counts its signatures (a
    // synced one does not).
    assert_refused(finish(addr, &ceremony, signed.clone()).await).await;
    assert!(logs::has(&[
        "Phone of the replay test",
        "the sign-in was not begun, or is over"
    ]));
    // Nor does it finish a request it was not signed for.
    let (other, _) = begin(addr).await;
    assert_refused(finish(addr, &other, signed).await).await;
}

#[tokio::test]
async fn an_assertion_for_another_relying_party_is_refused() {
    logs::keep();
    let (addr, _root, alice, _dir) = server("passkey-sign-in-rp").await;
    let mut phone = Authenticator::new();
    add_passkey(&alice, &mut phone, "Phone of the rp test").await;
    let (ceremony, options) = begin(addr).await;
    // On the right page, signed for the wider domain.
    let signed = phone.get_with(ORIGIN, &options, |options| {
        options.public_key.rp_id = "example.org".into();
    });
    assert_refused(finish(addr, &ceremony, signed).await).await;
    assert!(logs::has(&[
        "Phone of the rp test",
        "relying party id hash does not match"
    ]));
    // And from another site's page, for the right one.
    let (ceremony, options) = begin(addr).await;
    let signed = phone.get("https://evil.example", &options);
    assert_refused(finish(addr, &ceremony, signed).await).await;
    assert!(logs::has(&[
        "Phone of the rp test",
        "origin does not match"
    ]));
    // Unchanged, the same passkey signs in.
    assert_eq!(
        sign_in_with(addr, &mut phone).await.status(),
        reqwest::StatusCode::OK
    );
}

#[tokio::test]
async fn an_assertion_without_user_verification_is_refused() {
    logs::keep();
    let (addr, _root, alice, _dir) = server("passkey-sign-in-uv").await;
    let mut phone = Authenticator::new();
    add_passkey(&alice, &mut phone, "Phone of the uv test").await;
    let (ceremony, options) = begin(addr).await;
    // A browser that did not ask for the fingerprint or the PIN.
    let signed = phone.get_with(ORIGIN, &options, |options| {
        options.public_key.user_verification = UserVerificationPolicy::Discouraged_DO_NOT_USE;
    });
    assert_refused(finish(addr, &ceremony, signed).await).await;
    assert!(logs::has(&[
        "Phone of the uv test",
        "user verified bit is not set"
    ]));
    assert_eq!(
        sign_in_with(addr, &mut phone).await.status(),
        reqwest::StatusCode::OK
    );
}

/// A passkey that counts its signatures and was copied: once the server
/// has seen the original's count, the copy's is behind it.
#[tokio::test]
async fn a_copied_passkey_whose_counter_goes_backwards_is_refused_and_logged() {
    logs::keep();
    let (addr, _root, alice, _dir) = server("passkey-copied").await;
    let mut key = Authenticator::new();
    add_passkey(&alice, &mut key, "Key of the copy test").await;
    let mut copy = key.copy();
    for _ in 0..2 {
        assert_eq!(
            sign_in_with(addr, &mut key).await.status(),
            reqwest::StatusCode::OK
        );
    }
    assert!(!logs::has(&["Key of the copy test"]));
    assert_refused(sign_in_with(addr, &mut copy).await).await;
    assert!(logs::has(&[
        "Key of the copy test",
        "the signature counter went backwards"
    ]));
    // The original is ahead still.
    assert_eq!(
        sign_in_with(addr, &mut key).await.status(),
        reqwest::StatusCode::OK
    );
}

#[tokio::test]
async fn a_removed_passkey_and_a_disabled_user_s_passkey_sign_no_one_in() {
    let (addr, root, alice, _dir) = server("passkey-gone").await;
    let mut phone = Authenticator::new();
    let mut laptop = Authenticator::new();
    let on_phone = add_passkey(&alice, &mut phone, "iPhone").await;
    add_passkey(&alice, &mut laptop, "MacBook").await;
    alice.passkey_remove(on_phone).await.unwrap();
    assert_refused(sign_in_with(addr, &mut phone).await).await;
    assert_eq!(
        sign_in_with(addr, &mut laptop).await.status(),
        reqwest::StatusCode::OK
    );
    root.user_disable("alice".into()).await.unwrap();
    assert_refused(sign_in_with(addr, &mut laptop).await).await;
}

/// What an authenticator returns as the user handle is not signed. One
/// that names another user gets no one in, not even the passkey's own.
#[tokio::test]
async fn an_assertion_naming_another_user_is_refused() {
    logs::keep();
    let (addr, root, alice, _dir) = server("passkey-handle").await;
    let mut phone = Authenticator::new();
    add_passkey(&alice, &mut phone, "Phone of the handle test").await;
    let roots = root.passkey_add_begin().await.unwrap()["publicKey"]["user"]["id"].clone();
    let (ceremony, options) = begin(addr).await;
    let mut signed = phone.get(ORIGIN, &options);
    assert_ne!(signed["response"]["userHandle"], roots);
    signed["response"]["userHandle"] = roots;
    assert_refused(finish(addr, &ceremony, signed).await).await;
    assert!(logs::has(&[
        "Phone of the handle test",
        "the user handle is not the passkey's user's"
    ]));
}

/// P1-11's limit counts a passkey that fails as it counts a wrong
/// password, under where it comes from: the address the proxy names.
#[tokio::test]
async fn failed_passkey_sign_ins_count_in_the_login_limit() {
    let (addr, _dir) =
        server_at_behind("passkey-limit", ORIGIN, "127.0.0.1".parse().unwrap()).await;
    let root = anonymous(addr)
        .await
        .setup(setup(CODE, "root", PASSWORD))
        .await
        .unwrap();
    let root = with_token(addr, &root.token).await;
    let mut key = Authenticator::new();
    add_passkey(&root, &mut key, "Key").await;
    let from = |caller: &'static str, path: &'static str, body: Value| async move {
        reqwest::Client::new()
            .post(format!("http://{addr}{path}"))
            .header("origin", ORIGIN)
            .header("x-forwarded-for", caller)
            .json(&body)
            .send()
            .await
            .unwrap()
    };
    for _ in 0..5 {
        // A passkey this server never kept: made, and not registered.
        let mut stranger = Authenticator::new();
        stranger.create(ORIGIN, &root.passkey_add_begin().await.unwrap());
        let (ceremony, options) = begin(addr).await;
        let refused = from(
            "203.0.113.1",
            "/auth/passkey/finish",
            json!({ "ceremony": ceremony, "credential": stranger.get(ORIGIN, &options) }),
        )
        .await;
        assert_refused(refused).await;
    }
    // The sixth is not looked at, though the passkey is good.
    let (ceremony, options) = begin(addr).await;
    let limited = from(
        "203.0.113.1",
        "/auth/passkey/finish",
        json!({ "ceremony": ceremony, "credential": key.get(ORIGIN, &options) }),
    )
    .await;
    assert_eq!(limited.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
    // One limit with passwords: that address cannot try those either.
    let password = from(
        "203.0.113.1",
        "/auth/login",
        json!({ "username": "root", "password": PASSWORD }),
    )
    .await;
    assert_eq!(password.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
    // From elsewhere the passkey signs in.
    let (ceremony, options) = begin(addr).await;
    let elsewhere = from(
        "203.0.113.2",
        "/auth/passkey/finish",
        json!({ "ceremony": ceremony, "credential": key.get(ORIGIN, &options) }),
    )
    .await;
    assert_eq!(elsewhere.status(), reqwest::StatusCode::OK);
}

/// Wherever they come from, failures with one user's passkey limit that
/// user's passkeys, as wrong passwords limit a username.
#[tokio::test]
async fn a_passkey_that_keeps_failing_is_limited() {
    let (addr, _root, alice, _dir) = server("passkey-limit-user").await;
    let mut phone = Authenticator::new();
    add_passkey(&alice, &mut phone, "iPhone").await;
    for _ in 0..5 {
        let (ceremony, options) = begin(addr).await;
        let signed = phone.get("https://evil.example", &options);
        assert_refused(finish(addr, &ceremony, signed).await).await;
    }
    let limited = sign_in_with(addr, &mut phone).await;
    assert_eq!(limited.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
    let body: Value = limited.json().await.unwrap();
    assert_eq!(body["code"], error_code::RATE_LIMITED);
    assert!(body["data"]["retryAfterMs"].as_u64().unwrap() > 0);
    // Her password is another key: she is not locked out.
    let login = post(
        addr,
        "/auth/login",
        json!({ "username": "alice", "password": PASSWORD }),
    )
    .await;
    assert_eq!(login.status(), reqwest::StatusCode::OK);
}

/// The socket's own methods, as a client without cookies would use them.
#[tokio::test]
async fn a_passkey_signs_in_over_the_socket_too() {
    let (addr, _root, alice, _dir) = server("passkey-socket").await;
    let mut phone = Authenticator::new();
    add_passkey(&alice, &mut phone, "iPhone").await;
    let api = anonymous(addr).await;
    let challenge = api.passkey_login_begin().await.unwrap();
    let signed = phone.get(ORIGIN, &challenge.options);
    let login = api
        .passkey_login_finish(challenge.ceremony, signed)
        .await
        .unwrap();
    assert_eq!(
        (login.username.as_str(), login.role),
        ("alice", Role::Consumer)
    );
    let session = with_token(addr, &login.token).await;
    assert_eq!(session.who_am_i().await.unwrap().username, "alice");
}

#[tokio::test]
async fn a_server_without_passkeys_begins_no_sign_in() {
    let (addr, _dir) = server_at("passkey-http-sign-in", "http://media.lan").await;
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/auth/passkey/begin"))
        .header("origin", "http://media.lan")
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::NOT_IMPLEMENTED);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["code"], error_code::NOT_CONFIGURED);
    assert!(body["message"].as_str().unwrap().contains("HTTPS"));
}

/// A session confirms it is its user with one of their passkeys, as it
/// does with the password: the same request as a sign-in, finished in
/// the session.
#[tokio::test]
async fn a_passkey_confirms_its_own_user_and_no_one_else() {
    let (_addr, root, alice, _dir) = server("passkey-confirm").await;
    let mut phone = Authenticator::new();
    add_passkey(&alice, &mut phone, "iPhone").await;
    let challenge = alice.passkey_login_begin().await.unwrap();
    let signed = phone.get(ORIGIN, &challenge.options);
    alice
        .confirm_passkey(challenge.ceremony, signed)
        .await
        .unwrap();
    // Alice's passkey confirms nothing for root's session.
    let challenge = root.passkey_login_begin().await.unwrap();
    let signed = phone.get(ORIGIN, &challenge.options);
    assert_eq!(
        code(
            root.confirm_passkey(challenge.ceremony, signed)
                .await
                .unwrap_err()
        ),
        error_code::PASSKEY_REFUSED
    );
    // Nor does a confirmation replayed.
    let challenge = alice.passkey_login_begin().await.unwrap();
    let signed = phone.get(ORIGIN, &challenge.options);
    alice
        .confirm_passkey(challenge.ceremony.clone(), signed.clone())
        .await
        .unwrap();
    assert_eq!(
        code(
            alice
                .confirm_passkey(challenge.ceremony, signed)
                .await
                .unwrap_err()
        ),
        error_code::PASSKEY_REFUSED
    );
}
