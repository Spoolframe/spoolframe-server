//! Pairing through a server (P7-08), for a root terminal with no camera:
//! the device holds its request on a server both reach, under a short
//! code it shows; the root terminal (a Linux desktop with the identity's
//! security key) types the code, fetches the request, signs a credential
//! and posts it back; the device collects it, shows whose identity it is,
//! and keeps it. The server only carries signed data.
//!
//! Device:
//!
//! 1. [`Client::offer_relay_pairing`]: show [`RelayPairing::code`] (and
//!    [`RelayPairing::fingerprint`], for the terminal to compare).
//! 2. [`Client::collect_relay_pairing`] every few seconds, until it gives
//!    a [`RelayAnswer`] (or the code expires).
//! 3. Show [`RelayAnswer::identity_fingerprint`]; on "yes",
//!    [`Client::accept_relay_pairing`].
//!
//! Root terminal: [`Client::fetch_relay_pairing`] with the code typed;
//! show the device's name and [`RelayOffer::fingerprint`]; on "yes",
//! [`Client::answer_relay_pairing`] (one touch on the key).

use std::time::{Duration, Instant};

use iroh::EndpointId;
use spoolframe_protocol::pairing::{
    PairedServer, PairingAnswer, PairingOffer, RELAY_CODE_LEN, RELAY_TTL_SECS, RelayCode,
};
use spoolframe_protocol::{Reply, Request, Response};

use crate::SdkError;
use crate::client::{Client, parse_id};
use crate::pairing::Paired;
use crate::portable::{
    DevicePublicKey, IdentityRecord, PortableIdentityState, PortableUserId, RootSigner,
    SecurityKey, SecurityKeyError, UserDeviceCredential,
};
use spoolframe_protocol::portable::{HANDLE_LEN, PORTABLE_RP, root_set};

/// What [`Client::recover_with_key`] has the key sign: this domain, then
/// 32 random bytes. It proves the key holds a credential now; nothing
/// keeps or forwards it.
const RECOVER_DOMAIN: &[u8] = b"spoolframe recover v1\0";

/// A pairing request this device holds on a server (P7-08).
#[derive(Debug, Clone)]
pub struct RelayPairing {
    server: EndpointId,
    code: RelayCode,
    device_key: DevicePublicKey,
    expires_at: Instant,
}

impl RelayPairing {
    /// What the device shows, for the person to type on the root
    /// terminal: `ABCDE-FGHJK`.
    pub fn code(&self) -> String {
        self.code.display()
    }

    /// The device key's fingerprint, which the root terminal shows too.
    pub fn fingerprint(&self) -> String {
        self.device_key.fingerprint()
    }

    /// How long the code is still good for.
    pub fn expires_in(&self) -> Duration {
        self.expires_at.saturating_duration_since(Instant::now())
    }
}

/// A pairing request a root terminal fetched (P7-08).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayOffer {
    pub server: String,
    pub offer: PairingOffer,
}

impl RelayOffer {
    /// The device's name, as it gave it.
    pub fn name(&self) -> &str {
        &self.offer.name
    }

    /// The device key's fingerprint, which the device shows too.
    pub fn fingerprint(&self) -> String {
        self.offer.device_key.fingerprint()
    }
}

/// The root terminal's answer to this device's request, checked and not
/// yet kept (P7-08): show whose identity it is, then accept or drop it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayAnswer {
    pub identity: PortableUserId,
    credential: UserDeviceCredential,
    state: Option<PortableIdentityState>,
    servers: Vec<PairedServer>,
}

impl RelayAnswer {
    /// The identity's fingerprint: the person checks it is theirs before
    /// [`Client::accept_relay_pairing`].
    pub fn identity_fingerprint(&self) -> String {
        self.identity.fingerprint()
    }
}

