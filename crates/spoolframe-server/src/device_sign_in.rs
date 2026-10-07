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

use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use crate::clock::Clock;

use password_hash::rand_core::{OsRng, RngCore};
use spoolframe_protocol::device_sign_in::{ALPHABET, CODE_LEN};
use spoolframe_protocol::{AddressNetwork, DeviceAddress};
use spoolframe_types::Platform;
use tokio::sync::watch;

use crate::auth::SessionToken;

/// How long a code can be approved.
pub const LIFETIME: Duration = Duration::from_secs(5 * 60);

/// The longest one `AwaitDeviceSignIn` waits before `Pending`: short of
/// any idle timeout between the TV and the server.
pub const AWAIT: Duration = spoolframe_protocol::policy::wait::SIGN_IN;

/// Sign-ins one device may have pending at once.
pub const MAX_PENDING_PER_DEVICE: usize = 3;

/// Sign-ins pending at once on the server (PX-10, finding 16). Anyone can
/// begin one from a new endpoint id; a household has a few devices
/// signing in at a time.
pub const MAX_PENDING: usize = 256;

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
    /// The device asked to be trusted (P4-01); the approver decides.
    pub remember: bool,
    /// The address its request came from, if the server could tell: what
    /// the approver is shown (PX-09 finding 27).
    pub address: Option<IpAddr>,
}

/// Whether `ip` is a public address: not loopback, a private or
/// link-local network's, CGNAT's or a VPN's (`100.64.0.0/10`), an IPv6
/// unique-local one, or on one of the server's own networks. A device signing in from one is not on a network
/// the approver shares, which is what device-code phishing looks like:
/// the code is someone else's, sent to the approver as a link.
pub fn from_internet(ip: IpAddr) -> bool {
    network(ip) == AddressNetwork::Internet
}

/// What kind of network `ip` is on, for an approver (PX-09 finding 27):
/// the web approval page's `from_internet` and the apps' [`DeviceAddress`]
/// both come from here. It reads the server's interfaces now: sign-ins are
/// rare, and an address that changed since is judged as it is.
pub fn network(ip: IpAddr) -> AddressNetwork {
    network_with(ip, &own_networks())
}

/// [`network`], with the server's own on-link networks given as
/// `(address, prefix length)`. An address inside one of them is `Local`
/// whatever its kind: a home with IPv6 gives its devices global addresses
/// in the server's own /64, and a device there is in the same home. Only
/// the server's own prefixes count, never a guess at the ISP's /56 or /48,
/// which other homes share. The unspecified address, which no device sends
/// from, is not called public.
pub fn network_with(ip: IpAddr, own: &[(IpAddr, u8)]) -> AddressNetwork {
    let ip = ip.to_canonical();
    if ip.is_loopback() {
        return AddressNetwork::Loopback;
    }
    if own
        .iter()
        .any(|&(addr, len)| same_network(ip, addr.to_canonical(), len))
    {
        return AddressNetwork::Local;
    }
    match ip {
        IpAddr::V4(v4) => {
            let shared = v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64;
            if v4.is_private() || v4.is_link_local() || v4.is_unspecified() || shared {
                AddressNetwork::Local
            } else {
                AddressNetwork::Internet
            }
        }
        IpAddr::V6(v6) => {
            if v6.is_unspecified() || v6.is_unique_local() || v6.is_unicast_link_local() {
                AddressNetwork::Local
            } else {
                AddressNetwork::Internet
            }
        }
    }
}

/// Whether `ip` is in `own`'s network of `len` bits. A prefix of 0 is
/// every address, which no interface's network is, so it matches none.
fn same_network(ip: IpAddr, own: IpAddr, len: u8) -> bool {
    match (ip, own) {
        (IpAddr::V4(ip), IpAddr::V4(own)) if (1..=32).contains(&len) => {
            let mask = u32::MAX << (32 - u32::from(len));
            u32::from(ip) & mask == u32::from(own) & mask
        }
        (IpAddr::V6(ip), IpAddr::V6(own)) if (1..=128).contains(&len) => {
            let mask = u128::MAX << (128 - u32::from(len));
            u128::from(ip) & mask == u128::from(own) & mask
        }
        _ => false,
    }
}

