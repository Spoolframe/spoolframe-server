//! Identities at OpenID Connect providers, linked to local users (P3-04).
//! The OIDC flows are `crate::oidc`'s; this is storage, and the rule that
//! a user keeps a way to sign in.

use super::users::LocalUserId;
use super::{Store, StoreError, now};

/// One linked identity.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct IdentityRecord {
    pub user: LocalUserId,
    pub issuer: String,
    pub subject: String,
    /// Its email or username at the provider, for recognition only.
    pub display: Option<String>,
    pub linked_at: i64,
    pub last_used_at: Option<i64>,
}

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    /// The identity is another user's: never moved.
    #[error("that account is linked to another Spoolframe user")]
    LinkedToAnother,
    /// The user has an identity at this provider already.
    #[error("you have an account at this provider linked already; unlink it first")]
    ProviderLinked,
    #[error("no such linked account")]
    NoSuchLink,
    /// Removing it would leave the user with no way to sign in.
    #[error("that is the last way to sign in; set a password or add a passkey first")]
    LastSignInMethod,
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<sqlx::Error> for IdentityError {
    fn from(e: sqlx::Error) -> Self {
        IdentityError::Store(e.into())
    }
}

impl Store {
    /// Link `subject` at `issuer` to `user`. An identity that is anyone's
    /// already is refused, never moved, as is a second one of `user`'s at
    /// the same provider.
    pub async fn link_identity(
        &self,
        user: LocalUserId,
        issuer: &str,
        subject: &str,
        display: Option<&str>,
    ) -> Result<IdentityRecord, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let owner: Option<LocalUserId> = sqlx::query_scalar(
            "SELECT user_id FROM external_identities WHERE issuer = ?1 AND subject = ?2",
        )
        .bind(issuer)
        .bind(subject)
        .fetch_optional(&mut *tx)
        .await?;
        match owner {
            Some(id) if id == user => return Err(IdentityError::ProviderLinked),
            Some(_) => return Err(IdentityError::LinkedToAnother),
            None => {}
        }
        let linked_at = now();
        let inserted = sqlx::query(
            "INSERT INTO external_identities (user_id, issuer, subject, display, linked_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(user)
        .bind(issuer)
        .bind(subject)
        .bind(display)
        .bind(linked_at)
        .execute(&mut *tx)
        .await;
        match inserted {
            Ok(_) => {}
            // The other key: this user's at this issuer.
            Err(sqlx::Error::Database(db)) if db.is_unique_violation() => {
                return Err(IdentityError::ProviderLinked);
            }
            Err(e) => return Err(e.into()),
        }
        tx.commit().await?;
        Ok(IdentityRecord {
            user,
            issuer: issuer.to_owned(),
            subject: subject.to_owned(),
            display: display.map(str::to_owned),
            linked_at,
            last_used_at: None,
        })
    }

    /// The user `subject` at `issuer` is linked to, if anyone: a sign-in
    /// with it is theirs, and is kept as its last use.
    pub async fn identity_signs_in(
        &self,
        issuer: &str,
        subject: &str,
    ) -> Result<Option<LocalUserId>, StoreError> {
        let id: Option<LocalUserId> = sqlx::query_scalar(
            "UPDATE external_identities SET last_used_at = ?3 \
             WHERE issuer = ?1 AND subject = ?2 RETURNING user_id",
        )
        .bind(issuer)
        .bind(subject)
        .bind(now())
        .fetch_optional(&self.pool)
        .await?;
        Ok(id)
    }

    /// The user `subject` at `issuer` is linked to, without counting it as
    /// a use.
    pub async fn identity_owner(
        &self,
        issuer: &str,
        subject: &str,
    ) -> Result<Option<LocalUserId>, StoreError> {
        let id: Option<LocalUserId> = sqlx::query_scalar(
            "SELECT user_id FROM external_identities WHERE issuer = ?1 AND subject = ?2",
        )
        .bind(issuer)
        .bind(subject)
        .fetch_optional(&self.pool)
        .await?;
        Ok(id)
    }

