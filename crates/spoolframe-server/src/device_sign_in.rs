//! Device sign-ins waiting to be approved on another device (P3-07).
//!
//! A TV that cannot type a password begins one and shows its code; a
//! signed-in phone approves the code, and the TV, waiting on the request,
//! gets a session for itself. The request id is the TV's alone, and only
//! the endpoint that began it may wait on it; the code is what a person
//! carries between the two, so it is short, dies in [`LIFETIME`], is used
//! once, and guessing it is rate-limited by the caller.
//!
//! The approver is a phone with the app, over the client protocol, or any
//! browser signed in to the web UI (P3-05): the device's QR code can carry
//! the page that approves its code, so a phone needs no app, only a way
//! to sign in there.
//!
//! Pending sign-ins are only in memory: a restart drops them, and the TV
//! begins again.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use password_hash::rand_core::{OsRng, RngCore};
use spoolframe_protocol::device_sign_in::{ALPHABET, CODE_LEN};
use spoolframe_types::Platform;
use tokio::sync::watch;

use crate::auth::SessionToken;

/// How long a code can be approved.
pub const LIFETIME: Duration = Duration::from_secs(5 * 60);

/// The longest one `AwaitDeviceSignIn` waits before `Pending`: short of
/// any idle timeout between the TV and the server.
pub const AWAIT: Duration = Duration::from_secs(25);

/// Sign-ins one device may have pending at once.
pub const MAX_PENDING_PER_DEVICE: usize = 3;

/// A sign-in that was begun.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Started {
    pub request: String,
    pub code: String,
    pub expires_in: Duration,
}

/// The device behind a code, for the approver to confirm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waiting {
    pub endpoint: String,
    pub device_name: Option<String>,
    pub client_name: String,
    pub platform: Platform,
    /// Unix seconds.
    pub requested_at: i64,
}

/// A code taken for approval: the session goes to `endpoint`, then
/// [`DeviceSignIns::fulfil`] hands it over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub request: String,
    pub endpoint: String,
    pub device_name: Option<String>,
    pub platform: Platform,
}

/// Why a code could not be claimed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimError {
    /// Not a pending code: a guess, a typo, or one already used or expired.
    Unknown,
    /// The device approving itself.
    SameDevice,
}

/// What the waiting device learns.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Approved(SessionToken),
    /// Not yet; ask again.
    Pending,
    Denied,
    /// Timed out, failed, or not this device's to wait on.
    Expired,
}

#[derive(Debug, Clone)]
enum State {
    Waiting,
    Approved(SessionToken),
    Denied,
    Failed,
}

#[derive(Debug)]
struct Pending {
    waiting: Waiting,
    code: String,
    expires_at: Instant,
    state: watch::Sender<State>,
}

#[derive(Debug, Default)]
struct Inner {
    by_request: HashMap<String, Pending>,
    /// Codes that can still be claimed, to their request.
    by_code: HashMap<String, String>,
}

impl Inner {
    fn sweep(&mut self, now: Instant) {
        self.by_request.retain(|_, p| p.expires_at > now);
        let live = &self.by_request;
        self.by_code.retain(|_, request| live.contains_key(request));
    }
}

#[derive(Debug)]
pub struct DeviceSignIns {
    inner: Mutex<Inner>,
    lifetime: Duration,
}

impl Default for DeviceSignIns {
    fn default() -> Self {
        Self::with_lifetime(LIFETIME)
    }
}

