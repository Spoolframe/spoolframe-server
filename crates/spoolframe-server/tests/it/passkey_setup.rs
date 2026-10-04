//! The first admin set up with a passkey and no password (P3-01, P3-05),
//! from a browser: the setup code guards it as it guards `setup`.

use std::net::SocketAddr;

use reqwest::StatusCode;
use serde_json::{Value, json};
use spoolframe_admin_api::{AdminApiClient, Credentials, MeApiClient, PasswordChange, error_code};

use crate::admin::{CODE, anonymous, code, creds, server_at, setup, with_token};
use crate::passkeys::{Authenticator, ORIGIN, cookie, post, sign_in_with, who};

const PASSWORD: &str = "correct horse";

/// Begin a passkey setup as `username` with `code`.
async fn begin(addr: SocketAddr, code: &str, username: &str) -> reqwest::Response {
    post(
        addr,
        "/auth/setup/passkey/begin",
        json!({ "code": code, "username": username }),
    )
    .await
}

/// Begin one with the right code: the ceremony and the options.
async fn begun(addr: SocketAddr, username: &str) -> (Value, Value) {
    let response = begin(addr, CODE, username).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    (body["ceremony"].clone(), body["options"].clone())
}

async fn finish(addr: SocketAddr, code: &str, ceremony: &Value, made: &Value) -> reqwest::Response {
    post(
        addr,
        "/auth/setup/passkey/finish",
        json!({ "code": code, "ceremony": ceremony, "name": "iPhone", "credential": made }),
    )
    .await
}

/// A response's error code.
async fn refused(response: reqwest::Response) -> i64 {
    assert!(response.headers().get("set-cookie").is_none());
    let body: Value = response.json().await.unwrap();
    body["code"].as_i64().unwrap()
}

/// A server whose first admin, `root`, was set up with `authenticator`'s
/// passkey alone: root's session token.
async fn set_up_with_a_passkey(
    test: &str,
    authenticator: &mut Authenticator,
) -> (SocketAddr, String, spoolframe_test_dir::TestDir) {
    let (addr, dir) = server_at(test, ORIGIN).await;
    let (ceremony, options) = begun(addr, "Root").await;
    let made = authenticator.create(ORIGIN, &options);
    let done = finish(addr, CODE, &ceremony, &made).await;
    assert_eq!(done.status(), StatusCode::OK);
    let session = cookie(&done);
    let body: Value = done.json().await.unwrap();
    assert_eq!(body, json!({ "username": "root", "role": "admin" }));
    let token = session.split_once('=').unwrap().1.to_owned();
    (addr, token, dir)
}

