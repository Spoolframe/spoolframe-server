//! Signing a TV in with a phone: the TV shows one QR code, the phone
//! scans it, the person picks which of the phone's servers to sign the
//! TV in to, and the TV saves each and signs in there as the profile the
//! phone signs in as. The wire is `spoolframe_protocol::phone_sign_in`;
//! no server takes part in the exchange itself.
//!
//! The TV side: [`Client::start_phone_sign_in`] gives a [`PhoneSignIn`]:
//! its [`PhoneSignIn::link`] to show as a QR code, then
//! [`PhoneSignIn::wait`]. The code is the pairing's ([`crate::pairing`]),
//! so with Allow guests on a guest's phone may pair its own identity with
//! the same code ([`Client::scan_pairing`]); either ends the code.
//!
//! The phone side: [`Client::scan_phone_sign_in`] gives a
//! [`SignInOffer`]: what the TV is, and [`SignInOffer::servers`], the
//! profiles this phone can sign it in as. Then [`SignInOffer::approve`]
//! with those the person picked, or [`SignInOffer::decline`].
//!
//! Each server's sign-in is made by the phone's session there
//! ([`crate::gate::On::offer_device_sign_in`]), as an approval of a code
//! is: it needs a recent sign-in. The TV redeems it as a device sign-in
//! it began itself; nothing is made on the server until it does.

use std::time::Duration;

use futures_util::StreamExt;
use futures_util::future::join_all;
use futures_util::stream::FuturesUnordered;
use iroh::EndpointId;
use iroh::endpoint::{Connection, RecvStream, SendStream};
use spoolframe_protocol::codec::{read_frame_within, write_frame};
use spoolframe_protocol::pairing::{self, PairedServer};
use spoolframe_protocol::phone_sign_in::{
    MAX_FRAME, MAX_OFFERS, Offer, OfferOutcome, OfferResult, PhoneFrame, Refusal, SIGNIN_ALPN,
    TvFrame,
};
use spoolframe_types::Platform;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::client::{Client, parse_id};
use crate::error::SdkError;
use crate::gate;
use crate::held::{Held, HeldOffer, HeldPhoneSignIn, Hold};
use crate::pairing::{
    APPROVAL, CODE_TTL, Connected, Paired, PairingError, Progress, STEP, Shared, confirmed, step,
};
use crate::portable_sign_in::vouched_for;
use crate::server_sign_ins::{ServerSignIn, ServerSignIns, Tally};
use crate::servers;
use crate::session::Session;

/// How long the TV gives one server to hand over its session, and the
/// phone the TV to say how all went: past it the server is not reached.
const REDEEM: Duration = Duration::from_secs(60);

impl From<Refusal> for PairingError {
    fn from(refusal: Refusal) -> Self {
        match refusal {
            Refusal::WrongCode => Self::WrongCode,
            Refusal::Expired => Self::Expired,
            Refusal::Failed | Refusal::Unknown => Self::Failed,
        }
    }
}

/// A phone that reached the TV's code, before it is done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhoneConnected {
    /// A phone signing the TV in to its accounts: "Continue on your
    /// phone".
    SigningIn,
    /// A guest's phone pairing its own identity: show
    /// [`Connected::fingerprint`] as a pairing does.
    Guest(Connected),
}

/// How a code shown with [`Client::start_phone_sign_in`] ended, once
/// the TV confirmed: what [`crate::HeldSignIn::confirm`] and
/// [`crate::HeldPairing::confirm`] give.
#[derive(Debug, Clone)]
pub(crate) enum PhoneSignInOutcome {
    /// A phone signed this TV in to the servers its person picked.
    SignedIn(PhoneSignedIn),
    /// A guest's phone paired its own identity (Allow guests is on).
    Paired(Paired),
}

/// What a phone signed this TV in to: [`crate::PairedSignIn`]'s shape.
/// Each server is saved here, pinned but not verified (the phone
/// vouched for it, not this device's owner), whether or not its sign-in
/// went through; a server saved here already is left as it was.
#[derive(Debug, Clone, Default)]
pub struct PhoneSignedIn {
    /// In the phone's order: open the first.
    pub sessions: Vec<Session>,
    /// Each server not signed in to, a hex endpoint id, and why.
    pub failed: Vec<(String, SdkError)>,
}