impl DeviceSignIns {
    /// Codes live `lifetime` ([`LIFETIME`]; tests shorten it).
    pub fn with_lifetime(lifetime: Duration) -> Self {
        Self {
            inner: Mutex::default(),
            lifetime,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        let mut inner = self.inner.lock().expect("never held across a panic");
        inner.sweep(Instant::now());
        inner
    }

    /// Begin a sign-in for `waiting.endpoint`; `None` if it already has
    /// [`MAX_PENDING_PER_DEVICE`] pending.
    pub fn begin(&self, waiting: Waiting) -> Option<Started> {
        let mut inner = self.lock();
        let pending = inner
            .by_request
            .values()
            .filter(|p| p.waiting.endpoint == waiting.endpoint)
            .count();
        if pending >= MAX_PENDING_PER_DEVICE {
            return None;
        }
        let code = loop {
            let code = new_code();
            if !inner.by_code.contains_key(&code) {
                break code;
            }
        };
        let request = SessionToken::generate().0;
        inner.by_code.insert(code.clone(), request.clone());
        inner.by_request.insert(
            request.clone(),
            Pending {
                waiting,
                code: code.clone(),
                expires_at: Instant::now() + self.lifetime,
                state: watch::Sender::new(State::Waiting),
            },
        );
        Some(Started {
            request,
            code,
            expires_in: self.lifetime,
        })
    }

    /// The device waiting on `code`, if it can still be approved.
    pub fn lookup(&self, code: &str) -> Option<Waiting> {
        let inner = self.lock();
        let request = inner.by_code.get(code)?;
        Some(inner.by_request[request].waiting.clone())
    }

    /// Take `code` to approve from `approver`: from now on nobody else can
    /// claim or deny it. The approver is an endpoint, or `None` for a web
    /// session, which is no endpoint and so never the one waiting.
    pub fn claim(&self, code: &str, approver: Option<&str>) -> Result<Claim, ClaimError> {
        let mut inner = self.lock();
        let request = inner.by_code.get(code).ok_or(ClaimError::Unknown)?;
        let waiting = &inner.by_request[request].waiting;
        if approver == Some(waiting.endpoint.as_str()) {
            return Err(ClaimError::SameDevice);
        }
        let claim = Claim {
            request: request.clone(),
            endpoint: waiting.endpoint.clone(),
            device_name: waiting.device_name.clone(),
            platform: waiting.platform,
        };
        inner.by_code.remove(code);
        Ok(claim)
    }

    /// Hand the claimed sign-in its session, or `None` to fail it (the
    /// session could not be made): the device stops waiting either way.
    pub fn fulfil(&self, request: &str, token: Option<SessionToken>) {
        let inner = self.lock();
        if let Some(pending) = inner.by_request.get(request) {
            pending.state.send_replace(match token {
                Some(token) => State::Approved(token),
                None => State::Failed,
            });
        }
    }

    /// Refuse `code`; `false` if it is not pending.
    pub fn deny(&self, code: &str) -> bool {
        let mut inner = self.lock();
        let Some(request) = inner.by_code.remove(code) else {
            return false;
        };
        inner.by_request[&request].state.send_replace(State::Denied);
        true
    }

    /// Wait up to `max` for `request`, begun by `endpoint`, to be decided.
    /// A decided request is gone once told: its session is handed out once.
    pub async fn wait(&self, request: &str, endpoint: &str, max: Duration) -> Outcome {
        let (mut state, expires_at) = {
            let inner = self.lock();
            match inner.by_request.get(request) {
                Some(p) if p.waiting.endpoint == endpoint => (p.state.subscribe(), p.expires_at),
                // Not this device's: as if it were not there at all.
                _ => return Outcome::Expired,
            }
        };
        let deadline = (Instant::now() + max).min(expires_at);
        let decided = tokio::time::timeout_at(
            deadline.into(),
            state.wait_for(|s| !matches!(s, State::Waiting)),
        )
        .await;
        match decided {
            Ok(Ok(_)) => self.take(request),
            // Dropped by a sweep: it expired.
            Ok(Err(_)) => Outcome::Expired,
            Err(_) if deadline >= expires_at => Outcome::Expired,
            Err(_) => Outcome::Pending,
        }
    }

    fn take(&self, request: &str) -> Outcome {
        let mut inner = self.lock();
        let Some(pending) = inner.by_request.remove(request) else {
            return Outcome::Expired;
        };
        inner.by_code.remove(&pending.code);
        match pending.state.borrow().clone() {
            State::Approved(token) => Outcome::Approved(token),
            State::Denied => Outcome::Denied,
            State::Failed => Outcome::Expired,
            State::Waiting => unreachable!("taken once decided"),
        }
    }
}

/// [`CODE_LEN`] letters of [`ALPHABET`], each uniform: bytes past the last
/// whole multiple of its length are drawn again.
fn new_code() -> String {
    let limit = 256 - 256 % ALPHABET.len();
    let mut code = String::with_capacity(CODE_LEN);
    while code.len() < CODE_LEN {
        let mut byte = [0u8];
        OsRng.fill_bytes(&mut byte);
        let b = usize::from(byte[0]);
        if b < limit {
            code.push(char::from(ALPHABET[b % ALPHABET.len()]));
        }
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tv(endpoint: &str) -> Waiting {
        Waiting {
            endpoint: endpoint.into(),
            device_name: Some("Living room TV".into()),
            client_name: "spoolframe-tvos".into(),
            platform: Platform::Tvos,
            requested_at: 0,
        }
    }

    #[test]
    fn codes_are_of_the_alphabet() {
        for _ in 0..100 {
            let code = new_code();
            assert_eq!(
                spoolframe_protocol::device_sign_in::normalize(&code).as_deref(),
                Some(code.as_str())
            );
        }
    }

    #[tokio::test]
    async fn an_approved_sign_in_is_handed_over_once() {
        let sign_ins = DeviceSignIns::default();
        let started = sign_ins.begin(tv("tv")).unwrap();
        let claim = sign_ins.claim(&started.code, Some("phone")).unwrap();
        assert_eq!(claim.endpoint, "tv");
        // The session made for it is on a device of the platform it said.
        assert_eq!(claim.platform, Platform::Tvos);
        // Claimed: nobody can claim, deny or look it up again.
        assert_eq!(
            sign_ins.claim(&started.code, Some("phone")),
            Err(ClaimError::Unknown)
        );
        assert!(!sign_ins.deny(&started.code));
        assert_eq!(sign_ins.lookup(&started.code), None);
        sign_ins.fulfil(&claim.request, Some(SessionToken("t".into())));
        assert_eq!(
            sign_ins.wait(&started.request, "tv", AWAIT).await,
            Outcome::Approved(SessionToken("t".into()))
        );
        assert_eq!(
            sign_ins.wait(&started.request, "tv", AWAIT).await,
            Outcome::Expired
        );
    }

    #[tokio::test]
    async fn only_the_device_that_began_it_may_wait() {
        let sign_ins = DeviceSignIns::default();
        let started = sign_ins.begin(tv("tv")).unwrap();
        assert_eq!(
            sign_ins.wait(&started.request, "elsewhere", AWAIT).await,
            Outcome::Expired
        );
    }

    #[test]
    fn a_device_cannot_approve_itself() {
        let sign_ins = DeviceSignIns::default();
        let started = sign_ins.begin(tv("tv")).unwrap();
        assert_eq!(
            sign_ins.claim(&started.code, Some("tv")),
            Err(ClaimError::SameDevice)
        );
        // A web session is no device at all.
        assert_eq!(sign_ins.claim(&started.code, None).unwrap().endpoint, "tv");
    }

    #[test]
    fn a_device_has_few_pending() {
        let sign_ins = DeviceSignIns::default();
        for _ in 0..MAX_PENDING_PER_DEVICE {
            assert!(sign_ins.begin(tv("tv")).is_some());
        }
        assert!(sign_ins.begin(tv("tv")).is_none());
        assert!(sign_ins.begin(tv("other tv")).is_some());
    }

    #[tokio::test]
    async fn waiting_is_pending_until_decided_and_expired_after() {
        let sign_ins = DeviceSignIns::with_lifetime(Duration::from_millis(300));
        let started = sign_ins.begin(tv("tv")).unwrap();
        assert_eq!(
            sign_ins
                .wait(&started.request, "tv", Duration::from_millis(50))
                .await,
            Outcome::Pending
        );
        assert_eq!(
            sign_ins.wait(&started.request, "tv", AWAIT).await,
            Outcome::Expired
        );
        assert_eq!(sign_ins.lookup(&started.code), None);
    }
}
