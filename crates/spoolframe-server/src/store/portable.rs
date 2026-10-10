//! Portable users' identity states (P7-02, ARCHITECTURE §48). Which
//! credential they refuse is `crate::portable`'s; this is storage, and the
//! rules that hold whoever calls: revocation only grows (every valid
//! state adds its keys, whatever its revision), a state replaces the
//! newest only with a higher revision, and a revocation is dropped only
//! once it has lapsed, when no credential it could stop still holds.

use spoolframe_protocol::portable::{
    DevicePublicKey, PortableIdentityState, PortableUserId, REVOCATION_WINDOW, StateError,
    revocation_lapsed,
};

use super::{Store, StoreError};

#[derive(Debug, thiserror::Error)]
pub enum IdentityStateError {
    /// Not signed by the user it names, or over the size limit.
    #[error(transparent)]
    Invalid(#[from] StateError),
    /// A revision past what the database counts to (2^63 − 1). No root
    /// this project signs with comes near it.
    #[error("the identity state's revision is out of range")]
    RevisionOutOfRange,
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<sqlx::Error> for IdentityStateError {
    fn from(e: sqlx::Error) -> Self {
        IdentityStateError::Store(e.into())
    }
}

/// What taking in a state changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateAccepted {
    /// It is now its user's newest state here.
    pub newest: bool,
    /// Revocations it added, or made earlier, here.
    pub revocations: u64,
}

impl Store {
    /// Take in `state`, if it is valid: its revocations that have not
    /// lapsed at `now` join its user's here, each at its earliest
    /// `revoked_at`, whatever its revision; and it becomes the newest
    /// if its revision is higher than the newest here. One transaction,
    /// so two arriving at once leave the higher as the newest and both
    /// lists in the union. A different state at the same revision (the
    /// root signed two) is not kept as the newest, and the log says so;
    /// its revocations count all the same.
    pub async fn accept_identity_state(
        &self,
        state: &PortableIdentityState,
        now: i64,
    ) -> Result<StateAccepted, IdentityStateError> {
        state.verify()?;
        let revision =
            i64::try_from(state.revision).map_err(|_| IdentityStateError::RevisionOutOfRange)?;
        let user = state.user.to_string();
        let json = serde_json::to_string(state).expect("an identity state always serializes");
        let mut tx = self.pool.begin().await?;
        let mut revocations = 0;
        let mut ended = 0;
        for entry in state.revoked.iter() {
            if revocation_lapsed(entry.revoked_at, now) {
                continue;
            }
            revocations += sqlx::query(
                "INSERT INTO portable_revoked_keys (user_id, device_key, revoked_at) \
                 VALUES (?1, ?2, ?3) \
                 ON CONFLICT (user_id, device_key) DO UPDATE SET revoked_at = excluded.revoked_at \
                 WHERE excluded.revoked_at < portable_revoked_keys.revoked_at",
            )
            .bind(&user)
            .bind(entry.key.to_string())
            .bind(entry.revoked_at)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            // The sessions the key opened end with it (P7-06): a guest's,
            // and a tied account's opened with the credential.
            let key = entry.key.to_string();
            ended += sqlx::query(
                "UPDATE portable_sessions SET revoked_at = ?3 \
                 WHERE portable_user_id = ?1 AND device_key = ?2 AND revoked_at IS NULL",
            )
            .bind(&user)
            .bind(&key)
            .bind(now)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            ended += sqlx::query(
                "UPDATE sessions SET revoked_at = ?3 \
                 WHERE portable_user_id = ?1 AND portable_device_key = ?2 AND revoked_at IS NULL",
            )
            .bind(&user)
            .bind(&key)
            .bind(now)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        }
        Self::prune_revocations(&mut tx, &user, now).await?;
        let newest = sqlx::query(
            "INSERT INTO portable_identity_states (user_id, revision, state, seen_at) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT (user_id) DO UPDATE SET \
               revision = excluded.revision, state = excluded.state, seen_at = excluded.seen_at \
             WHERE excluded.revision > portable_identity_states.revision",
        )
        .bind(&user)
        .bind(revision)
        .bind(json)
        .bind(now)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        tx.commit().await?;
        if ended > 0 {
            tracing::info!(user = %state.user, sessions = ended, "ended the sessions of revoked device keys");
            self.note_sessions_ended();
        }
        if newest || revocations > 0 {
            tracing::info!(
                user = %state.user,
                revision = state.revision,
                newest,
                revocations,
                "took in a portable identity state"
            );
        }
        if !newest
            && let Some(stored) = self.identity_state(state.user).await?
            && stored.revision == state.revision
            && !stored.same_as(state)
        {
            tracing::warn!(
                user = %state.user,
                revision = state.revision,
                "two different identity states signed at one revision; keeping the first seen \
                 as the newest, and the revocations of both"
            );
        }
        Ok(StateAccepted {
            newest,
            revocations,
        })
    }