/// A code this TV shows for a phone to sign it in (the TV side). Show
/// [`PhoneSignIn::link`] as a QR code, then [`PhoneSignIn::wait`].
/// Dropping it, or [`PhoneSignIn::cancel`], stops it: the TV takes no
/// more connections for it at once.
pub struct PhoneSignIn {
    link: String,
    expires_at: Instant,
    progress: watch::Receiver<Progress>,
    tally: watch::Receiver<Tally>,
    task: JoinHandle<()>,
}

impl std::fmt::Debug for PhoneSignIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhoneSignIn")
            .field("expires_in", &self.expires_in())
            .finish_non_exhaustive()
    }
}

impl PhoneSignIn {
    /// What the QR code carries: a pairing link,
    /// `spoolframe://pair?e=…&c=…&n=…&a=…`. It holds the one-time code:
    /// show it, never log it.
    pub fn link(&self) -> &str {
        &self.link
    }

    /// How long the code is still good for: until then a phone can
    /// start; one started finishes after.
    pub fn expires_in(&self) -> Duration {
        self.expires_at.saturating_duration_since(Instant::now())
    }

    /// The phone that presented the code, once one has: before the person
    /// on it answers. If the code's use ends before a phone gets that far,
    /// the error [`PhoneSignIn::wait`] gives. May be called alongside
    /// [`PhoneSignIn::wait`], and again.
    pub async fn connected(&self) -> Result<PhoneConnected, SdkError> {
        Progress::connected(&self.progress).await
    }

    /// What the phone handed, held until the person at the TV says:
    /// [`HeldPhoneSignIn::SignIn`], a phone's offers (who signs the TV in,
    /// to which servers), or [`HeldPhoneSignIn::Guest`], a guest's
    /// pairing. Nothing is saved or signed in to before its `confirm`;
    /// its `reject`, dropping it undecided, or [`APPROVAL`] with no
    /// answer turns it down.
    ///
    /// Or [`SdkError::Pairing`] with why it ended before
    /// ([`PairingError::Expired`], [`PairingError::WrongCode`],
    /// [`PairingError::Declined`], [`PairingError::Aborted`]). A later
    /// call gives the same, sharing its decision.
    pub async fn wait(&self) -> Result<HeldPhoneSignIn, SdkError> {
        let (held, decision) = crate::held::held(&self.progress).await?;
        Ok(HeldPhoneSignIn::new(held, decision, self.progress.clone()))
    }

    /// Each server a phone signs this TV in to, as it finishes: "Signed
    /// in to 2 of 3" while the rest are dialled. Its total is known once
    /// the phone's offers come; it ends with no server if the code ends
    /// another way. Each call reads from the first server again.
    pub fn server_sign_ins(&self) -> ServerSignIns {
        ServerSignIns::new(self.tally.clone())
    }

    /// Stop: the code stops working at once.
    pub fn cancel(&self) {
        self.task.abort();
    }
}

impl Drop for PhoneSignIn {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Client {
    /// Show a code on this TV for a phone to sign it in to the phone's
    /// servers (the TV side). `name` is what the phone shows ("Sign in
    /// Living Room TV?").
    ///
    /// The QR code carries this device's key and the addresses it is
    /// bound at, so a phone on the same network reaches it. With `relay`
    /// it also carries this device's home relay, if it has one, so a
    /// phone elsewhere may reach it through that: the phone uses the
    /// relay only when the code names one.
    ///
    /// The TV takes phone sign-ins while it lives, whether or not the
    /// owner allows guests; with guests allowed
    /// ([`crate::Portable::allow_guests`]) a guest's phone may instead
    /// pair its own identity with the same code. The code is good for
    /// [`CODE_TTL`] and once. One code at a time, this or a pairing's:
    /// [`PairingError::AlreadyPairing`].
    pub async fn start_phone_sign_in(
        &self,
        name: &str,
        relay: bool,
    ) -> Result<PhoneSignIn, SdkError> {
        self.start_phone_sign_in_for(name, relay, CODE_TTL).await
    }

