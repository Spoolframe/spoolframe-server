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

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use futures_util::future::join_all;
use iroh::EndpointId;
use iroh::endpoint::{Connection, RecvStream, SendStream};
use spoolframe_protocol::codec::{read_frame_within, write_frame};
use spoolframe_protocol::pairing::{self, PairedServer};
use spoolframe_protocol::phone_sign_in::{
    FailureKind, MAX_FRAME, MAX_OFFERS, Offer, OfferOutcome, OfferResult, PhoneFrame, Refusal,
    SIGNIN_ALPN, TvFrame,
};
use spoolframe_types::Platform;
use tokio::sync::{oneshot, watch};

use crate::client::{Client, parse_id};
use crate::error::SdkError;
use crate::gate;
use crate::held::{Held, HeldOffer, HeldPhoneSignIn, Hold};
use crate::pairing::{
    APPROVAL, Answer, CODE_TTL, Connected, Listener, Paired, PairingError, Progress, STEP, Shared,
    confirmed, finish_and_wait_closed, step,
};
use crate::portable_sign_in::vouched_for;
use crate::server_sign_ins::{ServerSignIn, ServerSignIns, Tally, sign_in_each};
use crate::servers;
use crate::session::{self, SavedProfile, Session, StoredSession};
use crate::whos_watching::PairedSignIn;

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
    SignedIn(PairedSignIn),
    /// A guest's phone paired its own identity (Allow guests is on).
    Paired(Paired),
}

