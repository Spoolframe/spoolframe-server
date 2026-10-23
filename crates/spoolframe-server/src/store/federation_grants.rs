//! The libraries this server shares with paired servers (P8-03,
//! ARCHITECTURE §52): server-wide grants, one row per pair and library,
//! the first library ACL there is. Shared is browse and watch, for every
//! user of the paired server; who among them watches is that server's
//! call.
//!
//! A revoked grant keeps its row with `revoked_at`; sharing the library
//! again makes a new one. Unpairing revokes every grant of the pair in the
//! same transaction as the pair's row ([`super::federation`]).

use sqlx::Row;
use uuid::Uuid;

use super::{Store, StoreError};

/// A library shared with a paired server, or once shared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantRecord {
    pub id: String,
    /// The pair's row ([`super::federation::PeerRecord::id`]).
    pub peer: String,
    /// The library's `LibraryId`, as written.
    pub library: String,
    /// Who shared it.
    pub granted_by: String,
    /// Unix seconds.
    pub granted_at: i64,
    pub revoked_at: Option<i64>,
}

impl GrantRecord {
    /// Not revoked.
    pub fn live(&self) -> bool {
        self.revoked_at.is_none()
    }
}

/// A grant row's columns, as [`record`] reads them: a macro, so each query
/// is one `concat!`ed literal.
macro_rules! columns {
    () => {
        "id, peer, library, granted_by, granted_at, revoked_at"
    };
}

fn record(row: &sqlx::sqlite::SqliteRow) -> Result<GrantRecord, sqlx::Error> {
    Ok(GrantRecord {
        id: row.try_get("id")?,
        peer: row.try_get("peer")?,
        library: row.try_get("library")?,
        granted_by: row.try_get("granted_by")?,
        granted_at: row.try_get("granted_at")?,
        revoked_at: row.try_get("revoked_at")?,
    })
}

/// Revoke every live grant of pair `peer`, at `now`, on `conn`: what
/// unpairing does with them. The grants revoked.
pub(super) async fn revoke_all(
    conn: &mut sqlx::SqliteConnection,
    peer: &str,
    now: i64,
) -> Result<Vec<GrantRecord>, sqlx::Error> {
    let rows = sqlx::query(concat!(
        "UPDATE federation_grants SET revoked_at = ?2 \
         WHERE peer = ?1 AND revoked_at IS NULL RETURNING ",
        columns!()
    ))
    .bind(peer)
    .bind(now)
    .fetch_all(conn)
    .await?;
    rows.iter().map(record).collect()
}

