//! Signing a TV in with a phone by QR code: the TV shows a code
//! ([`SpoolPhoneSignIn`]), the phone scans it, the person ticks the
//! servers, and the TV signs in to each as the phone's profile there
//! ([`SpoolSignInOffer`]). The same code pairs a guest's own identity
//! when the TV allows guests.

use std::sync::Arc;

use spoolframe_client_sdk::{self as sdk, ErrorKind, types::Platform};

use crate::client::SpoolClient;
use crate::error::SpoolError;
use crate::pairing::{Connected, SpoolHeldPairing};
use crate::runtime::run;
use crate::session::SpoolSession;
use crate::sign_in::{ServerFailure, ServerSignInWatch};

/// A phone that reached the TV's code, before it is done.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum PhoneConnected {
    /// A phone signing the TV in: "Continue on your phone".
    SigningIn,
    /// A guest's phone pairing its own identity: show its fingerprint.
    Guest { connected: Connected },
}

/// What a phone signed this TV in to. Each server is saved here, pinned
/// but not verified.
#[derive(uniffi::Record)]
pub struct PhoneSignedIn {
    /// In the phone's order: open the first.
    pub sessions: Vec<Arc<SpoolSession>>,
    pub failed: Vec<ServerFailure>,
}

/// A server a phone offers to sign this TV in to, held.
#[derive(Debug, Clone, uniffi::Record)]
pub struct HeldOffer {
    pub endpoint_id: String,
    pub server_name: String,
    /// Who the TV signs in as there.
    pub user_name: String,
}

/// A phone's sign-in held until the person at the TV says: who signs it
/// in, to which servers. Nothing is saved or signed in to before
/// `confirm`; released undecided, or left two minutes, it is rejected.
#[derive(uniffi::Object)]
pub struct SpoolHeldSignIn(Arc<sdk::HeldSignIn>);

#[uniffi::export]
impl SpoolHeldSignIn {
    /// The servers, in the phone's order.
    pub fn offers(&self) -> Vec<HeldOffer> {
        self.0
            .offers()
            .iter()
            .map(|o| HeldOffer {
                endpoint_id: o.server.endpoint_id.clone(),
                server_name: o.server.name.clone(),
                user_name: o.user_name.clone(),
            })
            .collect()
    }

    /// Save each server and sign in to it.
    pub async fn confirm(&self) -> Result<PhoneSignedIn, SpoolError> {
        let held = self.0.clone();
        let signed = run(async move { held.confirm().await }).await?;
        Ok(PhoneSignedIn {
            sessions: signed
                .sessions
                .into_iter()
                .map(|s| Arc::new(SpoolSession::new(s)))
                .collect(),
            failed: signed
                .failed
                .into_iter()
                .map(|(server, error)| ServerFailure::new(server, error))
                .collect(),
        })
    }

    /// Sign in to nothing; the phone is told each was declined.
    pub async fn reject(&self) -> Result<(), SpoolError> {
        let held = self.0.clone();
        Ok(run(async move { held.reject().await }).await?)
    }
}

/// What the TV's code holds once a phone has answered.
#[derive(uniffi::Enum)]
pub enum HeldPhoneSignIn {
    /// A phone signing this TV in to its accounts.
    SignIn { held: Arc<SpoolHeldSignIn> },
    /// A guest pairing its own identity (Allow guests is on).
    Guest { held: Arc<SpoolHeldPairing> },
}

/// A code this TV shows for a phone to sign it in. Show `link()` as a QR
/// code, never in a log: it carries the one-time code.
#[derive(uniffi::Object)]
pub struct SpoolPhoneSignIn(Arc<sdk::PhoneSignIn>);

#[uniffi::export]
impl SpoolPhoneSignIn {
    /// What the QR code carries.
    pub fn link(&self) -> String {
        self.0.link().to_owned()
    }

    /// Seconds the code is still good for.
    pub fn expires_in_secs(&self) -> u64 {
        self.0.expires_in().as_secs()
    }

    /// A phone has presented the code, before it is done. The code's end
    /// (`Pairing { … }`) when none does.
    pub async fn connected(&self) -> Result<PhoneConnected, SpoolError> {
        let shown = self.0.clone();
        Ok(match run(async move { shown.connected().await }).await? {
            sdk::PhoneConnected::SigningIn => PhoneConnected::SigningIn,
            sdk::PhoneConnected::Guest(connected) => PhoneConnected::Guest {
                connected: connected.into(),
            },
        })
    }

