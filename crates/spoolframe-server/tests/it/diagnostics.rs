//! Diagnostics (feature `spoolframe:diagnostics`): an app sends its
//! playback log and what it runs on; an admin lists, reads and deletes
//! it through the admin API. Refused without the feature, over its size,
//! and more than once a minute from a session; a consumer cannot read it.

use std::net::SocketAddr;
use std::time::Duration;

use jsonrpsee::ws_client::WsClient;
use spoolframe_admin_api::{AdminApiClient, MeApiClient, error_code};
use spoolframe_protocol::diagnostics::{DiagnosticsReport, MAX_LOG_BYTES};
use spoolframe_protocol::{Error, Feature, Features, Reply, Request, Response, SessionId};
use spoolframe_server::admin::{self, Origins, SetupCode, TrustedProxies};
use spoolframe_types::Platform;

use crate::admin::{CODE, admin, anonymous, code, creds, setup, with_token};
use crate::support::{TestClient, TestServer};

const PASSWORD: &str = "correct horse";

/// A server with Alice, a consumer, and an admin API beside it: root's
/// client, and the API's address.
async fn beside(test: &str) -> (TestServer, WsClient, SocketAddr) {
    let server = TestServer::start(test).await;
    let (addr, handle) = admin::serve(
        "127.0.0.1:0".parse().unwrap(),
        admin(server.auth.clone(), Some(SetupCode::new(CODE))),
        Origins::default(),
        TrustedProxies::default(),
    )
    .await
    .unwrap();
    std::mem::forget(handle);
    let root = anonymous(addr)
        .await
        .setup(setup(CODE, "root", "correct horse battery"))
        .await
        .unwrap();
    // After the first admin: set-up is only while there are no users.
    server.add_user("alice", PASSWORD, None).await;
    (server, with_token(addr, &root.token).await, addr)
}

/// Alice on an iPhone whose `Hello` named `features`.
async fn alice(server: &TestServer, features: Features) -> (TestClient, SessionId) {
    let client = server.client().await;
    client.hello_with_features(Platform::Ios, features).await;
    let session = client.login("alice", PASSWORD).await;
    (client, session)
}

fn diagnostics() -> Features {
    Features::only(Feature::Diagnostics)
}

fn report(log: String) -> DiagnosticsReport {
    DiagnosticsReport {
        device_model: "iPhone16,2".into(),
        os_name: "iOS".into(),
        os_version: "26.1".into(),
        app_name: "Spoolframe".into(),
        app_version: "1.4.0 (212)".into(),
        log,
        note: Some("It froze at the credits".into()),
    }
}

async fn send(client: &TestClient, session: &SessionId, report: DiagnosticsReport) -> Response {
    client
        .call(Some(session), Request::SendDiagnostics { report })
        .await
}

/// Sent, listed newest first with who and where, read whole, deleted.
#[tokio::test]
async fn a_report_is_sent_listed_read_and_deleted() {
    let (server, root, _) = beside("diagnostics-round-trip").await;
    let (phone, session) = alice(&server, diagnostics()).await;
    let log = "12:00:01 play plex:movie:1\n12:00:09 stalled\n".to_owned();
    let id = match send(&phone, &session, report(log.clone())).await {
        Response::Ok(Reply::DiagnosticsSent { id }) => id,
        other => panic!("{other:?}"),
    };
    let listed = root.diagnostics_list().await.unwrap();
    assert_eq!(listed.len(), 1);
    let row = &listed[0];
    assert_eq!(row.id, id);
    assert_eq!(row.user_name, "alice");
    assert!(row.user_id.starts_with("local:"), "{}", row.user_id);
    assert_eq!(row.endpoint_id, phone.endpoint.id().to_string());
    assert_eq!(row.device_model, "iPhone16,2");
    assert_eq!(row.log_bytes, log.len() as u64);
    assert!(row.has_note);
    let detail = root.diagnostics(id.clone()).await.unwrap();
    assert_eq!(detail.summary, *row);
    assert_eq!(detail.log, log);
    assert_eq!(detail.note.as_deref(), Some("It froze at the credits"));
    root.diagnostics_delete(id.clone()).await.unwrap();
    assert!(root.diagnostics_list().await.unwrap().is_empty());
    let gone = root.diagnostics(id.clone()).await.unwrap_err();
    assert_eq!(code(gone), error_code::NO_SUCH_REPORT);
    let gone = root.diagnostics_delete(id).await.unwrap_err();
    assert_eq!(code(gone), error_code::NO_SUCH_REPORT);
    server.stop().await;
}

