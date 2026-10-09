//! Whether a portable user's device credential authenticates here (P7-02,
//! ARCHITECTURE §45, §48), checked by this server alone: no account
//! server is asked.
//!
//! Only the check. The pre-auth request that carries a credential, the
//! nonce it answers, the session it makes and the principal it resolves
//! to (local-first where tied) are P7-06's.

use spoolframe_protocol::portable::{
    CredentialError, PortableUserId, Signature, UserDeviceCredential, verify_device_proof,
};

use crate::store::{Store, StoreError};

/// Why a presented credential does not authenticate.
#[derive(Debug, thiserror::Error)]
pub enum PresentationError {
    /// The credential, or the device's proof with it, does not hold.
    #[error(transparent)]
    Credential(#[from] CredentialError),
    /// An identity state seen here revokes its device key.
    #[error("the device key is revoked")]
    Revoked,
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
///    newest's, since revocation only grows).
///
/// The third is only as fresh as the states that have reached this
/// server (§48): a revocation stops a credential here once a device or
/// the phone brings the state that carries it, and expiry ends it
/// regardless.
pub async fn check_presentation(
    store: &Store,
    credential: &UserDeviceCredential,
    server: &[u8; 32],
    nonce: &[u8; 32],
    signature: &Signature,
    now: i64,
) -> Result<PortableUserId, PresentationError> {
    credential.verify(now)?;
    verify_device_proof(credential, server, nonce, signature)?;
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
        check_presentation(store, credential, &SERVER, &NONCE, signature, now).await
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
}