    /// `user`'s linked identities, oldest first.
    pub async fn user_identities(
        &self,
        user: LocalUserId,
    ) -> Result<Vec<IdentityRecord>, StoreError> {
        Ok(sqlx::query_as(
            "SELECT user_id AS user, issuer, subject, display, linked_at, last_used_at \
             FROM external_identities WHERE user_id = ?1 ORDER BY linked_at, issuer",
        )
        .bind(user)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Unlink `user`'s identity at `issuer`, unless it is their last way
    /// to sign in.
    pub async fn unlink_identity(
        &self,
        user: LocalUserId,
        issuer: &str,
    ) -> Result<(), IdentityError> {
        let mut tx = self.pool.begin().await?;
        let (theirs, others): (bool, i64) = sqlx::query_as(
            "SELECT \
               EXISTS (SELECT 1 FROM external_identities WHERE user_id = ?1 AND issuer = ?2), \
               (SELECT count(*) FROM external_identities WHERE user_id = ?1 AND issuer != ?2) \
               + (SELECT count(*) FROM passkeys WHERE user_id = ?1) \
               + (SELECT count(*) FROM password_credentials WHERE user_id = ?1)",
        )
        .bind(user)
        .bind(issuer)
        .fetch_one(&mut *tx)
        .await?;
        if !theirs {
            return Err(IdentityError::NoSuchLink);
        }
        if others == 0 {
            return Err(IdentityError::LastSignInMethod);
        }
        sqlx::query("DELETE FROM external_identities WHERE user_id = ?1 AND issuer = ?2")
            .bind(user)
            .bind(issuer)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use spoolframe_types::Role;

    use super::super::tests::{TestStore, test_store};
    use super::*;

    const ISSUER: &str = "https://id.example.org";

    async fn store_with(test: &str) -> (TestStore, LocalUserId, LocalUserId) {
        let store = test_store(test).await;
        let alice = store
            .add_user("alice", None, "phc", Role::Consumer)
            .await
            .unwrap();
        let bob = store
            .add_user("bob", None, "phc", Role::Consumer)
            .await
            .unwrap();
        (store, alice, bob)
    }

    async fn without_password(store: &TestStore, user: LocalUserId) {
        sqlx::query("DELETE FROM password_credentials WHERE user_id = ?1")
            .bind(user)
            .execute(&store.pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_linked_identity_signs_in_its_user_and_is_listed() {
        let (store, alice, _) = store_with("identity-link").await;
        store
            .link_identity(alice, ISSUER, "sub-1", Some("alice@example.org"))
            .await
            .unwrap();
        assert_eq!(
            store.identity_signs_in(ISSUER, "sub-1").await.unwrap(),
            Some(alice)
        );
        // The same subject at another provider is another identity.
        assert_eq!(
            store
                .identity_signs_in("https://other.example.org", "sub-1")
                .await
                .unwrap(),
            None
        );
        let listed = store.user_identities(alice).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].display.as_deref(), Some("alice@example.org"));
        assert!(listed[0].last_used_at.is_some());
    }

    #[tokio::test]
    async fn an_identity_is_one_user_s_and_never_moves() {
        let (store, alice, bob) = store_with("identity-taken").await;
        store
            .link_identity(alice, ISSUER, "sub-1", None)
            .await
            .unwrap();
        assert!(matches!(
            store.link_identity(bob, ISSUER, "sub-1", None).await,
            Err(IdentityError::LinkedToAnother)
        ));
        assert_eq!(
            store.identity_owner(ISSUER, "sub-1").await.unwrap(),
            Some(alice)
        );
        // One per provider per user.
        assert!(matches!(
            store.link_identity(alice, ISSUER, "sub-2", None).await,
            Err(IdentityError::ProviderLinked)
        ));
    }

    #[tokio::test]
    async fn the_last_way_to_sign_in_is_not_unlinked() {
        let (store, alice, bob) = store_with("identity-last").await;
        store
            .link_identity(alice, ISSUER, "sub-1", None)
            .await
            .unwrap();
        without_password(&store, alice).await;
        assert!(matches!(
            store.unlink_identity(alice, ISSUER).await,
            Err(IdentityError::LastSignInMethod)
        ));
        // A passkey is another way in.
        store
            .add_passkey(alice, "cred-1", "MacBook", "{}", "media.example.org")
            .await
            .unwrap();
        // And the identity counts for the passkey's sake.
        store.remove_passkey(alice, "cred-1").await.unwrap();
        assert!(matches!(
            store.unlink_identity(alice, ISSUER).await,
            Err(IdentityError::LastSignInMethod)
        ));
        store.set_password("alice", "phc", None).await.unwrap();
        store.unlink_identity(alice, ISSUER).await.unwrap();
        assert_eq!(store.user_identities(alice).await.unwrap(), []);
        // Bob has no link to remove.
        assert!(matches!(
            store.unlink_identity(bob, ISSUER).await,
            Err(IdentityError::NoSuchLink)
        ));
    }
}
