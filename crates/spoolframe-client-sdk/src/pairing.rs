//! Pairing a device with a portable identity by QR code (P7-03,
//! ARCHITECTURE §47), over a direct Iroh connection: no Spoolframe server
//! takes part.
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
//! phone's, and [`Pairing::wait`] for how it ended. Only while one is live does the device's endpoint accept
//! the pairing ALPN, and only when its owner allows guests
//! ([`crate::Portable::allow_guests`]).
//!
//! The phone side: [`crate::Client::scan_pairing`] gives a
//! [`PairingOffer`]: the device's name and its key's fingerprint, to show,
//! then [`PairingOffer::authorize`] or [`PairingOffer::decline`].

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use iroh::endpoint::{Connection, Incoming, RecvStream, SendStream};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayUrl, TransportAddr};
use parking_lot::Mutex;
use spoolframe_protocol::codec::{read_frame_within, write_frame};
use spoolframe_protocol::pairing::{
    self, CODE_LEN, CODE_TTL_SECS, DeviceFrame, LinkError, MAX_FRAME, PAIR_ALPN, PairingCode,
    PairingLink, PhoneFrame, Refusal,
};
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::Instant;

use crate::client::Client;
use crate::error::SdkError;
use crate::portable::{DevicePublicKey, Portable, PortableUserId};
use crate::refusals::{self, Refusals};

/// How long a pairing's code is good for: [`CODE_TTL_SECS`].
pub const CODE_TTL: Duration = Duration::from_secs(CODE_TTL_SECS);

/// How long either side waits for the other's next frame, but for the
/// person's answer on the phone.
const STEP: Duration = Duration::from_secs(10);

/// How long the device waits for the person on the phone to authorize or
/// decline, once it has sent its key.
const APPROVAL: Duration = Duration::from_secs(2 * 60);

/// How long the phone waits to reach the device.
const DIAL: Duration = Duration::from_secs(10);

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
}

impl From<Refusal> for PairingError {
    fn from(refusal: Refusal) -> Self {
        match refusal {
            Refusal::NotAllowed => Self::NotAllowed,
            Refusal::WrongCode => Self::WrongCode,
            Refusal::Expired => Self::Expired,
            Refusal::CredentialRefused => Self::CredentialRefused,
            Refusal::Failed => Self::Failed,
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
}

impl Connected {
    fn new(identity: PortableUserId, device_key: DevicePublicKey) -> Self {
        Self {
            identity,
            device_key,
            fingerprint: device_key.fingerprint(),
        }
    }
}

/// How far a pairing got. Both fields only ever go from `None` to `Some`.
#[derive(Default)]
struct Progress {
    connected: Option<Connected>,
    outcome: Option<Result<Paired, SdkError>>,
}

/// A pairing code this device shows (the device side). Show
/// [`Pairing::link`] as a QR code; once [`Pairing::connected`] gives the
/// phone that scanned it, show its fingerprint ("Check your phone shows
/// K7QX-3MPA-9TRD-WF2H"); then [`Pairing::wait`] for the end.
/// Dropping it, or [`Pairing::cancel`], stops the pairing: the device
/// stops taking pairing connections at once.
pub struct Pairing {
    link: String,
    expires_at: Instant,
    progress: watch::Receiver<Progress>,
    task: JoinHandle<()>,
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
        &self.link
    }

    /// How long the code is still good for: until then a phone can start
    /// the pairing; one started finishes after.
    pub fn expires_in(&self) -> Duration {
        self.expires_at.saturating_duration_since(Instant::now())
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
        let mut progress = self.progress.clone();
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
            // Paired always connected first.
            Ok(Progress {
                outcome: Some(Ok(paired)),
                ..
            }) => Ok(Connected::new(paired.identity, paired.device_key)),
            Ok(Progress { .. }) | Err(_) => Err(cancelled()),
        }
    }

    /// How the pairing ended: [`Paired`], or [`SdkError::Pairing`] with
    /// why ([`PairingError::Expired`], [`PairingError::WrongCode`],
    /// [`PairingError::Declined`], [`PairingError::NotAllowed`] if the
    /// owner turned guests off meanwhile, [`PairingError::Aborted`]). Any
    /// but [`Paired`] leaves no credential installed, nor a device key
    /// made for it. Every call gives the same answer.
    pub async fn wait(&self) -> Result<Paired, SdkError> {
        let mut progress = self.progress.clone();
        match progress.wait_for(|p| p.outcome.is_some()).await {
            Ok(p) => p.outcome.clone().unwrap_or_else(|| Err(cancelled())),
            Err(_) => Err(cancelled()),
        }
    }

