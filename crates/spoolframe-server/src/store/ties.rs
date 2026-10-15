//! Ties between local accounts and portable identities (P7-04, P7-09,
//! ARCHITECTURE §58). A tie is one row beside the account: nothing keyed
//! on the local user moves when it is made, and severing it deletes that
//! row and nothing else. Whether the identity proved it is the caller's
//! (`crate::client`); this keeps the rule that holds whoever calls: an
//! account ties to any number of identities, an identity to one account on
//! this server.

use std::collections::HashMap;

use spoolframe_protocol::portable::PortableUserId;

use super::users::LocalUserId;
use super::{Store, StoreError};

/// One of a local account's ties.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tie {
    pub portable: PortableUserId,
    /// Unix seconds.
    pub tied_at: i64,
    /// What signed it: `root` or `device`.
    pub proof_kind: String,
    /// Unix seconds of the identity's last sign-in into the account
    /// through this tie (P7-06); `None` until one.
    pub last_used_at: Option<i64>,
}

/// What [`Store::tie`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tied {
    /// The tie is new.
    New,
    /// The identity was tied to this account already: nothing changed,
    /// its first proof and date stand.
    Already,
}

#[derive(Debug, thiserror::Error)]
pub enum TieError {
    /// Another account here is tied to the identity.
    #[error("the portable identity is tied to another account here")]
    PortableIdTaken,
    #[error(transparent)]
    Store(#[from] StoreError),
}

type TieRow = (LocalUserId, String, i64, String, Option<i64>);

/// The columns a [`TieRow`] reads, for `concat!`: sqlx takes only a
/// static string as SQL.
macro_rules! tie_columns {
    () => {
        "SELECT local_user_id, portable_user_id, tied_at, proof_kind, last_used_at \
         FROM portable_ties "
    };
}

fn tie_from((_, portable, tied_at, proof_kind, last_used_at): TieRow) -> Result<Tie, StoreError> {
    Ok(Tie {
        portable: super::parse_id(portable)?,
        tied_at,
        proof_kind,
        last_used_at,
    })
}

impl Store {
    /// Tie `user`'s account to `portable`, proved by a `proof_kind`
    /// signature. The account may have other ties; the identity may not
    /// be tied to another account here ([`TieError::PortableIdTaken`]),
    /// and tied to this one already it is [`Tied::Already`], changing
    /// nothing. One write transaction: the insert and the read of who
    /// holds the identity see the same table.
    pub async fn tie(
        &self,
        user: LocalUserId,
        portable: PortableUserId,
        proof_kind: &str,
    ) -> Result<Tied, TieError> {
        let mut tx = self.begin_write().await.map_err(StoreError::from)?;
        let inserted = sqlx::query(
            "INSERT INTO portable_ties (local_user_id, portable_user_id, tied_at, proof_kind) \
             VALUES (?1, ?2, ?3, ?4) ON CONFLICT (portable_user_id) DO NOTHING",
        )
        .bind(user)
        .bind(portable.to_string())
        .bind(self.now())
        .bind(proof_kind)
        .execute(&mut *tx)
        .await
        .map_err(StoreError::from)?
        .rows_affected();
        let tied = if inserted == 1 {
            Tied::New
        } else {
            let holder: LocalUserId = sqlx::query_scalar(
                "SELECT local_user_id FROM portable_ties WHERE portable_user_id = ?1",
            )
            .bind(portable.to_string())
            .fetch_one(&mut *tx)
            .await
            .map_err(StoreError::from)?;
            if holder != user {
                return Err(TieError::PortableIdTaken);
            }
            Tied::Already
        };
        tx.commit().await.map_err(StoreError::from)?;
        Ok(tied)
    }

    /// `user`'s ties, the oldest first.
    pub async fn ties_of(&self, user: LocalUserId) -> Result<Vec<Tie>, StoreError> {
        let rows: Vec<TieRow> = sqlx::query_as(concat!(
            tie_columns!(),
            "WHERE local_user_id = ?1 ORDER BY tied_at, portable_user_id"
        ))
        .bind(user)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(tie_from).collect()
    }

