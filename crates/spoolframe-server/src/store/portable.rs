//! Portable users' identity states (P7-02, ARCHITECTURE §48). Which
//! credential they refuse is `crate::portable`'s; this is storage, and the
//! rules that hold whoever calls: revocation only grows (every valid
//! state adds its keys, whatever its revision), a state replaces the
//! newest only with a higher revision (or, at the same one, a lower
//! digest), a revocation is dropped only once it has lapsed, when no
//! credential it could stop still holds, and a root-changing state is
//! dropped only once the kept ones carry its root changes.

use std::cmp::Ordering;

use spoolframe_protocol::portable::{
    DevicePublicKey, GenesisError, HANDLE_LEN, IdentityGenesis, PortableIdentityState,
    PortableUserId, REVOCATION_WINDOW, RootSet, StateError, newest_first, revocation_lapsed,
    root_set,
};

use super::{Store, StoreError};

/// A genesis handle as kept in `portable_geneses.handle`: 64 lowercase hex
/// digits, as the genesis JSON writes it (migration 0025 copied them from
/// there).
fn handle_hex(handle: &[u8; HANDLE_LEN]) -> String {
    handle.iter().map(|b| format!("{b:02x}")).collect()
}

/// The root-changing states kept for an identity (P7-08), past which the
/// older go: each lists every change its signer knew of, so along one
/// line the newest few carry it all. An older one whose changes these do
/// not carry (a rival branch's removal) is kept beyond it.
const MAX_ROOT_STATES: i64 = 32;