impl Client {
    /// What server `endpoint_id` keeps of identity `user` (P7-08): its
    /// genesis and newest state, for recovering it with one of its keys
    /// ([`crate::Portable::import_record`]). [`SdkError::NotFound`] where
    /// the identity is not known.
    pub async fn fetch_identity_record(
        &self,
        endpoint_id: &str,
        user: PortableUserId,
    ) -> Result<IdentityRecord, SdkError> {
        let server = parse_id(endpoint_id)?;
        match self
            .call(server, None, Request::IdentityRecord { user })
            .await?
        {
            Response::Ok(Reply::IdentityRecord(record)) if record.user == user => Ok(record),
            Response::Ok(other) => Err(SdkError::Protocol(format!(
                "IdentityRecord answered {other:?}"
            ))),
            Response::Err(e) => Err(SdkError::from_server(e)),
        }
    }

    /// What server `endpoint_id` keeps of the identity whose genesis
    /// handle is `handle` (P7-08): the user handle a security key gives
    /// back for its credential, on a fresh install that knows nothing
    /// else. A key added to a phrase identity carries the identity's id as
    /// its handle, and that finds it too. The record is checked to be the
    /// one the handle names. [`SdkError::NotFound`] where no identity
    /// known there has it; a server before this answers
    /// [`SdkError::Unavailable`] or a protocol error, as for any request it
    /// does not know.
    pub async fn fetch_identity_record_by_handle(
        &self,
        endpoint_id: &str,
        handle: [u8; HANDLE_LEN],
    ) -> Result<IdentityRecord, SdkError> {
        let server = parse_id(endpoint_id)?;
        match self
            .call(server, None, Request::IdentityRecordByHandle { handle })
            .await?
        {
            Response::Ok(Reply::IdentityRecord(record)) if names(&record, &handle) => Ok(record),
            Response::Ok(other) => Err(SdkError::Protocol(format!(
                "IdentityRecordByHandle answered {other:?}"
            ))),
            Response::Err(e) => Err(SdkError::from_server(e)),
        }
    }

    /// Recover an identity on this device with only `key`, one of its
    /// roots, from server `endpoint_id`, which knows it (P7-08), as
    /// `name`. The key is asked once with no allow list (a touch and its
    /// PIN): each credential it holds under [`PORTABLE_RP`] names its
    /// identity by handle. The first whose record the server has, and
    /// among whose roots now the credential is, is imported
    /// ([`crate::Portable::import_record`]); its id is returned. Then
    /// [`crate::Portable::pair_self`] with the same key gives this device
    /// a credential.
    ///
    /// [`SdkError::NotARoot`] if the key's credential names an identity the
    /// server knows but is not one of its roots now (it was removed);
    /// [`SdkError::NotFound`] if the server knows none of the identities
    /// the key names; [`SecurityKeyError::NoCredential`] if the key holds
    /// no credential under [`PORTABLE_RP`] at all.
    pub async fn recover_with_key(
        &self,
        endpoint_id: &str,
        key: &dyn SecurityKey,
        name: &str,
    ) -> Result<PortableUserId, SdkError> {
        let mut message = RECOVER_DOMAIN.to_vec();
        let mut nonce = [0u8; 32];
        getrandom::fill(&mut nonce).expect("the OS gives random bytes");
        message.extend_from_slice(&nonce);
        let hash: [u8; 32] = <sha2::Sha256 as sha2::Digest>::digest(&message).into();
        let answers = key.assert(PORTABLE_RP, hash, &[]).await?;
        let mut outcome = Err(SdkError::NotFound("portable identity".into()));
        for answer in answers {
            let Some(handle) = answer.user_handle else {
                continue;
            };
            let record = match self
                .fetch_identity_record_by_handle(endpoint_id, handle)
                .await
            {
                Ok(record) => record,
                Err(SdkError::NotFound(what)) => {
                    if !matches!(outcome, Err(SdkError::NotARoot)) {
                        outcome = Err(SdkError::NotFound(what));
                    }
                    continue;
                }
                Err(e) => return Err(e),
            };
            let roots = root_set(
                &record.user,
                record.genesis.as_ref(),
                record.state.as_slice(),
            );
            let Some(root) = roots.members.iter().find(|r| {
                matches!(r, spoolframe_protocol::portable::RootKey::SecurityKey { credential_id, .. }
                    if *credential_id == answer.credential_id)
            }) else {
                outcome = Err(SdkError::NotARoot);
                continue;
            };
            if !root.verify(&message, &answer.signature) {
                return Err(SecurityKeyError::BadAssertion.into());
            }
            let id = self.portable().import_record(record, name).await?;
            tracing::info!(%id, "recovered a portable identity with one of its keys");
            return Ok(id);
        }
        outcome
    }

