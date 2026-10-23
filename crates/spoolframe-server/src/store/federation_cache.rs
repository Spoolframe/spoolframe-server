//! This server's cache of what paired servers share with it (P8-02): their
//! libraries, items, external ids and artwork files, by origin (the
//! sharing server's server id) and the origin's own ids.
//!
//! A sync's pages are staged as they come, and written to the cache in
//! one transaction once the answer is whole: an answer cut short, or past
//! a limit, leaves the cache and the library's cursor as they were. A
//! whole library (a snapshot) drops the items it did not name.
//!
//! When a library or a pair goes, the items a user here still holds
//! (watched, liked, in a playlist, downloaded) are kept, flagged
//! unavailable, without artwork; the rest go.

use std::collections::HashSet;

use spoolframe_protocol::federation::{RemoteItem, SharedLibrary};
use sqlx::Row;

use super::{Store, StoreError};

/// A cached library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedLibrary {
    pub origin: String,
    /// Its id on the origin.
    pub library: String,
    /// The pair row it came over.
    pub peer: String,
    pub title: String,
    /// `movies`, `shows` or `music`.
    pub kind: String,
    /// The last whole sync's cursor.
    pub cursor: Option<String>,
    pub synced_at: Option<i64>,
    /// How many whole syncs brought a change: the revision held here.
    pub revision: i64,
}

/// A cached item, as the origin sent it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedItem {
    pub item: RemoteItem,
    /// The origin's library it is in.
    pub library: String,
    /// Kept after its library or pair went: not browsed, no artwork.
    pub unavailable: bool,
}

/// How a whole sync went into the cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheSynced {
    Written,
    /// Not written: the origin's items would have been `items`, past the
    /// limit. The cache stays as it was.
    OverLimit {
        items: u64,
    },
}

/// The most the paired servers' image files take, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileLimits {
    /// Each origin's.
    pub per_origin: u64,
    /// Every origin's together.
    pub total: u64,
}

/// Delete the least recently used files, in `tx`, until each origin's are
/// within `limits.per_origin` and all of them within `limits.total`: the
/// origins and names of the files gone. Each step reads the kept total
/// and the one oldest file by index, and stops once under.
async fn evict(
    tx: &mut sqlx::Transaction<'static, sqlx::Sqlite>,
    limits: FileLimits,
) -> Result<Vec<(String, String)>, StoreError> {
    let cap = |limit: u64| i64::try_from(limit).unwrap_or(i64::MAX);
    let mut gone = Vec::new();
    let over: Vec<String> =
        sqlx::query_scalar("SELECT origin FROM federation_cache_bytes WHERE bytes > ?1")
            .bind(cap(limits.per_origin))
            .fetch_all(&mut **tx)
            .await?;
    for origin in &over {
        evict_until(tx, &mut gone, Some(origin), cap(limits.per_origin)).await?;
    }
    evict_until(tx, &mut gone, None, cap(limits.total)).await?;
    Ok(gone)
}

/// Delete the least recently used files of `origin`, or of every origin,
/// into `gone`, until their bytes are within `limit`.
async fn evict_until(
    tx: &mut sqlx::Transaction<'static, sqlx::Sqlite>,
    gone: &mut Vec<(String, String)>,
    origin: Option<&str>,
    limit: i64,
) -> Result<(), StoreError> {
    loop {
        let held: i64 = match origin {
            Some(origin) => {
                sqlx::query_scalar("SELECT bytes FROM federation_cache_bytes WHERE origin = ?1")
                    .bind(origin)
                    .fetch_optional(&mut **tx)
                    .await?
                    .unwrap_or(0)
            }
            None => {
                sqlx::query_scalar("SELECT coalesce(sum(bytes), 0) FROM federation_cache_bytes")
                    .fetch_one(&mut **tx)
                    .await?
            }
        };
        if held <= limit {
            return Ok(());
        }
        let oldest: Option<(String, String, String)> = match origin {
            Some(origin) => {
                sqlx::query_as(
                    "SELECT origin, artwork, file FROM federation_cache_files \
                     WHERE origin = ?1 ORDER BY last_used, rowid LIMIT 1",
                )
                .bind(origin)
                .fetch_optional(&mut **tx)
                .await?
            }
            None => {
                sqlx::query_as(
                    "SELECT origin, artwork, file FROM federation_cache_files \
                     ORDER BY last_used, rowid LIMIT 1",
                )
                .fetch_optional(&mut **tx)
                .await?
            }
        };
        let Some((of, artwork, file)) = oldest else {
            return Ok(());
        };
        sqlx::query("DELETE FROM federation_cache_files WHERE origin = ?1 AND artwork = ?2")
            .bind(&of)
            .bind(&artwork)
            .execute(&mut **tx)
            .await?;
        gone.push((of, file));
    }
}

/// An image fetched from an origin, as a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedFile {
    pub file: String,
    pub content_type: String,
}

fn library(row: &sqlx::sqlite::SqliteRow) -> Result<CachedLibrary, sqlx::Error> {
    Ok(CachedLibrary {
        origin: row.try_get("origin")?,
        library: row.try_get("library")?,
        peer: row.try_get("peer")?,
        title: row.try_get("title")?,
        kind: row.try_get("kind")?,
        cursor: row.try_get("cursor")?,
        synced_at: row.try_get("synced_at")?,
        revision: row.try_get("revision")?,
    })
}

/// The item a row's `body` holds; a body that does not read, as none.
fn body(text: &str) -> Option<RemoteItem> {
    serde_json::from_str(text).ok()
}

/// `text` for a `LIKE … ESCAPE '\'`: its own `%`, `_` and `\` matched
/// as themselves.
fn like_pattern(text: &str) -> String {
    let mut pattern = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '%' | '_' | '\\') {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    pattern
}

fn bodies(rows: Vec<String>) -> Vec<RemoteItem> {
    rows.iter().filter_map(|b| body(b)).collect()
}

/// A library row's columns: a macro, so each query is one `concat!`ed
/// literal.
macro_rules! library_columns {
    () => {
        "origin, library, peer, title, kind, cursor, synced_at, revision"
    };
}