    pub(crate) async fn start_phone_sign_in_for(
        &self,
        name: &str,
        relay: bool,
        ttl: Duration,
    ) -> Result<PhoneSignIn, SdkError> {
        let mut alpns = vec![SIGNIN_ALPN.to_vec()];
        if self.portable().allow_guests().await? {
            alpns.push(pairing::PAIR_ALPN.to_vec());
        }
        let home = self.inner.endpoint.addr().relay_urls().next().cloned();
        let relays = link_relays(relay, home.map(|url| url.to_string()));
        let listener = self.listen(name, ttl, alpns, relays)?;
        Ok(PhoneSignIn {
            link: listener.link,
            expires_at: listener.expires_at,
            progress: listener.progress,
            tally: listener.tally,
            task: listener.task,
        })
    }

    /// Scan a TV's code to sign it in (the phone side): dial the TV it
    /// names and present the code. The TV answers with what it is: show
    /// [`SignInOffer::device_name`] and the profiles
    /// [`SignInOffer::servers`] lists, then [`SignInOffer::approve`] or
    /// [`SignInOffer::decline`].
    ///
    /// The phone dials the TV's key and nothing else, at the addresses in
    /// the code, and through the relay in it if it names one (the TV
    /// decides that, not this device's relay setting). A TV whose code
    /// is spent or wrong answers [`PairingError::WrongCode`] or
    /// [`PairingError::Expired`]; one this phone cannot reach, or too old
    /// to take phone sign-ins, [`SdkError::Unreachable`].
    pub async fn scan_phone_sign_in(&self, link: &str) -> Result<SignInOffer, SdkError> {
        let link = pairing::parse_link(link).map_err(PairingError::from)?;
        let device = EndpointId::from_bytes(&link.endpoint).map_err(|_| {
            PairingError::NotALink(pairing::LinkError::Damaged("not an endpoint id"))
        })?;
        let conn = self.dial_device(&link, SIGNIN_ALPN, true).await?;
        let cut = |e: String| SdkError::from(PairingError::Aborted(e));
        let (mut send, mut recv) = conn.open_bi().await.map_err(|e| cut(e.to_string()))?;
        write_frame(&mut send, &PhoneFrame::Hello { code: link.code })
            .await
            .map_err(|e| cut(e.to_string()))?;
        match step(STEP, read_frame_within(&mut recv, MAX_FRAME)).await? {
            TvFrame::Device {
                name,
                platform,
                client_name,
            } => Ok(SignInOffer {
                device,
                device_name: name.chars().take(pairing::MAX_NAME).collect(),
                platform,
                client_name: client_name.chars().take(pairing::MAX_NAME).collect(),
                client: self.clone(),
                offers: tokio::sync::Mutex::new(Vec::new()),
                streams: tokio::sync::Mutex::new(Some(Streams { conn, send, recv })),
            }),
            TvFrame::Refused { reason } => {
                conn.close(0u32.into(), b"refused");
                Err(PairingError::from(reason).into())
            }
            TvFrame::Done { .. } | TvFrame::Unknown => {
                conn.close(0u32.into(), b"out of turn");
                Err(cut("the TV answered out of turn".into()))
            }
        }
    }

    /// Save the servers `offers` hand this TV, and redeem each offer's
    /// sign-in there, all at once: how each went, in the offers' order,
    /// for the phone, and for the app; each also told to `tally` as it
    /// finishes.
    async fn redeem_offers(
        &self,
        offers: Vec<Offer>,
        tally: &watch::Sender<Tally>,
    ) -> (Vec<OfferResult>, PhoneSignedIn) {
        tally.send_modify(|t| t.total = Some(offers.len()));
        let handed = offers.iter().map(|o| o.server.clone()).collect();
        let saved = self.save_paired_servers(handed).await;
        let mut redeeming: FuturesUnordered<_> = offers
            .iter()
            .enumerate()
            .map(|(at, offer)| {
                let saved = &saved;
                async move { (at, self.redeem_offer(offer, saved).await) }
            })
            .collect();
        let mut redeemed: Vec<Option<Result<Session, SdkError>>> = vec![None; offers.len()];
        while let Some((at, result)) = redeeming.next().await {
            tally.send_modify(|t| {
                t.done.push(ServerSignIn {
                    server: offers[at].server.endpoint_id.to_ascii_lowercase(),
                    result: result.clone(),
                })
            });
            redeemed[at] = Some(result);
        }
        drop(redeeming);
        let redeemed = redeemed
            .into_iter()
            .map(|r| r.expect("each offer redeemed once"));
        let mut results = Vec::with_capacity(offers.len());
        let mut signed = PhoneSignedIn::default();
        for (offer, redeemed) in offers.iter().zip(redeemed) {
            let server = offer.server.endpoint_id.to_ascii_lowercase();
            let outcome = match redeemed {
                Ok(session) => {
                    signed.sessions.push(session);
                    OfferOutcome::SignedIn
                }
                Err(e) => {
                    tracing::info!(%server, error = %e, "a phone's sign-in did not go through");
                    let outcome = OfferOutcome::failed(&e.to_string());
                    signed.failed.push((server.clone(), e));
                    outcome
                }
            };
            results.push(OfferResult { server, outcome });
        }
        (results, signed)
    }