/// A code this TV shows for a phone to sign it in (the TV side). Show
/// [`PhoneSignIn::link`] as a QR code, then [`PhoneSignIn::wait`].
/// Dropping it, or [`PhoneSignIn::cancel`], stops it: the TV takes no
/// more connections for it at once.
pub struct PhoneSignIn {
    listener: Listener,
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
        &self.listener.link
    }

    /// How long the code is still good for: until then a phone can
    /// start; one started finishes after.
    pub fn expires_in(&self) -> Duration {
        self.listener.expires_in()
    }

    /// The phone that presented the code, once one has: before the person
    /// on it answers. If the code's use ends before a phone gets that far,
    /// the error [`PhoneSignIn::wait`] gives. May be called alongside
    /// [`PhoneSignIn::wait`], and again.
    pub async fn connected(&self) -> Result<PhoneConnected, SdkError> {
        Progress::connected(&self.listener.progress).await
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
        let progress = &self.listener.progress;
        let (held, decision) = Progress::held(progress).await?;
        Ok(HeldPhoneSignIn::new(held, decision, progress.clone()))
    }

    /// Each server a phone signs this TV in to, as it finishes: "Signed
    /// in to 2 of 3" while the rest are dialled. Its total is known once
    /// the phone's offers come; it ends with no server if the code ends
    /// another way. Each call reads from the first server again.
    pub fn server_sign_ins(&self) -> ServerSignIns {
        ServerSignIns::new(self.listener.tally.clone())
    }

    /// Stop: the code stops working at once.
    pub fn cancel(&self) {
        self.listener.cancel();
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
    /// pair its own identity with the same code. With guests off, a
    /// guest's phone that scans it is refused
    /// ([`PairingError::NotAllowed`] there) and the code is left for a
    /// phone signing the TV in. The code is good for
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
        // A guest's dial is taken with guests off too, to be refused
        // typed (`NotAllowed`) instead of failing the handshake.
        let alpns = vec![SIGNIN_ALPN.to_vec(), pairing::PAIR_ALPN.to_vec()];
        let home = self.inner.endpoint.addr().relay_urls().next().cloned();
        let relays = link_relays(relay, home.map(|url| url.to_string()));
        let listener = self.listen(name, ttl, alpns, relays)?;
        Ok(PhoneSignIn { listener })
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
        let conn = self.dial_device(&link, SIGNIN_ALPN, true).await?;
        // Iroh proved this key: the one the link names.
        let device = conn.remote_id();
        let cut = |e: String| SdkError::from(PairingError::Aborted(e));
        let (mut send, mut recv) = conn.open_bi().await.map_err(|e| cut(e.to_string()))?;
        let name = crate::pairing::sent_name(Some(self.inner.device.name.clone()));
        write_frame(
            &mut send,
            &PhoneFrame::Hello {
                code: link.code,
                name,
            },
        )
        .await
        .map_err(|e| cut(e.to_string()))?;
        match step(STEP, read_frame_within(&mut recv, MAX_FRAME)).await? {
            TvFrame::Device {
                name,
                platform,
                client_name,
            } => Ok(SignInOffer {
                device,
                device_name: pairing::cut_name(&name),
                platform,
                client_name: pairing::cut_name(&client_name),
                client: self.clone(),
                offers: tokio::sync::Mutex::new(Vec::new()),
                signed_in: Default::default(),
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
    /// sign-in there, all at once: each one's session or why not, in the
    /// offers' order, each also told to `tally` as it finishes.
    async fn redeem_offers(
        &self,
        offers: &[Offer],
        tally: &watch::Sender<Tally>,
    ) -> Vec<Result<Session, SdkError>> {
        let handed = offers.iter().map(|o| o.server.clone()).collect();
        let saved = self.save_paired_servers(handed).await;
        let redeeming = offers
            .iter()
            .map(|offer| {
                let saved = &saved;
                let (server, who) = Profiles::key_of(offer);
                (format!("{server}/{who}"), async move {
                    ServerSignIn {
                        server: offer.server.endpoint_id.to_ascii_lowercase(),
                        result: self.redeem_offer(offer, saved).await,
                    }
                })
            })
            .collect();
        sign_in_each(tally, redeeming).await
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
        // The same operation is asked again on each new connection until
        // the deadline: one the server answered on a connection that was
        // cut replays its `LoggedIn`, so the session made is collected.
        let until = tokio::time::Instant::now() + REDEEM;
        tokio::time::timeout_at(
            until,
            self.await_device_request_until(server, &offer.request, Some(until)),
        )
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
/// pairing connection does: once confirmed, with the first round's
/// sign-ins, while later rounds go on in a task of their own
/// ([`rounds`]).
pub(crate) async fn handle(
    shared: &Arc<Shared>,
    conn: Connection,
) -> Option<Result<PairedSignIn, SdkError>> {
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
    let PhoneFrame::Hello { code, name } = hello else {
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
    let phone_name = crate::pairing::sent_name(name);
    let offers = match held_until_confirmed(shared, &conn, &mut send, &mut recv, phone_name).await {
        Ok(offers) => offers,
        Err((reason, e)) => {
            if let Some(reason) = reason {
                refuse(&conn, &mut send, reason).await;
            }
            return Some(Err(e));
        }
    };
    // Confirmed: from here the sign-in is the person's, not the code's.
    // Its rounds run on their own, so closing the code's screen (the
    // listener) stops none of them, and a new code may be shown meanwhile.
    shared.rounds_open.store(true, Ordering::SeqCst);
    let (first, first_done) = oneshot::channel();
    tokio::spawn(rounds(
        shared.clone(),
        Streams { conn, send, recv },
        offers,
        first,
    ));
    Some(
        first_done
            .await
            .unwrap_or_else(|_| Err(PairingError::Aborted("the sign-in stopped".into()).into())),
    )
}

/// Tell the phone why, and wait briefly for it to hang up.
async fn refuse(conn: &Connection, send: &mut SendStream, reason: Refusal) {
    let _ = write_frame(send, &TvFrame::Refused { reason }).await;
    finish_and_wait_closed(conn, send).await;
}

/// The part of a sign-in whose code was right up to the person at the TV:
/// what the TV is, the phone's first offers, held until they confirm.
/// The offers, confirmed; or the refusal to send the phone, if any, and
/// the error.
async fn held_until_confirmed(
    shared: &Shared,
    conn: &Connection,
    send: &mut SendStream,
    recv: &mut RecvStream,
    phone_name: Option<String>,
) -> Result<Vec<Offer>, (Option<Refusal>, SdkError)> {
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
    shared.progress.send_modify(|p| {
        p.held = Some((
            Held::SignIn {
                offers: held,
                phone_name,
            },
            hold,
        ))
    });
    let answer = confirmed(shared, conn, answered).await;
    if answer == Answer::PhoneLeft {
        // Nobody to tell: the phone is gone.
        return Err((None, crate::pairing::phone_left()));
    }
    if answer == Answer::Rejected {
        let results = offers
            .iter()
            .map(|o| OfferResult {
                server: o.server.endpoint_id.to_ascii_lowercase(),
                outcome: OfferOutcome::Declined,
            })
            .collect();
        if write_frame(send, &TvFrame::Done { results }).await.is_ok() {
            finish_and_wait_closed(conn, send).await;
        }
        return Err((None, crate::held::rejected_here()));
    }
    Ok(offers)
}

/// A confirmed sign-in's rounds: the first round's `offers` redeemed,
/// told to `first` and the phone, then any later rounds the phone sends
/// (servers it could not offer at first, or the TV could not redeem),
/// redeemed as they come. The person at the TV said yes to this phone
/// signing it in, so they are not held again. It ends when the phone
/// hangs up or says no more, or after [`APPROVAL`] without a round.
///
/// Each profile (server and who signs in) counts once however often it
/// is offered: a retry's result replaces its earlier one, in the
/// sign-ins and in the total, and takes no new place among
/// [`MAX_OFFERS`].
async fn rounds(
    shared: Arc<Shared>,
    Streams {
        conn,
        mut send,
        mut recv,
    }: Streams,
    offers: Vec<Offer>,
    first: oneshot::Sender<Result<PairedSignIn, SdkError>>,
) {
    let mut profiles = Profiles::default();
    let mut first = Some(first);
    let mut round = Some(offers);
    while let Some(offers) = round.take() {
        let results = profiles.redeem(&shared, offers).await;
        let signed = profiles.signed();
        let so_far = signed.clone();
        shared.progress.send_modify(|p| {
            p.outcome = Some(Ok(PhoneSignInOutcome::SignedIn(so_far)));
        });
        if let Some(first) = first.take() {
            let _ = first.send(Ok(signed));
        }
        // The phone may have gone meanwhile: what was signed in stays.
        if write_frame(&mut send, &TvFrame::Done { results })
            .await
            .is_err()
        {
            break;
        }
        if let Ok(Ok(PhoneFrame::Offers { offers })) = tokio::time::timeout(
            APPROVAL,
            read_frame_within::<_, PhoneFrame>(&mut recv, MAX_FRAME),
        )
        .await
        {
            round = Some(offers);
        }
    }
    conn.close(0u32.into(), b"done");
    let signed = profiles.signed();
    tracing::info!(
        sessions = signed.sessions.len(),
        failed = signed.failed.len(),
        "a phone's sign-in is done"
    );
    shared.rounds_open.store(false, Ordering::SeqCst);
    shared.tally.send_modify(|t| t.settled = true);
}

/// Each profile a phone's rounds offered, in the order first offered,
/// with its latest result.
#[derive(Default)]
struct Profiles {
    /// Its server (lowercase hex) and who signs in there ([`Profiles::key_of`]).
    keys: Vec<(String, String)>,
    results: Vec<Option<(String, Result<Session, SdkError>)>>,
}

impl Profiles {
    /// An offer's profile: its server, and its account's username, or
    /// its display name from a phone that does not send the username.
    fn key_of(offer: &Offer) -> (String, String) {
        let who = match &offer.username {
            Some(username) => format!("u:{username}"),
            None => format!("n:{}", offer.user_name),
        };
        (offer.server.endpoint_id.to_ascii_lowercase(), who)
    }

    /// Redeem `offers`, a round: each new profile takes a place, while
    /// there are places ([`MAX_OFFERS`]); a profile offered before keeps
    /// its own. How each went, in the round's order.
    async fn redeem(&mut self, shared: &Shared, offers: Vec<Offer>) -> Vec<OfferResult> {
        let mut at = Vec::with_capacity(offers.len());
        for offer in &offers {
            let key = Self::key_of(offer);
            let place = match self.keys.iter().position(|k| *k == key) {
                Some(place) => Some(place),
                None if self.keys.len() < MAX_OFFERS => {
                    self.keys.push(key);
                    self.results.push(None);
                    Some(self.keys.len() - 1)
                }
                None => None,
            };
            at.push(place);
        }
        let total = self.keys.len();
        shared.tally.send_modify(|t| t.total = Some(total));
        let taken: Vec<Offer> = offers
            .iter()
            .zip(&at)
            .filter(|(_, place)| place.is_some())
            .map(|(offer, _)| offer.clone())
            .collect();
        let mut redeemed = shared
            .client
            .redeem_offers(&taken, &shared.tally)
            .await
            .into_iter();
        offers
            .iter()
            .zip(at)
            .map(|(offer, place)| {
                let server = offer.server.endpoint_id.to_ascii_lowercase();
                let Some(place) = place else {
                    return OfferResult {
                        server,
                        outcome: OfferOutcome::failed(
                            FailureKind::Refused,
                            "too many servers for one sign-in",
                        ),
                    };
                };
                let result = redeemed.next().expect("one result per offer taken");
                let outcome = match &result {
                    Ok(_) => OfferOutcome::SignedIn,
                    Err(e) => {
                        tracing::info!(%server, error = %e, "a phone's sign-in did not go through");
                        let failure = SignInFailure::of(e);
                        OfferOutcome::failed(failure.kind(), &failure.to_string())
                    }
                };
                self.results[place] = Some((server.clone(), result));
                OfferResult { server, outcome }
            })
            .collect()
    }

    /// Each profile's latest: signed in, or why not.
    fn signed(&self) -> PairedSignIn {
        let mut signed = PairedSignIn::default();
        for (server, result) in self.results.iter().flatten() {
            match result {
                Ok(session) => signed.sessions.push(session.clone()),
                Err(e) => signed.failed.push((server.clone(), e.clone())),
            }
        }
        signed
    }
}

struct Streams {
    conn: Connection,
    send: SendStream,
    recv: RecvStream,
}

/// A profile this phone can sign a TV in as: an account on one of its
/// servers ([`SignInOffer::servers`]).
#[derive(Debug, Clone)]
pub struct SignInChoice {
    /// The server as this phone has it saved, which the TV saves too.
    pub server: PairedServer,
    pub profile: ChoiceProfile,
}

/// Who a [`SignInChoice`] signs the TV in as, and whether this phone can.
#[derive(Debug, Clone)]
pub enum ChoiceProfile {
    /// Its session here makes the sign-in: the TV signs in as its user.
    Session(Session),
    /// The profile is kept here, but the server ended its session (it
    /// expired, was revoked, or an admin reset the password). Listed so
    /// the phone can say "Sign in to <server> again"; approving it gives
    /// [`SignInFailure::SessionEnded`] and asks the server nothing.
    SessionEnded(SavedProfile),
}

impl SignInChoice {
    /// The session that makes the sign-in; `None` once it has ended.
    pub fn session(&self) -> Option<&Session> {
        match &self.profile {
            ChoiceProfile::Session(session) => Some(session),
            ChoiceProfile::SessionEnded(_) => None,
        }
    }

    /// Whether its session here has ended: sign in to the server again
    /// before it can sign the TV in.
    pub fn session_ended(&self) -> bool {
        matches!(self.profile, ChoiceProfile::SessionEnded(_))
    }

    /// The account's username there.
    pub fn username(&self) -> &str {
        match &self.profile {
            ChoiceProfile::Session(session) => &session.user().username,
            ChoiceProfile::SessionEnded(profile) => &profile.username,
        }
    }

    /// Who the TV signs in as, for the phone's list: the display name.
    pub fn display_name(&self) -> &str {
        match &self.profile {
            ChoiceProfile::Session(session) => &session.user().display_name,
            ChoiceProfile::SessionEnded(profile) => &profile.display_name,
        }
    }
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignInOfferOutcome {
    /// It is.
    SignedIn,
    /// This phone could not make the sign-in, for the server's reason
    /// ([`SignInFailure::RecentSignInRequired`]: confirm it is them, then
    /// offer it again on the same [`SignInOffer`]).
    NotOffered(SignInFailure),
    /// The TV did not sign in: why, as the TV said
    /// ([`SignInFailure::Declined`] when the person at the TV turned it
    /// down: nothing was redeemed, and the grant runs out unused).
    Failed(SignInFailure),
}

/// Why one server did not sign the TV in, for the phone or the TV to
/// show, and to act on: "Confirm it's you", "Try again in a few
/// minutes". [`SignInFailure::of`] reads it from an error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SignInFailure {
    /// Too many sign-ins at once there, or too many waiting (for the
    /// TV, for the user, or on the server): try again in a few minutes.
    #[error("too many sign-ins waiting there; try again in a few minutes")]
    RateLimited,
    /// The phone's session there must confirm it is them first.
    #[error("confirm it's you first")]
    RecentSignInRequired,
    /// The phone's session there has ended (it expired, was revoked, or
    /// an admin reset the password): sign in to that server again.
    #[error("your session there has ended; sign in to it again")]
    SessionEnded,
    /// The account may not sign a device in there: a guest's, or one
    /// whose role does not allow it.
    #[error("this account can't sign a device in there")]
    NotAllowed,
    /// The server does not take sign-ins from a phone (too old, or the
    /// feature is off).
    #[error("that server doesn't take sign-ins from a phone")]
    FeatureOff,
    /// It could not be reached, or did not answer in time.
    #[error("couldn't reach the server")]
    Unreachable,
    /// The server refused it otherwise: its message.
    #[error("{0}")]
    Refused(String),
    /// The person at the TV turned it down.
    #[error("turned down on the TV")]
    Declined,
    /// The sign-in ran out before the TV used it.
    #[error("the sign-in expired before the TV used it")]
    Expired,
}

impl SignInFailure {
    /// The failure an error from a server (or from reaching it) is.
    pub fn of(error: &SdkError) -> Self {
        match error {
            SdkError::RateLimited { .. } | SdkError::ServerFull => Self::RateLimited,
            SdkError::RecentSignInRequired => Self::RecentSignInRequired,
            SdkError::NotForPortableUsers | SdkError::AdminRequired | SdkError::NotAllowed => {
                Self::NotAllowed
            }
            SdkError::FeatureOff { .. }
            | SdkError::Unsupported { .. }
            | SdkError::NotOnServer { .. } => Self::FeatureOff,
            SdkError::Unreachable(_)
            | SdkError::TimedOut(_)
            | SdkError::ServerLookupFailed { .. } => Self::Unreachable,
            SdkError::SignInExpired => Self::Expired,
            SdkError::Pairing(PairingError::Declined | PairingError::Rejected) => Self::Declined,
            other => Self::Refused(other.to_string()),
        }
    }

    /// The failure an error from the phone's own offer is, from
    /// [`SignInOffer::offer`]. As
    /// [`SignInFailure::of`], but [`SdkError::SignInRequired`] there means
    /// the phone's session on that server has ended:
    /// [`SignInFailure::SessionEnded`], "Sign in to it again".
    /// [`SignInOffer::approve`] reads its errors with this already.
    ///
    /// Not part of [`SignInFailure::of`], which also reads a TV's errors:
    /// a TV told it must sign in has nothing to do on the phone.
    pub fn of_offer(error: &SdkError) -> Self {
        match error {
            SdkError::SignInRequired => Self::SessionEnded,
            other => Self::of(other),
        }
    }

    /// The wire's kind for it.
    fn kind(&self) -> FailureKind {
        match self {
            Self::RateLimited => FailureKind::RateLimited,
            Self::RecentSignInRequired => FailureKind::RecentSignInRequired,
            Self::SessionEnded => FailureKind::SessionEnded,
            Self::NotAllowed => FailureKind::NotAllowed,
            Self::FeatureOff => FailureKind::FeatureOff,
            Self::Unreachable => FailureKind::Unreachable,
            Self::Expired => FailureKind::Expired,
            // A TV does not send a decline as a failure.
            Self::Refused(_) | Self::Declined => FailureKind::Refused,
        }
    }

    /// What a TV said: `kind`, and `reason` for one refused or of a kind
    /// this build does not know.
    fn from_wire(kind: FailureKind, reason: String) -> Self {
        match kind {
            FailureKind::RateLimited => Self::RateLimited,
            FailureKind::RecentSignInRequired => Self::RecentSignInRequired,
            FailureKind::SessionEnded => Self::SessionEnded,
            FailureKind::NotAllowed => Self::NotAllowed,
            FailureKind::FeatureOff => Self::FeatureOff,
            FailureKind::Unreachable => Self::Unreachable,
            FailureKind::Expired => Self::Expired,
            FailureKind::Refused | FailureKind::Unknown => Self::Refused(reason),
        }
    }
}

/// The choices of one server: `live`, the sessions it answered for, and
/// the profiles `stored` here, read once every server has answered. One
/// choice per profile, and the later read wins. A live session is listed
/// only while the read still keeps it, under its username with its
/// token: one whose token is gone ended after the server answered (it
/// signed the phone out while another server was still asked), so it is
/// `SessionEnded`, not a session that no longer works; one forgotten
/// here, or replaced by another sign-in, is not listed live.
///
/// `phone_sign_in` is whether the server has `spoolframe:phone-sign-in`.
/// A live session needs it known on. An ended profile is left out only when it is known off:
/// signing in again there could never sign the TV in. Unknown, it is
/// listed, as it needs no answer from the server.
///
/// Guests' profiles (a portable identity's own session) are never
/// listed, live or ended.
fn server_choices(
    card: &PairedServer,
    live: Vec<Session>,
    stored: &[StoredSession],
    phone_sign_in: PhoneSignInSupport,
) -> Vec<SignInChoice> {
    let kept: std::collections::HashSet<(&str, &str)> = stored
        .iter()
        .filter_map(|s| Some((s.username.as_str(), s.token.as_deref()?)))
        .collect();
    let mut choices = Vec::new();
    if phone_sign_in == PhoneSignInSupport::On {
        choices.extend(
            live.into_iter()
                .filter(|s| !s.user().is_guest())
                .filter(|s| kept.contains(&(s.user().username.as_str(), s.token.0.as_str())))
                .map(|session| SignInChoice {
                    server: card.clone(),
                    profile: ChoiceProfile::Session(session),
                }),
        );
    }
    if phone_sign_in != PhoneSignInSupport::Off {
        choices.extend(
            stored
                .iter()
                .filter(|s| s.token.is_none() && !s.is_guest())
                .map(|s| SignInChoice {
                    server: card.clone(),
                    profile: ChoiceProfile::SessionEnded(SavedProfile::from(s)),
                }),
        );
    }
    choices
}

/// Whether a server takes phone sign-ins (`spoolframe:phone-sign-in`), as
/// this device last heard from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PhoneSignInSupport {
    On,
    Off,
    /// Not heard from yet.
    Unknown,
}

impl PhoneSignInSupport {
    fn of(client: &Client, server: EndpointId) -> Self {
        match client.server_has_feature(server, spoolframe_protocol::Feature::PhoneSignIn) {
            Some(true) => Self::On,
            Some(false) => Self::Off,
            None => Self::Unknown,
        }
    }
}

/// One profile: its server's key and its username there, which two
/// profiles with one display name do not share.
type ProfileKey = (String, String);

/// A sign-in made for the TV, and the profile it is for.
struct Made {
    offer: Offer,
    key: ProfileKey,
}

impl Made {
    fn key_of(choice: &SignInChoice) -> ProfileKey {
        (
            choice.server.endpoint_id.clone(),
            choice.username().to_owned(),
        )
    }

    /// Whether `offers` take one more for `key`: one already there is
    /// replaced, else at most [`MAX_OFFERS`].
    fn has_room(offers: &[Made], key: &ProfileKey) -> bool {
        offers.len() < MAX_OFFERS || offers.iter().any(|m| m.key == *key)
    }

    /// Keep `made` in `offers`, in place of the same profile's.
    fn keep(offers: &mut Vec<Made>, made: Made) -> Result<(), SdkError> {
        if !Self::has_room(offers, &made.key) {
            return Err(too_many());
        }
        offers.retain(|m| m.key != made.key);
        offers.push(made);
        Ok(())
    }
}

fn too_many() -> SdkError {
    SdkError::InvalidInput(format!("at most {MAX_OFFERS} servers at a time"))
}

/// A TV that answered a scanned code (the phone side): what to show
/// before the person picks the servers to sign it in to.
pub struct SignInOffer {
    device: EndpointId,
    device_name: String,
    platform: Platform,
    client_name: String,
    client: Client,
    /// The sign-ins made so far, to send.
    offers: tokio::sync::Mutex<Vec<Made>>,
    /// The profiles the TV said it signed in to in an earlier round:
    /// never offered again, so no second session is made for them.
    signed_in: parking_lot::Mutex<std::collections::HashSet<ProfileKey>>,
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

    /// The profiles this phone can sign the TV in as, for the person to
    /// tick: each on a server verified here with its server id pinned,
    /// of a local account (a portable identity's tied to one included,
    /// not a guest's). One per live session where the server has
    /// `spoolframe:phone-sign-in`: each server is asked who its sessions
    /// are, all at once, and one not answering has none listed.
    ///
    /// Then, for each server, the profiles kept here whose session the
    /// server ended, [`ChoiceProfile::SessionEnded`]: "Sign in to
    /// <server> again". Those need no answer from the server, so they are
    /// listed for one not answering too, and whether it takes phone
    /// sign-ins is known only once they are signed in again.
    pub async fn servers(&self) -> Result<Vec<SignInChoice>, SdkError> {
        let storage = self.client.inner.storage.as_ref();
        let saved = servers::load(storage).await?;
        let cards: Vec<_> = saved.iter().filter_map(vouched_for).collect();
        // Every server asked, all at once.
        let asked = join_all(cards.iter().map(|(id, _)| {
            let client = &self.client;
            async move {
                client.profiles(&id.to_string()).await.unwrap_or_else(|e| {
                    tracing::debug!(server = %id, error = %e, "no sessions listed for a phone sign-in");
                    Vec::new()
                })
            }
        }))
        .await;
        // Then each server's profiles read, once every server has
        // answered: a session one server ended while a slower one was
        // still asked is ended in these. One server's profiles that cannot
        // be read lose only its own choices, as a server not answering
        // does.
        let read = join_all(cards.iter().map(|(id, _)| async move {
            session::load(storage, id).await.unwrap_or_else(|e| {
                tracing::warn!(server = %id, error = %e, "a server's profiles cannot be read; none listed for a phone sign-in");
                Vec::new()
            })
        }))
        .await;
        let mut choices = Vec::new();
        for (((id, card), live), stored) in cards.into_iter().zip(asked).zip(read) {
            let support = PhoneSignInSupport::of(&self.client, id);
            choices.extend(server_choices(&card, live, &stored, support));
        }
        Ok(choices)
    }

    /// Make `choice`'s sign-in for the TV on its server now, to send with
    /// [`SignInOffer::send`]; `remember` trusts the TV for its user
    /// there. The server's refusal is this call's error: on
    /// [`SdkError::RecentSignInRequired`], confirm it is them
    /// ([`Session::reauth_options`]) and call this again; on
    /// [`SdkError::SignInRequired`], its session has ended (as for a
    /// [`ChoiceProfile::SessionEnded`] choice, which is not tried): sign
    /// in to the server again. The same profile again replaces its
    /// sign-in. At most
    /// [`MAX_OFFERS`] servers ([`SdkError::InvalidInput`] past it); none
    /// once sent.
    pub async fn offer(&self, choice: &SignInChoice, remember: bool) -> Result<(), SdkError> {
        let key = Made::key_of(choice);
        if self.signed_in.lock().contains(&key) {
            // The TV is signed in to it already: nothing to offer.
            return Ok(());
        }
        self.check_open().await?;
        if !Made::has_room(&self.offers.lock().await, &key) {
            return Err(too_many());
        }
        let made = self.make(choice, remember).await?;
        Made::keep(&mut *self.offers.lock().await, made)
    }

    /// [`SdkError::Pairing`] once answered: nothing more can be sent.
    async fn check_open(&self) -> Result<(), SdkError> {
        if self.streams.lock().await.is_none() {
            return Err(PairingError::Aborted("already answered".into()).into());
        }
        Ok(())
    }

    /// Ask `choice`'s server for the TV's sign-in. One whose session has
    /// ended is [`SdkError::SignInRequired`], with nothing asked.
    async fn make(&self, choice: &SignInChoice, remember: bool) -> Result<Made, SdkError> {
        let Some(session) = choice.session() else {
            return Err(SdkError::SignInRequired);
        };
        let request = session
            .feature::<gate::PhoneSignIn>()?
            .offer_device_sign_in(self.device, &self.device_name, self.platform, remember)
            .await?;
        Ok(Made {
            offer: Offer {
                server: choice.server.clone(),
                request,
                user_name: session.user().display_name.clone(),
                username: Some(session.user().username.clone()),
            },
            key: Made::key_of(choice),
        })
    }

    /// Hand the TV every sign-in [`SignInOffer::offer`] made, and wait
    /// for it to sign in to each: how each went, in the order they were
    /// made. [`SdkError::InvalidInput`] with none made, and nothing sent.
    ///
    /// If one [`SignInOfferOutcome::Failed`], the TV still waits: offer
    /// it again (or another) and send again, with no new scan, until
    /// [`SignInOffer::finish`]. Otherwise the TV is told it is done.
    pub async fn send(&self) -> Result<Vec<SignInOfferResult>, SdkError> {
        Ok(self
            .send_made(false)
            .await?
            .into_iter()
            .map(|(_, r)| r)
            .collect())
    }

    /// Tell the TV this phone offers no more: it stops waiting for
    /// another round. Dropping the offer does the same.
    pub async fn finish(&self) {
        if let Some(streams) = self.streams.lock().await.take() {
            streams.conn.close(0u32.into(), b"done");
        }
    }

    /// [`SignInOffer::send`], each result with the profile it is for.
    /// The TV is kept waiting for another round if `more` may follow, or
    /// one failed.
    async fn send_made(
        &self,
        more: bool,
    ) -> Result<Vec<(ProfileKey, SignInOfferResult)>, SdkError> {
        let made = std::mem::take(&mut *self.offers.lock().await);
        if made.is_empty() {
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
        let offers = made.iter().map(|m| m.offer.clone()).collect();
        write_frame(&mut send, &PhoneFrame::Offers { offers })
            .await
            .map_err(|e| cut(e.to_string()))?;
        // The TV signs in to each before it answers.
        let answer = step(
            // The person at the TV confirms first, then it signs in.
            APPROVAL + REDEEM + STEP,
            read_frame_within::<_, TvFrame>(&mut recv, MAX_FRAME),
        )
        .await;
        let answer = match answer {
            Ok(answer) => answer,
            Err(e) => {
                conn.close(0u32.into(), b"done");
                return Err(e);
            }
        };
        let mut results = match answer {
            TvFrame::Done { results } => results.into_iter(),
            TvFrame::Refused { reason } => {
                conn.close(0u32.into(), b"done");
                return Err(PairingError::from(reason).into());
            }
            TvFrame::Device { .. } | TvFrame::Unknown => {
                conn.close(0u32.into(), b"done");
                return Err(cut("the TV answered out of turn".into()));
            }
        };
        let done = made
            .into_iter()
            .map(|Made { offer, key }| {
                let outcome = match results.next().map(|r| r.outcome) {
                    Some(OfferOutcome::SignedIn) => SignInOfferOutcome::SignedIn,
                    Some(OfferOutcome::Failed { reason, kind }) => {
                        SignInOfferOutcome::Failed(SignInFailure::from_wire(kind, reason))
                    }
                    Some(OfferOutcome::Declined) => {
                        SignInOfferOutcome::Failed(SignInFailure::Declined)
                    }
                    Some(OfferOutcome::Unknown) => SignInOfferOutcome::Failed(
                        SignInFailure::Refused("the TV's answer is not known here".into()),
                    ),
                    None => SignInOfferOutcome::Failed(SignInFailure::Refused(
                        "the TV did not say".into(),
                    )),
                };
                let result = SignInOfferResult {
                    endpoint_id: offer.server.endpoint_id,
                    server_name: offer.server.name,
                    user_name: offer.user_name,
                    outcome,
                };
                (key, result)
            })
            .collect::<Vec<_>>();
        self.signed_in.lock().extend(
            done.iter()
                .filter(|(_, r)| r.outcome == SignInOfferOutcome::SignedIn)
                .map(|(key, _)| key.clone()),
        );
        let declined = done
            .iter()
            .any(|(_, r)| r.outcome == SignInOfferOutcome::Failed(SignInFailure::Declined));
        let failed = done
            .iter()
            .any(|(_, r)| matches!(r.outcome, SignInOfferOutcome::Failed(_)));
        if (more || failed) && !declined {
            // The TV waits for another round.
            *self.streams.lock().await = Some(Streams { conn, send, recv });
        } else {
            conn.close(0u32.into(), b"done");
        }
        tracing::info!(
            device = %self.device.fmt_short(),
            signed_in = done
                .iter()
                .filter(|(_, r)| matches!(r.outcome, SignInOfferOutcome::SignedIn))
                .count(),
            "signed a TV in"
        );
        Ok(done)
    }

    /// [`SignInOffer::offer`] each of `chosen`, all at once, then
    /// [`SignInOffer::send`]: how each went, in `chosen`'s order. One the
    /// phone could not offer is [`SignInOfferOutcome::NotOffered`] and
    /// stops none of the rest: [`SignInFailure::SessionEnded`] for one
    /// whose session has ended, before or during this call. If none could be offered nothing is sent,
    /// and the TV still waits: confirm it is them where the server asked,
    /// and approve again.
    pub async fn approve(
        &self,
        chosen: &[SignInChoice],
        remember: bool,
    ) -> Result<Vec<SignInOfferResult>, SdkError> {
        // One the TV signed in to in an earlier round is not offered
        // again: it stays signed in, with no second session made.
        let signed: std::collections::HashSet<ProfileKey> = self.signed_in.lock().clone();
        let to_make: Vec<&SignInChoice> = chosen
            .iter()
            .filter(|c| !signed.contains(&Made::key_of(c)))
            .collect();
        let mut not_offered: std::collections::HashMap<ProfileKey, SignInFailure> =
            std::collections::HashMap::new();
        let mut sent = Vec::new();
        if !to_make.is_empty() {
            self.check_open().await?;
            // One whose session has ended is not tried.
            let (ended, live): (Vec<&SignInChoice>, Vec<&SignInChoice>) =
                to_make.iter().copied().partition(|c| c.session_ended());
            for choice in ended {
                not_offered.insert(Made::key_of(choice), SignInFailure::SessionEnded);
            }
            let made = join_all(live.iter().map(|choice| self.make(choice, remember))).await;
            {
                let mut offers = self.offers.lock().await;
                for (choice, made) in live.iter().zip(made) {
                    if let Err(e) = made.and_then(|made| Made::keep(&mut offers, made)) {
                        not_offered.insert(Made::key_of(choice), SignInFailure::of_offer(&e));
                    }
                }
            }
            if not_offered.len() < to_make.len() {
                // One not offered may be offered again, once they confirm.
                sent = self.send_made(!not_offered.is_empty()).await?;
            }
        }
        let results = chosen
            .iter()
            .map(|choice| {
                let key = Made::key_of(choice);
                let outcome = match not_offered.remove(&key) {
                    _ if signed.contains(&key) => SignInOfferOutcome::SignedIn,
                    Some(failure) => SignInOfferOutcome::NotOffered(failure),
                    None => match sent.iter().find(|(k, _)| *k == key) {
                        Some((_, result)) => return result.clone(),
                        None => SignInOfferOutcome::Failed(SignInFailure::Refused(
                            "the TV did not say".into(),
                        )),
                    },
                };
                SignInOfferResult {
                    endpoint_id: choice.server.endpoint_id.clone(),
                    server_name: choice.server.name.clone(),
                    user_name: choice.display_name().to_owned(),
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
        finish_and_wait_closed(&conn, &mut send).await;
        sent.map_err(|e| PairingError::Aborted(e.to_string()).into())
    }
}

#[cfg(test)]
#[path = "phone_sign_in_tests.rs"]
mod tests;
