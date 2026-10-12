//! Watch state rows (P1-14). The rules (thresholds, throttling) live in
//! `crate::watch`; this is storage.

use std::collections::{HashMap, HashSet};

use sqlx::{QueryBuilder, Sqlite};

use super::{Store, StoreError};
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
    /// Clear a "removed from Continue watching" for the item (P1-22): the
    /// user played or marked it. Not for state taken from elsewhere
    /// (PX-04): hides are this server's own.
    pub unhide: bool,
}

impl Store {
    /// Save one item's state, with its external ids.
    pub async fn save_watch(
        &self,
        principal: Principal,
        update: WatchUpdate<'_>,
    ) -> Result<(), StoreError> {
        self.save_watch_at(principal, update, self.now()).await
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
        let mut tx = self.begin_write().await?;
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
        if update.unhide {
            clear_hidden(&mut *tx, &principal, update.media_id).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Remove `media_id` (a movie or an episode: the card, never a show)
    /// from `principal`'s Continue watching and Next up, until its watch
    /// state changes (P1-22). `external_ids` let the hide follow the item
    /// to a new key.
    pub async fn hide_continue(
        &self,
        principal: Principal,
        media_id: &str,
        external_ids: &[String],
    ) -> Result<(), StoreError> {
        let principal = principal.to_string();
        let mut tx = self.begin_write().await?;
        sqlx::query(
            "INSERT INTO continue_hidden (principal, media_id, hidden_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT (principal, media_id) DO UPDATE SET hidden_at = excluded.hidden_at",
        )
        .bind(&principal)
        .bind(media_id)
        .bind(self.now())
        .execute(&mut *tx)
        .await?;
        for id in external_ids {
            sqlx::query(
                "INSERT INTO continue_hidden_ids (principal, media_id, external_id) \
                 VALUES (?1, ?2, ?3) \
                 ON CONFLICT (principal, external_id) DO UPDATE SET media_id = excluded.media_id",
            )
            .bind(&principal)
            .bind(media_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Undo [`Store::hide_continue`].
    pub async fn unhide_continue(
        &self,
        principal: Principal,
        media_id: &str,
    ) -> Result<(), StoreError> {
        // One statement: it takes the writer when it starts, so it needs
        // no transaction of its own.
        clear_hidden(&self.pool, &principal.to_string(), media_id).await
    }

    /// Clear the hide of `media_id` without a state written: progress
    /// within the write throttle still counts as playing it.
    pub async fn clear_hidden(
        &self,
        principal: Principal,
        media_id: &str,
    ) -> Result<(), StoreError> {
        // Throttled progress reports usually have nothing to clear. A
        // DELETE, even of no rows, would still acquire SQLite's writer.
        // Read outside a transaction; a concurrent hide is seen on the
        // next progress report, without an in-memory invalidation cache.
        let hidden: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM continue_hidden \
             WHERE principal = ?1 AND media_id = ?2)",
        )
        .bind(principal.to_string())
        .bind(media_id)
        .fetch_one(&self.pool)
        .await?;
        if !hidden {
            return Ok(());
        }
        // One statement: it takes the writer when it starts, so it needs
        // no transaction of its own.
        clear_hidden(&self.pool, &principal.to_string(), media_id).await
    }

    /// What `principal` removed from Continue watching or Next up: the
    /// MediaIds of those cards.
    pub async fn hidden(&self, principal: Principal) -> Result<HashSet<String>, StoreError> {
        let ids: Vec<String> =
            sqlx::query_scalar("SELECT media_id FROM continue_hidden WHERE principal = ?1")
                .bind(principal.to_string())
                .fetch_all(&self.pool)
                .await?;
        Ok(ids.into_iter().collect())
    }

    /// The key an item with one of `external_ids` is hidden under, when
    /// `media_id` itself is not hidden. That key is the item's earlier one
    /// only if the item has left it, which the caller checks before
    /// [`Store::move_hidden`].
    pub async fn hidden_under_another_key(
        &self,
        principal: Principal,
        media_id: &str,
        external_ids: &[String],
    ) -> Result<Option<String>, StoreError> {
        self.under_another_key(
            "continue_hidden",
            "continue_hidden_ids",
            principal,
            media_id,
            external_ids,
        )
        .await
    }

    /// Move the hide of `from` to `to` (the item got a new key), unless
    /// `to` is hidden by now.
    pub async fn move_hidden(
        &self,
        principal: Principal,
        from: &str,
        to: &str,
    ) -> Result<(), StoreError> {
        // The ids follow through ON UPDATE CASCADE.
        sqlx::query(
            "UPDATE continue_hidden SET media_id = ?3 WHERE principal = ?1 AND media_id = ?2 \
             AND NOT EXISTS (SELECT 1 FROM continue_hidden WHERE principal = ?1 AND media_id = ?3)",
        )
        .bind(principal.to_string())
        .bind(from)
        .bind(to)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The key an item with one of `external_ids` has state under, when
    /// `media_id` itself has none. That key is the item's earlier one only
    /// if the item has left it, which the caller checks before
    /// [`Store::move_watch`]: the same film in two libraries is two items
    /// with the same external ids.
    pub async fn watch_under_another_key(
        &self,
        principal: Principal,
        media_id: &str,
        external_ids: &[String],
    ) -> Result<Option<String>, StoreError> {
        self.under_another_key(
            "watch_state",
            "watch_state_ids",
            principal,
            media_id,
            external_ids,
        )
        .await
    }

    /// The key in `table` that an item with one of `external_ids` is
    /// under (its ids in `ids_table`), when `media_id` itself is not
    /// there: one query, the absence of `media_id` in its WHERE.
    async fn under_another_key(
        &self,
        table: &'static str,
        ids_table: &'static str,
        principal: Principal,
        media_id: &str,
        external_ids: &[String],
    ) -> Result<Option<String>, StoreError> {
        if external_ids.is_empty() {
            return Ok(None);
        }
        let principal = principal.to_string();
        // The table names are this module's constants, never input.
        let mut query: QueryBuilder<Sqlite> = QueryBuilder::new(format!(
            "SELECT i.media_id FROM {ids_table} i WHERE i.principal = "
        ));
        query.push_bind(&principal).push(format!(
            " AND NOT EXISTS (SELECT 1 FROM {table} o WHERE o.principal = i.principal \
               AND o.media_id = "
        ));
        query.push_bind(media_id).push(") AND i.external_id IN (");
        let mut ids = query.separated(", ");
        for id in external_ids {
            ids.push_bind(id);
        }
        query.push(") LIMIT 1");
        Ok(query
            .build_query_scalar()
            .fetch_optional(&self.pool)
            .await?)
    }

    /// Move the state under `from` to `to` (the item got a new key). The
    /// caller looked for `from` and asked the catalog about it before
    /// this, and `to` may have a state by now (the Plex sync wrote one):
    /// then nothing moves, in the one statement that checks.
    pub async fn move_watch(
        &self,
        principal: Principal,
        from: &str,
        to: &str,
    ) -> Result<(), StoreError> {
        let principal = principal.to_string();
        // The ids follow through ON UPDATE CASCADE.
        let moved = sqlx::query(
            "UPDATE watch_state SET media_id = ?3 WHERE principal = ?1 AND media_id = ?2 \
             AND NOT EXISTS (SELECT 1 FROM watch_state WHERE principal = ?1 AND media_id = ?3)",
        )
        .bind(&principal)
        .bind(from)
        .bind(to)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if moved > 0 {
            tracing::info!(%principal, %from, %to, "watch state re-linked");
        }
        Ok(())
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

    /// The episodes of `show_ids` started (position at least
    /// `min_position_ms`) and not watched, whatever their number: what
    /// keeps a show out of Next up, past Continue watching's length too.
    pub async fn in_progress_in_shows(
        &self,
        principal: Principal,
        min_position_ms: i64,
        show_ids: &[String],
    ) -> Result<Vec<WatchRow>, StoreError> {
        if show_ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut query: QueryBuilder<Sqlite> = QueryBuilder::new(
            "SELECT media_id, show_id, position_ms, duration_ms, watched, updated_at \
             FROM watch_state WHERE watched = 0 AND principal = ",
        );
        query
            .push_bind(principal.to_string())
            .push(" AND position_ms >= ")
            .push_bind(min_position_ms)
            .push(" AND show_id IN (");
        let mut ids = query.separated(", ");
        for id in show_ids {
            ids.push_bind(id);
        }
        query.push(")");
        Ok(query.build_query_as().fetch_all(&self.pool).await?)
    }

    /// Every user's started and unwatched items, the most recently played
    /// first, each once: what frame previews are made for first (P3-13).
    pub async fn in_progress_anyone(&self, limit: u32) -> Result<Vec<String>, StoreError> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT media_id FROM watch_state \
             WHERE watched = 0 AND position_ms > 0 \
             GROUP BY media_id ORDER BY MAX(updated_at) DESC LIMIT ?1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
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

    /// The watched episodes of `show_id`, with when each was last
    /// watched (Unix seconds).
    pub async fn watched_in_show(
        &self,
        principal: Principal,
        show_id: &str,
    ) -> Result<HashMap<String, i64>, StoreError> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT media_id, updated_at FROM watch_state \
             WHERE principal = ?1 AND show_id = ?2 AND watched = 1",
        )
        .bind(principal.to_string())
        .bind(show_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().collect())
    }
}

async fn clear_hidden<'e>(
    db: impl sqlx::SqliteExecutor<'e>,
    principal: &str,
    media_id: &str,
) -> Result<(), StoreError> {
    sqlx::query("DELETE FROM continue_hidden WHERE principal = ?1 AND media_id = ?2")
        .bind(principal)
        .bind(media_id)
        .execute(db)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::tests::{TestStore, test_store};
    use super::*;
    use crate::store::users::LocalUserId;

    const ALICE: Principal = Principal::LocalUser(LocalUserId::from_uuid(uuid::Uuid::from_u128(1)));
    const BOB: Principal = Principal::LocalUser(LocalUserId::from_uuid(uuid::Uuid::from_u128(2)));

    #[tokio::test]
    async fn clearing_nothing_does_not_wait_for_sqlites_writer() {
        let store = store("clear-hidden-read-only").await;
        let mut writer = store.pool.acquire().await.unwrap();
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut *writer)
            .await
            .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            store.clear_hidden(ALICE, "plex:episode:1"),
        )
        .await
        .expect("a no-op clear tried to acquire the writer")
        .unwrap();
        sqlx::query("ROLLBACK").execute(&mut *writer).await.unwrap();
        // A later hide must still be cleared on the next progress report.
        store
            .hide_continue(ALICE, "plex:episode:1", &[])
            .await
            .unwrap();
        store.clear_hidden(ALICE, "plex:episode:1").await.unwrap();
        assert!(store.hidden(ALICE).await.unwrap().is_empty());
    }

