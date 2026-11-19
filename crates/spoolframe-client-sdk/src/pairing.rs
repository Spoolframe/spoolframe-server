//! Pairing a device with a portable identity by QR code, over a direct
//! Iroh connection: no Spoolframe server takes part.
//!
//! The device (a TV: no camera) shows a QR code; the identity's phone, the
//! one that holds its root, scans it, dials the device, and names the
//! identity; the device answers with its key for it; the person on the
//! phone authorizes ("Authorize Living room TV?") and the phone's root
//! signs a credential for that key, which goes back over the same
//! connection. The wire is `spoolframe_protocol::pairing`.
//!
//! The device side: [`crate::Client::start_pairing`] gives a [`Pairing`]:
//! its [`Pairing::link`] to show as a QR code, [`Pairing::connected`] for
//! the phone that scanned it and the fingerprint to show beside the
//! phone's, and [`Pairing::wait`] for how it ended. Only while one is
//! live does the device's endpoint accept the pairing ALPN, and only when
//! its owner allows guests ([`crate::Portable::allow_guests`]).
//!
//! The phone side: [`crate::Client::scan_pairing`] gives a
//! [`PairingOffer`]: the device's name and its key's fingerprint, to show,
//! then [`PairingOffer::authorize`] or [`PairingOffer::decline`].
//!
//! A device's code is also how a phone signs it in to its own accounts
//! ([`crate::phone_sign_in`]): one listener takes both ALPNs, and the code
//! goes to whichever phone presents it first.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use iroh::endpoint::{Connection, Incoming, RecvStream, SendStream};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayUrl, TransportAddr};
use parking_lot::Mutex;
use spoolframe_protocol::codec::{read_frame_within, write_frame};
use spoolframe_protocol::pairing::{
    self, CODE_LEN, CODE_TTL_SECS, DeviceFrame, LinkError, MAX_FRAME, PAIR_ALPN, PairedServer,
    PairingCode, PairingLink, PhoneFrame, Refusal,
};
use tokio::sync::{oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::Instant;

use crate::client::Client;
use crate::error::SdkError;
use crate::held::{self, Held, HeldPairing, Hold, HoldEnd};
use crate::phone_sign_in::{PhoneConnected, PhoneSignInOutcome};
use crate::portable::{DevicePublicKey, GuestServer, Portable, PortableUserId};
use crate::refusals::{self, Refusals};

/// How long a pairing's code is good for: [`CODE_TTL_SECS`].
pub const CODE_TTL: Duration = Duration::from_secs(CODE_TTL_SECS);

/// How long either side waits for the other's next frame, but for the
/// person's answer on the phone.
pub(crate) const STEP: Duration = Duration::from_secs(10);

/// How long the device waits for the person on the phone to authorize or
/// decline, once it has sent its key.
pub(crate) const APPROVAL: Duration = Duration::from_secs(2 * 60);

/// How long the phone waits to reach the device.
pub(crate) const DIAL: Duration = Duration::from_secs(10);

/// Why a pairing did not happen.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PairingError {
    /// The device does not allow guests ([`crate::Portable::allow_guests`]):
    /// this one, at [`crate::Client::start_pairing`], or the one the phone
    /// reached. Turn it on on the device to pair.
    #[error("this device does not allow sign-ins from other people's accounts")]
    NotAllowed,
    /// The scanned text is not a pairing code.
    #[error(transparent)]
    NotALink(#[from] LinkError),
    /// This device shows a pairing code already: one at a time.
    #[error("this device is already showing a pairing code")]
    AlreadyPairing,
    /// The phone does not hold the identity's root, so it cannot
    /// authorize a device for it: recover the identity on it first.
    #[error("this device does not hold that identity's root")]
    NoRoot,
    /// The code presented is not the one the device shows, or was used.
    #[error("the pairing code does not match")]
    WrongCode,
    /// The code was shown more than [`CODE_TTL`] ago: show a new one.
    #[error("the pairing code has expired")]
    Expired,
    /// The person on the phone said no.
    #[error("the pairing was declined on the phone")]
    Declined,
    /// The device did not take the credential: not for its key or
    /// identity, out of date, or revoked by the state that came with it.
    #[error("the device did not accept the credential")]
    CredentialRefused,
    /// The device could not finish (its storage, most likely).
    #[error("the device could not finish pairing")]
    Failed,
    /// The other side went away, or did not answer in time, part way.
    #[error("the pairing was cut off: {0}")]
    Aborted(String),
    /// The person at the device turned it down
    /// ([`crate::HeldPairing::reject`], [`crate::HeldSignIn::reject`]),
    /// or did not answer in time: nothing was kept.
    #[error("it was turned down on the device")]
    Rejected,
}

impl From<Refusal> for PairingError {
    fn from(refusal: Refusal) -> Self {
        match refusal {
            Refusal::NotAllowed => Self::NotAllowed,
            Refusal::WrongCode => Self::WrongCode,
            Refusal::Expired => Self::Expired,
            Refusal::CredentialRefused => Self::CredentialRefused,
            Refusal::Rejected => Self::Rejected,
            Refusal::Failed | Refusal::Unknown => Self::Failed,
        }
    }
}

/// A pairing that ended well: the device acts as `identity` now, with the
/// credential the phone gave its `device_key`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paired {
    pub identity: PortableUserId,
    pub device_key: DevicePublicKey,
    /// `device_key`'s fingerprint: [`Connected::fingerprint`].
    pub fingerprint: String,
    /// What the guest calls the identity, as their phone said:
    /// [`Connected::name`].
    pub name: Option<String>,
    /// What the guest's phone calls itself: [`Connected::phone_name`].
    pub phone_name: Option<String>,
    /// The servers the phone signs in to as `identity`, and has
    /// verified, saved on this device now: [`Client::sign_in_portable`] on
    /// each. Saved pinned but not verified, since the guest's phone
    /// vouches for them and not this device's owner: a password sign-in
    /// there shows "Not verified" as for any pin no one confirmed. A
    /// server this device had saved already is left as it was. Empty from
    /// a phone that has none. One whose server id a saved server has
    /// under another key is left out.
    pub servers: Vec<PairedServer>,
}

