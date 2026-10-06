//! What many wrong PINs and passwords do (P2-08) to a user's sessions,
//! through [`Auth`] on a clock the test moves past each failure window.
//! Which request counts against which limit stays in `tests/it/pin.rs`.

use spoolframe_types::Platform;

use super::*;
use crate::clock::Clock;

const PASSWORD: &str = "correct horse";
const PIN: &str = "4321";

/// Auth over a store of its own with alice, who has a PIN and is signed
/// in on a TV and a desktop.
struct Pins {
    // Declared first, so the store closes before its directory goes.
    auth: Auth,
    clock: Clock,
    alice: LocalUserId,
    on_tv: SessionToken,
    on_desktop: SessionToken,
    _dir: spoolframe_test_dir::TestDir,
}

fn on(endpoint: &str) -> SessionBinding {
    SessionBinding::Endpoint {
        endpoint_id: endpoint.into(),
        device_name: None,
        platform: Platform::Linux,
    }
}

async fn pins(test: &str) -> Pins {
    let dir = state_dir(&format!("pins-{test}")).await;
    let clock = Clock::default();
    let store = Store::open(dir.as_ref())
        .await
        .unwrap()
        .with_clock(clock.clone());
    let phc = hash_password(PASSWORD).await.unwrap();
    let alice = store
        .add_user("alice", None, &phc, Role::Consumer)
        .await
        .unwrap();
    store
        .set_pin(alice, &hash_pin(PIN).await.unwrap(), PIN.len() as u8)
        .await
        .unwrap();
    let auth = Auth::new(store);
    let mut sessions = Vec::new();
    for endpoint in ["tv", "desktop"] {
        let login = auth
            .login_password("alice", PASSWORD, on(endpoint), Some(endpoint), false)
            .await
            .unwrap();
        sessions.push(login.token);
    }
    let on_desktop = sessions.pop().unwrap();
    let on_tv = sessions.pop().unwrap();
    Pins {
        auth,
        clock,
        alice,
        on_tv,
        on_desktop,
        _dir: dir,
    }
}

impl Pins {
    async fn live_on(&self, token: &SessionToken, endpoint: &str) -> bool {
        self.auth
            .endpoint_session(&token.0, endpoint)
            .await
            .unwrap()
            .is_some()
    }
}

/// Wrong current PINs (as Change PIN and Remove PIN check them) end in
/// the session on that device going after ten in a row, the failure
/// window waited out each time it limits them. Only that device's.
#[tokio::test]
async fn ten_wrong_current_pins_revoke_that_device_s_session() {
    let p = pins("proof-revoke").await;
    let mut wrong = 0;
    let last = loop {
        match p.auth.check_pin(p.alice, "tv", "0000").await {
            Err(UnlockError::RateLimited { .. }) => p.clock.advance(FAILURE_WINDOW),
            Err(UnlockError::Wrong) => wrong += 1,
            Err(e) => break e,
            Ok(()) => panic!("passed with a wrong PIN"),
        }
    };
    assert_eq!(wrong, 9);
    assert!(matches!(last, UnlockError::Revoked), "{last:?}");
    assert!(!p.live_on(&p.on_tv, "tv").await);
    assert!(p.live_on(&p.on_desktop, "desktop").await);
}

/// However many wrong passwords, the session stays: a failed sign-in
/// signs no one out (unlike ten wrong PINs), and the PIN is untouched.
#[tokio::test]
async fn wrong_passwords_never_revoke_the_session() {
    let p = pins("password-revoke").await;
    let mut wrong = 0;
    while wrong < 12 {
        match p
            .auth
            .check_password("alice", "wrong password", Some("tv"))
            .await
        {
            Err(LoginError::RateLimited { .. }) => p.clock.advance(FAILURE_WINDOW),
            Err(LoginError::InvalidCredentials) => wrong += 1,
            other => panic!("not a wrong password: {other:?}"),
        }
    }
    assert!(p.live_on(&p.on_tv, "tv").await);
    assert!(p.auth.store().pin_phc(p.alice).await.unwrap().is_some());
}
