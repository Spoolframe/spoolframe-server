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
//! A phone that reached the TV directly (`spoolframe/signin/1`) may
//! instead offer it a sign-in ([`DeviceSignIns::offer`]): approved in
//! advance for the TV's endpoint, with no code. The TV redeems it with the
//! same wait, and only then is its session made: an offer the TV never
//! redeems runs out with nothing made.
//!
//! Pending sign-ins are only in memory: a restart drops them, and the TV
//! begins again.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use crate::clock::Clock;
use spoolframe_protocol::device_sign_in::{ALPHABET, CODE_LEN};
use spoolframe_protocol::{AddressNetwork, DeviceAddress};
use spoolframe_types::Platform;
use tokio::sync::watch;

use crate::auth::SessionToken;
use crate::store::sessions::SignedInWith;
use crate::store::users::LocalUserId;

/// How long a code can be approved.
pub const LIFETIME: Duration = Duration::from_secs(5 * 60);

/// The longest one `AwaitDeviceSignIn` waits before `Pending`: short of
/// any idle timeout between the TV and the server.
pub const AWAIT: Duration = spoolframe_protocol::policy::wait::SIGN_IN;

/// Sign-ins one device may have under way at once by its own codes. A
/// device shows one code at a time, so one it begins again replaces its
/// codes still waiting: only those being approved count against this.
/// Offers to it do not: someone else makes those, and the device cannot
/// clear them, so they must never keep it from showing a code.
pub const MAX_PENDING_PER_DEVICE: usize = 3;

/// Offers one device may have pending at once ([`DeviceSignIns::offer`]),
/// whoever made them: a phone offering every profile it has on a few
/// servers, and retrying some, fits; one signed-in user cannot flood a
/// device it knows the key of.
pub const MAX_OFFERS_PER_DEVICE: usize = 8;

/// Sign-ins pending at once on the server (PX-10, finding 16). Anyone can
/// begin one from a new endpoint id; a household has a few devices
/// signing in at a time.
pub const MAX_PENDING: usize = 256;

/// Offers one user may have pending at once
/// ([`DeviceSignIns::offer`]): the most a phone hands a TV in one go.
/// Without it one signed-in user could take the server's whole
/// [`MAX_PENDING`] with offers to keys of their choosing.
pub const MAX_OFFERS_PER_USER: usize = spoolframe_protocol::phone_sign_in::MAX_OFFERS;

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

/// Whether `ip` is a global unicast IPv6 address: not unique-local
/// (`fc00::/7`), link-local (`fe80::/10`), an IPv4 one written as IPv6
/// (`::ffff:0:0/96`), loopback, unspecified or multicast. A device can be
/// on the server's own network ([`network`] says `Local`) and still have
/// one, which the approval page says, as "on your network (public IPv6)".
/// The apps decide this for themselves from [`DeviceAddress`].
pub fn public_ipv6(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(_) => false,
        IpAddr::V6(v6) => {
            v6.to_ipv4_mapped().is_none()
                && !v6.is_unique_local()
                && !v6.is_unicast_link_local()
                && !v6.is_loopback()
                && !v6.is_unspecified()
                && !v6.is_multicast()
        }
    }
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
/// itself alone.
fn own_networks() -> Vec<(IpAddr, u8)> {
    crate::ifaddrs::up()
        .into_iter()
        .map(|addr| {
            let unmasked = if addr.ip.is_ipv4() { 32 } else { 64 };
            (addr.ip, addr.prefix.unwrap_or(unmasked))
        })
        .collect()
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

/// A sign-in approved in advance for a device by a session of `user`'s
/// ([`DeviceSignIns::offer`]): what its session is made with when the
/// device redeems it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub user: LocalUserId,
    /// The device is trusted for `user` once it signs in: the approver's
    /// choice.
    pub remember: bool,
    /// How the offering session signed in: the device's session rests
    /// on it, as an approved one does.
    pub signed_in_with: Option<SignedInWith>,
    /// The offering session's token hash: the offer holds only while that
    /// session does.
    pub offered_by: String,
}

/// An offer the device came to redeem: claimed for it now, so only
/// [`DeviceSignIns::fulfil`] moves it on, as for a claimed code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redeeming {
    pub claim: Claim,
    pub grant: Grant,
}