    /// What the phone handed, held until this TV confirms; or
    /// `Pairing { … }` with why it ended before.
    pub async fn wait(&self) -> Result<HeldPhoneSignIn, SpoolError> {
        let shown = self.0.clone();
        Ok(match run(async move { shown.wait().await }).await? {
            sdk::HeldPhoneSignIn::SignIn(held) => HeldPhoneSignIn::SignIn {
                held: Arc::new(SpoolHeldSignIn(Arc::new(held))),
            },
            sdk::HeldPhoneSignIn::Guest(held) => HeldPhoneSignIn::Guest {
                held: Arc::new(SpoolHeldPairing(Arc::new(held))),
            },
        })
    }

    /// Each server a phone signs this TV in to, as it finishes; its
    /// total once the phone's offers come.
    pub fn server_sign_ins(&self) -> Arc<ServerSignInWatch> {
        ServerSignInWatch::new(self.0.server_sign_ins())
    }

    /// Stop: the code stops working at once.
    pub fn cancel(&self) {
        self.0.cancel();
    }
}

/// A profile the phone can sign the TV in as: one session on one server.
#[derive(uniffi::Object)]
pub struct SpoolSignInChoice(sdk::SignInChoice);

#[uniffi::export]
impl SpoolSignInChoice {
    /// The server's key, hex.
    pub fn endpoint_id(&self) -> String {
        self.0.server.endpoint_id.clone()
    }

    pub fn server_name(&self) -> String {
        self.0.server.name.clone()
    }

    /// The session that signs the TV in: "Confirm it's you" runs on it.
    pub fn session(&self) -> Arc<SpoolSession> {
        Arc::new(SpoolSession::new(self.0.session.clone()))
    }
}

/// Whether the TV is signed in to one server.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum SignInOfferOutcome {
    SignedIn,
    /// The phone could not make the sign-in: on `RecentSignInRequired`,
    /// "Confirm it's you" on the choice's session, then `offer` again.
    NotOffered {
        kind: ErrorKind,
        message: String,
    },
    /// The TV could not redeem it, as the TV said.
    Failed {
        reason: String,
    },
    /// The person at the TV turned it down.
    Declined,
}

/// How one server's sign-in went.
#[derive(Debug, Clone, uniffi::Record)]
pub struct SignInOfferResult {
    pub endpoint_id: String,
    pub server_name: String,
    /// Who the TV signs in as there.
    pub user_name: String,
    pub outcome: SignInOfferOutcome,
}

impl From<sdk::SignInOfferResult> for SignInOfferResult {
    fn from(result: sdk::SignInOfferResult) -> Self {
        Self {
            endpoint_id: result.endpoint_id,
            server_name: result.server_name,
            user_name: result.user_name,
            outcome: match result.outcome {
                sdk::SignInOfferOutcome::SignedIn => SignInOfferOutcome::SignedIn,
                sdk::SignInOfferOutcome::NotOffered(error) => {
                    let error = SpoolError::from(error);
                    SignInOfferOutcome::NotOffered {
                        kind: error.kind(),
                        message: error.to_string(),
                    }
                }
                sdk::SignInOfferOutcome::Failed(reason) => SignInOfferOutcome::Failed { reason },
                sdk::SignInOfferOutcome::Declined => SignInOfferOutcome::Declined,
            },
        }
    }
}

/// A TV that answered a scanned code: what it is, the profiles to tick,
/// then `approve` or `decline`.
#[derive(uniffi::Object)]
pub struct SpoolSignInOffer(Arc<sdk::SignInOffer>);

#[uniffi::export]
impl SpoolSignInOffer {
    /// What the TV calls itself.
    pub fn device_name(&self) -> String {
        self.0.device_name().to_owned()
    }

    pub fn platform(&self) -> Platform {
        self.0.platform()
    }

    pub fn client_name(&self) -> String {
        self.0.client_name().to_owned()
    }

    /// The profiles this phone can sign the TV in as, one per session.
    pub async fn servers(&self) -> Result<Vec<Arc<SpoolSignInChoice>>, SpoolError> {
        let offer = self.0.clone();
        let choices = run(async move { offer.servers().await }).await?;
        Ok(choices
            .into_iter()
            .map(|c| Arc::new(SpoolSignInChoice(c)))
            .collect())
    }

