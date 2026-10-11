//! Grants to portable users and their sessions (P7-06, ARCHITECTURE §50,
//! §51). A grant lets someone whose account this server does not host
//! watch here as `Principal::PortableUser`; it is all the server keeps of
//! them. Whether they proved who they are is the caller's
//! (`crate::portable`); this keeps the rows.

use spoolframe_protocol::portable::{DevicePublicKey, PortableUserId, UserDeviceCredential};
use spoolframe_types::Platform;

use super::{Store, StoreError};

/// A grant, as admins see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub portable: PortableUserId,
    /// What they asked to be called; `None`: their fingerprint.
    pub display_name: Option<String>,
    /// Unix seconds.
    pub granted_at: i64,
    /// The invite that made it, if one did (P7-05).
    pub invite_id: Option<String>,
    /// Their sessions that are live now.
    pub live_sessions: u64,
}

impl Grant {
    /// What to call them: their display name, else their fingerprint.
    pub fn shown_name(&self) -> String {
        shown_name(self.portable, self.display_name.as_deref())
    }
}

/// What to call portable user `portable` named `display_name`.
pub fn shown_name(portable: PortableUserId, display_name: Option<&str>) -> String {
    display_name
        .map(str::to_owned)
        .unwrap_or_else(|| portable.fingerprint())
}

/// The portable user behind a live session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortableSessionUser {
    pub user: PortableUserId,
    /// Their display name, else their fingerprint.
    pub display_name: String,
}

/// The credential a session was opened with (P7-06, after review): the
/// identity and device key it names, and when it expires. A session is
/// never renewed past that, and an identity state that revokes the key
/// revokes the session ([`Store::accept_identity_state`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CredentialBinding {
    pub user: PortableUserId,
    pub device_key: DevicePublicKey,
    /// Unix seconds.
    pub expires_at: i64,
}

impl CredentialBinding {
    pub fn of(credential: &UserDeviceCredential) -> Self {
        Self {
            user: credential.user,
            device_key: credential.device_key,
            expires_at: credential.expires_at,
        }
    }
}

fn parse_id(text: String) -> Result<PortableUserId, StoreError> {
    text.parse()
        .map_err(|e| StoreError::Db(sqlx::Error::Decode(Box::new(e))))
}

