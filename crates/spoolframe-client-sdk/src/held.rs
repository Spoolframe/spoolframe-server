//! A TV's say before anything a phone hands it is kept: a guest's
//! pairing ([`HeldPairing`]) and a phone's sign-in ([`HeldSignIn`]) are
//! held once the phone has answered, until the person at the TV confirms
//! or rejects. Nothing is installed, saved or signed in to before
//! [`HeldPairing::confirm`] or [`HeldSignIn::confirm`].
//!
//! A held value dropped undecided rejects, as does no answer within
//! [`crate::pairing::APPROVAL`] of the hold.

use std::sync::{Arc, Weak};

use parking_lot::Mutex;
use spoolframe_protocol::pairing::PairedServer;
use tokio::sync::{oneshot, watch};

use crate::error::SdkError;
use crate::pairing::{Paired, PairingError, Progress, cancelled};
use crate::phone_sign_in::PhoneSignInOutcome;
use crate::portable::{DevicePublicKey, PortableUserId};
use crate::whos_watching::PairedSignIn;

/// The TV's answer: the first [`Decision::decide`] is sent, and dropped
/// undecided (every holder gone) the waiting connection reads it as a
/// rejection, its sender gone.
#[derive(Debug)]
pub(crate) struct Decision(Mutex<Option<oneshot::Sender<bool>>>);

impl Decision {
    fn decide(&self, confirm: bool) {
        if let Some(answer) = self.0.lock().take() {
            let _ = answer.send(confirm);
        }
    }

    /// Whether the app has answered yet.
    fn decided(&self) -> bool {
        self.0.lock().is_none()
    }
}

/// How a hold ended without the app's answer: what
/// [`HeldSignIn::ended`] and [`HeldPairing::ended`] give, so a confirm
/// page can close itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldEnd {
    /// The phone hung up while the TV held what it sent.
    PhoneLeft,
    /// Nobody answered within two minutes: rejected, as with no.
    TimedOut,
    /// The code stopped: cancelled, or its listener dropped.
    CodeEnded,
}

/// Where a held value's [`Decision`] waits for the app, in
/// [`Progress::held`]: kept here until the app first takes it, then only
/// known here, so that the app's copies alone keep it.
#[derive(Debug, Clone)]
pub(crate) struct Hold(Arc<Mutex<Claim>>);

#[derive(Debug)]
enum Claim {
    Kept(Arc<Decision>),
    Taken(Weak<Decision>),
}

impl Hold {
    /// A hold, and where its answer comes: `true` to confirm.
    pub(crate) fn new() -> (Self, oneshot::Receiver<bool>) {
        let (answer, answered) = oneshot::channel();
        let decision = Arc::new(Decision(Mutex::new(Some(answer))));
        (Self(Arc::new(Mutex::new(Claim::Kept(decision)))), answered)
    }

    /// The decision, shared with every earlier take while one lives.
    /// Once every copy is gone it was made (or dropped, a rejection):
    /// a spent one then, whose confirm or reject reads that end.
    pub(crate) fn take(&self) -> Arc<Decision> {
        let mut claim = self.0.lock();
        match &*claim {
            Claim::Kept(decision) => {
                let decision = decision.clone();
                *claim = Claim::Taken(Arc::downgrade(&decision));
                decision
            }
            Claim::Taken(shared) => shared
                .upgrade()
                .unwrap_or_else(|| Arc::new(Decision(Mutex::new(None)))),
        }
    }
}

/// What is held.
#[derive(Debug, Clone)]
pub(crate) enum Held {
    /// A guest's pairing, as it would be installed.
    Guest(Paired),
    /// A phone's offers, and what it calls itself.
    SignIn {
        offers: Vec<HeldOffer>,
        phone_name: Option<String>,
    },
}

/// A held value's decision, and the code's progress to read its end
/// from: what [`HeldPairing`] and [`HeldSignIn`] share.
struct Pending {
    decision: Arc<Decision>,
    progress: watch::Receiver<Progress>,
}

impl Pending {
    /// Confirm, and the end once the TV has acted on it.
    async fn confirm(&self) -> Result<PhoneSignInOutcome, SdkError> {
        self.decision.decide(true);
        Progress::outcome(&self.progress).await
    }

    /// Reject: `Ok` unless it was confirmed first.
    async fn reject(&self) -> Result<(), SdkError> {
        self.decision.decide(false);
        match Progress::outcome(&self.progress).await {
            Ok(_) => Err(SdkError::InvalidInput("it was confirmed already".into())),
            Err(_) => Ok(()),
        }
    }

    /// How the hold ended without the app's answer; never, once the app
    /// answered.
    async fn ended(&self) -> HoldEnd {
        let mut progress = self.progress.clone();
        let end = match progress.wait_for(|p| p.held_end.is_some()).await {
            Ok(p) => p.held_end,
            // The code's task is gone (cancelled, or its listener
            // dropped) with no end recorded: the code ended, unless the
            // app's own answer ended the hold first.
            Err(_) => (!self.decision.decided()).then_some(HoldEnd::CodeEnded),
        };
        match end {
            Some(end) => end,
            None => std::future::pending().await,
        }
    }
}

/// A guest's pairing, held until the person at the TV says: the identity
/// the guest's phone authorized this device for, and the servers it
/// handed. Show "Add Steve as a guest?" with the fingerprint, then
/// [`HeldPairing::confirm`] or [`HeldPairing::reject`]. Dropped
/// undecided, it rejects.
pub struct HeldPairing {
    pairing: Paired,
    pending: Pending,
}

