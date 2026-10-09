//! Tying a local account to a portable identity (P7-04, ARCHITECTURE
//! §58): the challenge a session begins, and the check of what the
//! identity signs over it. The link itself is the store's
//! (`crate::store::ties`).
//!
//! The server never holds the root (§46): the identity proves the tie by
//! signing [`tie_message`] with its root, on the phone that holds it, or
//! with a device key whose credential the root signed. A challenge belongs
//! to the session that began it, and to its user; it lasts
//! [`TIE_CHALLENGE_LIFETIME`] and is used once, whatever the proof.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use password_hash::rand_core::{OsRng, RngCore};
use spoolframe_protocol::portable::{PortableUserId, TieProof, TieProofError, tie_message};

use crate::clock::Clock;
use crate::store::users::LocalUserId;
use crate::store::{Store, StoreError};

/// How long a tie challenge can be answered.
pub const TIE_CHALLENGE_LIFETIME: Duration = Duration::from_secs(5 * 60);

/// The most challenges one user has outstanding, across their sessions.
/// A new one past it ends their oldest.
pub const MAX_TIE_CHALLENGES_PER_USER: usize = 4;

/// The most challenges outstanding on the server. Only a session that
/// signed in recently begins one, so this is never reached by people.
const MAX_TIE_CHALLENGES: usize = 10_000;

/// The tie challenges begun and not yet answered, by session (its token's
/// hash): one at a time each, a new one replacing it.
#[derive(Debug)]
pub struct TieChallenges {
    clock: Clock,
    lifetime: Duration,
    pending: Mutex<HashMap<String, Pending>>,
}

#[derive(Debug, Clone, Copy)]
struct Pending {
    user: LocalUserId,
    nonce: [u8; 32],
    began: Instant,
}

/// Why a tie challenge was not answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChallengeError {
    /// The server holds as many as it takes.
    Full,
}

impl TieChallenges {
    /// Challenges that expire by `clock`, after [`TIE_CHALLENGE_LIFETIME`].
    pub fn new(clock: Clock) -> Self {
        Self {
            clock,
            lifetime: TIE_CHALLENGE_LIFETIME,
            pending: Mutex::default(),
        }
    }

    /// A new challenge for `user` in `session`: its nonce, and how long it
    /// lasts. Any the session began before is dropped, as is the user's
    /// oldest past [`MAX_TIE_CHALLENGES_PER_USER`].
    pub fn begin(
        &self,
        session: &str,
        user: LocalUserId,
    ) -> Result<([u8; 32], Duration), ChallengeError> {
        let now = self.clock.now();
        let mut nonce = [0u8; 32];
        OsRng.fill_bytes(&mut nonce);
        let mut pending = self.pending.lock();
        pending.retain(|_, p| now.duration_since(p.began) < self.lifetime);
        pending.remove(session);
        let mut theirs: Vec<(String, Instant)> = pending
            .iter()
            .filter(|(_, p)| p.user == user)
            .map(|(session, p)| (session.clone(), p.began))
            .collect();
        theirs.sort_by_key(|(_, began)| *began);
        let over = (theirs.len() + 1).saturating_sub(MAX_TIE_CHALLENGES_PER_USER);
        for (session, _) in theirs.into_iter().take(over) {
            pending.remove(&session);
        }
        if pending.len() >= MAX_TIE_CHALLENGES {
            return Err(ChallengeError::Full);
        }
        pending.insert(
            session.to_owned(),
            Pending {
                user,
                nonce,
                began: now,
            },
        );
        Ok((nonce, self.lifetime))
    }

    /// The nonce of the challenge `session` began for `user`, used up; `None`
    /// if it has none outstanding, it is over, or it was begun for another
    /// user.
    pub fn take(&self, session: &str, user: LocalUserId) -> Option<[u8; 32]> {
        let now = self.clock.now();
        let mut pending = self.pending.lock();
        let found = pending.get(session).copied()?;
        if found.user != user {
            return None;
        }
        pending.remove(session);
        (now.duration_since(found.began) < self.lifetime).then_some(found.nonce)
    }
}