    /// Redeem `offer`, whose server `saved` (what saving the offers'
    /// servers kept) must have.
    async fn redeem_offer(
        &self,
        offer: &Offer,
        saved: &Result<Vec<PairedServer>, SdkError>,
    ) -> Result<Session, SdkError> {
        let Some(card) = offer.server.clone().within_bounds() else {
            return Err(SdkError::InvalidInput("not a server's key".into()));
        };
        match saved {
            Ok(saved) if saved.iter().any(|s| s.endpoint_id == card.endpoint_id) => {}
            Ok(_) => {
                return Err(SdkError::InvalidInput(
                    "a server saved here has its id under another key".into(),
                ));
            }
            Err(e) => return Err(e.clone()),
        }
        let server = parse_id(&card.endpoint_id)?;
        tokio::time::timeout(REDEEM, self.await_device_request(server, &offer.request))
            .await
            .map_err(|_| SdkError::TimedOut(format!("the server {}", server.fmt_short())))?
    }
}

/// The relays a TV's code names: its home relay, and only when the app
/// asked for one.
fn link_relays(relay: bool, home: Option<String>) -> Vec<String> {
    home.filter(|_| relay).into_iter().collect()
}

/// One phone sign-in connection: [`crate::pairing`]'s listener hands it
/// those on [`SIGNIN_ALPN`]. `Some` if it decided the code's use, as a
/// pairing connection does.
pub(crate) async fn handle(
    shared: &Shared,
    conn: Connection,
) -> Option<Result<PhoneSignedIn, SdkError>> {
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
    let PhoneFrame::Hello { code } = hello else {
        shared.refused_as("signin_hello", &source, None, "no hello");
        return None;
    };
    if let Err((reason, decided)) = shared.take(&code) {
        let (reason, why) = match reason {
            pairing::Refusal::Expired => (Refusal::Expired, "the code has expired"),
            _ => (Refusal::WrongCode, "the code does not match"),
        };
        shared.refused_as("signin_hello", &source, None, why);
        refuse(&conn, &mut send, reason).await;
        return decided.then(|| Err(PairingError::from(reason).into()));
    }
    let outcome = sign_in(shared, &conn, &mut send, &mut recv).await;
    if let Err((Some(reason), _)) = &outcome {
        refuse(&conn, &mut send, *reason).await;
    }
    Some(outcome.map_err(|(_, e)| e))
}

/// Tell the phone why, and wait briefly for it to hang up.
async fn refuse(conn: &Connection, send: &mut SendStream, reason: Refusal) {
    let _ = write_frame(send, &TvFrame::Refused { reason }).await;
    let _ = send.finish();
    let _ = tokio::time::timeout(STEP, conn.closed()).await;
}

/// The rest of a sign-in whose code was right: what the TV is, the
/// phone's offers, each redeemed, and how each went. On an error, the
/// refusal to send the phone, if any, and the error.
async fn sign_in(
    shared: &Shared,
    conn: &Connection,
    send: &mut SendStream,
    recv: &mut RecvStream,
) -> Result<PhoneSignedIn, (Option<Refusal>, SdkError)> {
    let aborted = |e: String| (None, SdkError::from(PairingError::Aborted(e)));
    let device = &shared.client.inner.device;
    write_frame(
        send,
        &TvFrame::Device {
            name: shared.name.clone(),
            platform: device.platform,
            client_name: device.client_name.clone(),
        },
    )
    .await
    .map_err(|e| aborted(e.to_string()))?;
    shared
        .progress
        .send_modify(|p| p.connected = Some(PhoneConnected::SigningIn));
    let answer = step(
        APPROVAL,
        read_frame_within::<_, PhoneFrame>(recv, MAX_FRAME),
    )
    .await
    .map_err(|e| (None, e))?;
    let offers = match answer {
        PhoneFrame::Offers { mut offers } => {
            offers.truncate(MAX_OFFERS);
            offers
        }
        PhoneFrame::Declined => return Err((None, PairingError::Declined.into())),
        PhoneFrame::Hello { .. } | PhoneFrame::Unknown => {
            return Err(aborted("the phone answered out of turn".into()));
        }
    };
    // Held for the person at the TV: nothing saved or redeemed unless
    // they confirm.
    let (hold, answered) = Hold::new();
    let held = offers
        .iter()
        .map(|o| HeldOffer {
            server: o.server.clone(),
            user_name: o.user_name.chars().take(pairing::MAX_NAME).collect(),
        })
        .collect();
    shared
        .progress
        .send_modify(|p| p.held = Some((Held::SignIn(held), hold)));
    if !confirmed(conn, answered).await {
        let results = offers
            .iter()
            .map(|o| OfferResult {
                server: o.server.endpoint_id.to_ascii_lowercase(),
                outcome: OfferOutcome::Declined,
            })
            .collect();
        if write_frame(send, &TvFrame::Done { results }).await.is_ok() {
            let _ = send.finish();
            let _ = tokio::time::timeout(STEP, conn.closed()).await;
        }
        return Err((None, crate::held::rejected_here()));
    }
    let (results, signed) = shared.client.redeem_offers(offers, &shared.tally).await;
    // The phone may have gone meanwhile: what was signed in stays.
    if write_frame(send, &TvFrame::Done { results }).await.is_ok() {
        let _ = send.finish();
        let _ = tokio::time::timeout(STEP, conn.closed()).await;
    }
    Ok(signed)
}

struct Streams {
    conn: Connection,
    send: SendStream,
    recv: RecvStream,
}

/// A profile this phone can sign a TV in as: a session of an account on
/// one of its servers ([`SignInOffer::servers`]).
#[derive(Debug, Clone)]
pub struct SignInChoice {
    /// The session that makes the sign-in: the TV signs in as its user.
    pub session: Session,
    /// The server as this phone has it saved, which the TV saves too.
    pub server: PairedServer,
}

/// How one server's sign-in went, for the phone to show.
#[derive(Debug, Clone)]
pub struct SignInOfferResult {
    /// The server's key, hex.
    pub endpoint_id: String,
    pub server_name: String,
    /// Who the TV signs in as there: the profile's display name.
    pub user_name: String,
    pub outcome: SignInOfferOutcome,
}

/// Whether the TV is signed in to one server.
#[derive(Debug, Clone)]
pub enum SignInOfferOutcome {
    /// It is.
    SignedIn,
    /// This phone could not make the sign-in: the server's answer
    /// ([`SdkError::RecentSignInRequired`]: confirm it is them, then
    /// [`SignInOffer::offer`] again).
    NotOffered(SdkError),
    /// The TV could not redeem it: why, as the TV said (it could not
    /// reach the server, most likely).
    Failed(String),
    /// The person at the TV turned the sign-in down: nothing was
    /// redeemed, and the grant runs out unused.
    Declined,
}

/// A TV that answered a scanned code (the phone side): what to show
/// before the person picks the servers to sign it in to.
pub struct SignInOffer {
    device: EndpointId,
    device_name: String,
    platform: Platform,
    client_name: String,
    client: Client,
    /// The sign-ins made so far, to send, with who each signs in as.
    offers: tokio::sync::Mutex<Vec<(Offer, String)>>,
    streams: tokio::sync::Mutex<Option<Streams>>,
}

impl std::fmt::Debug for SignInOffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignInOffer")
            .field("device", &self.device)
            .field("device_name", &self.device_name)
            .field("platform", &self.platform)
            .finish_non_exhaustive()
    }
}

