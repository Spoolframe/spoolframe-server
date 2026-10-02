//! Local users and their password credentials (ARCHITECTURE §21).

use spoolframe_types::Role;
use sqlx::Row;

use super::{Store, StoreError, now};

/// A local user's database id. Every login method ends here (§24B).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LocalUserId(pub i64);

/// A user as the admin sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRecord {
    pub id: LocalUserId,
    pub username: String,
    /// What clients show; `None` means the username.
    pub display_name: Option<String>,
    pub role: Role,
    pub created_at: i64,
    pub disabled_at: Option<i64>,
    pub has_password: bool,
}

/// What a password login needs about a user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasswordLogin {
    pub id: LocalUserId,
    pub role: Role,
    pub disabled: bool,
    /// `None` when the user has no password credential.
    pub phc: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum UserError {
    #[error("user {0:?} already exists")]
    Exists(String),
    #[error("no user {0:?}")]
    NoSuchUser(String),
    #[error("invalid username {0:?}: use 1-64 letters, digits, '.', '_' or '-'")]
    InvalidUsername(String),
    /// `setup` after the first user exists.
    #[error("the server already has users")]
    AlreadySetUp,
    /// Demoting or disabling the last enabled admin (P2-09).
    #[error("{0:?} is the last enabled admin; make another admin first")]
    LastAdmin(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<sqlx::Error> for UserError {
    fn from(e: sqlx::Error) -> Self {
        UserError::Store(e.into())
    }
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.is_unique_violation())
}

/// Usernames are compared without case: stored lowercased.
pub fn normalize_username(username: &str) -> Result<String, UserError> {
    let name = username.trim().to_lowercase();
    let valid = (1..=64).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if valid {
        Ok(name)
    } else {
        Err(UserError::InvalidUsername(username.to_owned()))
    }
}

impl Store {
    /// Add a user with a password hash (PHC string).
    pub async fn add_user(
        &self,
        username: &str,
        display_name: Option<&str>,
        phc: &str,
        role: Role,
    ) -> Result<LocalUserId, UserError> {
        let name = normalize_username(username)?;
        let display_name = display_name.map(str::trim).filter(|d| !d.is_empty());
        let now = now();
        let mut tx = self.pool.begin().await?;
        let inserted = sqlx::query(
            "INSERT INTO users (username, display_name, role, created_at) \
             VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(&name)
        .bind(display_name)
        .bind(stored(role))
        .bind(now)
        .execute(&mut *tx)
        .await;
        let id = match inserted {
            Ok(done) => done.last_insert_rowid(),
            Err(e) if is_unique_violation(&e) => return Err(UserError::Exists(name)),
            Err(e) => return Err(e.into()),
        };
        add_password(&mut tx, id, phc, now).await?;
        tx.commit().await?;
        Ok(LocalUserId(id))
    }

    /// Add the first user, as an admin, only if there are no users yet.
    /// Atomic: of two concurrent setups exactly one succeeds.
    pub async fn add_first_admin(
        &self,
        username: &str,
        phc: &str,
    ) -> Result<LocalUserId, UserError> {
        let name = normalize_username(username)?;
        let now = now();
        let mut tx = self.pool.begin().await?;
        let inserted = sqlx::query(
            "INSERT INTO users (username, role, created_at) \
             SELECT ?1, 'admin', ?2 WHERE NOT EXISTS (SELECT 1 FROM users)",
        )
        .bind(&name)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() == 0 {
            return Err(UserError::AlreadySetUp);
        }
        let id = inserted.last_insert_rowid();
        add_password(&mut tx, id, phc, now).await?;
        tx.commit().await?;
        Ok(LocalUserId(id))
    }

    /// Replace a user's password hash.
    pub async fn set_password(&self, username: &str, phc: &str) -> Result<(), UserError> {
        let name = normalize_username(username)?;
        let mut tx = self.pool.begin().await?;
        let id = user_id(&mut tx, &name).await?;
        sqlx::query(
            "INSERT INTO password_credentials (user_id, phc, updated_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT (user_id) DO UPDATE SET phc = excluded.phc, updated_at = excluded.updated_at",
        )
        .bind(id.0)
        .bind(phc)
        .bind(now())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Make `username` an admin or a consumer (P2-09). The last enabled
    /// admin stays one.
    pub async fn set_role(&self, username: &str, role: Role) -> Result<(), UserError> {
        let name = normalize_username(username)?;
        let mut tx = self.pool.begin().await?;
        let id = user_id(&mut tx, &name).await?;
        if !role.is_admin() {
            keep_an_admin(&mut tx, id, &name).await?;
        }
        sqlx::query("UPDATE users SET role = ?2 WHERE id = ?1")
            .bind(id.0)
            .bind(stored(role))
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// One user, by id.
    pub async fn user_by_id(&self, id: LocalUserId) -> Result<Option<UserRecord>, UserError> {
        Ok(self.list_users().await?.into_iter().find(|u| u.id == id))
    }

    /// Disable a user and revoke every session they have. Returns how many
    /// sessions were revoked. The last enabled admin stays enabled.
    pub async fn disable_user(&self, username: &str) -> Result<u64, UserError> {
        let name = normalize_username(username)?;
        let now = now();
        let mut tx = self.pool.begin().await?;
        let id = user_id(&mut tx, &name).await?;
        keep_an_admin(&mut tx, id, &name).await?;
        sqlx::query("UPDATE users SET disabled_at = ?2 WHERE id = ?1 AND disabled_at IS NULL")
            .bind(id.0)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        let revoked = sqlx::query(
            "UPDATE sessions SET revoked_at = ?2 WHERE user_id = ?1 AND revoked_at IS NULL",
        )
        .bind(id.0)
        .bind(now)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        Ok(revoked)
    }

    /// Let a disabled user log in again. Their revoked sessions stay
    /// revoked.
    pub async fn enable_user(&self, username: &str) -> Result<(), UserError> {
        let name = normalize_username(username)?;
        let mut tx = self.pool.begin().await?;
        let id = user_id(&mut tx, &name).await?;
        sqlx::query("UPDATE users SET disabled_at = NULL WHERE id = ?1")
            .bind(id.0)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn user_count(&self) -> Result<u64, UserError> {
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
            .fetch_one(&self.pool)
            .await?;
        Ok(count as u64)
    }

    pub async fn list_users(&self) -> Result<Vec<UserRecord>, UserError> {
        let rows = sqlx::query(
            "SELECT u.id, u.username, u.display_name, u.role, u.created_at, u.disabled_at, \
                    p.user_id IS NOT NULL AS has_password \
             FROM users u LEFT JOIN password_credentials p ON p.user_id = u.id \
             ORDER BY u.username",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(UserRecord {
                    id: LocalUserId(row.try_get("id")?),
                    username: row.try_get("username")?,
                    display_name: row.try_get("display_name")?,
                    role: role(row.try_get("role")?),
                    created_at: row.try_get("created_at")?,
                    disabled_at: row.try_get("disabled_at")?,
                    has_password: row.try_get("has_password")?,
                })
            })
            .collect()
    }

    /// What a password login for `username` needs, or `None` if there is
    /// no such user.
    pub async fn password_login(&self, username: &str) -> Result<Option<PasswordLogin>, UserError> {
        let Ok(name) = normalize_username(username) else {
            return Ok(None);
        };
        let row = sqlx::query(
            "SELECT u.id, u.role, u.disabled_at IS NOT NULL AS disabled, p.phc \
             FROM users u LEFT JOIN password_credentials p ON p.user_id = u.id \
             WHERE u.username = ?1",
        )
        .bind(&name)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok(PasswordLogin {
                id: LocalUserId(row.try_get("id")?),
                role: role(row.try_get("role")?),
                disabled: row.try_get("disabled")?,
                phc: row.try_get("phc")?,
            })
        })
        .transpose()
    }
}

/// A role as the database stores it. `Unknown` is never stored: the
/// column's CHECK allows only the two.
pub(crate) fn stored(role: Role) -> &'static str {
    match role {
        Role::Admin => "admin",
        _ => "consumer",
    }
}

/// A role as the database has it.
pub(crate) fn role(stored: String) -> Role {
    stored.parse().unwrap_or(Role::Unknown)
}

async fn add_password(
    tx: &mut sqlx::SqliteConnection,
    user: i64,
    phc: &str,
    now: i64,
) -> Result<(), UserError> {
    sqlx::query("INSERT INTO password_credentials (user_id, phc, updated_at) VALUES (?1, ?2, ?3)")
        .bind(user)
        .bind(phc)
        .bind(now)
        .execute(tx)
        .await?;
    Ok(())
}

/// Refuse to take `id` out of the enabled admins if it is the last one.
/// In the change's own transaction, so two admins demoting each other at
/// once cannot both succeed.
async fn keep_an_admin(
    tx: &mut sqlx::SqliteConnection,
    id: LocalUserId,
    name: &str,
) -> Result<(), UserError> {
    let (is_enabled_admin, others): (bool, i64) = sqlx::query_as(
        "SELECT \
           EXISTS (SELECT 1 FROM users WHERE id = ?1 AND role = 'admin' AND disabled_at IS NULL), \
           (SELECT count(*) FROM users WHERE id != ?1 AND role = 'admin' AND disabled_at IS NULL)",
    )
    .bind(id.0)
    .fetch_one(tx)
    .await?;
    if is_enabled_admin && others == 0 {
        return Err(UserError::LastAdmin(name.to_owned()));
    }
    Ok(())
}

async fn user_id(tx: &mut sqlx::SqliteConnection, name: &str) -> Result<LocalUserId, UserError> {
    let id: Option<i64> = sqlx::query_scalar("SELECT id FROM users WHERE username = ?1")
        .bind(name)
        .fetch_optional(tx)
        .await?;
    id.map(LocalUserId)
        .ok_or_else(|| UserError::NoSuchUser(name.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{TestStore, state_dir, test_store};
    use super::*;

    async fn store(test: &str) -> TestStore {
        test_store(test).await
    }

    #[tokio::test]
    async fn a_user_is_added_and_listed() {
        let store = store("add").await;
        store
            .add_user("Alice", Some("Alice A."), "phc-a", Role::Consumer)
            .await
            .unwrap();
        let users = store.list_users().await.unwrap();
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].username, "alice");
        assert_eq!(users[0].display_name.as_deref(), Some("Alice A."));
        assert!(users[0].has_password);
        assert_eq!(users[0].role, Role::Consumer);
        assert_eq!(users[0].disabled_at, None);
    }

    #[tokio::test]
    async fn usernames_are_unique_without_case() {
        let store = store("unique").await;
        store
            .add_user("alice", None, "phc", Role::Consumer)
            .await
            .unwrap();
        assert!(matches!(
            store.add_user("ALICE", None, "phc", Role::Consumer).await,
            Err(UserError::Exists(_))
        ));
    }

    #[tokio::test]
    async fn a_bad_username_is_refused() {
        let store = store("bad-name").await;
        for name in ["", "a b", "x".repeat(65).as_str(), "al/ice"] {
            assert!(
                matches!(
                    store.add_user(name, None, "phc", Role::Consumer).await,
                    Err(UserError::InvalidUsername(_))
                ),
                "{name:?}"
            );
        }
    }

    #[tokio::test]
    async fn the_first_admin_is_added_only_to_an_empty_server() {
        let store = store("first-admin").await;
        store.add_first_admin("root", "phc").await.unwrap();
        assert!(
            store
                .password_login("root")
                .await
                .unwrap()
                .unwrap()
                .role
                .is_admin()
        );
        assert!(matches!(
            store.add_first_admin("other", "phc").await,
            Err(UserError::AlreadySetUp)
        ));
    }

    #[tokio::test]
    async fn the_last_enabled_admin_cannot_be_demoted_or_disabled() {
        let store = store("last-admin").await;
        store.add_first_admin("root", "phc").await.unwrap();
        assert!(matches!(
            store.set_role("root", Role::Consumer).await,
            Err(UserError::LastAdmin(_))
        ));
        assert!(matches!(
            store.disable_user("root").await,
            Err(UserError::LastAdmin(_))
        ));
        // With a second admin, either may go.
        store
            .add_user("alice", None, "phc", Role::Admin)
            .await
            .unwrap();
        store.set_role("root", Role::Consumer).await.unwrap();
        assert!(matches!(
            store.disable_user("alice").await,
            Err(UserError::LastAdmin(_))
        ));
        // A disabled admin does not count.
        store.set_role("root", Role::Admin).await.unwrap();
        store.disable_user("root").await.unwrap();
        assert!(matches!(
            store.set_role("alice", Role::Consumer).await,
            Err(UserError::LastAdmin(_))
        ));
    }

    /// A database from before P2-09: `is_admin` becomes `role`.
    #[tokio::test]
    async fn is_admin_becomes_the_admin_or_consumer_role() {
        let dir = state_dir("roles-migration").await;
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(dir.as_ref().join(super::super::DB_FILE))
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = sqlx::SqlitePool::connect_with(options).await.unwrap();
        let mut before = sqlx::migrate!("./migrations");
        before.migrations = before
            .iter()
            .filter(|m| m.version < 6)
            .cloned()
            .collect::<Vec<_>>()
            .into();
        before.run(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO users (username, is_admin, created_at) \
             VALUES ('root', 1, 0), ('alice', 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        let store = Store::open(dir.as_ref()).await.unwrap();
        let roles: Vec<_> = store
            .list_users()
            .await
            .unwrap()
            .into_iter()
            .map(|u| (u.username, u.role))
            .collect();
        assert_eq!(
            roles,
            [
                ("alice".into(), Role::Consumer),
                ("root".into(), Role::Admin)
            ]
        );
    }

    #[tokio::test]
    async fn a_password_is_replaced() {
        let store = store("passwd").await;
        store
            .add_user("alice", None, "old", Role::Consumer)
            .await
            .unwrap();
        store.set_password("alice", "new").await.unwrap();
        let login = store.password_login("alice").await.unwrap().unwrap();
        assert_eq!(login.phc.as_deref(), Some("new"));
    }

    #[tokio::test]
    async fn an_unknown_user_has_no_login() {
        let store = store("unknown").await;
        assert_eq!(store.password_login("nobody").await.unwrap(), None);
        assert!(matches!(
            store.set_password("nobody", "x").await,
            Err(UserError::NoSuchUser(_))
        ));
    }

    #[tokio::test]
    async fn disabling_revokes_sessions_and_enabling_does_not_restore_them() {
        let store = store("disable").await;
        let id = store
            .add_user("alice", None, "phc", Role::Consumer)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO sessions (token_hash, user_id, kind, created_at, expires_at) \
             VALUES ('h', ?1, 'api', 0, 99999999999)",
        )
        .bind(id.0)
        .execute(&store.pool)
        .await
        .unwrap();
        assert_eq!(store.disable_user("alice").await.unwrap(), 1);
        assert!(
            store
                .password_login("alice")
                .await
                .unwrap()
                .unwrap()
                .disabled
        );
        store.enable_user("alice").await.unwrap();
        assert!(
            !store
                .password_login("alice")
                .await
                .unwrap()
                .unwrap()
                .disabled
        );
        let revoked: bool = sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM sessions")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert!(revoked);
    }

    #[tokio::test]
    async fn a_session_kind_and_its_endpoint_must_agree() {
        let store = store("session-check").await;
        let id = store
            .add_user("alice", None, "phc", Role::Consumer)
            .await
            .unwrap();
        let endpoint_without_id = sqlx::query(
            "INSERT INTO sessions (token_hash, user_id, kind, created_at, expires_at) \
             VALUES ('h', ?1, 'endpoint', 0, 1)",
        )
        .bind(id.0)
        .execute(&store.pool)
        .await;
        assert!(endpoint_without_id.is_err());
    }
}