impl Store {
    /// Share `library` with live pair `peer`, by `granted_by`. A library
    /// shared already keeps its grant: the live grant, new or not; `None`
    /// when `peer` is not a live pair.
    pub async fn grant_library(
        &self,
        peer: &str,
        library: &str,
        granted_by: &str,
    ) -> Result<Option<GrantRecord>, StoreError> {
        let mut tx = self.begin_write().await?;
        let live: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM federation_peers WHERE id = ?1 AND revoked_at IS NULL)",
        )
        .bind(peer)
        .fetch_one(&mut *tx)
        .await?;
        if !live {
            return Ok(None);
        }
        let kept = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM federation_grants WHERE peer = ?1 AND library = ?2 AND revoked_at IS NULL"
        ))
        .bind(peer)
        .bind(library)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(row) = kept {
            tx.rollback().await?;
            return Ok(Some(record(&row)?));
        }
        let row = sqlx::query(concat!(
            "INSERT INTO federation_grants (id, peer, library, granted_by, granted_at) \
             VALUES (?1, ?2, ?3, ?4, ?5) RETURNING ",
            columns!()
        ))
        .bind(Uuid::now_v7().hyphenated().to_string())
        .bind(peer)
        .bind(library)
        .bind(granted_by)
        .bind(self.now())
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(record(&row)?))
    }

    /// Stop sharing `library` with pair `peer`: the grant revoked, if
    /// there was a live one. What the pair was last given of it (P8-02)
    /// is forgotten: shared again, it starts from a whole library.
    pub async fn revoke_library_grant(
        &self,
        peer: &str,
        library: &str,
    ) -> Result<Option<GrantRecord>, StoreError> {
        let row = sqlx::query(concat!(
            "UPDATE federation_grants SET revoked_at = ?3 \
             WHERE peer = ?1 AND library = ?2 AND revoked_at IS NULL RETURNING ",
            columns!()
        ))
        .bind(peer)
        .bind(library)
        .bind(self.now())
        .fetch_optional(&self.pool)
        .await?;
        let revoked = row.as_ref().map(record).transpose()?;
        if revoked.is_some() {
            self.forget_sync_states(peer, Some(library)).await?;
        }
        Ok(revoked)
    }

    /// The live grants of pair `peer`, the oldest first.
    pub async fn live_grants(&self, peer: &str) -> Result<Vec<GrantRecord>, StoreError> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM federation_grants WHERE peer = ?1 AND revoked_at IS NULL \
             ORDER BY granted_at, id"
        ))
        .bind(peer)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(record).collect::<Result<_, _>>()?)
    }

    /// Whether `library` is shared with pair `peer` now: a live grant of
    /// a live pair.
    pub async fn is_granted(&self, peer: &str, library: &str) -> Result<bool, StoreError> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM federation_grants g \
                            JOIN federation_peers p ON p.id = g.peer \
                            WHERE g.peer = ?1 AND g.library = ?2 \
                              AND g.revoked_at IS NULL AND p.revoked_at IS NULL)",
        )
        .bind(peer)
        .bind(library)
        .fetch_one(&self.pool)
        .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::super::federation::NewPeer;
    use super::super::tests::{TestStore, test_store};
    use super::*;

    async fn paired(test: &str) -> (TestStore, String) {
        let store = test_store(test).await;
        let peer = store
            .add_peer(
                NewPeer {
                    server_id: "mzxw6ytbojtg633c",
                    endpoint_id: &"0b".repeat(32),
                    display_name: "Bob's server",
                    addrs: &[],
                    relays: &[],
                },
                "alice",
            )
            .await
            .unwrap();
        (store, peer.id)
    }

    fn libraries(grants: &[GrantRecord]) -> Vec<&str> {
        grants.iter().map(|g| g.library.as_str()).collect()
    }

    #[tokio::test]
    async fn a_shared_library_is_granted_once_and_kept_when_shared_again() {
        let (store, peer) = paired("fed-grant").await;
        let first = store
            .grant_library(&peer, "plex:1", "alice")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (first.peer.as_str(), first.granted_by.as_str()),
            (peer.as_str(), "alice")
        );
        assert!(first.live());
        store.clock().advance(std::time::Duration::from_secs(5));
        let again = store
            .grant_library(&peer, "plex:1", "bob")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(again, first);
        store.grant_library(&peer, "plex:2", "alice").await.unwrap();
        assert_eq!(
            libraries(&store.live_grants(&peer).await.unwrap()),
            ["plex:1", "plex:2"]
        );
        assert!(store.is_granted(&peer, "plex:1").await.unwrap());
        assert!(!store.is_granted(&peer, "plex:3").await.unwrap());
    }

    #[tokio::test]
    async fn a_revoked_grant_is_not_granted_and_sharing_again_is_a_new_grant() {
        let (store, peer) = paired("fed-grant-revoke").await;
        let first = store
            .grant_library(&peer, "plex:1", "alice")
            .await
            .unwrap()
            .unwrap();
        store
            .keep_sync_state(&peer, "plex:1", None, "c1", &[])
            .await
            .unwrap();
        let revoked = store.revoke_library_grant(&peer, "plex:1").await.unwrap();
        assert_eq!(store.sync_state(&peer, "plex:1", "c1").await.unwrap(), None);
        assert_eq!(revoked.map(|g| g.id), Some(first.id.clone()));
        assert_eq!(
            store.revoke_library_grant(&peer, "plex:1").await.unwrap(),
            None
        );
        assert!(!store.is_granted(&peer, "plex:1").await.unwrap());
        assert!(store.live_grants(&peer).await.unwrap().is_empty());
        let second = store
            .grant_library(&peer, "plex:1", "alice")
            .await
            .unwrap()
            .unwrap();
        assert_ne!(second.id, first.id);
    }

    /// Unpairing revokes the pair's grants with its row, and forgets what
    /// it was sent; a pair that is not live is granted nothing.
    #[tokio::test]
    async fn unpairing_revokes_the_pair_s_grants() {
        let (store, peer) = paired("fed-grant-unpair").await;
        store.grant_library(&peer, "plex:1", "alice").await.unwrap();
        store.grant_library(&peer, "plex:2", "alice").await.unwrap();
        store
            .keep_sync_state(&peer, "plex:1", None, "c1", &[])
            .await
            .unwrap();
        store.revoke_peer(&peer).await.unwrap().unwrap();
        assert_eq!(store.sync_state(&peer, "plex:1", "c1").await.unwrap(), None);
        assert!(store.live_grants(&peer).await.unwrap().is_empty());
        assert!(!store.is_granted(&peer, "plex:1").await.unwrap());
        assert_eq!(
            store.grant_library(&peer, "plex:1", "alice").await.unwrap(),
            None
        );
        assert_eq!(
            store
                .grant_library("no-such", "plex:1", "alice")
                .await
                .unwrap(),
            None
        );
    }
}
