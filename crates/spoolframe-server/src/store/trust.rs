//! Trusted devices (P4-01): a device that may start a session for a user
//! without a full sign-in. Whether a request may make or change trust is
//! the caller's (`crate::client`, `crate::admin`); this is storage, and
//! the rules that must hold whoever calls: trust is made only from a live
//! session or for a device the user has signed in on, it never outlives
//! what ends it, and at most one user opens by itself on a device.

use super::sessions::SessionBinding;
use super::users::LocalUserId;
use super::{Store, StoreError};

/// A user this device may resume, as `TrustedProfiles` shows them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedProfile {
    pub user: LocalUserId,
    pub username: String,
    /// The display name, or the username when there is none.
    pub display_name: String,
    /// The user's PIN is asked for on this device (P2-08).
    pub pin_required: bool,
    pub auto_login: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum TrustError {
    /// Not an endpoint the user has signed in from.
    #[error("no such device for this user")]
    NoSuchDevice,
    /// Auto-login without trust.
    #[error("a device opens a profile by itself only if it is trusted")]
    AutoLoginUntrusted,
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<sqlx::Error> for TrustError {
    fn from(e: sqlx::Error) -> Self {
        TrustError::Store(e.into())
    }
}

/// What a trusted device's resume made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resumed {
    /// A new session, recorded under the hash it was asked for.
    Session,
    /// No session: no trust, a lapsed one (now removed), or a disabled
    /// user. Deliberately one answer.
    Refused,
}

impl Store {
    /// Trust the device of the live endpoint session `token_hash` for its
    /// user: a sign-in with "Remember this device", or the session turning
    /// it on for its own device. Only while the session is live, in one
    /// statement, so a revocation (a password change, say) that lands
    /// first leaves no trust behind. An existing row keeps its auto-login.
    /// Whether the device is trusted after it.
    pub async fn remember_session_device(&self, token_hash: &str) -> Result<bool, StoreError> {
        let now = self.now();
        let made = sqlx::query(
            "INSERT INTO trusted_endpoints (user_id, endpoint_id, created_at, last_used_at) \
             SELECT s.user_id, s.endpoint_id, ?2, ?2 \
             FROM sessions s JOIN users u ON u.id = s.user_id \
             WHERE s.token_hash = ?1 AND s.kind = 'endpoint' AND s.revoked_at IS NULL \
               AND s.expires_at > ?2 AND u.disabled_at IS NULL \
             ON CONFLICT (user_id, endpoint_id) DO UPDATE SET last_used_at = excluded.last_used_at",
        )
        .bind(token_hash)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(made.rows_affected() > 0)
    }