    /// The newest identity state of `user` seen here, if any: the one to
    /// hand on. What it revokes is not all that is revoked here
    /// ([`Store::revokes`]).
    pub async fn identity_state(
        &self,
        user: PortableUserId,
    ) -> Result<Option<PortableIdentityState>, StoreError> {
        let json: Option<String> =
            sqlx::query_scalar("SELECT state FROM portable_identity_states WHERE user_id = ?1")
                .bind(user.to_string())
                .fetch_optional(&self.pool)
                .await?;
        json.map(|json| {
            serde_json::from_str(&json).map_err(|e| StoreError::Db(sqlx::Error::Decode(e.into())))
        })
        .transpose()
    }

    /// Whether any valid state seen here revokes `user`'s `key`, by a
    /// revocation that has not lapsed at `now`. The user's lapsed ones are
    /// dropped on the way.
    pub async fn revokes(
        &self,
        user: PortableUserId,
        key: &DevicePublicKey,
        now: i64,
    ) -> Result<bool, StoreError> {
        let user = user.to_string();
        let mut tx = self.pool.begin().await?;
        Self::prune_revocations(&mut tx, &user, now).await?;
        let (revoked,): (bool,) = sqlx::query_as(
            "SELECT EXISTS (SELECT 1 FROM portable_revoked_keys \
                            WHERE user_id = ?1 AND device_key = ?2)",
        )
        .bind(&user)
        .bind(key.to_string())
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(revoked)
    }

