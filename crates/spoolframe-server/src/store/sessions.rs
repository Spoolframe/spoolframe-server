//! Sessions: what a successful login leaves behind. Rows are keyed by a
//! hash of the token, so the database alone cannot be used to act as a
//! user.

use spoolframe_types::Role;

use super::users::{LocalUserId, role};
use super::{Store, StoreError, now};

/// What a session is bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionBinding {
    /// An Iroh client endpoint (hex `EndpointId`); usable only from it
    /// (P1-12). `device_name` is what the client calls itself.
    Endpoint {
        endpoint_id: String,
        device_name: Option<String>,
    },
    /// The admin API: the CLI, and the web UI (P3-01).
    Api,
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
}

/// One of a user's live sessions, as the user may see it: never the token.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct SessionRecord {
    /// To recognise the caller's own session; not shown to clients.
    pub token_hash: String,
    /// `None` for an admin API session.
    pub device_name: Option<String>,
    pub endpoint_id: Option<String>,
    pub created_at: i64,
    pub last_used_at: i64,
}

/// A session's use is written back (renewing its expiry) at most this
/// often, so busy clients do not turn every request into a write.
pub const RENEW_EVERY_SECS: i64 = 60;

impl Store {
    /// Record a session for `user`, expiring at `expires_at`.
    pub async fn create_session(
        &self,
        token_hash: &str,
        user: LocalUserId,
        binding: &SessionBinding,
        expires_at: i64,
    ) -> Result<(), StoreError> {
        let now = now();
        let mut tx = self.pool.begin().await?;
        let (kind, endpoint) = match binding {
            SessionBinding::Endpoint {
                endpoint_id,
                device_name,
            } => {
                // The device row appears on the first login from an
                // endpoint, and keeps the latest name the client gave.
                sqlx::query(
                    "INSERT INTO devices (endpoint_id, display_name, first_seen_at, last_seen_at) \
                     VALUES (?1, ?2, ?3, ?3) \
                     ON CONFLICT (endpoint_id) DO UPDATE SET \
                       display_name = coalesce(excluded.display_name, devices.display_name), \
                       last_seen_at = excluded.last_seen_at",
                )
                .bind(endpoint_id)
                .bind(device_name)
                .bind(now)
                .execute(&mut *tx)
                .await?;
                ("endpoint", Some(endpoint_id.as_str()))
            }
            SessionBinding::Api => ("api", None),
        };
        sqlx::query(
            "INSERT INTO sessions \
               (token_hash, user_id, kind, endpoint_id, created_at, last_used_at, \
                authenticated_at, expires_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?5, ?6)",
        )
        .bind(token_hash)
        .bind(user.0)
        .bind(kind)
        .bind(endpoint)
        .bind(now)
        .bind(expires_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// The user of a live API session: not revoked, not expired, and the
    /// user not disabled.
    pub async fn api_session(
        &self,
        token_hash: &str,
    ) -> Result<Option<ApiSessionUser>, StoreError> {
        let row: Option<(i64, String, i64)> = sqlx::query_as(
            "SELECT u.id, u.role, s.authenticated_at \
             FROM sessions s JOIN users u ON u.id = s.user_id \
             WHERE s.token_hash = ?1 AND s.kind = 'api' AND s.revoked_at IS NULL \
               AND s.expires_at > ?2 AND u.disabled_at IS NULL",
        )
        .bind(token_hash)
        .bind(now())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(id, stored, authenticated_at)| ApiSessionUser {
            user: LocalUserId(id),
            role: role(stored),
            authenticated_at,
        }))
    }