impl std::fmt::Debug for HeldPairing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeldPairing")
            .field("pairing", &self.pairing)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for HeldSignIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeldSignIn")
            .field("offers", &self.offers)
            .finish_non_exhaustive()
    }
}

impl HeldPairing {
    pub(crate) fn new(
        pairing: Paired,
        decision: Arc<Decision>,
        progress: watch::Receiver<Progress>,
    ) -> Self {
        Self {
            pairing,
            pending: Pending { decision, progress },
        }
    }

    pub fn identity(&self) -> PortableUserId {
        self.pairing.identity
    }

    /// This device's key for the identity.
    pub fn device_key(&self) -> DevicePublicKey {
        self.pairing.device_key
    }

    /// The device key's fingerprint, the one the guest's phone showed.
    pub fn fingerprint(&self) -> &str {
        &self.pairing.fingerprint
    }

    /// What the guest calls the identity, as their phone said.
    pub fn name(&self) -> Option<&str> {
        self.pairing.name.as_deref()
    }

    /// What the guest's phone calls itself ("Steve's iPhone"), as it
    /// said; `None` from a phone before it said.
    pub fn phone_name(&self) -> Option<&str> {
        self.pairing.phone_name.as_deref()
    }

    /// The servers the guest's phone handed, to be saved on confirm.
    pub fn servers(&self) -> &[PairedServer] {
        &self.pairing.servers
    }

    /// Install the credential, save the servers, and tell the phone it
    /// is done: [`Paired`], with the servers saved. An error if the
    /// pairing ended meanwhile (the phone went, or no answer in time).
    pub async fn confirm(&self) -> Result<Paired, SdkError> {
        match self.pending.confirm().await? {
            PhoneSignInOutcome::Paired(paired) => Ok(paired),
            PhoneSignInOutcome::SignedIn(_) => Err(cancelled()),
        }
    }

    /// Install nothing and tell the phone no
    /// ([`PairingError::NotAllowed`] there). The device key made for the
    /// pairing is removed, as for any pairing that does not end paired.
    pub async fn reject(&self) -> Result<(), SdkError> {
        self.pending.reject().await
    }
    /// Once the hold is over without this app's answer: the phone left,
    /// nobody answered in time, or the code stopped. Close the confirm
    /// page then. Never resolves once the app confirmed or rejected, so
    /// race it against the page's own answer.
    pub async fn ended(&self) -> HoldEnd {
        self.pending.ended().await
    }
}

/// A server a phone offers to sign the TV in to, held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldOffer {
    /// The server as the phone has it saved; saved here on confirm.
    pub server: PairedServer,
    /// Who the TV signs in as there: the profile's display name, as the
    /// phone said.
    pub user_name: String,
}

/// A phone's sign-in, held until the person at the TV says: who is
/// signing it in, and to which servers. Show them, then
/// [`HeldSignIn::confirm`] or [`HeldSignIn::reject`]. Dropped undecided,
/// it rejects.
pub struct HeldSignIn {
    offers: Vec<HeldOffer>,
    phone_name: Option<String>,
    pending: Pending,
}

impl HeldSignIn {
    /// What the phone signing the TV in calls itself ("Andrey's iPhone
    /// wants to sign this TV in"), as it said; `None` from a phone before
    /// it said.
    pub fn phone_name(&self) -> Option<&str> {
        self.phone_name.as_deref()
    }

    /// The servers, in the phone's order.
    pub fn offers(&self) -> &[HeldOffer] {
        &self.offers
    }

    /// Save each server and sign in to it, then tell the phone how each
    /// went: [`PairedSignIn`]. Each server is saved, pinned but not
    /// verified (the phone vouched for it, not this device's owner),
    /// whether or not its sign-in went through; one saved here already
    /// is left as it was. [`crate::PhoneSignIn::server_sign_ins`]
    /// gives each as it finishes meanwhile, and any the phone offers
    /// after (a server it retries), which are not held again.
    pub async fn confirm(&self) -> Result<PairedSignIn, SdkError> {
        match self.pending.confirm().await? {
            PhoneSignInOutcome::SignedIn(signed) => Ok(signed),
            PhoneSignInOutcome::Paired(_) => Err(cancelled()),
        }
    }

    /// Sign in to nothing and save no server; the phone is told each was
    /// declined here. The phone's grants run out on their servers unused.
    pub async fn reject(&self) -> Result<(), SdkError> {
        self.pending.reject().await
    }
    /// Once the hold is over without this app's answer: the phone left,
    /// nobody answered in time, or the code stopped. Close the confirm
    /// page then. Never resolves once the app confirmed or rejected, so
    /// race it against the page's own answer.
    pub async fn ended(&self) -> HoldEnd {
        self.pending.ended().await
    }
}

/// What a TV's code holds once a phone has answered
/// ([`crate::PhoneSignIn::wait`]).
#[derive(Debug)]
pub enum HeldPhoneSignIn {
    /// A phone signing this TV in to its accounts.
    SignIn(HeldSignIn),
    /// A guest's phone pairing its own identity (Allow guests is on).
    Guest(HeldPairing),
}

impl HeldPhoneSignIn {
    pub(crate) fn new(
        held: Held,
        decision: Arc<Decision>,
        progress: watch::Receiver<Progress>,
    ) -> Self {
        match held {
            Held::Guest(pairing) => Self::Guest(HeldPairing::new(pairing, decision, progress)),
            Held::SignIn { offers, phone_name } => Self::SignIn(HeldSignIn {
                offers,
                phone_name,
                pending: Pending { decision, progress },
            }),
        }
    }
}

/// The error a rejected hold ends the code with, on the TV.
pub(crate) fn rejected_here() -> SdkError {
    PairingError::Rejected.into()
}