/// A phone that scanned the code and named `identity`; the device made
/// (or had) its `device_key` for it and sent it. The person on the phone
/// has not answered yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connected {
    pub identity: PortableUserId,
    pub device_key: DevicePublicKey,
    /// `device_key`'s fingerprint (`K7QX-3MPA-9TRD-WF2H`): the very string
    /// the phone shows as [`PairingOffer::fingerprint`]. Show it on the
    /// device so the person can check the two match before authorizing.
    pub fingerprint: String,
    /// What the guest calls the identity ("Steve"), as their phone said,
    /// at most [`pairing::MAX_NAME`] characters: "Check this matches
    /// Steve's phone". `None` from a phone that did not say (one before
    /// it was sent). The phone's word, not proved: show it beside the
    /// fingerprint, never instead of it.
    pub name: Option<String>,
    /// What the guest's phone calls itself ("Steve's iPhone"), its
    /// device name, as it said: `None` from a phone before it said. The
    /// phone's word, as `name` is.
    pub phone_name: Option<String>,
}

impl Connected {
    fn new(
        identity: PortableUserId,
        device_key: DevicePublicKey,
        name: Option<String>,
        phone_name: Option<String>,
    ) -> Self {
        Self {
            identity,
            device_key,
            fingerprint: device_key.fingerprint(),
            name,
            phone_name,
        }
    }
}

/// A name a phone sent, cut as names are; none if it sent none, or an
/// empty one.
pub(crate) fn sent_name(name: Option<String>) -> Option<String> {
    name.map(|n| pairing::cut_name(&n))
        .filter(|n| !n.is_empty())
}

/// How far a device's code got, for a pairing or a phone sign-in. Each
/// field only ever goes from `None` to `Some`.
#[derive(Default)]
pub(crate) struct Progress {
    pub(crate) connected: Option<PhoneConnected>,
    /// What the phone handed, held for the person at the device.
    pub(crate) held: Option<(Held, Hold)>,
    /// How the hold ended without the app's answer, if it did.
    pub(crate) held_end: Option<HoldEnd>,
    pub(crate) outcome: Option<Result<PhoneSignInOutcome, SdkError>>,
}

impl Progress {
    /// The phone that reached the code, once one has: as
    /// [`Pairing::connected`] says.
    pub(crate) async fn connected(
        progress: &watch::Receiver<Progress>,
    ) -> Result<PhoneConnected, SdkError> {
        let mut progress = progress.clone();
        let seen = progress
            .wait_for(|p| p.connected.is_some() || p.outcome.is_some())
            .await;
        match seen.as_deref() {
            Ok(Progress {
                connected: Some(connected),
                ..
            }) => Ok(connected.clone()),
            Ok(Progress {
                outcome: Some(Err(e)),
                ..
            }) => Err(e.clone()),
            // Either end came after its phone connected.
            Ok(Progress {
                outcome: Some(Ok(PhoneSignInOutcome::Paired(paired))),
                ..
            }) => Ok(PhoneConnected::Guest(Connected::new(
                paired.identity,
                paired.device_key,
                paired.name.clone(),
                paired.phone_name.clone(),
            ))),
            Ok(Progress {
                outcome: Some(Ok(PhoneSignInOutcome::SignedIn(_))),
                ..
            }) => Ok(PhoneConnected::SigningIn),
            Ok(Progress { .. }) | Err(_) => Err(cancelled()),
        }
    }

    /// What the code's connection holds, once it does, and its
    /// decision: or the error the code ended with before. Every call
    /// shares one decision; once every copy of it went undecided, it was
    /// rejected.
    pub(crate) async fn held(
        progress: &watch::Receiver<Progress>,
    ) -> Result<(Held, Arc<crate::held::Decision>), SdkError> {
        let mut watching = progress.clone();
        let seen = watching
            .wait_for(|p| p.held.is_some() || p.outcome.is_some())
            .await;
        match seen.as_deref() {
            Ok(Progress {
                held: Some((held, hold)),
                ..
            }) => Ok((held.clone(), hold.take())),
            Ok(Progress {
                outcome: Some(Err(e)),
                ..
            }) => Err(e.clone()),
            Ok(Progress { .. }) | Err(_) => Err(cancelled()),
        }
    }

    /// How the code's use ended: as [`Pairing::wait`] says.
    pub(crate) async fn outcome(
        progress: &watch::Receiver<Progress>,
    ) -> Result<PhoneSignInOutcome, SdkError> {
        let mut progress = progress.clone();
        match progress.wait_for(|p| p.outcome.is_some()).await {
            Ok(p) => p.outcome.clone().unwrap_or_else(|| Err(cancelled())),
            Err(_) => Err(cancelled()),
        }
    }
}

/// A pairing code this device shows (the device side). Show
/// [`Pairing::link`] as a QR code; once [`Pairing::connected`] gives the
/// phone that scanned it, show its fingerprint ("Check your phone shows
/// K7QX-3MPA-9TRD-WF2H"); then [`Pairing::wait`] for the end.
/// Dropping it, or [`Pairing::cancel`], stops the pairing: the device
/// stops taking pairing connections at once.
pub struct Pairing {
    listener: Listener,
}

impl std::fmt::Debug for Pairing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pairing")
            .field("expires_in", &self.expires_in())
            .finish_non_exhaustive()
    }
}

