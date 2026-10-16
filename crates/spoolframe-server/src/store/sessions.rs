//! Sessions: what a successful login leaves behind. Rows are keyed by a
//! hash of the token, so the database alone cannot be used to act as a
//! user.

use spoolframe_types::{Platform, Role};

use super::users::{LocalUserId, role};
use super::{Store, StoreError};

/// What a session is bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionBinding {
    /// An Iroh client endpoint (hex `EndpointId`); usable only from it
    /// (P1-12). `device_name` is what the client calls itself.
    Endpoint {
        endpoint_id: String,
        device_name: Option<String>,
        /// What kind of client it runs, from its `Hello`.
        platform: Platform,
    },
    /// The admin API: the CLI, and the web UI (P3-01).
    Api,
}

/// How a session was signed in: what a password reset looks at to end
/// only the sessions that rest on the password. A session a device
/// approval or a trusted device's resume made carries its parent's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignedInWith {
    /// The account password, including the one an invite's redemption or
    /// the first-admin setup chose.
    Password,
    /// A passkey, on the web or in an app.
    Passkey,
    /// A single sign-on provider (OIDC).
    Sso,
    /// A portable identity's device credential (P7-06).
    PortableCredential,
}

impl SignedInWith {
    pub fn as_str(self) -> &'static str {
        match self {
            SignedInWith::Password => "password",
            SignedInWith::Passkey => "passkey",
            SignedInWith::Sso => "sso",
            SignedInWith::PortableCredential => "portable_credential",
        }
    }

    /// What the database wrote; `None` for NULL (made before it was kept,
    /// or from a parent of unknown origin) and for anything else.
    pub fn from_column(stored: Option<&str>) -> Option<Self> {
        match stored? {
            "password" => Some(SignedInWith::Password),
            "passkey" => Some(SignedInWith::Passkey),
            "sso" => Some(SignedInWith::Sso),
            "portable_credential" => Some(SignedInWith::PortableCredential),
            _ => None,
        }
    }
}

/// A live API session's user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApiSessionUser {
    pub user: LocalUserId,
    /// Read from the user on each request, so a change applies at once.
    pub role: Role,
    /// When they last proved who they are in this session (Unix seconds):
    /// at sign-in, or since ([`Store::confirm_session`]).
    pub authenticated_at: i64,
    /// How the session was signed in; `None` when not known.
    pub signed_in_with: Option<SignedInWith>,
    /// Signed in with a one-time password an admin reset to: the session
    /// may only change the password, or sign out.
    pub password_change_only: bool,
}

/// Whose a session is, for the refused-request log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionOwner {
    pub username: String,
    /// The device's endpoint it is bound to; `None` for an API session.
    pub endpoint_id: Option<String>,
    /// Not revoked, not expired, and its user not disabled.
    pub live: bool,
}

/// The user behind a live endpoint session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionUser {
    pub user: LocalUserId,
    pub username: String,
    /// The display name, or the username when there is none.
    pub display_name: String,
    pub role: Role,
    /// The user's PIN is asked for on this session's device (P2-08).
    pub pin_required: bool,
    /// When they last proved who they are in this session (Unix seconds):
    /// at sign-in.
    pub authenticated_at: i64,
    /// How the session was signed in; `None` when not known.
    pub signed_in_with: Option<SignedInWith>,
    /// As [`ApiSessionUser::password_change_only`].
    pub password_change_only: bool,
}

/// One of a user's live sessions, as the user may see it: never the token.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct SessionRecord {
    /// To recognise the caller's own session; not shown to clients.
    pub token_hash: String,
    /// `None` for an admin API session.
    pub device_name: Option<String>,
    pub endpoint_id: Option<String>,
    /// The device's [`Platform`] by name, or for an API session what
    /// signed it in; see [`SessionRecord::platform`].
    platform: Option<String>,
    pub created_at: i64,
    pub last_used_at: i64,
}

impl SessionRecord {
    /// What kind of client the session's device runs, or for an API
    /// session [`Platform::Web`] or [`Platform::Cli`]; `None` for a device
    /// or an API session from before the server began keeping it.
    pub fn platform(&self) -> Option<Platform> {
        self.platform.as_deref().map(Platform::from_name)
    }
}

/// A session's use is written back (renewing its expiry) at most this
/// often, so busy clients do not turn every request into a write.
pub const RENEW_EVERY_SECS: i64 = 60;

/// How long a dead session (revoked or expired) is kept before it is
/// purged: a month, so recent sign-outs are still there to look into.
pub const DEAD_SESSION_KEEP_SECS: i64 = 30 * 86_400;

/// How often dead sessions are purged.
const PURGE_EVERY: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

/// When and how a session being recorded was signed in.
#[derive(Debug, Clone, Copy)]
pub(super) struct Stamp {
    pub now: i64,
    /// When its user last proved who they are: `now` for a sign-in, 0 for
    /// a session nobody proved anything for (a resume, a credential).
    pub authenticated_at: i64,
    pub signed_in_with: Option<SignedInWith>,
}