/// A phone that goes without answering tells the TV at once, rather
/// than leaving it waiting for the connection to time out.
impl Drop for SignInOffer {
    fn drop(&mut self) {
        if let Some(streams) = self.streams.get_mut().take() {
            streams.conn.close(0u32.into(), b"gone");
        }
    }
}

impl SignInOffer {
    /// What the TV calls itself: "Sign in Living Room TV?".
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// The TV's platform, as it said: for its icon.
    pub fn platform(&self) -> Platform {
        self.platform
    }

    /// The TV's app, as it said.
    pub fn client_name(&self) -> &str {
        &self.client_name
    }

    /// The TV's key.
    pub fn device(&self) -> EndpointId {
        self.device
    }

    /// The profiles this phone can sign the TV in as, one per session,
    /// for the person to tick: each on a server verified here with its
    /// server id pinned, of a local account (a portable identity's tied
    /// to one included, not a guest's), where the server has
    /// `spoolframe:phone-sign-in`. Each server is asked who its sessions
    /// are, all at once; one not answering is left out.
    pub async fn servers(&self) -> Result<Vec<SignInChoice>, SdkError> {
        let saved = servers::load(self.client.inner.storage.as_ref()).await?;
        let cards: Vec<_> = saved.iter().filter_map(vouched_for).collect();
        let asked = join_all(cards.iter().map(|(id, _)| {
            let client = &self.client;
            async move { client.profiles(&id.to_string()).await }
        }))
        .await;
        let mut choices = Vec::new();
        for ((id, card), profiles) in cards.into_iter().zip(asked) {
            let profiles = match profiles {
                Ok(profiles) => profiles,
                Err(e) => {
                    tracing::debug!(server = %id, error = %e, "left out of a phone sign-in");
                    continue;
                }
            };
            choices.extend(
                profiles
                    .into_iter()
                    .filter(|s| s.user().portable_id().is_none())
                    .filter(|s| s.has_feature(spoolframe_protocol::Feature::PhoneSignIn))
                    .map(|session| SignInChoice {
                        session,
                        server: card.clone(),
                    }),
            );
        }
        Ok(choices)
    }