impl Pairing {
    /// What the QR code carries: `spoolframe://pair?e=…&c=…&n=…&a=…`. It
    /// holds the one-time code: show it, never log it.
    pub fn link(&self) -> &str {
        &self.listener.link
    }

    /// How long the code is still good for: until then a phone can start
    /// the pairing; one started finishes after.
    pub fn expires_in(&self) -> Duration {
        self.listener.expires_in()
    }

    /// The phone that scanned the code, once the device has taken its
    /// hello and sent it the device key for the identity it named: before
    /// the person on the phone answers. Show [`Connected::fingerprint`]
    /// so they can check their phone shows the same, then
    /// [`Pairing::wait`].
    ///
    /// If the pairing ends before a phone gets that far, the error
    /// [`Pairing::wait`] gives ([`PairingError::Expired`],
    /// [`PairingError::WrongCode`], [`PairingError::NotAllowed`],
    /// [`PairingError::Aborted`] when cancelled). Once a phone got that
    /// far, every call gives it, however the pairing then ends: whether
    /// it paired is [`Pairing::wait`]'s to say. May be called alongside
    /// [`Pairing::wait`], before or after it, and again.
    pub async fn connected(&self) -> Result<Connected, SdkError> {
        match Progress::connected(&self.listener.progress).await? {
            PhoneConnected::Guest(connected) => Ok(connected),
            // A pairing's code takes no sign-in ALPN.
            PhoneConnected::SigningIn => Err(cancelled()),
        }
    }

    /// The pairing the guest's phone authorized, held: nothing is
    /// installed or saved until [`HeldPairing::confirm`]. Show who it is
    /// ([`HeldPairing::name`], the fingerprint), then confirm or
    /// [`HeldPairing::reject`]; dropped undecided, or left
    /// [`APPROVAL`] unanswered, it is rejected.
    ///
    /// Or [`SdkError::Pairing`] with why it ended before
    /// ([`PairingError::Expired`], [`PairingError::WrongCode`],
    /// [`PairingError::Declined`], [`PairingError::NotAllowed`] if the
    /// owner turned guests off meanwhile, [`PairingError::Aborted`]),
    /// with no credential installed, nor a device key made for it. A
    /// later call gives the same pairing, sharing its decision.
    pub async fn wait(&self) -> Result<HeldPairing, SdkError> {
        let progress = &self.listener.progress;
        match Progress::held(progress).await? {
            (Held::Guest(pairing), decision) => {
                Ok(HeldPairing::new(pairing, decision, progress.clone()))
            }
            // A pairing's code takes no sign-in ALPN.
            (Held::SignIn { .. }, _) => Err(cancelled()),
        }
    }

    /// Stop the pairing: the code stops working at once.
    pub fn cancel(&self) {
        self.listener.cancel();
    }
}

/// What became of what a device holds for its person ([`confirmed`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Answer {
    /// They confirmed it, with the phone still there.
    Confirmed,
    /// They said no, every holder was dropped undecided, or
    /// [`APPROVAL`] passed with no answer.
    Rejected,
    /// The phone hung up first: nobody at the device said anything.
    PhoneLeft,
}

/// What the person at the device said of what is held, within
/// [`APPROVAL`], with the phone still there. A hold that ends without
/// their answer (the phone left, or no answer in time) says so in
/// [`Progress::held_end`], for [`crate::HeldSignIn::ended`].
pub(crate) async fn confirmed(
    shared: &Shared,
    conn: &Connection,
    answered: oneshot::Receiver<bool>,
) -> Answer {
    // `None`: the phone hung up first.
    let answer = tokio::select! {
        answer = tokio::time::timeout(APPROVAL, answered) => Some(answer),
        _ = conn.closed() => None,
    };
    let (answer, end) = match answer {
        None => (Answer::PhoneLeft, Some(HoldEnd::PhoneLeft)),
        Some(_) if conn.close_reason().is_some() => (Answer::PhoneLeft, Some(HoldEnd::PhoneLeft)),
        Some(Ok(Ok(true))) => (Answer::Confirmed, None),
        // A no, or every holder dropped undecided: none left to tell.
        Some(Ok(_)) => (Answer::Rejected, None),
        Some(Err(_)) => (Answer::Rejected, Some(HoldEnd::TimedOut)),
    };
    if let Some(end) = end {
        shared.progress.send_modify(|p| p.held_end = Some(end));
    }
    answer
}

/// The error a code ends with when the phone hung up while the device
/// held what it sent.
pub(crate) fn phone_left() -> SdkError {
    PairingError::Aborted("the phone left".into()).into()
}

/// Close our side of `send` and wait, briefly, for the peer to read it
/// and hang up: the end of every exchange on a code's connection.
pub(crate) async fn finish_and_wait_closed(conn: &Connection, send: &mut SendStream) {
    let _ = send.finish();
    let _ = tokio::time::timeout(STEP, conn.closed()).await;
}

/// What a pairing stopped before it ended says.
pub(crate) fn cancelled() -> SdkError {
    PairingError::Aborted("cancelled".into()).into()
}

/// The endpoint takes the code's ALPNs while this lives; dropped, it
/// takes none again, as the client's endpoint is built, and the next
/// pairing or phone sign-in may start.
struct Listening {
    endpoint: Endpoint,
    live: Arc<AtomicBool>,
}

impl Drop for Listening {
    fn drop(&mut self) {
        self.endpoint.set_alpns(Vec::new());
        self.live.store(false, Ordering::SeqCst);
    }
}

/// Where the pairing's code is.
enum CodeState {
    /// Shown; good until the deadline.
    Waiting(PairingCode),
    /// Presented, rightly or wrongly: the pairing is decided.
    Taken,
    /// Not presented in time.
    Expired,
}