impl Store {
    /// Record a session for `user`, expiring at `expires_at`, signed in
    /// as `signed_in_with` says (`None`: from a parent of unknown origin).
    pub async fn create_session(
        &self,
        token_hash: &str,
        user: LocalUserId,
        binding: &SessionBinding,
        expires_at: i64,
        signed_in_with: Option<SignedInWith>,
    ) -> Result<(), StoreError> {
        let now = self.now();
        let mut tx = self.begin_write().await?;
        Self::insert_session(
            &mut tx,
            token_hash,
            user,
            binding,
            expires_at,
            Stamp {
                now,
                authenticated_at: now,
                signed_in_with,
            },
            None,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Record a session for `user` opened with a portable identity's
    /// device credential, the account being tied to it (P7-06): expiring
    /// at `expires_at` or when the credential does, whichever is first,
    /// never renewed past it, and revoked with the credential's key. Its
    /// user did not prove who they are to this server here: it counts as
    /// signed in long ago, so what needs a recent sign-in asks them to
    /// confirm first, as after a trusted device's resume.
    pub async fn create_credential_session(
        &self,
        token_hash: &str,
        user: LocalUserId,
        binding: &SessionBinding,
        expires_at: i64,
        credential: &super::grants::CredentialBinding,
    ) -> Result<(), StoreError> {
        let now = self.now();
        let mut tx = self.begin_write().await?;
        Self::insert_session(
            &mut tx,
            token_hash,
            user,
            binding,
            expires_at.min(credential.expires_at),
            Stamp {
                now,
                authenticated_at: 0,
                signed_in_with: Some(SignedInWith::PortableCredential),
            },
            Some(credential),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// [`Store::create_session`] inside `tx`, as `stamp` says.
    pub(super) async fn insert_session(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        token_hash: &str,
        user: LocalUserId,
        binding: &SessionBinding,
        expires_at: i64,
        stamp: Stamp,
        credential: Option<&super::grants::CredentialBinding>,
    ) -> Result<(), StoreError> {
        let Stamp {
            now,
            authenticated_at,
            signed_in_with,
        } = stamp;
        let (kind, endpoint) = match binding {
            SessionBinding::Endpoint {
                endpoint_id,
                device_name,
                platform,
            } => {
                // The device row appears on the first login from an
                // endpoint, and keeps the latest name and platform the
                // client gave.
                sqlx::query(
                    "INSERT INTO devices \
                       (endpoint_id, display_name, first_seen_at, last_seen_at, platform) \
                     VALUES (?1, ?2, ?3, ?3, ?4) \
                     ON CONFLICT (endpoint_id) DO UPDATE SET \
                       display_name = coalesce(excluded.display_name, devices.display_name), \
                       last_seen_at = excluded.last_seen_at, \
                       platform = excluded.platform",
                )
                .bind(endpoint_id)
                .bind(device_name)
                .bind(now)
                .bind(platform.as_str())
                .execute(&mut **tx)
                .await?;
                ("endpoint", Some(endpoint_id.as_str()))
            }
            SessionBinding::Api => ("api", None),
        };
        // The web UI says it made one afterwards (`mark_web_session`).
        let client = matches!(binding, SessionBinding::Api).then_some(Platform::Cli.as_str());
        sqlx::query(
            "INSERT INTO sessions \
               (token_hash, user_id, kind, endpoint_id, created_at, last_used_at, \
                authenticated_at, expires_at, client, signed_in_with, \
                portable_user_id, portable_device_key, portable_expires_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?8, ?6, ?7, ?9, ?10, ?11, ?12)",
        )
        .bind(token_hash)
        .bind(user)
        .bind(kind)
        .bind(endpoint)
        .bind(now)
        .bind(expires_at)
        .bind(client)
        .bind(authenticated_at)
        .bind(signed_in_with.map(SignedInWith::as_str))
        .bind(credential.map(|c| c.user.to_string()))
        .bind(credential.map(|c| c.device_key.to_string()))
        .bind(credential.map(|c| c.expires_at))
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// The user of a live API session: not revoked, not expired, and the
    /// user not disabled. Changes nothing; [`Store::use_api_session`] is a
    /// use.
    pub async fn api_session(
        &self,
        token_hash: &str,
    ) -> Result<Option<ApiSessionUser>, StoreError> {
        Ok(self
            .fetch_api_session(token_hash, self.now())
            .await?
            .map(|(user, _)| user))
    }

    /// [`Store::api_session`], as a use: the session is renewed to
    /// `idle_secs` from now, but never past `max_secs` after it was made,
    /// written at most once a [`RENEW_EVERY_SECS`] (A-18).
    pub async fn use_api_session(
        &self,
        token_hash: &str,
        idle_secs: i64,
        max_secs: i64,
    ) -> Result<Option<ApiSessionUser>, StoreError> {
        let now = self.now();
        let Some((user, last_used_at)) = self.fetch_api_session(token_hash, now).await? else {
            return Ok(None);
        };
        if now - last_used_at >= RENEW_EVERY_SECS {
            self.renew_api_session(token_hash, now, idle_secs, max_secs)
                .await?;
        }
        Ok(Some(user))
    }

    /// The live API session whose token hashes to `token_hash` at `now`,
    /// and when it was last used: one rule for what a live one is.
    async fn fetch_api_session(
        &self,
        token_hash: &str,
        now: i64,
    ) -> Result<Option<(ApiSessionUser, i64)>, StoreError> {
        type Row = (LocalUserId, String, i64, i64, Option<String>, bool);
        let row: Option<Row> = sqlx::query_as(
            "SELECT u.id, u.role, s.authenticated_at, s.last_used_at, s.signed_in_with, \
                    s.password_change_only \
             FROM sessions s JOIN users u ON u.id = s.user_id \
             WHERE s.token_hash = ?1 AND s.kind = 'api' AND s.revoked_at IS NULL \
               AND s.expires_at > ?2 AND u.disabled_at IS NULL",
        )
        .bind(token_hash)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(
            |(id, stored, authenticated_at, last_used_at, with, password_change_only)| {
                let user = ApiSessionUser {
                    user: id,
                    role: role(stored),
                    authenticated_at,
                    signed_in_with: SignedInWith::from_column(with.as_deref()),
                    password_change_only,
                };
                (user, last_used_at)
            },
        ))
    }

    /// Write the renewal [`Store::use_api_session`] found due, unless
    /// another request wrote it since: requests that arrive together all
    /// read the same stale `last_used_at`, and only the first of them
    /// writes (each write waits for the one before it, and a stalled disk
    /// held four web UI requests for 2.3 s). Whether this one wrote.
    async fn renew_api_session(
        &self,
        token_hash: &str,
        now: i64,
        idle_secs: i64,
        max_secs: i64,
    ) -> Result<bool, StoreError> {
        let renewed = sqlx::query(
            "UPDATE sessions SET last_used_at = ?2, expires_at = min(?3, created_at + ?4) \
             WHERE token_hash = ?1 AND last_used_at <= ?5",
        )
        .bind(token_hash)
        .bind(now)
        .bind(now + idle_secs)
        .bind(max_secs)
        .bind(now - RENEW_EVERY_SECS)
        .execute(&self.pool)
        .await?;
        Ok(renewed.rows_affected() > 0)
    }

    /// Write the renewal [`Store::use_endpoint_session`] found due, with
    /// the device's and its trust's last use, unless another request wrote
    /// it since: only the first of requests that arrive together writes,
    /// as for [`Store::renew_api_session`]. Whether this one wrote.
    async fn renew_endpoint_session(
        &self,
        user: LocalUserId,
        token_hash: &str,
        endpoint_id: &str,
        now: i64,
        lifetime_secs: i64,
    ) -> Result<bool, StoreError> {
        let mut tx = self.begin_write().await?;
        let renewed = sqlx::query(
            "UPDATE sessions SET last_used_at = ?2, \
               expires_at = min(?3, coalesce(portable_expires_at, ?3)) \
             WHERE token_hash = ?1 AND last_used_at <= ?4",
        )
        .bind(token_hash)
        .bind(now)
        .bind(now + lifetime_secs)
        .bind(now - RENEW_EVERY_SECS)
        .execute(&mut *tx)
        .await?;
        if renewed.rows_affected() == 0 {
            return Ok(false);
        }
        sqlx::query("UPDATE devices SET last_seen_at = ?2 WHERE endpoint_id = ?1")
            .bind(endpoint_id)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        // A device in use is not idle: its trust does not lapse (P4-01).
        sqlx::query(
            "UPDATE trusted_endpoints SET last_used_at = ?3 \
             WHERE user_id = ?1 AND endpoint_id = ?2",
        )
        .bind(user)
        .bind(endpoint_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Whose session `token_hash` is, live or not, and where it is bound:
    /// what the refused-request log names (finding 46). Changes nothing.
    pub async fn session_owner(
        &self,
        token_hash: &str,
    ) -> Result<Option<SessionOwner>, StoreError> {
        let row: Option<(String, Option<String>, bool)> = sqlx::query_as(
            "SELECT u.username, s.endpoint_id, \
                    s.revoked_at IS NULL AND s.expires_at > ?2 AND u.disabled_at IS NULL \
             FROM sessions s JOIN users u ON u.id = s.user_id \
             WHERE s.token_hash = ?1",
        )
        .bind(token_hash)
        .bind(self.now())
        .fetch_optional(&self.pool)
        .await?;
        if let Some((username, endpoint_id, live)) = row {
            return Ok(Some(SessionOwner {
                username,
                endpoint_id,
                live,
            }));
        }
        // A portable user's (P7-06), named by their fingerprint.
        Ok(self
            .portable_session_owner(token_hash)
            .await?
            .map(|(user, endpoint_id, live)| SessionOwner {
                username: format!("portable:{}", user.fingerprint()),
                endpoint_id: Some(endpoint_id),
                live,
            }))
    }

    /// Limit session `token_hash` to changing its user's password, or
    /// signing out: one a one-time password opened.
    pub async fn restrict_to_password_change(&self, token_hash: &str) -> Result<(), StoreError> {
        sqlx::query("UPDATE sessions SET password_change_only = 1 WHERE token_hash = ?1")
            .bind(token_hash)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The session's user proved who they are again, now.
    pub async fn confirm_session(&self, token_hash: &str) -> Result<(), StoreError> {
        sqlx::query("UPDATE sessions SET authenticated_at = ?2 WHERE token_hash = ?1")
            .bind(token_hash)
            .bind(self.now())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Make the session's sign-in `secs` older than it is.
    #[cfg(test)]
    pub(crate) async fn age_session(&self, token_hash: &str, secs: i64) {
        sqlx::query(
            "UPDATE sessions SET authenticated_at = authenticated_at - ?2 WHERE token_hash = ?1",
        )
        .bind(token_hash)
        .bind(secs)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    /// The user of a live endpoint session used from `endpoint_id`: the
    /// session's own endpoint, not revoked, not expired, the user not
    /// disabled. A use renews it to `lifetime_secs` from now, written at
    /// most once a [`RENEW_EVERY_SECS`].
    pub async fn use_endpoint_session(
        &self,
        token_hash: &str,
        endpoint_id: &str,
        lifetime_secs: i64,
    ) -> Result<Option<SessionUser>, StoreError> {
        let now = self.now();
        type Row = (
            LocalUserId,
            String,
            Option<String>,
            String,
            i64,
            bool,
            i64,
            Option<String>,
            bool,
        );
        let row: Option<Row> = sqlx::query_as(
            "SELECT u.id, u.username, u.display_name, u.role, s.last_used_at, \
                    EXISTS (SELECT 1 FROM pin_credentials p WHERE p.user_id = u.id) \
                    AND EXISTS (SELECT 1 FROM pin_required r \
                                WHERE r.user_id = u.id AND r.endpoint_id = s.endpoint_id), \
                    s.authenticated_at, s.signed_in_with, s.password_change_only \
             FROM sessions s JOIN users u ON u.id = s.user_id \
             WHERE s.token_hash = ?1 AND s.kind = 'endpoint' AND s.endpoint_id = ?2 \
               AND s.revoked_at IS NULL AND s.expires_at > ?3 AND u.disabled_at IS NULL \
               AND NOT EXISTS (SELECT 1 FROM portable_revoked_keys r \
                               WHERE r.user_id = s.portable_user_id \
                                 AND r.device_key = s.portable_device_key)",
        )
        .bind(token_hash)
        .bind(endpoint_id)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;
        let Some((
            id,
            username,
            display_name,
            stored,
            last_used_at,
            pin_required,
            authenticated_at,
            with,
            password_change_only,
        )) = row
        else {
            return Ok(None);
        };
        if now - last_used_at >= RENEW_EVERY_SECS {
            self.renew_endpoint_session(id, token_hash, endpoint_id, now, lifetime_secs)
                .await?;
        }
        Ok(Some(SessionUser {
            user: id,
            display_name: display_name.unwrap_or_else(|| username.clone()),
            username,
            role: role(stored),
            pin_required,
            authenticated_at,
            signed_in_with: SignedInWith::from_column(with.as_deref()),
            password_change_only,
        }))
    }

    /// How many live sessions each enabled user has, by binding (P3-14):
    /// `(user, app, web)`, where an app session is bound to a device's
    /// endpoint and a web one is the admin API's (the CLI's or a browser's).
    pub async fn live_session_counts(&self) -> Result<Vec<(LocalUserId, u64, u64)>, StoreError> {
        let rows: Vec<(LocalUserId, i64, i64)> = sqlx::query_as(
            "SELECT s.user_id, \
                    sum(s.kind = 'endpoint'), sum(s.kind = 'api') \
             FROM sessions s JOIN users u ON u.id = s.user_id \
             WHERE s.revoked_at IS NULL AND s.expires_at > ?1 AND u.disabled_at IS NULL \
             GROUP BY s.user_id ORDER BY s.user_id",
        )
        .bind(self.now())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(user, app, web)| (user, app.max(0) as u64, web.max(0) as u64))
            .collect())
    }

    /// Mark the admin API session `token_hash` as the web UI's (P3-18):
    /// what set it as a browser's cookie.
    pub async fn mark_web_session(&self, token_hash: &str) -> Result<(), StoreError> {
        sqlx::query("UPDATE sessions SET client = ?2 WHERE token_hash = ?1 AND kind = 'api'")
            .bind(token_hash)
            .bind(Platform::Web.as_str())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// `user`'s live sessions, on every device, most recently used first.
    pub async fn list_sessions(&self, user: LocalUserId) -> Result<Vec<SessionRecord>, StoreError> {
        Ok(sqlx::query_as(
            "SELECT s.token_hash, d.display_name AS device_name, s.endpoint_id, \
                    CASE s.kind WHEN 'api' THEN s.client ELSE d.platform END AS platform, \
                    s.created_at, s.last_used_at \
             FROM sessions s LEFT JOIN devices d ON d.endpoint_id = s.endpoint_id \
             WHERE s.user_id = ?1 AND s.revoked_at IS NULL AND s.expires_at > ?2 \
             ORDER BY s.last_used_at DESC",
        )
        .bind(user)
        .bind(self.now())
        .fetch_all(&self.pool)
        .await?)
    }

    /// The name device `endpoint_id` gave when someone signed in from it;
    /// `None` for a device no one signed in from, or one with no name.
    pub async fn device_name(&self, endpoint_id: &str) -> Result<Option<String>, StoreError> {
        Ok(
            sqlx::query_scalar("SELECT display_name FROM devices WHERE endpoint_id = ?1")
                .bind(endpoint_id)
                .fetch_optional(&self.pool)
                .await?
                .flatten(),
        )
    }

    /// What kind of client `endpoint_id` runs now, as its `Hello` said, if
    /// it is a device someone has signed in from: so a device signed in
    /// before the server kept platforms gets one when it next connects.
    pub async fn note_device_platform(
        &self,
        endpoint_id: &str,
        platform: Platform,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE devices SET platform = ?2 WHERE endpoint_id = ?1 AND platform IS NOT ?2",
        )
        .bind(endpoint_id)
        .bind(platform.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Revoke every live session of `user`'s but `keep` (a token hash):
    /// "sign out everywhere else" (P3-03). Every other device's trust for
    /// them goes too (P4-01), or those devices would resume at once. How
    /// many were revoked.
    pub async fn revoke_other_sessions(
        &self,
        user: LocalUserId,
        keep: &str,
    ) -> Result<u64, StoreError> {
        let mut tx = self.begin_write().await?;
        let revoked = sqlx::query(
            "UPDATE sessions SET revoked_at = ?3 \
             WHERE user_id = ?1 AND token_hash != ?2 AND revoked_at IS NULL",
        )
        .bind(user)
        .bind(keep)
        .bind(self.now())
        .execute(&mut *tx)
        .await?
        .rows_affected();
        sqlx::query(
            "DELETE FROM trusted_endpoints WHERE user_id = ?1 AND endpoint_id IS NOT \
               (SELECT endpoint_id FROM sessions WHERE token_hash = ?2)",
        )
        .bind(user)
        .bind(keep)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        self.note_sessions_ended();
        Ok(revoked)
    }

    /// Revoke one session: signed out, or ended by its user or an admin.
    /// Its device's trust for its user goes with it (P4-01): signing out
    /// forgets the device, and a session someone ended must not come back
    /// as a resume. Revoking an unknown or already revoked one is not an
    /// error.
    pub async fn revoke_session(&self, token_hash: &str) -> Result<(), StoreError> {
        let mut tx = self.begin_write().await?;
        sqlx::query(
            "DELETE FROM trusted_endpoints WHERE (user_id, endpoint_id) IN \
               (SELECT user_id, endpoint_id FROM sessions \
                WHERE token_hash = ?1 AND kind = 'endpoint')",
        )
        .bind(token_hash)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE sessions SET revoked_at = ?2 WHERE token_hash = ?1 AND revoked_at IS NULL",
        )
        .bind(token_hash)
        .bind(self.now())
        .execute(&mut *tx)
        .await?;
        // Or a portable user's (P7-06): the token says which, not the
        // caller.
        sqlx::query(
            "UPDATE portable_sessions SET revoked_at = ?2 \
             WHERE token_hash = ?1 AND revoked_at IS NULL",
        )
        .bind(token_hash)
        .bind(self.now())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        self.note_sessions_ended();
        Ok(())
    }

    /// Revoke a session nobody holds: one made for a device sign-in whose
    /// code ran out before the device was handed it. Unlike
    /// [`Store::revoke_session`], the device's trust stays: the device
    /// never had this session, and its user did not sign out there.
    pub async fn discard_unheld_session(&self, token_hash: &str) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE sessions SET revoked_at = ?2 WHERE token_hash = ?1 AND revoked_at IS NULL",
        )
        .bind(token_hash)
        .bind(self.now())
        .execute(&self.pool)
        .await?;
        self.note_sessions_ended();
        Ok(())
    }

    /// Remove sessions dead (revoked or expired) since before `keep_secs`
    /// ago, as a portable user's. Of a local user's, the newest row for
    /// each device (and the newest API one) is kept however old: those
    /// rows are also the record of which devices the user signed in from
    /// and when they were last seen (`user_devices`, `device_count`,
    /// `last_seen_at`, a PIN's "is this their device"). How many went.
    pub async fn purge_dead_sessions(&self, keep_secs: i64) -> Result<u64, StoreError> {
        let before = self.now() - keep_secs;
        let mut tx = self.begin_write().await?;
        let local = sqlx::query(
            "DELETE FROM sessions \
             WHERE min(coalesce(revoked_at, expires_at), expires_at) <= ?1 \
               AND token_hash NOT IN ( \
                 SELECT token_hash FROM ( \
                   SELECT token_hash, row_number() OVER ( \
                     PARTITION BY user_id, kind, endpoint_id \
                     ORDER BY coalesce(last_used_at, created_at) DESC, created_at DESC \
                   ) AS newest FROM sessions) \
                 WHERE newest = 1)",
        )
        .bind(before)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let portable = sqlx::query(
            "DELETE FROM portable_sessions \
             WHERE min(coalesce(revoked_at, expires_at), expires_at) <= ?1",
        )
        .bind(before)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        Ok(local + portable)
    }

    /// Purge dead sessions every [`PURGE_EVERY`], keeping
    /// [`DEAD_SESSION_KEEP_SECS`] of them, and spent OAuth codes and
    /// tokens, until the store is closed.
    pub fn spawn_session_purge(&self) {
        let store = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(PURGE_EVERY).await;
                if store.pool.is_closed() {
                    return;
                }
                match store.purge_dead_sessions(DEAD_SESSION_KEEP_SECS).await {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(removed = n, "purged dead sessions"),
                    Err(e) => tracing::warn!(error = %e, "cannot purge dead sessions"),
                }
                // OAuth's codes and tokens past their time, and clients
                // nobody granted anything (PX-42).
                match store
                    .purge_oauth(crate::oauth::UNUSED_CLIENT.as_secs() as i64)
                    .await
                {
                    Ok(0) => {}
                    Ok(n) => tracing::debug!(removed = n, "purged spent OAuth codes and tokens"),
                    Err(e) => tracing::warn!(error = %e, "cannot purge OAuth codes and tokens"),
                }
            }
        });
    }

    /// Whether `endpoint_id` is a device with a live session (PX-10): one
    /// not revoked, not expired, and its user not disabled. Such a device
    /// is its owner's on the server's relay; one with only a portable
    /// user's session ([`Store::endpoint_has_portable_session`]) is a
    /// friend's there.
    pub async fn endpoint_signed_in(&self, endpoint_id: &str) -> Result<bool, StoreError> {
        let (live,): (bool,) = sqlx::query_as(
            "SELECT EXISTS (SELECT 1 FROM sessions s JOIN users u ON u.id = s.user_id \
               WHERE s.kind = 'endpoint' AND s.endpoint_id = ?1 AND s.revoked_at IS NULL \
                 AND s.expires_at > ?2 AND u.disabled_at IS NULL)",
        )
        .bind(endpoint_id)
        .bind(self.now())
        .fetch_one(&self.pool)
        .await?;
        Ok(live)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{TestStore, test_store};
    use super::*;
    use crate::store::now;

    async fn store_with_alice(test: &str) -> (TestStore, LocalUserId) {
        let store = test_store(test).await;
        let id = store
            .add_user("alice", None, "phc", Role::Consumer)
            .await
            .unwrap();
        (store, id)
    }

    #[tokio::test]
    async fn an_api_session_names_its_user_until_revoked() {
        let (store, alice) = store_with_alice("api").await;
        store
            .create_session("h", alice, &SessionBinding::Api, now() + 60, None)
            .await
            .unwrap();
        assert_eq!(
            store.api_session("h").await.unwrap().map(|s| s.user),
            Some(alice)
        );
        store.revoke_session("h").await.unwrap();
        assert_eq!(store.api_session("h").await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_session_is_as_fresh_as_its_sign_in_or_its_last_confirmation() {
        let (store, alice) = store_with_alice("fresh").await;
        store
            .create_session("h", alice, &SessionBinding::Api, now() + 3600, None)
            .await
            .unwrap();
        let at = |store: &Store| {
            let store = store.clone();
            async move {
                store
                    .api_session("h")
                    .await
                    .unwrap()
                    .unwrap()
                    .authenticated_at
            }
        };
        let signed_in = at(&store).await;
        assert!(now() - signed_in < 5);
        store.age_session("h", 900).await;
        assert_eq!(at(&store).await, signed_in - 900);
        store.confirm_session("h").await.unwrap();
        assert!(now() - at(&store).await < 5);
    }

    #[tokio::test]
    async fn an_expired_session_is_not_live() {
        let (store, alice) = store_with_alice("expired").await;
        store
            .create_session("h", alice, &SessionBinding::Api, now() - 1, None)
            .await
            .unwrap();
        assert_eq!(store.api_session("h").await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_disabled_user_s_session_is_not_live() {
        let (store, alice) = store_with_alice("disabled").await;
        store
            .create_session("h", alice, &SessionBinding::Api, now() + 60, None)
            .await
            .unwrap();
        store.disable_user("alice").await.unwrap();
        assert_eq!(store.api_session("h").await.unwrap(), None);
    }

    fn endpoint(id: &str) -> SessionBinding {
        SessionBinding::Endpoint {
            endpoint_id: id.into(),
            device_name: Some(format!("device {id}")),
            platform: Platform::Tvos,
        }
    }

    /// Review 2026-10-11 M18: dead sessions past the keep go, but each
    /// device's newest row stays, so the user's devices, their count and
    /// when the user was last seen read as before; a live one is never
    /// touched.
    #[tokio::test]
    async fn dead_sessions_are_purged_but_each_devices_newest_stays() {
        let (store, alice) = store_with_alice("purge").await;
        for (hash, binding) in [
            ("tv-1", endpoint("tv")),
            ("tv-2", endpoint("tv")),
            ("tv-3", endpoint("tv")),
            ("web-1", SessionBinding::Api),
            ("web-2", SessionBinding::Api),
            ("phone", endpoint("phone")),
        ] {
            store
                .create_session(hash, alice, &binding, now() + 3600, None)
                .await
                .unwrap();
        }
        for hash in ["tv-1", "tv-2", "tv-3", "web-1", "web-2"] {
            store.revoke_session(hash).await.unwrap();
        }
        // Long dead: revoked and last used years ago, the newest of each
        // device's last.
        for (hash, age) in [
            ("tv-1", 300),
            ("tv-2", 200),
            ("tv-3", 100),
            ("web-1", 300),
            ("web-2", 100),
        ] {
            sqlx::query(
                "UPDATE sessions SET revoked_at = revoked_at - ?2 * 86400, \
                        last_used_at = last_used_at - ?2 * 86400, \
                        created_at = created_at - ?2 * 86400 WHERE token_hash = ?1",
            )
            .bind(hash)
            .bind(age)
            .execute(&store.pool)
            .await
            .unwrap();
        }
        let devices = |store: &Store| {
            let store = store.clone();
            async move {
                let mut d: Vec<String> = store
                    .user_devices(alice)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|d| d.endpoint_id)
                    .collect();
                d.sort();
                d
            }
        };
        let record = store.user_by_id(alice).await.unwrap().unwrap();
        let before = devices(&store).await;
        assert_eq!(
            store
                .purge_dead_sessions(DEAD_SESSION_KEEP_SECS)
                .await
                .unwrap(),
            3
        );
        let mut left: Vec<String> = sqlx::query_scalar("SELECT token_hash FROM sessions")
            .fetch_all(&store.pool)
            .await
            .unwrap();
        left.sort();
        assert_eq!(left, ["phone", "tv-3", "web-2"]);
        assert_eq!(devices(&store).await, before);
        let after = store.user_by_id(alice).await.unwrap().unwrap();
        assert_eq!(after.device_count, record.device_count);
        assert_eq!(after.last_seen_at, record.last_seen_at);
        assert_eq!(after.session_count, record.session_count);
        // Purging again removes nothing more.
        assert_eq!(
            store
                .purge_dead_sessions(DEAD_SESSION_KEEP_SECS)
                .await
                .unwrap(),
            0
        );
    }

    /// Review 2026-10-11 M18: whether a device is signed in is found by
    /// its endpoint, not by reading every session.
    #[tokio::test]
    async fn signed_in_devices_are_looked_up_by_endpoint() {
        let store = test_store("by-endpoint").await;
        for (query, index) in [
            (
                "SELECT 1 FROM sessions s WHERE s.kind = 'endpoint' AND s.endpoint_id = 'tv'",
                "sessions_by_endpoint",
            ),
            (
                "SELECT 1 FROM portable_sessions WHERE endpoint_id = 'tv'",
                "portable_sessions_by_endpoint",
            ),
        ] {
            let plan: Vec<(i64, i64, i64, String)> =
                sqlx::query_as(sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {query}")))
                    .fetch_all(&store.pool)
                    .await
                    .unwrap();
            assert!(
                plan.iter().any(|(_, _, _, detail)| detail.contains(index)),
                "{query}: {plan:?}"
            );
        }
    }

    /// What the metrics count (P3-14): live sessions only, apps apart from
    /// the admin API.
    #[tokio::test]
    async fn live_sessions_are_counted_per_user_by_binding() {
        let (store, alice) = store_with_alice("counted").await;
        for (hash, binding, expires) in [
            ("tv", endpoint("tv"), now() + 60),
            ("phone", endpoint("phone"), now() + 60),
            ("web", SessionBinding::Api, now() + 60),
            ("old", endpoint("old"), now() - 1),
            ("gone", SessionBinding::Api, now() + 60),
        ] {
            store
                .create_session(hash, alice, &binding, expires, None)
                .await
                .unwrap();
        }
        store.revoke_session("gone").await.unwrap();
        assert_eq!(store.live_session_counts().await.unwrap(), [(alice, 2, 1)]);
        store.disable_user("alice").await.unwrap();
        assert_eq!(store.live_session_counts().await.unwrap(), []);
    }

    async fn last_used(store: &Store, hash: &str) -> (i64, i64) {
        sqlx::query_as("SELECT last_used_at, expires_at FROM sessions WHERE token_hash = ?1")
            .bind(hash)
            .fetch_one(&store.pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn an_endpoint_session_works_only_from_its_endpoint() {
        let (store, alice) = store_with_alice("own-endpoint").await;
        store
            .create_session("h", alice, &endpoint("t"), now() + 60, None)
            .await
            .unwrap();
        let user = store
            .use_endpoint_session("h", "t", 60)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(user.user, alice);
        assert_eq!(user.display_name, "alice");
        assert_eq!(
            store.use_endpoint_session("h", "x", 60).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn an_api_session_is_not_an_endpoint_session() {
        let (store, alice) = store_with_alice("api-not-endpoint").await;
        store
            .create_session("h", alice, &SessionBinding::Api, now() + 60, None)
            .await
            .unwrap();
        assert_eq!(
            store.use_endpoint_session("h", "t", 60).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn use_renews_the_session_at_most_once_a_minute() {
        let (store, alice) = store_with_alice("renew").await;
        store
            .create_session("h", alice, &endpoint("t"), now() + 60, None)
            .await
            .unwrap();
        // Just created: no write yet.
        store.use_endpoint_session("h", "t", 1000).await.unwrap();
        let (_, expires) = last_used(&store, "h").await;
        assert!(expires <= now() + 60, "renewed too early");

        // Pretend the last use was two minutes ago.
        sqlx::query("UPDATE sessions SET last_used_at = last_used_at - 120")
            .execute(&store.pool)
            .await
            .unwrap();
        store.use_endpoint_session("h", "t", 1000).await.unwrap();
        let (used, expires) = last_used(&store, "h").await;
        assert!(now() - used < RENEW_EVERY_SECS);
        assert!(expires >= now() + 999);
    }

    #[tokio::test]
    async fn requests_that_find_a_renewal_due_together_write_it_once() {
        let (store, alice) = store_with_alice("renew-once").await;
        store
            .create_session("tv", alice, &endpoint("t"), now() + 60, None)
            .await
            .unwrap();
        store
            .create_session("web", alice, &SessionBinding::Api, now() + 60, None)
            .await
            .unwrap();
        sqlx::query("UPDATE sessions SET last_used_at = last_used_at - 120")
            .execute(&store.pool)
            .await
            .unwrap();
        // Requests that all read the stale last use, then renew: the
        // first writes, the rest find it written.
        let now = store.now();
        assert!(
            store
                .renew_endpoint_session(alice, "tv", "t", now, 1000)
                .await
                .unwrap()
        );
        assert!(
            !store
                .renew_endpoint_session(alice, "tv", "t", now, 1000)
                .await
                .unwrap()
        );
        assert!(
            store
                .renew_api_session("web", now, 1000, 5000)
                .await
                .unwrap()
        );
        assert!(
            !store
                .renew_api_session("web", now, 1000, 5000)
                .await
                .unwrap()
        );
        // A minute on, it is due again.
        let later = now + RENEW_EVERY_SECS;
        assert!(
            store
                .renew_api_session("web", later, 1000, 5000)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn sessions_are_listed_without_dead_ones() {
        let (store, alice) = store_with_alice("list").await;
        store
            .create_session("a", alice, &endpoint("t"), now() + 60, None)
            .await
            .unwrap();
        store
            .create_session("b", alice, &SessionBinding::Api, now() + 60, None)
            .await
            .unwrap();
        store
            .create_session("c", alice, &endpoint("x"), now() + 60, None)
            .await
            .unwrap();
        store.revoke_session("c").await.unwrap();
        let listed = store.list_sessions(alice).await.unwrap();
        let mut hashes: Vec<_> = listed.iter().map(|s| s.token_hash.as_str()).collect();
        hashes.sort();
        assert_eq!(hashes, ["a", "b"]);
        let device = listed.iter().find(|s| s.token_hash == "a").unwrap();
        assert_eq!(device.device_name.as_deref(), Some("device t"));
    }

    #[tokio::test]
    async fn signing_out_everywhere_else_keeps_only_the_one_session() {
        let (store, alice) = store_with_alice("others").await;
        let bob = store
            .add_user("bob", None, "phc", Role::Consumer)
            .await
            .unwrap();
        for (hash, binding) in [
            ("web", SessionBinding::Api),
            ("cli", SessionBinding::Api),
            ("tv", endpoint("t")),
        ] {
            store
                .create_session(hash, alice, &binding, now() + 60, None)
                .await
                .unwrap();
        }
        store
            .create_session("bobs", bob, &SessionBinding::Api, now() + 60, None)
            .await
            .unwrap();
        assert_eq!(store.revoke_other_sessions(alice, "web").await.unwrap(), 2);
        let left: Vec<_> = store
            .list_sessions(alice)
            .await
            .unwrap()
            .into_iter()
            .map(|s| s.token_hash)
            .collect();
        assert_eq!(left, ["web"]);
        // Nobody else's.
        assert_eq!(store.list_sessions(bob).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_endpoint_session_records_its_device_and_is_not_an_api_session() {
        let (store, alice) = store_with_alice("endpoint").await;
        let binding = SessionBinding::Endpoint {
            endpoint_id: "e9c1".into(),
            device_name: Some("Living room".into()),
            platform: Platform::Tvos,
        };
        store
            .create_session("h", alice, &binding, now() + 60, None)
            .await
            .unwrap();
        let name: Option<String> =
            sqlx::query_scalar("SELECT display_name FROM devices WHERE endpoint_id = 'e9c1'")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(name.as_deref(), Some("Living room"));
        assert_eq!(store.api_session("h").await.unwrap(), None);
    }
}
