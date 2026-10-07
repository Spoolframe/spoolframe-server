//! How long a web (API) session lasts (A-18): it ends after an hour
//! unused, a use keeps it going, and it never outlives 12 hours from its
//! sign-in. On a clock the test moves instead of sleeping.

use super::*;
use crate::clock::Clock;

const PASSWORD: &str = "correct horse";
const MINUTE: Duration = Duration::from_secs(60);

/// Auth on a clock of its own, with alice signed in to the web UI.
struct Signed {
    // Declared first, so the store closes before its directory goes.
    auth: Auth,
    clock: Clock,
    token: SessionToken,
    _dir: spoolframe_test_dir::TestDir,
}

async fn signed_in(test: &str) -> Signed {
    let dir = state_dir(&format!("api-sessions-{test}")).await;
    let clock = Clock::default();
    let store = Store::open(dir.as_ref())
        .await
        .unwrap()
        .with_clock(clock.clone());
    let phc = hash_password(PASSWORD).await.unwrap();
    store
        .add_user("alice", None, &phc, Role::Consumer)
        .await
        .unwrap();
    let auth = Auth::new(store);
    let token = auth
        .login_password("alice", PASSWORD, SessionBinding::Api, None, false)
        .await
        .unwrap()
        .token;
    Signed {
        auth,
        clock,
        token,
        _dir: dir,
    }
}

impl Signed {
    async fn live(&self) -> bool {
        self.auth
            .api_session(&self.token.0)
            .await
            .unwrap()
            .is_some()
    }
}

#[tokio::test]
async fn a_web_session_left_unused_for_an_hour_ends() {
    let s = signed_in("idle").await;
    s.clock.advance(59 * MINUTE);
    assert!(s.live().await);
    // That use was the last: an hour after it, the session is over.
    s.clock.advance(60 * MINUTE + MINUTE);
    assert!(!s.live().await);
}

#[tokio::test]
async fn a_web_session_in_use_lasts_past_the_idle_time() {
    let s = signed_in("in-use").await;
    for _ in 0..8 {
        s.clock.advance(30 * MINUTE);
        assert!(s.live().await);
    }
}

#[tokio::test]
async fn a_web_session_in_use_still_ends_12_hours_after_its_sign_in() {
    let s = signed_in("in-use-max").await;
    // Used every half hour up to 11 hours 30 minutes.
    for _ in 0..23 {
        s.clock.advance(30 * MINUTE);
        assert!(s.live().await);
    }
    s.clock.advance(31 * MINUTE);
    assert!(!s.live().await);
}

#[tokio::test]
async fn an_unused_web_session_is_not_listed_or_counted() {
    let s = signed_in("idle-listed").await;
    let alice = s.auth.api_session(&s.token.0).await.unwrap().unwrap().user;
    s.clock.advance(61 * MINUTE);
    assert_eq!(s.auth.store().list_sessions(alice).await.unwrap(), []);
    assert_eq!(s.auth.store().live_session_counts().await.unwrap(), []);
}