impl Store {
    /// Keep `shared` as a library origin `origin` shares over pair
    /// `peer`, as it says now; its cursor and items stay.
    pub async fn cache_library(
        &self,
        origin: &str,
        peer: &str,
        shared: &SharedLibrary,
        kind: &str,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO federation_cache_libraries (origin, library, peer, title, kind) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT (origin, library) DO UPDATE SET peer = ?3, title = ?4, kind = ?5",
        )
        .bind(origin)
        .bind(&shared.id)
        .bind(peer)
        .bind(&shared.title)
        .bind(kind)
        .execute(&self.pool)
        .await?;
        // Shared again: no longer said to be withdrawn.
        sqlx::query("DELETE FROM federation_withdrawn WHERE origin = ?1 AND library = ?2")
            .bind(origin)
            .bind(&shared.id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The cached libraries, of `origin` or of every origin.
    pub async fn cached_libraries(
        &self,
        origin: Option<&str>,
    ) -> Result<Vec<CachedLibrary>, StoreError> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            library_columns!(),
            " FROM federation_cache_libraries WHERE ?1 IS NULL OR origin = ?1 \
              ORDER BY origin, title, library"
        ))
        .bind(origin)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(library).collect::<Result<_, _>>()?)
    }

    /// Cached library `library` of `origin`.
    pub async fn cached_library(
        &self,
        origin: &str,
        library_id: &str,
    ) -> Result<Option<CachedLibrary>, StoreError> {
        let row = sqlx::query(concat!(
            "SELECT ",
            library_columns!(),
            " FROM federation_cache_libraries WHERE origin = ?1 AND library = ?2"
        ))
        .bind(origin)
        .bind(library_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(library).transpose()?)
    }

    /// Start a sync of `origin`'s `library_id`: what an earlier one left
    /// staged, cut short, goes.
    pub async fn begin_cache_sync(&self, origin: &str, library_id: &str) -> Result<(), StoreError> {
        self.abort_cache_sync(origin, library_id).await
    }

    /// Keep one page of a sync of `origin`'s `library_id` aside, until
    /// the answer is whole: its items, and the ids it names as gone. An
    /// item named twice is kept as last named.
    pub async fn stage_cache_page(
        &self,
        origin: &str,
        library_id: &str,
        items: &[RemoteItem],
        removed: &[String],
    ) -> Result<(), StoreError> {
        let mut tx = self.begin_write().await?;
        for item in items {
            if item.kind.as_str().is_none() {
                continue;
            }
            sqlx::query(
                "INSERT OR REPLACE INTO federation_cache_staged \
                     (origin, library, item, hash, body) VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .bind(origin)
            .bind(library_id)
            .bind(&item.id)
            .bind(&item.hash)
            .bind(serde_json::to_string(item).expect("an item serializes"))
            .execute(&mut *tx)
            .await?;
        }
        for id in removed {
            sqlx::query(
                "INSERT OR REPLACE INTO federation_cache_staged \
                     (origin, library, item, hash, body) VALUES (?1, ?2, ?3, NULL, NULL)",
            )
            .bind(origin)
            .bind(library_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Drop what a sync of `origin`'s `library_id` staged: the cache and
    /// the library's cursor stay as they were.
    pub async fn abort_cache_sync(&self, origin: &str, library_id: &str) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM federation_cache_staged WHERE origin = ?1 AND library = ?2")
            .bind(origin)
            .bind(library_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// A sync of `origin`'s `library_id` over pair `peer` is whole, under
    /// `cursor`: what it staged is written, in one transaction, and the
    /// cursor kept as the library's. Items added or changed are written,
    /// with their external ids and artwork; those named as gone go; with
    /// `snapshot`, the answer was the whole library, and the items it did
    /// not name go too. Past `max_items` items of `origin`'s in all,
    /// nothing is written. Either way the staged pages go.
    ///
    /// An item kept unavailable from another pair is not written, unless
    /// that pair had `peer`'s key (M2): a server that pairs later under
    /// the same server id does not take over the ids a user here holds.
    pub async fn finish_cache_sync(
        &self,
        origin: &str,
        peer: &str,
        library_id: &str,
        cursor: &str,
        snapshot: bool,
        max_items: u64,
    ) -> Result<CacheSynced, StoreError> {
        let mut tx = self.begin_write().await?;
        // Only what is new or changed is read back: an item whose hash is
        // the one kept stays as it is, but for the library it is in.
        let changed: Vec<(String, bool)> = sqlx::query_as(
            "SELECT s.body, i.item IS NOT NULL FROM federation_cache_staged s \
             LEFT JOIN federation_cache_items i ON i.origin = s.origin AND i.item = s.item \
             WHERE s.origin = ?1 AND s.library = ?2 AND s.body IS NOT NULL \
               AND (i.item IS NULL OR i.hash IS NOT s.hash OR i.unavailable = 1) \
               AND (i.item IS NULL OR i.unavailable = 0 OR i.peer = ?3 \
                    OR (SELECT endpoint_id FROM federation_peers WHERE id = i.peer) = \
                       (SELECT endpoint_id FROM federation_peers WHERE id = ?3))",
        )
        .bind(origin)
        .bind(library_id)
        .bind(peer)
        .fetch_all(&mut *tx)
        .await?;
        let taken_over: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM federation_cache_staged s \
             JOIN federation_cache_items i ON i.origin = s.origin AND i.item = s.item \
             WHERE s.origin = ?1 AND s.library = ?2 AND s.body IS NOT NULL \
               AND i.unavailable = 1 AND i.peer <> ?3 \
               AND (SELECT endpoint_id FROM federation_peers WHERE id = i.peer) IS NOT \
                   (SELECT endpoint_id FROM federation_peers WHERE id = ?3)",
        )
        .bind(origin)
        .bind(library_id)
        .bind(peer)
        .fetch_one(&mut *tx)
        .await?;
        if taken_over > 0 {
            tracing::warn!(
                origin,
                peer,
                items = taken_over,
                "a server paired under a server id another key had names items a user here \
                 holds: they stay unavailable"
            );
        }
        for (text, had) in changed {
            let Some(item) = body(&text) else {
                continue;
            };
            let Some(kind) = item.kind.as_str() else {
                continue;
            };
            sqlx::query(
                "INSERT INTO federation_cache_items \
                     (origin, item, library, kind, parent, grandparent, title, position, year, \
                      body, hash, unavailable, cached_at, peer) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 0, ?12, ?13) \
                 ON CONFLICT (origin, item) DO UPDATE SET \
                     library = ?3, kind = ?4, parent = ?5, grandparent = ?6, title = ?7, \
                     position = ?8, year = ?9, body = ?10, hash = ?11, unavailable = 0, \
                     peer = ?13",
            )
            .bind(origin)
            .bind(&item.id)
            .bind(library_id)
            .bind(kind)
            .bind(&item.parent)
            .bind(&item.grandparent)
            .bind(&item.title)
            .bind(item.index.map(i64::from))
            .bind(item.year.map(i64::from))
            .bind(&text)
            .bind(&item.hash)
            .bind(self.now())
            .bind(peer)
            .execute(&mut *tx)
            .await?;
            // A new row has nothing of its own yet to replace.
            if had {
                for table in [
                    "DELETE FROM federation_cache_external_ids WHERE origin = ?1 AND item = ?2",
                    "DELETE FROM federation_cache_item_artwork WHERE origin = ?1 AND item = ?2",
                ] {
                    sqlx::query(table)
                        .bind(origin)
                        .bind(&item.id)
                        .execute(&mut *tx)
                        .await?;
                }
            }
            for id in &item.external_ids {
                sqlx::query(
                    "INSERT OR IGNORE INTO federation_cache_external_ids \
                         (origin, item, external_id) VALUES (?1, ?2, ?3)",
                )
                .bind(origin)
                .bind(&item.id)
                .bind(id)
                .execute(&mut *tx)
                .await?;
            }
            for artwork in &item.artwork {
                let Some(kind) = artwork.kind.as_str() else {
                    continue;
                };
                sqlx::query(
                    "INSERT OR IGNORE INTO federation_cache_item_artwork \
                         (origin, item, artwork, kind) VALUES (?1, ?2, ?3, ?4)",
                )
                .bind(origin)
                .bind(&item.id)
                .bind(&artwork.id)
                .bind(kind)
                .execute(&mut *tx)
                .await?;
            }
        }
        // An unchanged item may have moved library on the origin.
        sqlx::query(
            "UPDATE federation_cache_items SET library = ?2 \
             WHERE origin = ?1 AND library IS NOT ?2 AND unavailable = 0 AND item IN \
                 (SELECT item FROM federation_cache_staged \
                  WHERE origin = ?1 AND library = ?2 AND body IS NOT NULL)",
        )
        .bind(origin)
        .bind(library_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM federation_cache_items \
             WHERE origin = ?1 AND library = ?2 AND unavailable = 0 AND item IN \
                 (SELECT item FROM federation_cache_staged \
                  WHERE origin = ?1 AND library = ?2 AND body IS NULL)",
        )
        .bind(origin)
        .bind(library_id)
        .execute(&mut *tx)
        .await?;
        if snapshot {
            sqlx::query(
                "DELETE FROM federation_cache_items \
                 WHERE origin = ?1 AND library = ?2 AND unavailable = 0 AND item NOT IN \
                     (SELECT item FROM federation_cache_staged \
                      WHERE origin = ?1 AND library = ?2 AND body IS NOT NULL)",
            )
            .bind(origin)
            .bind(library_id)
            .execute(&mut *tx)
            .await?;
        }
        let held: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM federation_cache_items WHERE origin = ?1")
                .bind(origin)
                .fetch_one(&mut *tx)
                .await?;
        let held = u64::try_from(held).unwrap_or_default();
        if held > max_items {
            tx.rollback().await?;
            self.abort_cache_sync(origin, library_id).await?;
            return Ok(CacheSynced::OverLimit { items: held });
        }
        sqlx::query("DELETE FROM federation_cache_staged WHERE origin = ?1 AND library = ?2")
            .bind(origin)
            .bind(library_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE federation_cache_libraries \
             SET revision = revision + (cursor IS NOT ?3), cursor = ?3, synced_at = ?4 \
             WHERE origin = ?1 AND library = ?2",
        )
        .bind(origin)
        .bind(library_id)
        .bind(cursor)
        .bind(self.now())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(CacheSynced::Written)
    }

    /// Drop `origin`'s cached `library_id`, or every library of it with
    /// `None`: the items in `keep` (origin ids a user here holds) stay,
    /// flagged unavailable and without artwork; the rest go, and the
    /// libraries' rows.
    pub async fn drop_cached(
        &self,
        origin: &str,
        library_id: Option<&str>,
        keep: &HashSet<String>,
    ) -> Result<(), StoreError> {
        let mut tx = self.begin_write().await?;
        let items: Vec<String> = sqlx::query_scalar(
            "SELECT item FROM federation_cache_items \
             WHERE origin = ?1 AND (?2 IS NULL OR library = ?2)",
        )
        .bind(origin)
        .bind(library_id)
        .fetch_all(&mut *tx)
        .await?;
        for item in items {
            if keep.contains(&item) {
                sqlx::query(
                    "UPDATE federation_cache_items SET unavailable = 1 \
                     WHERE origin = ?1 AND item = ?2",
                )
                .bind(origin)
                .bind(&item)
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    "DELETE FROM federation_cache_item_artwork WHERE origin = ?1 AND item = ?2",
                )
                .bind(origin)
                .bind(&item)
                .execute(&mut *tx)
                .await?;
            } else {
                sqlx::query("DELETE FROM federation_cache_items WHERE origin = ?1 AND item = ?2")
                    .bind(origin)
                    .bind(&item)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        for table in [
            "DELETE FROM federation_cache_staged \
             WHERE origin = ?1 AND (?2 IS NULL OR library = ?2)",
            "DELETE FROM federation_cache_libraries \
             WHERE origin = ?1 AND (?2 IS NULL OR library = ?2)",
            // Unpaired: what it withdrew is no longer news.
            "DELETE FROM federation_withdrawn WHERE origin = ?1 AND ?2 IS NULL",
        ] {
            sqlx::query(table)
                .bind(origin)
                .bind(library_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Keep that `origin`, over pair `peer`, stopped sharing `library_id`
    /// (titled `title`) now.
    pub async fn withdraw_cached(
        &self,
        origin: &str,
        peer: &str,
        library_id: &str,
        title: &str,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO federation_withdrawn (origin, library, peer, title, withdrawn_at) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT (origin, library) DO UPDATE SET \
                 peer = ?3, title = ?4, withdrawn_at = ?5",
        )
        .bind(origin)
        .bind(library_id)
        .bind(peer)
        .bind(title)
        .bind(self.now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The libraries pair `peer` stopped sharing at `since` or after, as
    /// (library, title, when), the latest first.
    pub async fn withdrawn(
        &self,
        peer: &str,
        since: i64,
    ) -> Result<Vec<(String, String, i64)>, StoreError> {
        Ok(sqlx::query_as(
            "SELECT library, title, withdrawn_at FROM federation_withdrawn \
             WHERE peer = ?1 AND withdrawn_at >= ?2 ORDER BY withdrawn_at DESC, title",
        )
        .bind(peer)
        .bind(since)
        .fetch_all(&self.pool)
        .await?)
    }

    /// How many of `origin`'s films, shows and artists each cached library
    /// holds, as (library, count): what a user here browses first.
    pub async fn cached_counts(&self, origin: &str) -> Result<Vec<(String, i64)>, StoreError> {
        Ok(sqlx::query_as(
            "SELECT library, COUNT(*) FROM federation_cache_items \
             WHERE origin = ?1 AND unavailable = 0 AND kind IN ('movie', 'show', 'artist') \
             GROUP BY library",
        )
        .bind(origin)
        .fetch_all(&self.pool)
        .await?)
    }

    /// The bytes of `origin`'s artwork kept here.
    pub async fn cached_bytes(&self, origin: &str) -> Result<u64, StoreError> {
        let bytes: Option<i64> =
            sqlx::query_scalar("SELECT bytes FROM federation_cache_bytes WHERE origin = ?1")
                .bind(origin)
                .fetch_optional(&self.pool)
                .await?;
        Ok(bytes
            .and_then(|b| u64::try_from(b).ok())
            .unwrap_or_default())
    }

    /// Every `fed:` id a user of this server holds: in their watch state,
    /// plays, likes, playlists or downloads. What a dropped cache keeps.
    pub async fn held_federated_ids(&self) -> Result<Vec<String>, StoreError> {
        Ok(sqlx::query_scalar(
            "SELECT media_id FROM watch_state WHERE media_id LIKE 'fed:%' \
             UNION SELECT media_id FROM plays WHERE media_id LIKE 'fed:%' \
             UNION SELECT media_id FROM liked_changes WHERE media_id LIKE 'fed:%' \
             UNION SELECT media_id FROM playlist_entries WHERE media_id LIKE 'fed:%' \
             UNION SELECT media_id FROM downloads WHERE media_id LIKE 'fed:%'",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// Cached item `item` of `origin`, available or not.
    pub async fn cached_item(
        &self,
        origin: &str,
        item: &str,
    ) -> Result<Option<CachedItem>, StoreError> {
        let row: Option<(String, String, bool)> = sqlx::query_as(
            "SELECT body, library, unavailable FROM federation_cache_items \
             WHERE origin = ?1 AND item = ?2",
        )
        .bind(origin)
        .bind(item)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|(text, library, unavailable)| {
            Some(CachedItem {
                item: body(&text)?,
                library,
                unavailable,
            })
        }))
    }

    /// The available items under `parent` of `origin`'s: a show's
    /// seasons, a season's episodes, an artist's albums, an album's
    /// tracks, in their order.
    pub async fn cached_children(
        &self,
        origin: &str,
        parent: &str,
    ) -> Result<Vec<RemoteItem>, StoreError> {
        let rows = sqlx::query_scalar(
            "SELECT body FROM federation_cache_items \
             WHERE origin = ?1 AND parent = ?2 AND unavailable = 0 \
             ORDER BY position IS NULL, position, title, item",
        )
        .bind(origin)
        .bind(parent)
        .fetch_all(&self.pool)
        .await?;
        Ok(bodies(rows))
    }

    /// The available items of `kind` under `grandparent` of `origin`'s: a
    /// show's episodes.
    pub async fn cached_grandchildren(
        &self,
        origin: &str,
        grandparent: &str,
        kind: &str,
    ) -> Result<Vec<RemoteItem>, StoreError> {
        let rows = sqlx::query_scalar(
            "SELECT body FROM federation_cache_items \
             WHERE origin = ?1 AND grandparent = ?2 AND kind = ?3 AND unavailable = 0 \
             ORDER BY item",
        )
        .bind(origin)
        .bind(grandparent)
        .bind(kind)
        .fetch_all(&self.pool)
        .await?;
        Ok(bodies(rows))
    }

    /// One page of the available items of `kind` in `origin`'s
    /// `library_id`, by title; and how many there are.
    pub async fn cached_page(
        &self,
        origin: &str,
        library_id: &str,
        kind: &str,
        offset: u32,
        limit: u32,
    ) -> Result<(Vec<RemoteItem>, u32), StoreError> {
        let rows = sqlx::query_scalar(
            "SELECT body FROM federation_cache_items \
             WHERE origin = ?1 AND library = ?2 AND kind = ?3 AND unavailable = 0 \
             ORDER BY title COLLATE NOCASE, item LIMIT ?5 OFFSET ?4",
        )
        .bind(origin)
        .bind(library_id)
        .bind(kind)
        .bind(i64::from(offset))
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM federation_cache_items \
             WHERE origin = ?1 AND library = ?2 AND kind = ?3 AND unavailable = 0",
        )
        .bind(origin)
        .bind(library_id)
        .bind(kind)
        .fetch_one(&self.pool)
        .await?;
        Ok((bodies(rows), u32::try_from(total).unwrap_or(u32::MAX)))
    }

    /// The available item of `kind` with one of `external_ids`, of any
    /// origin: its origin and id there.
    pub async fn find_cached_external(
        &self,
        kind: &str,
        external_ids: &[String],
    ) -> Result<Option<(String, String)>, StoreError> {
        for id in external_ids {
            let found: Option<(String, String)> = sqlx::query_as(
                "SELECT i.origin, i.item FROM federation_cache_external_ids e \
                 JOIN federation_cache_items i ON i.origin = e.origin AND i.item = e.item \
                 WHERE e.external_id = ?1 AND i.kind = ?2 AND i.unavailable = 0 \
                 ORDER BY i.origin, i.item LIMIT 1",
            )
            .bind(id)
            .bind(kind)
            .fetch_optional(&self.pool)
            .await?;
            if found.is_some() {
                return Ok(found);
            }
        }
        Ok(None)
    }

    /// The available items of live pairs of `kind` with one of
    /// `external_ids`: a title's copies on paired servers (P8-05), each
    /// with its origin, by origin. Of `year` when both it and the copy
    /// have one: an id a paired server says is not taken on its word.
    pub async fn cached_copies(
        &self,
        kind: &str,
        external_ids: &[String],
        year: Option<u16>,
    ) -> Result<Vec<(String, String)>, StoreError> {
        if external_ids.is_empty() {
            return Ok(Vec::new());
        }
        Ok(sqlx::query_as(
            "SELECT DISTINCT i.origin, i.item FROM federation_cache_external_ids e \
             JOIN federation_cache_items i ON i.origin = e.origin AND i.item = e.item \
             WHERE e.external_id IN (SELECT value FROM json_each(?1)) \
               AND i.kind = ?2 AND i.unavailable = 0 \
               AND (?3 IS NULL OR i.year IS NULL OR i.year = ?3) \
               AND i.origin IN (SELECT server_id FROM federation_peers WHERE revoked_at IS NULL) \
             ORDER BY i.origin, i.item",
        )
        .bind(serde_json::to_string(external_ids).expect("strings serialize"))
        .bind(kind)
        .bind(year)
        .fetch_all(&self.pool)
        .await?)
    }

    /// The available episodes of live pairs numbered `episode` of season
    /// `season` of a show with one of `show_ids`: an episode's copies by
    /// its show's ids, season and number (P8-05), by origin.
    pub async fn cached_episode_copies(
        &self,
        show_ids: &[String],
        season: u32,
        episode: u32,
    ) -> Result<Vec<(String, String)>, StoreError> {
        if show_ids.is_empty() {
            return Ok(Vec::new());
        }
        Ok(sqlx::query_as(
            "SELECT DISTINCT i.origin, i.item FROM federation_cache_external_ids e \
             JOIN federation_cache_items i \
               ON i.origin = e.origin AND i.grandparent = e.item \
             WHERE e.external_id IN (SELECT value FROM json_each(?1)) \
               AND i.kind = 'episode' AND i.unavailable = 0 AND i.position = ?3 \
               AND json_extract(i.body, '$.parent_index') = ?2 \
               AND i.origin IN (SELECT server_id FROM federation_peers WHERE revoked_at IS NULL) \
             ORDER BY i.origin, i.item",
        )
        .bind(serde_json::to_string(show_ids).expect("strings serialize"))
        .bind(season)
        .bind(episode)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Whether image `artwork` of `origin`'s is named by an available
    /// cached item: the only artwork served from it.
    pub async fn cached_artwork_named(
        &self,
        origin: &str,
        artwork: &str,
    ) -> Result<bool, StoreError> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM federation_cache_item_artwork a \
                            JOIN federation_cache_items i \
                              ON i.origin = a.origin AND i.item = a.item \
                            WHERE a.origin = ?1 AND a.artwork = ?2 AND i.unavailable = 0)",
        )
        .bind(origin)
        .bind(artwork)
        .fetch_one(&self.pool)
        .await?)
    }

    /// The file image `artwork` of `origin`'s was kept as, if it was;
    /// marked used now.
    pub async fn cached_file(
        &self,
        origin: &str,
        artwork: &str,
    ) -> Result<Option<CachedFile>, StoreError> {
        let row: Option<(String, String)> = sqlx::query_as(
            "UPDATE federation_cache_files SET last_used = ?3 \
             WHERE origin = ?1 AND artwork = ?2 RETURNING file, content_type",
        )
        .bind(origin)
        .bind(artwork)
        .bind(self.now())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(file, content_type)| CachedFile { file, content_type }))
    }

    /// Keep image `artwork` of `origin`'s as `file`, `bytes` long. Then
    /// the least recently used files past `limits` go: the origin's
    /// own, then everyone's (M3, M4). The origins and names of their
    /// files, to delete.
    pub async fn keep_cached_file(
        &self,
        origin: &str,
        artwork: &str,
        file: &CachedFile,
        bytes: u64,
        limits: FileLimits,
    ) -> Result<Vec<(String, String)>, StoreError> {
        let mut tx = self.begin_write().await?;
        sqlx::query(
            "INSERT INTO federation_cache_files \
                 (origin, artwork, file, content_type, bytes, last_used) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT (origin, artwork) DO UPDATE SET \
                 file = ?3, content_type = ?4, bytes = ?5, last_used = ?6",
        )
        .bind(origin)
        .bind(artwork)
        .bind(&file.file)
        .bind(&file.content_type)
        .bind(i64::try_from(bytes).unwrap_or(i64::MAX))
        .bind(self.now())
        .execute(&mut *tx)
        .await?;
        let gone = evict(&mut tx, limits).await?;
        tx.commit().await?;
        Ok(gone)
    }

    /// The least recently used files past `limits` go, as when one is
    /// kept: a lower limit takes effect at once. The origins and names of
    /// their files, to delete.
    pub async fn evict_cached_files(
        &self,
        limits: FileLimits,
    ) -> Result<Vec<(String, String)>, StoreError> {
        let mut tx = self.begin_write().await?;
        let gone = evict(&mut tx, limits).await?;
        tx.commit().await?;
        Ok(gone)
    }

    /// The posters of available items that have no file yet, at most
    /// `limit` of `origin`'s: what is fetched after a sync.
    pub async fn missing_posters(
        &self,
        origin: &str,
        limit: u32,
    ) -> Result<Vec<String>, StoreError> {
        Ok(sqlx::query_scalar(
            "SELECT DISTINCT a.artwork FROM federation_cache_item_artwork a \
             JOIN federation_cache_items i ON i.origin = a.origin AND i.item = a.item \
             WHERE a.origin = ?1 AND a.kind = 'poster' AND i.unavailable = 0 \
               AND NOT EXISTS (SELECT 1 FROM federation_cache_files f \
                               WHERE f.origin = a.origin AND f.artwork = a.artwork) \
             LIMIT ?2",
        )
        .bind(origin)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// The available items of live pairs (an unpaired server's rows wait
    /// to be dropped) of `kind` whose title holds `text` (any case),
    /// of `year` if one is given, in `scope` (an origin's library) or in
    /// every one cached: at most `limit` of them, those whose title starts
    /// with it first, then by title; and how many there are (P8-02's
    /// search over the cache). Each with its origin.
    pub async fn search_cached(
        &self,
        kind: &str,
        text: &str,
        year: Option<u16>,
        scope: Option<(&str, &str)>,
        limit: u32,
    ) -> Result<(Vec<(String, RemoteItem)>, u32), StoreError> {
        let pattern = like_pattern(text);
        let (origin, library) = scope.unzip();
        // Only counted (a search's other scopes): the count alone.
        if limit == 0 {
            let total: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM federation_cache_items \
                 WHERE kind = ?1 AND unavailable = 0 AND origin IN (SELECT server_id FROM federation_peers WHERE revoked_at IS NULL) \
                   AND title LIKE '%' || ?2 || '%' ESCAPE '\\' \
                   AND (?3 IS NULL OR year = ?3) \
                   AND (?4 IS NULL OR (origin = ?4 AND library = ?5))",
            )
            .bind(kind)
            .bind(&pattern)
            .bind(year)
            .bind(origin)
            .bind(library)
            .fetch_one(&self.pool)
            .await?;
            return Ok((Vec::new(), u32::try_from(total).unwrap_or(u32::MAX)));
        }
        // The page and how many there are, in one statement.
        let rows: Vec<(String, String, i64)> = sqlx::query_as(
            "SELECT origin, body, COUNT(*) OVER () FROM federation_cache_items \
             WHERE kind = ?1 AND unavailable = 0 AND origin IN (SELECT server_id FROM federation_peers WHERE revoked_at IS NULL) \
               AND title LIKE '%' || ?2 || '%' ESCAPE '\\' \
               AND (?3 IS NULL OR year = ?3) \
               AND (?4 IS NULL OR (origin = ?4 AND library = ?5)) \
             ORDER BY title NOT LIKE ?2 || '%' ESCAPE '\\', title COLLATE NOCASE, origin, item \
             LIMIT ?6",
        )
        .bind(kind)
        .bind(&pattern)
        .bind(year)
        .bind(origin)
        .bind(library)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        let total = rows.first().map_or(0, |(_, _, total)| *total);
        let found = rows
            .into_iter()
            .filter_map(|(origin, text, _)| Some((origin, body(&text)?)))
            .collect();
        Ok((found, u32::try_from(total).unwrap_or(u32::MAX)))
    }

    /// The available films, shows and albums cached most recently, of
    /// `origin` or of every origin: at most `limit`, the newest first
    /// (P8-02's recently added, and a paired server's Home row). Each with
    /// its origin.
    pub async fn recent_cached(
        &self,
        origin: Option<&str>,
        limit: u32,
    ) -> Result<Vec<(String, RemoteItem)>, StoreError> {
        // One query each way, so each walks its own index.
        let rows: Vec<(String, String)> = match origin {
            Some(origin) => {
                sqlx::query_as(
                    "SELECT origin, body FROM federation_cache_items \
                     WHERE origin = ?1 AND unavailable = 0 AND origin IN (SELECT server_id FROM federation_peers WHERE revoked_at IS NULL) \
                       AND kind IN ('movie', 'show', 'album') \
                     ORDER BY cached_at DESC, rowid DESC LIMIT ?2",
                )
                .bind(origin)
                .bind(i64::from(limit))
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query_as(
                    "SELECT origin, body FROM federation_cache_items \
                     WHERE unavailable = 0 AND origin IN (SELECT server_id FROM federation_peers WHERE revoked_at IS NULL) \
                       AND kind IN ('movie', 'show', 'album') \
                     ORDER BY cached_at DESC, rowid DESC LIMIT ?1",
                )
                .bind(i64::from(limit))
                .fetch_all(&self.pool)
                .await?
            }
        };
        Ok(rows
            .into_iter()
            .filter_map(|(origin, text)| Some((origin, body(&text)?)))
            .collect())
    }

    /// The origins whose own Home row `principal` turned off (P8-02).
    pub async fn hidden_server_rows(&self, principal: &str) -> Result<HashSet<String>, StoreError> {
        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT origin FROM federation_home_rows_hidden WHERE principal = ?1",
        )
        .bind(principal)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().collect())
    }

    /// Show `origin`'s own Home row to `principal`, or not.
    pub async fn set_server_row(
        &self,
        principal: &str,
        origin: &str,
        shown: bool,
    ) -> Result<(), StoreError> {
        if shown {
            sqlx::query(
                "DELETE FROM federation_home_rows_hidden WHERE principal = ?1 AND origin = ?2",
            )
            .bind(principal)
            .bind(origin)
            .execute(&self.pool)
            .await?;
        } else {
            sqlx::query(
                "INSERT INTO federation_home_rows_hidden (principal, origin, hidden_at) \
                 VALUES (?1, ?2, ?3) ON CONFLICT (principal, origin) DO NOTHING",
            )
            .bind(principal)
            .bind(origin)
            .bind(self.now())
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    /// Forget the files kept for `origin`'s images that no available item
    /// names any more, or every one of them with `all`: the names of the
    /// files, to delete.
    pub async fn forget_cached_files(
        &self,
        origin: &str,
        all: bool,
    ) -> Result<Vec<String>, StoreError> {
        Ok(sqlx::query_scalar(
            "DELETE FROM federation_cache_files \
             WHERE origin = ?1 AND (?2 OR NOT EXISTS ( \
                 SELECT 1 FROM federation_cache_item_artwork a \
                 JOIN federation_cache_items i ON i.origin = a.origin AND i.item = a.item \
                 WHERE a.origin = ?1 AND a.artwork = federation_cache_files.artwork \
                   AND i.unavailable = 0)) \
             RETURNING file",
        )
        .bind(origin)
        .bind(all)
        .fetch_all(&self.pool)
        .await?)
    }
}

#[cfg(test)]
mod tests {
    use spoolframe_protocol::federation::{
        RemoteArtwork, RemoteArtworkKind, RemoteKind, RemoteLibraryKind,
    };

    use super::super::federation::NewPeer;
    use super::super::tests::{TestStore, test_store};
    use super::*;

    const ORIGIN: &str = "aaaaaaaaaaaaaaaa";

    async fn paired(test: &str) -> (TestStore, String) {
        let store = test_store(test).await;
        let peer = store
            .add_peer(
                NewPeer {
                    server_id: ORIGIN,
                    endpoint_id: &"0a".repeat(32),
                    display_name: "Alice's server",
                    addrs: &[],
                    relays: &[],
                },
                "bob",
            )
            .await
            .unwrap();
        (store, peer.id)
    }

    fn film(key: &str, title: &str) -> RemoteItem {
        RemoteItem {
            id: format!("plex:movie:{key}"),
            kind: RemoteKind::Movie,
            title: title.into(),
            parent: None,
            grandparent: None,
            parent_title: None,
            grandparent_title: None,
            index: None,
            parent_index: None,
            year: None,
            date: None,
            summary: None,
            content_rating: None,
            duration_ms: None,
            count: None,
            genres: Vec::new(),
            artwork: vec![RemoteArtwork {
                id: format!("plex:/library/metadata/{key}/thumb/9"),
                kind: RemoteArtworkKind::Poster,
            }],
            external_ids: vec![format!("tmdb://{key}")],
            hash: String::new(),
        }
        .hashed()
    }

    async fn movies(store: &Store, peer: &str) {
        store
            .cache_library(
                ORIGIN,
                peer,
                &SharedLibrary {
                    id: "plex:1".into(),
                    title: "Movies".into(),
                    kind: RemoteLibraryKind::Movies,
                },
                "movies",
            )
            .await
            .unwrap();
    }

    /// One whole sync of Movies under `cursor`, staged and written.
    async fn synced(
        store: &Store,
        peer: &str,
        cursor: &str,
        items: &[RemoteItem],
        removed: &[String],
        snapshot: bool,
    ) {
        store.begin_cache_sync(ORIGIN, "plex:1").await.unwrap();
        store
            .stage_cache_page(ORIGIN, "plex:1", items, removed)
            .await
            .unwrap();
        match store
            .finish_cache_sync(ORIGIN, peer, "plex:1", cursor, snapshot, u64::MAX)
            .await
            .unwrap()
        {
            CacheSynced::Written => {}
            over => panic!("{over:?}"),
        }
    }

    fn titles(items: &[RemoteItem]) -> Vec<&str> {
        items.iter().map(|i| i.title.as_str()).collect()
    }

    /// A whole library's pages, then its end: the items listed by title,
    /// those the snapshot did not name gone; the posters with no file yet
    /// are what is fetched.
    #[tokio::test]
    async fn a_snapshot_replaces_what_the_library_held() {
        let (store, peer) = paired("fed-cache-snapshot").await;
        movies(&store, &peer).await;
        synced(
            &store,
            &peer,
            "c1",
            &[film("2", "Brazil"), film("1", "Alien")],
            &[],
            true,
        )
        .await;
        assert_eq!(store.missing_posters(ORIGIN, 10).await.unwrap().len(), 2);
        let (page, total) = store
            .cached_page(ORIGIN, "plex:1", "movie", 0, 10)
            .await
            .unwrap();
        assert_eq!((titles(&page), total), (vec!["Alien", "Brazil"], 2));
        let library = store
            .cached_library(ORIGIN, "plex:1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(library.cursor.as_deref(), Some("c1"));

        // The next whole library has Alien as it was, and no Brazil.
        synced(&store, &peer, "c2", &[film("1", "Alien")], &[], true).await;
        assert_eq!(
            store.missing_posters(ORIGIN, 10).await.unwrap(),
            ["plex:/library/metadata/1/thumb/9"]
        );
        let (page, _) = store
            .cached_page(ORIGIN, "plex:1", "movie", 0, 10)
            .await
            .unwrap();
        assert_eq!(titles(&page), ["Alien"]);
        assert_eq!(
            store
                .find_cached_external("movie", &["tmdb://1".into()])
                .await
                .unwrap(),
            Some((ORIGIN.to_owned(), "plex:movie:1".to_owned()))
        );
    }

    /// A sync cut short, or one that would take the origin past its
    /// limit, leaves the cache and the cursor as they were (H1).
    #[tokio::test]
    async fn a_sync_cut_short_or_past_the_limit_writes_nothing() {
        let (store, peer) = paired("fed-cache-abort").await;
        movies(&store, &peer).await;
        synced(&store, &peer, "c1", &[film("1", "Alien")], &[], true).await;
        // Cut short: staged, never finished.
        store.begin_cache_sync(ORIGIN, "plex:1").await.unwrap();
        store
            .stage_cache_page(ORIGIN, "plex:1", &[film("2", "Brazil")], &[])
            .await
            .unwrap();
        store.abort_cache_sync(ORIGIN, "plex:1").await.unwrap();
        holds_alien_under(&store, "c1").await;
        // Whole, but two items where the origin may hold one.
        store.begin_cache_sync(ORIGIN, "plex:1").await.unwrap();
        store
            .stage_cache_page(ORIGIN, "plex:1", &[film("2", "Brazil")], &[])
            .await
            .unwrap();
        assert_eq!(
            store
                .finish_cache_sync(ORIGIN, &peer, "plex:1", "c2", false, 1)
                .await
                .unwrap(),
            CacheSynced::OverLimit { items: 2 }
        );
        holds_alien_under(&store, "c1").await;
        // Nothing staged is left for the next sync.
        synced(&store, &peer, "c3", &[], &[], false).await;
        holds_alien_under(&store, "c3").await;
    }

    /// Movies holds Alien alone, under `cursor`.
    async fn holds_alien_under(store: &Store, cursor: &str) {
        let (page, _) = store
            .cached_page(ORIGIN, "plex:1", "movie", 0, 10)
            .await
            .unwrap();
        assert_eq!(titles(&page), ["Alien"]);
        let library = store.cached_library(ORIGIN, "plex:1").await.unwrap();
        assert_eq!(library.unwrap().cursor.as_deref(), Some(cursor));
    }

    /// A held item, kept unavailable after its pair went, stays so when
    /// another server pairs later under the same server id, and comes
    /// back when the same server (its key) pairs again (M2).
    #[tokio::test]
    async fn a_held_item_comes_back_only_for_the_key_that_cached_it() {
        let (store, first) = paired("fed-cache-takeover").await;
        movies(&store, &first).await;
        synced(&store, &first, "c1", &[film("1", "Alien")], &[], true).await;
        store.revoke_peer(&first).await.unwrap().unwrap();
        let keep = HashSet::from(["plex:movie:1".to_owned()]);
        store.drop_cached(ORIGIN, None, &keep).await.unwrap();
        let pair_again = |endpoint: &'static str| {
            let store = store.store.clone();
            async move {
                let peer = store
                    .add_peer(
                        NewPeer {
                            server_id: ORIGIN,
                            endpoint_id: endpoint,
                            display_name: "Alice's server",
                            addrs: &[],
                            relays: &[],
                        },
                        "bob",
                    )
                    .await
                    .unwrap()
                    .id;
                movies(&store, &peer).await;
                synced(&store, &peer, "c2", &[film("1", "Alien")], &[], true).await;
                let kept = store.cached_item(ORIGIN, "plex:movie:1").await.unwrap();
                (peer, kept.unwrap().unavailable)
            }
        };
        let impostor = "0c".repeat(32).leak();
        let (impostor, unavailable) = pair_again(impostor).await;
        assert!(unavailable, "another key does not take it over");
        store.revoke_peer(&impostor).await.unwrap().unwrap();
        let (_, unavailable) = pair_again("0a".repeat(32).leak()).await;
        assert!(!unavailable, "the key that cached it");
    }

    /// An item an answer names twice is as it was named last.
    #[tokio::test]
    async fn an_item_named_twice_is_as_named_last() {
        let (store, peer) = paired("fed-cache-twice").await;
        movies(&store, &peer).await;
        store.begin_cache_sync(ORIGIN, "plex:1").await.unwrap();
        store
            .stage_cache_page(ORIGIN, "plex:1", &[film("1", "Alien")], &[])
            .await
            .unwrap();
        store
            .stage_cache_page(ORIGIN, "plex:1", &[], &["plex:movie:1".into()])
            .await
            .unwrap();
        store
            .finish_cache_sync(ORIGIN, &peer, "plex:1", "c1", false, u64::MAX)
            .await
            .unwrap();
        assert_eq!(
            store.cached_item(ORIGIN, "plex:movie:1").await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn changes_add_and_remove_and_leave_the_rest() {
        let (store, peer) = paired("fed-cache-changes").await;
        movies(&store, &peer).await;
        synced(&store, &peer, "c1", &[film("1", "Alien")], &[], false).await;
        synced(
            &store,
            &peer,
            "c2",
            &[film("2", "Brazil")],
            &["plex:movie:1".into()],
            false,
        )
        .await;
        let (page, _) = store
            .cached_page(ORIGIN, "plex:1", "movie", 0, 10)
            .await
            .unwrap();
        assert_eq!(titles(&page), ["Brazil"]);
    }

    /// A dropped library keeps what a user holds, unavailable and with no
    /// artwork, and nothing else.
    #[tokio::test]
    async fn a_dropped_library_keeps_only_what_a_user_holds() {
        let (store, peer) = paired("fed-cache-drop").await;
        movies(&store, &peer).await;
        synced(
            &store,
            &peer,
            "c1",
            &[film("1", "Alien"), film("2", "Brazil")],
            &[],
            true,
        )
        .await;
        let poster = "plex:/library/metadata/1/thumb/9";
        assert!(store.cached_artwork_named(ORIGIN, poster).await.unwrap());
        store
            .keep_cached_file(
                ORIGIN,
                poster,
                &CachedFile {
                    file: "f1".into(),
                    content_type: "image/png".into(),
                },
                10,
                ROOMY,
            )
            .await
            .unwrap();
        let keep = HashSet::from(["plex:movie:1".to_owned()]);
        store.drop_cached(ORIGIN, None, &keep).await.unwrap();
        assert!(
            store
                .cached_libraries(Some(ORIGIN))
                .await
                .unwrap()
                .is_empty()
        );
        let kept = store
            .cached_item(ORIGIN, "plex:movie:1")
            .await
            .unwrap()
            .unwrap();
        assert!(kept.unavailable);
        assert_eq!(
            store.cached_item(ORIGIN, "plex:movie:2").await.unwrap(),
            None
        );
        assert!(!store.cached_artwork_named(ORIGIN, poster).await.unwrap());
        assert_eq!(
            store.forget_cached_files(ORIGIN, false).await.unwrap(),
            ["f1"]
        );
        assert_eq!(
            store
                .find_cached_external("movie", &["tmdb://1".into()])
                .await
                .unwrap(),
            None,
            "an unavailable item is not found"
        );
    }

    const ROOMY: FileLimits = FileLimits {
        per_origin: 100,
        total: 1_000,
    };

    fn png(name: &str) -> CachedFile {
        CachedFile {
            file: name.into(),
            content_type: "image/png".into(),
        }
    }

    /// Past an origin's limit, its least recently used files go first,
    /// and only as many as it takes.
    #[tokio::test]
    async fn files_past_the_cap_go_least_recently_used_first() {
        let (store, _) = paired("fed-cache-lru").await;
        for name in ["a", "b"] {
            store
                .keep_cached_file(ORIGIN, name, &png(name), 40, ROOMY)
                .await
                .unwrap();
            store.clock().advance(std::time::Duration::from_secs(1));
        }
        // a is used again: b is the least recent now.
        assert!(store.cached_file(ORIGIN, "a").await.unwrap().is_some());
        store.clock().advance(std::time::Duration::from_secs(1));
        let gone = store
            .keep_cached_file(ORIGIN, "c", &png("c"), 40, ROOMY)
            .await
            .unwrap();
        assert_eq!(gone, [(ORIGIN.to_owned(), "b".to_owned())]);
        assert!(store.cached_file(ORIGIN, "b").await.unwrap().is_none());
        assert!(store.cached_file(ORIGIN, "a").await.unwrap().is_some());
    }

    /// Every origin's files together are held to the total, the least
    /// recently used of any going first; a lower limit takes effect at
    /// the next eviction, with no file kept (M4, M5).
    #[tokio::test]
    async fn files_past_the_total_go_whichever_origin_they_are_of() {
        let (store, _) = paired("fed-cache-total").await;
        let other = "bbbbbbbbbbbbbbbb";
        let limits = FileLimits {
            per_origin: 100,
            total: 100,
        };
        for (origin, name) in [(ORIGIN, "a"), (other, "b"), (ORIGIN, "c")] {
            store
                .keep_cached_file(origin, name, &png(name), 40, limits)
                .await
                .map(|gone| {
                    if name == "c" {
                        assert_eq!(gone, [(ORIGIN.to_owned(), "a".to_owned())]);
                    }
                })
                .unwrap();
            store.clock().advance(std::time::Duration::from_secs(1));
        }
        let lower = FileLimits {
            per_origin: 30,
            total: 100,
        };
        let mut gone = store.evict_cached_files(lower).await.unwrap();
        gone.sort();
        assert_eq!(
            gone,
            [
                (ORIGIN.to_owned(), "c".to_owned()),
                (other.to_owned(), "b".to_owned())
            ]
        );
        assert!(store.evict_cached_files(lower).await.unwrap().is_empty());
    }

    /// A title is found by any part of it, in any case, the ones it
    /// starts with first; by year and library too. A `%` is matched as
    /// itself.
    #[tokio::test]
    async fn the_cache_is_searched_by_title_year_and_library() {
        let (store, peer) = paired("fed-cache-search").await;
        movies(&store, &peer).await;
        let dated = |key: &str, title: &str, year: u16| {
            RemoteItem {
                year: Some(year),
                ..film(key, title)
            }
            .hashed()
        };
        synced(
            &store,
            &peer,
            "c1",
            &[
                dated("1", "Alien", 1979),
                dated("2", "Aliens", 1986),
                dated("3", "The Alienist", 2018),
                dated("4", "100% Wolf", 2020),
            ],
            &[],
            true,
        )
        .await;
        let titles_of = |found: &[(String, RemoteItem)]| -> Vec<String> {
            found.iter().map(|(_, i)| i.title.clone()).collect()
        };
        let (found, total) = store
            .search_cached("movie", "ALIEN", None, None, 2)
            .await
            .unwrap();
        assert_eq!(titles_of(&found), ["Alien", "Aliens"]);
        assert_eq!(total, 3);
        let (found, _) = store
            .search_cached("movie", "alien", Some(2018), None, 10)
            .await
            .unwrap();
        assert_eq!(titles_of(&found), ["The Alienist"]);
        let (found, _) = store
            .search_cached("movie", "alien", None, Some((ORIGIN, "plex:9")), 10)
            .await
            .unwrap();
        assert!(found.is_empty(), "another library");
        let (found, _) = store
            .search_cached("movie", "0%", None, Some((ORIGIN, "plex:1")), 10)
            .await
            .unwrap();
        assert_eq!(titles_of(&found), ["100% Wolf"]);
        let (found, _) = store
            .search_cached("show", "alien", None, None, 10)
            .await
            .unwrap();
        assert!(found.is_empty(), "another kind");
    }

    /// The items cached last come first; an item synced again keeps when
    /// it first came.
    #[tokio::test]
    async fn recently_cached_goes_by_when_each_item_first_came() {
        let (store, peer) = paired("fed-cache-recent").await;
        movies(&store, &peer).await;
        synced(&store, &peer, "c1", &[film("1", "Alien")], &[], true).await;
        store.clock().advance(std::time::Duration::from_secs(5));
        synced(
            &store,
            &peer,
            "c2",
            &[film("2", "Brazil"), film("1", "Alien (1979)")],
            &[],
            true,
        )
        .await;
        let recent = store.recent_cached(None, 10).await.unwrap();
        let titles: Vec<&str> = recent.iter().map(|(_, i)| i.title.as_str()).collect();
        assert_eq!(titles, ["Brazil", "Alien (1979)"]);
        assert!(
            store
                .recent_cached(Some("bbbbbbbbbbbbbbbb"), 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_server_s_home_row_is_turned_off_and_on_per_user() {
        let store = test_store("fed-cache-rows").await;
        store
            .set_server_row("local:a", ORIGIN, false)
            .await
            .unwrap();
        store
            .set_server_row("local:a", ORIGIN, false)
            .await
            .unwrap();
        assert_eq!(
            store.hidden_server_rows("local:a").await.unwrap(),
            HashSet::from([ORIGIN.to_owned()])
        );
        assert!(
            store
                .hidden_server_rows("local:b")
                .await
                .unwrap()
                .is_empty()
        );
        store.set_server_row("local:a", ORIGIN, true).await.unwrap();
        assert!(
            store
                .hidden_server_rows("local:a")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn the_ids_a_user_holds_are_found_in_each_table() {
        let store = test_store("fed-cache-held").await;
        let user = store
            .add_user(
                "bob",
                None,
                "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA",
                spoolframe_types::Role::Consumer,
            )
            .await
            .unwrap();
        let principal = crate::principal::Principal::LocalUser(user);
        let id = "fed:movie:aaaaaaaaaaaaaaaa:plex:1";
        store
            .save_watch(
                principal,
                crate::store::watch::WatchUpdate {
                    media_id: id,
                    show_id: None,
                    external_ids: &[],
                    position_ms: Some(60_000),
                    duration_ms: None,
                    watched: false,
                    unhide: true,
                },
            )
            .await
            .unwrap();
        assert_eq!(store.held_federated_ids().await.unwrap(), [id]);
    }
}
