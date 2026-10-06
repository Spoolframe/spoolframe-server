//! Device sign-in (P3-07) through [`Auth`] and its [`DeviceSignIns`], as
//! the approve, deny and await requests drive them: claim then
//! [`Auth::approve_device`], [`DeviceSignIns::deny`], and
//! [`DeviceSignIns::wait`] from the endpoint that began it. The requests'
//! own checks (a session, a recent sign-in, the guess limit, the web UI)
//! stay in `tests/it/device_sign_in.rs`.

use std::time::Duration;

use spoolframe_types::Platform;

use super::*;
use crate::clock::Clock;
use crate::device_sign_in::{AWAIT, ClaimError, LIFETIME, Outcome, Started, Waiting};

/// Auth over a store of its own with alice, on a clock the test moves.
struct Approvals {
    // Declared first, so the store closes before its directory goes.
    auth: Auth,
    clock: Clock,
    alice: LocalUserId,
    _dir: spoolframe_test_dir::TestDir,
}

async fn approvals(test: &str) -> Approvals {
    let dir = state_dir(&format!("dsi-{test}")).await;
    let clock = Clock::default();
    let store = Store::open(dir.as_ref())
        .await
        .unwrap()
        .with_clock(clock.clone());
    let alice = store
        .add_user("alice", None, "phc", Role::Consumer)
        .await
        .unwrap();
    Approvals {
        auth: Auth::new(store),
        clock,
        alice,
        _dir: dir,
    }
}

impl Approvals {
    /// The TV, endpoint `tv`, begins a sign-in.
    fn tv_begins(&self) -> Started {
        self.auth
            .sign_ins()
            .begin(Waiting {
                address: None,
                endpoint: "tv".into(),
                device_name: Some("Living room TV".into()),
                client_name: "test".into(),
                platform: Platform::Tvos,
                requested_at: self.auth.store().now(),
                remember: false,
            })
            .expect("room for a sign-in")
    }

    /// Alice approves `code` from her phone, as `ApproveDeviceSignIn`
    /// does once her session is checked: claim it, then sign the device
    /// in. `None` if the code could not be claimed.
    async fn approve(&self, code: &str) -> Option<Approval> {
        let claim = self.auth.sign_ins().claim(code, Some("phone")).ok()?;
        Some(
            self.auth
                .approve_device(self.alice, &claim, false)
                .await
                .unwrap(),
        )
    }

    /// What the device at `endpoint` waiting on `request` is told.
    async fn wait(&self, request: &str, endpoint: &str) -> Outcome {
        self.auth.sign_ins().wait(request, endpoint, AWAIT).await
    }

    /// The TV waiting on `request`, in the background.
    fn tv_awaits(&self, request: &str) -> tokio::task::JoinHandle<Outcome> {
        let (auth, request) = (self.auth.clone(), request.to_owned());
        tokio::spawn(async move { auth.sign_ins().wait(&request, "tv", AWAIT).await })
    }

    /// Alice's sessions on the TV.
    async fn sessions_on_tv(&self) -> usize {
        self.auth
            .store()
            .list_sessions(self.alice)
            .await
            .unwrap()
            .iter()
            .filter(|s| s.device_name.as_deref() == Some("Living room TV"))
            .count()
    }
}

#[tokio::test]
async fn a_code_is_used_once() {
    let a = approvals("once").await;
    let started = a.tv_begins();
    assert_eq!(a.approve(&started.code).await, Some(Approval::SignedIn));
    assert_eq!(
        a.auth.sign_ins().claim(&started.code, Some("phone")),
        Err(ClaimError::Unknown)
    );
    // The session is handed over once, and is the TV's.
    let Outcome::Approved(handover) = a.wait(&started.request, "tv").await else {
        panic!("not approved");
    };
    let on_tv = a.auth.endpoint_session(&handover.token.0, "tv").await;
    assert_eq!(on_tv.unwrap().unwrap().user, a.alice);
    assert_eq!(a.wait(&started.request, "tv").await, Outcome::Expired);
}