/// What every connection of one code shares.
pub(crate) struct Shared {
    /// The device: what a phone sign-in signs in, and saves servers on.
    pub(crate) client: Client,
    portable: Portable,
    refusals: Arc<Refusals>,
    /// What the device calls itself.
    pub(crate) name: String,
    deadline: Instant,
    code: Mutex<CodeState>,
    /// Dropped (the task stopped) with no outcome: cancelled.
    pub(crate) progress: watch::Sender<Progress>,
    /// Each server a phone sign-in signs in to, as it finishes.
    pub(crate) tally: watch::Sender<crate::server_sign_ins::Tally>,
    /// A confirmed phone sign-in's rounds are going on, past the code's
    /// own end: they settle the tally, and keep the outcome current.
    pub(crate) rounds_open: std::sync::atomic::AtomicBool,
    /// The code takes phone sign-ins as well as guests: a guest refused
    /// because guests are off leaves it for a sign-in.
    takes_sign_ins: bool,
}

impl Shared {
    /// Take the code if `presented` is it, in time. A code still waiting
    /// is taken whatever was presented: a wrong code ends the pairing. On
    /// a refusal, whether this call decided the pairing (took the code).
    pub(crate) fn take(&self, presented: &PairingCode) -> Result<(), (Refusal, bool)> {
        let mut state = self.code.lock();
        match &*state {
            CodeState::Waiting(_) if Instant::now() >= self.deadline => {
                *state = CodeState::Expired;
                Err((Refusal::Expired, true))
            }
            CodeState::Waiting(code) => {
                let right = code.matches(presented);
                *state = CodeState::Taken;
                if right {
                    Ok(())
                } else {
                    Err((Refusal::WrongCode, true))
                }
            }
            CodeState::Taken => Err((Refusal::WrongCode, false)),
            CodeState::Expired => Err((Refusal::Expired, false)),
        }
    }

    /// Mark the code expired unless it was taken. Whether it expired.
    fn expire(&self) -> bool {
        let mut state = self.code.lock();
        if matches!(*state, CodeState::Waiting(_)) {
            *state = CodeState::Expired;
            true
        } else {
            false
        }
    }

    fn refused(&self, source: &str, user: Option<PortableUserId>, reason: &'static str) {
        self.refused_as("pair_hello", source, user, reason);
    }

    pub(crate) fn refused_as(
        &self,
        request: &'static str,
        source: &str,
        user: Option<PortableUserId>,
        reason: &'static str,
    ) {
        self.refusals.refused(refusals::Refusal {
            request,
            source,
            user: user.map(|u| u.to_string()),
            reason,
        });
    }
}

/// A code shown and the task taking connections for it: what a
/// [`Pairing`] and a [`crate::PhoneSignIn`] are.
/// Dropped, it stops the task: the code stops working at once.
pub(crate) struct Listener {
    pub(crate) link: String,
    expires_at: Instant,
    pub(crate) progress: watch::Receiver<Progress>,
    pub(crate) tally: watch::Receiver<crate::server_sign_ins::Tally>,
    task: JoinHandle<()>,
}

impl Listener {
    pub(crate) fn expires_in(&self) -> Duration {
        self.expires_at.saturating_duration_since(Instant::now())
    }

