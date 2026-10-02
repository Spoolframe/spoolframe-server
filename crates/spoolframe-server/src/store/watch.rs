//! Watch state rows (P1-14). The rules (thresholds, throttling) live in
//! `crate::watch`; this is storage.

use std::collections::{HashMap, HashSet};

use sqlx::{QueryBuilder, Sqlite};

use super::{Store, StoreError, now};
use crate::principal::Principal;

/// One item's state for one principal.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct WatchRow {
    pub media_id: String,
    pub show_id: Option<String>,
    pub position_ms: Option<i64>,
    pub duration_ms: Option<i64>,
    pub watched: bool,
    pub updated_at: i64,
}

/// A state to save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchUpdate<'a> {
    pub media_id: &'a str,
    pub show_id: Option<&'a str>,
    pub external_ids: &'a [String],
    pub position_ms: Option<i64>,
    pub duration_ms: Option<i64>,
    pub watched: bool,
}

impl Store {
    /// Save one item's state, with its external ids.
    pub async fn save_watch(
        &self,
        principal: Principal,
        update: WatchUpdate<'_>,
    ) -> Result<(), StoreError> {
        self.save_watch_at(principal, update, now()).await
    }

    /// [`Store::save_watch`], as of `updated_at` (Unix seconds) rather than
    /// now: for state taken from elsewhere (PX-04's Plex sync), which keeps
    /// the time it changed there.
    pub async fn save_watch_at(
        &self,
        principal: Principal,
        update: WatchUpdate<'_>,
        updated_at: i64,
    ) -> Result<(), StoreError> {
        let principal = principal.to_string();
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO watch_state \
               (principal, media_id, show_id, position_ms, duration_ms, watched, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT (principal, media_id) DO UPDATE SET \
               show_id = coalesce(excluded.show_id, watch_state.show_id), \
               position_ms = excluded.position_ms, \
               duration_ms = coalesce(excluded.duration_ms, watch_state.duration_ms), \
               watched = excluded.watched, \
               updated_at = excluded.updated_at",
        )
        .bind(&principal)
        .bind(update.media_id)
        .bind(update.show_id)
        .bind(update.position_ms)
        .bind(update.duration_ms)
        .bind(update.watched)
        .bind(updated_at)
        .execute(&mut *tx)
        .await?;
        for id in update.external_ids {
            // An id names one item: if it was another row's, it is this
            // one's now.
            sqlx::query(
                "INSERT INTO watch_state_ids (principal, media_id, external_id) \
                 VALUES (?1, ?2, ?3) \
                 ON CONFLICT (principal, external_id) DO UPDATE SET media_id = excluded.media_id",
            )
            .bind(&principal)
            .bind(update.media_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// If `media_id` has no state but an item with one of `external_ids`
    /// does, move that state to `media_id` (the item got a new key).
    /// Returns whether a row moved.
    pub async fn relink_watch(
        &self,
        principal: Principal,
        media_id: &str,
        external_ids: &[String],
    ) -> Result<bool, StoreError> {
        if external_ids.is_empty() {
            return Ok(false);
        }
        let principal = principal.to_string();
        let mut tx = self.pool.begin().await?;
        let has_own: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM watch_state WHERE principal = ?1 AND media_id = ?2")
                .bind(&principal)
                .bind(media_id)
                .fetch_optional(&mut *tx)
                .await?;
        if has_own.is_some() {
            return Ok(false);
        }
        let mut query: QueryBuilder<Sqlite> =
            QueryBuilder::new("SELECT media_id FROM watch_state_ids WHERE principal = ");
        query.push_bind(&principal).push(" AND external_id IN (");
        let mut ids = query.separated(", ");
        for id in external_ids {
            ids.push_bind(id);
        }
        query.push(") LIMIT 1");
        let old: Option<String> = query.build_query_scalar().fetch_optional(&mut *tx).await?;
        let Some(old) = old else {
            return Ok(false);
        };
        // The ids follow through ON UPDATE CASCADE.
        sqlx::query("UPDATE watch_state SET media_id = ?3 WHERE principal = ?1 AND media_id = ?2")
            .bind(&principal)
            .bind(&old)
            .bind(media_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        tracing::info!(%principal, from = %old, to = %media_id, "watch state re-linked");
        Ok(true)
    }

    /// Every state `principal` has.
    pub async fn all_watch(&self, principal: Principal) -> Result<Vec<WatchRow>, StoreError> {
        Ok(sqlx::query_as(
            "SELECT media_id, show_id, position_ms, duration_ms, watched, updated_at \
             FROM watch_state WHERE principal = ?1",
        )
        .bind(principal.to_string())
        .fetch_all(&self.pool)
        .await?)
    }

    /// The states of `media_ids`, in one query.
    pub async fn watch_states(
        &self,
        principal: Principal,
        media_ids: &[String],
    ) -> Result<HashMap<String, WatchRow>, StoreError> {
        if media_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let mut query: QueryBuilder<Sqlite> = QueryBuilder::new(
            "SELECT media_id, show_id, position_ms, duration_ms, watched, updated_at \
             FROM watch_state WHERE principal = ",
        );
        query
            .push_bind(principal.to_string())
            .push(" AND media_id IN (");
        let mut ids = query.separated(", ");
        for id in media_ids {
            ids.push_bind(id);
        }
        query.push(")");
        let rows: Vec<WatchRow> = query.build_query_as().fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(|r| (r.media_id.clone(), r)).collect())
    }

    /// Items started (position at least `min_position_ms`) and not
    /// watched, most recently updated first.
    pub async fn in_progress(
        &self,
        principal: Principal,
        min_position_ms: i64,
        limit: u32,
    ) -> Result<Vec<WatchRow>, StoreError> {
        Ok(sqlx::query_as(
            "SELECT media_id, show_id, position_ms, duration_ms, watched, updated_at \
             FROM watch_state \
             WHERE principal = ?1 AND watched = 0 AND position_ms >= ?2 \
             ORDER BY updated_at DESC LIMIT ?3",
        )
        .bind(principal.to_string())
        .bind(min_position_ms)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Shows with an episode watched or in progress since `since` (Unix
    /// seconds), the most recently active first.
    pub async fn active_shows(
        &self,
        principal: Principal,
        since: i64,
        limit: u32,
    ) -> Result<Vec<String>, StoreError> {
        Ok(sqlx::query_scalar(
            "SELECT show_id FROM watch_state \
             WHERE principal = ?1 AND show_id IS NOT NULL AND updated_at >= ?2 \
             GROUP BY show_id ORDER BY max(updated_at) DESC LIMIT ?3",
        )
        .bind(principal.to_string())
        .bind(since)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// The watched episodes of `show_id`.
    pub async fn watched_in_show(
        &self,
        principal: Principal,
        show_id: &str,
    ) -> Result<HashSet<String>, StoreError> {
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT media_id FROM watch_state \
             WHERE principal = ?1 AND show_id = ?2 AND watched = 1",
        )
        .bind(principal.to_string())
        .bind(show_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(ids.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::state_dir;
    use super::*;
    use crate::store::users::LocalUserId;

    const ALICE: Principal = Principal::LocalUser(LocalUserId(1));
    const BOB: Principal = Principal::LocalUser(LocalUserId(2));

    fn at<'a>(media_id: &'a str, position_ms: i64, ids: &'a [String]) -> WatchUpdate<'a> {
        WatchUpdate {
            media_id,
            show_id: None,
            external_ids: ids,
            position_ms: Some(position_ms),
            duration_ms: Some(7_000_000),
            watched: false,
        }
    }

    async fn store(test: &str) -> Store {
        Store::open(&state_dir(test)).await.unwrap()
    }

    #[tokio::test]
    async fn each_principal_has_their_own_state() {
        let store = store("principals").await;
        store
            .save_watch(ALICE, at("plex:movie:1", 120_000, &[]))
            .await
            .unwrap();
        store
            .save_watch(BOB, at("plex:movie:1", 900_000, &[]))
            .await
            .unwrap();
        let ids = ["plex:movie:1".to_owned()];
        let alice = store.watch_states(ALICE, &ids).await.unwrap();
        let bob = store.watch_states(BOB, &ids).await.unwrap();
        assert_eq!(alice["plex:movie:1"].position_ms, Some(120_000));
        assert_eq!(bob["plex:movie:1"].position_ms, Some(900_000));
    }

    #[tokio::test]
    async fn states_are_looked_up_in_one_batch() {
        let store = store("batch").await;
        for n in 0..5 {
            store
                .save_watch(ALICE, at(&format!("plex:movie:{n}"), 70_000, &[]))
                .await
                .unwrap();
        }
        let ids: Vec<String> = (0..8).map(|n| format!("plex:movie:{n}")).collect();
        assert_eq!(store.watch_states(ALICE, &ids).await.unwrap().len(), 5);
    }

    #[tokio::test]
    async fn a_state_moves_to_an_item_s_new_key_through_its_external_ids() {
        let store = store("relink").await;
        let ids = vec!["imdb://tt0078748".to_owned(), "tmdb://348".to_owned()];
        store
            .save_watch(ALICE, at("plex:movie:1", 120_000, &ids))
            .await
            .unwrap();

        // Plex rebuilt the library: the same movie is now plex:movie:99.
        let new_ids = vec!["tmdb://348".to_owned()];
        assert!(
            store
                .relink_watch(ALICE, "plex:movie:99", &new_ids)
                .await
                .unwrap()
        );
        let found = store
            .watch_states(
                ALICE,
                &["plex:movie:99".to_owned(), "plex:movie:1".to_owned()],
            )
            .await
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found["plex:movie:99"].position_ms, Some(120_000));
        // Nothing to move a second time.
        assert!(
            !store
                .relink_watch(ALICE, "plex:movie:99", &new_ids)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn another_principal_s_state_is_never_relinked() {
        let store = store("relink-other").await;
        let ids = vec!["imdb://tt0078748".to_owned()];
        store
            .save_watch(BOB, at("plex:movie:1", 120_000, &ids))
            .await
            .unwrap();
        assert!(
            !store
                .relink_watch(ALICE, "plex:movie:99", &ids)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn in_progress_is_started_unwatched_newest_first() {
        let store = store("in-progress").await;
        store
            .save_watch(ALICE, at("plex:movie:1", 120_000, &[]))
            .await
            .unwrap();
        store
            .save_watch(ALICE, at("plex:movie:2", 30_000, &[]))
            .await
            .unwrap();
        let mut watched = at("plex:movie:3", 6_900_000, &[]);
        watched.watched = true;
        store.save_watch(ALICE, watched).await.unwrap();
        // Make movie 1 the older one.
        sqlx::query(
            "UPDATE watch_state SET updated_at = updated_at - 100 WHERE media_id = 'plex:movie:1'",
        )
        .execute(&store.pool)
        .await
        .unwrap();
        store
            .save_watch(ALICE, at("plex:movie:4", 200_000, &[]))
            .await
            .unwrap();
        let got: Vec<_> = store
            .in_progress(ALICE, 60_000, 10)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.media_id)
            .collect();
        assert_eq!(got, ["plex:movie:4", "plex:movie:1"]);
    }
}
