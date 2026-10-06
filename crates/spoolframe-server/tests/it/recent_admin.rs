//! An admin's sensitive changes need a recent sign-in (PX-10, PX-09
//! finding 3): what could take over an account or the server, from an
//! admin session left open or a stolen cookie, as `me.*` already does.

use jsonrpsee::core::ClientError;
use jsonrpsee::ws_client::WsClient;
use serde_json::json;
use spoolframe_admin_api::{AdminApiClient, Credentials, MeApiClient, NewUser, Role, error_code};

use crate::admin::code;
use crate::settings::served;

/// A provider to set a client secret for.
const FILE: &str = r#"
    admin { public_url = "https://media.example.org"; }
    auth { oidc [
        { name = "Pocket ID"; issuer = "https://id.example.org"; client_id = "spoolframe"; },
    ] }
"#;

/// Make every session of `username` signed in an hour ago.
async fn age(dir: &std::path::Path, username: &str) {
    let pool = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::new().filename(dir.join("spoolframe.db")),
    )
    .await
    .unwrap();
    sqlx::query(
        "UPDATE sessions SET authenticated_at = authenticated_at - 3600 \
         WHERE user_id = (SELECT id FROM users WHERE username = ?1)",
    )
    .bind(username)
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
}

fn user(username: &str, role: Role) -> NewUser {
    NewUser {
        username: username.into(),
        display_name: None,
        password: "battery staple".into(),
        role,
    }
}

/// Every call the rule covers, each with the outcome it had.
async fn sensitive(root: &WsClient) -> Vec<(&'static str, Option<i32>)> {
    fn outcome<T>(r: Result<T, ClientError>) -> Option<i32> {
        r.err().map(code)
    }
    let bob = || "bob".to_owned();
    vec![
        (
            "userSetPassword",
            outcome(
                root.user_set_password(Credentials {
                    username: bob(),
                    password: "new password".into(),
                })
                .await,
            ),
        ),
        (
            "userRemovePasskey",
            outcome(root.user_remove_passkey(bob(), "cred".into()).await),
        ),
        ("userClearPin", outcome(root.user_clear_pin(bob()).await)),
        (
            "userSetDeviceTrust",
            outcome(
                root.user_set_device_trust(bob(), "ab".repeat(32), true, false)
                    .await,
            ),
        ),
        (
            "userForgetDevice",
            outcome(root.user_forget_device(bob(), "ab".repeat(32)).await),
        ),
        (
            "userSetPinRequired(false)",
            outcome(
                root.user_set_pin_required(bob(), "ab".repeat(32), false)
                    .await,
            ),
        ),
        (
            "userSsoUnlink",
            outcome(
                root.user_sso_unlink(bob(), "https://id.example.org".into())
                    .await,
            ),
        ),
        (
            "userAdd(admin)",
            outcome(root.user_add(user("carol", Role::Admin)).await),
        ),
        (
            "userSetRole(admin)",
            outcome(root.user_set_role(bob(), Role::Admin).await),
        ),
        (
            "settingSet(admin.*)",
            outcome(
                root.setting_set("admin.public_url".into(), json!("https://tv.example.org"))
                    .await,
            ),
        ),
        (
            "settingUnset(admin.*)",
            outcome(root.setting_unset("admin.public_url".into()).await),
        ),
        (
            "settingSet(auth.*)",
            outcome(root.setting_set("auth.oidc".into(), json!([])).await),
        ),
        (
            "settingUnset(auth.*)",
            outcome(root.setting_unset("auth.oidc".into()).await),
        ),
        (
            "ssoSetClientSecret",
            outcome(
                root.sso_set_client_secret("Pocket ID".into(), Some("s3cr3t".into()))
                    .await,
            ),
        ),
        (
            "plexServerSetToken",
            outcome(
                root.plex_server_set_token("nas".into(), "token".into())
                    .await,
            ),
        ),
    ]
}

/// Signed in an hour ago, an admin's sensitive calls are refused with
/// `RECENT_SIGN_IN_REQUIRED`, and none of them happens.
#[tokio::test]
async fn an_old_admin_session_cannot_make_sensitive_changes() {
    let (_addr, root, dir) = served("recent-admin-old", FILE).await;
    root.user_add(user("bob", Role::Consumer)).await.unwrap();
    age(dir.as_ref(), "root").await;

    for (call, outcome) in sensitive(&root).await {
        assert_eq!(outcome, Some(error_code::RECENT_SIGN_IN_REQUIRED), "{call}");
    }
    let users = root.user_list().await.unwrap();
    assert!(users.iter().all(|u| u.username != "carol"), "{users:?}");
    assert!(
        users
            .iter()
            .any(|u| u.username == "bob" && u.role == Role::Consumer)
    );
}

/// What cannot take anything over still works from an old session.
#[tokio::test]
async fn an_old_admin_session_still_makes_the_other_changes() {
    let (_addr, root, dir) = served("recent-admin-other", FILE).await;
    root.user_add(user("bob", Role::Consumer)).await.unwrap();
    age(dir.as_ref(), "root").await;

    root.user_add(user("dave", Role::Consumer)).await.unwrap();
    root.user_set_role("bob".into(), Role::Consumer)
        .await
        .unwrap();
    root.setting_set("watch.watched_percent".into(), json!(85))
        .await
        .unwrap();
    let asked = root
        .user_set_pin_required("bob".into(), "ab".repeat(32), true)
        .await;
    assert_ne!(
        asked.err().map(code),
        Some(error_code::RECENT_SIGN_IN_REQUIRED)
    );
}

/// Confirmed again, as the web UI's "Confirm it's you" does, the same
/// session gets past the rule on every call.
#[tokio::test]
async fn a_confirmed_admin_session_makes_sensitive_changes() {
    let (_addr, root, dir) = served("recent-admin-fresh", FILE).await;
    root.user_add(user("bob", Role::Consumer)).await.unwrap();
    age(dir.as_ref(), "root").await;
    root.confirm_password("correct horse".into()).await.unwrap();

    for (call, outcome) in sensitive(&root).await {
        assert_ne!(outcome, Some(error_code::RECENT_SIGN_IN_REQUIRED), "{call}");
    }
    let users = root.user_list().await.unwrap();
    assert!(
        users
            .iter()
            .any(|u| u.username == "carol" && u.role == Role::Admin)
    );
}