/// The server's on-link networks now: each address on an interface that
/// is up, with its netmask's prefix length. With no netmask an IPv6
/// address is taken as a /64, what SLAAC hands out, and an IPv4 one as
/// itself alone. Kept apart from [`crate::announce::interfaces`], which
/// does not read netmasks.
fn own_networks() -> Vec<(IpAddr, u8)> {
    let mut found = Vec::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `head` with a list it allocates, which is
    // read only until the freeifaddrs below; each entry's address and
    // netmask are read as the struct the address's family names.
    unsafe {
        if libc::getifaddrs(&mut head) != 0 {
            return found;
        }
        let mut entry = head;
        while let Some(ifa) = entry.as_ref() {
            entry = ifa.ifa_next;
            if ifa.ifa_addr.is_null() || ifa.ifa_flags & libc::IFF_UP as u32 == 0 {
                continue;
            }
            let mask = ifa.ifa_netmask;
            let own = match i32::from((*ifa.ifa_addr).sa_family) {
                libc::AF_INET => {
                    let sa = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                    let len = if mask.is_null() {
                        32
                    } else {
                        let m = &*(mask as *const libc::sockaddr_in);
                        u32::from_be(m.sin_addr.s_addr).count_ones() as u8
                    };
                    (IpAddr::from(sa.sin_addr.s_addr.to_ne_bytes()), len)
                }
                libc::AF_INET6 => {
                    let sa = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                    let len = if mask.is_null() {
                        64
                    } else {
                        let m = &*(mask as *const libc::sockaddr_in6);
                        u128::from_be_bytes(m.sin6_addr.s6_addr).count_ones() as u8
                    };
                    (IpAddr::from(sa.sin6_addr.s6_addr), len)
                }
                _ => continue,
            };
            found.push(own);
        }
        libc::freeifaddrs(head);
    }
    found
}

/// `ip` as an approving app is shown it: an IPv4 address never in its
/// IPv6-mapped form, as the web page shows it.
pub fn device_address(ip: IpAddr) -> DeviceAddress {
    DeviceAddress {
        address: ip.to_canonical().to_string(),
        network: network(ip),
    }
}

/// A code taken for approval: the session goes to `endpoint`, then
/// [`DeviceSignIns::fulfil`] hands it over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub request: String,
    pub endpoint: String,
    pub device_name: Option<String>,
    pub platform: Platform,
    /// The device asked to be trusted (P4-01).
    pub remember: bool,
}

/// What an approved device is handed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handover {
    pub token: SessionToken,
    /// The device is trusted for the approver after it (P4-01).
    pub remembered: bool,
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
    Approved(Handover),
    /// Not yet; ask again.
    Pending,
    Denied,
    /// Timed out, failed, or not this device's to wait on.
    Expired,
}

/// Where a pending sign-in is. Every step is taken under the one lock and
/// only from the state before it, so one code has one outcome: two
/// approvals, an approval and a denial, or an approval and the code running
/// out cannot both pass.
#[derive(Debug, Clone)]
enum State {
    /// Its code can be claimed or denied.
    Waiting,
    /// Claimed by an approver making its session; only
    /// [`DeviceSignIns::fulfil`] moves it on.
    Claimed,
    Approved(Handover),
    Denied,
    Failed,
}

impl State {
    /// What the waiting device is told, once it is decided.
    fn outcome(&self) -> Option<Outcome> {
        match self {
            State::Waiting | State::Claimed => None,
            State::Approved(handover) => Some(Outcome::Approved(handover.clone())),
            State::Denied => Some(Outcome::Denied),
            State::Failed => Some(Outcome::Expired),
        }
    }
}

/// How long a decided sign-in waits for its device to come and be told,
/// past the code's own end: decided a moment before it, the device may be
/// between two waits.
const HANDOVER: Duration = AWAIT;

#[derive(Debug)]
struct Pending {
    waiting: Waiting,
    code: String,
    expires_at: Instant,
    state: watch::Sender<State>,
    /// How many waits it has had: the latest is the one waiting
    /// ([`DeviceSignIns::wait`]).
    waits: watch::Sender<u64>,
}

impl Pending {
    fn decide(&mut self, state: State, now: Instant) {
        self.expires_at = self.expires_at.max(now + HANDOVER);
        self.state.send_replace(state);
    }
}

