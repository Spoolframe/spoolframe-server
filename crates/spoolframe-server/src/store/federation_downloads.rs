//! Downloads of a paired server's items (P8-07), on the server the user is
//! at home on (SB): its record of each, by its own id, beside the paired
//! server's (SA's) id of it. SA makes and keeps the file; this keeps what
//! SA last said of it, so the device asks SB alone.

use super::{Store, StoreError};

/// A download of a paired server's item, as SB keeps it.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct RemoteDownloadRow {
    /// SB's id: what the device names.
    pub id: String,
    pub principal: String,
    /// The paired server's ServerId.
    pub origin: String,
    /// The library the item is in, by SA's own id.
    pub library: String,
    /// The item's `fed:` id.
    pub item: String,
    /// SA's id of the download.
    pub remote_id: String,
    /// The wire's `DownloadState`, as JSON.
    pub state: String,
    /// The wire's `DownloadFile`, as JSON.
    pub file: Option<String>,
    pub expected_size: Option<i64>,
    pub unavailable_at: Option<i64>,
}

/// A download to keep, as SA answered its preparation.
#[derive(Debug, Clone, Copy)]
pub struct NewRemoteDownload<'a> {
    pub principal: &'a str,
    pub origin: &'a str,
    pub library: &'a str,
    pub item: &'a str,
    pub remote_id: &'a str,
    pub state: &'a str,
    pub file: Option<&'a str>,
    pub expected_size: Option<u64>,
}

macro_rules! columns {
    () => {
        "id, principal, origin, library, item, remote_id, state, file, expected_size, \
         unavailable_at"
    };
}

fn size(n: Option<u64>) -> Option<i64> {
    n.map(|n| i64::try_from(n).unwrap_or(i64::MAX))
}

impl Store {
    /// Keep `new`: the user's same download of SA's (by SA's id) keeps its
    /// id here, and is available again.
    pub async fn put_remote_download(
        &self,
        new: NewRemoteDownload<'_>,
    ) -> Result<RemoteDownloadRow, StoreError> {
        let id = uuid::Uuid::new_v4().simple().to_string();
        Ok(sqlx::query_as(concat!(
            "INSERT INTO federation_downloads \
                 (id, principal, origin, library, item, remote_id, state, file, expected_size, \
                  created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10) \
             ON CONFLICT (principal, remote_id) DO UPDATE SET \
                 origin = excluded.origin, library = excluded.library, item = excluded.item, \
                 state = excluded.state, file = COALESCE(excluded.file, file), \
                 expected_size = COALESCE(excluded.expected_size, expected_size), \
                 unavailable_at = NULL, updated_at = excluded.updated_at \
             RETURNING ",
            columns!()
        ))
        .bind(&id)
        .bind(new.principal)
        .bind(new.origin)
        .bind(new.library)
        .bind(new.item)
        .bind(new.remote_id)
        .bind(new.state)
        .bind(new.file)
        .bind(size(new.expected_size))
        .bind(self.now())
        .fetch_one(&self.pool)
        .await?)
    }