    pub(crate) fn cancel(&self) {
        self.task.abort();
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Client {
    /// Show a pairing code on this device (the device side): for a
    /// guest's phone to scan and authorize this device as their portable
    /// identity. `name` is what the phone shows ("Authorize Living room
    /// TV?").
    ///
    /// Refused, before anything is sent anywhere, unless the owner allows
    /// guests here ([`crate::Portable::allow_guests`]):
    /// [`PairingError::NotAllowed`]. One pairing at a time:
    /// [`PairingError::AlreadyPairing`].
    ///
    /// The code is good for [`CODE_TTL`] and once. The QR code carries
    /// this device's endpoint id and the addresses it is bound at: a phone
    /// on the same network reaches it. This device has no relay of its
    /// own, so a phone on another network does not.
    pub async fn start_pairing(&self, name: &str) -> Result<Pairing, SdkError> {
        self.start_pairing_for(name, CODE_TTL).await
    }

    pub(crate) async fn start_pairing_for(
        &self,
        name: &str,
        ttl: Duration,
    ) -> Result<Pairing, SdkError> {
        let portable = self.portable();
        if !portable.allow_guests().await? {
            self.inner.refusals.refused(refusals::Refusal {
                request: "start_pairing",
                source: "local",
                user: None,
                reason: "guests are not allowed on this device",
            });
            return Err(PairingError::NotAllowed.into());
        }
        let listener = self.listen(name, ttl, vec![PAIR_ALPN.to_vec()], Vec::new())?;
        Ok(Pairing { listener })
    }

    /// Show a code on this device for `ttl`, taking connections on
    /// `alpns` while it lives, its link naming `relays`: one listener at a
    /// time, [`PairingError::AlreadyPairing`] otherwise. A phone that
    /// pairs a guest has the servers it handed saved here before the
    /// listener's outcome says so.
    pub(crate) fn listen(
        &self,
        name: &str,
        ttl: Duration,
        alpns: Vec<Vec<u8>>,
        relays: Vec<String>,
    ) -> Result<Listener, SdkError> {
        let live = self.inner.pairing.clone();
        if live.swap(true, Ordering::SeqCst) {
            return Err(PairingError::AlreadyPairing.into());
        }
        let takes_sign_ins = alpns
            .iter()
            .any(|alpn| alpn == spoolframe_protocol::SIGNIN_ALPN);
        let endpoint = self.inner.endpoint.clone();
        endpoint.set_alpns(alpns);
        let listening = Listening {
            endpoint: endpoint.clone(),
            live,
        };
        let mut bytes = [0u8; CODE_LEN];
        getrandom::fill(&mut bytes).expect("the OS gives random bytes");
        let code = PairingCode::from_bytes(bytes);
        let link = PairingLink {
            endpoint: *endpoint.id().as_bytes(),
            code,
            name: name.to_owned(),
            addrs: self.pairing_addrs(),
            relays,
        }
        .link();
        let deadline = Instant::now() + ttl;
        let shared = Arc::new(Shared {
            client: self.clone(),
            portable: self.portable(),
            refusals: self.inner.refusals.clone(),
            name: name.to_owned(),
            deadline,
            code: Mutex::new(CodeState::Waiting(code)),
            progress: watch::Sender::new(Progress::default()),
            tally: watch::Sender::new(Default::default()),
            rounds_open: Default::default(),
            takes_sign_ins,
        });
        let progress = shared.progress.subscribe();
        let tally = shared.tally.subscribe();
        let client = self.clone();
        let task = tokio::spawn(async move {
            let mut outcome = serve(endpoint, shared.clone()).await;
            drop(listening);
            if let Ok(PhoneSignInOutcome::Paired(paired)) = &mut outcome {
                client.keep_paired(paired).await;
            }
            match &outcome {
                Ok(PhoneSignInOutcome::Paired(paired)) => {
                    tracing::info!(identity = %paired.identity, "paired a guest")
                }
                Ok(PhoneSignInOutcome::SignedIn(signed)) => tracing::info!(
                    sessions = signed.sessions.len(),
                    failed = signed.failed.len(),
                    "a phone signed this device in"
                ),
                Err(e) => tracing::info!(error = %e, "a code ended unused"),
            }
            // A phone sign-in's rounds set its outcome themselves, and keep
            // it current: never put the first round's back over it.
            shared.progress.send_if_modified(|p| {
                let unset = p.outcome.is_none();
                if unset {
                    p.outcome = Some(outcome);
                }
                unset
            });
            if !shared.rounds_open.load(Ordering::SeqCst) {
                shared.tally.send_modify(|t| t.settled = true);
            }
        });
        tracing::info!(expires_in = ?ttl, "showing a pairing code");
        Ok(Listener {
            link,
            expires_at: deadline,
            progress,
            tally,
            task,
        })
    }

    /// Save the servers a guest's phone handed `paired` and note where the
    /// guest was paired, before the app hears it paired: it signs in to
    /// them next. The guest is paired whether or not they could be.
    async fn keep_paired(&self, paired: &mut Paired) {
        let handed = std::mem::take(&mut paired.servers);
        match self.save_paired_servers(handed).await {
            Ok(saved) => paired.servers = saved,
            Err(e) => {
                tracing::warn!(error = %e, "could not save the servers a pairing handed")
            }
        }
        // For the guest's row: the first server its phone handed.
        let server = paired.servers.first().map(|s| GuestServer {
            endpoint_id: s.endpoint_id.clone(),
            name: s.name.clone(),
        });
        if let Err(e) = self.portable().note_paired(paired.identity, server).await {
            tracing::warn!(identity = %paired.identity, error = %e, "could not keep where the guest was paired");
        }
    }

    /// Where a phone may dial this device: the endpoint's own addresses,
    /// those of its network interfaces, then the sockets it is bound on.
    pub(crate) fn pairing_addrs(&self) -> Vec<SocketAddr> {
        let endpoint = &self.inner.endpoint;
        let mut addrs: Vec<SocketAddr> = Vec::new();
        let known = endpoint.addr().ip_addrs().copied().collect::<Vec<_>>();
        for addr in known.into_iter().chain(endpoint.bound_sockets()) {
            if !addr.ip().is_unspecified() && !addrs.contains(&addr) {
                addrs.push(addr);
            }
        }
        addrs.truncate(pairing::MAX_ADDRS);
        addrs
    }

    /// Dial the device `link` names on `alpn`: at its addresses, and
    /// through the relays it names if `relays`. The device's key and
    /// nothing else: Iroh proves it.
    pub(crate) async fn dial_device(
        &self,
        link: &PairingLink,
        alpn: &[u8],
        relays: bool,
    ) -> Result<Connection, SdkError> {
        let device = EndpointId::from_bytes(&link.endpoint)
            .map_err(|_| PairingError::NotALink(LinkError::Damaged("not an endpoint id")))?;
        let relays: Vec<RelayUrl> = if relays {
            link.relays
                .iter()
                .filter_map(|r| r.parse::<RelayUrl>().ok())
                .collect()
        } else {
            Vec::new()
        };
        let addr = EndpointAddr::from_parts(
            device,
            link.addrs
                .iter()
                .copied()
                .map(TransportAddr::Ip)
                .chain(relays.into_iter().map(TransportAddr::Relay)),
        );
        tokio::time::timeout(DIAL, self.inner.endpoint.connect(addr, alpn))
            .await
            .map_err(|_| SdkError::Unreachable(format!("the device {}", device.fmt_short())))?
            .map_err(|e| SdkError::Unreachable(format!("the device {}: {e}", device.fmt_short())))
    }

    /// Scan a device's pairing code (the phone side): dial the
    /// device it names, prove the code, and name `identity`, whose root
    /// this device must hold ([`PairingError::NoRoot`], checked before
    /// anything is sent). The device answers with its key for the
    /// identity: show [`PairingOffer::device_name`] and
    /// [`PairingOffer::fingerprint`], then [`PairingOffer::authorize`] or
    /// [`PairingOffer::decline`].
    ///
    /// The phone dials the endpoint id in the code and nothing else; Iroh
    /// proves the device's key, so it pairs with the device that showed
    /// the code or with none.
    pub async fn scan_pairing(
        &self,
        link: &str,
        identity: PortableUserId,
    ) -> Result<PairingOffer, SdkError> {
        let link = pairing::parse_link(link).map_err(PairingError::from)?;
        let portable = self.portable();
        match portable.load_root(identity).await {
            Ok(_) => {}
            Err(SdkError::UnknownIdentity(_)) => return Err(PairingError::NoRoot.into()),
            Err(e) => return Err(e),
        }
        // For the device's "Check this matches Steve's phone".
        let name = portable
            .list()
            .await?
            .into_iter()
            .find(|i| i.id == identity)
            .map(|i| pairing::cut_name(&i.name))
            .filter(|n| !n.is_empty());
        let use_relays = self.inner.relay_policy.default();
        let conn = self.dial_device(&link, PAIR_ALPN, use_relays).await?;
        let cut = |e: String| SdkError::from(PairingError::Aborted(e));
        let (mut send, mut recv) = conn.open_bi().await.map_err(|e| cut(e.to_string()))?;
        write_frame(
            &mut send,
            &PhoneFrame::Hello {
                code: link.code,
                identity,
                name,
                phone_name: sent_name(Some(self.inner.device.name.clone())),
            },
        )
        .await
        .map_err(|e| cut(e.to_string()))?;
        match step(STEP, read_frame_within(&mut recv, MAX_FRAME)).await? {
            DeviceFrame::Key { device_key, name } => Ok(PairingOffer {
                identity,
                device_key,
                device_name: pairing::cut_name(&name),
                portable,
                client: self.clone(),
                streams: tokio::sync::Mutex::new(Some(Streams { conn, send, recv })),
            }),
            DeviceFrame::Refused { reason } => {
                conn.close(0u32.into(), b"refused");
                Err(PairingError::from(reason).into())
            }
            DeviceFrame::Done => {
                conn.close(0u32.into(), b"out of turn");
                Err(cut("the device answered out of turn".into()))
            }
        }
    }
}

/// One frame's wait: in time, and as a frame.
pub(crate) async fn step<T>(
    within: Duration,
    read: impl std::future::Future<Output = Result<T, spoolframe_protocol::codec::CodecError>>,
) -> Result<T, SdkError> {
    match tokio::time::timeout(within, read).await {
        Err(_) => Err(PairingError::Aborted("no answer in time".into()).into()),
        Ok(Err(e)) => Err(PairingError::Aborted(e.to_string()).into()),
        Ok(Ok(frame)) => Ok(frame),
    }
}

struct Streams {
    conn: Connection,
    send: SendStream,
    recv: RecvStream,
}

/// A device that answered a scanned pairing code (the phone side): what
/// to show before the person authorizes it.
pub struct PairingOffer {
    identity: PortableUserId,
    device_key: DevicePublicKey,
    device_name: String,
    portable: Portable,
    /// What knows the servers the identity signs in to.
    client: Client,
    streams: tokio::sync::Mutex<Option<Streams>>,
}

impl std::fmt::Debug for PairingOffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingOffer")
            .field("identity", &self.identity)
            .field("device_key", &self.device_key)
            .field("device_name", &self.device_name)
            .finish_non_exhaustive()
    }
}

