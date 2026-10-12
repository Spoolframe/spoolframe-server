//! Whether a portable user's device credential authenticates here (P7-02,
//! ARCHITECTURE §45, §48), checked by this server alone: no account
//! server is asked.
//!
//! And the nonces a device signs over to sign in with one (P7-06): the
//! request that carries it and the session it makes are `crate::client`'s.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use password_hash::rand_core::{OsRng, RngCore};
use spoolframe_protocol::portable::{
    CredentialError, GenesisError, PortableIdentityState, PortableUserId, Signature,
    UserDeviceCredential, root_set, verify_device_proof,
};

use crate::clock::Clock;
use crate::store::{Store, StoreError};

/// How long a portable sign-in's nonce can be answered.
pub const SIGN_IN_NONCE_LIFETIME: Duration = Duration::from_secs(5 * 60);

/// The most nonces outstanding on the server. Each draws on the
/// strangers' budget, so this is a backstop, never reached by people.
const MAX_SIGN_IN_NONCES: usize = 10_000;

/// The portable sign-ins begun and not yet finished, by device endpoint:
/// one at a time each, a new one replacing it.
#[derive(Debug)]
pub struct SignInNonces {
    clock: Clock,
    pending: Mutex<HashMap<String, ([u8; 32], Instant)>>,
}

impl SignInNonces {
    pub fn new(clock: Clock) -> Self {
        Self {
            clock,
            pending: Mutex::default(),
        }
    }

    /// A new nonce for `endpoint`, replacing any it had; `None` when the
    /// server holds as many as it takes.
    pub fn begin(&self, endpoint: &str) -> Option<[u8; 32]> {
        let now = self.clock.now();
        let mut nonce = [0u8; 32];
        OsRng.fill_bytes(&mut nonce);
        let mut pending = self.pending.lock();
        pending.retain(|_, (_, began)| now.duration_since(*began) < SIGN_IN_NONCE_LIFETIME);
        pending.remove(endpoint);
        if pending.len() >= MAX_SIGN_IN_NONCES {
            return None;
        }
        pending.insert(endpoint.to_owned(), (nonce, now));
        Some(nonce)
    }

    /// `endpoint`'s nonce, used up; `None` if it has none, or it is over.
    pub fn take(&self, endpoint: &str) -> Option<[u8; 32]> {
        let now = self.clock.now();
        let (nonce, began) = self.pending.lock().remove(endpoint)?;
        (now.duration_since(began) < SIGN_IN_NONCE_LIFETIME).then_some(nonce)
    }
}

