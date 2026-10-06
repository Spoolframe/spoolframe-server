//! Local users and their password credentials (ARCHITECTURE §21).

use std::fmt;
use std::str::FromStr;

use spoolframe_types::Role;
use sqlx::Row;
use sqlx::encode::IsNull;
use sqlx::error::BoxDynError;
use sqlx::sqlite::{Sqlite, SqliteArgumentsBuffer, SqliteTypeInfo, SqliteValueRef};
use uuid::Uuid;

use super::{Store, StoreError, now};

/// A local user's id. Every login method ends here (§24B).
///
/// A UUIDv7 (P3-20): unique beyond this server, saying nothing about how
/// many users it has, and still sorting by creation. Shown and stored
/// lowercase and hyphenated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocalUserId(Uuid);

impl LocalUserId {
    /// A new user's id.
    pub fn generate() -> Self {
        Self(Uuid::now_v7())
    }

    /// An id as it is. For tests and for parsing; a new user gets
    /// [`LocalUserId::generate`].
    pub const fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }

    pub fn uuid(&self) -> Uuid {
        self.0
    }
}

impl fmt::Display for LocalUserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0.hyphenated(), f)
    }
}

impl FromStr for LocalUserId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Uuid::try_parse(s).map(Self)
    }
}

/// Stored as its text, as principals carry it (`local:<uuid>`), so the
/// database reads the same in `sqlite3` as in a log.
impl sqlx::Type<Sqlite> for LocalUserId {
    fn type_info() -> SqliteTypeInfo {
        <String as sqlx::Type<Sqlite>>::type_info()
    }
}

impl sqlx::Encode<'_, Sqlite> for LocalUserId {
    fn encode_by_ref(&self, buf: &mut SqliteArgumentsBuffer) -> Result<IsNull, BoxDynError> {
        <String as sqlx::Encode<Sqlite>>::encode(self.to_string(), buf)
    }
}

impl<'r> sqlx::Decode<'r, Sqlite> for LocalUserId {
    fn decode(value: SqliteValueRef<'r>) -> Result<Self, BoxDynError> {
        Ok(<&str as sqlx::Decode<Sqlite>>::decode(value)?.parse()?)
    }
}

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
    /// When their password was set or last changed; `None` without one.
    pub password_changed_at: Option<i64>,
    /// When any of their sessions was last used.
    pub last_seen_at: Option<i64>,
    pub has_passkey: bool,
    /// They have linked an SSO identity (P3-04).
    pub has_sso: bool,
    pub has_pin: bool,
    /// The devices they have signed in from (any session, revoked too),
    /// as `user_devices` lists them.
    pub device_count: u64,
    /// Their live sessions: not revoked, not expired, as `list_sessions`
    /// lists them.
    pub session_count: u64,
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
    /// A display name that is too long or has control characters (P3-03).
    #[error("a display name is at most {MAX_DISPLAY_NAME_LEN} characters, on one line")]
    InvalidDisplayName,
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

/// The longest display name a user can give themselves.
pub const MAX_DISPLAY_NAME_LEN: usize = 64;