    /// Drop `user`'s revocations that have lapsed at `now`.
    async fn prune_revocations(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        user: &str,
        now: i64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM portable_revoked_keys WHERE user_id = ?1 AND revoked_at < ?2")
            .bind(user)
            .bind(now.saturating_sub(REVOCATION_WINDOW))
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    /// Every revocation of `user` here, key and time, by key.
    #[cfg(test)]
    pub(crate) async fn revoked_keys(&self, user: PortableUserId) -> Vec<(String, i64)> {
        sqlx::query_as(
            "SELECT device_key, revoked_at FROM portable_revoked_keys \
             WHERE user_id = ?1 ORDER BY device_key",
        )
        .bind(user.to_string())
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use ed25519_dalek::{Signer, SigningKey};
    use spoolframe_protocol::portable::{DevicePublicKey, RevokedKey};

    use super::super::tests::test_store;
    use super::*;

    /// 2026-10-09T00:00:00Z.
    pub(crate) const T: i64 = 1_791_504_000;

    pub(crate) fn root(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    pub(crate) fn user(seed: u8) -> PortableUserId {
        PortableUserId::from_bytes(root(seed).verifying_key().to_bytes()).unwrap()
    }

    pub(crate) fn device(seed: u8) -> DevicePublicKey {
        DevicePublicKey::from_bytes(root(seed).verifying_key().to_bytes()).unwrap()
    }

    /// Device `seed`'s key, revoked at [`T`].
    pub(crate) fn entry(seed: u8) -> RevokedKey {
        RevokedKey {
            key: device(seed),
            revoked_at: T,
        }
    }

    /// User 7's state at `revision`, revoking `revoked` at [`T`].
    pub(crate) fn state(revision: u64, revoked: &[DevicePublicKey]) -> PortableIdentityState {
        let entries = revoked
            .iter()
            .map(|&key| RevokedKey { key, revoked_at: T })
            .collect();
        PortableIdentityState::new(user(7), revision, entries, T, |m| root(7).sign(m)).unwrap()
    }

    #[tokio::test]
    async fn a_user_with_no_state_has_none() {
        let store = test_store("portable-none").await;
        assert!(store.identity_state(user(7)).await.unwrap().is_none());
    }

    /// Whether `store` takes `state` in as the newest, at [`T`].
    async fn newest(store: &Store, state: &PortableIdentityState) -> bool {
        store.accept_identity_state(state, T).await.unwrap().newest
    }

    #[tokio::test]
    async fn a_newer_state_replaces_an_older_one_and_not_the_reverse() {
        let store = test_store("portable-newer").await;
        let first = state(1, &[device(9)]);
        let second = state(2, &[device(9), device(11)]);
        assert!(newest(&store, &first).await);
        assert!(newest(&store, &second).await);
        assert!(!newest(&store, &first).await);
        assert!(!newest(&store, &second).await);
        assert_eq!(store.identity_state(user(7)).await.unwrap(), Some(second));
    }

    #[tokio::test]
    async fn another_state_at_the_same_revision_keeps_the_first_and_both_revocations() {
        let store = test_store("portable-same").await;
        let first = state(3, &[device(9)]);
        let rival = state(3, &[device(11)]);
        assert!(newest(&store, &first).await);
        let taken = store.accept_identity_state(&rival, T).await.unwrap();
        assert_eq!(
            taken,
            StateAccepted {
                newest: false,
                revocations: 1
            }
        );
        assert_eq!(store.identity_state(user(7)).await.unwrap(), Some(first));
        assert!(store.revokes(user(7), &device(9), T).await.unwrap());
        assert!(store.revokes(user(7), &device(11), T).await.unwrap());
    }

    #[tokio::test]
    async fn an_older_state_adds_its_revocations_and_a_newer_cannot_undo_them() {
        let store = test_store("portable-union").await;
        assert!(newest(&store, &state(2, &[device(9)])).await);
        // Signed by a second holder of the root that never saw revision 2.
        assert!(!newest(&store, &state(1, &[device(11)])).await);
        assert!(newest(&store, &state(3, &[device(13)])).await);
        for seed in [9, 11, 13] {
            assert!(
                store.revokes(user(7), &device(seed), T).await.unwrap(),
                "{seed}"
            );
        }
        assert!(!store.revokes(user(7), &device(15), T).await.unwrap());
    }

    #[tokio::test]
    async fn a_key_keeps_its_earliest_revocation() {
        let store = test_store("portable-earliest").await;
        let at = |revision, revoked_at| {
            PortableIdentityState::new(
                user(7),
                revision,
                vec![RevokedKey {
                    key: device(9),
                    revoked_at,
                }],
                T,
                |m| root(7).sign(m),
            )
            .unwrap()
        };
        store.accept_identity_state(&at(1, T), T).await.unwrap();
        let earlier = store
            .accept_identity_state(&at(2, T - 100), T)
            .await
            .unwrap();
        assert_eq!(earlier.revocations, 1);
        let later = store
            .accept_identity_state(&at(3, T + 100), T)
            .await
            .unwrap();
        assert_eq!(later.revocations, 0);
        assert_eq!(
            store.revoked_keys(user(7)).await,
            [(device(9).to_string(), T - 100)]
        );
    }

    #[tokio::test]
    async fn a_lapsed_revocation_is_dropped() {
        let store = test_store("portable-lapsed").await;
        let lapsed_at = T + REVOCATION_WINDOW + 1;
        // Lapsed on arrival: not kept, though the state is the newest.
        let taken = store
            .accept_identity_state(&state(1, &[device(9)]), lapsed_at)
            .await
            .unwrap();
        assert_eq!(
            taken,
            StateAccepted {
                newest: true,
                revocations: 0
            }
        );
        assert_eq!(store.revoked_keys(user(7)).await, []);
        // Kept while live, then dropped when asked about once lapsed.
        store
            .accept_identity_state(&state(2, &[device(11)]), T)
            .await
            .unwrap();
        assert!(
            store
                .revokes(user(7), &device(11), T + REVOCATION_WINDOW)
                .await
                .unwrap()
        );
        assert!(
            !store
                .revokes(user(7), &device(11), lapsed_at)
                .await
                .unwrap()
        );
        assert_eq!(store.revoked_keys(user(7)).await, []);
    }

    #[tokio::test]
    async fn a_state_not_signed_by_its_user_is_refused_and_not_kept() {
        let store = test_store("portable-forged").await;
        let forged =
            PortableIdentityState::new(user(7), 1, vec![entry(9)], T, |m| root(8).sign(m)).unwrap();
        assert!(matches!(
            store.accept_identity_state(&forged, T).await,
            Err(IdentityStateError::Invalid(StateError::BadSignature))
        ));
        let tampered = PortableIdentityState {
            revoked: vec![],
            ..state(1, &[device(9)])
        };
        assert!(matches!(
            store.accept_identity_state(&tampered, T).await,
            Err(IdentityStateError::Invalid(StateError::BadSignature))
        ));
        assert!(store.identity_state(user(7)).await.unwrap().is_none());
        assert_eq!(store.revoked_keys(user(7)).await, []);
    }

    #[tokio::test]
    async fn users_states_keep_apart() {
        let store = test_store("portable-apart").await;
        let other =
            PortableIdentityState::new(user(8), 5, vec![entry(9)], T, |m| root(8).sign(m)).unwrap();
        assert!(newest(&store, &state(1, &[])).await);
        assert!(newest(&store, &other).await);
        assert_eq!(
            store.identity_state(user(7)).await.unwrap(),
            Some(state(1, &[]))
        );
        assert_eq!(store.identity_state(user(8)).await.unwrap(), Some(other));
        assert!(!store.revokes(user(7), &device(9), T).await.unwrap());
        assert!(store.revokes(user(8), &device(9), T).await.unwrap());
    }

    #[tokio::test]
    async fn a_revision_past_the_database_s_range_is_refused() {
        let store = test_store("portable-range").await;
        let huge = state(u64::MAX, &[]);
        assert!(matches!(
            store.accept_identity_state(&huge, T).await,
            Err(IdentityStateError::RevisionOutOfRange)
        ));
    }
}