impl Store {
    /// Grant `portable` the right to watch here, calling them
    /// `display_name`, from invite `invite_id` if one made it. A live grant
    /// there already stays as it was: its first name and invite are kept.
    /// A revoked one is granted afresh. `true` if this made it.
    pub async fn grant_portable(
        &self,
        portable: PortableUserId,
        display_name: Option<&str>,
        invite_id: Option<&str>,
    ) -> Result<bool, StoreError> {
        let made = sqlx::query(
            "INSERT INTO portable_grants (portable_user_id, display_name, granted_at, invite_id) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT (portable_user_id) DO UPDATE SET \
               display_name = excluded.display_name, granted_at = excluded.granted_at, \
               invite_id = excluded.invite_id, revoked_at = NULL \
             WHERE portable_grants.revoked_at IS NOT NULL",
        )
        .bind(portable.to_string())
        .bind(display_name)
        .bind(self.now())
        .bind(invite_id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(made == 1)
    }

    /// `portable`'s live grant, if they have one here.
    pub async fn grant_of(&self, portable: PortableUserId) -> Result<Option<Grant>, StoreError> {
        Ok(self.grants_where(Some(portable)).await?.pop())
    }

    /// Every live grant, oldest first; revoked ones are left out.
    pub async fn grants(&self) -> Result<Vec<Grant>, StoreError> {
        self.grants_where(None).await
    }

    /// Every live grant, or with `one` only that portable user's: the same
    /// record either way, its sessions counted only for the rows asked for.
    async fn grants_where(&self, one: Option<PortableUserId>) -> Result<Vec<Grant>, StoreError> {
        macro_rules! select {
            () => {
                "SELECT g.portable_user_id, g.display_name, g.granted_at, g.invite_id, \
                        (SELECT count(*) FROM portable_sessions s \
                         WHERE s.portable_user_id = g.portable_user_id \
                           AND s.revoked_at IS NULL AND s.expires_at > ?1) \
                 FROM portable_grants g WHERE g.revoked_at IS NULL "
            };
        }
        type Row = (String, Option<String>, i64, Option<String>, i64);
        let rows: Vec<Row> = match one {
            None => {
                sqlx::query_as(concat!(
                    select!(),
                    "ORDER BY g.granted_at, g.portable_user_id"
                ))
                .bind(self.now())
                .fetch_all(&self.pool)
                .await?
            }
            Some(portable) => {
                sqlx::query_as(concat!(select!(), "AND g.portable_user_id = ?2"))
                    .bind(self.now())
                    .bind(portable.to_string())
                    .fetch_all(&self.pool)
                    .await?
            }
        };
        rows.into_iter()
            .map(|(id, display_name, granted_at, invite_id, live)| {
                Ok(Grant {
                    portable: parse_id(id)?,
                    display_name,
                    granted_at,
                    invite_id,
                    live_sessions: live as u64,
                })
            })
            .collect()
    }

    /// Take `portable`'s grant away, and with it every session of theirs:
    /// `false` if they had no live one. The grant stays as a tombstone
    /// (`revoked_at`): an invite made before it cannot bring them back
    /// ([`Store::redeem_portable_invite`]), only a newer one.
    pub async fn revoke_grant(&self, portable: PortableUserId) -> Result<bool, StoreError> {
        let mut tx = self.begin_write().await?;
        sqlx::query("DELETE FROM portable_sessions WHERE portable_user_id = ?1")
            .bind(portable.to_string())
            .execute(&mut *tx)
            .await?;
        let removed = sqlx::query(
            "UPDATE portable_grants SET revoked_at = ?2 \
             WHERE portable_user_id = ?1 AND revoked_at IS NULL",
        )
        .bind(portable.to_string())
        .bind(self.now())
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        self.note_sessions_ended();
        Ok(removed == 1)
    }

    /// Record a session for granted portable user `credential.user` on
    /// `endpoint_id`, opened with `credential`, expiring at `expires_at`
    /// or when the credential does, whichever is first.
    pub async fn create_portable_session(
        &self,
        token_hash: &str,
        credential: &CredentialBinding,
        endpoint_id: &str,
        device_name: Option<&str>,
        platform: Platform,
        expires_at: i64,
    ) -> Result<(), StoreError> {
        let now = self.now();
        sqlx::query(
            "INSERT INTO portable_sessions \
               (token_hash, portable_user_id, endpoint_id, device_name, platform, \
                created_at, last_used_at, expires_at, device_key, credential_expires_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?8, ?9)",
        )
        .bind(token_hash)
        .bind(credential.user.to_string())
        .bind(endpoint_id)
        .bind(device_name)
        .bind(platform.as_str())
        .bind(now)
        .bind(expires_at.min(credential.expires_at))
        .bind(credential.device_key.to_string())
        .bind(credential.expires_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The portable user of a live session used from `endpoint_id`: its
    /// own endpoint, not revoked, not expired, its grant live, and its
    /// device key not revoked by any state seen here. A use renews it to
    /// `lifetime_secs` from now, never past its credential's expiry,
    /// written at most once a [`super::sessions::RENEW_EVERY_SECS`].
    pub async fn use_portable_session(
        &self,
        token_hash: &str,
        endpoint_id: &str,
        lifetime_secs: i64,
    ) -> Result<Option<PortableSessionUser>, StoreError> {
        let now = self.now();
        let row: Option<(String, Option<String>, i64)> = sqlx::query_as(
            "SELECT g.portable_user_id, g.display_name, s.last_used_at \
             FROM portable_sessions s \
             JOIN portable_grants g ON g.portable_user_id = s.portable_user_id \
             WHERE s.token_hash = ?1 AND s.endpoint_id = ?2 \
               AND s.revoked_at IS NULL AND s.expires_at > ?3 AND g.revoked_at IS NULL \
               AND NOT EXISTS (SELECT 1 FROM portable_revoked_keys r \
                               WHERE r.user_id = s.portable_user_id \
                                 AND r.device_key = s.device_key)",
        )
        .bind(token_hash)
        .bind(endpoint_id)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;
        let Some((id, display_name, last_used_at)) = row else {
            return Ok(None);
        };
        if now - last_used_at >= super::sessions::RENEW_EVERY_SECS {
            self.renew_portable_session(token_hash, now, lifetime_secs)
                .await?;
        }
        let user = parse_id(id)?;
        Ok(Some(PortableSessionUser {
            user,
            display_name: shown_name(user, display_name.as_deref()),
        }))
    }

    /// Write the renewal [`Store::use_portable_session`] found due,
    /// unless another request wrote it since: only the first of requests
    /// that arrive together writes, as for endpoint sessions. Whether this
    /// one wrote.
    async fn renew_portable_session(
        &self,
        token_hash: &str,
        now: i64,
        lifetime_secs: i64,
    ) -> Result<bool, StoreError> {
        let renewed = sqlx::query(
            "UPDATE portable_sessions SET last_used_at = ?2, \
               expires_at = min(?3, coalesce(credential_expires_at, ?3)) \
             WHERE token_hash = ?1 AND last_used_at <= ?4",
        )
        .bind(token_hash)
        .bind(now)
        .bind(now + lifetime_secs)
        .bind(now - super::sessions::RENEW_EVERY_SECS)
        .execute(&self.pool)
        .await?;
        Ok(renewed.rows_affected() > 0)
    }

    /// Whose portable session `token_hash` is, live or not, and its
    /// endpoint: for the refused-request log.
    pub(crate) async fn portable_session_owner(
        &self,
        token_hash: &str,
    ) -> Result<Option<(PortableUserId, String, bool)>, StoreError> {
        let row: Option<(String, String, bool)> = sqlx::query_as(
            "SELECT s.portable_user_id, s.endpoint_id, \
                    s.revoked_at IS NULL AND s.expires_at > ?2 \
             FROM portable_sessions s WHERE s.token_hash = ?1",
        )
        .bind(token_hash)
        .bind(self.now())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(id, endpoint, live)| Ok((parse_id(id)?, endpoint, live)))
            .transpose()
    }

    /// Whether `endpoint_id` has a live session of anyone's: a local
    /// user's ([`Store::endpoint_signed_in`]) or a portable user's. Such a
    /// device gets the normal connection limits; any other is a stranger.
    pub async fn endpoint_has_any_session(&self, endpoint_id: &str) -> Result<bool, StoreError> {
        Ok(self.endpoint_signed_in(endpoint_id).await?
            || self.endpoint_has_portable_session(endpoint_id).await?)
    }

    /// Whether `endpoint_id` has a live portable session.
    pub async fn endpoint_has_portable_session(
        &self,
        endpoint_id: &str,
    ) -> Result<bool, StoreError> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM portable_sessions \
               WHERE endpoint_id = ?1 AND revoked_at IS NULL AND expires_at > ?2)",
        )
        .bind(endpoint_id)
        .bind(self.now())
        .fetch_one(&self.pool)
        .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::test_store;
    use super::*;
    use crate::store::portable::tests::{device, state, user};