    /// Every account's ties, each account's the oldest first: the user
    /// list's, in one read.
    pub async fn all_ties(&self) -> Result<HashMap<LocalUserId, Vec<Tie>>, StoreError> {
        let rows: Vec<TieRow> = sqlx::query_as(concat!(
            tie_columns!(),
            "ORDER BY tied_at, portable_user_id"
        ))
        .fetch_all(&self.pool)
        .await?;
        let mut ties: HashMap<LocalUserId, Vec<Tie>> = HashMap::new();
        for row in rows {
            ties.entry(row.0).or_default().push(tie_from(row)?);
        }
        Ok(ties)
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

    /// `portable` signed in to `user`'s account through its tie, now.
    /// Nothing for a tie severed since.
    pub async fn tie_used(
        &self,
        user: LocalUserId,
        portable: PortableUserId,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE portable_ties SET last_used_at = ?3 \
             WHERE local_user_id = ?1 AND portable_user_id = ?2",
        )
        .bind(user)
        .bind(portable.to_string())
        .bind(self.now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Sever `user`'s tie to `portable`: whether there was one. Only that
    /// link row goes; the account's other ties stay.
    pub async fn untie(
        &self,
        user: LocalUserId,
        portable: PortableUserId,
    ) -> Result<bool, StoreError> {
        let removed = sqlx::query(
            "DELETE FROM portable_ties WHERE local_user_id = ?1 AND portable_user_id = ?2",
        )
        .bind(user)
        .bind(portable.to_string())
        .execute(&self.pool)
        .await?;
        Ok(removed.rows_affected() == 1)
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
        assert_eq!(store.ties_of(alice).await.unwrap(), []);
        assert_eq!(
            store.tie(alice, portable(7), "root").await.unwrap(),
            Tied::New
        );
        let ties = store.ties_of(alice).await.unwrap();
        assert_eq!(ties.len(), 1);
        assert_eq!(ties[0].portable, portable(7));
        assert_eq!(ties[0].proof_kind, "root");
        assert_eq!(ties[0].last_used_at, None);
        assert_eq!(store.tied_account(portable(7)).await.unwrap(), Some(alice));
    }

    #[tokio::test]
    async fn an_account_ties_several_identities() {
        let store = test_store("tie-several").await;
        let alice = account(&store, "alice").await;
        store.tie(alice, portable(7), "root").await.unwrap();
        store.clock().advance(std::time::Duration::from_secs(5));
        store.tie(alice, portable(8), "device").await.unwrap();
        assert_eq!(store.tied_account(portable(7)).await.unwrap(), Some(alice));
        assert_eq!(store.tied_account(portable(8)).await.unwrap(), Some(alice));
        let ties: Vec<_> = store
            .ties_of(alice)
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.portable)
            .collect();
        assert_eq!(ties, [portable(7), portable(8)]);
        assert_eq!(store.all_ties().await.unwrap()[&alice].len(), 2);
    }

    #[tokio::test]
    async fn tying_an_identity_tied_here_again_changes_nothing() {
        let store = test_store("tie-again").await;
        let alice = account(&store, "alice").await;
        store.tie(alice, portable(7), "root").await.unwrap();
        let before = store.ties_of(alice).await.unwrap();
        store.clock().advance(std::time::Duration::from_secs(5));
        assert_eq!(
            store.tie(alice, portable(7), "device").await.unwrap(),
            Tied::Already
        );
        assert_eq!(store.ties_of(alice).await.unwrap(), before);
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
        assert_eq!(store.ties_of(bob).await.unwrap(), []);
    }

    #[tokio::test]
    async fn severing_one_tie_leaves_the_others() {
        let store = test_store("untie-one").await;
        let alice = account(&store, "alice").await;
        store.tie(alice, portable(7), "root").await.unwrap();
        store.tie(alice, portable(8), "root").await.unwrap();
        assert!(store.untie(alice, portable(7)).await.unwrap());
        let left: Vec<_> = store
            .ties_of(alice)
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.portable)
            .collect();
        assert_eq!(left, [portable(8)]);
        assert_eq!(store.tied_account(portable(7)).await.unwrap(), None);
    }

