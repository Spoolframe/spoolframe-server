//! Rival identity states (2026-10-12). Two holders of one root (a phone
//! with the phrase and a security key, or two devices the phrase root is
//! synced to) can each sign a state from the same revision. Keeping only
//! the higher one, and signing the next from it, dropped what the other
//! said: a security key removed on the losing branch came back as a root.
//!
//! So a device keeps **heads**: the newest state (the highest revision,
//! then the lower [`PortableIdentityState::digest`], as servers pick it),
//! and a rival the newest does not cover, at most [`MAX_HEADS`] in all,
//! which is what a sign-in carries. A state is *covered* when the other
//! heads together revoke every key it revokes that has not lapsed, add
//! every root it adds, and remove every root it removes, no later; a
//! covered state adds nothing, and is not kept. The roots are worked out
//! over every head.
//!
//! The next state a holder signs is over the **merge** of all heads, at
//! one past the highest revision: every revocation (each at its earliest,
//! lapsed ones left out, as before), every root added and every root
//! removed (each at its earliest). All three only grow, so the merge is
//! the same in any order, and it loses nothing; a holder of the phrase
//! root signs it as soon as a rival arrives ([`Portable::accept_state`]),
//! without asking. A security key holder merges with its next root
//! action, or [`Portable::reconcile`].
//!
//! One thing is left out of a merge: a root added, by its adder's word,
//! after that adder's removal. It is not a member anyway, and a server
//! that holds the removal refuses a state that brings it
//! ([`RootSet::refuses_additions`]), with the merge's revocations.

use serde::Deserialize;
use spoolframe_protocol::messages::MAX_SIGN_IN_STATES;
use spoolframe_protocol::portable::newest_first;

#[cfg(doc)]
use super::{Portable, RootSet};
use super::{
    PortableIdentityState, PortableUserId, Revoked, RevokedKey, RootKey, RootSigner, StateChanges,
    revocation_lapsed, unix_now,
};
use crate::error::SdkError;
use crate::storage::{Storage, StorageError, keys};

/// The most heads a device keeps of one identity: what a sign-in carries.
pub const MAX_HEADS: usize = MAX_SIGN_IN_STATES;

impl super::Portable {
    /// The heads of identity `id` here, newest first: the newest state,
    /// and a rival the newest does not cover, if any (at most
    /// [`MAX_HEADS`]). What a sign-in carries. Empty if it has none.
    pub async fn identity_states(
        &self,
        id: PortableUserId,
    ) -> Result<Vec<PortableIdentityState>, SdkError> {
        Ok(load_heads(self.storage.as_ref(), id).await?)
    }

    /// With more than one head here, sign one state over all of them with
    /// `signer`, at one past the highest revision, and keep it as the
    /// only head; carry it to servers. `None` when there is one head or
    /// none: nothing to merge. A device that holds the phrase root does
    /// this itself as a rival arrives; a security key identity's device
    /// calls it with the key (or merges with its next root action).
    /// Refused as [`StateError::TooManyRevoked`] or
    /// [`StateError::TooManyRootChanges`] if the merge is over the
    /// protocol's limits.
    ///
    /// [`StateError::TooManyRevoked`]: super::StateError::TooManyRevoked
    /// [`StateError::TooManyRootChanges`]: super::StateError::TooManyRootChanges
    pub async fn reconcile(
        &self,
        id: PortableUserId,
        signer: RootSigner<'_>,
    ) -> Result<Option<PortableIdentityState>, SdkError> {
        if load_heads(self.storage.as_ref(), id).await?.len() < 2 {
            return Ok(None);
        }
        let state = self.new_state(id, signer, None, |_| {}, unix_now()).await?;
        tracing::info!(%id, revision = state.revision, "merged rival portable identity states");
        Ok(Some(state))
    }

