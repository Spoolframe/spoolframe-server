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
use crate::phone_sign_in::{PhoneSignInOutcome, PhoneSignedIn};
use crate::portable::{DevicePublicKey, PortableUserId};

/// The TV's answer, sent once. Dropped unsent (every holder gone), the
/// waiting connection reads it as a rejection.
#[derive(Debug)]
pub(crate) struct Decision(Mutex<Option<oneshot::Sender<bool>>>);

impl Decision {
    fn decide(&self, confirm: bool) {
        if let Some(answer) = self.0.lock().take() {
            let _ = answer.send(confirm);
        }
    }
}

/// Where a held value's [`Decision`] waits for the app: the first
/// [`held`] takes it from the slot, later ones share it while it lives.
#[derive(Debug, Clone)]
pub(crate) struct Hold {
    slot: Arc<Mutex<Option<Arc<Decision>>>>,
    shared: Weak<Decision>,
}

impl Hold {
    /// A hold, and where its answer comes: `true` to confirm.
    pub(crate) fn new() -> (Self, oneshot::Receiver<bool>) {
        let (answer, answered) = oneshot::channel();
        let decision = Arc::new(Decision(Mutex::new(Some(answer))));
        let hold = Self {
            shared: Arc::downgrade(&decision),
            slot: Arc::new(Mutex::new(Some(decision))),
        };
        (hold, answered)
    }

    fn take(&self) -> Arc<Decision> {
        self.slot
            .lock()
            .take()
            .or_else(|| self.shared.upgrade())
            // Decided and gone: confirm or reject now reads the end.
            .unwrap_or_else(|| Arc::new(Decision(Mutex::new(None))))
    }
}

/// What is held.
#[derive(Debug, Clone)]
pub(crate) enum Held {
    /// A guest's pairing, as it would be installed.
    Guest(Paired),
    /// A phone's offers.
    SignIn(Vec<HeldOffer>),
}

/// What the code's connection holds, once it does, and its decision: or
/// the error the code ended with before.
pub(crate) async fn held(
    progress: &watch::Receiver<Progress>,
) -> Result<(Held, Arc<Decision>), SdkError> {
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

/// The end once decided, for a confirm: its value.
async fn ended(progress: &watch::Receiver<Progress>) -> Result<PhoneSignInOutcome, SdkError> {
    Progress::outcome(progress).await
}

/// The end once decided, for a reject: `Ok` unless it was confirmed
/// first.
async fn rejected(progress: &watch::Receiver<Progress>) -> Result<(), SdkError> {
    match ended(progress).await {
        Ok(_) => Err(SdkError::InvalidInput("it was confirmed already".into())),
        Err(_) => Ok(()),
    }
}

/// A guest's pairing, held until the person at the TV says: the identity
/// the guest's phone authorized this device for, and the servers it
/// handed. Show "Add Steve as a guest?" with the fingerprint, then
/// [`HeldPairing::confirm`] or [`HeldPairing::reject`]. Dropped
/// undecided, it rejects.
pub struct HeldPairing {
    pairing: Paired,
    decision: Arc<Decision>,
    progress: watch::Receiver<Progress>,
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
            decision,
            progress,
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

    /// The servers the guest's phone handed, to be saved on confirm.
    pub fn servers(&self) -> &[PairedServer] {
        &self.pairing.servers
    }

    /// Install the credential, save the servers, and tell the phone it
    /// is done: [`Paired`], with the servers saved. An error if the
    /// pairing ended meanwhile (the phone went, or no answer in time).
    pub async fn confirm(&self) -> Result<Paired, SdkError> {
        self.decision.decide(true);
        match ended(&self.progress).await? {
            PhoneSignInOutcome::Paired(paired) => Ok(paired),
            PhoneSignInOutcome::SignedIn(_) => Err(cancelled()),
        }
    }

    /// Install nothing and tell the phone no
    /// ([`PairingError::NotAllowed`] there). The device key made for the
    /// pairing is removed, as for any pairing that does not end paired.
    pub async fn reject(&self) -> Result<(), SdkError> {
        self.decision.decide(false);
        rejected(&self.progress).await
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
    decision: Arc<Decision>,
    progress: watch::Receiver<Progress>,
}

impl HeldSignIn {
    /// The servers, in the phone's order.
    pub fn offers(&self) -> &[HeldOffer] {
        &self.offers
    }

    /// Save each server and sign in to it, then tell the phone how each
    /// went: [`PhoneSignedIn`]. [`crate::PhoneSignIn::server_sign_ins`]
    /// gives each as it finishes meanwhile.
    pub async fn confirm(&self) -> Result<PhoneSignedIn, SdkError> {
        self.decision.decide(true);
        match ended(&self.progress).await? {
            PhoneSignInOutcome::SignedIn(signed) => Ok(signed),
            PhoneSignInOutcome::Paired(_) => Err(cancelled()),
        }
    }

    /// Sign in to nothing and save no server; the phone is told each was
    /// declined here. The phone's grants run out on their servers unused.
    pub async fn reject(&self) -> Result<(), SdkError> {
        self.decision.decide(false);
        rejected(&self.progress).await
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
            Held::SignIn(offers) => Self::SignIn(HeldSignIn {
                offers,
                decision,
                progress,
            }),
        }
    }
}

/// The error a rejected hold ends the code with, on the TV.
pub(crate) fn rejected_here() -> SdkError {
    PairingError::Rejected.into()
}