impl PairingOffer {
    /// What the device calls itself: "Authorize Living room TV?".
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// The device's key for the identity, as the credential will name it.
    pub fn device_key(&self) -> DevicePublicKey {
        self.device_key
    }

    /// The device key's fingerprint (`K7QX-3MPA-9TRD-WF2H`), the format of
    /// an identity's: to compare with the one the device shows, if it
    /// shows one.
    pub fn fingerprint(&self) -> String {
        self.device_key.fingerprint()
    }

    /// The identity the device is to act as.
    pub fn identity(&self) -> PortableUserId {
        self.identity
    }

    /// The servers this phone could hand the device: each it has
    /// verified and signs in to as the identity, at most
    /// [`spoolframe_protocol::pairing::MAX_PAIRED_SERVERS`]. Show them
    /// ticked; [`PairingOffer::authorize_with`] hands the ones kept.
    pub async fn servers(&self) -> Result<Vec<PairedServer>, SdkError> {
        self.client.paired_servers(self.identity).await
    }

    /// Authorize the device: the root signs a credential for its key,
    /// recorded as `label` ([`crate::Portable::issued`]; the device's
    /// name if `None`), and the device installs it with the identity's
    /// newest state. With them go every one of
    /// [`PairingOffer::servers`], for the device to save and sign in to.
    /// Done once the device says so. Refused, with nothing sent, if this
    /// device no longer holds the root ([`PairingError::NoRoot`]). Once
    /// only.
    pub async fn authorize(&self, label: Option<&str>) -> Result<(), SdkError> {
        let servers = self.servers().await?;
        self.authorize_handing(label, servers).await
    }

    /// [`PairingOffer::authorize`], handing only the servers of
    /// [`PairingOffer::servers`] whose endpoint ids are in `chosen`, in
    /// that list's order; none hands none. [`SdkError::InvalidInput`],
    /// with nothing sent, for an id that is not one of them.
    pub async fn authorize_with(
        &self,
        label: Option<&str>,
        chosen: &[String],
    ) -> Result<(), SdkError> {
        let offered = self.servers().await?;
        if let Some(stray) = chosen
            .iter()
            .find(|id| !offered.iter().any(|s| s.endpoint_id == **id))
        {
            return Err(SdkError::InvalidInput(format!(
                "{stray} is not a server this phone can hand over"
            )));
        }
        let servers = offered
            .into_iter()
            .filter(|s| chosen.contains(&s.endpoint_id))
            .collect();
        self.authorize_handing(label, servers).await
    }