/// Why a tie proof does not prove its identity here.
#[derive(Debug, thiserror::Error)]
pub enum TieCheckError {
    #[error(transparent)]
    Proof(#[from] TieProofError),
    /// An identity state seen here revokes the device key that signed.
    #[error("the device key is revoked")]
    Revoked,
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Whether `proof` shows `user` tying to the account `username` on the
/// server whose endpoint id is `server`, over `nonce`, at `now` (Unix
/// seconds): the root's signature, or a device key's with a credential
/// for `user` that holds and that no identity state seen here revokes
/// ([`Store::revokes`]).
pub async fn check_tie(
    store: &Store,
    proof: &TieProof,
    user: PortableUserId,
    server: &[u8; 32],
    nonce: &[u8; 32],
    username: &str,
    now: i64,
) -> Result<(), TieCheckError> {
    // A username is at most 64 characters: always some message.
    let message = tie_message(server, nonce, &user, username).ok_or(TieProofError::BadSignature)?;
    proof.verify(&user, &message, now)?;
    if let TieProof::Device { credential, .. } = proof
        && store.revokes(user, &credential.device_key, now).await?
    {
        return Err(TieCheckError::Revoked);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Signer, SigningKey};
    use spoolframe_protocol::portable::{MAX_CREDENTIAL_VALIDITY, UserDeviceCredential};
    use uuid::Uuid;

    use super::*;
    use crate::store::portable::tests::{T, device, root, state, user};
    use crate::store::tests::test_store;

    const SERVER: [u8; 32] = [0x5a; 32];
    const NONCE: [u8; 32] = [0xa5; 32];

    fn local(n: u128) -> LocalUserId {
        LocalUserId::from_uuid(Uuid::from_u128(n))
    }

    fn signed(key: &SigningKey, username: &str) -> ed25519_dalek::Signature {
        key.sign(&tie_message(&SERVER, &NONCE, &user(7), username).unwrap())
    }

    fn by_device(seed: u8) -> TieProof {
        TieProof::Device {
            credential: UserDeviceCredential::new(
                user(7),
                device(seed),
                1,
                T,
                T + MAX_CREDENTIAL_VALIDITY,
                |m| root(7).sign(m),
            ),
            signature: signed(&SigningKey::from_bytes(&[seed; 32]), "alice"),
        }
    }

    #[test]
    fn a_challenge_is_its_session_s_and_used_once() {
        let challenges = TieChallenges::new(Clock::default());
        let (nonce, lasts) = challenges.begin("s1", local(1)).unwrap();
        assert_eq!(lasts, TIE_CHALLENGE_LIFETIME);
        // Not another session's, nor another user's in this one.
        assert_eq!(challenges.take("s2", local(1)), None);
        assert_eq!(challenges.take("s1", local(2)), None);
        assert_eq!(challenges.take("s1", local(1)), Some(nonce));
        assert_eq!(challenges.take("s1", local(1)), None);
    }

    #[test]
    fn beginning_again_replaces_the_session_s_challenge() {
        let challenges = TieChallenges::new(Clock::default());
        let (first, _) = challenges.begin("s1", local(1)).unwrap();
        let (second, _) = challenges.begin("s1", local(1)).unwrap();
        assert_ne!(first, second);
        assert_eq!(challenges.take("s1", local(1)), Some(second));
    }

    #[test]
    fn a_challenge_past_its_lifetime_is_over() {
        let clock = Clock::default();
        let challenges = TieChallenges::new(clock.clone());
        challenges.begin("s1", local(1)).unwrap();
        clock.advance(TIE_CHALLENGE_LIFETIME);
        assert_eq!(challenges.take("s1", local(1)), None);
    }

    #[test]
    fn a_user_holds_a_few_challenges_at_most() {
        let clock = Clock::default();
        let challenges = TieChallenges::new(clock.clone());
        for n in 0..=MAX_TIE_CHALLENGES_PER_USER {
            challenges.begin(&format!("s{n}"), local(1)).unwrap();
            clock.advance(Duration::from_millis(1));
        }
        // The oldest went for the newest; another user's are their own.
        challenges.begin("other", local(2)).unwrap();
        assert_eq!(challenges.take("s0", local(1)), None);
        for n in 1..=MAX_TIE_CHALLENGES_PER_USER {
            assert!(challenges.take(&format!("s{n}"), local(1)).is_some());
        }
        assert!(challenges.take("other", local(2)).is_some());
    }

    #[tokio::test]
    async fn the_root_or_a_device_ties() {
        let store = test_store("tie-check-good").await;
        let by_root = TieProof::Root {
            signature: signed(&root(7), "alice"),
        };
        for proof in [by_root, by_device(9)] {
            check_tie(&store, &proof, user(7), &SERVER, &NONCE, "alice", T)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn a_proof_for_another_account_is_refused() {
        let store = test_store("tie-check-account").await;
        let proof = TieProof::Root {
            signature: signed(&root(7), "alice"),
        };
        assert!(matches!(
            check_tie(&store, &proof, user(7), &SERVER, &NONCE, "bob", T).await,
            Err(TieCheckError::Proof(TieProofError::BadSignature))
        ));
    }

    #[tokio::test]
    async fn a_revoked_device_key_ties_nothing() {
        let store = test_store("tie-check-revoked").await;
        store
            .accept_identity_state(&state(1, &[device(9)]), T)
            .await
            .unwrap();
        assert!(matches!(
            check_tie(&store, &by_device(9), user(7), &SERVER, &NONCE, "alice", T).await,
            Err(TieCheckError::Revoked)
        ));
        // Another device of the same identity still ties.
        check_tie(&store, &by_device(11), user(7), &SERVER, &NONCE, "alice", T)
            .await
            .unwrap();
    }
}