/// `name` as a display name is stored: trimmed; `None` when empty, which
/// shows the username. At most [`MAX_DISPLAY_NAME_LEN`] characters, and
/// none of them a control character (a line break included).
pub fn display_name(name: Option<&str>) -> Result<Option<&str>, UserError> {
    let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) else {
        return Ok(None);
    };
    if name.chars().count() > MAX_DISPLAY_NAME_LEN || name.chars().any(char::is_control) {
        return Err(UserError::InvalidDisplayName);
    }
    Ok(Some(name))
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
        let id = LocalUserId::generate();
        let mut tx = self.pool.begin().await?;
        let inserted = sqlx::query(
            "INSERT INTO users (id, username, display_name, role, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(id)
        .bind(&name)
        .bind(display_name)
        .bind(stored(role))
        .bind(now)
        .execute(&mut *tx)
        .await;
        match inserted {
            Ok(_) => {}
            Err(e) if is_unique_violation(&e) => return Err(UserError::Exists(name)),
            Err(e) => return Err(e.into()),
        }
        add_password(&mut tx, id, phc, now).await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Add the first user, as an admin, only if there are no users yet.
    /// Atomic: of two concurrent setups exactly one succeeds.
    pub async fn add_first_admin(
        &self,
        username: &str,
        phc: &str,
    ) -> Result<LocalUserId, UserError> {
        let now = now();
        let mut tx = self.pool.begin().await?;
        let id = add_first_user(&mut tx, username, now).await?;
        add_password(&mut tx, id, phc, now).await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Replace a user's password hash, and revoke every live session of
    /// theirs but `keep` (a token hash, the session that made the change):
    /// whoever knew the old password, on the web or on a device, signs in
    /// again (PX-09 finding 4). How many were revoked.
    pub async fn set_password(
        &self,
        username: &str,
        phc: &str,
        keep: Option<&str>,
    ) -> Result<u64, UserError> {
        let name = normalize_username(username)?;
        let now = now();
        let mut tx = self.pool.begin().await?;
        let id = user_id(&mut tx, &name).await?;
        sqlx::query(
            "INSERT INTO password_credentials (user_id, phc, updated_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT (user_id) DO UPDATE SET phc = excluded.phc, updated_at = excluded.updated_at",
        )
        .bind(id)
        .bind(phc)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        let revoked = sqlx::query(
            "UPDATE sessions SET revoked_at = ?2 \
             WHERE user_id = ?1 AND revoked_at IS NULL AND token_hash IS NOT ?3",
        )
        .bind(id)
        .bind(now)
        .bind(keep)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        // And none of their devices resumes them (P4-01): trust was made
        // under the old password, by whoever knew it.
        forget_trust(&mut tx, id).await?;
        tx.commit().await?;
        // The other devices' connections are closed (PX-10).
        self.note_sessions_ended();
        Ok(revoked)
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
            .bind(id)
            .bind(stored(role))
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Set what `user` is shown as (P3-03); `None` or blank shows the
    /// username.
    pub async fn set_display_name(
        &self,
        user: LocalUserId,
        name: Option<&str>,
    ) -> Result<(), UserError> {
        let name = display_name(name)?;
        let changed = sqlx::query("UPDATE users SET display_name = ?2 WHERE id = ?1")
            .bind(user)
            .bind(name)
            .execute(&self.pool)
            .await?;
        if changed.rows_affected() == 0 {
            return Err(UserError::NoSuchUser(format!("#{user}")));
        }
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
            .bind(id)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        let revoked = sqlx::query(
            "UPDATE sessions SET revoked_at = ?2 WHERE user_id = ?1 AND revoked_at IS NULL",
        )
        .bind(id)
        .bind(now)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        // Enabling them again does not bring it back (P4-01).
        forget_trust(&mut tx, id).await?;
        tx.commit().await?;
        self.note_sessions_ended();
        Ok(revoked)
    }

    /// Let a disabled user log in again. Their revoked sessions stay
    /// revoked.
    pub async fn enable_user(&self, username: &str) -> Result<(), UserError> {
        let name = normalize_username(username)?;
        let mut tx = self.pool.begin().await?;
        let id = user_id(&mut tx, &name).await?;
        sqlx::query("UPDATE users SET disabled_at = NULL WHERE id = ?1")
            .bind(id)
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
                    p.user_id IS NOT NULL AS has_password, p.updated_at AS password_changed_at, \
                    (SELECT max(s.last_used_at) FROM sessions s WHERE s.user_id = u.id) \
                        AS last_seen_at, \
                    EXISTS (SELECT 1 FROM passkeys k WHERE k.user_id = u.id) AS has_passkey, \
                    EXISTS (SELECT 1 FROM external_identities i WHERE i.user_id = u.id) \
                        AS has_sso, \
                    EXISTS (SELECT 1 FROM pin_credentials c WHERE c.user_id = u.id) AS has_pin, \
                    (SELECT count(DISTINCT s.endpoint_id) FROM sessions s \
                     WHERE s.user_id = u.id) AS device_count, \
                    (SELECT count(*) FROM sessions s \
                     WHERE s.user_id = u.id AND s.revoked_at IS NULL AND s.expires_at > ?1) \
                        AS session_count \
             FROM users u LEFT JOIN password_credentials p ON p.user_id = u.id \
             ORDER BY u.username",
        )
        .bind(now())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(UserRecord {
                    id: row.try_get("id")?,
                    username: row.try_get("username")?,
                    display_name: row.try_get("display_name")?,
                    role: role(row.try_get("role")?),
                    created_at: row.try_get("created_at")?,
                    disabled_at: row.try_get("disabled_at")?,
                    has_password: row.try_get("has_password")?,
                    password_changed_at: row.try_get("password_changed_at")?,
                    last_seen_at: row.try_get("last_seen_at")?,
                    has_passkey: row.try_get("has_passkey")?,
                    has_sso: row.try_get("has_sso")?,
                    has_pin: row.try_get("has_pin")?,
                    device_count: row.try_get::<i64, _>("device_count")? as u64,
                    session_count: row.try_get::<i64, _>("session_count")? as u64,
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
                id: row.try_get("id")?,
                role: role(row.try_get("role")?),
                disabled: row.try_get("disabled")?,
                phc: row.try_get("phc")?,
            })
        })
        .transpose()
    }
}

