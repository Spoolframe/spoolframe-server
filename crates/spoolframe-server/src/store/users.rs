//! Local users and their password credentials (ARCHITECTURE §21).

use std::fmt;
use std::str::FromStr;

use spoolframe_types::Role;
use sqlx::Row;
use sqlx::encode::IsNull;
use sqlx::error::BoxDynError;
use sqlx::sqlite::{Sqlite, SqliteArgumentsBuffer, SqliteTypeInfo, SqliteValueRef};
use uuid::Uuid;

use super::{Store, StoreError};

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
    /// When they left the server from an app (PX-47): disabled too, and
    /// an admin restores them by enabling them.
    pub left_at: Option<i64>,
    pub has_password: bool,
    /// When their password was set or last changed; `None` without one.
    pub password_changed_at: Option<i64>,
    /// The password is a one-time one an admin reset to, not yet changed.
    pub password_must_change: bool,
    /// When that one-time password stops working (Unix seconds).
    pub password_expires_at: Option<i64>,
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
    /// Their account's ties to portable identities (P7-04, P7-09), the
    /// oldest first.
    pub ties: Vec<super::ties::Tie>,
}

/// What a password login needs about a user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasswordLogin {
    pub id: LocalUserId,
    pub role: Role,
    pub disabled: bool,
    /// `None` when the user has no password credential.
    pub phc: Option<String>,
    /// The password is a one-time one an admin reset to: signing in with
    /// it gives a session that may only change it.
    pub must_change: bool,
    /// When a one-time password stops working (Unix seconds).
    pub expires_at: Option<i64>,
}

/// What resetting a password ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reset {
    /// Sessions revoked: those that rested on the password, or whose
    /// origin is not known.
    pub revoked: u64,
    /// Devices no longer trusted for the user, on the same rule.
    pub untrusted: u64,
}

/// A user [`Store::delete_user`] deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletedUser {
    pub id: LocalUserId,
    /// Their downloads, gone from the store: what was being made for
    /// them stops, and the files no other download names go.
    pub downloads: Vec<super::downloads::DownloadRow>,
    /// Their playlists, gone with them: whoever saw one is told, after.
    pub playlists: Vec<super::playlists::GonePlaylist>,
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
    /// Demoting or disabling the last enabled admin with a password: an
    /// admin's passkeys and providers can all be lost, a password cannot
    /// be, and getting back in otherwise needs a shell on the server.
    #[error("{0:?} is the last enabled admin with a password; give another admin a password first")]
    LastPasswordAdmin(String),
    /// Removing the password of a user who has no passkey and no linked
    /// provider account: they could not sign in again.
    #[error("{0:?} has no other way to sign in: add a passkey or link a sign-in provider first")]
    LastSignInMethod(String),
    /// Removing a password that is not there.
    #[error("{0:?} has no password")]
    NoPassword(String),
    /// A display name that is too long or has control characters (P3-03).
    #[error("a display name is at most {MAX_DISPLAY_NAME_LEN} characters, on one line")]
    InvalidDisplayName,
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Which users [`Store::users_where`] reads.
enum Which<'a> {
    All,
    Id(LocalUserId),
    Name(&'a str),
}