    /// Make `choice`'s sign-in for the TV on its server now, to send with
    /// [`SignInOffer::send`]; `remember` trusts the TV for its user
    /// there. The server's refusal is this call's error: on
    /// [`SdkError::RecentSignInRequired`], confirm it is them
    /// ([`Session::reauth_options`]) and call this again. The same
    /// profile again replaces its sign-in. At most
    /// [`MAX_OFFERS`] servers ([`SdkError::InvalidInput`] past it); none
    /// once sent.
    pub async fn offer(&self, choice: &SignInChoice, remember: bool) -> Result<(), SdkError> {
        if self.streams.lock().await.is_none() {
            return Err(PairingError::Aborted("already answered".into()).into());
        }
        let user_name = choice.session.user().display_name.clone();
        let username = choice.session.user().username.clone();
        let endpoint_id = choice.server.endpoint_id.clone();
        let same = |(o, u): &(Offer, String)| o.server.endpoint_id == endpoint_id && *u == username;
        {
            let offers = self.offers.lock().await;
            if offers.len() >= MAX_OFFERS && !offers.iter().any(same) {
                return Err(SdkError::InvalidInput(format!(
                    "at most {MAX_OFFERS} servers at a time"
                )));
            }
        }
        let request = choice
            .session
            .feature::<gate::PhoneSignIn>()?
            .offer_device_sign_in(self.device, &self.device_name, self.platform, remember)
            .await?;
        let mut offers = self.offers.lock().await;
        offers.retain(|o| !same(o));
        offers.push((
            Offer {
                server: choice.server.clone(),
                request,
                user_name,
            },
            username,
        ));
        Ok(())
    }