    /// Device: hold a pairing request on server `endpoint_id` (one this
    /// device can reach: saved, or heard on the LAN), under a new code,
    /// as `name`. The device key is made now, before the identity is
    /// known, and becomes the identity's when the answer is accepted.
    pub async fn offer_relay_pairing(
        &self,
        endpoint_id: &str,
        name: &str,
    ) -> Result<RelayPairing, SdkError> {
        let server = parse_id(endpoint_id)?;
        if !self.portable().allow_guests().await? {
            return Err(crate::pairing::PairingError::NotAllowed.into());
        }
        let device_key = self.portable().pending_device_key().await?;
        let mut random = [0u8; RELAY_CODE_LEN];
        getrandom::fill(&mut random).expect("the OS gives random bytes");
        let code = RelayCode::from_random(random);
        let offer = PairingOffer {
            code: code.clone(),
            device_key,
            name: name.to_owned(),
        };
        expect_done(
            self.call(server, None, Request::OfferPairing(offer))
                .await?,
        )?;
        Ok(RelayPairing {
            server,
            code,
            device_key,
            expires_at: Instant::now() + Duration::from_secs(RELAY_TTL_SECS),
        })
    }

    /// Device: the answer to `pairing`, if one came: checked (a
    /// credential for this device's key that holds, signed by one of its
    /// identity's roots, with a state of that identity) but not kept.
    /// `None` while there is none yet; ask again in a few seconds.
    pub async fn collect_relay_pairing(
        &self,
        pairing: &RelayPairing,
    ) -> Result<Option<RelayAnswer>, SdkError> {
        let answer = match self
            .call(
                pairing.server,
                None,
                Request::CollectPairing {
                    code: pairing.code.clone(),
                },
            )
            .await?
        {
            Response::Ok(Reply::PairingAnswered { answer }) => answer,
            Response::Ok(other) => {
                return Err(SdkError::Protocol(format!(
                    "CollectPairing answered {other:?}"
                )));
            }
            Response::Err(e) => return Err(SdkError::from_server(e)),
        };
        let Some(PairingAnswer {
            credential,
            state,
            servers,
        }) = answer
        else {
            return Ok(None);
        };
        let credential = *credential;
        if credential.device_key != pairing.device_key {
            return Err(SdkError::NotThisDevice);
        }
        credential.verify(crate::servers::now() as i64)?;
        let state = state.map(|s| *s);
        if let Some(state) = &state {
            state.verify()?;
            if state.user != credential.user {
                return Err(SdkError::UnknownIdentity(state.user.to_string()));
            }
        }
        if let Some(genesis) = credential.genesis.as_deref() {
            genesis
                .verify()
                .map_err(|e| SdkError::InvalidInput(e.to_string()))?;
            if genesis.user() != credential.user {
                return Err(SdkError::InvalidInput(
                    "the credential's genesis is another identity's".into(),
                ));
            }
        }
        let roots = spoolframe_protocol::portable::root_set(
            &credential.user,
            credential.genesis.as_deref(),
            state.as_slice(),
        );
        if !roots.holds(&credential.signer()) {
            // The answer, not a key in hand: not NotARoot.
            return Err(spoolframe_protocol::portable::CredentialError::BadSignature.into());
        }
        Ok(Some(RelayAnswer {
            identity: credential.user,
            credential,
            state,
            servers,
        }))
    }

