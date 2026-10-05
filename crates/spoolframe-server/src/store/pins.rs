//! Profile PINs and where they are asked for (P2-08). Checking a PIN and
//! the lock it opens are `crate::auth`'s; this is storage.

use spoolframe_types::Platform;

use super::users::LocalUserId;
use super::{Store, StoreError, now};

/// One of a user's devices: an endpoint they have signed in from.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct DeviceRecord {
    /// Hex `EndpointId`.
    pub endpoint_id: String,
    /// What the client called itself.
    pub name: Option<String>,
    /// Its [`Platform`] by name; see [`DeviceRecord::platform`].
    platform: Option<String>,
    /// Unix seconds.
    pub last_seen_at: i64,
    /// The user's PIN is asked for on it.
    pub pin_required: bool,
}

/// What may be said about a user's PIN: never its hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinInfo {
    /// How many digits it has; `None` for a PIN set before they were kept.
    pub digits: Option<u8>,
}

impl DeviceRecord {
    /// What kind of client it runs; `None` for a device not seen since the
    /// server began keeping it.
    pub fn platform(&self) -> Option<Platform> {
        self.platform.as_deref().map(Platform::from_name)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PinError {
    /// Requiring a PIN the user does not have.
    #[error("set a PIN first")]
    NoPin,
    /// Not an endpoint the user has signed in from.
    #[error("no such device for this user")]
    NoSuchDevice,
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<sqlx::Error> for PinError {
    fn from(e: sqlx::Error) -> Self {
        PinError::Store(e.into())
    }
}

impl Store {
    /// `user`'s PIN hash, if they have one.
    pub async fn pin_phc(&self, user: LocalUserId) -> Result<Option<String>, StoreError> {
        Ok(
            sqlx::query_scalar("SELECT phc FROM pin_credentials WHERE user_id = ?1")
                .bind(user)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// Whether `user` has a PIN, and how long it is.
    pub async fn pin_info(&self, user: LocalUserId) -> Result<Option<PinInfo>, StoreError> {
        let digits: Option<Option<i64>> =
            sqlx::query_scalar("SELECT digits FROM pin_credentials WHERE user_id = ?1")
                .bind(user)
                .fetch_optional(&self.pool)
                .await?;
        Ok(digits.map(|digits| PinInfo {
            // The column allows only 4 to 8.
            digits: digits.and_then(|d| u8::try_from(d).ok()),
        }))
    }

    /// Set or replace `user`'s PIN: its hash, and how many digits it has.
    /// Where it is required stays.
    pub async fn set_pin(
        &self,
        user: LocalUserId,
        phc: &str,
        digits: u8,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO pin_credentials (user_id, phc, updated_at, digits) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT (user_id) DO UPDATE SET phc = excluded.phc, \
             updated_at = excluded.updated_at, digits = excluded.digits",
        )
        .bind(user)
        .bind(phc)
        .bind(now())
        .bind(i64::from(digits))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Remove `user`'s PIN, and with it its requirement on every device:
    /// a requirement for a PIN nobody can enter would lock them out.
    pub async fn clear_pin(&self, user: LocalUserId) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM pin_required WHERE user_id = ?1")
            .bind(user)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM pin_credentials WHERE user_id = ?1")
            .bind(user)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Ask for `user`'s PIN on `endpoint_id` (one of their devices), or
    /// stop asking.
    pub async fn set_pin_required(
        &self,
        user: LocalUserId,
        endpoint_id: &str,
        required: bool,
    ) -> Result<(), PinError> {
        let mut tx = self.pool.begin().await?;
        let theirs: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM sessions WHERE user_id = ?1 AND endpoint_id = ?2 LIMIT 1",
        )
        .bind(user)
        .bind(endpoint_id)
        .fetch_optional(&mut *tx)
        .await?;
        if theirs.is_none() {
            return Err(PinError::NoSuchDevice);
        }
        if required {
            let has_pin: Option<i64> =
                sqlx::query_scalar("SELECT 1 FROM pin_credentials WHERE user_id = ?1")
                    .bind(user)
                    .fetch_optional(&mut *tx)
                    .await?;
            if has_pin.is_none() {
                return Err(PinError::NoPin);
            }
            sqlx::query(
                "INSERT INTO pin_required (user_id, endpoint_id, set_at) VALUES (?1, ?2, ?3) \
                 ON CONFLICT DO NOTHING",
            )
            .bind(user)
            .bind(endpoint_id)
            .bind(now())
            .execute(&mut *tx)
            .await?;
        } else {
            sqlx::query("DELETE FROM pin_required WHERE user_id = ?1 AND endpoint_id = ?2")
                .bind(user)
                .bind(endpoint_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// The devices `user` has signed in from, most recently seen first.
    pub async fn user_devices(&self, user: LocalUserId) -> Result<Vec<DeviceRecord>, StoreError> {
        Ok(sqlx::query_as(
            "SELECT d.endpoint_id, d.display_name AS name, d.platform, d.last_seen_at, \
                    EXISTS (SELECT 1 FROM pin_required r \
                            WHERE r.user_id = ?1 AND r.endpoint_id = d.endpoint_id) AS pin_required \
             FROM devices d \
             WHERE d.endpoint_id IN (SELECT endpoint_id FROM sessions WHERE user_id = ?1) \
             ORDER BY d.last_seen_at DESC",
        )
        .bind(user)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Revoke `user`'s sessions on `endpoint_id` (too many wrong PINs
    /// there). Returns how many.
    pub async fn revoke_device_sessions(
        &self,
        user: LocalUserId,
        endpoint_id: &str,
    ) -> Result<u64, StoreError> {
        Ok(sqlx::query(
            "UPDATE sessions SET revoked_at = ?3 \
             WHERE user_id = ?1 AND endpoint_id = ?2 AND revoked_at IS NULL",
        )
        .bind(user)
        .bind(endpoint_id)
        .bind(now())
        .execute(&self.pool)
        .await?
        .rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use spoolframe_types::Role;

    use super::super::sessions::SessionBinding;
    use super::super::tests::test_store;
    use super::*;

    async fn alice_on(store: &Store, endpoints: &[&str]) -> LocalUserId {
        let alice = store
            .add_user("alice", None, "phc", Role::Consumer)
            .await
            .unwrap();
        for (n, endpoint) in endpoints.iter().enumerate() {
            store
                .create_session(
                    &format!("token-{n}"),
                    alice,
                    &SessionBinding::Endpoint {
                        endpoint_id: (*endpoint).into(),
                        device_name: Some(format!("device {n}")),
                        platform: Platform::Linux,
                    },
                    now() + 3600,
                )
                .await
                .unwrap();
        }
        alice
    }

    #[tokio::test]
    async fn a_pin_is_required_per_device_and_cleared_everywhere_with_it() {
        let store = test_store("pins").await;
        let alice = alice_on(&store, &["tv", "desktop"]).await;
        // Required needs a PIN to ask for.
        assert!(matches!(
            store.set_pin_required(alice, "tv", true).await,
            Err(PinError::NoPin)
        ));
        store.set_pin(alice, "pin-phc", 4).await.unwrap();
        store.set_pin_required(alice, "tv", true).await.unwrap();
        let required: Vec<_> = store
            .user_devices(alice)
            .await
            .unwrap()
            .into_iter()
            .map(|d| (d.endpoint_id, d.pin_required))
            .collect();
        assert!(required.contains(&("tv".into(), true)));
        assert!(required.contains(&("desktop".into(), false)));
        // Not a device of hers.
        assert!(matches!(
            store.set_pin_required(alice, "elsewhere", true).await,
            Err(PinError::NoSuchDevice)
        ));

        store.clear_pin(alice).await.unwrap();
        assert_eq!(store.pin_phc(alice).await.unwrap(), None);
        assert!(
            store
                .user_devices(alice)
                .await
                .unwrap()
                .iter()
                .all(|d| !d.pin_required)
        );
    }

    #[tokio::test]
    async fn a_pin_s_length_is_kept_and_unknown_for_one_set_before_it_was() {
        let store = test_store("pin-digits").await;
        let alice = alice_on(&store, &["tv"]).await;
        assert_eq!(store.pin_info(alice).await.unwrap(), None);
        store.set_pin(alice, "pin-phc", 6).await.unwrap();
        assert_eq!(
            store.pin_info(alice).await.unwrap(),
            Some(PinInfo { digits: Some(6) })
        );
        store.set_pin(alice, "pin-phc", 4).await.unwrap();
        assert_eq!(
            store.pin_info(alice).await.unwrap(),
            Some(PinInfo { digits: Some(4) })
        );
        // As a PIN from before migration 0010 is: a hash and no length.
        sqlx::query("UPDATE pin_credentials SET digits = NULL")
            .execute(&store.pool)
            .await
            .unwrap();
        assert_eq!(
            store.pin_info(alice).await.unwrap(),
            Some(PinInfo { digits: None })
        );
        store.clear_pin(alice).await.unwrap();
        assert_eq!(store.pin_info(alice).await.unwrap(), None);
    }

    #[tokio::test]
    async fn too_many_wrong_pins_revoke_that_device_s_sessions_only() {
        let store = test_store("pin-revoke").await;
        let alice = alice_on(&store, &["tv", "desktop"]).await;
        assert_eq!(store.revoke_device_sessions(alice, "tv").await.unwrap(), 1);
        let live: i64 =
            sqlx::query_scalar("SELECT count(*) FROM sessions WHERE revoked_at IS NULL")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(live, 1);
    }
}
