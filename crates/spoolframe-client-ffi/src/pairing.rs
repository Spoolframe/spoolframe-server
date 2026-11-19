//! Pairing a guest's identity onto a device by QR code: the device
//! shows a code ([`SpoolPairing`]), the guest's phone scans it and
//! authorizes the device ([`SpoolPairingOffer`]). Device to device on the
//! LAN; no server.

use std::sync::Arc;

use spoolframe_client_sdk as sdk;

use crate::error::SpoolError;
use crate::runtime::run;

/// Who a pairing put on this device.
#[derive(Debug, Clone, uniffi::Record)]
pub struct Paired {
    /// The guest's identity, hex.
    pub identity: String,
    pub fingerprint: String,
    /// What the guest calls the identity, as their phone said.
    pub name: Option<String>,
    /// The servers the guest's phone signs in to as it, saved here now
    /// (pinned, not verified): `sign_in_portable` on each.
    pub servers: Vec<PairedServer>,
}

/// A server a pairing brought.
#[derive(Debug, Clone, uniffi::Record)]
pub struct PairedServer {
    pub endpoint_id: String,
    pub name: String,
}

/// A phone that reached this device's code, before it answers.
#[derive(Debug, Clone, uniffi::Record)]
pub struct Connected {
    /// The identity it would put here, hex.
    pub identity: String,
    pub identity_fingerprint: String,
    /// This device's key, as the phone shows it.
    pub device_fingerprint: String,
    /// What the guest calls the identity ("Steve"), as their phone said:
    /// "Check this matches Steve's phone". None from an older phone.
    pub name: Option<String>,
}

impl From<sdk::Paired> for Paired {
    fn from(paired: sdk::Paired) -> Self {
        Self {
            identity: paired.identity.to_string(),
            fingerprint: paired.identity.fingerprint(),
            name: paired.name,
            servers: paired
                .servers
                .into_iter()
                .map(|s| PairedServer {
                    endpoint_id: s.endpoint_id,
                    name: s.name,
                })
                .collect(),
        }
    }
}

impl From<sdk::Connected> for Connected {
    fn from(connected: sdk::Connected) -> Self {
        Self {
            identity: connected.identity.to_string(),
            identity_fingerprint: connected.identity.fingerprint(),
            device_fingerprint: connected.fingerprint,
            name: connected.name,
        }
    }
}

/// A pairing code this device shows. Show `link()` as a QR code, never in
/// a log: it carries the one-time code.
#[derive(uniffi::Object)]
pub struct SpoolPairing(pub(crate) Arc<sdk::Pairing>);

#[uniffi::export]
impl SpoolPairing {
    /// What the QR code carries.
    pub fn link(&self) -> String {
        self.0.link().to_owned()
    }

    /// Seconds the code is still good for.
    pub fn expires_in_secs(&self) -> u64 {
        self.0.expires_in().as_secs()
    }

    /// A phone has connected, before it answers: the identity it offers
    /// and this device's key fingerprint, the one the phone shows. The
    /// pairing's end (`Pairing { … }`) when none connects.
    pub async fn connected(&self) -> Result<Connected, SpoolError> {
        let pairing = self.0.clone();
        let connected = run(async move { pairing.connected().await }).await?;
        Ok(connected.into())
    }

    /// The pairing the guest's phone authorized, held until this device
    /// confirms; or `Pairing { … }` with why it ended before.
    pub async fn wait(&self) -> Result<Arc<SpoolHeldPairing>, SpoolError> {
        let pairing = self.0.clone();
        let held = run(async move { pairing.wait().await }).await?;
        Ok(Arc::new(SpoolHeldPairing(Arc::new(held))))
    }

    /// Stop: the code stops working at once.
    pub fn cancel(&self) {
        self.0.cancel();
    }
}

/// A guest's pairing held until the person at this device says: nothing
/// is installed or saved before `confirm`. Released undecided, or left
/// two minutes, it is rejected.
#[derive(uniffi::Object)]
pub struct SpoolHeldPairing(pub(crate) Arc<sdk::HeldPairing>);

#[uniffi::export]
impl SpoolHeldPairing {
    /// The guest's identity, hex.
    pub fn identity(&self) -> String {
        self.0.identity().to_string()
    }

    pub fn identity_fingerprint(&self) -> String {
        self.0.identity().fingerprint()
    }

    /// This device's key, as the guest's phone showed it.
    pub fn device_fingerprint(&self) -> String {
        self.0.fingerprint().to_owned()
    }

    /// What the guest calls the identity, as their phone said.
    pub fn name(&self) -> Option<String> {
        self.0.name().map(str::to_owned)
    }

    /// The servers the guest's phone handed, saved on confirm.
    pub fn servers(&self) -> Vec<PairedServer> {
        self.0
            .servers()
            .iter()
            .map(|s| PairedServer {
                endpoint_id: s.endpoint_id.clone(),
                name: s.name.clone(),
            })
            .collect()
    }

    /// Install it and save the servers: the guest.
    pub async fn confirm(&self) -> Result<Paired, SpoolError> {
        let held = self.0.clone();
        Ok(run(async move { held.confirm().await }).await?.into())
    }

    /// Install nothing; the phone is told no.
    pub async fn reject(&self) -> Result<(), SpoolError> {
        let held = self.0.clone();
        Ok(run(async move { held.reject().await }).await?)
    }
}

/// A device the phone reached from its code, waiting for a yes or no.
#[derive(uniffi::Object)]
pub struct SpoolPairingOffer(pub(crate) Arc<sdk::PairingOffer>);

#[uniffi::export]
impl SpoolPairingOffer {
    /// What the device calls itself.
    pub fn device_name(&self) -> String {
        self.0.device_name().to_owned()
    }

    /// The device key's fingerprint, to compare with what the device shows.
    pub fn fingerprint(&self) -> String {
        self.0.fingerprint()
    }

    /// The identity it would act as, hex.
    pub fn identity(&self) -> String {
        self.0.identity().to_string()
    }

    /// Authorize it, named `label` in the identity's devices (the
    /// device's own name when none).
    pub async fn authorize(&self, label: Option<String>) -> Result<(), SpoolError> {
        let offer = self.0.clone();
        Ok(run(async move { offer.authorize(label.as_deref()).await }).await?)
    }

    pub async fn decline(&self) -> Result<(), SpoolError> {
        let offer = self.0.clone();
        Ok(run(async move { offer.decline().await }).await?)
    }
}