async fn setup_open(addr: SocketAddr) -> Value {
    reqwest::get(format!("http://{addr}/auth/setup"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_first_admin_with_only_a_passkey_signs_in_with_it_and_no_password() {
    let mut phone = Authenticator::new();
    let (addr, token, _dir) = set_up_with_a_passkey("setup-passkey", &mut phone).await;
    assert_eq!(setup_open(addr).await["open"], false);
    let root = with_token(addr, &token).await;
    let me = root.who_am_i().await.unwrap();
    assert!(!me.has_password);
    let passkeys = root.passkeys().await.unwrap();
    assert_eq!(passkeys.len(), 1);
    assert_eq!(passkeys[0].name, "iPhone");

    // The passkey signs them in, with no username.
    let signed_in = sign_in_with(addr, &mut phone).await;
    assert_eq!(signed_in.status(), StatusCode::OK);
    assert_eq!(who(addr, &cookie(&signed_in)).await["username"], "root");

    // No password does, and the refusal is a wrong password's: it says
    // nothing about the account.
    for password in ["", PASSWORD] {
        let err = anonymous(addr)
            .await
            .login(creds("root", password))
            .await
            .unwrap_err();
        assert_eq!(code(err), error_code::INVALID_CREDENTIALS);
    }
    let nobody = anonymous(addr)
        .await
        .login(creds("nobody", PASSWORD))
        .await
        .unwrap_err();
    assert_eq!(code(nobody), error_code::INVALID_CREDENTIALS);
}

#[tokio::test]
async fn a_passkey_setup_needs_the_setup_code_to_begin() {
    let (addr, _dir) = server_at("setup-passkey-code", ORIGIN).await;
    for wrong in ["AAAA-BBBB-CCCC", ""] {
        let response = begin(addr, wrong, "root").await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            refused(response).await,
            i64::from(error_code::INVALID_SETUP_CODE)
        );
    }
    let missing = post(
        addr,
        "/auth/setup/passkey/begin",
        json!({ "username": "root" }),
    )
    .await;
    assert_eq!(missing.status(), StatusCode::BAD_REQUEST);
    assert_eq!(setup_open(addr).await["open"], true);
}

#[tokio::test]
async fn a_passkey_setup_needs_the_setup_code_to_finish() {
    let (addr, _dir) = server_at("setup-passkey-code-finish", ORIGIN).await;
    let (ceremony, options) = begun(addr, "root").await;
    let made = Authenticator::new().create(ORIGIN, &options);
    for wrong in ["AAAA-BBBB-CCCC", ""] {
        let response = finish(addr, wrong, &ceremony, &made).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            refused(response).await,
            i64::from(error_code::INVALID_SETUP_CODE)
        );
    }
    assert_eq!(setup_open(addr).await["open"], true);
    // A wrong code did not use the request up.
    let done = finish(addr, CODE, &ceremony, &made).await;
    assert_eq!(done.status(), StatusCode::OK);
}

#[tokio::test]
async fn guessing_the_setup_code_for_a_passkey_is_limited_as_setup_is() {
    let (addr, _dir) = server_at("setup-passkey-guess", ORIGIN).await;
    for _ in 0..spoolframe_server::auth::MAX_FAILURES {
        begin(addr, "WRNG-WRNG-WRNG", "root").await;
    }
    let response = begin(addr, CODE, "root").await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    // One limit, whichever way setup is tried.
    let err = anonymous(addr)
        .await
        .setup(setup(CODE, "root", PASSWORD))
        .await
        .unwrap_err();
    assert_eq!(code(err), error_code::RATE_LIMITED);
}

/// A request begun while the server had no users finishes nothing once
/// another setup has made one.
#[tokio::test]
async fn a_passkey_setup_begun_before_another_finished_is_refused() {
    let (addr, _dir) = server_at("setup-passkey-late", ORIGIN).await;
    let (ceremony, options) = begun(addr, "late").await;
    let made = Authenticator::new().create(ORIGIN, &options);
    anonymous(addr)
        .await
        .setup(setup(CODE, "root", PASSWORD))
        .await
        .unwrap();
    let late = finish(addr, CODE, &ceremony, &made).await;
    assert_eq!(late.status(), StatusCode::CONFLICT);
    assert_eq!(refused(late).await, i64::from(error_code::ALREADY_SET_UP));
    // Nor is another begun.
    let again = begin(addr, CODE, "late").await;
    assert_eq!(again.status(), StatusCode::CONFLICT);
    // root is the one admin.
    let root = anonymous(addr)
        .await
        .login(creds("root", PASSWORD))
        .await
        .unwrap();
    let users = with_token(addr, &root.token)
        .await
        .user_list()
        .await
        .unwrap();
    assert_eq!(users.len(), 1);
}

#[tokio::test]
async fn of_two_passkey_setups_at_once_one_makes_the_admin() {
    let (addr, _dir) = server_at("setup-passkey-race", ORIGIN).await;
    let (first, first_options) = begun(addr, "first").await;
    let (second, second_options) = begun(addr, "second").await;
    let first_made = Authenticator::new().create(ORIGIN, &first_options);
    let second_made = Authenticator::new().create(ORIGIN, &second_options);
    let (a, b) = tokio::join!(
        finish(addr, CODE, &first, &first_made),
        finish(addr, CODE, &second, &second_made),
    );
    let mut statuses = [a.status(), b.status()];
    statuses.sort();
    assert_eq!(statuses, [StatusCode::OK, StatusCode::CONFLICT]);
}

#[tokio::test]
async fn a_server_without_passkeys_sets_up_with_a_password_and_says_why() {
    let (addr, _dir) = server_at("setup-passkey-http", "http://media.lan").await;
    let open = setup_open(addr).await;
    assert_eq!(open["open"], true);
    assert!(
        open["passkeysUnavailable"]
            .as_str()
            .unwrap()
            .contains("HTTPS"),
        "{open}"
    );
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/auth/setup/passkey/begin"))
        .header("origin", "http://media.lan")
        .json(&json!({ "code": CODE, "username": "root" }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["code"], error_code::NOT_CONFIGURED);
    assert!(body["message"].as_str().unwrap().contains("HTTPS"));
    anonymous(addr)
        .await
        .setup(setup(CODE, "root", PASSWORD))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_server_with_passkeys_says_nothing_is_missing() {
    let (addr, _dir) = server_at("setup-passkey-https", ORIGIN).await;
    assert_eq!(
        setup_open(addr).await,
        json!({ "open": true, "passkeysUnavailable": null })
    );
}

#[tokio::test]
async fn a_passkey_only_admin_keeps_their_last_passkey() {
    let (addr, token, _dir) =
        set_up_with_a_passkey("setup-passkey-last", &mut Authenticator::new()).await;
    let root = with_token(addr, &token).await;
    let id = root.passkeys().await.unwrap()[0].id.clone();
    let err = root.passkey_remove(id.clone()).await.unwrap_err();
    assert_eq!(code(err), error_code::LAST_SIGN_IN_METHOD);
    assert_eq!(root.passkeys().await.unwrap().len(), 1);
    // With a password beside it, it may go.
    root.set_password(PasswordChange {
        current_password: String::new(),
        new_password: PASSWORD.into(),
    })
    .await
    .unwrap();
    root.passkey_remove(id).await.unwrap();
}

#[tokio::test]
async fn a_passkey_only_user_sets_a_first_password() {
    let (addr, token, _dir) =
        set_up_with_a_passkey("setup-passkey-first-password", &mut Authenticator::new()).await;
    let root = with_token(addr, &token).await;
    root.set_password(PasswordChange {
        current_password: String::new(),
        new_password: PASSWORD.into(),
    })
    .await
    .unwrap();
    assert!(root.who_am_i().await.unwrap().has_password);
    anonymous(addr)
        .await
        .login(creds("root", PASSWORD))
        .await
        .unwrap();
    // From now on a change needs that password.
    let err = root
        .set_password(PasswordChange {
            current_password: String::new(),
            new_password: "battery staple".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(code(err), error_code::INVALID_CREDENTIALS);
}

#[tokio::test]
async fn an_admin_s_reset_gives_a_passkey_only_user_a_password() {
    let (addr, token, _dir) =
        set_up_with_a_passkey("setup-passkey-reset", &mut Authenticator::new()).await;
    let root = with_token(addr, &token).await;
    root.user_set_password(Credentials {
        username: "root".into(),
        password: PASSWORD.into(),
    })
    .await
    .unwrap();
    assert!(root.who_am_i().await.unwrap().has_password);
    anonymous(addr)
        .await
        .login(creds("root", PASSWORD))
        .await
        .unwrap();
}