    fn at<'a>(media_id: &'a str, position_ms: i64, ids: &'a [String]) -> WatchUpdate<'a> {
        WatchUpdate {
            media_id,
            show_id: None,
            external_ids: ids,
            position_ms: Some(position_ms),
            duration_ms: Some(7_000_000),
            watched: false,
            unhide: true,
        }
    }

    #[tokio::test]
    async fn a_hide_is_per_principal_and_cleared_by_a_write_for_that_item_only() {
        let store = store("hidden").await;
        for id in ["plex:movie:1", "plex:episode:70", "plex:episode:71"] {
            store.hide_continue(ALICE, id, &[]).await.unwrap();
        }
        assert_eq!(store.hidden(ALICE).await.unwrap().len(), 3);
        assert!(store.hidden(BOB).await.unwrap().is_empty());

        store
            .save_watch(ALICE, at("plex:movie:1", 120_000, &[]))
            .await
            .unwrap();
        // Another episode of the same show leaves 71 hidden.
        let episode = WatchUpdate {
            show_id: Some("plex:show:7"),
            ..at("plex:episode:70", 120_000, &[])
        };
        store.save_watch(ALICE, episode).await.unwrap();
        assert_eq!(
            store.hidden(ALICE).await.unwrap(),
            ["plex:episode:71".to_owned()].into()
        );
    }

