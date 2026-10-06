//! Passkeys as stored (P3-05). The WebAuthn ceremonies are
//! `crate::passkeys`'; this is storage, and the rule that a user keeps a
//! way to sign in.

use super::users::{LocalUserId, UserError, add_first_user};
use super::{Store, StoreError};

/// One passkey.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct PasskeyRecord {
    /// base64url.
    pub credential_id: String,
    pub user: LocalUserId,
    pub name: String,
    /// webauthn-rs' credential, as JSON.
    pub credential: String,
    /// The relying-party ID it was made under.
    pub rp_id: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

/// A passkey with whose it is, for the log.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct OwnedPasskey {
    pub username: String,
    pub name: String,
    pub rp_id: String,
}

/// A passkey just made for a user who does not exist yet: the first
/// admin's, at setup (P3-01).
#[derive(Debug, Clone, Copy)]
pub struct FirstPasskey<'a> {
    /// The WebAuthn user handle it was made with.
    pub handle: &'a str,
    pub credential_id: &'a str,
    /// As [`passkey_name`] gives it.
    pub name: &'a str,
    pub credential: &'a str,
    pub rp_id: &'a str,
}

#[derive(Debug, thiserror::Error)]
pub enum PasskeyError {
    #[error("no such passkey")]
    NoSuchPasskey,
    /// The credential is already someone's passkey.
    #[error("that passkey is already registered")]
    Exists,
    #[error("a passkey's name is 1 to {MAX_NAME_LEN} characters")]
    InvalidName,
    /// Removing it would leave the user with no way to sign in.
    #[error("that is the last way to sign in; set a password first")]
    LastSignInMethod,
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<sqlx::Error> for PasskeyError {
    fn from(e: sqlx::Error) -> Self {
        PasskeyError::Store(e.into())
    }
}

/// The longest name a passkey can have.
pub const MAX_NAME_LEN: usize = 64;

/// `name` as stored: trimmed, and not empty or longer than
/// [`MAX_NAME_LEN`].
pub fn passkey_name(name: &str) -> Result<&str, PasskeyError> {
    let name = name.trim();
    if (1..=MAX_NAME_LEN).contains(&name.chars().count()) {
        Ok(name)
    } else {
        Err(PasskeyError::InvalidName)
    }
}

impl Store {
    /// `user`'s WebAuthn user handle: the one they have, or `fresh` from
    /// now on.
    pub async fn passkey_user_handle(
        &self,
        user: LocalUserId,
        fresh: &str,
    ) -> Result<String, StoreError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO passkey_users (user_id, handle) VALUES (?1, ?2) ON CONFLICT DO NOTHING",
        )
        .bind(user)
        .bind(fresh)
        .execute(&mut *tx)
        .await?;
        let handle = sqlx::query_scalar("SELECT handle FROM passkey_users WHERE user_id = ?1")
            .bind(user)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(handle)
    }

    /// The user whose WebAuthn user handle is `handle`.
    pub async fn passkey_handle_user(
        &self,
        handle: &str,
    ) -> Result<Option<LocalUserId>, StoreError> {
        Ok(
            sqlx::query_scalar("SELECT user_id FROM passkey_users WHERE handle = ?1")
                .bind(handle)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// Keep a new passkey of `user`'s, made under `rp_id`.
    pub async fn add_passkey(
        &self,
        user: LocalUserId,
        credential_id: &str,
        name: &str,
        credential: &str,
        rp_id: &str,
    ) -> Result<PasskeyRecord, PasskeyError> {
        let name = passkey_name(name)?;
        let created_at = self.now();
        let inserted = sqlx::query(
            "INSERT INTO passkeys (credential_id, user_id, name, credential, rp_id, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(credential_id)
        .bind(user)
        .bind(name)
        .bind(credential)
        .bind(rp_id)
        .bind(created_at)
        .execute(&self.pool)
        .await;
        match inserted {
            Ok(_) => Ok(PasskeyRecord {
                credential_id: credential_id.to_owned(),
                user,
                name: name.to_owned(),
                credential: credential.to_owned(),
                rp_id: rp_id.to_owned(),
                created_at,
                last_used_at: None,
            }),
            Err(sqlx::Error::Database(db)) if db.is_unique_violation() => Err(PasskeyError::Exists),
            Err(e) => Err(e.into()),
        }
    }

    /// `user`'s passkeys, oldest first.
    pub async fn list_passkeys(&self, user: LocalUserId) -> Result<Vec<PasskeyRecord>, StoreError> {
        Ok(sqlx::query_as(
            "SELECT credential_id, user_id AS user, name, credential, rp_id, created_at, \
                    last_used_at \
             FROM passkeys WHERE user_id = ?1 ORDER BY created_at, credential_id",
        )
        .bind(user)
        .fetch_all(&self.pool)
        .await?)
    }

    /// The passkey with this credential id, whoever's it is.
    pub async fn passkey(&self, credential_id: &str) -> Result<Option<PasskeyRecord>, StoreError> {
        Ok(sqlx::query_as(
            "SELECT credential_id, user_id AS user, name, credential, rp_id, created_at, \
                    last_used_at \
             FROM passkeys WHERE credential_id = ?1",
        )
        .bind(credential_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Add the first user, an admin whose one way to sign in is `passkey`
    /// (P3-01): only if there are no users yet, and of two setups at once
    /// exactly one succeeds, as [`Store::add_first_admin`].
    pub async fn add_first_admin_with_passkey(
        &self,
        username: &str,
        passkey: FirstPasskey<'_>,
    ) -> Result<LocalUserId, UserError> {
        let now = self.now();
        let mut tx = self.pool.begin().await?;
        let id = add_first_user(&mut tx, username, now).await?;
        sqlx::query("INSERT INTO passkey_users (user_id, handle) VALUES (?1, ?2)")
            .bind(id)
            .bind(passkey.handle)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO passkeys (credential_id, user_id, name, credential, rp_id, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(passkey.credential_id)
        .bind(id)
        .bind(passkey.name)
        .bind(passkey.credential)
        .bind(passkey.rp_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Rename one of `user`'s passkeys.
    pub async fn rename_passkey(
        &self,
        user: LocalUserId,
        credential_id: &str,
        name: &str,
    ) -> Result<(), PasskeyError> {
        let name = passkey_name(name)?;
        let renamed =
            sqlx::query("UPDATE passkeys SET name = ?3 WHERE user_id = ?1 AND credential_id = ?2")
                .bind(user)
                .bind(credential_id)
                .bind(name)
                .execute(&self.pool)
                .await?;
        if renamed.rows_affected() == 0 {
            return Err(PasskeyError::NoSuchPasskey);
        }
        Ok(())
    }

    /// Remove one of `user`'s passkeys, unless it is their last way to
    /// sign in: a user may have no password (set up with a passkey,
    /// P3-01); a linked SSO identity (P3-04) counts as the password does.
    pub async fn remove_passkey(
        &self,
        user: LocalUserId,
        credential_id: &str,
    ) -> Result<(), PasskeyError> {
        let mut tx = self.pool.begin().await?;
        let (theirs, others, has_password): (bool, i64, bool) = sqlx::query_as(
            "SELECT \
               EXISTS (SELECT 1 FROM passkeys WHERE user_id = ?1 AND credential_id = ?2), \
               (SELECT count(*) FROM passkeys WHERE user_id = ?1 AND credential_id != ?2), \
               EXISTS (SELECT 1 FROM password_credentials WHERE user_id = ?1) \
               OR EXISTS (SELECT 1 FROM external_identities WHERE user_id = ?1)",
        )
        .bind(user)
        .bind(credential_id)
        .fetch_one(&mut *tx)
        .await?;
        if !theirs {
            return Err(PasskeyError::NoSuchPasskey);
        }
        if others == 0 && !has_password {
            return Err(PasskeyError::LastSignInMethod);
        }
        sqlx::query("DELETE FROM passkeys WHERE user_id = ?1 AND credential_id = ?2")
            .bind(user)
            .bind(credential_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// A passkey signed its user in: keep the credential as it is now (its
    /// signature counter moved) and when.
    pub async fn passkey_used(
        &self,
        credential_id: &str,
        credential: &str,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE passkeys SET credential = ?2, last_used_at = ?3 WHERE credential_id = ?1",
        )
        .bind(credential_id)
        .bind(credential)
        .bind(self.now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The passkeys made under neither `rp_id`, the web UI's relying
    /// party, nor `app_rp_id`, the apps' (P3-19, PX-14): no browser or app
    /// offers them here. App passkeys made for the relying party every
    /// server once shared are among them.
    pub async fn passkeys_of_other_hosts(
        &self,
        rp_id: &str,
        app_rp_id: &str,
    ) -> Result<Vec<OwnedPasskey>, StoreError> {
        Ok(sqlx::query_as(
            "SELECT u.username, p.name, p.rp_id FROM passkeys p JOIN users u ON u.id = p.user_id \
             WHERE p.rp_id NOT IN (?1, ?2) ORDER BY u.username, p.created_at",
        )
        .bind(rp_id)
        .bind(app_rp_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// The credential ids (base64url) of `username`'s passkeys made under
    /// `rp_id`, oldest first; none for a user who does not exist or is
    /// disabled.
    pub async fn passkey_ids_of(
        &self,
        username: &str,
        rp_id: &str,
    ) -> Result<Vec<String>, StoreError> {
        Ok(sqlx::query_scalar(
            "SELECT p.credential_id FROM passkeys p JOIN users u ON u.id = p.user_id \
             WHERE u.username = ?1 AND p.rp_id = ?2 AND u.disabled_at IS NULL \
             ORDER BY p.created_at, p.credential_id",
        )
        .bind(username)
        .bind(rp_id)
        .fetch_all(&self.pool)
        .await?)
    }
}

#[cfg(test)]
mod tests {
    use spoolframe_types::Role;

    use super::super::tests::{TestStore, test_store};
    use super::*;

    /// A server's app relying party (PX-14).
    const APP_RP: &str = "mzxw6ytbojtg633c.spoolframe.app";

    async fn store_with_alice(test: &str) -> (TestStore, LocalUserId) {
        let store = test_store(test).await;
        let id = store
            .add_user("alice", None, "phc", Role::Consumer)
            .await
            .unwrap();
        (store, id)
    }

    async fn add(store: &Store, user: LocalUserId, id: &str) -> PasskeyRecord {
        store
            .add_passkey(user, id, "MacBook", "{}", "media.example.org")
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_passkey_is_kept_listed_renamed_and_removed() {
        let (store, alice) = store_with_alice("passkey").await;
        let added = add(&store, alice, "cred-1").await;
        assert_eq!(
            store.list_passkeys(alice).await.unwrap(),
            std::slice::from_ref(&added)
        );
        assert_eq!(store.passkey("cred-1").await.unwrap(), Some(added));
        store
            .rename_passkey(alice, "cred-1", "  Work laptop ")
            .await
            .unwrap();
        assert_eq!(
            store.list_passkeys(alice).await.unwrap()[0].name,
            "Work laptop"
        );
        store.remove_passkey(alice, "cred-1").await.unwrap();
        assert_eq!(store.list_passkeys(alice).await.unwrap(), []);
    }

    #[tokio::test]
    async fn a_credential_is_one_user_s_passkey_only() {
        let (store, alice) = store_with_alice("passkey-once").await;
        let bob = store
            .add_user("bob", None, "phc", Role::Consumer)
            .await
            .unwrap();
        add(&store, alice, "cred-1").await;
        assert!(matches!(
            store
                .add_passkey(bob, "cred-1", "Stolen", "{}", "media.example.org")
                .await,
            Err(PasskeyError::Exists)
        ));
    }

    #[tokio::test]
    async fn another_user_s_passkey_is_not_found() {
        let (store, alice) = store_with_alice("passkey-own").await;
        let bob = store
            .add_user("bob", None, "phc", Role::Consumer)
            .await
            .unwrap();
        add(&store, alice, "cred-1").await;
        assert!(matches!(
            store.rename_passkey(bob, "cred-1", "Mine now").await,
            Err(PasskeyError::NoSuchPasskey)
        ));
        assert!(matches!(
            store.remove_passkey(bob, "cred-1").await,
            Err(PasskeyError::NoSuchPasskey)
        ));
        assert_eq!(store.list_passkeys(alice).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_name_is_trimmed_and_neither_empty_nor_long() {
        let (store, alice) = store_with_alice("passkey-name").await;
        for bad in ["", "   ", &"x".repeat(MAX_NAME_LEN + 1)] {
            assert!(matches!(
                store
                    .add_passkey(alice, "cred-1", bad, "{}", "media.example.org")
                    .await,
                Err(PasskeyError::InvalidName)
            ));
        }
        add(&store, alice, "cred-1").await;
        assert!(matches!(
            store.rename_passkey(alice, "cred-1", " ").await,
            Err(PasskeyError::InvalidName)
        ));
    }

    /// A user without a password (set up with a passkey) keeps their last
    /// passkey.
    #[tokio::test]
    async fn the_last_way_to_sign_in_is_not_removed() {
        let (store, alice) = store_with_alice("passkey-last").await;
        add(&store, alice, "cred-1").await;
        add(&store, alice, "cred-2").await;
        sqlx::query("DELETE FROM password_credentials WHERE user_id = ?1")
            .bind(alice)
            .execute(&store.pool)
            .await
            .unwrap();
        // One of two: the other still signs them in.
        store.remove_passkey(alice, "cred-1").await.unwrap();
        assert!(matches!(
            store.remove_passkey(alice, "cred-2").await,
            Err(PasskeyError::LastSignInMethod)
        ));
        assert_eq!(store.list_passkeys(alice).await.unwrap().len(), 1);
    }

    fn first(credential_id: &'static str) -> FirstPasskey<'static> {
        FirstPasskey {
            handle: "handle-1",
            credential_id,
            name: "iPhone",
            credential: "{}",
            rp_id: "media.example.org",
        }
    }

    #[tokio::test]
    async fn the_first_admin_can_have_only_a_passkey() {
        let store = test_store("passkey-first-admin").await;
        let root = store
            .add_first_admin_with_passkey("Root", first("cred-1"))
            .await
            .unwrap();
        let user = store.user_by_id(root).await.unwrap().unwrap();
        assert_eq!(user.username, "root");
        assert!(user.role.is_admin());
        assert!(!user.has_password);
        assert_eq!(store.list_passkeys(root).await.unwrap()[0].name, "iPhone");
        assert_eq!(
            store.passkey_handle_user("handle-1").await.unwrap(),
            Some(root)
        );
        // Their only passkey stays.
        assert!(matches!(
            store.remove_passkey(root, "cred-1").await,
            Err(PasskeyError::LastSignInMethod)
        ));
        // Setup is over, and the refused one kept nothing.
        assert!(matches!(
            store
                .add_first_admin_with_passkey("other", first("cred-2"))
                .await,
            Err(UserError::AlreadySetUp)
        ));
        assert_eq!(store.passkey("cred-2").await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_user_has_one_handle() {
        let (store, alice) = store_with_alice("passkey-handle").await;
        assert_eq!(
            store.passkey_user_handle(alice, "first").await.unwrap(),
            "first"
        );
        assert_eq!(
            store.passkey_user_handle(alice, "second").await.unwrap(),
            "first"
        );
        assert_eq!(
            store.passkey_handle_user("first").await.unwrap(),
            Some(alice)
        );
        assert_eq!(store.passkey_handle_user("second").await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_user_s_passkeys_of_one_relying_party_are_listed_by_username() {
        let (store, alice) = store_with_alice("passkey-ids-of").await;
        add(&store, alice, "web-1").await;
        store
            .add_passkey(alice, "app-1", "iPhone", "{}", APP_RP)
            .await
            .unwrap();
        store
            .add_passkey(alice, "app-2", "iPad", "{}", APP_RP)
            .await
            .unwrap();
        let ids = |name: &'static str| {
            let store = &store;
            async move { store.passkey_ids_of(name, APP_RP).await.unwrap() }
        };
        assert_eq!(ids("alice").await, ["app-1", "app-2"]);
        assert_eq!(ids("nobody").await, Vec::<String>::new());
        store.disable_user("alice").await.unwrap();
        assert_eq!(ids("alice").await, Vec::<String>::new());
    }

    #[tokio::test]
    async fn a_use_keeps_the_credential_and_the_time() {
        let (store, alice) = store_with_alice("passkey-used").await;
        add(&store, alice, "cred-1").await;
        store
            .passkey_used("cred-1", r#"{"counter":2}"#)
            .await
            .unwrap();
        let used = store.passkey("cred-1").await.unwrap().unwrap();
        assert_eq!(used.credential, r#"{"counter":2}"#);
        assert!(used.last_used_at.is_some());
    }

    #[tokio::test]
    async fn passkeys_of_another_host_are_found() {
        let (store, alice) = store_with_alice("passkey-orphans").await;
        add(&store, alice, "cred-1").await;
        store
            .add_passkey(alice, "cred-2", "Tunnel", "{}", "localhost")
            .await
            .unwrap();
        // An app's (P3-19) works whatever the web UI's host is; one made
        // for the relying party every server once shared (PX-14) does not.
        store
            .add_passkey(alice, "cred-3", "iPhone", "{}", APP_RP)
            .await
            .unwrap();
        store
            .add_passkey(alice, "cred-4", "iPad", "{}", "spoolframe.app")
            .await
            .unwrap();
        // Made in the same second, they are in no set order.
        let mut others = store
            .passkeys_of_other_hosts("media.example.org", APP_RP)
            .await
            .unwrap();
        others.sort_by(|a, b| a.rp_id.cmp(&b.rp_id));
        assert_eq!(
            others,
            [
                OwnedPasskey {
                    username: "alice".into(),
                    name: "Tunnel".into(),
                    rp_id: "localhost".into(),
                },
                OwnedPasskey {
                    username: "alice".into(),
                    name: "iPad".into(),
                    rp_id: "spoolframe.app".into(),
                },
            ]
        );
    }
}