    /// Hand the TV every sign-in [`SignInOffer::offer`] made, and wait
    /// for it to sign in to each: how each went, in the order they were
    /// made. [`SdkError::InvalidInput`] with none made, and nothing sent.
    /// Once only.
    pub async fn send(&self) -> Result<Vec<SignInOfferResult>, SdkError> {
        let offers = std::mem::take(&mut *self.offers.lock().await);
        if offers.is_empty() {
            return Err(SdkError::InvalidInput(
                "no server to sign the TV in to".into(),
            ));
        }
        let Some(Streams {
            conn,
            mut send,
            mut recv,
        }) = self.streams.lock().await.take()
        else {
            return Err(PairingError::Aborted("already answered".into()).into());
        };
        let cut = |e: String| SdkError::from(PairingError::Aborted(e));
        let (offers, _): (Vec<Offer>, Vec<String>) = offers.into_iter().unzip();
        write_frame(
            &mut send,
            &PhoneFrame::Offers {
                offers: offers.clone(),
            },
        )
        .await
        .map_err(|e| cut(e.to_string()))?;
        // The TV signs in to each before it answers.
        let answer = step(
            // The person at the TV confirms first, then it signs in.
            APPROVAL + REDEEM + STEP,
            read_frame_within::<_, TvFrame>(&mut recv, MAX_FRAME),
        )
        .await;
        conn.close(0u32.into(), b"done");
        let mut results = match answer? {
            TvFrame::Done { results } => results.into_iter(),
            TvFrame::Refused { reason } => return Err(PairingError::from(reason).into()),
            TvFrame::Device { .. } | TvFrame::Unknown => {
                return Err(cut("the TV answered out of turn".into()));
            }
        };
        let done = offers
            .into_iter()
            .map(|offer| {
                let outcome = match results.next().map(|r| r.outcome) {
                    Some(OfferOutcome::SignedIn) => SignInOfferOutcome::SignedIn,
                    Some(OfferOutcome::Failed { reason }) => SignInOfferOutcome::Failed(reason),
                    Some(OfferOutcome::Declined) => SignInOfferOutcome::Declined,
                    Some(OfferOutcome::Unknown) => {
                        SignInOfferOutcome::Failed("the TV's answer is not known here".into())
                    }
                    None => SignInOfferOutcome::Failed("the TV did not say".into()),
                };
                SignInOfferResult {
                    endpoint_id: offer.server.endpoint_id,
                    server_name: offer.server.name,
                    user_name: offer.user_name,
                    outcome,
                }
            })
            .collect::<Vec<_>>();
        tracing::info!(
            device = %self.device.fmt_short(),
            signed_in = done
                .iter()
                .filter(|r| matches!(r.outcome, SignInOfferOutcome::SignedIn))
                .count(),
            "signed a TV in"
        );
        Ok(done)
    }

    /// [`SignInOffer::offer`] each of `chosen`, then [`SignInOffer::send`]:
    /// how each went, in `chosen`'s order. One the phone could not offer
    /// is [`SignInOfferOutcome::NotOffered`] and stops none of the rest.
    /// If none could be offered nothing is sent, and the TV still waits:
    /// confirm it is them where the server asked, and approve again.
    pub async fn approve(
        &self,
        chosen: &[SignInChoice],
        remember: bool,
    ) -> Result<Vec<SignInOfferResult>, SdkError> {
        let mut not_offered = Vec::new();
        for (at, choice) in chosen.iter().enumerate() {
            if let Err(e) = self.offer(choice, remember).await {
                not_offered.push((at, e));
            }
        }
        let mut sent = if not_offered.len() < chosen.len() {
            self.send().await?
        } else {
            Vec::new()
        };
        let results = chosen
            .iter()
            .enumerate()
            .map(|(at, choice)| {
                let user_name = choice.session.user().display_name.clone();
                let outcome = match not_offered.iter().find(|(n, _)| *n == at) {
                    Some((_, e)) => SignInOfferOutcome::NotOffered(e.clone()),
                    None => match sent.iter().position(|r| {
                        r.endpoint_id == choice.server.endpoint_id && r.user_name == user_name
                    }) {
                        Some(found) => return sent.remove(found),
                        None => SignInOfferOutcome::Failed("the TV did not say".into()),
                    },
                };
                SignInOfferResult {
                    endpoint_id: choice.server.endpoint_id.clone(),
                    server_name: choice.server.name.clone(),
                    user_name,
                    outcome,
                }
            })
            .collect();
        Ok(results)
    }

    /// Decline: the TV is told, and gets nothing. The sign-ins
    /// [`SignInOffer::offer`] made are left to run out unredeemed. Once
    /// only.
    pub async fn decline(&self) -> Result<(), SdkError> {
        self.offers.lock().await.clear();
        let Some(Streams { conn, mut send, .. }) = self.streams.lock().await.take() else {
            return Err(PairingError::Aborted("already answered".into()).into());
        };
        let sent = write_frame(&mut send, &PhoneFrame::Declined).await;
        let _ = send.finish();
        let _ = tokio::time::timeout(STEP, conn.closed()).await;
        sent.map_err(|e| PairingError::Aborted(e.to_string()).into())
    }
}

#[cfg(test)]
#[path = "phone_sign_in_tests.rs"]
mod tests;