    async fn authorize_handing(
        &self,
        label: Option<&str>,
        servers: Vec<PairedServer>,
    ) -> Result<(), SdkError> {
        let Some(Streams {
            conn,
            mut send,
            mut recv,
        }) = self.streams.lock().await.take()
        else {
            return Err(PairingError::Aborted("already answered".into()).into());
        };
        let label = label.unwrap_or(&self.device_name);
        let credential = match self
            .portable
            .issue_credential(self.identity, self.device_key, Some(label))
            .await
        {
            Ok(credential) => credential,
            Err(e) => {
                conn.close(0u32.into(), b"not authorized");
                return Err(match e {
                    SdkError::UnknownIdentity(_) => PairingError::NoRoot.into(),
                    e => e,
                });
            }
        };
        let state = self.portable.identity_state(self.identity).await?;
        let cut = |e: String| SdkError::from(PairingError::Aborted(e));
        write_frame(
            &mut send,
            &PhoneFrame::Credential {
                credential: Box::new(credential),
                state: state.map(Box::new),
                servers,
            },
        )
        .await
        .map_err(|e| cut(e.to_string()))?;
        // The person at the device confirms first: as long as it gives
        // them, and a step more.
        let answer = step(APPROVAL + STEP, read_frame_within(&mut recv, MAX_FRAME)).await;
        conn.close(0u32.into(), b"done");
        match answer? {
            DeviceFrame::Done => {
                tracing::info!(identity = %self.identity, device_key = %self.device_key, "authorized a device");
                Ok(())
            }
            DeviceFrame::Refused { reason } => Err(PairingError::from(reason).into()),
            DeviceFrame::Key { .. } => Err(cut("the device answered out of turn".into())),
        }
    }

    /// Decline: the device is told, and gets nothing. Once only.
    pub async fn decline(&self) -> Result<(), SdkError> {
        let Some(Streams { conn, mut send, .. }) = self.streams.lock().await.take() else {
            return Err(PairingError::Aborted("already answered".into()).into());
        };
        let sent = write_frame(&mut send, &PhoneFrame::Declined).await;
        finish_and_wait_closed(&conn, &mut send).await;
        sent.map_err(|e| PairingError::Aborted(e.to_string()).into())
    }
}

/// Take connections until one decides the code's use, or the code
/// expires untaken.
async fn serve(endpoint: Endpoint, shared: Arc<Shared>) -> Result<PhoneSignInOutcome, SdkError> {
    let mut handlers: JoinSet<Option<Result<PhoneSignInOutcome, SdkError>>> = JoinSet::new();
    let mut deadline_passed = false;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(shared.deadline), if !deadline_passed => {
                deadline_passed = true;
                if shared.expire() {
                    return Err(PairingError::Expired.into());
                }
            }
            incoming = endpoint.accept() => match incoming {
                None => return Err(PairingError::Aborted("the endpoint closed".into()).into()),
                Some(incoming) => {
                    handlers.spawn(handle(shared.clone(), incoming));
                }
            },
            Some(handled) = handlers.join_next() => {
                if let Ok(Some(outcome)) = handled {
                    return outcome;
                }
            }
        }
    }
}

/// One connection, by its ALPN. `Some` if it decided the code's use: it
/// presented the code, rightly or wrongly. `None` if it went away
/// before, or came after the code's use was decided.
async fn handle(
    shared: Arc<Shared>,
    incoming: Incoming,
) -> Option<Result<PhoneSignInOutcome, SdkError>> {
    let accepting = incoming.accept().ok()?;
    let conn = tokio::time::timeout(STEP, accepting).await.ok()?.ok()?;
    match conn.alpn() {
        PAIR_ALPN => handle_pair(&shared, conn)
            .await
            .map(|outcome| outcome.map(PhoneSignInOutcome::Paired)),
        spoolframe_protocol::SIGNIN_ALPN => crate::phone_sign_in::handle(&shared, conn)
            .await
            .map(|outcome| outcome.map(PhoneSignInOutcome::SignedIn)),
        _ => None,
    }
}

/// One pairing connection: [`handle`]'s.
async fn handle_pair(shared: &Shared, conn: Connection) -> Option<Result<Paired, SdkError>> {
    let source = conn.remote_id().fmt_short().to_string();
    let (mut send, mut recv) = tokio::time::timeout(STEP, conn.accept_bi())
        .await
        .ok()?
        .ok()?;
    let hello = tokio::time::timeout(
        STEP,
        read_frame_within::<_, PhoneFrame>(&mut recv, MAX_FRAME),
    )
    .await
    .ok()?
    .ok()?;
    let PhoneFrame::Hello {
        code,
        identity,
        name,
        phone_name,
    } = hello
    else {
        shared.refused(&source, None, "no hello");
        return None;
    };
    // A phone sign-in's code takes guests' dials whether or not guests
    // are allowed, so a guest with guests off is told so, typed, rather
    // than failing the handshake. Their dial leaves the code alone: the
    // same code still signs the TV in from a phone.
    if shared.takes_sign_ins {
        match shared.portable.allow_guests().await {
            Ok(true) => {}
            Ok(false) => {
                shared.refused(
                    &source,
                    Some(identity),
                    "guests are not allowed on this device",
                );
                refuse(&conn, &mut send, Refusal::NotAllowed).await;
                return None;
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not read whether guests are allowed");
                refuse(&conn, &mut send, Refusal::Failed).await;
                return None;
            }
        }
    }
    if let Err((reason, decided)) = shared.take(&code) {
        let why = match reason {
            Refusal::Expired => "the code has expired",
            _ => "the code does not match",
        };
        shared.refused(&source, Some(identity), why);
        refuse(&conn, &mut send, reason).await;
        return decided.then(|| Err(PairingError::from(reason).into()));
    }
    // A pairing's own code: the owner may have turned guests off since it
    // was shown, which ends it.
    match shared.portable.allow_guests().await {
        Ok(true) => {}
        Ok(false) => {
            shared.refused(
                &source,
                Some(identity),
                "guests are not allowed on this device",
            );
            refuse(&conn, &mut send, Refusal::NotAllowed).await;
            return Some(Err(PairingError::NotAllowed.into()));
        }
        Err(e) => {
            refuse(&conn, &mut send, Refusal::Failed).await;
            return Some(Err(e));
        }
    }
    let names = Names {
        name: sent_name(name),
        phone_name: sent_name(phone_name),
    };
    let outcome = pair(shared, &conn, &mut send, &mut recv, identity, names).await;
    if let Err((Some(reason), _)) = &outcome {
        if *reason == Refusal::CredentialRefused {
            shared.refused(&source, Some(identity), "the credential does not hold here");
        }
        refuse(&conn, &mut send, *reason).await;
    }
    Some(outcome.map_err(|(_, e)| e))
}

