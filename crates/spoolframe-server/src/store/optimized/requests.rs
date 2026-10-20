//! On-demand requests (PX-37, the user's decision 2026-10-20): an admin
//! asks for an item, a season, a show or a library to be optimized in a
//! profile. A request is a kept rule: its films and episodes are listed
//! as its items, each with the copy made for it, and the items its scope
//! gains later are added. A pinned request's copies are never evicted.
//! Expanding a scope and asking the pool is the pool's
//! ([`crate::optimized::requests`]); this module only keeps the records.

use super::super::{Store, StoreError};
use super::decode_error;

/// What a request covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScopeKind {
    /// One film or episode.
    Item,
    /// A season's episodes.
    Season,
    /// A show's episodes, specials too.
    Show,
    /// A film library's films, or a show library's episodes.
    Library,
}

impl ScopeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Item => "item",
            Self::Season => "season",
            Self::Show => "show",
            Self::Library => "library",
        }
    }

    fn parse(s: &str) -> Result<Self, sqlx::Error> {
        Ok(match s {
            "item" => Self::Item,
            "season" => Self::Season,
            "show" => Self::Show,
            "library" => Self::Library,
            other => return Err(decode_error("scope_kind", other)),
        })
    }
}

/// A request's row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestRow {
    /// The server's own.
    pub id: String,
    pub scope_kind: ScopeKind,
    /// A media id, or a library id for a library.
    pub scope_id: String,
    /// A profile's name.
    pub profile: String,
    pub pinned: bool,
    /// What its copies are expected to take, from its items' lengths and
    /// its profile's bitrate, as it was last asked for or pinned.
    pub estimated_bytes: u64,
    /// The admin's user id.
    pub created_by: String,
    pub created_at: i64,
}

/// A request with how far it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestProgress {
    pub row: RequestRow,
    /// The films and episodes it covers.
    pub items: u64,
    /// Those whose copy is made.
    pub done: u64,
    /// Those whose copy could not be asked for or made.
    pub failed: u64,
    /// The made copies' tracks together.
    pub size_bytes: u64,
}

/// An item the pool has not been asked for yet: its request's profile
/// with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnaskedItem {
    pub request_id: String,
    pub media_id: String,
    pub profile: String,
    /// Its round when listed: an ask is counted only in the same round.
    pub round: i64,
}

/// A new request, or one for the same scope and profile again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRequest<'a> {
    pub scope_kind: ScopeKind,
    pub scope_id: &'a str,
    pub profile: &'a str,
    pub pinned: bool,
    pub estimated_bytes: u64,
    pub created_by: &'a str,
}

/// A request's columns, for [`request_row`].
macro_rules! request_columns {
    () => {
        "id, scope_kind, scope_id, profile, pinned, estimated_bytes, created_by, created_at"
    };
}

fn request_row(r: &sqlx::sqlite::SqliteRow) -> Result<RequestRow, sqlx::Error> {
    use sqlx::Row;
    Ok(RequestRow {
        id: r.try_get("id")?,
        scope_kind: ScopeKind::parse(&r.try_get::<String, _>("scope_kind")?)?,
        scope_id: r.try_get("scope_id")?,
        profile: r.try_get("profile")?,
        pinned: r.try_get("pinned")?,
        estimated_bytes: super::unsigned("estimated_bytes", r.try_get("estimated_bytes")?)?,
        created_by: r.try_get("created_by")?,
        created_at: r.try_get("created_at")?,
    })
}