    #[tokio::test]
    async fn severing_nothing_says_so() {
        let store = test_store("untie-none").await;
        let alice = account(&store, "alice").await;
        let bob = account(&store, "bob").await;
        assert!(!store.untie(alice, portable(7)).await.unwrap());
        // Another account's tie is not this one's to sever.
        store.tie(bob, portable(7), "root").await.unwrap();
        assert!(!store.untie(alice, portable(7)).await.unwrap());
        assert_eq!(store.tied_account(portable(7)).await.unwrap(), Some(bob));
    }

    #[tokio::test]
    async fn a_sign_in_through_a_tie_is_its_last_use() {
        let store = test_store("tie-used").await;
        let alice = account(&store, "alice").await;
        store.tie(alice, portable(7), "root").await.unwrap();
        store.tie(alice, portable(8), "root").await.unwrap();
        store.clock().advance(std::time::Duration::from_secs(60));
        // Whole seconds of the system clock: the use may land on the next
        // second after `before`.
        let before = store.now();
        store.tie_used(alice, portable(8)).await.unwrap();
        let after = store.now();
        let used = |id| {
            let ties = store.ties_of(alice);
            async move {
                ties.await
                    .unwrap()
                    .into_iter()
                    .find(|t| t.portable == id)
                    .unwrap()
                    .last_used_at
            }
        };
        assert_eq!(used(portable(7)).await, None);
        let last = used(portable(8)).await.expect("the tie's use is recorded");
        assert!(
            (before..=after).contains(&last),
            "{before} ≤ {last} ≤ {after}"
        );
    }

    /// Before migration 0028 an account had one tie at most: each is
    /// still there after it, not yet used.
    #[tokio::test]
    async fn the_migration_keeps_every_tie() {
        let dir = super::super::tests::state_dir("ties-migrated").await;
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(dir.as_ref().join(super::super::DB_FILE))
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = sqlx::pool::PoolOptions::<sqlx::Sqlite>::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        let mut before = sqlx::migrate!("./migrations");
        before.migrations = before
            .iter()
            .filter(|m| m.version < 28)
            .cloned()
            .collect::<Vec<_>>()
            .into();
        before.run(&pool).await.unwrap();
        let (a, b) = (
            "01900000-0000-7000-8000-00000000000a",
            "01900000-0000-7000-8000-00000000000b",
        );
        sqlx::query(
            "INSERT INTO users (id, username, created_at) VALUES (?1, 'alice', 1), (?2, 'bob', 1)",
        )
        .bind(a)
        .bind(b)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO portable_ties (local_user_id, portable_user_id, tied_at, proof_kind) \
               VALUES (?1, ?3, 10, 'root'), (?2, ?4, 20, 'device')",
        )
        .bind(a)
        .bind(b)
        .bind(portable(7).to_string())
        .bind(portable(8).to_string())
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        let store = Store::open(dir.as_ref()).await.unwrap();
        let rows: Vec<(String, String, i64, String, Option<i64>)> = sqlx::query_as(
            "SELECT local_user_id, portable_user_id, tied_at, proof_kind, last_used_at \
             FROM portable_ties ORDER BY tied_at",
        )
        .fetch_all(&store.pool)
        .await
        .unwrap();
        store.pool.close().await;
        assert_eq!(
            rows,
            [
                (a.into(), portable(7).to_string(), 10, "root".into(), None),
                (b.into(), portable(8).to_string(), 20, "device".into(), None),
            ]
        );
    }
}