    /// Merge the heads of `id` with the phrase root, if it is here and
    /// still a root; else leave them for [`Self::reconcile`]. A merge the
    /// limits refuse leaves both heads, and the log says so.
    pub(super) async fn merge_heads_if_root_here(
        &self,
        id: PortableUserId,
    ) -> Result<(), SdkError> {
        match self.load_root(id).await {
            Ok(_) => {}
            Err(SdkError::UnknownIdentity(_)) => return Ok(()),
            Err(e) => return Err(e),
        }
        if !self.roots(id).await?.holds(&RootKey::phrase_of(&id)) {
            return Ok(());
        }
        match self.reconcile(id, RootSigner::Phrase).await {
            Ok(_) => Ok(()),
            Err(SdkError::IdentityState(e)) => {
                tracing::warn!(%id, error = %e, "rival portable identity states cannot be merged");
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

/// Whether `by` carry everything `state` says ([the module](self)).
fn covered(state: &PortableIdentityState, by: &[PortableIdentityState], now: i64) -> bool {
    state.root_changes_covered_by(by)
        && state
            .revoked
            .iter()
            .filter(|r| !revocation_lapsed(r.revoked_at, now))
            .all(|r| by.iter().any(|h| h.revokes(&r.key)))
}

/// `heads` with `state` taken in, newest first: the newest always kept,
/// each other one only if the rest do not cover it. `None` if `state` is
/// one of them already, or they cover it: nothing changes.
pub(super) fn take_in(
    heads: &[PortableIdentityState],
    state: &PortableIdentityState,
    now: i64,
) -> Option<Vec<PortableIdentityState>> {
    if heads.iter().any(|h| h.same_as(state)) {
        return None;
    }
    let mut all = heads.to_vec();
    all.push(state.clone());
    all.sort_by(newest_first);
    // Oldest first, so of two that cover each other the newer stays.
    for i in (1..all.len()).rev() {
        let others: Vec<PortableIdentityState> = all
            .iter()
            .enumerate()
            .filter(|&(j, _)| j != i)
            .map(|(_, s)| s.clone())
            .collect();
        if covered(&all[i], &others, now) {
            all.remove(i);
        }
    }
    if all.len() > MAX_HEADS {
        // Only a device that cannot sign gets here (a holder of the
        // phrase root merges at two): its revoked keys are kept apart, and
        // whoever signs next for the identity merges what reaches it.
        tracing::warn!(
            id = %state.user,
            heads = all.len(),
            "more rival portable identity states than are kept; keeping the newest"
        );
        all.truncate(MAX_HEADS);
    }
    all.iter().any(|h| h.same_as(state)).then_some(all)
}

/// What the next state over `heads` says before its own change: every
/// revocation in `union` (which holds the heads') not lapsed at `now`,
/// and every root added and removed in any head, less an addition made,
/// by its adder's word, after that adder's removal.
pub(super) fn merged_changes(
    heads: &[PortableIdentityState],
    union: &Revoked,
    now: i64,
) -> StateChanges {
    let roots_removed: Vec<_> = heads
        .iter()
        .flat_map(|h| h.roots_removed.iter().cloned())
        .collect();
    let removed_at = |root: &RootKey| {
        roots_removed
            .iter()
            .filter(|r| r.root == *root)
            .map(|r| r.removed_at)
            .min()
    };
    let mut roots_added = Vec::new();
    for added in heads.iter().flat_map(|h| &h.roots_added) {
        let after_removal = removed_at(&added.added_by).is_some_and(|at| added.added_at > at);
        if !after_removal
            && roots_added
                .iter()
                .all(|a: &super::AddedRoot| a.root != added.root)
        {
            roots_added.push(added.clone());
        }
    }
    StateChanges {
        revoked: union
            .iter()
            .filter(|&(_, &at)| !revocation_lapsed(at, now))
            .map(|(&key, &revoked_at)| RevokedKey { key, revoked_at })
            .collect(),
        roots_added,
        roots_removed,
    }
}

/// `portable-state/<id>` as stored: one state (as before heads), or more.
#[derive(Deserialize)]
#[serde(untagged)]
enum Stored {
    One(Box<PortableIdentityState>),
    Many(Vec<PortableIdentityState>),
}

/// The heads of `id` here, newest first; unreadable is an error, never
/// "none".
pub(super) async fn load_heads(
    storage: &dyn Storage,
    id: PortableUserId,
) -> Result<Vec<PortableIdentityState>, StorageError> {
    let Some(bytes) = storage.data(&keys::portable_state(&id.to_string())).await? else {
        return Ok(Vec::new());
    };
    let stored: Stored = serde_json::from_slice(&bytes)
        .map_err(|e| StorageError(format!("the identity state of {id} cannot be read: {e}")))?;
    let mut heads = match stored {
        Stored::One(state) => vec![*state],
        Stored::Many(heads) => heads,
    };
    if heads.iter().any(|s| s.user != id) {
        return Err(StorageError(format!(
            "the identity state stored for {id} is another identity's"
        )));
    }
    heads.sort_by(newest_first);
    Ok(heads)
}

/// Keep `heads` as those of `id`: one as a single state, as before, so
/// what an identity with no rival stores does not change; none removes
/// them.
pub(super) async fn save_heads(
    storage: &dyn Storage,
    id: PortableUserId,
    heads: &[PortableIdentityState],
) -> Result<(), StorageError> {
    let bytes = match heads {
        [] => None,
        [one] => Some(serde_json::to_vec(one).expect("an identity state always serializes")),
        many => Some(serde_json::to_vec(many).expect("identity states always serialize")),
    };
    storage
        .set_data(&keys::portable_state(&id.to_string()), bytes)
        .await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ed25519_dalek::SigningKey;

    use super::*;
    use crate::portable::{DevicePublicKey, Portable, RootKey, SoftSecurityKey, public_key};
    use crate::storage::MemoryStorage;

    fn portable() -> Portable {
        Portable::new(Arc::new(MemoryStorage::default()))
    }

    fn device(n: u8) -> DevicePublicKey {
        public_key(&SigningKey::from_bytes(&[n; 32]))
    }

    /// Two devices that hold one phrase root (synced, or the phrase
    /// recovered on both), with security key `k` added by the first and
    /// that state known to both; and `k`'s root.
    async fn two_holders() -> (Portable, Portable, PortableUserId, SoftSecurityKey, RootKey) {
        let a = portable();
        let (id, phrase) = a.create("Sam").await.unwrap();
        let k = SoftSecurityKey::new();
        let added = a.add_root(id, RootSigner::Phrase, &k, "K").await.unwrap();
        let b = portable();
        b.recover(&phrase.to_string(), "Sam").await.unwrap();
        b.accept_state(added).await.unwrap();
        let k_root = k.credentials()[0].root();
        (a, b, id, k, k_root)
    }

    /// A device that is not a root holder, with a credential for `id`
    /// from `holder`.
    async fn device_of(holder: &Portable, id: PortableUserId) -> Portable {
        let tv = portable();
        let key = tv.device_key(id).await.unwrap();
        let credential = holder.issue_credential(id, key, None).await.unwrap();
        tv.install_credential(credential).await.unwrap();
        tv
    }

    /// A security key removed on one branch and a device revoked on the
    /// other, at one revision: each holder takes in the other's and signs
    /// one state over both, at one past it, with the removal and the
    /// revocation; the next state keeps both.
    #[tokio::test]
    async fn a_removal_and_a_revocation_on_rival_branches_both_survive_the_merge() {
        let (a, b, id, _k, k_root) = two_holders().await;
        let removed = a
            .remove_root(id, RootSigner::Phrase, &k_root)
            .await
            .unwrap();
        let revoked = b.revoke(id, device(1)).await.unwrap();
        assert_eq!((removed.revision, revoked.revision), (2, 2));
        b.accept_state(removed.clone()).await.unwrap();
        a.accept_state(revoked.clone()).await.unwrap();
        for holder in [&a, &b] {
            assert!(!holder.roots(id).await.unwrap().holds(&k_root));
            let heads = holder.identity_states(id).await.unwrap();
            assert_eq!(heads.len(), 1, "merged into one: {heads:?}");
            let merged = &heads[0];
            assert_eq!(merged.revision, 3);
            assert!(merged.roots_removed.iter().any(|r| r.root == k_root));
            assert!(merged.revokes(&device(1)));
            let next = holder.revoke(id, device(2)).await.unwrap();
            assert_eq!(next.revision, 4);
            assert!(next.roots_removed.iter().any(|r| r.root == k_root));
            assert!(next.revokes(&device(1)) && next.revokes(&device(2)));
        }
    }

    /// The same with the revocation signed by the security key itself, on
    /// the branch that has not seen its removal: the merge still removes
    /// it, and keeps its revocation.
    #[tokio::test]
    async fn a_revocation_by_a_key_removed_on_the_other_branch_survives_the_merge() {
        let (a, b, id, k, k_root) = two_holders().await;
        let removed = a
            .remove_root(id, RootSigner::Phrase, &k_root)
            .await
            .unwrap();
        let revoked = b
            .revoke_by(id, RootSigner::Key(&k), device(1))
            .await
            .unwrap();
        a.accept_state(revoked).await.unwrap();
        b.accept_state(removed).await.unwrap();
        for holder in [&a, &b] {
            let heads = holder.identity_states(id).await.unwrap();
            assert_eq!(heads.len(), 1, "{heads:?}");
            assert!(heads[0].revokes(&device(1)));
            assert!(heads[0].roots_removed.iter().any(|r| r.root == k_root));
            assert!(!holder.roots(id).await.unwrap().holds(&k_root));
            // And the removed key acts no more.
            let refused = holder
                .revoke_by(id, RootSigner::Key(&k), device(2))
                .await
                .unwrap_err();
            assert_eq!(refused, SdkError::NotARoot);
        }
    }

    /// A rival branch on which a security key, removed here, adds another
    /// key and revokes a device: the state is refused (the anti-backdating
    /// rule), but its revocation is kept, and the next state carries it.
    #[tokio::test]
    async fn a_refused_rival_s_revocations_are_kept() {
        let (a, b, id, k, k_root) = two_holders().await;
        a.remove_root(id, RootSigner::Phrase, &k_root)
            .await
            .unwrap();
        let k3 = SoftSecurityKey::new();
        b.add_root(id, RootSigner::Key(&k), &k3, "K3")
            .await
            .unwrap();
        let revoked = b
            .revoke_by(id, RootSigner::Key(&k), device(1))
            .await
            .unwrap();
        assert!(matches!(
            a.accept_state(revoked).await,
            Err(SdkError::IdentityState(_))
        ));
        assert_eq!(
            a.issue_credential(id, device(1), None).await.unwrap_err(),
            SdkError::DeviceKeyRevoked
        );
        let next = a.revoke(id, device(2)).await.unwrap();
        assert!(next.revokes(&device(1)));
    }

    /// A device that does not hold the root keeps a lower-revision rival
    /// that removes a root as a second head, and carries both; its roots
    /// count the removal.
    #[tokio::test]
    async fn a_lower_revision_rival_that_removes_a_root_is_kept_as_a_head() {
        let (a, b, id, _k, k_root) = two_holders().await;
        let tv = device_of(&a, id).await;
        for n in 1..=3 {
            let state = a.revoke(id, device(n)).await.unwrap();
            tv.accept_state(state).await.unwrap();
        }
        let removed = b
            .remove_root(id, RootSigner::Phrase, &k_root)
            .await
            .unwrap();
        assert_eq!(removed.revision, 2);
        assert!(tv.accept_state(removed.clone()).await.unwrap());
        assert!(!tv.roots(id).await.unwrap().holds(&k_root));
        let heads = tv.identity_states(id).await.unwrap();
        assert_eq!(heads.len(), 2, "{heads:?}");
        assert_eq!(heads[0].revision, 4, "the newest first");
        assert!(heads[1].same_as(&removed));
    }

    /// A state the heads cover (an older one of the same line) is not
    /// kept, nor is the same state twice.
    #[tokio::test]
    async fn a_state_the_heads_cover_is_not_kept() {
        let (a, _b, id, _k, _) = two_holders().await;
        let tv = device_of(&a, id).await;
        let first = a.revoke(id, device(1)).await.unwrap();
        let second = a.revoke(id, device(2)).await.unwrap();
        assert!(tv.accept_state(second.clone()).await.unwrap());
        assert!(!tv.accept_state(first).await.unwrap());
        assert!(!tv.accept_state(second.clone()).await.unwrap());
        assert_eq!(tv.identity_states(id).await.unwrap(), [second]);
    }

    /// A newer state that says less than the one here (a rival that never
    /// saw a revocation) is kept as the newest, with the one here as its
    /// rival; a holder of the root merges them at one past the newer.
    #[tokio::test]
    async fn a_higher_revision_that_says_less_keeps_the_richer_as_a_rival() {
        let (a, b, id, _k, _) = two_holders().await;
        let tv = device_of(&a, id).await;
        let revoked = a.revoke(id, device(1)).await.unwrap();
        tv.accept_state(revoked.clone()).await.unwrap();
        b.revoke(id, device(2)).await.unwrap();
        let ahead = b.revoke(id, device(3)).await.unwrap();
        assert_eq!(ahead.revision, 3);
        assert!(tv.accept_state(ahead.clone()).await.unwrap());
        let heads = tv.identity_states(id).await.unwrap();
        assert_eq!(heads.len(), 2);
        assert!(heads[0].same_as(&ahead) && heads[1].same_as(&revoked));
        // The device the phrase is on merges at once.
        a.accept_state(ahead).await.unwrap();
        let merged = a.identity_states(id).await.unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].revision, 4);
        for n in 1..=3 {
            assert!(merged[0].revokes(&device(n)), "{n}");
        }
    }

    /// A key identity's device cannot merge without a touch: it keeps both
    /// heads until a root action, or `reconcile` with the key, signs one.
    #[tokio::test]
    async fn a_key_identity_merges_its_heads_with_a_touch() {
        let desktop = portable();
        let (k1, k2) = (SoftSecurityKey::new(), SoftSecurityKey::new());
        let id = desktop.create_with_keys(&[&k1, &k2], "Sam").await.unwrap();
        let laptop = portable();
        laptop
            .import_record(desktop.identity_record(id).await.unwrap(), "Sam")
            .await
            .unwrap();
        let one = desktop
            .revoke_by(id, RootSigner::Key(&k1), device(1))
            .await
            .unwrap();
        let other = laptop
            .revoke_by(id, RootSigner::Key(&k2), device(2))
            .await
            .unwrap();
        desktop.accept_state(other).await.unwrap();
        assert_eq!(desktop.identity_states(id).await.unwrap().len(), 2);
        assert!(one.revision == 1);
        let merged = desktop
            .reconcile(id, RootSigner::Key(&k1))
            .await
            .unwrap()
            .expect("two heads to merge");
        assert_eq!(merged.revision, 2);
        assert!(merged.revokes(&device(1)) && merged.revokes(&device(2)));
        assert_eq!(desktop.identity_states(id).await.unwrap(), [merged]);
        // One head: nothing to merge.
        assert_eq!(
            desktop.reconcile(id, RootSigner::Key(&k1)).await.unwrap(),
            None
        );
    }
}