#[derive(Debug, thiserror::Error)]
pub enum IdentityStateError {
    /// Not signed by the user it names, or over the size limit.
    #[error(transparent)]
    Invalid(#[from] StateError),
    /// A revision past what the database counts to (2^63 − 1). No root
    /// this project signs with comes near it.
    #[error("the identity state's revision is out of range")]
    RevisionOutOfRange,
    /// Signed by a root that was never one of its identity's here (P7-08).
    #[error("the identity state is signed by a root that is not its identity's")]
    NotARoot,
    /// A genesis that is not one, or not of the identity it is kept for.
    #[error("not the identity's genesis: {0}")]
    Genesis(GenesisError),
    #[error(transparent)]
    Store(#[from] StoreError),
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
    /// if it comes first by [`newest_first`]: a higher revision, or the
    /// same one and a lower digest. One transaction, so two arriving at
    /// once leave the same newest whatever their order, and both lists
    /// in the union. Two different states at one revision (two holders
    /// signed one each) are logged; the revocations of both count. A
    /// state refused because a removed root adds a key through it is
    /// [`IdentityStateError::NotARoot`], its revocations counted.
    pub async fn accept_identity_state(
        &self,
        state: &PortableIdentityState,
        now: i64,
    ) -> Result<StateAccepted, IdentityStateError> {
        state.verify()?;
        // The signer must be one of the identity's roots, ever (P7-08):
        // the genesis's, or one a root added. A removed root's states
        // still count, for what they remove.
        let (kept, mut all) = self.root_states(state.user, None).await?;
        let before = root_set(&state.user, kept.as_ref(), &all);
        all.push(state.clone());
        let roots = root_set(&state.user, kept.as_ref(), &all);
        if !roots.knew(&state.signer()) {
            return Err(IdentityStateError::NotARoot);
        }
        // A root removed here adds no one new from now on, whatever time
        // its endorsement claims: a stolen key cannot backdate its way
        // past a removal this server already has. The state is refused,
        // but it is the word of one of the identity's roots, so its
        // revocations count: a rival branch's revocation is not lost here
        // for the addition that came with it.
        if before.refuses_additions(&state.roots_added) {
            let user = state.user.to_string();
            let mut tx = self.begin_write().await?;
            let (revocations, ended) = Self::take_revocations(&mut tx, &user, state, now).await?;
            tx.commit().await?;
            if ended > 0 {
                self.note_sessions_ended();
            }
            tracing::warn!(
                user = %state.user,
                revision = state.revision,
                revocations,
                "refused an identity state that adds a root through a removed one; kept its revocations"
            );
            return Err(IdentityStateError::NotARoot);
        }
        let changes_roots = state.signer.is_some()
            || !state.roots_added.is_empty()
            || !state.roots_removed.is_empty();
        let revision =
            i64::try_from(state.revision).map_err(|_| IdentityStateError::RevisionOutOfRange)?;
        let user = state.user.to_string();
        let json = serde_json::to_string(state).expect("an identity state always serializes");
        let mut tx = self.begin_write().await?;
        let (revocations, ended) = Self::take_revocations(&mut tx, &user, state, now).await?;
        Self::prune_revocations(&mut tx, &user, now).await?;
        if changes_roots {
            let mut digest = blake3::Hasher::new();
            digest.update(&state.signed_message());
            digest.update(
                serde_json::to_string(&state.signature)
                    .expect("a signature always serializes")
                    .as_bytes(),
            );
            sqlx::query(
                "INSERT INTO portable_root_states (user_id, digest, revision, state) \
                 VALUES (?1, ?2, ?3, ?4) ON CONFLICT (user_id, digest) DO NOTHING",
            )
            .bind(&user)
            .bind(digest.finalize().to_hex().as_str())
            .bind(revision)
            .bind(&json)
            .execute(&mut *tx)
            .await?;
            Self::evict_root_states(&mut tx, &user).await?;
        }
        // S2: the newest is the highest revision, then the lower digest,
        // whichever came first, so every server hands on the same one.
        let stored: Option<String> =
            sqlx::query_scalar("SELECT state FROM portable_identity_states WHERE user_id = ?1")
                .bind(&user)
                .fetch_optional(&mut *tx)
                .await?;
        let stored: Option<PortableIdentityState> = stored
            .map(|json| serde_json::from_str(&json).map_err(super::decode_error))
            .transpose()?;
        let newest = stored
            .as_ref()
            .is_none_or(|stored| newest_first(state, stored) == Ordering::Less);
        if newest {
            sqlx::query(
                "INSERT INTO portable_identity_states (user_id, revision, state, seen_at) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT (user_id) DO UPDATE SET \
                   revision = excluded.revision, state = excluded.state, seen_at = excluded.seen_at",
            )
            .bind(&user)
            .bind(revision)
            .bind(json)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
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
        if let Some(stored) = stored
            && stored.revision == state.revision
            && !stored.same_as(state)
        {
            tracing::warn!(
                user = %state.user,
                revision = state.revision,
                "two different identity states signed at one revision; the lower digest is the \
                 newest, and the revocations of both count"
            );
        }
        Ok(StateAccepted {
            newest,
            revocations,
        })
    }

    /// Keep the root-changing states of `user` within [`MAX_ROOT_STATES`]
    /// (S1): the newest that many, and any older one whose root changes
    /// the kept ones do not carry. Each state lists every change its
    /// signer knew of, so along one line the newest carry the older's;
    /// a rival branch's do not, and dropping one would forget a removal
    /// (a removed key would be a root again here).
    async fn evict_root_states(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        user: &str,
    ) -> Result<(), StoreError> {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT digest, state FROM portable_root_states WHERE user_id = ?1")
                .bind(user)
                .fetch_all(&mut **tx)
                .await?;
        if rows.len() <= MAX_ROOT_STATES as usize {
            return Ok(());
        }
        // A row that does not parse is kept: it is not ours to judge.
        let mut states: Vec<(String, PortableIdentityState)> = rows
            .into_iter()
            .filter_map(|(digest, json)| Some((digest, serde_json::from_str(&json).ok()?)))
            .collect();
        states.sort_by(|(_, a), (_, b)| newest_first(a, b));
        let older = states.split_off(MAX_ROOT_STATES as usize);
        let mut kept: Vec<PortableIdentityState> = states.into_iter().map(|(_, s)| s).collect();
        for (digest, state) in older {
            if state.root_changes_covered_by(&kept) {
                sqlx::query("DELETE FROM portable_root_states WHERE user_id = ?1 AND digest = ?2")
                    .bind(user)
                    .bind(&digest)
                    .execute(&mut **tx)
                    .await?;
            } else {
                kept.push(state);
            }
        }
        Ok(())
    }

    /// Add `state`'s revocations that have not lapsed at `now` to its
    /// user's here, each at its earliest, and end the sessions of each
    /// key revoked. How many it added or made earlier, and how many
    /// sessions it ended.
    async fn take_revocations(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        user: &str,
        state: &PortableIdentityState,
        now: i64,
    ) -> Result<(u64, u64), StoreError> {
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
            .bind(user)
            .bind(entry.key.to_string())
            .bind(entry.revoked_at)
            .execute(&mut **tx)
            .await?
            .rows_affected();
            // The sessions the key opened end with it (P7-06): a guest's,
            // and a tied account's opened with the credential.
            let key = entry.key.to_string();
            ended += sqlx::query(
                "UPDATE portable_sessions SET revoked_at = ?3 \
                 WHERE portable_user_id = ?1 AND device_key = ?2 AND revoked_at IS NULL",
            )
            .bind(user)
            .bind(&key)
            .bind(now)
            .execute(&mut **tx)
            .await?
            .rows_affected();
            ended += sqlx::query(
                "UPDATE sessions SET revoked_at = ?3 \
                 WHERE portable_user_id = ?1 AND portable_device_key = ?2 AND revoked_at IS NULL",
            )
            .bind(user)
            .bind(&key)
            .bind(now)
            .execute(&mut **tx)
            .await?
            .rows_affected();
        }
        Ok((revocations, ended))
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
        json.map(|json| serde_json::from_str(&json).map_err(super::decode_error))
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
        let mut tx = self.begin_write().await?;
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

    /// Keep `genesis` for the identity it begins (P7-08), if it is one: the
    /// first seen stays (another of the same id has the same roots).
    /// Only for an identity known here, as states are.
    pub async fn accept_genesis(
        &self,
        genesis: &IdentityGenesis,
        now: i64,
    ) -> Result<PortableUserId, IdentityStateError> {
        genesis.verify().map_err(IdentityStateError::Genesis)?;
        let user = genesis.user();
        let json = serde_json::to_string(genesis).expect("a genesis always serializes");
        let added = sqlx::query(
            "INSERT INTO portable_geneses (user_id, genesis, seen_at, handle) \
             VALUES (?1, ?2, ?3, ?4) ON CONFLICT (user_id) DO NOTHING",
        )
        .bind(user.to_string())
        .bind(json)
        .bind(now)
        .bind(handle_hex(&genesis.handle))
        .execute(&self.pool)
        .await?
        .rows_affected()
            > 0;
        if added {
            tracing::info!(
                user = %user,
                roots = genesis.roots.len(),
                "took in a portable identity's genesis"
            );
        }
        Ok(user)
    }

    /// The identity whose genesis kept here has `handle` (P7-08): what a
    /// security key gives back as its credential's user handle.
    pub async fn identity_by_handle(
        &self,
        handle: &[u8; HANDLE_LEN],
    ) -> Result<Option<PortableUserId>, StoreError> {
        let user: Option<String> = sqlx::query_scalar(
            "SELECT user_id FROM portable_geneses WHERE handle = ?1 ORDER BY seen_at LIMIT 1",
        )
        .bind(handle_hex(handle))
        .fetch_optional(&self.pool)
        .await?;
        user.map(super::parse_id).transpose()
    }

    /// The genesis of `user` kept here, if any.
    pub async fn identity_genesis(
        &self,
        user: PortableUserId,
    ) -> Result<Option<IdentityGenesis>, StoreError> {
        let json: Option<String> =
            sqlx::query_scalar("SELECT genesis FROM portable_geneses WHERE user_id = ?1")
                .bind(user.to_string())
                .fetch_optional(&self.pool)
                .await?;
        json.map(|json| serde_json::from_str(&json).map_err(super::decode_error))
            .transpose()
    }

    /// The roots of `user` (P7-08): from its genesis (`genesis`, if given
    /// and of `user`, else the one kept here, else the phrase root its id
    /// is), and every root-changing state kept here with `states`, each
    /// already verified. `states` are held in memory only.
    pub async fn root_set(
        &self,
        user: PortableUserId,
        genesis: Option<&IdentityGenesis>,
        states: &[PortableIdentityState],
    ) -> Result<RootSet, StoreError> {
        let (kept, mut all) = self.root_states(user, genesis).await?;
        all.extend(states.iter().filter(|s| s.user == user).cloned());
        let genesis = genesis.filter(|g| g.user() == user).or(kept.as_ref());
        Ok(root_set(&user, genesis, &all))
    }

    /// What [`Store::root_set`] works `user`'s roots out from, read once:
    /// the genesis kept here (only when `genesis` is not `user`'s own),
    /// and the root-changing states kept here. For a caller that needs
    /// the set both with and without states of its own.
    pub async fn root_states(
        &self,
        user: PortableUserId,
        genesis: Option<&IdentityGenesis>,
    ) -> Result<(Option<IdentityGenesis>, Vec<PortableIdentityState>), StoreError> {
        let kept = match genesis.filter(|g| g.user() == user) {
            Some(_) => None,
            None => self.identity_genesis(user).await?,
        };
        let rows: Vec<String> =
            sqlx::query_scalar("SELECT state FROM portable_root_states WHERE user_id = ?1")
                .bind(user.to_string())
                .fetch_all(&self.pool)
                .await?;
        let states = rows
            .iter()
            .filter_map(|json| serde_json::from_str(json).ok())
            .collect();
        Ok((kept, states))
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

    /// S2: of two states at one revision, the newest is the one with the
    /// lower digest, whichever came first, so every server hands on the
    /// same one; the revocations of both count.
    #[tokio::test]
    async fn rivals_at_one_revision_keep_the_same_newest_whichever_came_first() {
        let one = state(3, &[device(9)]);
        let other = state(3, &[device(11)]);
        let lower = if one.digest() < other.digest() {
            one.clone()
        } else {
            other.clone()
        };
        for (name, first, second) in [
            ("portable-same-a", &one, &other),
            ("portable-same-b", &other, &one),
        ] {
            let store = test_store(name).await;
            assert!(newest(&store, first).await);
            let taken = store.accept_identity_state(second, T).await.unwrap();
            assert_eq!(taken.revocations, 1);
            assert_eq!(taken.newest, second.same_as(&lower), "{name}");
            assert_eq!(
                store.identity_state(user(7)).await.unwrap().as_ref(),
                Some(&lower),
                "{name}"
            );
            assert!(store.revokes(user(7), &device(9), T).await.unwrap());
            assert!(store.revokes(user(7), &device(11), T).await.unwrap());
            // The same state again changes nothing.
            assert!(!newest(&store, &lower).await);
        }
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

    // P7-08.

    use spoolframe_protocol::portable::soft_key::SoftKey;
    use spoolframe_protocol::portable::{IdentityGenesis, RootKey, StateChanges};

    fn genesis_of(keys: &[&SoftKey]) -> IdentityGenesis {
        let handle = [0x33; 32];
        let roots: Vec<RootKey> = keys.iter().map(|k| k.root()).collect();
        let message = IdentityGenesis::message(&handle, &roots).unwrap();
        IdentityGenesis {
            handle,
            signatures: keys.iter().map(|k| k.assert(&message)).collect(),
            roots,
        }
    }

    fn key_state(genesis: &IdentityGenesis, by: &SoftKey, revision: u64) -> PortableIdentityState {
        PortableIdentityState::signed_by(
            genesis.user(),
            by.root(),
            revision,
            StateChanges {
                revoked: vec![entry(9)],
                ..StateChanges::default()
            },
            T,
            |m| by.assert(m),
        )
        .unwrap()
    }

    /// P7-08: a kept genesis is found by its handle, the one a security
    /// key gives back; and the handle column holds what the genesis JSON
    /// holds, which migration 0025 copied for rows kept before it.
    #[tokio::test]
    async fn a_kept_genesis_is_found_by_its_handle() {
        let store = test_store("portable-genesis-handle").await;
        let (a, b) = (SoftKey::from_seed(3), SoftKey::from_seed(4));
        let genesis = genesis_of(&[&a, &b]);
        let user = store.accept_genesis(&genesis, T).await.unwrap();
        assert_eq!(
            store.identity_by_handle(&genesis.handle).await.unwrap(),
            Some(user)
        );
        assert_eq!(store.identity_by_handle(&[0x44; 32]).await.unwrap(), None);
        let (copied,): (bool,) = sqlx::query_as(
            "SELECT lower(json_extract(genesis, '$.handle')) = handle \
             FROM portable_geneses WHERE user_id = ?1",
        )
        .bind(user.to_string())
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert!(copied);
    }

    #[tokio::test]
    async fn a_key_identity_s_state_is_taken_once_its_genesis_is_here() {
        let store = test_store("portable-key-genesis").await;
        let (a, b) = (SoftKey::from_seed(3), SoftKey::from_seed(4));
        let genesis = genesis_of(&[&a, &b]);
        let user = genesis.user();
        assert!(matches!(
            store
                .accept_identity_state(&key_state(&genesis, &a, 1), T)
                .await,
            Err(IdentityStateError::NotARoot)
        ));
        assert_eq!(store.accept_genesis(&genesis, T).await.unwrap(), user);
        assert_eq!(
            store.identity_genesis(user).await.unwrap(),
            Some(genesis.clone())
        );
        store
            .accept_identity_state(&key_state(&genesis, &a, 1), T)
            .await
            .unwrap();
        assert!(store.revokes(user, &device(9), T).await.unwrap());
        // A key the identity never had signs no state of it.
        let stranger = SoftKey::from_seed(5);
        assert!(matches!(
            store
                .accept_identity_state(&key_state(&genesis, &stranger, 2), T)
                .await,
            Err(IdentityStateError::NotARoot)
        ));
    }

    /// P7-08, Apple: a genesis one key signed in raw CTAP2 (Linux) and
    /// the other through WebAuthn (an iPhone) is kept and read back as it
    /// came, and a state the WebAuthn key signs is taken.
    #[tokio::test]
    async fn a_genesis_and_state_signed_through_webauthn_are_kept() {
        let store = test_store("portable-webauthn-genesis").await;
        let (linux, iphone) = (SoftKey::from_seed(3), SoftKey::from_seed(4));
        let handle = [0x33; 32];
        let roots = vec![linux.root(), iphone.root()];
        let message = IdentityGenesis::message(&handle, &roots).unwrap();
        let genesis = IdentityGenesis {
            handle,
            signatures: vec![linux.assert(&message), iphone.assert_webauthn(&message)],
            roots,
        };
        let user = store.accept_genesis(&genesis, T).await.unwrap();
        assert_eq!(
            store.identity_genesis(user).await.unwrap(),
            Some(genesis.clone())
        );
        let state = PortableIdentityState::signed_by(
            user,
            iphone.root(),
            1,
            StateChanges {
                revoked: vec![entry(9)],
                ..StateChanges::default()
            },
            T,
            |m| iphone.assert_webauthn(m),
        )
        .unwrap();
        store.accept_identity_state(&state, T).await.unwrap();
        assert!(store.revokes(user, &device(9), T).await.unwrap());
        // The same state in the iPhone key's name, signed by the Linux key.
        let forged = PortableIdentityState::signed_by(
            user,
            iphone.root(),
            2,
            StateChanges::default(),
            T,
            |m| linux.assert_webauthn(m),
        )
        .unwrap();
        assert!(store.accept_identity_state(&forged, T).await.is_err());
    }

    #[tokio::test]
    async fn a_genesis_that_does_not_hold_is_not_kept() {
        let store = test_store("portable-bad-genesis").await;
        let (a, b) = (SoftKey::from_seed(3), SoftKey::from_seed(4));
        let mut genesis = genesis_of(&[&a, &b]);
        genesis.signatures.swap(0, 1);
        assert!(matches!(
            store.accept_genesis(&genesis, T).await,
            Err(IdentityStateError::Genesis(_))
        ));
        assert_eq!(store.identity_genesis(genesis.user()).await.unwrap(), None);
    }

    #[tokio::test]
    async fn the_root_states_kept_are_bounded() {
        let store = test_store("portable-root-states-bounded").await;
        let a = SoftKey::from_seed(3);
        let genesis = genesis_of(&[&a]);
        store.accept_genesis(&genesis, T).await.unwrap();
        for revision in 1..=(MAX_ROOT_STATES as u64 + 5) {
            store
                .accept_identity_state(&key_state(&genesis, &a, revision), T)
                .await
                .unwrap();
        }
        let (kept,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM portable_root_states WHERE user_id = ?1")
                .bind(genesis.user().to_string())
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(kept, MAX_ROOT_STATES);
    }

    /// S1: a root removed in a state of one branch is still removed after
    /// the other branch, which never saw it, signs more root-changing
    /// states than are kept: a state whose changes the kept ones do not
    /// carry is not dropped.
    #[tokio::test]
    async fn a_rival_branch_s_removal_outlives_the_root_state_window() {
        use spoolframe_protocol::portable::RemovedRoot;

        let store = test_store("portable-rival-removal").await;
        let (a, b) = (SoftKey::from_seed(3), SoftKey::from_seed(4));
        let genesis = genesis_of(&[&a, &b]);
        let user = genesis.user();
        store.accept_genesis(&genesis, T).await.unwrap();
        let removal = PortableIdentityState::signed_by(
            user,
            a.root(),
            2,
            StateChanges {
                roots_removed: vec![RemovedRoot {
                    root: b.root(),
                    removed_at: T,
                }],
                ..StateChanges::default()
            },
            T,
            |m| a.assert(m),
        )
        .unwrap();
        store.accept_identity_state(&removal, T).await.unwrap();
        assert!(
            !store
                .root_set(user, None, &[])
                .await
                .unwrap()
                .holds(&b.root())
        );
        // The other branch: a's states that never heard of the removal.
        for revision in 2..=(MAX_ROOT_STATES as u64 + 8) {
            store
                .accept_identity_state(&key_state(&genesis, &a, revision), T)
                .await
                .unwrap();
        }
        let roots = store.root_set(user, None, &[]).await.unwrap();
        assert!(!roots.holds(&b.root()), "the removed key is back");
        assert!(roots.holds(&a.root()));
        // Still bounded: the window, and the one state it does not cover.
        let (kept,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM portable_root_states WHERE user_id = ?1")
                .bind(user.to_string())
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(kept, MAX_ROOT_STATES + 1);
    }

    /// A state refused because a removed root adds a key through it still
    /// counts for its revocations: they are one of the identity's roots'
    /// word, and dropping them would leave a rival branch's revocation
    /// unheard here.
    #[tokio::test]
    async fn a_refused_backdated_state_s_revocations_count() {
        use spoolframe_protocol::portable::{
            AddedRoot, RemovedRoot, root_add_message, root_endorse_message,
        };

        let store = test_store("portable-refused-revokes").await;
        let (a, b, c) = (
            SoftKey::from_seed(3),
            SoftKey::from_seed(4),
            SoftKey::from_seed(5),
        );
        let genesis = genesis_of(&[&a, &b]);
        let user = genesis.user();
        store.accept_genesis(&genesis, T).await.unwrap();
        let removal = PortableIdentityState::signed_by(
            user,
            a.root(),
            1,
            StateChanges {
                roots_removed: vec![RemovedRoot {
                    root: b.root(),
                    removed_at: T + 10,
                }],
                ..StateChanges::default()
            },
            T,
            |m| a.assert(m),
        )
        .unwrap();
        store.accept_identity_state(&removal, T).await.unwrap();
        let c_added = AddedRoot {
            root: c.root(),
            added_by: b.root(),
            added_at: T + 5,
            endorsement: b.assert(&root_endorse_message(&user, &c.root(), T + 5)),
            possession: c.assert(&root_add_message(&user, &c.root())),
        };
        let rival = PortableIdentityState::signed_by(
            user,
            b.root(),
            1,
            StateChanges {
                revoked: vec![entry(11)],
                roots_added: vec![c_added],
                ..StateChanges::default()
            },
            T,
            |m| b.assert(m),
        )
        .unwrap();
        assert!(matches!(
            store.accept_identity_state(&rival, T).await,
            Err(IdentityStateError::NotARoot)
        ));
        assert!(store.revokes(user, &device(11), T).await.unwrap());
        assert!(
            !store
                .root_set(user, None, &[])
                .await
                .unwrap()
                .holds(&c.root())
        );
    }

    #[tokio::test]
    async fn a_removed_key_s_backdated_addition_is_refused_once_the_removal_is_here() {
        use spoolframe_protocol::portable::{
            AddedRoot, RemovedRoot, root_add_message, root_endorse_message,
        };

        let store = test_store("portable-backdated").await;
        let (a, b, thief) = (
            SoftKey::from_seed(3),
            SoftKey::from_seed(4),
            SoftKey::from_seed(5),
        );
        let genesis = genesis_of(&[&a]);
        let user = genesis.user();
        store.accept_genesis(&genesis, T).await.unwrap();
        let added = |key: &SoftKey, by: &SoftKey, at: i64| AddedRoot {
            root: key.root(),
            added_by: by.root(),
            added_at: at,
            endorsement: by.assert(&root_endorse_message(&user, &key.root(), at)),
            possession: key.assert(&root_add_message(&user, &key.root())),
        };
        let signed = |by: &SoftKey, revision, add: Vec<AddedRoot>, remove: Vec<RemovedRoot>| {
            PortableIdentityState::signed_by(
                user,
                by.root(),
                revision,
                StateChanges {
                    roots_added: add,
                    roots_removed: remove,
                    ..StateChanges::default()
                },
                T,
                |m| by.assert(m),
            )
            .unwrap()
        };
        let b_added = added(&b, &a, T);
        store
            .accept_identity_state(&signed(&a, 1, vec![b_added.clone()], vec![]), T)
            .await
            .unwrap();
        let b_removed = RemovedRoot {
            root: b.root(),
            removed_at: T + 10,
        };
        store
            .accept_identity_state(
                &signed(&a, 2, vec![b_added.clone()], vec![b_removed.clone()]),
                T,
            )
            .await
            .unwrap();
        // B, stolen and removed, adds the thief's key, claiming it did so
        // before its removal.
        let backdated = signed(
            &b,
            3,
            vec![b_added, added(&thief, &b, T + 5)],
            vec![b_removed],
        );
        assert_eq!(backdated.verify(), Ok(()));
        assert!(matches!(
            store.accept_identity_state(&backdated, T).await,
            Err(IdentityStateError::NotARoot)
        ));
        let roots = store.root_set(user, None, &[]).await.unwrap();
        assert!(roots.holds(&a.root()));
        assert!(!roots.holds(&b.root()));
        assert!(!roots.holds(&thief.root()));
    }
}