    #[tokio::test]
    async fn a_write_from_elsewhere_leaves_a_hide_alone() {
        let store = store("hidden-sync").await;
        store
            .hide_continue(ALICE, "plex:movie:1", &[])
            .await
            .unwrap();
        let synced = WatchUpdate {
            unhide: false,
            ..at("plex:movie:1", 120_000, &[])
        };
        store.save_watch_at(ALICE, synced, 1).await.unwrap();
        assert!(store.hidden(ALICE).await.unwrap().contains("plex:movie:1"));
        store.unhide_continue(ALICE, "plex:movie:1").await.unwrap();
        assert!(store.hidden(ALICE).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_hide_is_found_and_moved_through_its_external_ids() {
        let store = store("hidden-relink").await;
        let ids = vec!["imdb://tt0078748".to_owned()];
        store
            .hide_continue(ALICE, "plex:movie:1", &ids)
            .await
            .unwrap();
        let old = store
            .hidden_under_another_key(ALICE, "plex:movie:99", &ids)
            .await
            .unwrap();
        assert_eq!(old.as_deref(), Some("plex:movie:1"));
        store
            .move_hidden(ALICE, "plex:movie:1", "plex:movie:99")
            .await
            .unwrap();
        let hidden = store.hidden(ALICE).await.unwrap();
        assert!(hidden.contains("plex:movie:99") && !hidden.contains("plex:movie:1"));
    }

    async fn store(test: &str) -> TestStore {
        test_store(test).await
    }

    /// Before migration 0015 a removed episode hid its show, stored under
    /// the show's id. Those hides are dropped, so the show comes back; a
    /// movie's and an episode's own stay.
    #[tokio::test]
    async fn the_migration_drops_the_hides_of_shows() {
        let dir = super::super::tests::state_dir("hidden-per-card").await;
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(dir.as_ref().join(super::super::DB_FILE))
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = sqlx::pool::PoolOptions::<Sqlite>::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        let mut before = sqlx::migrate!("./migrations");
        before.migrations = before
            .iter()
            .filter(|m| m.version < 15)
            .cloned()
            .collect::<Vec<_>>()
            .into();
        before.run(&pool).await.unwrap();
        sqlx::raw_sql(
            "INSERT INTO continue_hidden (principal, media_id, hidden_at) \
               VALUES ('local:a', 'plex:show:7', 1), ('local:a', 'plex:movie:1', 2), \
                      ('local:b', 'plex:episode:70', 3); \
             INSERT INTO continue_hidden_ids (principal, media_id, external_id) \
               VALUES ('local:a', 'plex:show:7', 'tvdb://7'), \
                      ('local:a', 'plex:movie:1', 'imdb://tt1');",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        let store = Store::open(dir.as_ref()).await.unwrap();
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT principal, media_id FROM continue_hidden ORDER BY media_id")
                .fetch_all(&store.pool)
                .await
                .unwrap();
        let ids: Vec<String> = sqlx::query_scalar("SELECT external_id FROM continue_hidden_ids")
            .fetch_all(&store.pool)
            .await
            .unwrap();
        store.pool.close().await;
        assert_eq!(
            rows,
            [
                ("local:b".to_owned(), "plex:episode:70".to_owned()),
                ("local:a".to_owned(), "plex:movie:1".to_owned()),
            ]
        );
        assert_eq!(ids, ["imdb://tt1"]);
    }

    #[tokio::test]
    async fn everyone_s_items_in_progress_come_most_recently_played_first_and_once() {
        let store = store("in-progress-anyone").await;
        store
            .save_watch_at(ALICE, at("plex:movie:1", 60_000, &[]), 100)
            .await
            .unwrap();
        store
            .save_watch_at(BOB, at("plex:movie:2", 60_000, &[]), 300)
            .await
            .unwrap();
        // Alice plays 2 too, earlier than Bob: listed once, as of Bob.
        store
            .save_watch_at(ALICE, at("plex:movie:2", 30_000, &[]), 200)
            .await
            .unwrap();
        let watched = WatchUpdate {
            watched: true,
            ..at("plex:movie:3", 60_000, &[])
        };
        store.save_watch_at(BOB, watched, 400).await.unwrap();
        store
            .save_watch_at(BOB, at("plex:movie:4", 0, &[]), 500)
            .await
            .unwrap();
        assert_eq!(
            store.in_progress_anyone(10).await.unwrap(),
            ["plex:movie:2", "plex:movie:1"]
        );
        assert_eq!(store.in_progress_anyone(1).await.unwrap(), ["plex:movie:2"]);
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
    async fn a_state_is_found_and_moved_through_its_external_ids() {
        let store = store("relink").await;
        let ids = vec!["imdb://tt0078748".to_owned(), "tmdb://348".to_owned()];
        store
            .save_watch(ALICE, at("plex:movie:1", 120_000, &ids))
            .await
            .unwrap();

        // Plex rebuilt the library: the same movie is now plex:movie:99.
        let new_ids = vec!["tmdb://348".to_owned()];
        let old = store
            .watch_under_another_key(ALICE, "plex:movie:99", &new_ids)
            .await
            .unwrap();
        assert_eq!(old.as_deref(), Some("plex:movie:1"));
        store
            .move_watch(ALICE, "plex:movie:1", "plex:movie:99")
            .await
            .unwrap();
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
        let again = store
            .watch_under_another_key(ALICE, "plex:movie:99", &new_ids)
            .await
            .unwrap();
        assert_eq!(again, None);
    }

    /// Between the search for an earlier key and the move, something else
    /// (the Plex sync) wrote a state for the new key.
    #[tokio::test]
    async fn a_state_is_not_moved_onto_a_key_that_has_one() {
        let store = store("relink-taken").await;
        let ids = vec!["imdb://tt0078748".to_owned()];
        store
            .save_watch(ALICE, at("plex:movie:1", 120_000, &ids))
            .await
            .unwrap();
        store
            .save_watch(ALICE, at("plex:movie:99", 900_000, &[]))
            .await
            .unwrap();
        store
            .move_watch(ALICE, "plex:movie:1", "plex:movie:99")
            .await
            .unwrap();
        let found = store
            .watch_states(
                ALICE,
                &["plex:movie:99".to_owned(), "plex:movie:1".to_owned()],
            )
            .await
            .unwrap();
        assert_eq!(found["plex:movie:1"].position_ms, Some(120_000));
        assert_eq!(found["plex:movie:99"].position_ms, Some(900_000));
    }

    #[tokio::test]
    async fn a_hide_is_not_moved_onto_a_key_that_has_one() {
        let store = store("hidden-relink-taken").await;
        store
            .hide_continue(ALICE, "plex:movie:1", &[])
            .await
            .unwrap();
        store
            .hide_continue(ALICE, "plex:movie:99", &[])
            .await
            .unwrap();
        store
            .move_hidden(ALICE, "plex:movie:1", "plex:movie:99")
            .await
            .unwrap();
        assert_eq!(store.hidden(ALICE).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn another_principal_s_state_is_never_found() {
        let store = store("relink-other").await;
        let ids = vec!["imdb://tt0078748".to_owned()];
        store
            .save_watch(BOB, at("plex:movie:1", 120_000, &ids))
            .await
            .unwrap();
        let old = store
            .watch_under_another_key(ALICE, "plex:movie:99", &ids)
            .await
            .unwrap();
        assert_eq!(old, None);
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