pub(super) fn is_unique_violation(e: &sqlx::Error) -> bool {
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
        let display_name = self::display_name(display_name)?;
        let now = self.now();
        let id = LocalUserId::generate();
        let mut tx = self.begin_write().await?;
        if !insert_user(&mut tx, id, &name, display_name, role, now).await? {
            return Err(UserError::Exists(name));
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
        let now = self.now();
        let mut tx = self.begin_write().await?;
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
        let now = self.now();
        let mut tx = self.begin_write().await?;
        let id = user_id(&mut tx, &name).await?;
        sqlx::query(
            "INSERT INTO password_credentials (user_id, phc, updated_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT (user_id) DO UPDATE SET phc = excluded.phc, \
               updated_at = excluded.updated_at, must_change = 0, expires_at = NULL",
        )
        .bind(id)
        .bind(phc)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        // The session that set it may do anything again, if it was one a
        // one-time password opened.
        sqlx::query("UPDATE sessions SET password_change_only = 0 WHERE token_hash = ?1")
            .bind(keep)
            .execute(&mut *tx)
            .await?;
        // And none of their devices resumes them (P4-01): trust was made
        // under the old password, by whoever knew it.
        let revoked = end_sessions(&mut tx, id, now, keep).await?;
        tx.commit().await?;
        // The other devices' connections are closed (PX-10).
        self.note_sessions_ended();
        Ok(revoked)
    }

    /// Remove `username`'s password: they sign in with a passkey or a
    /// provider from then on. Not their last way to sign in, nor the last
    /// enabled admin's password. As a new password does, it ends every
    /// session of theirs but `keep` and forgets their trusted devices: a
    /// password taken away is often one that leaked.
    pub async fn remove_password(
        &self,
        username: &str,
        keep: Option<&str>,
    ) -> Result<u64, UserError> {
        let name = normalize_username(username)?;
        let now = self.now();
        let mut tx = self.begin_write().await?;
        let id = user_id(&mut tx, &name).await?;
        let (has_password, other_ways): (bool, bool) = sqlx::query_as(
            "SELECT \
               EXISTS (SELECT 1 FROM password_credentials WHERE user_id = ?1), \
               EXISTS (SELECT 1 FROM passkeys WHERE user_id = ?1) \
               OR EXISTS (SELECT 1 FROM external_identities WHERE user_id = ?1)",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        if !has_password {
            return Err(UserError::NoPassword(name));
        }
        if !other_ways {
            return Err(UserError::LastSignInMethod(name));
        }
        keep_a_password_admin(&mut tx, id, &name).await?;
        sqlx::query("DELETE FROM password_credentials WHERE user_id = ?1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        let revoked = end_sessions(&mut tx, id, now, keep).await?;
        tx.commit().await?;
        self.note_sessions_ended();
        Ok(revoked)
    }

    /// Make `username` an admin or a consumer (P2-09). The last enabled
    /// admin stays one.
    pub async fn set_role(&self, username: &str, role: Role) -> Result<(), UserError> {
        let name = normalize_username(username)?;
        let mut tx = self.begin_write().await?;
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
        Ok(self.users_where(Which::Id(id)).await?.pop())
    }

    /// The user named `username` (lowercased, as usernames are kept).
    pub async fn user_by_name(&self, username: &str) -> Result<Option<UserRecord>, UserError> {
        Ok(self.users_where(Which::Name(username)).await?.pop())
    }

    /// Whether an enabled admin has a password: the rule every change
    /// keeps once setup is finished, so the server is never left to
    /// passkeys and providers alone.
    pub async fn an_admin_has_a_password(&self) -> Result<bool, UserError> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM users u JOIN password_credentials p ON p.user_id = u.id \
             WHERE u.role = 'admin' AND u.disabled_at IS NULL)",
        )
        .fetch_one(&self.pool)
        .await?)
    }

    /// Disable a user and revoke every session they have. Returns how many
    /// sessions were revoked. The last enabled admin stays enabled.
    pub async fn disable_user(&self, username: &str) -> Result<u64, UserError> {
        let name = normalize_username(username)?;
        let now = self.now();
        let mut tx = self.begin_write().await?;
        let id = user_id(&mut tx, &name).await?;
        keep_an_admin(&mut tx, id, &name).await?;
        sqlx::query("UPDATE users SET disabled_at = ?2 WHERE id = ?1 AND disabled_at IS NULL")
            .bind(id)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        // Enabling them again does not bring it back (P4-01).
        let revoked = end_sessions(&mut tx, id, now, None).await?;
        tx.commit().await?;
        self.note_sessions_ended();
        Ok(revoked)
    }

    /// A local user leaves the server from an app (PX-47): set aside, not
    /// deleted. Disabled and marked as left, every session ended and no
    /// device trusted, as [`Store::disable_user`] does; their data stays
    /// for an admin to restore ([`Store::enable_user`]). The last enabled
    /// admin, or the last with a password, cannot leave. How many
    /// sessions were ended.
    pub async fn leave_local(&self, id: LocalUserId) -> Result<u64, UserError> {
        let now = self.now();
        let mut tx = self.begin_write().await?;
        let name: Option<String> = sqlx::query_scalar("SELECT username FROM users WHERE id = ?1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
        let name = name.ok_or_else(|| UserError::NoSuchUser(id.to_string()))?;
        keep_an_admin(&mut tx, id, &name).await?;
        sqlx::query(
            "UPDATE users SET disabled_at = coalesce(disabled_at, ?2), left_at = ?2 \
             WHERE id = ?1",
        )
        .bind(id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        let revoked = end_sessions(&mut tx, id, now, None).await?;
        tx.commit().await?;
        self.note_sessions_ended();
        Ok(revoked)
    }

    /// Delete user `username` and every row kept for them, in one
    /// transaction: their sessions and ways to sign in, their devices'
    /// trust and PIN, their watch state, plays, likes, downloads, hidden
    /// rows, track choices and playlists, server-wide ones too. Whoever
    /// pinned a playlist of theirs gets a note, as for any playlist
    /// deleted. What others keep of what they did stays: the
    /// audit log, and who made an invite, downloaded a subtitle or timed
    /// one. The last enabled admin, or the last with a password, cannot be
    /// deleted. Their id, their downloads, which the caller lets go, and
    /// their playlists as they were, whose viewers the caller tells.
    pub async fn delete_user(&self, username: &str) -> Result<DeletedUser, UserError> {
        let name = normalize_username(username)?;
        let mut tx = self.begin_write().await?;
        let id = user_id(&mut tx, &name).await?;
        keep_an_admin(&mut tx, id, &name).await?;
        let principal = crate::principal::Principal::LocalUser(id).to_string();
        let downloads = super::downloads::delete_downloads_of(&mut tx, &principal).await?;
        let playlists = super::playlists::owned_by(&mut tx, &principal).await?;
        delete_principal_rows(&mut tx, &principal, self.now()).await?;
        // What only a local user keeps.
        for delete in [
            "DELETE FROM federation_downloads WHERE principal = ?1",
            "DELETE FROM federation_home_rows_hidden WHERE principal = ?1",
            "DELETE FROM federation_play_from WHERE principal = ?1",
            "DELETE FROM subtitle_download_log WHERE principal = ?1",
            "DELETE FROM subtitle_download_denied WHERE principal = ?1",
        ] {
            sqlx::query(delete)
                .bind(&principal)
                .execute(&mut *tx)
                .await?;
        }
        // Then what refers to the user's row; an app's grant takes its
        // codes and tokens with it.
        for delete in [
            "DELETE FROM sessions WHERE user_id = ?1",
            "DELETE FROM password_credentials WHERE user_id = ?1",
            "DELETE FROM pin_required WHERE user_id = ?1",
            "DELETE FROM pin_credentials WHERE user_id = ?1",
            "DELETE FROM passkeys WHERE user_id = ?1",
            "DELETE FROM passkey_users WHERE user_id = ?1",
            "DELETE FROM external_identities WHERE user_id = ?1",
            "DELETE FROM trusted_endpoints WHERE user_id = ?1",
            "DELETE FROM oauth_grants WHERE user_id = ?1",
            "DELETE FROM portable_ties WHERE local_user_id = ?1",
            "DELETE FROM legacy_user_ids WHERE new_id = ?1",
            "DELETE FROM users WHERE id = ?1",
        ] {
            sqlx::query(delete).bind(id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        self.note_sessions_ended();
        Ok(DeletedUser {
            id,
            downloads,
            playlists,
        })
    }

    /// Refuse taking local user `id` (`name`) out of the enabled admins
    /// now, as [`Store::delete_user`] would: `LastAdmin` or
    /// `LastPasswordAdmin`. Changes nothing.
    pub async fn check_admin_kept(&self, id: LocalUserId, name: &str) -> Result<(), UserError> {
        let mut conn = self.pool.acquire().await.map_err(StoreError::from)?;
        keep_an_admin(&mut conn, id, name).await
    }

    /// Whether local user `id` may leave the server now (PX-47): false
    /// exactly when [`Store::leave_local`] would refuse them as the last
    /// enabled admin, or the last enabled admin with a password, by the
    /// same check. Changes nothing; `WhoAmI` answers it, for an app to
    /// grey out "Leave this server" before it is asked.
    pub async fn can_leave(&self, id: LocalUserId) -> Result<bool, sqlx::Error> {
        let mut conn = self.pool.acquire().await?;
        Ok(admin_loss(&mut conn, id).await?.is_none())
    }

    /// Let a disabled user log in again, one who left (PX-47) too: their
    /// revoked sessions stay revoked.
    pub async fn enable_user(&self, username: &str) -> Result<(), UserError> {
        let name = normalize_username(username)?;
        let mut tx = self.begin_write().await?;
        let id = user_id(&mut tx, &name).await?;
        sqlx::query("UPDATE users SET disabled_at = NULL, left_at = NULL WHERE id = ?1")
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
        self.users_where(Which::All).await
    }

    /// The users `which` asks for: the same record whichever, each column
    /// worked out only for the rows asked for.
    async fn users_where(&self, which: Which<'_>) -> Result<Vec<UserRecord>, UserError> {
        macro_rules! select {
            () => {
                "SELECT u.id, u.username, u.display_name, u.role, u.created_at, \
                    u.disabled_at, u.left_at, \
                    p.user_id IS NOT NULL AS has_password, p.updated_at AS password_changed_at, \
                    coalesce(p.must_change, 0) AS password_must_change, \
                    p.expires_at AS password_expires_at, \
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
             FROM users u LEFT JOIN password_credentials p ON p.user_id = u.id "
            };
        }
        let rows = match which {
            Which::All => {
                sqlx::query(concat!(select!(), "ORDER BY u.username"))
                    .bind(self.now())
                    .fetch_all(&self.pool)
                    .await?
            }
            Which::Id(id) => {
                sqlx::query(concat!(select!(), "WHERE u.id = ?2"))
                    .bind(self.now())
                    .bind(id)
                    .fetch_all(&self.pool)
                    .await?
            }
            Which::Name(username) => {
                sqlx::query(concat!(select!(), "WHERE u.username = ?2"))
                    .bind(self.now())
                    .bind(username)
                    .fetch_all(&self.pool)
                    .await?
            }
        };
        let mut users = rows
            .into_iter()
            .map(|row| {
                Ok(UserRecord {
                    id: row.try_get("id")?,
                    username: row.try_get("username")?,
                    display_name: row.try_get("display_name")?,
                    role: role(row.try_get("role")?),
                    created_at: row.try_get("created_at")?,
                    disabled_at: row.try_get("disabled_at")?,
                    left_at: row.try_get("left_at")?,
                    has_password: row.try_get("has_password")?,
                    password_changed_at: row.try_get("password_changed_at")?,
                    password_must_change: row.try_get("password_must_change")?,
                    password_expires_at: row.try_get("password_expires_at")?,
                    last_seen_at: row.try_get("last_seen_at")?,
                    has_passkey: row.try_get("has_passkey")?,
                    has_sso: row.try_get("has_sso")?,
                    has_pin: row.try_get("has_pin")?,
                    device_count: row.try_get::<i64, _>("device_count")? as u64,
                    session_count: row.try_get::<i64, _>("session_count")? as u64,
                    ties: Vec::new(),
                })
            })
            .collect::<Result<Vec<_>, UserError>>()?;
        // Several ties to an account (P7-09) would repeat its row in a
        // join: read apart, and handed out by account.
        let mut ties = match which {
            Which::All => self.all_ties().await?,
            Which::Id(_) | Which::Name(_) => {
                let mut ties = std::collections::HashMap::new();
                for user in &users {
                    ties.insert(user.id, self.ties_of(user.id).await?);
                }
                ties
            }
        };
        for user in &mut users {
            user.ties = ties.remove(&user.id).unwrap_or_default();
        }
        Ok(users)
    }

    /// What a password login for `username` needs, or `None` if there is
    /// no such user.
    pub async fn password_login(&self, username: &str) -> Result<Option<PasswordLogin>, UserError> {
        let Ok(name) = normalize_username(username) else {
            return Ok(None);
        };
        let row = sqlx::query(
            "SELECT u.id, u.role, u.disabled_at IS NOT NULL AS disabled, p.phc, \
                    coalesce(p.must_change, 0) AS must_change, p.expires_at \
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
                must_change: row.try_get("must_change")?,
                expires_at: row.try_get("expires_at")?,
            })
        })
        .transpose()
    }
}