#[derive(Debug, Default)]
struct Inner {
    by_request: HashMap<String, Pending>,
    /// Codes that can still be claimed, to their request: those `Waiting`.
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
    /// What codes expire by (PX-09).
    clock: Clock,
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
            clock: Clock::default(),
        }
    }

    /// Codes expire by `clock` (tests move it instead of sleeping).
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// How long a code lives.
    pub fn lifetime(&self) -> Duration {
        self.lifetime
    }

    fn lock(&self) -> parking_lot::MutexGuard<'_, Inner> {
        let mut inner = self.inner.lock();
        inner.sweep(self.clock.now());
        inner
    }

    /// Begin a sign-in for `waiting.endpoint`; `None` if it already has
    /// [`MAX_PENDING_PER_DEVICE`] pending, or the server [`MAX_PENDING`].
    pub fn begin(&self, waiting: Waiting) -> Option<Started> {
        let mut inner = self.lock();
        if inner.by_request.len() >= MAX_PENDING {
            return None;
        }
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
                expires_at: self.clock.now() + self.lifetime,
                state: watch::Sender::new(State::Waiting),
                waits: watch::Sender::new(0),
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
        let request = inner.by_code.get(code).ok_or(ClaimError::Unknown)?.clone();
        let pending = inner
            .by_request
            .get_mut(&request)
            .expect("swept with its code");
        if approver == Some(pending.waiting.endpoint.as_str()) {
            return Err(ClaimError::SameDevice);
        }
        pending.state.send_replace(State::Claimed);
        let claim = Claim {
            request,
            endpoint: pending.waiting.endpoint.clone(),
            device_name: pending.waiting.device_name.clone(),
            platform: pending.waiting.platform,
            remember: pending.waiting.remember,
        };
        inner.by_code.remove(code);
        Ok(claim)
    }

    /// Hand the claimed sign-in its session, or `None` to fail it (the
    /// session could not be made): the device stops waiting either way.
    /// `false` if the device will never be told: the code ran out, and the
    /// device was told so, before the session was made. The caller then
    /// revokes the session, which nobody holds.
    #[must_use]
    pub fn fulfil(&self, request: &str, handover: Option<Handover>) -> bool {
        let mut inner = self.lock();
        match inner.by_request.get_mut(request) {
            Some(pending) if matches!(*pending.state.borrow(), State::Claimed) => {
                pending.decide(
                    match handover {
                        Some(handover) => State::Approved(handover),
                        None => State::Failed,
                    },
                    self.clock.now(),
                );
                true
            }
            _ => false,
        }
    }

    /// Refuse `code`; `false` if it is not pending.
    pub fn deny(&self, code: &str) -> bool {
        let mut inner = self.lock();
        let Some(request) = inner.by_code.remove(code) else {
            return false;
        };
        let pending = inner
            .by_request
            .get_mut(&request)
            .expect("swept with its code");
        pending.decide(State::Denied, self.clock.now());
        true
    }

    /// Wait up to `max` for `request`, begun by `endpoint`, to be decided.
    /// A decided request is gone once told: its session is handed out once.
    ///
    /// `max` is a real wait, on the runtime's timer; the code's expiry is
    /// by the clock, so the wait ends at whichever of the two comes first.
    ///
    /// A request is waited on once at a time: a newer wait ends this one as
    /// [`Outcome::Pending`]. Each wait holds an operation in the anonymous
    /// pool for its long poll, so a device can hold no more of it than it
    /// has requests pending (finding 48).
    pub async fn wait(&self, request: &str, endpoint: &str, max: Duration) -> Outcome {
        let until = tokio::time::Instant::now() + max;
        let (mut state, mut waits, this) = {
            let inner = self.lock();
            match inner.by_request.get(request) {
                Some(p) if p.waiting.endpoint == endpoint => {
                    p.waits.send_modify(|n| *n += 1);
                    let waits = p.waits.subscribe();
                    let this = *waits.borrow();
                    (p.state.subscribe(), waits, this)
                }
                // Not this device's: as if it were not there at all.
                _ => return Outcome::Expired,
            }
        };
        loop {
            let Some(expires_at) = self.lock().by_request.get(request).map(|p| p.expires_at) else {
                return Outcome::Expired;
            };
            let left = expires_at.saturating_duration_since(self.clock.now());
            let decided =
                tokio::time::timeout_at(until.min(tokio::time::Instant::now() + left), async {
                    tokio::select! {
                        // A newer wait is told the outcome instead.
                        biased;
                        Ok(_) = waits.wait_for(|n| *n != this) => None,
                        decided = state.wait_for(|s| s.outcome().is_some()) => {
                            Some(decided.map(|_| ()))
                        }
                    }
                })
                .await;
            match decided {
                Ok(None) => return Outcome::Pending,
                // Dropped by a sweep: it expired.
                Ok(Some(Err(_))) => return Outcome::Expired,
                // Decided, or out of time: which of the two is settled
                // under the lock, against an approver deciding it now.
                Ok(Some(Ok(()))) | Err(_) => match self.take(request) {
                    Some(outcome) => return outcome,
                    None if tokio::time::Instant::now() >= until => return Outcome::Pending,
                    None => continue,
                },
            }
        }
    }

    /// The outcome of `request`, removing it, if it is decided; `Expired`
    /// if it ran out undecided. `None` while it is still undecided.
    fn take(&self, request: &str) -> Option<Outcome> {
        let mut inner = self.lock();
        let Some(pending) = inner.by_request.get(request) else {
            return Some(Outcome::Expired);
        };
        let outcome = pending.state.borrow().outcome()?;
        let pending = inner.by_request.remove(request).expect("just seen");
        inner.by_code.remove(&pending.code);
        Some(outcome)
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

    fn handover(token: &str) -> Handover {
        Handover {
            token: SessionToken(token.into()),
            remembered: false,
        }
    }

    fn tv(endpoint: &str) -> Waiting {
        Waiting {
            address: None,
            endpoint: endpoint.into(),
            device_name: Some("Living room TV".into()),
            client_name: "spoolframe-tvos".into(),
            platform: Platform::Tvos,
            requested_at: 0,
            remember: false,
        }
    }

    #[test]
    fn the_approver_learns_whether_the_device_asked_to_be_remembered() {
        let sign_ins = DeviceSignIns::default();
        let asked = sign_ins
            .begin(Waiting {
                address: None,
                remember: true,
                ..tv("tv")
            })
            .unwrap();
        let did_not = sign_ins.begin(tv("laptop")).unwrap();
        assert!(sign_ins.lookup(&asked.code).unwrap().remember);
        assert!(sign_ins.claim(&asked.code, None).unwrap().remember);
        assert!(!sign_ins.claim(&did_not.code, None).unwrap().remember);
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
        assert!(sign_ins.fulfil(&claim.request, Some(handover("t"))));
        assert_eq!(
            sign_ins.wait(&started.request, "tv", AWAIT).await,
            Outcome::Approved(handover("t"))
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

    #[test]
    fn the_server_holds_few_pending_sign_ins_in_all() {
        let sign_ins = DeviceSignIns::default();
        for i in 0..MAX_PENDING {
            assert!(sign_ins.begin(tv(&format!("tv-{i}"))).is_some());
        }
        assert!(sign_ins.begin(tv("one more tv")).is_none());
    }

    /// Approvers on many threads at once: one claim, whatever the order.
    #[test]
    fn one_code_is_claimed_once_by_concurrent_approvers() {
        let sign_ins = std::sync::Arc::new(DeviceSignIns::default());
        let started = sign_ins.begin(tv("tv")).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
        let claims: Vec<_> = (0..16)
            .map(|i| {
                let (sign_ins, barrier, code) =
                    (sign_ins.clone(), barrier.clone(), started.code.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    sign_ins.claim(&code, Some(&format!("phone {i}"))).is_ok()
                })
            })
            .collect();
        let won = claims
            .into_iter()
            .map(|c| c.join().unwrap())
            .filter(|won| *won)
            .count();
        assert_eq!(won, 1);
    }

    /// An approval and a denial at once: one of them decides.
    #[test]
    fn a_code_is_approved_or_denied_never_both() {
        for _ in 0..200 {
            let sign_ins = std::sync::Arc::new(DeviceSignIns::default());
            let started = sign_ins.begin(tv("tv")).unwrap();
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let approve = {
                let (sign_ins, barrier, code) =
                    (sign_ins.clone(), barrier.clone(), started.code.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    sign_ins.claim(&code, Some("phone")).is_ok()
                })
            };
            barrier.wait();
            let denied = sign_ins.deny(&started.code);
            let approved = approve.join().unwrap();
            assert!(approved != denied, "approved {approved}, denied {denied}");
        }
    }

    /// Approved a moment before the code ran out: the device, between two
    /// waits then, still gets the session the approver was told it got.
    #[tokio::test]
    async fn an_approval_just_before_expiry_is_handed_over() {
        let clock = Clock::default();
        // Shorter than the handover, which then keeps it.
        let lifetime = HANDOVER / 2;
        let sign_ins = DeviceSignIns::with_lifetime(lifetime).with_clock(clock.clone());
        let started = sign_ins.begin(tv("tv")).unwrap();
        let claim = sign_ins.claim(&started.code, Some("phone")).unwrap();
        assert!(sign_ins.fulfil(&claim.request, Some(handover("t"))));
        clock.advance(lifetime * 3 / 2);
        assert_eq!(
            sign_ins.wait(&started.request, "tv", AWAIT).await,
            Outcome::Approved(handover("t"))
        );
    }

    /// Claimed, but the code ran out before the session was made: the
    /// device has been told it expired, so the approver must hear so too.
    #[tokio::test]
    async fn an_approval_after_expiry_is_not_handed_over() {
        let clock = Clock::default();
        let sign_ins = DeviceSignIns::default().with_clock(clock.clone());
        let started = sign_ins.begin(tv("tv")).unwrap();
        let claim = sign_ins.claim(&started.code, Some("phone")).unwrap();
        clock.advance(LIFETIME);
        assert_eq!(
            sign_ins.wait(&started.request, "tv", AWAIT).await,
            Outcome::Expired
        );
        assert!(!sign_ins.fulfil(&claim.request, Some(handover("t"))));
    }

    /// A session is handed over once, also to two waits at once.
    #[tokio::test]
    async fn two_waits_get_one_session() {
        let sign_ins = std::sync::Arc::new(DeviceSignIns::default());
        let started = sign_ins.begin(tv("tv")).unwrap();
        let waits: Vec<_> = (0..2)
            .map(|_| {
                let (sign_ins, request) = (sign_ins.clone(), started.request.clone());
                tokio::spawn(async move { sign_ins.wait(&request, "tv", AWAIT).await })
            })
            .collect();
        let claim = sign_ins.claim(&started.code, None).unwrap();
        assert!(sign_ins.fulfil(&claim.request, Some(handover("t"))));
        let mut outcomes = Vec::new();
        for wait in waits {
            outcomes.push(wait.await.unwrap());
        }
        let approved = outcomes
            .iter()
            .filter(|o| matches!(o, Outcome::Approved(_)))
            .count();
        assert_eq!(approved, 1, "{outcomes:?}");
    }

    /// Finding 48: each wait holds an operation in the anonymous pool for
    /// its whole long poll. One sign-in is waited on once at a time, so a
    /// device cannot keep more waits than it has sign-ins: a newer wait
    /// ends the one before it as still pending.
    #[tokio::test(start_paused = true)]
    async fn a_newer_wait_ends_the_one_before_it() {
        let sign_ins = std::sync::Arc::new(DeviceSignIns::default());
        let started = sign_ins.begin(tv("tv")).unwrap();
        let first = {
            let (sign_ins, request) = (sign_ins.clone(), started.request.clone());
            tokio::spawn(async move { sign_ins.wait(&request, "tv", AWAIT).await })
        };
        tokio::task::yield_now().await;
        let second = {
            let (sign_ins, request) = (sign_ins.clone(), started.request.clone());
            tokio::spawn(async move { sign_ins.wait(&request, "tv", AWAIT).await })
        };
        let first = tokio::time::timeout(Duration::from_secs(1), first).await;
        assert!(
            matches!(first, Ok(Ok(Outcome::Pending))),
            "the older wait went on: {first:?}"
        );
        let claim = sign_ins.claim(&started.code, None).unwrap();
        assert!(sign_ins.fulfil(&claim.request, Some(handover("t"))));
        assert!(matches!(second.await.unwrap(), Outcome::Approved(_)));
    }

    #[tokio::test]
    async fn waiting_is_pending_until_decided_and_expired_after() {
        let clock = Clock::default();
        let sign_ins = DeviceSignIns::default().with_clock(clock.clone());
        let started = sign_ins.begin(tv("tv")).unwrap();
        assert_eq!(
            sign_ins
                .wait(&started.request, "tv", Duration::from_millis(50))
                .await,
            Outcome::Pending
        );
        clock.advance(LIFETIME);
        assert_eq!(
            sign_ins.wait(&started.request, "tv", AWAIT).await,
            Outcome::Expired
        );
        assert_eq!(sign_ins.lookup(&started.code), None);
    }

    /// PX-09 finding 27: what an approving app is told. Loopback is told
    /// apart from a home network; the rest is as the web page judges it.
    #[test]
    fn an_address_s_network_is_loopback_local_or_internet() {
        let at = |text: &str| network_with(text.parse().unwrap(), &[]);
        for this_host in ["127.0.0.1", "127.8.9.10", "::1", "::ffff:127.0.0.1"] {
            assert_eq!(at(this_host), AddressNetwork::Loopback, "{this_host}");
        }
        for near in [
            "10.0.0.5",
            "172.16.4.2",
            "192.168.86.20",
            "169.254.1.1",
            "100.101.102.103",
            "fe80::1",
            "fd7a:115c:a1e0::1",
            "::ffff:192.168.1.2",
        ] {
            assert_eq!(at(near), AddressNetwork::Local, "{near}");
        }
        for far in ["203.0.113.9", "8.8.8.8", "2001:db8::1", "2a00:1450::1"] {
            assert_eq!(at(far), AddressNetwork::Internet, "{far}");
        }
        assert_eq!(
            device_address("::ffff:192.168.1.2".parse().unwrap()),
            DeviceAddress {
                address: "192.168.1.2".into(),
                network: AddressNetwork::Local,
            }
        );
    }

    /// Finding 27's false positive: a global IPv6 address in the server's
    /// own /64 is a device in the same home, not on the internet.
    #[test]
    fn an_address_in_the_server_s_own_ipv6_prefix_is_local() {
        let own = [("2603:8000:c33e:6ac6::1".parse().unwrap(), 64)];
        assert_eq!(
            network_with("2603:8000:c33e:6ac6::abcd".parse().unwrap(), &own),
            AddressNetwork::Local
        );
    }

    /// Only the server's own prefix counts: the next /64 of the same /56
    /// is another home's.
    #[test]
    fn another_prefix_of_the_same_isp_block_is_internet() {
        let own = [("2603:8000:c33e:6ac6::1".parse().unwrap(), 64)];
        assert_eq!(
            network_with("2603:8000:c33e:6ac7::abcd".parse().unwrap(), &own),
            AddressNetwork::Internet
        );
    }

    /// A public IPv4 network the server is on is on-link too.
    #[test]
    fn an_address_on_the_server_s_public_ipv4_network_is_local() {
        let own = [("203.0.113.7".parse().unwrap(), 24)];
        assert_eq!(
            network_with("203.0.113.9".parse().unwrap(), &own),
            AddressNetwork::Local
        );
        assert_eq!(
            network_with("::ffff:203.0.113.9".parse().unwrap(), &own),
            AddressNetwork::Local
        );
        assert_eq!(
            network_with("203.0.114.9".parse().unwrap(), &own),
            AddressNetwork::Internet
        );
    }

    /// The server's own addresses, read from its interfaces, are on its
    /// own networks, so none of them is called public.
    #[test]
    fn the_server_s_own_addresses_are_not_internet() {
        for (addr, len) in own_networks() {
            assert!(len <= if addr.is_ipv4() { 32 } else { 128 }, "{addr}/{len}");
            assert_ne!(network(addr), AddressNetwork::Internet, "{addr}/{len}");
        }
    }

    /// PX-09 finding 27: what the approval page warns of.
    #[test]
    fn only_a_public_address_is_from_the_internet() {
        let at = |text: &str| from_internet(text.parse().unwrap());
        for near in [
            "127.0.0.1",
            "10.0.0.5",
            "172.16.4.2",
            "192.168.86.20",
            "169.254.1.1",
            "100.101.102.103",
            "::1",
            "fe80::1",
            "fd7a:115c:a1e0::1",
            "::ffff:192.168.1.2",
        ] {
            assert!(!at(near), "{near}");
        }
        for far in ["203.0.113.9", "8.8.8.8", "2001:db8::1", "2a00:1450::1"] {
            assert!(at(far), "{far}");
        }
    }
}