    /// Stop the pairing: the code stops working at once.
    pub fn cancel(&self) {
        self.task.abort();
    }
}

/// What a pairing stopped before it ended says.
fn cancelled() -> SdkError {
    PairingError::Aborted("cancelled".into()).into()
}

impl Drop for Pairing {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The endpoint takes the pairing ALPN while this lives; dropped, it
/// takes none again, as the client's endpoint is built, and the next
/// pairing may start.
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

/// What every connection of one pairing shares.
struct Shared {
    portable: Portable,
    refusals: Arc<Refusals>,
    name: String,
    deadline: Instant,
    code: Mutex<CodeState>,
    /// Dropped (the task stopped) with no outcome: cancelled.
    progress: watch::Sender<Progress>,
}

impl Shared {
    /// Take the code if `presented` is it, in time. A code still waiting
    /// is taken whatever was presented: a wrong code ends the pairing. On
    /// a refusal, whether this call decided the pairing (took the code).
    fn take(&self, presented: &PairingCode) -> Result<(), (Refusal, bool)> {
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
        self.refusals.refused(refusals::Refusal {
            request: "pair_hello",
            source,
            user: user.map(|u| u.to_string()),
            reason,
        });
    }
}

impl Client {
    /// Show a pairing code on this device (the device side, P7-03): for a
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
        let live = self.inner.pairing.clone();
        if live.swap(true, Ordering::SeqCst) {
            return Err(PairingError::AlreadyPairing.into());
        }
        let endpoint = self.inner.endpoint.clone();
        endpoint.set_alpns(vec![PAIR_ALPN.to_vec()]);
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
            relays: Vec::new(),
        }
        .link();
        let deadline = Instant::now() + ttl;
        let shared = Arc::new(Shared {
            portable,
            refusals: self.inner.refusals.clone(),
            name: name.to_owned(),
            deadline,
            code: Mutex::new(CodeState::Waiting(code)),
            progress: watch::Sender::new(Progress::default()),
        });
        let progress = shared.progress.subscribe();
        let task = tokio::spawn(async move {
            let outcome = serve(endpoint, shared.clone()).await;
            drop(listening);
            match &outcome {
                Ok(paired) => tracing::info!(identity = %paired.identity, "paired a guest"),
                Err(e) => tracing::info!(error = %e, "a pairing ended without a guest"),
            }
            shared.progress.send_modify(|p| p.outcome = Some(outcome));
        });
        tracing::info!(expires_in = ?ttl, "showing a pairing code");
        Ok(Pairing {
            link,
            expires_at: deadline,
            progress,
            task,
        })
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