/// What a guest's phone said it and the identity are called.
struct Names {
    name: Option<String>,
    phone_name: Option<String>,
}

/// Tell the phone why, and wait briefly for it to hang up.
async fn refuse(conn: &Connection, send: &mut SendStream, reason: Refusal) {
    let _ = write_frame(send, &DeviceFrame::Refused { reason }).await;
    finish_and_wait_closed(conn, send).await;
}

/// The rest of a pairing whose code was right: the device's key, the
/// phone's answer, the credential installed. On an error, the refusal to
/// send the phone, if any, and the error; a device key made for this
/// pairing is removed.
async fn pair(
    shared: &Shared,
    conn: &Connection,
    send: &mut SendStream,
    recv: &mut RecvStream,
    identity: PortableUserId,
    Names { name, phone_name }: Names,
) -> Result<Paired, (Option<Refusal>, SdkError)> {
    let portable = &shared.portable;
    let failed = |e: SdkError| (Some(Refusal::Failed), e);
    let existed = portable.has_device_key(identity).await.map_err(failed)?;
    let outcome = async {
        let device_key = portable.device_key(identity).await.map_err(failed)?;
        let aborted = |e: String| (None, SdkError::from(PairingError::Aborted(e)));
        write_frame(
            send,
            &DeviceFrame::Key {
                device_key,
                name: shared.name.clone(),
            },
        )
        .await
        .map_err(|e| aborted(e.to_string()))?;
        shared.progress.send_modify(|p| {
            p.connected = Some(PhoneConnected::Guest(Connected::new(
                identity,
                device_key,
                name.clone(),
                phone_name.clone(),
            )))
        });
        let answer = step(
            APPROVAL,
            read_frame_within::<_, PhoneFrame>(recv, MAX_FRAME),
        )
        .await
        .map_err(|e| (None, e))?;
        let (credential, state, servers) = match answer {
            PhoneFrame::Credential {
                credential,
                state,
                servers,
            } => (*credential, state, servers),
            PhoneFrame::Declined => return Err((None, PairingError::Declined.into())),
            PhoneFrame::Hello { .. } => return Err(aborted("a second hello".into())),
        };
        let refused = |e: SdkError| (Some(Refusal::CredentialRefused), e);
        if credential.user != identity || credential.device_key != device_key {
            return Err(refused(SdkError::NotThisDevice));
        }
        // A genesis decides which roots are believed: one that does not
        // hold, or is another identity's, is refused before anything is
        // kept (install_with_state checks it too).
        if let Some(genesis) = credential.genesis.as_deref()
            && (genesis.verify().is_err() || genesis.user() != identity)
        {
            return Err(refused(PairingError::CredentialRefused.into()));
        }
        if let Some(state) = &state {
            state.verify().map_err(|e| refused(e.into()))?;
            if state.user != identity {
                return Err(refused(SdkError::UnknownIdentity(state.user.to_string())));
            }
            if state.revokes(&device_key) {
                return Err(refused(SdkError::DeviceKeyRevoked));
            }
        }
        // Held for the person at the device: nothing is installed or
        // saved unless they confirm.
        let (hold, answered) = Hold::new();
        let held = Paired {
            identity,
            device_key,
            fingerprint: device_key.fingerprint(),
            name: name.clone(),
            phone_name: phone_name.clone(),
            servers: servers
                .iter()
                .take(spoolframe_protocol::pairing::MAX_PAIRED_SERVERS)
                .cloned()
                .filter_map(PairedServer::within_bounds)
                .collect(),
        };
        shared
            .progress
            .send_modify(|p| p.held = Some((Held::Guest(held), hold)));
        match confirmed(shared, conn, answered).await {
            Answer::Confirmed => {}
            Answer::Rejected => return Err((Some(Refusal::Rejected), held::rejected_here())),
            // Nobody to refuse: the phone is gone.
            Answer::PhoneLeft => return Err((None, phone_left())),
        }
        // The state first: a root it adds may have signed the
        // credential. Nothing half done: a refused credential leaves no
        // state this device did not have.
        portable
            .install_with_state(credential, state.map(|s| *s))
            .await
            .map_err(|e| match e {
                SdkError::Storage(_) => failed(e),
                e => refused(e),
            })?;
        write_frame(send, &DeviceFrame::Done)
            .await
            .map_err(|e| aborted(e.to_string()))?;
        finish_and_wait_closed(conn, send).await;
        Ok(Paired {
            identity,
            device_key,
            fingerprint: device_key.fingerprint(),
            name,
            phone_name,
            servers,
        })
    }
    .await;
    if outcome.is_err()
        && !existed
        && let Err(e) = portable.drop_unused_device_key(identity).await
    {
        tracing::warn!(%identity, error = %e, "could not remove a device key no pairing used");
    }
    outcome
}

#[cfg(test)]
#[path = "pairing_tests.rs"]
pub(crate) mod tests;