    /// Device: keep `answer`, once the person said it is their identity:
    /// the pending key becomes this device's key for it, the credential
    /// and state are installed, and the servers saved as a pairing saves
    /// them ([`Paired::servers`]).
    pub async fn accept_relay_pairing(&self, answer: RelayAnswer) -> Result<Paired, SdkError> {
        let portable = self.portable();
        let identity = answer.identity;
        let device_key = answer.credential.device_key;
        portable
            .adopt_pending_device_key(identity, &device_key)
            .await?;
        portable
            .install_with_state(answer.credential, answer.state)
            .await?;
        let servers = self.save_paired_servers(answer.servers).await?;
        tracing::info!(%identity, %device_key, "paired through a server");
        Ok(Paired {
            identity,
            device_key,
            fingerprint: device_key.fingerprint(),
            servers,
        })
    }

    /// Root terminal: the request held under `code` (as the person typed
    /// it) on server `endpoint_id`. Show its name and fingerprint before
    /// answering.
    pub async fn fetch_relay_pairing(
        &self,
        endpoint_id: &str,
        code: &str,
    ) -> Result<RelayOffer, SdkError> {
        let server = parse_id(endpoint_id)?;
        let code = RelayCode::parse(code).ok_or_else(|| {
            SdkError::InvalidInput("a pairing code is 10 letters and digits".into())
        })?;
        match self
            .call(server, None, Request::FetchPairing { code: code.clone() })
            .await?
        {
            Response::Ok(Reply::PairingOffered(offer)) if offer.code == code => Ok(RelayOffer {
                server: endpoint_id.to_owned(),
                offer,
            }),
            Response::Ok(other) => Err(SdkError::Protocol(format!(
                "FetchPairing answered {other:?}"
            ))),
            Response::Err(e) => Err(SdkError::from_server(e)),
        }
    }

    /// Root terminal: answer `offer` for `identity`: a credential for its
    /// key from `signer` (the security key in hand, or the phrase root
    /// here), recorded as `label`, with the identity's newest state and
    /// the servers it signs in to.
    pub async fn answer_relay_pairing(
        &self,
        offer: &RelayOffer,
        identity: PortableUserId,
        signer: RootSigner<'_>,
        label: Option<&str>,
    ) -> Result<(), SdkError> {
        let server = parse_id(&offer.server)?;
        let portable = self.portable();
        let credential = portable
            .issue_credential_by(identity, signer, offer.offer.device_key, label)
            .await?;
        let state = portable.identity_state(identity).await?;
        let servers = self.paired_servers(identity).await?;
        expect_done(
            self.call(
                server,
                None,
                Request::AnswerPairing {
                    code: offer.offer.code.clone(),
                    answer: PairingAnswer {
                        credential: Box::new(credential),
                        state: state.map(Box::new),
                        servers,
                    },
                },
            )
            .await?,
        )
    }
}

fn expect_done(response: Response) -> Result<(), SdkError> {
    match response {
        Response::Ok(Reply::Done) => Ok(()),
        Response::Ok(other) => Err(SdkError::Protocol(format!("expected Done, got {other:?}"))),
        Response::Err(e) => Err(SdkError::from_server(e)),
    }
}

/// Whether `record` is the identity whose genesis handle is `handle`: its
/// genesis has it (and begins it: the id is the genesis's), or it has no
/// genesis and its id is the handle (a phrase identity, whose added keys
/// carry its id as their handle).
fn names(record: &IdentityRecord, handle: &[u8; HANDLE_LEN]) -> bool {
    match &record.genesis {
        Some(genesis) => genesis.handle == *handle && genesis.user() == record.user,
        None => record.user.as_bytes() == handle,
    }
}