    /// The session's user proved who they are again, now.
    pub async fn confirm_session(&self, token_hash: &str) -> Result<(), StoreError> {
        sqlx::query("UPDATE sessions SET authenticated_at = ?2 WHERE token_hash = ?1")
            .bind(token_hash)
            .bind(now())
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
        let now = now();
        let row: Option<(i64, String, Option<String>, String, i64, bool)> = sqlx::query_as(
            "SELECT u.id, u.username, u.display_name, u.role, s.last_used_at, \
                    EXISTS (SELECT 1 FROM pin_credentials p WHERE p.user_id = u.id) \
                    AND EXISTS (SELECT 1 FROM pin_required r \
                                WHERE r.user_id = u.id AND r.endpoint_id = s.endpoint_id) \
             FROM sessions s JOIN users u ON u.id = s.user_id \
             WHERE s.token_hash = ?1 AND s.kind = 'endpoint' AND s.endpoint_id = ?2 \
               AND s.revoked_at IS NULL AND s.expires_at > ?3 AND u.disabled_at IS NULL",
        )
        .bind(token_hash)
        .bind(endpoint_id)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;
        let Some((id, username, display_name, stored, last_used_at, pin_required)) = row else {
            return Ok(None);
        };
        if now - last_used_at >= RENEW_EVERY_SECS {
            let mut tx = self.pool.begin().await?;
            sqlx::query(
                "UPDATE sessions SET last_used_at = ?2, expires_at = ?3 WHERE token_hash = ?1",
            )
            .bind(token_hash)
            .bind(now)
            .bind(now + lifetime_secs)
            .execute(&mut *tx)
            .await?;
            sqlx::query("UPDATE devices SET last_seen_at = ?2 WHERE endpoint_id = ?1")
                .bind(endpoint_id)
                .bind(now)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
        }
        Ok(Some(SessionUser {
            user: LocalUserId(id),
            display_name: display_name.unwrap_or_else(|| username.clone()),
            username,
            role: role(stored),
            pin_required,
        }))
    }

    /// `user`'s live sessions, on every device, most recently used first.
    pub async fn list_sessions(&self, user: LocalUserId) -> Result<Vec<SessionRecord>, StoreError> {
        Ok(sqlx::query_as(
            "SELECT s.token_hash, d.display_name AS device_name, s.endpoint_id, s.created_at, \
                    s.last_used_at \
             FROM sessions s LEFT JOIN devices d ON d.endpoint_id = s.endpoint_id \
             WHERE s.user_id = ?1 AND s.revoked_at IS NULL AND s.expires_at > ?2 \
             ORDER BY s.last_used_at DESC",
        )
        .bind(user.0)
        .bind(now())
        .fetch_all(&self.pool)
        .await?)
    }

    /// Revoke every live session of `user`'s but `keep` (a token hash):
    /// "sign out everywhere else" (P3-03). How many were revoked.
    pub async fn revoke_other_sessions(
        &self,
        user: LocalUserId,
        keep: &str,
    ) -> Result<u64, StoreError> {
        Ok(sqlx::query(
            "UPDATE sessions SET revoked_at = ?3 \
             WHERE user_id = ?1 AND token_hash != ?2 AND revoked_at IS NULL",
        )
        .bind(user.0)
        .bind(keep)
        .bind(now())
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// Revoke one session. Revoking an unknown or already revoked one is
    /// not an error.
    pub async fn revoke_session(&self, token_hash: &str) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE sessions SET revoked_at = ?2 WHERE token_hash = ?1 AND revoked_at IS NULL",
        )
        .bind(token_hash)
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{TestStore, test_store};
    use super::*;

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
            .create_session("h", alice, &SessionBinding::Api, now() + 60)
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
            .create_session("h", alice, &SessionBinding::Api, now() + 3600)
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
            .create_session("h", alice, &SessionBinding::Api, now() - 1)
            .await
            .unwrap();
        assert_eq!(store.api_session("h").await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_disabled_user_s_session_is_not_live() {
        let (store, alice) = store_with_alice("disabled").await;
        store
            .create_session("h", alice, &SessionBinding::Api, now() + 60)
            .await
            .unwrap();
        store.disable_user("alice").await.unwrap();
        assert_eq!(store.api_session("h").await.unwrap(), None);
    }

    fn endpoint(id: &str) -> SessionBinding {
        SessionBinding::Endpoint {
            endpoint_id: id.into(),
            device_name: Some(format!("device {id}")),
        }
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
            .create_session("h", alice, &endpoint("t"), now() + 60)
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
            .create_session("h", alice, &SessionBinding::Api, now() + 60)
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
            .create_session("h", alice, &endpoint("t"), now() + 60)
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
    async fn sessions_are_listed_without_dead_ones() {
        let (store, alice) = store_with_alice("list").await;
        store
            .create_session("a", alice, &endpoint("t"), now() + 60)
            .await
            .unwrap();
        store
            .create_session("b", alice, &SessionBinding::Api, now() + 60)
            .await
            .unwrap();
        store
            .create_session("c", alice, &endpoint("x"), now() + 60)
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
                .create_session(hash, alice, &binding, now() + 60)
                .await
                .unwrap();
        }
        store
            .create_session("bobs", bob, &SessionBinding::Api, now() + 60)
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
        };
        store
            .create_session("h", alice, &binding, now() + 60)
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