    /// Scan a device's pairing code (the phone side, P7-03): dial the
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
        let device = EndpointId::from_bytes(&link.endpoint)
            .map_err(|_| PairingError::NotALink(LinkError::Damaged("not an endpoint id")))?;
        let relays = if self.inner.relays {
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
        let conn = tokio::time::timeout(DIAL, self.inner.endpoint.connect(addr, PAIR_ALPN))
            .await
            .map_err(|_| SdkError::Unreachable(format!("the device {}", device.fmt_short())))?
            .map_err(|e| {
                SdkError::Unreachable(format!("the device {}: {e}", device.fmt_short()))
            })?;
        let cut = |e: String| SdkError::from(PairingError::Aborted(e));
        let (mut send, mut recv) = conn.open_bi().await.map_err(|e| cut(e.to_string()))?;
        write_frame(
            &mut send,
            &PhoneFrame::Hello {
                code: link.code,
                identity,
            },
        )
        .await
        .map_err(|e| cut(e.to_string()))?;
        match step(STEP, read_frame_within(&mut recv, MAX_FRAME)).await? {
            DeviceFrame::Key { device_key, name } => {
                let mut name = name;
                name.truncate(
                    name.char_indices()
                        .nth(pairing::MAX_NAME)
                        .map_or(name.len(), |(at, _)| at),
                );
                Ok(PairingOffer {
                    identity,
                    device_key,
                    device_name: name,
                    portable,
                    streams: tokio::sync::Mutex::new(Some(Streams { conn, send, recv })),
                })
            }
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
async fn step<T>(
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

    /// Authorize the device: the root signs a credential for its key,
    /// recorded as `label` ([`crate::Portable::issued`]; the device's
    /// name if `None`), and the device installs it with the identity's
    /// newest state. Done once the device says so. Refused, with nothing
    /// sent, if this device no longer holds the root
    /// ([`PairingError::NoRoot`]). Once only.
    pub async fn authorize(&self, label: Option<&str>) -> Result<(), SdkError> {
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
                state,
            },
        )
        .await
        .map_err(|e| cut(e.to_string()))?;
        let answer = step(STEP, read_frame_within(&mut recv, MAX_FRAME)).await;
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
        let _ = send.finish();
        // Until the device has read it and closed, or briefly.
        let _ = tokio::time::timeout(STEP, conn.closed()).await;
        sent.map_err(|e| PairingError::Aborted(e.to_string()).into())
    }
}

/// Take pairing connections until one decides the pairing, or the code
/// expires untaken.
async fn serve(endpoint: Endpoint, shared: Arc<Shared>) -> Result<Paired, SdkError> {
    let mut handlers: JoinSet<Option<Result<Paired, SdkError>>> = JoinSet::new();
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

/// One pairing connection. `Some` if it decided the pairing: it presented
/// the code, rightly or wrongly. `None` if it went away before, or came
/// after the pairing was decided.
async fn handle(shared: Arc<Shared>, incoming: Incoming) -> Option<Result<Paired, SdkError>> {
    let accepting = incoming.accept().ok()?;
    let conn = tokio::time::timeout(STEP, accepting).await.ok()?.ok()?;
    if conn.alpn() != PAIR_ALPN {
        return None;
    }
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
    let PhoneFrame::Hello { code, identity } = hello else {
        shared.refused(&source, None, "no hello");
        return None;
    };
    if let Err((reason, decided)) = shared.take(&code) {
        let why = match reason {
            Refusal::Expired => "the code has expired",
            _ => "the code does not match",
        };
        shared.refused(&source, Some(identity), why);
        refuse(&conn, &mut send, reason).await;
        return decided.then(|| Err(PairingError::from(reason).into()));
    }
    // The owner may have turned guests off since the code was shown.
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
    let outcome = pair(&shared, &conn, &mut send, &mut recv, identity).await;
    if let Err((Some(reason), _)) = &outcome {
        if *reason == Refusal::CredentialRefused {
            shared.refused(&source, Some(identity), "the credential does not hold here");
        }
        refuse(&conn, &mut send, *reason).await;
    }
    Some(outcome.map_err(|(_, e)| e))
}

/// Tell the phone why, and wait briefly for it to hang up.
async fn refuse(conn: &Connection, send: &mut SendStream, reason: Refusal) {
    let _ = write_frame(send, &DeviceFrame::Refused { reason }).await;
    let _ = send.finish();
    let _ = tokio::time::timeout(STEP, conn.closed()).await;
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
        shared
            .progress
            .send_modify(|p| p.connected = Some(Connected::new(identity, device_key)));
        let answer = step(
            APPROVAL,
            read_frame_within::<_, PhoneFrame>(recv, MAX_FRAME),
        )
        .await
        .map_err(|e| (None, e))?;
        let (credential, state) = match answer {
            PhoneFrame::Credential { credential, state } => (*credential, state),
            PhoneFrame::Declined => return Err((None, PairingError::Declined.into())),
            PhoneFrame::Hello { .. } => return Err(aborted("a second hello".into())),
        };
        let refused = |e: SdkError| (Some(Refusal::CredentialRefused), e);
        if credential.user != identity || credential.device_key != device_key {
            return Err(refused(SdkError::NotThisDevice));
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
        portable
            .install_credential(credential)
            .await
            .map_err(|e| match e {
                SdkError::Storage(_) => failed(e),
                e => refused(e),
            })?;
        if let Some(state) = state
            && let Err(e) = portable.accept_state(state).await
        {
            // Nothing half done: the credential goes with the state.
            let _ = portable.drop_credential(identity).await;
            return Err(failed(e));
        }
        write_frame(send, &DeviceFrame::Done)
            .await
            .map_err(|e| aborted(e.to_string()))?;
        let _ = send.finish();
        // The phone closes once it has read it.
        let _ = tokio::time::timeout(STEP, conn.closed()).await;
        Ok(Paired {
            identity,
            device_key,
            fingerprint: device_key.fingerprint(),
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
mod tests;