    /// User `u`'s credential for device 9, good for a day from now.
    fn binding(store: &Store, u: u8) -> CredentialBinding {
        CredentialBinding {
            user: user(u),
            device_key: device(9),
            expires_at: store.now() + 86_400,
        }
    }

    #[tokio::test]
    async fn a_grant_is_made_once_and_keeps_its_first_name() {
        let store = test_store("grant-once").await;
        assert!(
            store
                .grant_portable(user(7), Some("Sam"), Some("inv"))
                .await
                .unwrap()
        );
        assert!(
            !store
                .grant_portable(user(7), Some("Other"), None)
                .await
                .unwrap()
        );
        let grant = store.grant_of(user(7)).await.unwrap().unwrap();
        assert_eq!(grant.display_name.as_deref(), Some("Sam"));
        assert_eq!(grant.invite_id.as_deref(), Some("inv"));
        assert!(store.grant_of(user(8)).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn requests_that_find_a_renewal_due_together_write_it_once() {
        let store = test_store("grant-renew-once").await;
        store.grant_portable(user(7), None, None).await.unwrap();
        store
            .create_portable_session(
                "h",
                &binding(&store, 7),
                "ep",
                None,
                Platform::Tvos,
                store.now() + 3600,
            )
            .await
            .unwrap();
        sqlx::query("UPDATE portable_sessions SET last_used_at = last_used_at - 120")
            .execute(&store.pool)
            .await
            .unwrap();
        // Two requests that both read the stale last use, then renew.
        let now = store.now();
        assert!(store.renew_portable_session("h", now, 3600).await.unwrap());
        assert!(!store.renew_portable_session("h", now, 3600).await.unwrap());
    }

    #[tokio::test]
    async fn a_session_lives_while_its_grant_does() {
        let store = test_store("grant-session").await;
        store.grant_portable(user(7), None, None).await.unwrap();
        let expires = store.now() + 3600;
        store
            .create_portable_session(
                "h",
                &binding(&store, 7),
                "ep",
                Some("TV"),
                Platform::Tvos,
                expires,
            )
            .await
            .unwrap();
        let who = store
            .use_portable_session("h", "ep", 3600)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(who.user, user(7));
        assert_eq!(who.display_name, user(7).fingerprint());
        assert_eq!(store.grants().await.unwrap()[0].live_sessions, 1);
        // Only from its own endpoint.
        assert!(
            store
                .use_portable_session("h", "other", 3600)
                .await
                .unwrap()
                .is_none()
        );
        assert!(store.endpoint_has_portable_session("ep").await.unwrap());

        assert!(store.revoke_grant(user(7)).await.unwrap());
        assert!(
            store
                .use_portable_session("h", "ep", 3600)
                .await
                .unwrap()
                .is_none()
        );
        assert!(!store.endpoint_has_portable_session("ep").await.unwrap());
        assert!(!store.revoke_grant(user(7)).await.unwrap());
    }

    #[tokio::test]
    async fn a_revoked_session_is_dead() {
        let store = test_store("grant-revoked-session").await;
        store.grant_portable(user(7), None, None).await.unwrap();
        store
            .create_portable_session(
                "h",
                &binding(&store, 7),
                "ep",
                None,
                Platform::Ios,
                store.now() + 60,
            )
            .await
            .unwrap();
        store.revoke_session("h").await.unwrap();
        assert!(
            store
                .use_portable_session("h", "ep", 3600)
                .await
                .unwrap()
                .is_none()
        );
    }

    /// A revoked grant is a tombstone: no longer listed or live, and
    /// granted afresh only on purpose.
    #[tokio::test]
    async fn a_revoked_grant_stays_as_a_tombstone() {
        let store = test_store("grant-tombstone").await;
        store
            .grant_portable(user(7), Some("Sam"), None)
            .await
            .unwrap();
        assert!(store.revoke_grant(user(7)).await.unwrap());
        assert!(store.grant_of(user(7)).await.unwrap().is_none());
        assert!(store.grants().await.unwrap().is_empty());
        let revoked: Option<i64> = sqlx::query_scalar(
            "SELECT revoked_at FROM portable_grants WHERE portable_user_id = ?1",
        )
        .bind(user(7).to_string())
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert!(revoked.is_some());
        assert!(store.grant_portable(user(7), None, None).await.unwrap());
        assert!(store.grant_of(user(7)).await.unwrap().is_some());
    }

    /// P7-06 review: a session never outlives its credential, renewals
    /// included.
    #[tokio::test]
    async fn a_session_is_capped_at_its_credential_s_expiry() {
        let store = test_store("grant-credential-cap").await;
        store.grant_portable(user(7), None, None).await.unwrap();
        let credential = CredentialBinding {
            expires_at: store.now() + 600,
            ..binding(&store, 7)
        };
        store
            .create_portable_session(
                "h",
                &credential,
                "ep",
                None,
                Platform::Tvos,
                store.now() + 86_400,
            )
            .await
            .unwrap();
        let expires = || async {
            sqlx::query_scalar::<_, i64>("SELECT expires_at FROM portable_sessions")
                .fetch_one(&store.pool)
                .await
                .unwrap()
        };
        assert_eq!(expires().await, credential.expires_at);
        store.clock().advance(std::time::Duration::from_secs(120));
        store
            .use_portable_session("h", "ep", 86_400)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(expires().await, credential.expires_at);
        store.clock().advance(std::time::Duration::from_secs(600));
        assert!(
            store
                .use_portable_session("h", "ep", 86_400)
                .await
                .unwrap()
                .is_none()
        );
    }

    /// P7-06 review: a state revoking the key ends the sessions opened
    /// with it, and only those.
    #[tokio::test]
    async fn a_state_revoking_the_key_ends_its_sessions() {
        let store = test_store("grant-revoked-key").await;
        store.grant_portable(user(7), None, None).await.unwrap();
        let mut ended = store.sessions_ended();
        store
            .create_portable_session(
                "tv",
                &binding(&store, 7),
                "ep-tv",
                None,
                Platform::Tvos,
                store.now() + 3600,
            )
            .await
            .unwrap();
        let phone = CredentialBinding {
            device_key: device(10),
            ..binding(&store, 7)
        };
        store
            .create_portable_session(
                "phone",
                &phone,
                "ep-phone",
                None,
                Platform::Ios,
                store.now() + 3600,
            )
            .await
            .unwrap();
        ended.borrow_and_update();
        store
            .accept_identity_state(&state(1, &[device(9)]), store.now())
            .await
            .unwrap();
        assert!(ended.has_changed().unwrap());
        assert!(
            store
                .use_portable_session("tv", "ep-tv", 3600)
                .await
                .unwrap()
                .is_none()
        );
        let revoked: Option<i64> =
            sqlx::query_scalar("SELECT revoked_at FROM portable_sessions WHERE token_hash = 'tv'")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert!(revoked.is_some(), "revoked, not only refused on use");
        assert!(
            store
                .use_portable_session("phone", "ep-phone", 3600)
                .await
                .unwrap()
                .is_some()
        );
    }
}