/// What the waiting device learns.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Approved(Handover),
    /// An offer, now claimed for this wait: make its session, then wait
    /// again for the handover.
    Offered(Box<Redeeming>),
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
    /// Approved in advance, with no code: claimed when its device waits.
    Offered(Grant),
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
            State::Waiting | State::Offered(_) | State::Claimed => None,
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
    /// `None` for an offer, which no code names.
    code: Option<String>,
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
    /// Whether the server, device `endpoint` and, for an offer, the
    /// offering user `offering` each have room for one more pending
    /// sign-in: counted in one pass. A device's own codes and the offers
    /// made to it are counted apart ([`MAX_PENDING_PER_DEVICE`],
    /// [`MAX_OFFERS_PER_DEVICE`]).
    fn has_room_for(&self, endpoint: &str, offering: Option<LocalUserId>) -> bool {
        if self.by_request.len() >= MAX_PENDING {
            return false;
        }
        let (mut codes, mut offers, mut user) = (0, 0, 0);
        for p in self.by_request.values() {
            if p.waiting.endpoint == endpoint {
                // An offer has no code, whatever state it has reached.
                if p.code.is_some() {
                    codes += 1;
                } else {
                    offers += 1;
                }
            }
            if let Some(offering) = offering {
                let theirs = matches!(&*p.state.borrow(), State::Offered(g) if g.user == offering);
                user += usize::from(theirs);
            }
        }
        match offering {
            None => codes < MAX_PENDING_PER_DEVICE,
            Some(_) => offers < MAX_OFFERS_PER_DEVICE && user < MAX_OFFERS_PER_USER,
        }
    }

    /// Keep a new pending sign-in for `waiting`, in `state`, until
    /// `expires_at`; its request id.
    fn insert_pending(
        &mut self,
        waiting: Waiting,
        code: Option<String>,
        state: State,
        expires_at: Instant,
    ) -> String {
        let request = SessionToken::generate().0;
        if let Some(code) = &code {
            self.by_code.insert(code.clone(), request.clone());
        }
        self.by_request.insert(
            request.clone(),
            Pending {
                waiting,
                code,
                expires_at,
                state: watch::Sender::new(state),
                waits: watch::Sender::new(0),
            },
        );
        request
    }

    fn sweep(&mut self, now: Instant) {
        self.by_request.retain(|_, p| p.expires_at > now);
        let live = &self.by_request;
        self.by_code.retain(|_, request| live.contains_key(request));
    }

    /// Drop `endpoint`'s codes still waiting; one dropped tells its wait
    /// it expired. One being approved is left to finish.
    fn replace_waiting(&mut self, endpoint: &str) {
        self.by_request.retain(|_, p| {
            p.waiting.endpoint != endpoint || !matches!(*p.state.borrow(), State::Waiting)
        });
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

    /// Begin a sign-in for `waiting.endpoint`, in place of its codes still
    /// waiting; `None` if it already has [`MAX_PENDING_PER_DEVICE`] being
    /// approved, or the server [`MAX_PENDING`].
    pub fn begin(&self, waiting: Waiting) -> Option<Started> {
        let mut inner = self.lock();
        // Its old codes left on screen each time someone looked at the
        // ways to sign in, and three used up the device for 5 minutes.
        inner.replace_waiting(&waiting.endpoint);
        if !inner.has_room_for(&waiting.endpoint, None) {
            return None;
        }
        let code = loop {
            let code = new_code();
            if !inner.by_code.contains_key(&code) {
                break code;
            }
        };
        let expires_at = self.clock.now() + self.lifetime;
        let request = inner.insert_pending(waiting, Some(code.clone()), State::Waiting, expires_at);
        Some(Started {
            request,
            code,
            expires_in: self.lifetime,
        })
    }

    /// Offer `waiting.endpoint` a sign-in as `grant` says, approved in
    /// advance: it redeems it by waiting on the request. Its codes still
    /// waiting are left as they are; `None` if it already has
    /// [`MAX_OFFERS_PER_DEVICE`] offers pending, the offering user
    /// [`MAX_OFFERS_PER_USER`] offers not yet redeemed, or the server
    /// [`MAX_PENDING`]. The request id, which lives [`Self::lifetime`].
    pub fn offer(&self, waiting: Waiting, grant: Grant) -> Option<String> {
        let mut inner = self.lock();
        if !inner.has_room_for(&waiting.endpoint, Some(grant.user)) {
            return None;
        }
        let expires_at = self.clock.now() + self.lifetime;
        Some(inner.insert_pending(waiting, None, State::Offered(grant), expires_at))
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
                // An offer is claimed by the first wait of its device,
                // under the lock: one wait redeems it.
                Some(p) if p.waiting.endpoint == endpoint => {
                    let offered = match &*p.state.borrow() {
                        State::Offered(grant) => Some(grant.clone()),
                        _ => None,
                    };
                    if let Some(grant) = offered {
                        let redeeming = Redeeming {
                            claim: Claim {
                                request: request.to_owned(),
                                endpoint: p.waiting.endpoint.clone(),
                                device_name: p.waiting.device_name.clone(),
                                platform: p.waiting.platform,
                                // The device asked by taking the offer;
                                // the grant says whether it is trusted.
                                remember: grant.remember,
                            },
                            grant,
                        };
                        p.state.send_replace(State::Claimed);
                        return Outcome::Offered(Box::new(redeeming));
                    }
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
        if let Some(code) = &pending.code {
            inner.by_code.remove(code);
        }
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
        getrandom::fill(&mut byte).expect("the OS gives random bytes");
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

    fn grant() -> Grant {
        Grant {
            user: LocalUserId::generate(),
            remember: true,
            signed_in_with: Some(SignedInWith::Password),
            offered_by: "hash".into(),
        }
    }

    /// An offer is claimed by its device's first wait, which makes its
    /// session; the next wait is handed it, once.
    #[tokio::test]
    async fn an_offer_is_redeemed_by_its_device_once() {
        let sign_ins = DeviceSignIns::default();
        let grant = grant();
        let request = sign_ins.offer(tv("tv"), grant.clone()).unwrap();
        let Outcome::Offered(redeeming) = sign_ins.wait(&request, "tv", AWAIT).await else {
            panic!("not offered");
        };
        assert_eq!(redeeming.grant, grant);
        assert_eq!(redeeming.claim.endpoint, "tv");
        assert_eq!(redeeming.claim.platform, Platform::Tvos);
        assert!(sign_ins.fulfil(&request, Some(handover("t"))));
        assert_eq!(
            sign_ins.wait(&request, "tv", AWAIT).await,
            Outcome::Approved(handover("t"))
        );
        assert_eq!(sign_ins.wait(&request, "tv", AWAIT).await, Outcome::Expired);
    }

    #[tokio::test]
    async fn only_the_device_offered_to_redeems_an_offer() {
        let sign_ins = DeviceSignIns::default();
        let request = sign_ins.offer(tv("tv"), grant()).unwrap();
        assert_eq!(
            sign_ins.wait(&request, "elsewhere", AWAIT).await,
            Outcome::Expired
        );
        assert!(matches!(
            sign_ins.wait(&request, "tv", AWAIT).await,
            Outcome::Offered(_)
        ));
    }

    /// An offer has no code: it cannot be looked up, claimed or denied,
    /// and a device beginning a sign-in of its own keeps it.
    #[tokio::test]
    async fn an_offer_has_no_code_and_outlives_the_device_s_own_codes() {
        let sign_ins = DeviceSignIns::default();
        let request = sign_ins.offer(tv("tv"), grant()).unwrap();
        sign_ins.begin(tv("tv")).unwrap();
        sign_ins.begin(tv("tv")).unwrap();
        assert_eq!(sign_ins.lookup(""), None);
        assert_eq!(sign_ins.claim("", None), Err(ClaimError::Unknown));
        assert!(!sign_ins.deny(""));
        assert!(matches!(
            sign_ins.wait(&request, "tv", AWAIT).await,
            Outcome::Offered(_)
        ));
    }

    /// A device holds few offers, whoever makes them.
    #[test]
    fn a_device_holds_few_offers() {
        let sign_ins = DeviceSignIns::default();
        for _ in 0..MAX_OFFERS_PER_DEVICE {
            assert!(sign_ins.offer(tv("tv"), grant()).is_some());
        }
        assert!(sign_ins.offer(tv("tv"), grant()).is_none());
        assert!(sign_ins.offer(tv("other tv"), grant()).is_some());
    }

    /// Offers left unredeemed (turned down on the TV, its screen closed)
    /// never keep the TV from showing its own code: they are counted
    /// apart from its codes.
    #[test]
    fn offers_never_lock_a_device_out_of_its_own_code() {
        let sign_ins = DeviceSignIns::default();
        for _ in 0..MAX_PENDING_PER_DEVICE {
            assert!(sign_ins.offer(tv("tv"), grant()).is_some());
        }
        assert!(sign_ins.begin(tv("tv")).is_some());
    }

    /// One user holds few offers, whichever devices they are for; another
    /// user still offers.
    #[test]
    fn a_user_holds_few_offers() {
        let sign_ins = DeviceSignIns::default();
        let alice = grant();
        for i in 0..MAX_OFFERS_PER_USER {
            assert!(
                sign_ins
                    .offer(tv(&format!("tv-{i}")), alice.clone())
                    .is_some()
            );
        }
        assert!(sign_ins.offer(tv("one more tv"), alice).is_none());
        assert!(sign_ins.offer(tv("one more tv"), grant()).is_some());
    }

    /// Never redeemed, an offer runs out with nothing made.
    #[tokio::test]
    async fn an_offer_not_redeemed_expires() {
        let clock = Clock::default();
        let sign_ins = DeviceSignIns::default().with_clock(clock.clone());
        let request = sign_ins.offer(tv("tv"), grant()).unwrap();
        clock.advance(LIFETIME);
        assert_eq!(sign_ins.wait(&request, "tv", AWAIT).await, Outcome::Expired);
        assert!(sign_ins.lock().by_request.is_empty());
    }

    /// Redeemed, but its session not made in time: the device is told it
    /// expired, and the session is not handed over.
    #[tokio::test]
    async fn an_offer_whose_session_is_made_too_late_is_not_handed_over() {
        let clock = Clock::default();
        let sign_ins = DeviceSignIns::default().with_clock(clock.clone());
        let request = sign_ins.offer(tv("tv"), grant()).unwrap();
        assert!(matches!(
            sign_ins.wait(&request, "tv", AWAIT).await,
            Outcome::Offered(_)
        ));
        clock.advance(LIFETIME);
        assert_eq!(sign_ins.wait(&request, "tv", AWAIT).await, Outcome::Expired);
        assert!(!sign_ins.fulfil(&request, Some(handover("t"))));
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

    /// Looking through the ways to sign in shows a code each time: the
    /// device always gets one, and only the newest can be approved.
    #[test]
    fn a_device_beginning_again_replaces_its_waiting_code() {
        let sign_ins = DeviceSignIns::default();
        let first = sign_ins.begin(tv("tv")).unwrap();
        let mut last = None;
        for _ in 0..=MAX_PENDING_PER_DEVICE {
            last = sign_ins.begin(tv("tv"));
        }
        let last = last.expect("a code each time");
        assert!(sign_ins.lookup(&first.code).is_none());
        assert!(sign_ins.lookup(&last.code).is_some());
        assert!(sign_ins.begin(tv("other tv")).is_some());
        assert!(
            sign_ins.lookup(&last.code).is_some(),
            "another's is its own"
        );
    }

    #[tokio::test]
    async fn a_device_waiting_on_a_replaced_code_is_told_it_expired() {
        let sign_ins = DeviceSignIns::default();
        let first = sign_ins.begin(tv("tv")).unwrap();
        sign_ins.begin(tv("tv")).unwrap();
        assert_eq!(
            sign_ins.wait(&first.request, "tv", AWAIT).await,
            Outcome::Expired
        );
    }

    /// One being approved is left to finish, and those count: a device
    /// still has few sign-ins under way.
    #[test]
    fn a_device_has_few_sign_ins_being_approved() {
        let sign_ins = DeviceSignIns::default();
        let mut claimed = Vec::new();
        for _ in 0..MAX_PENDING_PER_DEVICE {
            let started = sign_ins.begin(tv("tv")).unwrap();
            sign_ins.claim(&started.code, None).unwrap();
            claimed.push(started.request);
        }
        assert!(sign_ins.begin(tv("tv")).is_none());
        let inner = sign_ins.lock();
        assert!(claimed.iter().all(|r| inner.by_request.contains_key(r)));
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

    /// A global unicast IPv6 address is public even on the server's own
    /// network: the approval page says so beside "on your network".
    #[test]
    fn only_a_global_unicast_ipv6_address_is_public_ipv6() {
        let at = |text: &str| public_ipv6(text.parse().unwrap());
        for public in ["2603:8000:c33e:6ac6::abcd", "2001:db8::1", "2a00:1450::1"] {
            assert!(at(public), "{public}");
        }
        for not in [
            "fd7a:115c:a1e0::1",
            "fc00::1",
            "fe80::1",
            "::ffff:203.0.113.9",
            "::1",
            "::",
            "ff02::1",
            "203.0.113.9",
            "192.168.86.20",
        ] {
            assert!(!at(not), "{not}");
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