    /// `principal`'s downloads of paired servers' items among `ids`.
    pub async fn remote_downloads(
        &self,
        principal: &str,
        ids: &[String],
    ) -> Result<Vec<RemoteDownloadRow>, StoreError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let ids = serde_json::to_string(ids).expect("ids are JSON");
        Ok(sqlx::query_as(concat!(
            "SELECT ",
            columns!(),
            " FROM federation_downloads \
             WHERE principal = ?1 AND id IN (SELECT value FROM json_each(?2))"
        ))
        .bind(principal)
        .bind(ids)
        .fetch_all(&self.pool)
        .await?)
    }

    /// `principal`'s download `id` of a paired server's item.
    pub async fn remote_download(
        &self,
        principal: &str,
        id: &str,
    ) -> Result<Option<RemoteDownloadRow>, StoreError> {
        Ok(sqlx::query_as(concat!(
            "SELECT ",
            columns!(),
            " FROM federation_downloads WHERE principal = ?1 AND id = ?2"
        ))
        .bind(principal)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// The downloads of `origin` whose SA id is `remote_id`, whoever's: what
    /// SA's `DownloadChanged` is told to.
    pub async fn remote_downloads_by_remote_id(
        &self,
        origin: &str,
        remote_id: &str,
    ) -> Result<Vec<RemoteDownloadRow>, StoreError> {
        Ok(sqlx::query_as(concat!(
            "SELECT ",
            columns!(),
            " FROM federation_downloads WHERE origin = ?1 AND remote_id = ?2"
        ))
        .bind(origin)
        .bind(remote_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Download `id`'s state and file as SA told them now, unless it is no
    /// longer available.
    pub async fn set_remote_download_state(
        &self,
        id: &str,
        state: &str,
        file: Option<&str>,
        expected_size: Option<u64>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE federation_downloads SET state = ?2, file = COALESCE(?3, file), \
                 expected_size = COALESCE(?4, expected_size), updated_at = ?5 \
             WHERE id = ?1 AND unavailable_at IS NULL",
        )
        .bind(id)
        .bind(state)
        .bind(file)
        .bind(size(expected_size))
        .bind(self.now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Delete `principal`'s download `id`: whether there was one.
    pub async fn delete_remote_download(
        &self,
        principal: &str,
        id: &str,
    ) -> Result<Option<RemoteDownloadRow>, StoreError> {
        Ok(sqlx::query_as(concat!(
            "DELETE FROM federation_downloads WHERE principal = ?1 AND id = ?2 RETURNING ",
            columns!()
        ))
        .bind(principal)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// `origin`'s downloads, of `library` alone if given, are no longer
    /// available: it stopped sharing it, or the pair ended. How many.
    pub async fn remote_downloads_unavailable(
        &self,
        origin: &str,
        library: Option<&str>,
    ) -> Result<u64, StoreError> {
        Ok(sqlx::query(
            "UPDATE federation_downloads SET unavailable_at = ?3 \
             WHERE origin = ?1 AND (?2 IS NULL OR library = ?2) AND unavailable_at IS NULL",
        )
        .bind(origin)
        .bind(library)
        .bind(self.now())
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// Forget the downloads that ended `kept` ago or more: no longer
    /// available, or failed, since. A made one stays until the device
    /// cancels it, as its file does there. How many.
    pub async fn prune_remote_downloads(
        &self,
        kept: std::time::Duration,
    ) -> Result<u64, StoreError> {
        let before = self.now() - i64::try_from(kept.as_secs()).unwrap_or(i64::MAX);
        Ok(sqlx::query(
            "DELETE FROM federation_downloads \
             WHERE unavailable_at < ?1 \
                OR (json_extract(state, '$.type') = 'failed' AND updated_at < ?1)",
        )
        .bind(before)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::test_store;
    use super::*;

    fn new<'a>(remote_id: &'a str, library: &'a str) -> NewRemoteDownload<'a> {
        NewRemoteDownload {
            principal: "local:00000000-0000-0000-0000-000000000007",
            origin: "mzxw6ytbojtg633c",
            library,
            item: "fed:movie:mzxw6ytbojtg633c:plex:1",
            remote_id,
            state: r#"{"type":"waiting"}"#,
            file: None,
            expected_size: Some(10),
        }
    }

    /// Downloads that ended a while ago are forgotten: no longer available,
    /// or failed. A made one, or one being made, stays.
    #[tokio::test]
    async fn ended_remote_downloads_are_forgotten_after_a_while() {
        let store = test_store("fed-downloads-prune").await;
        let made = store
            .put_remote_download(new("r1", "plex:1"))
            .await
            .unwrap();
        store
            .set_remote_download_state(
                &made.id,
                r#"{"type":"ready","size":1,"sha256":"00"}"#,
                None,
                None,
            )
            .await
            .unwrap();
        let making = store
            .put_remote_download(new("r2", "plex:1"))
            .await
            .unwrap();
        let failed = store
            .put_remote_download(new("r3", "plex:1"))
            .await
            .unwrap();
        store
            .set_remote_download_state(
                &failed.id,
                r#"{"type":"failed","reason":"no room","cause":"other"}"#,
                None,
                None,
            )
            .await
            .unwrap();
        let gone = store
            .put_remote_download(new("r4", "plex:2"))
            .await
            .unwrap();
        store
            .remote_downloads_unavailable(&gone.origin, Some("plex:2"))
            .await
            .unwrap();
        let kept = std::time::Duration::from_secs(30 * 86_400);
        assert_eq!(
            store.prune_remote_downloads(kept).await.unwrap(),
            0,
            "not yet"
        );
        store
            .clock()
            .advance(kept + std::time::Duration::from_secs(1));
        assert_eq!(store.prune_remote_downloads(kept).await.unwrap(), 2);
        let principal = &made.principal;
        assert!(
            store
                .remote_download(principal, &made.id)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .remote_download(principal, &making.id)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .remote_download(principal, &failed.id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .remote_download(principal, &gone.id)
                .await
                .unwrap()
                .is_none()
        );
    }

    /// The same download of SA's keeps its id here; another is another.
    /// A library no longer shared makes its downloads unavailable alone,
    /// and preparing one again makes it available.
    #[tokio::test]
    async fn a_remote_download_keeps_its_id_and_goes_unavailable_with_its_library() {
        let store = test_store("fed-downloads").await;
        let one = store
            .put_remote_download(new("r1", "plex:1"))
            .await
            .unwrap();
        let again = store
            .put_remote_download(new("r1", "plex:1"))
            .await
            .unwrap();
        assert_eq!(one.id, again.id);
        assert_eq!(again.expected_size, Some(10));
        let two = store
            .put_remote_download(new("r2", "plex:2"))
            .await
            .unwrap();
        assert_ne!(one.id, two.id);
        assert_eq!(
            store
                .remote_downloads_unavailable("mzxw6ytbojtg633c", Some("plex:1"))
                .await
                .unwrap(),
            1
        );
        let rows = store
            .remote_downloads(&one.principal, &[one.id.clone(), two.id.clone()])
            .await
            .unwrap();
        let of = |id: &str| rows.iter().find(|r| r.id == id).unwrap().unavailable_at;
        assert!(of(&one.id).is_some());
        assert!(of(&two.id).is_none());
        let back = store
            .put_remote_download(new("r1", "plex:1"))
            .await
            .unwrap();
        assert_eq!(
            (back.id.as_str(), back.unavailable_at),
            (one.id.as_str(), None)
        );
        assert!(
            store
                .delete_remote_download(&one.principal, &one.id)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .remote_download(&one.principal, &one.id)
                .await
                .unwrap()
                .is_none()
        );
    }
}