    /// Make `choice`'s sign-in now; nothing goes to the TV until `send`.
    pub async fn offer(
        &self,
        choice: Arc<SpoolSignInChoice>,
        remember: bool,
    ) -> Result<(), SpoolError> {
        let offer = self.0.clone();
        Ok(run(async move { offer.offer(&choice.0, remember).await }).await?)
    }

    /// Hand the TV every sign-in made, and how each went.
    pub async fn send(&self) -> Result<Vec<SignInOfferResult>, SpoolError> {
        let offer = self.0.clone();
        let results = run(async move { offer.send().await }).await?;
        Ok(results.into_iter().map(Into::into).collect())
    }

    /// `offer` each of `chosen`, then `send`; one refused stops none of
    /// the rest, and with none offered nothing is sent.
    pub async fn approve(
        &self,
        chosen: Vec<Arc<SpoolSignInChoice>>,
        remember: bool,
    ) -> Result<Vec<SignInOfferResult>, SpoolError> {
        let offer = self.0.clone();
        let chosen: Vec<sdk::SignInChoice> = chosen.iter().map(|c| c.0.clone()).collect();
        let results = run(async move { offer.approve(&chosen, remember).await }).await?;
        Ok(results.into_iter().map(Into::into).collect())
    }

    pub async fn decline(&self) -> Result<(), SpoolError> {
        let offer = self.0.clone();
        Ok(run(async move { offer.decline().await }).await?)
    }
}

#[uniffi::export]
impl SpoolClient {
    /// Show a code on this TV, called `name`, for a phone to sign it in
    /// to its servers; with `relay`, a phone elsewhere may reach it
    /// through this device's relay. Works with guests off; with them on
    /// the same code pairs a guest. One code at a time.
    pub async fn start_phone_sign_in(
        &self,
        name: String,
        relay: bool,
    ) -> Result<Arc<SpoolPhoneSignIn>, SpoolError> {
        let client = self.client.clone();
        let shown = run(async move { client.start_phone_sign_in(&name, relay).await }).await?;
        Ok(Arc::new(SpoolPhoneSignIn(Arc::new(shown))))
    }

    /// Reach the TV whose code `link` is, to sign it in.
    pub async fn scan_phone_sign_in(
        &self,
        link: String,
    ) -> Result<Arc<SpoolSignInOffer>, SpoolError> {
        let client = self.client.clone();
        let offer = run(async move { client.scan_phone_sign_in(&link).await }).await?;
        Ok(Arc::new(SpoolSignInOffer(Arc::new(offer))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(outcome: sdk::SignInOfferOutcome) -> SignInOfferResult {
        sdk::SignInOfferResult {
            endpoint_id: "ab".repeat(32),
            server_name: "Home".into(),
            user_name: "Alice".into(),
            outcome,
        }
        .into()
    }

    /// A server not signed in to comes with its key and why.
    #[test]
    fn a_failed_server_sign_in_names_its_server() {
        let done: crate::sign_in::ServerSignIn = sdk::ServerSignIn {
            server: "ab".repeat(32),
            result: Err(sdk::SdkError::RecentSignInRequired),
        }
        .into();
        assert!(matches!(
            done,
            crate::sign_in::ServerSignIn::Failed { failure }
                if failure.server == "ab".repeat(32)
                    && failure.kind == ErrorKind::NeedsRecentSignIn
        ));
    }

    /// A server that wants "Confirm it's you" says so by its kind, which
    /// the app checks before it confirms and offers again.
    #[test]
    fn a_refused_offer_says_its_kind() {
        let refused = result(sdk::SignInOfferOutcome::NotOffered(
            sdk::SdkError::RecentSignInRequired,
        ));
        assert_eq!(refused.user_name, "Alice");
        assert!(matches!(
            refused.outcome,
            SignInOfferOutcome::NotOffered {
                kind: ErrorKind::NeedsRecentSignIn,
                ..
            }
        ));
        assert!(matches!(
            result(sdk::SignInOfferOutcome::Failed("unreachable".into())).outcome,
            SignInOfferOutcome::Failed { reason } if reason == "unreachable"
        ));
        assert!(matches!(
            result(sdk::SignInOfferOutcome::SignedIn).outcome,
            SignInOfferOutcome::SignedIn
        ));
    }
}