    /// Whether `endpoint_id` is trusted for `user` (lapsed or not).
    pub async fn device_trusted(
        &self,
        user: LocalUserId,
        endpoint_id: &str,
    ) -> Result<bool, StoreError> {
        let (trusted,): (bool,) = sqlx::query_as(
            "SELECT EXISTS (SELECT 1 FROM trusted_endpoints \
                            WHERE user_id = ?1 AND endpoint_id = ?2)",
        )
        .bind(user)
        .bind(endpoint_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(trusted)
    }

    /// Remove `endpoint_id`'s trust for `user`, and nothing else: trust
    /// made for a device sign-in that ran out before the device was told.
    pub async fn forget_device(
        &self,
        user: LocalUserId,
        endpoint_id: &str,
    ) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM trusted_endpoints WHERE user_id = ?1 AND endpoint_id = ?2")
            .bind(user)
            .bind(endpoint_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Trust `endpoint_id` (one of `user`'s devices) or stop, and say
    /// whether it opens `user`'s profile by itself. Auto-login turns it
    /// off for the device's other users, and the PIN requirement off for
    /// `user` there. Stopping revokes `user`'s sessions on the device.
    /// Returns how many sessions were revoked.
    pub async fn set_device_trust(
        &self,
        user: LocalUserId,
        endpoint_id: &str,
        trusted: bool,
        auto_login: bool,
    ) -> Result<u64, TrustError> {
        if auto_login && !trusted {
            return Err(TrustError::AutoLoginUntrusted);
        }
        let now = self.now();
        let mut tx = self.begin_write().await?;
        let theirs: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM sessions WHERE user_id = ?1 AND endpoint_id = ?2 LIMIT 1",
        )
        .bind(user)
        .bind(endpoint_id)
        .fetch_optional(&mut *tx)
        .await?;
        if theirs.is_none() {
            return Err(TrustError::NoSuchDevice);
        }
        let mut revoked = 0;
        if trusted {
            if auto_login {
                sqlx::query(
                    "UPDATE trusted_endpoints SET auto_login = 0 \
                     WHERE endpoint_id = ?1 AND user_id != ?2 AND auto_login = 1",
                )
                .bind(endpoint_id)
                .bind(user)
                .execute(&mut *tx)
                .await?;
                sqlx::query("DELETE FROM pin_required WHERE user_id = ?1 AND endpoint_id = ?2")
                    .bind(user)
                    .bind(endpoint_id)
                    .execute(&mut *tx)
                    .await?;
            }
            sqlx::query(
                "INSERT INTO trusted_endpoints \
                   (user_id, endpoint_id, auto_login, created_at, last_used_at) \
                 VALUES (?1, ?2, ?3, ?4, ?4) \
                 ON CONFLICT (user_id, endpoint_id) DO UPDATE SET auto_login = excluded.auto_login",
            )
            .bind(user)
            .bind(endpoint_id)
            .bind(auto_login)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        } else {
            sqlx::query("DELETE FROM trusted_endpoints WHERE user_id = ?1 AND endpoint_id = ?2")
                .bind(user)
                .bind(endpoint_id)
                .execute(&mut *tx)
                .await?;
            revoked = sqlx::query(
                "UPDATE sessions SET revoked_at = ?3 \
                 WHERE user_id = ?1 AND endpoint_id = ?2 AND revoked_at IS NULL",
            )
            .bind(user)
            .bind(endpoint_id)
            .bind(now)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        }
        tx.commit().await?;
        if !trusted {
            self.note_sessions_ended();
        }
        Ok(revoked)
    }

    /// The users `endpoint_id` may resume, by username. Trust unused for
    /// `idle_secs` has lapsed: it is not listed, and is removed here. Only
    /// this endpoint's rows, and only enabled users.
    pub async fn trusted_profiles(
        &self,
        endpoint_id: &str,
        idle_secs: i64,
    ) -> Result<Vec<TrustedProfile>, StoreError> {
        let lapsed_before = self.now() - idle_secs;
        // Every relay admission asks this, and a lapsed row is rare. A
        // DELETE, even of no rows, would take SQLite's writer each time:
        // look first, outside a transaction. A row that lapses between
        // the two is left out of the list below all the same.
        let lapsed: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM trusted_endpoints \
                            WHERE endpoint_id = ?1 AND last_used_at <= ?2)",
        )
        .bind(endpoint_id)
        .bind(lapsed_before)
        .fetch_one(&self.pool)
        .await?;
        if lapsed {
            sqlx::query(
                "DELETE FROM trusted_endpoints WHERE endpoint_id = ?1 AND last_used_at <= ?2",
            )
            .bind(endpoint_id)
            .bind(lapsed_before)
            .execute(&self.pool)
            .await?;
        }
        type Row = (LocalUserId, String, Option<String>, bool, bool);
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT u.id, u.username, u.display_name, \
                    EXISTS (SELECT 1 FROM pin_credentials p WHERE p.user_id = u.id) \
                    AND EXISTS (SELECT 1 FROM pin_required r \
                                WHERE r.user_id = u.id AND r.endpoint_id = t.endpoint_id), \
                    t.auto_login \
             FROM trusted_endpoints t JOIN users u ON u.id = t.user_id \
             WHERE t.endpoint_id = ?1 AND t.last_used_at > ?2 AND u.disabled_at IS NULL \
             ORDER BY u.username",
        )
        .bind(endpoint_id)
        .bind(lapsed_before)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(user, username, display_name, pin_required, auto_login)| TrustedProfile {
                    user,
                    display_name: display_name.unwrap_or_else(|| username.clone()),
                    username,
                    pin_required,
                    auto_login,
                },
            )
            .collect())
    }

    /// Start a session for `user` on the device `binding` names, which
    /// must be trusted for them (P4-01): the trust row live (used within
    /// `idle_secs`) and the user enabled. The session is not a recent
    /// sign-in: its user proved nothing. The check and the session are
    /// one transaction, begun with the write that renews the trust, so
    /// nothing that ends trust can land between them. A lapsed row is
    /// removed.
    pub async fn resume_session(
        &self,
        token_hash: &str,
        user: LocalUserId,
        binding: &SessionBinding,
        expires_at: i64,
        idle_secs: i64,
    ) -> Result<Resumed, StoreError> {
        let SessionBinding::Endpoint { endpoint_id, .. } = binding else {
            return Ok(Resumed::Refused);
        };
        let now = self.now();
        let mut tx = self.begin_write().await?;
        let renewed = sqlx::query(
            "UPDATE trusted_endpoints SET last_used_at = ?3 \
             WHERE user_id = ?1 AND endpoint_id = ?2 AND last_used_at > ?4 \
               AND EXISTS (SELECT 1 FROM users u WHERE u.id = ?1 AND u.disabled_at IS NULL)",
        )
        .bind(user)
        .bind(endpoint_id)
        .bind(now)
        .bind(now - idle_secs)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if renewed == 0 {
            sqlx::query(
                "DELETE FROM trusted_endpoints \
                 WHERE user_id = ?1 AND endpoint_id = ?2 AND last_used_at <= ?3",
            )
            .bind(user)
            .bind(endpoint_id)
            .bind(now - idle_secs)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(Resumed::Refused);
        }
        // Nobody proved who they are: the device did. Until its user
        // confirms, the session makes no change that needs a recent
        // sign-in (PX-09).
        Self::insert_session(&mut tx, token_hash, user, binding, expires_at, now, 0).await?;
        tx.commit().await?;
        Ok(Resumed::Session)
    }

    /// Make trust older by `secs`, as if unused that long.
    #[cfg(test)]
    pub(crate) async fn age_trust(&self, user: LocalUserId, endpoint_id: &str, secs: i64) {
        sqlx::query(
            "UPDATE trusted_endpoints SET last_used_at = last_used_at - ?3 \
             WHERE user_id = ?1 AND endpoint_id = ?2",
        )
        .bind(user)
        .bind(endpoint_id)
        .bind(secs)
        .execute(&self.pool)
        .await
        .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use spoolframe_types::{Platform, Role};

    use super::super::tests::{TestStore, test_store};
    use super::*;
    use crate::store::now;

    const IDLE: i64 = 180 * 24 * 3600;

    fn on(endpoint: &str) -> SessionBinding {
        SessionBinding::Endpoint {
            endpoint_id: endpoint.into(),
            device_name: Some(format!("device {endpoint}")),
            platform: Platform::Linux,
        }
    }

    async fn user(store: &Store, name: &str) -> LocalUserId {
        store
            .add_user(name, None, "phc", Role::Consumer)
            .await
            .unwrap()
    }

    /// Alice, signed in on "tv" with session "alice-tv", the device
    /// remembered.
    async fn alice_trusted_on_tv(test: &str) -> (TestStore, LocalUserId) {
        let store = test_store(test).await;
        let alice = user(&store, "alice").await;
        store
            .create_session("alice-tv", alice, &on("tv"), now() + 3600)
            .await
            .unwrap();
        assert!(store.remember_session_device("alice-tv").await.unwrap());
        (store, alice)
    }

    async fn resume(store: &Store, hash: &str, user: LocalUserId, endpoint: &str) -> Resumed {
        store
            .resume_session(hash, user, &on(endpoint), now() + 3600, IDLE)
            .await
            .unwrap()
    }

    async fn listed(store: &Store, endpoint: &str) -> Vec<String> {
        store
            .trusted_profiles(endpoint, IDLE)
            .await
            .unwrap()
            .into_iter()
            .map(|p| p.username)
            .collect()
    }

    #[tokio::test]
    async fn a_remembered_device_resumes_its_user_from_itself_only() {
        let (store, alice) = alice_trusted_on_tv("trust-resume").await;
        assert_eq!(listed(&store, "tv").await, ["alice"]);
        assert_eq!(resume(&store, "r1", alice, "tv").await, Resumed::Session);
        assert_eq!(
            store
                .use_endpoint_session("r1", "tv", 60)
                .await
                .unwrap()
                .map(|u| u.user),
            Some(alice)
        );
        // Another device: no trust, no session, nothing listed.
        assert_eq!(
            resume(&store, "r2", alice, "laptop").await,
            Resumed::Refused
        );
        assert!(listed(&store, "laptop").await.is_empty());
        assert_eq!(store.api_session("r2").await.unwrap(), None);
    }

    #[tokio::test]
    async fn trust_is_never_another_user_s() {
        let (store, _alice) = alice_trusted_on_tv("trust-cross-user").await;
        let bob = user(&store, "bob").await;
        store
            .create_session("bob-tv", bob, &on("tv"), now() + 3600)
            .await
            .unwrap();
        assert_eq!(resume(&store, "r", bob, "tv").await, Resumed::Refused);
        assert_eq!(listed(&store, "tv").await, ["alice"]);
    }

    #[tokio::test]
    async fn a_revoked_session_makes_no_trust() {
        let store = test_store("trust-revoked-first").await;
        let alice = user(&store, "alice").await;
        store
            .create_session("h", alice, &on("tv"), now() + 3600)
            .await
            .unwrap();
        store.revoke_session("h").await.unwrap();
        assert!(!store.remember_session_device("h").await.unwrap());
        assert!(!store.device_trusted(alice, "tv").await.unwrap());
        // Nor does an API session, which has no device.
        store
            .create_session("web", alice, &SessionBinding::Api, now() + 3600)
            .await
            .unwrap();
        assert!(!store.remember_session_device("web").await.unwrap());
    }

    #[tokio::test]
    async fn trust_unused_past_the_limit_lapses_and_is_removed() {
        let (store, alice) = alice_trusted_on_tv("trust-lapse").await;
        store.age_trust(alice, "tv", IDLE - 60).await;
        assert_eq!(listed(&store, "tv").await, ["alice"]);
        store.age_trust(alice, "tv", 120).await;
        assert_eq!(resume(&store, "r", alice, "tv").await, Resumed::Refused);
        assert!(!store.device_trusted(alice, "tv").await.unwrap());
        assert!(listed(&store, "tv").await.is_empty());
    }

    /// Review 2026-10-11 M19: listing a device's profiles, which every
    /// relay admission does, does not wait for SQLite's writer when there
    /// is nothing lapsed to remove.
    #[tokio::test]
    async fn listing_profiles_does_not_wait_for_sqlites_writer() {
        let (store, _alice) = alice_trusted_on_tv("trust-list-read-only").await;
        let mut writer = store.pool.acquire().await.unwrap();
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut *writer)
            .await
            .unwrap();
        let listed = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            store.trusted_profiles("tv", IDLE),
        )
        .await
        .expect("listing profiles waited for the writer")
        .unwrap();
        sqlx::query("ROLLBACK").execute(&mut *writer).await.unwrap();
        assert_eq!(listed.len(), 1);
    }

    #[tokio::test]
    async fn a_lapsed_row_is_removed_when_the_device_lists_its_profiles() {
        let (store, alice) = alice_trusted_on_tv("trust-lapse-list").await;
        store.age_trust(alice, "tv", IDLE + 1).await;
        assert!(listed(&store, "tv").await.is_empty());
        assert!(!store.device_trusted(alice, "tv").await.unwrap());
    }

    #[tokio::test]
    async fn using_the_session_keeps_trust_from_lapsing() {
        let (store, alice) = alice_trusted_on_tv("trust-used").await;
        store.age_trust(alice, "tv", IDLE - 10).await;
        // As if the session was last used two minutes ago.
        sqlx::query("UPDATE sessions SET last_used_at = last_used_at - 120")
            .execute(&store.pool)
            .await
            .unwrap();
        store
            .use_endpoint_session("alice-tv", "tv", 3600)
            .await
            .unwrap()
            .unwrap();
        store.age_trust(alice, "tv", 60).await;
        assert_eq!(resume(&store, "r", alice, "tv").await, Resumed::Session);
    }

    #[tokio::test]
    async fn every_password_change_ends_all_of_the_user_s_trust() {
        let (store, alice) = alice_trusted_on_tv("trust-password").await;
        store
            .create_session("alice-laptop", alice, &on("laptop"), now() + 3600)
            .await
            .unwrap();
        store.remember_session_device("alice-laptop").await.unwrap();
        store
            .create_session("web", alice, &SessionBinding::Api, now() + 3600)
            .await
            .unwrap();
        // The web session that changes it is kept; trust is not.
        store
            .set_password("alice", "new", Some("web"))
            .await
            .unwrap();
        assert!(listed(&store, "tv").await.is_empty());
        assert!(listed(&store, "laptop").await.is_empty());
        assert_eq!(resume(&store, "r", alice, "tv").await, Resumed::Refused);
    }

    #[tokio::test]
    async fn disabling_the_user_ends_their_trust_and_enabling_does_not_bring_it_back() {
        let (store, alice) = alice_trusted_on_tv("trust-disabled").await;
        store.disable_user("alice").await.unwrap();
        assert_eq!(resume(&store, "r", alice, "tv").await, Resumed::Refused);
        store.enable_user("alice").await.unwrap();
        assert_eq!(resume(&store, "r", alice, "tv").await, Resumed::Refused);
        assert!(listed(&store, "tv").await.is_empty());
    }

    #[tokio::test]
    async fn revoking_a_session_ends_that_device_s_trust_only() {
        let (store, alice) = alice_trusted_on_tv("trust-revoke").await;
        store
            .create_session("alice-laptop", alice, &on("laptop"), now() + 3600)
            .await
            .unwrap();
        store.remember_session_device("alice-laptop").await.unwrap();
        store.revoke_session("alice-tv").await.unwrap();
        assert_eq!(resume(&store, "r", alice, "tv").await, Resumed::Refused);
        assert_eq!(listed(&store, "laptop").await, ["alice"]);
    }

    #[tokio::test]
    async fn signing_out_everywhere_else_ends_every_other_device_s_trust() {
        let (store, alice) = alice_trusted_on_tv("trust-others").await;
        store
            .create_session("alice-laptop", alice, &on("laptop"), now() + 3600)
            .await
            .unwrap();
        store.remember_session_device("alice-laptop").await.unwrap();
        store
            .revoke_other_sessions(alice, "alice-laptop")
            .await
            .unwrap();
        assert!(listed(&store, "tv").await.is_empty());
        assert_eq!(listed(&store, "laptop").await, ["alice"]);
    }

    #[tokio::test]
    async fn too_many_wrong_pins_end_that_device_s_trust() {
        let (store, alice) = alice_trusted_on_tv("trust-pins").await;
        store.revoke_device_sessions(alice, "tv").await.unwrap();
        assert_eq!(resume(&store, "r", alice, "tv").await, Resumed::Refused);
    }

    #[tokio::test]
    async fn a_session_nobody_holds_is_discarded_without_touching_trust() {
        let (store, alice) = alice_trusted_on_tv("trust-discard").await;
        store
            .create_session("unheld", alice, &on("tv"), now() + 3600)
            .await
            .unwrap();
        store.discard_unheld_session("unheld").await.unwrap();
        assert!(store.device_trusted(alice, "tv").await.unwrap());
        assert_eq!(store.api_session("unheld").await.unwrap(), None);
    }

    #[tokio::test]
    async fn stopping_trust_revokes_the_user_s_sessions_on_that_device() {
        let (store, alice) = alice_trusted_on_tv("trust-off").await;
        let revoked = store
            .set_device_trust(alice, "tv", false, false)
            .await
            .unwrap();
        assert_eq!(revoked, 1);
        assert_eq!(
            store
                .use_endpoint_session("alice-tv", "tv", 60)
                .await
                .unwrap(),
            None
        );
        assert!(listed(&store, "tv").await.is_empty());
    }

    #[tokio::test]
    async fn trust_is_set_only_for_a_device_the_user_has_used() {
        let store = test_store("trust-own-device").await;
        let alice = user(&store, "alice").await;
        let bob = user(&store, "bob").await;
        store
            .create_session("bob-tv", bob, &on("tv"), now() + 3600)
            .await
            .unwrap();
        assert!(matches!(
            store.set_device_trust(alice, "tv", true, false).await,
            Err(TrustError::NoSuchDevice)
        ));
        assert!(matches!(
            store.set_device_trust(bob, "tv", false, true).await,
            Err(TrustError::AutoLoginUntrusted)
        ));
        store
            .set_device_trust(bob, "tv", true, false)
            .await
            .unwrap();
        assert_eq!(listed(&store, "tv").await, ["bob"]);
    }

    #[tokio::test]
    async fn auto_login_is_one_user_s_per_device_and_turns_the_pin_off() {
        let (store, alice) = alice_trusted_on_tv("trust-auto").await;
        let bob = user(&store, "bob").await;
        store
            .create_session("bob-tv", bob, &on("tv"), now() + 3600)
            .await
            .unwrap();
        store.set_pin(alice, "pin", 4).await.unwrap();
        store.set_pin_required(alice, "tv", true).await.unwrap();
        store
            .set_device_trust(alice, "tv", true, true)
            .await
            .unwrap();
        let profiles = store.trusted_profiles("tv", IDLE).await.unwrap();
        assert_eq!(
            profiles
                .iter()
                .map(|p| (p.username.as_str(), p.auto_login, p.pin_required))
                .collect::<Vec<_>>(),
            [("alice", true, false)]
        );
        store.set_device_trust(bob, "tv", true, true).await.unwrap();
        let auto: Vec<_> = store
            .trusted_profiles("tv", IDLE)
            .await
            .unwrap()
            .into_iter()
            .map(|p| (p.username, p.auto_login))
            .collect();
        assert_eq!(auto, [("alice".into(), false), ("bob".into(), true)]);
    }

    #[tokio::test]
    async fn requiring_the_pin_turns_auto_login_off() {
        let (store, alice) = alice_trusted_on_tv("trust-pin-on").await;
        store.set_pin(alice, "pin", 4).await.unwrap();
        store
            .set_device_trust(alice, "tv", true, true)
            .await
            .unwrap();
        store.set_pin_required(alice, "tv", true).await.unwrap();
        let profile = &store.trusted_profiles("tv", IDLE).await.unwrap()[0];
        assert!(!profile.auto_login);
        assert!(profile.pin_required);
    }

    #[tokio::test]
    async fn the_devices_list_says_which_are_trusted() {
        let (store, alice) = alice_trusted_on_tv("trust-devices").await;
        store
            .create_session("alice-laptop", alice, &on("laptop"), now() + 3600)
            .await
            .unwrap();
        store
            .set_device_trust(alice, "tv", true, true)
            .await
            .unwrap();
        let mut devices: Vec<_> = store
            .user_devices(alice)
            .await
            .unwrap()
            .into_iter()
            .map(|d| (d.endpoint_id, d.trusted, d.auto_login))
            .collect();
        devices.sort();
        assert_eq!(
            devices,
            [("laptop".into(), false, false), ("tv".into(), true, true)]
        );
    }
}
