//! Ties between local accounts and portable identities (P7-04,
//! ARCHITECTURE §58). A tie is one row beside the account: nothing keyed
//! on the local user moves when it is made, and severing it deletes that
//! row and nothing else. Whether the identity proved it is the caller's
//! (`crate::client`); this keeps the rules that hold whoever calls: one
//! identity per account, and one account per identity on this server.

use spoolframe_protocol::portable::PortableUserId;

use super::users::{LocalUserId, is_unique_violation};
use super::{Store, StoreError};

/// A local account's tie.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tie {
    pub portable: PortableUserId,
    /// Unix seconds.
    pub tied_at: i64,
    /// What signed it: `root` or `device`.
    pub proof_kind: String,
}

#[derive(Debug, thiserror::Error)]
pub enum TieError {
    /// The account is tied already: it is severed first, never replaced.
    #[error("the account is tied to a portable identity already")]
    AlreadyTied,
    /// Another account here is tied to the identity.
    #[error("the portable identity is tied to another account here")]
    PortableIdTaken,
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<sqlx::Error> for TieError {
    fn from(e: sqlx::Error) -> Self {
        TieError::Store(e.into())
    }
}

impl Store {
    /// Tie `user`'s account to `portable`, proved by a `proof_kind`
    /// signature. One insert: the account's key and the identity's
    /// uniqueness decide a race between two ties, and which one refused
    /// it is read back after.
    pub async fn tie(
        &self,
        user: LocalUserId,
        portable: PortableUserId,
        proof_kind: &str,
    ) -> Result<(), TieError> {
        let inserted = sqlx::query(
            "INSERT INTO portable_ties (local_user_id, portable_user_id, tied_at, proof_kind) \
             VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(user)
        .bind(portable.to_string())
        .bind(self.now())
        .bind(proof_kind)
        .execute(&self.pool)
        .await;
        match inserted {
            Ok(_) => Ok(()),
            Err(e) if is_unique_violation(&e) => {
                if self.tie_of(user).await?.is_some() {
                    Err(TieError::AlreadyTied)
                } else {
                    Err(TieError::PortableIdTaken)
                }
            }
            Err(e) => Err(e.into()),
        }
    }

    /// `user`'s tie, if their account has one.
    pub async fn tie_of(&self, user: LocalUserId) -> Result<Option<Tie>, StoreError> {
        let row: Option<(String, i64, String)> = sqlx::query_as(
            "SELECT portable_user_id, tied_at, proof_kind FROM portable_ties \
             WHERE local_user_id = ?1",
        )
        .bind(user)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(portable, tied_at, proof_kind)| {
            Ok(Tie {
                portable: portable
                    .parse()
                    .map_err(|e| StoreError::Db(sqlx::Error::Decode(Box::new(e))))?,
                tied_at,
                proof_kind,
            })
        })
        .transpose()
    }

    /// The account tied to `portable` here, if any.
    pub async fn tied_account(
        &self,
        portable: PortableUserId,
    ) -> Result<Option<LocalUserId>, StoreError> {
        Ok(sqlx::query_scalar(
            "SELECT local_user_id FROM portable_ties WHERE portable_user_id = ?1",
        )
        .bind(portable.to_string())
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Sever `user`'s tie: the identity it was to, or `None` for no tie.
    /// Only the link row goes.
    pub async fn untie(&self, user: LocalUserId) -> Result<Option<PortableUserId>, StoreError> {
        let removed: Option<String> = sqlx::query_scalar(
            "DELETE FROM portable_ties WHERE local_user_id = ?1 RETURNING portable_user_id",
        )
        .bind(user)
        .fetch_optional(&self.pool)
        .await?;
        removed
            .map(|portable| {
                portable
                    .parse()
                    .map_err(|e| StoreError::Db(sqlx::Error::Decode(Box::new(e))))
            })
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use spoolframe_types::Role;

    use super::super::tests::test_store;
    use super::*;
    use crate::store::portable::tests::user as portable;

    async fn account(store: &Store, name: &str) -> LocalUserId {
        store
            .add_user(name, None, "phc", Role::Consumer)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_tie_is_kept_and_read_back() {
        let store = test_store("tie-kept").await;
        let alice = account(&store, "alice").await;
        assert_eq!(store.tie_of(alice).await.unwrap(), None);
        store.tie(alice, portable(7), "root").await.unwrap();
        let tie = store.tie_of(alice).await.unwrap().unwrap();
        assert_eq!(tie.portable, portable(7));
        assert_eq!(tie.proof_kind, "root");
        assert_eq!(store.tied_account(portable(7)).await.unwrap(), Some(alice));
    }

    #[tokio::test]
    async fn a_tied_account_ties_no_other_identity_until_severed() {
        let store = test_store("tie-once").await;
        let alice = account(&store, "alice").await;
        store.tie(alice, portable(7), "root").await.unwrap();
        assert!(matches!(
            store.tie(alice, portable(8), "root").await,
            Err(TieError::AlreadyTied)
        ));
        // The same identity again is no different: severed first.
        assert!(matches!(
            store.tie(alice, portable(7), "device").await,
            Err(TieError::AlreadyTied)
        ));
        assert_eq!(store.untie(alice).await.unwrap(), Some(portable(7)));
        store.tie(alice, portable(8), "device").await.unwrap();
        assert_eq!(
            store.tie_of(alice).await.unwrap().unwrap().portable,
            portable(8)
        );
    }

    #[tokio::test]
    async fn an_identity_ties_to_one_account_here() {
        let store = test_store("tie-one-account").await;
        let alice = account(&store, "alice").await;
        let bob = account(&store, "bob").await;
        store.tie(alice, portable(7), "root").await.unwrap();
        assert!(matches!(
            store.tie(bob, portable(7), "root").await,
            Err(TieError::PortableIdTaken)
        ));
        assert_eq!(store.tie_of(bob).await.unwrap(), None);
    }

    #[tokio::test]
    async fn severing_nothing_says_so() {
        let store = test_store("untie-none").await;
        let alice = account(&store, "alice").await;
        assert_eq!(store.untie(alice).await.unwrap(), None);
    }
}