impl Store {
    /// The request for `new`'s scope and profile, with its pin and
    /// estimate as `new` says (the first asker stays its creator), or a
    /// new one; then `media_ids`, its items now, added to those it has.
    /// The bool is whether the request is new.
    pub async fn put_optimized_request(
        &self,
        new: &NewRequest<'_>,
        media_ids: &[String],
    ) -> Result<(RequestRow, bool), StoreError> {
        let mut tx = self.begin_write().await?;
        let id: Option<String> = sqlx::query_scalar(
            "SELECT id FROM optimized_requests \
             WHERE scope_kind = ?1 AND scope_id = ?2 AND profile = ?3",
        )
        .bind(new.scope_kind.as_str())
        .bind(new.scope_id)
        .bind(new.profile)
        .fetch_optional(&mut *tx)
        .await?;
        let created = id.is_none();
        let id = match id {
            Some(id) => {
                sqlx::query(
                    "UPDATE optimized_requests SET pinned = ?2, estimated_bytes = ?3 \
                     WHERE id = ?1",
                )
                .bind(&id)
                .bind(new.pinned)
                .bind(super::signed(new.estimated_bytes))
                .execute(&mut *tx)
                .await?;
                id
            }
            None => {
                let id = uuid::Uuid::new_v4().simple().to_string();
                sqlx::query(
                    "INSERT INTO optimized_requests (id, scope_kind, scope_id, profile, pinned, \
                     estimated_bytes, created_by, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                )
                .bind(&id)
                .bind(new.scope_kind.as_str())
                .bind(new.scope_id)
                .bind(new.profile)
                .bind(new.pinned)
                .bind(super::signed(new.estimated_bytes))
                .bind(new.created_by)
                .bind(self.now())
                .execute(&mut *tx)
                .await?;
                id
            }
        };
        add_items(&mut tx, &id, media_ids).await?;
        tx.commit().await?;
        let row = self
            .optimized_request(&id)
            .await?
            .ok_or(StoreError::Db(sqlx::Error::RowNotFound))?;
        Ok((row, created))
    }

    /// Request `id`, if there is one.
    pub async fn optimized_request(&self, id: &str) -> Result<Option<RequestRow>, StoreError> {
        let found = sqlx::query(concat!(
            "SELECT ",
            request_columns!(),
            " FROM optimized_requests WHERE id = ?1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(found.as_ref().map(request_row).transpose()?)
    }

    /// The request for `scope_id` of `kind` in `profile`, if there is one.
    pub async fn optimized_request_for(
        &self,
        kind: ScopeKind,
        scope_id: &str,
        profile: &str,
    ) -> Result<Option<RequestRow>, StoreError> {
        let found = sqlx::query(concat!(
            "SELECT ",
            request_columns!(),
            " FROM optimized_requests \
             WHERE scope_kind = ?1 AND scope_id = ?2 AND profile = ?3"
        ))
        .bind(kind.as_str())
        .bind(scope_id)
        .bind(profile)
        .fetch_optional(&self.pool)
        .await?;
        Ok(found.as_ref().map(request_row).transpose()?)
    }

    /// Every request, newest first.
    pub async fn optimized_requests(&self) -> Result<Vec<RequestRow>, StoreError> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            request_columns!(),
            " FROM optimized_requests ORDER BY created_at DESC, id"
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(request_row).collect::<Result<_, _>>()?)
    }

    /// Every request with how far it is, newest first.
    pub async fn optimized_request_progress(&self) -> Result<Vec<RequestProgress>, StoreError> {
        use sqlx::Row;
        let counts = sqlx::query(
            "SELECT i.request_id, COUNT(*) AS items, \
             SUM(c.id IS NOT NULL AND c.outdated = 0 AND c.failure IS NULL \
               AND NOT EXISTS (SELECT 1 FROM optimized_copy_tracks m \
                 JOIN optimized_tracks t ON t.id = m.track_id \
                 WHERE m.copy_id = c.id AND t.state != 'ready')) AS done, \
             SUM(i.failure IS NOT NULL OR c.failure IS NOT NULL \
               OR EXISTS (SELECT 1 FROM optimized_copy_tracks m \
                 JOIN optimized_tracks t ON t.id = m.track_id \
                 WHERE m.copy_id = c.id AND t.state = 'failed')) AS failed, \
             COALESCE(SUM((SELECT SUM(t.size_bytes) FROM optimized_copy_tracks m \
               JOIN optimized_tracks t ON t.id = m.track_id \
               WHERE m.copy_id = c.id AND t.state = 'ready')), 0) AS size_bytes \
             FROM optimized_request_items i \
             LEFT JOIN optimized_copies c ON c.id = i.copy_id \
             GROUP BY i.request_id",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut of = std::collections::HashMap::new();
        for r in &counts {
            let id: String = r.try_get("request_id")?;
            let count = |column: &str| -> Result<u64, sqlx::Error> {
                super::unsigned(column, r.try_get::<i64, _>(column)?)
            };
            of.insert(
                id,
                (
                    count("items")?,
                    count("done")?,
                    count("failed")?,
                    count("size_bytes")?,
                ),
            );
        }
        Ok(self
            .optimized_requests()
            .await?
            .into_iter()
            .map(|row| {
                let (items, done, failed, size_bytes) =
                    of.get(&row.id).copied().unwrap_or_default();
                RequestProgress {
                    row,
                    items,
                    done,
                    failed,
                    size_bytes,
                }
            })
            .collect())
    }

    /// The requests in profile `profile`.
    pub async fn optimized_requests_in(
        &self,
        profile: &str,
    ) -> Result<Vec<RequestRow>, StoreError> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            request_columns!(),
            " FROM optimized_requests WHERE profile = ?1 ORDER BY created_at, id"
        ))
        .bind(profile)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(request_row).collect::<Result<_, _>>()?)
    }

    /// What the pinned requests but `except` are expected to take.
    pub async fn pinned_optimized_bytes(&self, except: Option<&str>) -> Result<u64, StoreError> {
        let sum: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(estimated_bytes), 0) FROM optimized_requests \
             WHERE pinned = 1 AND id IS NOT ?1",
        )
        .bind(except)
        .fetch_one(&self.pool)
        .await?;
        Ok(super::unsigned("estimated_bytes", sum)?)
    }

    /// Pin or unpin request `id`: whether it was there.
    pub async fn set_optimized_request_pinned(
        &self,
        id: &str,
        pinned: bool,
    ) -> Result<bool, StoreError> {
        let done = sqlx::query("UPDATE optimized_requests SET pinned = ?2 WHERE id = ?1")
            .bind(id)
            .bind(pinned)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Forget request `id` and its items: its copies stay, no longer
    /// pinned by it. Whether it was there.
    pub async fn delete_optimized_request(&self, id: &str) -> Result<bool, StoreError> {
        let mut tx = self.begin_write().await?;
        sqlx::query("DELETE FROM optimized_request_items WHERE request_id = ?1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        let gone = sqlx::query("DELETE FROM optimized_requests WHERE id = ?1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(gone.rows_affected() > 0)
    }

    /// Add `media_ids` to request `id`'s items, those it has kept as
    /// they are: how many are new.
    pub async fn add_optimized_request_items(
        &self,
        id: &str,
        media_ids: &[String],
    ) -> Result<u64, StoreError> {
        let mut tx = self.begin_write().await?;
        let added = add_items(&mut tx, id, media_ids).await?;
        tx.commit().await?;
        Ok(added)
    }

    /// Request `id`'s item `media_id`: whether it was asked for, and the
    /// copy made for it.
    #[cfg(test)]
    pub async fn optimized_request_item(
        &self,
        id: &str,
        media_id: &str,
    ) -> Result<Option<(bool, Option<String>)>, StoreError> {
        Ok(sqlx::query_as(
            "SELECT asked, copy_id FROM optimized_request_items \
             WHERE request_id = ?1 AND media_id = ?2",
        )
        .bind(id)
        .bind(media_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Request `id`'s items, by media id.
    pub async fn optimized_request_items(&self, id: &str) -> Result<Vec<String>, StoreError> {
        Ok(sqlx::query_scalar(
            "SELECT media_id FROM optimized_request_items WHERE request_id = ?1 ORDER BY media_id",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// The items the pool has not been asked for, oldest request first.
    pub async fn unasked_optimized_request_items(&self) -> Result<Vec<UnaskedItem>, StoreError> {
        let rows: Vec<(String, String, String, i64)> = sqlx::query_as(
            "SELECT i.request_id, i.media_id, r.profile, i.round FROM optimized_request_items i \
             JOIN optimized_requests r ON r.id = i.request_id \
             WHERE i.asked = 0 ORDER BY r.created_at, r.id, i.media_id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(request_id, media_id, profile, round)| UnaskedItem {
                request_id,
                media_id,
                profile,
                round,
            })
            .collect())
    }

    /// `item` was asked for: made into `copy`, or not asked for, for
    /// `failure`. Not counted if its profile changed since it was listed.
    pub async fn optimized_request_item_asked(
        &self,
        item: &UnaskedItem,
        copy: Option<&str>,
        failure: Option<&str>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE optimized_request_items SET asked = 1, copy_id = ?3, failure = ?4 \
             WHERE request_id = ?1 AND media_id = ?2 AND round = ?5",
        )
        .bind(&item.request_id)
        .bind(&item.media_id)
        .bind(copy)
        .bind(failure)
        .bind(item.round)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Every item of request `id` to be asked for again: its profile
    /// changed. How many.
    pub async fn requeue_optimized_request_items(&self, id: &str) -> Result<u64, StoreError> {
        let done = sqlx::query(
            "UPDATE optimized_request_items SET asked = 0, failure = NULL, round = round + 1 \
             WHERE request_id = ?1",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected())
    }
}

/// Add `media_ids` to request `id`'s items: how many are new.
async fn add_items(
    tx: &mut sqlx::Transaction<'static, sqlx::Sqlite>,
    id: &str,
    media_ids: &[String],
) -> Result<u64, sqlx::Error> {
    let mut added = 0;
    for media_id in media_ids {
        added += sqlx::query(
            "INSERT OR IGNORE INTO optimized_request_items (request_id, media_id) VALUES (?1, ?2)",
        )
        .bind(id)
        .bind(media_id)
        .execute(&mut **tx)
        .await?
        .rows_affected();
    }
    Ok(added)
}

/// Copy `id` is going: the items made into it have none now.
pub(super) async fn forget_copy(
    tx: &mut sqlx::Transaction<'static, sqlx::Sqlite>,
    id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE optimized_request_items SET copy_id = NULL WHERE copy_id = ?1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Profile `name` is gone: so are its requests.
pub(super) async fn delete_profile_requests(
    tx: &mut sqlx::Transaction<'static, sqlx::Sqlite>,
    name: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "DELETE FROM optimized_request_items WHERE request_id IN \
         (SELECT id FROM optimized_requests WHERE profile = ?1)",
    )
    .bind(name)
    .execute(&mut **tx)
    .await?;
    sqlx::query("DELETE FROM optimized_requests WHERE profile = ?1")
        .bind(name)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::super::tests::test_store;
    use super::*;

    fn season(pinned: bool) -> NewRequest<'static> {
        NewRequest {
            scope_kind: ScopeKind::Season,
            scope_id: "plex:season:11",
            profile: "Mobile",
            pinned,
            estimated_bytes: 100,
            created_by: "admin-1",
        }
    }

    fn ids(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| (*s).to_owned()).collect()
    }

    #[tokio::test]
    async fn the_same_scope_and_profile_is_the_same_request_with_its_new_pin() {
        let store = test_store("optimized-request-same").await;
        let (first, created) = store
            .put_optimized_request(&season(false), &ids(&["plex:episode:12"]))
            .await
            .unwrap();
        assert!(created);
        let (again, created) = store
            .put_optimized_request(&season(true), &ids(&["plex:episode:13"]))
            .await
            .unwrap();
        assert_eq!(
            (created, again.id == first.id, again.pinned),
            (false, true, true)
        );
        assert_eq!(
            store.optimized_request_items(&first.id).await.unwrap(),
            ["plex:episode:12", "plex:episode:13"]
        );
    }

    #[tokio::test]
    async fn an_asked_item_is_not_asked_again_until_requeued() {
        let store = test_store("optimized-request-asked").await;
        let (row, _) = store
            .put_optimized_request(&season(false), &ids(&["plex:episode:12"]))
            .await
            .unwrap();
        let listed = store.unasked_optimized_request_items().await.unwrap();
        store
            .optimized_request_item_asked(&listed[0], None, None)
            .await
            .unwrap();
        assert!(
            store
                .unasked_optimized_request_items()
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .requeue_optimized_request_items(&row.id)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            store.unasked_optimized_request_items().await.unwrap(),
            [UnaskedItem {
                request_id: row.id,
                media_id: "plex:episode:12".into(),
                profile: "Mobile".into(),
                round: 1,
            }]
        );
    }

    /// An ask begun before its profile changed is not counted: the item
    /// is asked for again, with the new video.
    #[tokio::test]
    async fn an_ask_from_before_a_requeue_is_not_counted() {
        let store = test_store("optimized-request-round").await;
        let (row, _) = store
            .put_optimized_request(&season(false), &ids(&["plex:episode:12"]))
            .await
            .unwrap();
        let listed = store.unasked_optimized_request_items().await.unwrap();
        store
            .requeue_optimized_request_items(&row.id)
            .await
            .unwrap();
        store
            .optimized_request_item_asked(&listed[0], Some("old-copy"), None)
            .await
            .unwrap();
        assert_eq!(
            store.unasked_optimized_request_items().await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn pinned_bytes_count_only_pinned_requests_but_the_one_named() {
        let store = test_store("optimized-request-pinned-bytes").await;
        let (pinned, _) = store
            .put_optimized_request(&season(true), &[])
            .await
            .unwrap();
        store
            .put_optimized_request(
                &NewRequest {
                    scope_id: "plex:season:14",
                    ..season(false)
                },
                &[],
            )
            .await
            .unwrap();
        assert_eq!(
            (
                store.pinned_optimized_bytes(None).await.unwrap(),
                store
                    .pinned_optimized_bytes(Some(&pinned.id))
                    .await
                    .unwrap()
            ),
            (100, 0)
        );
    }

    #[tokio::test]
    async fn deleting_a_profile_deletes_its_requests() {
        let store = test_store("optimized-request-profile-gone").await;
        let (row, _) = store
            .put_optimized_request(&season(true), &ids(&["plex:episode:12"]))
            .await
            .unwrap();
        store.delete_optimized_profile("Mobile").await.unwrap();
        assert_eq!(store.optimized_request(&row.id).await.unwrap(), None);
    }
}