/// Remove every device's trust for `user` (P4-01).
async fn forget_trust(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    user: LocalUserId,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM trusted_endpoints WHERE user_id = ?1")
        .bind(user)
        .execute(&mut **tx)
        .await?;
    Ok(())
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

/// Add `username` as an admin, in `tx`, only if there are no users yet: of
/// two setups at once exactly one gets past this.
pub(super) async fn add_first_user(
    tx: &mut sqlx::SqliteConnection,
    username: &str,
    now: i64,
) -> Result<LocalUserId, UserError> {
    let name = normalize_username(username)?;
    let id = LocalUserId::generate();
    let inserted = sqlx::query(
        "INSERT INTO users (id, username, role, created_at) \
         SELECT ?1, ?2, 'admin', ?3 WHERE NOT EXISTS (SELECT 1 FROM users)",
    )
    .bind(id)
    .bind(&name)
    .bind(now)
    .execute(tx)
    .await?;
    if inserted.rows_affected() == 0 {
        return Err(UserError::AlreadySetUp);
    }
    Ok(id)
}

async fn add_password(
    tx: &mut sqlx::SqliteConnection,
    user: LocalUserId,
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
    .bind(id)
    .fetch_one(tx)
    .await?;
    if is_enabled_admin && others == 0 {
        return Err(UserError::LastAdmin(name.to_owned()));
    }
    Ok(())
}

async fn user_id(tx: &mut sqlx::SqliteConnection, name: &str) -> Result<LocalUserId, UserError> {
    let id: Option<LocalUserId> = sqlx::query_scalar("SELECT id FROM users WHERE username = ?1")
        .bind(name)
        .fetch_optional(tx)
        .await?;
    id.ok_or_else(|| UserError::NoSuchUser(name.to_owned()))
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
    async fn a_display_name_is_set_trimmed_and_cleared() {
        let store = store("display-name").await;
        let id = store
            .add_user("alice", None, "phc", Role::Consumer)
            .await
            .unwrap();
        let shown = |store: &Store| {
            let store = store.clone();
            async move { store.user_by_id(id).await.unwrap().unwrap().display_name }
        };
        store
            .set_display_name(id, Some("  Alice A. "))
            .await
            .unwrap();
        assert_eq!(shown(&store).await.as_deref(), Some("Alice A."));
        store.set_display_name(id, Some("   ")).await.unwrap();
        assert_eq!(shown(&store).await, None);
        for bad in ["x".repeat(MAX_DISPLAY_NAME_LEN + 1), "two\nlines".into()] {
            assert!(matches!(
                store.set_display_name(id, Some(&bad)).await,
                Err(UserError::InvalidDisplayName)
            ));
        }
        assert_eq!(shown(&store).await, None);
    }

    #[tokio::test]
    async fn a_password_is_replaced() {
        let store = store("passwd").await;
        store
            .add_user("alice", None, "old", Role::Consumer)
            .await
            .unwrap();
        store.set_password("alice", "new", None).await.unwrap();
        let login = store.password_login("alice").await.unwrap().unwrap();
        assert_eq!(login.phc.as_deref(), Some("new"));
    }

    #[tokio::test]
    async fn an_unknown_user_has_no_login() {
        let store = store("unknown").await;
        assert_eq!(store.password_login("nobody").await.unwrap(), None);
        assert!(matches!(
            store.set_password("nobody", "x", None).await,
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
        .bind(id)
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
        .bind(id)
        .execute(&store.pool)
        .await;
        assert!(endpoint_without_id.is_err());
    }
}