#[tokio::test]
async fn a_denied_sign_in_ends_and_cannot_be_approved_after() {
    let a = approvals("deny").await;
    let started = a.tv_begins();
    let waiting = a.tv_awaits(&started.request);
    assert!(a.auth.sign_ins().deny(&started.code));
    assert_eq!(waiting.await.unwrap(), Outcome::Denied);
    assert_eq!(a.approve(&started.code).await, None);
    assert!(!a.auth.sign_ins().deny(&started.code));
}

#[tokio::test]
async fn only_the_device_that_began_it_gets_the_session() {
    let a = approvals("bound").await;
    let started = a.tv_begins();
    // Another device with the request id (seen over a shoulder, say).
    assert_eq!(a.wait(&started.request, "thief").await, Outcome::Expired);
    assert_eq!(a.approve(&started.code).await, Some(Approval::SignedIn));
    assert_eq!(a.wait(&started.request, "thief").await, Outcome::Expired);
    assert!(matches!(
        a.wait(&started.request, "tv").await,
        Outcome::Approved(_)
    ));
}

#[tokio::test]
async fn a_code_expires() {
    let a = approvals("expire").await;
    let started = a.tv_begins();
    a.clock.advance(LIFETIME - Duration::from_secs(1));
    assert!(a.auth.sign_ins().lookup(&started.code).is_some());
    a.clock.advance(Duration::from_secs(1));
    assert_eq!(a.wait(&started.request, "tv").await, Outcome::Expired);
    assert_eq!(a.approve(&started.code).await, None);
}

/// Many approvals of one code at once, as a double tap or two phones
/// send them: one signs the TV in, and one session is made.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn approvals_of_one_code_at_once_sign_in_once() {
    let a = std::sync::Arc::new(approvals("race-approve").await);
    let started = a.tv_begins();
    let approvals: Vec<_> = (0..8)
        .map(|_| {
            let (a, code) = (a.clone(), started.code.clone());
            tokio::spawn(async move { a.approve(&code).await })
        })
        .collect();
    let mut signed_in = 0;
    for approval in approvals {
        match approval.await.unwrap() {
            Some(Approval::SignedIn) => signed_in += 1,
            // The losers find the code gone.
            None => {}
            Some(other) => panic!("{other:?}"),
        }
    }
    assert_eq!(signed_in, 1);
    assert!(matches!(
        a.wait(&started.request, "tv").await,
        Outcome::Approved(_)
    ));
    assert_eq!(a.sessions_on_tv().await, 1);
}

/// An approval and a denial of one code at once: the TV is told the one
/// that won, and the other is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_approval_and_a_denial_at_once_have_one_outcome() {
    let a = std::sync::Arc::new(approvals("race-deny").await);
    let mut approved_tvs = 0;
    for _ in 0..4 {
        let started = a.tv_begins();
        let approval = {
            let (a, code) = (a.clone(), started.code.clone());
            tokio::spawn(async move { a.approve(&code).await })
        };
        let denied = a.auth.sign_ins().deny(&started.code);
        let approval = approval.await.unwrap();
        let told = a.wait(&started.request, "tv").await;
        match (approval, denied) {
            (Some(Approval::SignedIn), false) => {
                assert!(matches!(told, Outcome::Approved(_)), "{told:?}");
                approved_tvs += 1;
            }
            (None, true) => assert_eq!(told, Outcome::Denied),
            other => panic!("not one outcome: {other:?}"),
        }
    }
    assert_eq!(a.sessions_on_tv().await, approved_tvs);
}

/// A code claimed by an approver, then run out before its session was
/// made: the TV was told it expired, so the approver is told so too, and
/// no session is left that nobody holds.
#[tokio::test]
async fn an_approval_outrun_by_the_code_leaves_no_session() {
    let a = approvals("race-expire").await;
    let started = a.tv_begins();
    let claim = a.auth.sign_ins().claim(&started.code, None).unwrap();
    a.clock.advance(LIFETIME);
    assert_eq!(a.wait(&started.request, "tv").await, Outcome::Expired);
    assert_eq!(
        a.auth.approve_device(a.alice, &claim, false).await.unwrap(),
        Approval::Expired
    );
    assert_eq!(a.sessions_on_tv().await, 0);
}