/// A log over the cap is refused whole, naming the log; one at it is kept.
#[tokio::test]
async fn a_log_over_the_cap_is_refused() {
    let (server, root, _) = beside("diagnostics-cap").await;
    let (phone, session) = alice(&server, diagnostics()).await;
    match send(&phone, &session, report("a".repeat(MAX_LOG_BYTES + 1))).await {
        Response::Err(Error::InvalidInput { message }) => {
            assert!(message.contains("log"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    assert!(root.diagnostics_list().await.unwrap().is_empty());
    assert!(matches!(
        send(&phone, &session, report("a".repeat(MAX_LOG_BYTES))).await,
        Response::Ok(Reply::DiagnosticsSent { .. })
    ));
    assert_eq!(
        root.diagnostics_list().await.unwrap()[0].log_bytes,
        MAX_LOG_BYTES as u64
    );
    server.stop().await;
}

/// One a minute from a session: a second sooner is `RateLimited`, saying
/// when; a minute on, it is kept.
#[tokio::test]
async fn a_session_sends_one_a_minute() {
    let (server, root, _) = beside("diagnostics-rate").await;
    let (phone, session) = alice(&server, diagnostics()).await;
    assert!(matches!(
        send(&phone, &session, report("first".into())).await,
        Response::Ok(Reply::DiagnosticsSent { .. })
    ));
    match send(&phone, &session, report("second".into())).await {
        Response::Err(Error::RateLimited { retry_after_ms }) => {
            assert!(
                (1_000..=60_000).contains(&retry_after_ms),
                "{retry_after_ms}"
            );
        }
        other => panic!("{other:?}"),
    }
    server.clock.advance(Duration::from_secs(60));
    assert!(matches!(
        send(&phone, &session, report("third".into())).await,
        Response::Ok(Reply::DiagnosticsSent { .. })
    ));
    assert_eq!(root.diagnostics_list().await.unwrap().len(), 2);
    server.stop().await;
}

/// An app that did not name the feature is answered `Unsupported`, and
/// nothing is kept.
#[tokio::test]
async fn refused_without_the_feature() {
    let (server, root, _) = beside("diagnostics-off").await;
    let (phone, session) = alice(&server, Features::only(Feature::October2026)).await;
    assert!(matches!(
        send(&phone, &session, report("x".into())).await,
        Response::Err(Error::Unsupported { .. })
    ));
    assert!(root.diagnostics_list().await.unwrap().is_empty());
    server.stop().await;
}

/// A consumer cannot list, read or delete reports, their own included.
#[tokio::test]
async fn a_consumer_cannot_read_them() {
    let (server, root, addr) = beside("diagnostics-consumer").await;
    let (phone, session) = alice(&server, diagnostics()).await;
    let id = match send(&phone, &session, report("x".into())).await {
        Response::Ok(Reply::DiagnosticsSent { id }) => id,
        other => panic!("{other:?}"),
    };
    let token = anonymous(addr)
        .await
        .login(creds("alice", PASSWORD))
        .await
        .unwrap()
        .token;
    let alice = with_token(addr, &token).await;
    let refused = alice.diagnostics_list().await.unwrap_err();
    assert_eq!(code(refused), error_code::FORBIDDEN);
    let refused = alice.diagnostics(id.clone()).await.unwrap_err();
    assert_eq!(code(refused), error_code::FORBIDDEN);
    let refused = alice.diagnostics_delete(id).await.unwrap_err();
    assert_eq!(code(refused), error_code::FORBIDDEN);
    assert_eq!(root.diagnostics_list().await.unwrap().len(), 1);
    server.stop().await;
}

/// The SDK sends through the feature, and refuses a log over the cap
/// before sending anything.
#[tokio::test]
async fn the_sdk_sends_through_the_feature() {
    use spoolframe_client_sdk::gate::Diagnostics;
    use spoolframe_client_sdk::{MemoryStorage, SdkError};
    let (server, root, _) = beside("diagnostics-sdk").await;
    let storage = std::sync::Arc::new(MemoryStorage::default());
    let client = crate::sdk::client_on(&server, &storage).await;
    let alice = client
        .sign_in(&server.server.id().to_string(), "alice", PASSWORD, false)
        .await
        .unwrap();
    let on = alice.feature::<Diagnostics>().unwrap();
    let too_long = on
        .send_diagnostics(&report("a".repeat(MAX_LOG_BYTES + 1)))
        .await;
    assert!(
        matches!(&too_long, Err(SdkError::InvalidInput(why)) if why.contains("log")),
        "{too_long:?}"
    );
    let id = on.send_diagnostics(&report("sdk".into())).await.unwrap();
    let listed = root.diagnostics_list().await.unwrap();
    assert_eq!(listed.iter().map(|r| &r.id).collect::<Vec<_>>(), [&id]);
    client.close().await;
    server.stop().await;
}

/// The app reaches it as an object got only with the feature on.
#[tokio::test]
async fn the_ffi_sends_through_the_feature() {
    use spoolframe_client_ffi::SpoolSession;
    use spoolframe_client_sdk::MemoryStorage;
    let (server, root, _) = beside("diagnostics-ffi").await;
    let storage = std::sync::Arc::new(MemoryStorage::default());
    let client = crate::sdk::client_on(&server, &storage).await;
    let alice = client
        .sign_in(&server.server.id().to_string(), "alice", PASSWORD, false)
        .await
        .unwrap();
    let alice = SpoolSession::from(alice);
    assert!(alice.has_feature(Feature::Diagnostics));
    let id = alice
        .diagnostics()
        .unwrap()
        .send(report("ffi".into()))
        .await
        .unwrap();
    assert_eq!(root.diagnostics(id).await.unwrap().log, "ffi");
    assert_eq!(
        spoolframe_client_ffi::diagnostics_log_limit(),
        MAX_LOG_BYTES as u64
    );
    client.close().await;
    server.stop().await;
}
