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
        Ok(Connected {
            identity: connected.identity.to_string(),
            identity_fingerprint: connected.identity.fingerprint(),
            device_fingerprint: connected.fingerprint,
        })
    }

    /// How it ended: the guest, or `Pairing { … }` with why.
    pub async fn wait(&self) -> Result<Paired, SpoolError> {
        let pairing = self.0.clone();
        let paired = run(async move { pairing.wait().await }).await?;
        Ok(Paired {
            identity: paired.identity.to_string(),
            fingerprint: paired.identity.fingerprint(),
            servers: paired
                .servers
                .into_iter()
                .map(|s| PairedServer {
                    endpoint_id: s.endpoint_id,
                    name: s.name,
                })
                .collect(),
        })
    }

    /// Stop: the code stops working at once.
    pub fn cancel(&self) {
        self.0.cancel();
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