/// Why a presented credential does not authenticate.
#[derive(Debug, thiserror::Error)]
pub enum PresentationError {
    /// The credential, or the device's proof with it, does not hold.
    #[error(transparent)]
    Credential(#[from] CredentialError),
    /// An identity state seen here revokes its device key.
    #[error("the device key is revoked")]
    Revoked,
    /// The root that signed it is not one of its identity's now (P7-08):
    /// never added, or removed.
    #[error("the credential's root is not one of its identity's")]
    NotARoot,
    /// A state it carries is signed by a root its identity never had.
    #[error("an identity state it carries is not signed by one of its roots")]
    StateNotARoot,
    /// The genesis it carries is not one, or is another identity's.
    #[error("the genesis it carries is not its identity's: {0}")]
    Genesis(GenesisError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// The portable user `credential` proves, presented to the server whose
/// endpoint id is `server`, with `signature` the device's over its
/// `nonce`, at `now` (Unix seconds). It must hold:
///
/// 1. the root of the user it names signed it, and it is in date
///    ([`UserDeviceCredential::verify`]);
/// 2. its device key signed this server's nonce ([`verify_device_proof`]);
/// 3. no identity state seen here revokes the key ([`Store::revokes`]:
///    the union of every valid state's revocations, not only the
///    newest's, since revocation only grows);
/// 4. (P7-08) the root that signed it is one of its identity's now, as
///    the genesis (`genesis`, the one it carries, or the one kept here)
///    and the states kept here and `states` (carried, already verified,
///    held in memory only) work them out ([`Store::root_set`]); and every
///    state in `states` is signed by a root the identity ever had.
///
/// The third is only as fresh as the states that have reached this
/// server (§48): a revocation stops a credential here once a device or
/// the phone brings the state that carries it, and expiry ends it
/// regardless.
pub async fn check_presentation(
    store: &Store,
    credential: &UserDeviceCredential,
    states: &[PortableIdentityState],
    server: &[u8; 32],
    nonce: &[u8; 32],
    signature: &Signature,
    now: i64,
) -> Result<PortableUserId, PresentationError> {
    credential.verify(now)?;
    verify_device_proof(credential, server, nonce, signature)?;
    if let Some(genesis) = credential.genesis.as_deref() {
        genesis.verify().map_err(PresentationError::Genesis)?;
        if genesis.user() != credential.user {
            return Err(PresentationError::Genesis(GenesisError::BadSignature));
        }
    }
    // The roots this server knows (`before`), and with the states carried
    // in too (`roots`), from one read of what it keeps.
    let carried = credential.genesis.as_deref();
    let (kept, mut all) = store.root_states(credential.user, carried).await?;
    let genesis = carried
        .filter(|g| g.user() == credential.user)
        .or(kept.as_ref());
    let before = root_set(&credential.user, genesis, &all);
    all.extend(states.iter().filter(|s| s.user == credential.user).cloned());
    let roots = root_set(&credential.user, genesis, &all);
    if states.iter().any(|state| {
        state.user != credential.user
            || !roots.knew(&state.signer())
            || before.refuses_additions(&state.roots_added)
    }) {
        return Err(PresentationError::StateNotARoot);
    }
    if !roots.holds(&credential.signer()) {
        return Err(PresentationError::NotARoot);
    }
    if store
        .revokes(credential.user, &credential.device_key, now)
        .await?
    {
        return Err(PresentationError::Revoked);
    }
    Ok(credential.user)
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Signer, SigningKey};
    use spoolframe_protocol::portable::{
        MAX_CREDENTIAL_VALIDITY, PortableIdentityState, REVOCATION_WINDOW, device_proof_message,
    };

    use super::*;
    use crate::store::portable::tests::{T, device, entry, root, state, user};
    use crate::store::tests::{TestStore, test_store};

    const SERVER: [u8; 32] = [0x5a; 32];
    const NONCE: [u8; 32] = [0xa5; 32];

    /// User 7's credential for device `seed`, issued at [`T`] for the
    /// longest a credential may be.
    fn credential(seed: u8) -> UserDeviceCredential {
        UserDeviceCredential::new(
            user(7),
            device(seed),
            1,
            T,
            T + MAX_CREDENTIAL_VALIDITY,
            |m| root(7).sign(m),
        )
    }

    /// Device `seed`'s proof to [`SERVER`] over [`NONCE`].
    fn proof(seed: u8) -> Signature {
        SigningKey::from_bytes(&[seed; 32]).sign(&device_proof_message(&SERVER, &NONCE))
    }

    async fn check(
        store: &TestStore,
        credential: &UserDeviceCredential,
        signature: &Signature,
        now: i64,
    ) -> Result<PortableUserId, PresentationError> {
        check_presentation(store, credential, &[], &SERVER, &NONCE, signature, now).await
    }

    #[tokio::test]
    async fn a_good_credential_with_its_device_s_proof_is_its_user() {
        let store = test_store("presentation-good").await;
        let who = check(&store, &credential(9), &proof(9), T).await.unwrap();
        assert_eq!(who, user(7));
    }

    #[tokio::test]
    async fn a_proof_by_another_device_key_is_refused() {
        let store = test_store("presentation-wrong-key").await;
        assert!(matches!(
            check(&store, &credential(9), &proof(11), T).await,
            Err(PresentationError::Credential(
                CredentialError::BadDeviceProof
            ))
        ));
        // Nor does a proof made for another server or nonce pass.
        let elsewhere =
            SigningKey::from_bytes(&[9; 32]).sign(&device_proof_message(&[0; 32], &NONCE));
        assert!(matches!(
            check(&store, &credential(9), &elsewhere, T).await,
            Err(PresentationError::Credential(
                CredentialError::BadDeviceProof
            ))
        ));
    }

    #[tokio::test]
    async fn a_tampered_credential_is_refused() {
        let store = test_store("presentation-tampered").await;
        // Moved onto device 11's key, which then proves possession.
        let moved = UserDeviceCredential {
            device_key: device(11),
            ..credential(9)
        };
        assert!(matches!(
            check(&store, &moved, &proof(11), T).await,
            Err(PresentationError::Credential(CredentialError::BadSignature))
        ));
        let extended = UserDeviceCredential {
            expires_at: T + 2 * MAX_CREDENTIAL_VALIDITY,
            ..credential(9)
        };
        assert!(matches!(
            check(&store, &extended, &proof(9), T).await,
            Err(PresentationError::Credential(CredentialError::BadSignature))
        ));
    }

    #[tokio::test]
    async fn a_credential_signed_by_another_identity_s_root_is_refused() {
        let store = test_store("presentation-other-root").await;
        let forged =
            UserDeviceCredential::new(user(7), device(9), 1, T, T + MAX_CREDENTIAL_VALIDITY, |m| {
                root(8).sign(m)
            });
        assert!(matches!(
            check(&store, &forged, &proof(9), T).await,
            Err(PresentationError::Credential(CredentialError::BadSignature))
        ));
    }

    #[tokio::test]
    async fn a_newer_state_revoking_the_key_refuses_it_from_then_on() {
        let store = test_store("presentation-revoked").await;
        let tv = credential(9);
        let laptop = credential(11);
        // A state that revokes someone else changes nothing for either.
        store
            .accept_identity_state(&state(1, &[device(13)]), T)
            .await
            .unwrap();
        check(&store, &tv, &proof(9), T).await.unwrap();

        store
            .accept_identity_state(&state(2, &[device(13), device(9)]), T)
            .await
            .unwrap();
        assert!(matches!(
            check(&store, &tv, &proof(9), T).await,
            Err(PresentationError::Revoked)
        ));
        // The older state, brought again, does not undo it.
        assert!(
            !store
                .accept_identity_state(&state(1, &[device(13)]), T)
                .await
                .unwrap()
                .newest
        );
        assert!(matches!(
            check(&store, &tv, &proof(9), T + 1).await,
            Err(PresentationError::Revoked)
        ));
        // A renewed credential for the same key is refused just the same.
        let renewed = UserDeviceCredential::new(
            user(7),
            device(9),
            2,
            T + 1,
            T + 1 + MAX_CREDENTIAL_VALIDITY,
            |m| root(7).sign(m),
        );
        assert!(matches!(
            check(&store, &renewed, &proof(9), T + 1).await,
            Err(PresentationError::Revoked)
        ));
        // The other device carries on.
        check(&store, &laptop, &proof(11), T + 1).await.unwrap();
    }

    #[tokio::test]
    async fn an_expired_credential_is_refused_whatever_the_state() {
        let store = test_store("presentation-expired").await;
        let expired_at = T + MAX_CREDENTIAL_VALIDITY;
        assert!(matches!(
            check(&store, &credential(9), &proof(9), expired_at).await,
            Err(PresentationError::Credential(CredentialError::Expired))
        ));
        // A state that revokes nothing does not revive it.
        store
            .accept_identity_state(&state(1, &[]), T)
            .await
            .unwrap();
        assert!(matches!(
            check(&store, &credential(9), &proof(9), expired_at).await,
            Err(PresentationError::Credential(CredentialError::Expired))
        ));
    }

    #[tokio::test]
    async fn a_credential_from_beyond_the_clock_skew_is_refused() {
        let store = test_store("presentation-future").await;
        assert!(matches!(
            check(&store, &credential(9), &proof(9), T - 3600).await,
            Err(PresentationError::Credential(CredentialError::NotYetValid))
        ));
    }

    #[tokio::test]
    async fn a_credential_good_for_more_than_ninety_days_is_refused() {
        let store = test_store("presentation-long").await;
        let long = UserDeviceCredential::new(
            user(7),
            device(9),
            1,
            T,
            T + MAX_CREDENTIAL_VALIDITY + 1,
            |m| root(7).sign(m),
        );
        assert!(matches!(
            check(&store, &long, &proof(9), T).await,
            Err(PresentationError::Credential(CredentialError::TooLong))
        ));
    }

    #[tokio::test]
    async fn another_user_s_state_does_not_revoke_this_one_s_key() {
        let store = test_store("presentation-other-user").await;
        let theirs =
            PortableIdentityState::new(user(8), 1, vec![entry(9)], T, |m| root(8).sign(m)).unwrap();
        store.accept_identity_state(&theirs, T).await.unwrap();
        check(&store, &credential(9), &proof(9), T).await.unwrap();
    }

    #[tokio::test]
    async fn a_revocation_in_an_older_state_refuses_the_key_too() {
        let store = test_store("presentation-older-state").await;
        store
            .accept_identity_state(&state(2, &[device(13)]), T)
            .await
            .unwrap();
        // A state the newest does not include: its root holder never saw
        // revision 2.
        store
            .accept_identity_state(&state(1, &[device(9)]), T)
            .await
            .unwrap();
        assert!(matches!(
            check(&store, &credential(9), &proof(9), T).await,
            Err(PresentationError::Revoked)
        ));
    }

    #[tokio::test]
    async fn a_lapsed_revocation_no_longer_refuses() {
        let store = test_store("presentation-lapsed").await;
        store
            .accept_identity_state(&state(1, &[device(9)]), T)
            .await
            .unwrap();
        // A credential issued to the key after the revocation, by a root
        // that never knew of it; by the time the revocation lapses every
        // credential from before it has expired, so it is the only kind
        // left that could hold.
        let issued = T + REVOCATION_WINDOW;
        let late = UserDeviceCredential::new(
            user(7),
            device(9),
            9,
            issued,
            issued + MAX_CREDENTIAL_VALIDITY,
            |m| root(7).sign(m),
        );
        assert!(matches!(
            check(&store, &late, &proof(9), issued).await,
            Err(PresentationError::Revoked)
        ));
        check(&store, &late, &proof(9), issued + 1).await.unwrap();
    }

    // P7-08: an identity with security key roots.

    use spoolframe_protocol::portable::soft_key::SoftKey;
    use spoolframe_protocol::portable::{IdentityGenesis, RemovedRoot, RootKey, StateChanges};

    /// An identity begun with keys 3 and 4.
    fn key_identity() -> (IdentityGenesis, SoftKey, SoftKey) {
        let (a, b) = (SoftKey::from_seed(3), SoftKey::from_seed(4));
        let handle = [0x33; 32];
        let roots = vec![a.root(), b.root()];
        let message = IdentityGenesis::message(&handle, &roots).unwrap();
        let genesis = IdentityGenesis {
            handle,
            signatures: vec![a.assert(&message), b.assert(&message)],
            roots,
        };
        (genesis, a, b)
    }

    /// `by`'s credential for device `seed` of `genesis`'s identity,
    /// carrying the genesis.
    fn key_credential(genesis: &IdentityGenesis, by: &SoftKey, seed: u8) -> UserDeviceCredential {
        let mut credential = UserDeviceCredential::signed_by(
            genesis.user(),
            by.root(),
            device(seed),
            1,
            T,
            T + MAX_CREDENTIAL_VALIDITY,
            |m| by.assert(m),
        );
        credential.genesis = Some(Box::new(genesis.clone()));
        credential
    }

    #[tokio::test]
    async fn a_key_identity_s_credential_holds_with_its_genesis() {
        let store = test_store("presentation-key").await;
        let (genesis, a, b) = key_identity();
        for key in [&a, &b] {
            let who = check(&store, &key_credential(&genesis, key, 9), &proof(9), T)
                .await
                .unwrap();
            assert_eq!(who, genesis.user());
        }
        // Without the genesis, and none kept here: no root is known.
        let mut bare = key_credential(&genesis, &a, 9);
        bare.genesis = None;
        assert!(matches!(
            check(&store, &bare, &proof(9), T).await,
            Err(PresentationError::NotARoot)
        ));
        // Kept, it is not needed again.
        store.accept_genesis(&genesis, T).await.unwrap();
        check(&store, &bare, &proof(9), T).await.unwrap();
    }

    /// P7-08, Apple: key A, registered through raw CTAP2 on Linux, signs
    /// a credential through the system WebAuthn API on an iPhone; it holds
    /// as a CTAP2 one does. One over another message, or by another key
    /// in A's name, does not (the protocol's tests refuse the rest).
    #[tokio::test]
    async fn a_key_identity_s_webauthn_credential_holds_and_a_bad_one_does_not() {
        use spoolframe_protocol::portable::{CredentialError, RootSignature};

        let store = test_store("presentation-key-webauthn").await;
        let (genesis, a, b) = key_identity();
        let signed_by = |sign: &dyn Fn(&[u8]) -> RootSignature| {
            let mut credential = UserDeviceCredential::signed_by(
                genesis.user(),
                a.root(),
                device(9),
                1,
                T,
                T + MAX_CREDENTIAL_VALIDITY,
                sign,
            );
            credential.genesis = Some(Box::new(genesis.clone()));
            credential
        };
        let good = signed_by(&|m| a.assert_webauthn(m));
        assert!(matches!(good.signature, RootSignature::WebAuthn { .. }));
        assert_eq!(
            check(&store, &good, &proof(9), T).await.unwrap(),
            genesis.user()
        );
        for bad in [
            signed_by(&|_| a.assert_webauthn(b"another message")),
            signed_by(&|m| b.assert_webauthn(m)),
        ] {
            assert!(
                matches!(
                    check(&store, &bad, &proof(9), T).await,
                    Err(PresentationError::Credential(CredentialError::BadSignature))
                ),
                "{bad:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_key_that_is_not_the_identity_s_signs_no_credential_for_it() {
        let store = test_store("presentation-key-stranger").await;
        let (genesis, _, _) = key_identity();
        let stranger = SoftKey::from_seed(5);
        assert!(matches!(
            check(
                &store,
                &key_credential(&genesis, &stranger, 9),
                &proof(9),
                T
            )
            .await,
            Err(PresentationError::NotARoot)
        ));
        // Nor does another identity's genesis pass for this one.
        let (other, a, _) = {
            let (a, b) = (SoftKey::from_seed(6), SoftKey::from_seed(7));
            let roots = vec![a.root(), b.root()];
            let message = IdentityGenesis::message(&[0x44; 32], &roots).unwrap();
            (
                IdentityGenesis {
                    handle: [0x44; 32],
                    signatures: vec![a.assert(&message), b.assert(&message)],
                    roots,
                },
                a,
                b,
            )
        };
        let mut swapped = key_credential(&other, &a, 9);
        swapped.user = genesis.user();
        swapped.signature = a.assert(&UserDeviceCredential::message(
            &genesis.user(),
            &device(9),
            1,
            T,
            T + MAX_CREDENTIAL_VALIDITY,
        ));
        assert!(matches!(
            check(&store, &swapped, &proof(9), T).await,
            Err(PresentationError::Genesis(_))
        ));
    }

    #[tokio::test]
    async fn a_removed_key_s_credentials_fail_and_the_backup_s_hold() {
        let store = test_store("presentation-key-removed").await;
        let (genesis, a, b) = key_identity();
        store.accept_genesis(&genesis, T).await.unwrap();
        let removal = PortableIdentityState::signed_by(
            genesis.user(),
            b.root(),
            1,
            StateChanges {
                roots_removed: vec![RemovedRoot {
                    root: a.root(),
                    removed_at: T,
                }],
                ..StateChanges::default()
            },
            T,
            |m| b.assert(m),
        )
        .unwrap();
        store.accept_identity_state(&removal, T).await.unwrap();
        assert!(matches!(
            check(&store, &key_credential(&genesis, &a, 9), &proof(9), T).await,
            Err(PresentationError::NotARoot)
        ));
        check(&store, &key_credential(&genesis, &b, 11), &proof(11), T)
            .await
            .unwrap();
        // A state the removed key signs now changes nothing it could use.
        let late = PortableIdentityState::signed_by(
            genesis.user(),
            a.root(),
            2,
            StateChanges::default(),
            T,
            |m| a.assert(m),
        )
        .unwrap();
        store.accept_identity_state(&late, T).await.unwrap();
        assert!(matches!(
            check(&store, &key_credential(&genesis, &a, 9), &proof(9), T).await,
            Err(PresentationError::NotARoot)
        ));
    }

    #[tokio::test]
    async fn a_carried_state_by_a_stranger_root_is_refused() {
        let store = test_store("presentation-key-stranger-state").await;
        let (genesis, a, _) = key_identity();
        let stranger = SoftKey::from_seed(5);
        let state = PortableIdentityState::signed_by(
            genesis.user(),
            stranger.root(),
            1,
            StateChanges::default(),
            T,
            |m| stranger.assert(m),
        )
        .unwrap();
        assert!(matches!(
            check_presentation(
                &store,
                &key_credential(&genesis, &a, 9),
                &[state],
                &SERVER,
                &NONCE,
                &proof(9),
                T,
            )
            .await,
            Err(PresentationError::StateNotARoot)
        ));
    }

    #[tokio::test]
    async fn a_phrase_identity_s_added_key_signs_credentials_once_the_state_is_here() {
        let store = test_store("presentation-phrase-adds-key").await;
        let key = SoftKey::from_seed(3);
        let by_key = UserDeviceCredential::signed_by(
            user(7),
            key.root(),
            device(9),
            1,
            T,
            T + MAX_CREDENTIAL_VALIDITY,
            |m| key.assert(m),
        );
        assert!(matches!(
            check(&store, &by_key, &proof(9), T).await,
            Err(PresentationError::NotARoot)
        ));
        let adds = PortableIdentityState::signed_by(
            user(7),
            RootKey::phrase_of(&user(7)),
            1,
            StateChanges {
                roots_added: vec![spoolframe_protocol::portable::AddedRoot {
                    root: key.root(),
                    added_by: RootKey::phrase_of(&user(7)),
                    added_at: T,
                    endorsement: root(7)
                        .sign(&spoolframe_protocol::portable::root_endorse_message(
                            &user(7),
                            &key.root(),
                            T,
                        ))
                        .into(),
                    possession: key.assert(&spoolframe_protocol::portable::root_add_message(
                        &user(7),
                        &key.root(),
                    )),
                }],
                ..StateChanges::default()
            },
            T,
            |m| root(7).sign(m).into(),
        )
        .unwrap();
        store.accept_identity_state(&adds, T).await.unwrap();
        assert_eq!(check(&store, &by_key, &proof(9), T).await.unwrap(), user(7));
        // The phrase root still signs, as before.
        check(&store, &credential(11), &proof(11), T).await.unwrap();
    }
}