impl Store {
    /// Reset `username`'s password to the one-time `phc`, which they must
    /// change when they next sign in with it and which stops working at
    /// `expires_at`. Ends what rested on the old password, in one
    /// transaction: every live session of theirs signed in with it, or of
    /// unknown origin (made before sessions kept how they were signed in),
    /// and every device trusted on the same rule. Sessions signed in with
    /// a passkey, a sign-on provider or a portable identity's credential
    /// stay, as do the passkeys, provider links and ties themselves.
    pub async fn reset_password(
        &self,
        username: &str,
        phc: &str,
        expires_at: i64,
    ) -> Result<Reset, UserError> {
        let name = normalize_username(username)?;
        let now = self.now();
        let mut tx = self.begin_write().await?;
        let id = user_id(&mut tx, &name).await?;
        sqlx::query(
            "INSERT INTO password_credentials (user_id, phc, updated_at, must_change, expires_at) \
             VALUES (?1, ?2, ?3, 1, ?4) \
             ON CONFLICT (user_id) DO UPDATE SET phc = excluded.phc, \
               updated_at = excluded.updated_at, must_change = 1, \
               expires_at = excluded.expires_at",
        )
        .bind(id)
        .bind(phc)
        .bind(now)
        .bind(expires_at)
        .execute(&mut *tx)
        .await?;
        let revoked = sqlx::query(
            "UPDATE sessions SET revoked_at = ?2 \
             WHERE user_id = ?1 AND revoked_at IS NULL \
               AND (signed_in_with IS NULL OR signed_in_with = 'password')",
        )
        .bind(id)
        .bind(now)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let untrusted = sqlx::query(
            "DELETE FROM trusted_endpoints \
             WHERE user_id = ?1 AND (signed_in_with IS NULL OR signed_in_with = 'password')",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        self.note_sessions_ended();
        Ok(Reset { revoked, untrusted })
    }
}

/// End every live session of `user` but the one whose token hashes to
/// `keep`, and remove every device's trust for them (P4-01): their
/// credentials changed, or they were disabled, so nothing made under the
/// old state carries on. How many sessions were ended.
async fn end_sessions(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    user: LocalUserId,
    now: i64,
    keep: Option<&str>,
) -> Result<u64, sqlx::Error> {
    let revoked = sqlx::query(
        "UPDATE sessions SET revoked_at = ?2 \
         WHERE user_id = ?1 AND revoked_at IS NULL AND (?3 IS NULL OR token_hash IS NOT ?3)",
    )
    .bind(user)
    .bind(now)
    .bind(keep)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    sqlx::query("DELETE FROM trusted_endpoints WHERE user_id = ?1")
        .bind(user)
        .execute(&mut **tx)
        .await?;
    Ok(revoked)
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

/// Insert user `id` named `name` in `tx`: false if the name is taken.
/// An admin adding a user and an invite redeemed write the one row alike.
pub(super) async fn insert_user(
    tx: &mut sqlx::SqliteConnection,
    id: LocalUserId,
    name: &str,
    display_name: Option<&str>,
    role: Role,
    now: i64,
) -> Result<bool, sqlx::Error> {
    let inserted = sqlx::query(
        "INSERT INTO users (id, username, display_name, role, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(id)
    .bind(name)
    .bind(display_name)
    .bind(stored(role))
    .bind(now)
    .execute(tx)
    .await;
    match inserted {
        Ok(_) => Ok(true),
        Err(e) if is_unique_violation(&e) => Ok(false),
        Err(e) => Err(e),
    }
}

pub(super) async fn add_password(
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

/// Refuse to take `id` out of the enabled admins if it is the last one,
/// or the last one with a password. In the change's own transaction, so
/// two admins demoting each other at once cannot both succeed.
async fn keep_an_admin(
    tx: &mut sqlx::SqliteConnection,
    id: LocalUserId,
    name: &str,
) -> Result<(), UserError> {
    match admin_loss(tx, id).await? {
        None => Ok(()),
        Some(AdminLoss::LastAdmin) => Err(UserError::LastAdmin(name.to_owned())),
        Some(AdminLoss::LastPasswordAdmin) => Err(UserError::LastPasswordAdmin(name.to_owned())),
    }
}

/// What taking `id` out of the enabled admins would lose.
enum AdminLoss {
    /// The last enabled admin.
    LastAdmin,
    /// The last enabled admin with a password.
    LastPasswordAdmin,
}

/// Whether taking `id` out of the enabled admins would leave none, or
/// none with a password: [`keep_an_admin`]'s check, which
/// [`Store::can_leave`] asks without changing anything.
async fn admin_loss(
    conn: &mut sqlx::SqliteConnection,
    id: LocalUserId,
) -> Result<Option<AdminLoss>, sqlx::Error> {
    let (is_enabled_admin, has_password, others, others_with_password): (bool, bool, i64, i64) =
        sqlx::query_as(
            "SELECT \
               EXISTS (SELECT 1 FROM users \
                       WHERE id = ?1 AND role = 'admin' AND disabled_at IS NULL), \
               EXISTS (SELECT 1 FROM password_credentials WHERE user_id = ?1), \
               (SELECT count(*) FROM users \
                WHERE id != ?1 AND role = 'admin' AND disabled_at IS NULL), \
               (SELECT count(*) FROM users u JOIN password_credentials p ON p.user_id = u.id \
                WHERE u.id != ?1 AND u.role = 'admin' AND u.disabled_at IS NULL)",
        )
        .bind(id)
        .fetch_one(conn)
        .await?;
    if is_enabled_admin && others == 0 {
        return Ok(Some(AdminLoss::LastAdmin));
    }
    if is_enabled_admin && has_password && others_with_password == 0 {
        return Ok(Some(AdminLoss::LastPasswordAdmin));
    }
    Ok(None)
}

/// Refuse to take the password of `id` if it is the last enabled admin
/// with one.
async fn keep_a_password_admin(
    tx: &mut sqlx::SqliteConnection,
    id: LocalUserId,
    name: &str,
) -> Result<(), UserError> {
    let (is_enabled_admin, others_with_password): (bool, i64) = sqlx::query_as(
        "SELECT \
           EXISTS (SELECT 1 FROM users WHERE id = ?1 AND role = 'admin' AND disabled_at IS NULL), \
           (SELECT count(*) FROM users u JOIN password_credentials p ON p.user_id = u.id \
            WHERE u.id != ?1 AND u.role = 'admin' AND u.disabled_at IS NULL)",
    )
    .bind(id)
    .fetch_one(tx)
    .await?;
    if is_enabled_admin && others_with_password == 0 {
        return Err(UserError::LastPasswordAdmin(name.to_owned()));
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

/// Delete what `principal` (a user, local or portable, as text) kept here,
/// in `tx`: watch state, plays (PX-52), hidden rows, track preferences and
/// overrides, their playlists, pins, notes and places, and when they liked
/// what (PX-51). Whoever pinned a playlist of theirs gets a note, as for
/// any playlist deleted.
pub(super) async fn delete_principal_rows(
    tx: &mut sqlx::SqliteConnection,
    principal: &str,
    now: i64,
) -> Result<(), sqlx::Error> {
    super::playlists::note_pins(
        tx,
        principal,
        "p.owner = ?1",
        Some(principal),
        super::playlists::PinNoteReason::Deleted,
        now,
    )
    .await?;
    // The `_ids` tables first: they refer to their parents' rows. The rest
    // of a playlist's rows go with it.
    for delete in [
        "DELETE FROM watch_state_ids WHERE principal = ?1",
        "DELETE FROM watch_state WHERE principal = ?1",
        "DELETE FROM play_ids WHERE principal = ?1",
        "DELETE FROM plays WHERE principal = ?1",
        "DELETE FROM continue_hidden_ids WHERE principal = ?1",
        "DELETE FROM continue_hidden WHERE principal = ?1",
        "DELETE FROM track_overrides WHERE principal = ?1",
        "DELETE FROM track_preferences WHERE principal = ?1",
        "DELETE FROM playlists WHERE owner = ?1",
        "DELETE FROM playlist_shares WHERE principal = ?1",
        "DELETE FROM playlist_pins WHERE principal = ?1",
        "DELETE FROM playlist_pin_notes WHERE principal = ?1",
        "DELETE FROM playlist_places WHERE principal = ?1",
        "DELETE FROM liked_changes WHERE principal = ?1",
    ] {
        sqlx::query(delete)
            .bind(principal)
            .execute(&mut *tx)
            .await?;
    }
    Ok(())
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

    /// An admin adding a user is held to what a user may call themselves
    /// (review 2026-10-11 M23): no multi-line or over-long name reaches
    /// "Who's watching".
    #[tokio::test]
    async fn an_added_user_s_display_name_is_checked() {
        let store = store("add-display-name").await;
        for name in ["two\nlines", "x".repeat(MAX_DISPLAY_NAME_LEN + 1).as_str()] {
            assert!(
                matches!(
                    store
                        .add_user("bob", Some(name), "phc", Role::Consumer)
                        .await,
                    Err(UserError::InvalidDisplayName)
                ),
                "{name:?}"
            );
        }
        assert!(store.list_users().await.unwrap().is_empty());
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

    #[tokio::test]
    async fn the_last_enabled_admin_with_a_password_cannot_be_demoted_or_disabled() {
        let store = store("last-password-admin").await;
        store.add_first_admin("root", "phc").await.unwrap();
        store
            .add_user("alice", None, "phc", Role::Admin)
            .await
            .unwrap();
        sqlx::query(
            "DELETE FROM password_credentials \
             WHERE user_id = (SELECT id FROM users WHERE username = 'alice')",
        )
        .execute(&store.pool)
        .await
        .unwrap();
        assert!(store.an_admin_has_a_password().await.unwrap());
        // Alice, with no password, may go; root may not.
        assert!(matches!(
            store.set_role("root", Role::Consumer).await,
            Err(UserError::LastPasswordAdmin(_))
        ));
        assert!(matches!(
            store.disable_user("root").await,
            Err(UserError::LastPasswordAdmin(_))
        ));
        // Asked before a delete, it says which.
        let root = store.user_by_name("root").await.unwrap().unwrap().id;
        assert!(matches!(
            store.check_admin_kept(root, "root").await,
            Err(UserError::LastPasswordAdmin(_))
        ));
        assert!(matches!(
            store.delete_user("root").await,
            Err(UserError::LastPasswordAdmin(_))
        ));
        store.set_role("alice", Role::Consumer).await.unwrap();
        // Once another admin has one, root may go too.
        store
            .add_user("bob", None, "phc", Role::Admin)
            .await
            .unwrap();
        store.disable_user("root").await.unwrap();
        assert!(store.an_admin_has_a_password().await.unwrap());
    }

    #[tokio::test]
    async fn a_password_is_removed_only_while_another_way_in_and_another_admin_password_stay() {
        let store = store("remove-password").await;
        let root = store.add_first_admin("root", "phc").await.unwrap();
        let alice = store
            .add_user("alice", None, "phc", Role::Consumer)
            .await
            .unwrap();
        // No passkey, no provider: it is alice's only way in.
        assert!(matches!(
            store.remove_password("alice", None).await,
            Err(UserError::LastSignInMethod(_))
        ));
        store
            .link_identity(alice, "https://id.example.org", "sub-a", None)
            .await
            .unwrap();
        store.remove_password("alice", None).await.unwrap();
        assert!(matches!(
            store.remove_password("alice", None).await,
            Err(UserError::NoPassword(_))
        ));
        // Root has another way in, but is the only admin with a password.
        store
            .link_identity(root, "https://id.example.org", "sub-r", None)
            .await
            .unwrap();
        assert!(matches!(
            store.remove_password("root", None).await,
            Err(UserError::LastPasswordAdmin(_))
        ));
        store
            .add_user("bob", None, "phc", Role::Admin)
            .await
            .unwrap();
        store.remove_password("root", None).await.unwrap();
        assert!(store.an_admin_has_a_password().await.unwrap());
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

    /// Review 2026-10-11 M20: one user is read on its own, and is the
    /// same record the whole list has for it.
    #[tokio::test]
    async fn a_user_by_id_is_the_lists_record_of_them() {
        let store = store("by-id").await;
        let alice = store
            .add_user("alice", Some("Alice"), "phc", Role::Consumer)
            .await
            .unwrap();
        let bob = store
            .add_user("bob", None, "phc", Role::Admin)
            .await
            .unwrap();
        let listed = store.list_users().await.unwrap();
        for id in [alice, bob] {
            let one = store.user_by_id(id).await.unwrap();
            assert_eq!(one.as_ref(), listed.iter().find(|u| u.id == id));
        }
        let nobody = LocalUserId::from_uuid(uuid::Uuid::from_u128(99));
        assert_eq!(store.user_by_id(nobody).await.unwrap(), None);
        for name in ["alice", "bob"] {
            let one = store.user_by_name(name).await.unwrap();
            assert_eq!(one.as_ref(), listed.iter().find(|u| u.username == name));
        }
        assert_eq!(store.user_by_name("mallory").await.unwrap(), None);
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

    /// Columns that keep what a user did after they are deleted: records
    /// that outlive what they name.
    const KEPT: &[(&str, &str)] = &[
        ("audit_events", "actor"),
        ("audit_events", "target"),
        ("invites", "created_by"),
        ("subtitle_downloads", "downloaded_by"),
        ("subtitle_rejections", "rejected_by"),
        ("subtitle_timings", "offset_by"),
        ("subtitle_download_denied", "denied_by"),
        ("optimized_requests", "created_by"),
        ("federation_grants", "granted_by"),
        ("federation_peers", "paired_by"),
        ("federation_peers", "expected_by"),
    ];

    /// Every column of every table, but [`KEPT`], that holds `id` as a
    /// user id or a principal, and its rows.
    async fn rows_naming(store: &Store, id: LocalUserId) -> Vec<(String, String, i64)> {
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
             AND name != '_sqlx_migrations'",
        )
        .fetch_all(&store.pool)
        .await
        .unwrap();
        let principal = crate::principal::Principal::LocalUser(id).to_string();
        let mut naming = Vec::new();
        for table in tables {
            let columns: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info(?1)")
                .bind(&table)
                .fetch_all(&store.pool)
                .await
                .unwrap();
            for column in columns {
                if KEPT.contains(&(table.as_str(), column.as_str())) {
                    continue;
                }
                let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                    "SELECT count(*) FROM \"{table}\" WHERE \"{column}\" IN (?1, ?2)"
                )))
                .bind(id.to_string())
                .bind(&principal)
                .fetch_one(&store.pool)
                .await
                .unwrap();
                if n > 0 {
                    naming.push((table.clone(), column, n));
                }
            }
        }
        naming
    }

    /// A row of `id`'s in every table that keeps one.
    async fn fill(store: &Store, id: LocalUserId) {
        let p = crate::principal::Principal::LocalUser(id).to_string();
        let u = id.to_string();
        for (sql, binds) in [
            (
                "INSERT INTO devices (endpoint_id, first_seen_at, last_seen_at) VALUES ('e', 0, 0)",
                vec![],
            ),
            (
                "INSERT INTO sessions (token_hash, user_id, kind, endpoint_id, created_at, \
                 expires_at) VALUES ('h', ?1, 'endpoint', 'e', 0, 1)",
                vec![&u],
            ),
            (
                "INSERT INTO pin_credentials (user_id, phc, updated_at) VALUES (?1, 'pin', 0)",
                vec![&u],
            ),
            (
                "INSERT INTO pin_required (user_id, endpoint_id, set_at) VALUES (?1, 'e', 0)",
                vec![&u],
            ),
            (
                "INSERT INTO trusted_endpoints (user_id, endpoint_id, created_at, last_used_at) \
                 VALUES (?1, 'e', 0, 0)",
                vec![&u],
            ),
            (
                "INSERT INTO passkey_users (user_id, handle) VALUES (?1, 'handle')",
                vec![&u],
            ),
            (
                "INSERT INTO passkeys (credential_id, user_id, name, credential, rp_id, \
                 created_at) VALUES ('c', ?1, 'Phone', '{}', 'rp', 0)",
                vec![&u],
            ),
            (
                "INSERT INTO external_identities (user_id, issuer, subject, linked_at) \
                 VALUES (?1, 'https://idp', 'sub', 0)",
                vec![&u],
            ),
            (
                "INSERT INTO oauth_clients (id, name, redirect_uris, created_at) \
                 VALUES ('app', 'App', '[]', 0)",
                vec![],
            ),
            (
                "INSERT INTO oauth_grants (id, client_id, user_id, scopes, redirect_uri, \
                 created_at) VALUES ('g', 'app', ?1, 'read', 'https://app', 0)",
                vec![&u],
            ),
            (
                "INSERT INTO oauth_tokens (token_hash, grant_id, kind, expires_at) \
                 VALUES ('t', 'g', 'access', 1)",
                vec![],
            ),
            (
                "INSERT INTO portable_ties (portable_user_id, local_user_id, tied_at, \
                 proof_kind) VALUES ('pu', ?1, 0, 'genesis')",
                vec![&u],
            ),
            (
                "INSERT INTO legacy_user_ids (old_id, new_id) VALUES (7, ?1)",
                vec![&u],
            ),
            (
                "INSERT INTO watch_state (principal, media_id, updated_at) VALUES (?1, 'm', 0)",
                vec![&p],
            ),
            (
                "INSERT INTO watch_state_ids (principal, media_id, external_id) \
                 VALUES (?1, 'm', 'x')",
                vec![&p],
            ),
            (
                "INSERT INTO plays (principal, media_id, kind, started_at, ended_at) \
                 VALUES (?1, 'm', 'movie', 0, 0)",
                vec![&p],
            ),
            (
                "INSERT INTO play_ids (principal, media_id, external_id) VALUES (?1, 'm', 'x')",
                vec![&p],
            ),
            (
                "INSERT INTO continue_hidden (principal, media_id, hidden_at) VALUES (?1, 'm', 0)",
                vec![&p],
            ),
            (
                "INSERT INTO continue_hidden_ids (principal, media_id, external_id) \
                 VALUES (?1, 'm', 'x')",
                vec![&p],
            ),
            (
                "INSERT INTO track_preferences (principal, preferences, updated_at) \
                 VALUES (?1, '{}', 0)",
                vec![&p],
            ),
            (
                "INSERT INTO track_overrides (principal, scope, override, updated_at) \
                 VALUES (?1, 's', '{}', 0)",
                vec![&p],
            ),
            (
                "INSERT INTO liked_changes (principal, media_id, liked, at) VALUES (?1, 'm', 1, 0)",
                vec![&p],
            ),
            (
                "INSERT INTO downloads (id, principal, media_id, request, content_key, state, \
                 created_at, carried, stamp) \
                 VALUES ('d', ?1, 'm', '{}', 'k', 'waiting', 0, '[]', 's')",
                vec![&p],
            ),
            (
                "INSERT INTO federation_downloads (id, principal, origin, library, item, \
                 remote_id, state, created_at, updated_at) \
                 VALUES ('fd', ?1, 'o', 'l', 'i', 'r', '{}', 0, 0)",
                vec![&p],
            ),
            (
                "INSERT INTO federation_home_rows_hidden (principal, origin, hidden_at) \
                 VALUES (?1, 'o', 0)",
                vec![&p],
            ),
            (
                "INSERT INTO federation_play_from (principal, kind, external_id, server, \
                 chosen_at) VALUES (?1, 'movie', 'x', 's', 0)",
                vec![&p],
            ),
            (
                "INSERT INTO subtitle_download_log (principal, at) VALUES (?1, 0)",
                vec![&p],
            ),
            (
                "INSERT INTO subtitle_download_denied (principal, denied_by, denied_at) \
                 VALUES (?1, 'root', 0)",
                vec![&p],
            ),
            (
                "INSERT INTO playlist_pin_notes (principal, playlist_id, name, reason, noted_at) \
                 VALUES (?1, 'gone', 'Old', 'deleted', 0)",
                vec![&p],
            ),
        ] {
            let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
            for bind in binds {
                query = query.bind(bind.as_str());
            }
            query.execute(&store.pool).await.unwrap();
        }
    }

    /// A deleted user leaves no row that names them, but the records
    /// kept of what they did; another user's rows stay.
    #[tokio::test]
    async fn a_deleted_user_leaves_no_row_naming_them() {
        use crate::store::playlists::{NewPlaylist, ShareRow};
        let store = store("delete-user").await;
        store.add_first_admin("root", "phc").await.unwrap();
        let alice = store
            .add_user("alice", None, "phc", Role::Consumer)
            .await
            .unwrap();
        let bob = store
            .add_user("bob", None, "phc", Role::Consumer)
            .await
            .unwrap();
        let (a, b) = (
            crate::principal::Principal::LocalUser(alice).to_string(),
            crate::principal::Principal::LocalUser(bob).to_string(),
        );
        fill(&store, alice).await;
        let kinds = spoolframe_protocol::playlists::Kinds {
            movies: true,
            episodes: false,
            tracks: false,
        };
        let playlist = |owner: &str, server_wide, shares| NewPlaylist {
            owner: owner.into(),
            name: "Heists".into(),
            kinds,
            server_wide,
            shares,
        };
        let hers = store
            .create_playlist(&playlist(
                &a,
                false,
                vec![ShareRow {
                    principal: b.clone(),
                    can_edit: false,
                }],
            ))
            .await
            .unwrap();
        let everyones = store
            .create_playlist(&playlist(&a, true, vec![]))
            .await
            .unwrap();
        let bobs = store
            .create_playlist(&playlist(
                &b,
                false,
                vec![ShareRow {
                    principal: a.clone(),
                    can_edit: true,
                }],
            ))
            .await
            .unwrap();
        store.pin_playlist(&b, &hers.id, 0).await.unwrap();
        store.pin_playlist(&b, &everyones.id, 1).await.unwrap();
        store.pin_playlist(&a, &bobs.id, 0).await.unwrap();
        sqlx::query(
            "INSERT INTO playlist_places (principal, playlist_id, updated_at) VALUES (?1, ?2, 0)",
        )
        .bind(&a)
        .bind(&bobs.id)
        .execute(&store.pool)
        .await
        .unwrap();

        let deleted = store.delete_user("alice").await.unwrap();

        assert_eq!(rows_naming(&store, alice).await, vec![]);
        assert_eq!(
            deleted
                .downloads
                .iter()
                .map(|d| d.id.as_str())
                .collect::<Vec<_>>(),
            ["d"]
        );
        assert!(store.user_by_id(alice).await.unwrap().is_none());
        assert!(store.playlist(&hers.id).await.unwrap().is_none());
        // A server-wide one of hers goes too; Bob's is his.
        assert!(store.playlist(&everyones.id).await.unwrap().is_none());
        assert!(
            store
                .playlist(&bobs.id)
                .await
                .unwrap()
                .unwrap()
                .shares
                .is_empty()
        );
        // Bob had pinned both of hers: he is told each went.
        assert_eq!(store.pin_notes(&b).await.unwrap().len(), 2);
        assert!(store.user_by_id(bob).await.unwrap().is_some());
    }

    /// The last enabled admin cannot be deleted; nothing of theirs goes.
    #[tokio::test]
    async fn the_last_enabled_admin_cannot_be_deleted() {
        let store = store("delete-last-admin").await;
        let root = store.add_first_admin("root", "phc").await.unwrap();
        assert!(matches!(
            store.delete_user("root").await,
            Err(UserError::LastAdmin(_))
        ));
        assert!(store.user_by_id(root).await.unwrap().is_some());
        assert!(matches!(
            store.delete_user("nobody").await,
            Err(UserError::NoSuchUser(_))
        ));
    }
}
