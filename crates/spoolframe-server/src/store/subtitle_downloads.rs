//! Subtitles downloaded from a subtitle site (PX-02): storage. Fetching,
//! the files and who may do what are in `crate::subtitle_downloads`.

use sqlx::Row;

use super::{Store, StoreError};

/// One downloaded subtitle (migration 0036).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadRow {
    /// The server's own id: its file's name.
    pub id: String,
    pub media_id: String,
    /// The media file it was downloaded for.
    pub media_path: String,
    /// `opensubtitles`.
    pub source: String,
    /// The site's id of the file.
    pub file_id: u64,
    pub language: String,
    pub release: String,
    /// `srt`, `ass` or `vtt`: its file's extension.
    pub format: String,
    pub hearing_impaired: bool,
    /// `human`, `machine_translated` or `ai`.
    pub origin: String,
    pub hash_match: bool,
    /// A principal.
    pub downloaded_by: String,
    /// Their display name then.
    pub downloaded_name: String,
    pub downloaded_at: i64,
}

/// A day, in seconds: what a daily limit counts over.
pub const DAY_SECS: i64 = 24 * 60 * 60;

/// A row's columns, in [`row`]'s order: a macro, as sqlx takes only
/// static text.
macro_rules! columns {
    () => {
        "id, media_id, media_path, source, file_id, language, release, format, \
         hearing_impaired, origin, hash_match, downloaded_by, downloaded_name, downloaded_at"
    };
}

fn row(r: &sqlx::sqlite::SqliteRow) -> Result<DownloadRow, sqlx::Error> {
    Ok(DownloadRow {
        id: r.try_get("id")?,
        media_id: r.try_get("media_id")?,
        media_path: r.try_get("media_path")?,
        source: r.try_get("source")?,
        file_id: u64::try_from(r.try_get::<i64, _>("file_id")?).unwrap_or(0),
        language: r.try_get("language")?,
        release: r.try_get("release")?,
        format: r.try_get("format")?,
        hearing_impaired: r.try_get("hearing_impaired")?,
        origin: r.try_get("origin")?,
        hash_match: r.try_get("hash_match")?,
        downloaded_by: r.try_get("downloaded_by")?,
        downloaded_name: r.try_get("downloaded_name")?,
        downloaded_at: r.try_get("downloaded_at")?,
    })
}

impl Store {
    /// Every download, in the order they were made: what the server gives the media
    /// files at start.
    pub async fn all_subtitle_downloads(&self) -> Result<Vec<DownloadRow>, StoreError> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM subtitle_downloads ORDER BY rowid"
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row).collect::<Result<_, _>>()?)
    }

    /// Item `media_id`'s downloads in the order they were made (rowid): the order their tracks
    /// come in.
    pub async fn subtitle_downloads(&self, media_id: &str) -> Result<Vec<DownloadRow>, StoreError> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM subtitle_downloads WHERE media_id = ?1 \
             ORDER BY rowid"
        ))
        .bind(media_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row).collect::<Result<_, _>>()?)
    }

    /// The downloads of the media file at `media_path`, oldest first: the
    /// tracks it gains, in their order.
    pub async fn subtitle_downloads_of_file(
        &self,
        media_path: &str,
    ) -> Result<Vec<DownloadRow>, StoreError> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM subtitle_downloads WHERE media_path = ?1 ORDER BY rowid"
        ))
        .bind(media_path)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row).collect::<Result<_, _>>()?)
    }

    /// Item `media_id`'s download of the site's `file_id`, if it has one.
    pub async fn subtitle_download_of(
        &self,
        media_id: &str,
        source: &str,
        file_id: u64,
    ) -> Result<Option<DownloadRow>, StoreError> {
        let found = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM subtitle_downloads \
             WHERE media_id = ?1 AND source = ?2 AND file_id = ?3"
        ))
        .bind(media_id)
        .bind(source)
        .bind(file_id as i64)
        .fetch_optional(&self.pool)
        .await?;
        Ok(found.as_ref().map(row).transpose()?)
    }

    /// Keep `download`, and count it against its downloader's day. A
    /// rejection of the same result is taken back: someone wanted it
    /// again.
    pub async fn put_subtitle_download(&self, download: &DownloadRow) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(concat!(
            "INSERT INTO subtitle_downloads (",
            columns!(),
            ") VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"
        ))
        .bind(&download.id)
        .bind(&download.media_id)
        .bind(&download.media_path)
        .bind(&download.source)
        .bind(download.file_id as i64)
        .bind(&download.language)
        .bind(&download.release)
        .bind(&download.format)
        .bind(download.hearing_impaired)
        .bind(&download.origin)
        .bind(download.hash_match)
        .bind(&download.downloaded_by)
        .bind(&download.downloaded_name)
        .bind(download.downloaded_at)
        .execute(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO subtitle_download_log (principal, at) VALUES (?1, ?2)")
            .bind(&download.downloaded_by)
            .bind(download.downloaded_at)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "DELETE FROM subtitle_rejections WHERE media_id = ?1 AND source = ?2 AND file_id = ?3",
        )
        .bind(&download.media_id)
        .bind(&download.source)
        .bind(download.file_id as i64)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM subtitle_download_log WHERE at < ?1")
            .bind(self.now() - DAY_SECS)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Take download `id` away, as `by` (a principal), and remember its
    /// result as rejected for its item. `None` if there was none.
    pub async fn remove_subtitle_download(
        &self,
        id: &str,
        by: &str,
    ) -> Result<Option<DownloadRow>, StoreError> {
        let mut tx = self.pool.begin().await?;
        let found = sqlx::query(concat!(
            "DELETE FROM subtitle_downloads WHERE id = ?1 RETURNING ",
            columns!()
        ))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(removed) = found.as_ref().map(row).transpose()? else {
            return Ok(None);
        };
        sqlx::query(
            "INSERT OR REPLACE INTO subtitle_rejections \
             (media_id, source, file_id, rejected_by, rejected_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(&removed.media_id)
        .bind(&removed.source)
        .bind(removed.file_id as i64)
        .bind(by)
        .bind(self.now())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(removed))
    }

    /// The site's file ids removed as bad for item `media_id`.
    pub async fn subtitle_rejections(
        &self,
        media_id: &str,
        source: &str,
    ) -> Result<Vec<u64>, StoreError> {
        let rows = sqlx::query(
            "SELECT file_id FROM subtitle_rejections WHERE media_id = ?1 AND source = ?2",
        )
        .bind(media_id)
        .bind(source)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| r.try_get::<i64, _>("file_id").map(|id| id as u64))
            .collect::<Result<_, _>>()?)
    }

    /// When each of `principal`'s downloads of the last day was made,
    /// oldest first.
    pub async fn subtitle_downloads_today(&self, principal: &str) -> Result<Vec<i64>, StoreError> {
        let rows = sqlx::query(
            "SELECT at FROM subtitle_download_log WHERE principal = ?1 AND at > ?2 ORDER BY at",
        )
        .bind(principal)
        .bind(self.now() - DAY_SECS)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| r.try_get::<i64, _>("at"))
            .collect::<Result<_, _>>()?)
    }

    /// Whether `principal` may download subtitles: unless an admin took
    /// it from them.
    pub async fn may_download_subtitles(&self, principal: &str) -> Result<bool, StoreError> {
        let denied = sqlx::query("SELECT 1 FROM subtitle_download_denied WHERE principal = ?1")
            .bind(principal)
            .fetch_optional(&self.pool)
            .await?;
        Ok(denied.is_none())
    }

    /// Let `principal` download subtitles, or not, as admin `by` says.
    pub async fn set_may_download_subtitles(
        &self,
        principal: &str,
        allowed: bool,
        by: &str,
    ) -> Result<(), StoreError> {
        if allowed {
            sqlx::query("DELETE FROM subtitle_download_denied WHERE principal = ?1")
                .bind(principal)
                .execute(&self.pool)
                .await?;
        } else {
            sqlx::query(
                "INSERT OR REPLACE INTO subtitle_download_denied (principal, denied_by, denied_at) \
                 VALUES (?1, ?2, ?3)",
            )
            .bind(principal)
            .bind(by)
            .bind(self.now())
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    /// Every principal an admin took downloading from.
    pub async fn subtitle_download_denied(&self) -> Result<Vec<String>, StoreError> {
        let rows = sqlx::query("SELECT principal FROM subtitle_download_denied ORDER BY principal")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .iter()
            .map(|r| r.try_get::<String, _>("principal"))
            .collect::<Result<_, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::super::tests::test_store;
    use super::*;

    fn download(id: &str, file_id: u64, by: &str, at: i64) -> DownloadRow {
        DownloadRow {
            id: id.into(),
            media_id: "plex:movie:1".into(),
            media_path: "/films/Film.mkv".into(),
            source: "opensubtitles".into(),
            file_id,
            language: "en".into(),
            release: "Film.2019.1080p".into(),
            format: "srt".into(),
            hearing_impaired: false,
            origin: "human".into(),
            hash_match: false,
            downloaded_by: by.into(),
            downloaded_name: "Alice".into(),
            downloaded_at: at,
        }
    }

    #[tokio::test]
    async fn a_download_is_kept_and_found_by_its_result() {
        let store = test_store("subtitle-downloads").await;
        let now = store.now();
        store
            .put_subtitle_download(&download("d1", 7, "local:1", now))
            .await
            .unwrap();
        assert_eq!(
            store
                .subtitle_download_of("plex:movie:1", "opensubtitles", 7)
                .await
                .unwrap(),
            Some(download("d1", 7, "local:1", now))
        );
        assert_eq!(
            store
                .subtitle_download_of("plex:movie:2", "opensubtitles", 7)
                .await
                .unwrap(),
            None
        );
        // One result once per item.
        assert!(
            store
                .put_subtitle_download(&download("d2", 7, "local:1", now))
                .await
                .is_err()
        );
    }

    /// Removed, it is rejected for its item; downloaded again, it is not.
    /// The removal does not give the downloader's day back.
    #[tokio::test]
    async fn a_removed_download_is_rejected_and_still_counted() {
        let store = test_store("subtitle-downloads-removed").await;
        let now = store.now();
        store
            .put_subtitle_download(&download("d1", 7, "local:1", now))
            .await
            .unwrap();
        let removed = store
            .remove_subtitle_download("d1", "local:1")
            .await
            .unwrap();
        assert_eq!(removed.map(|r| r.file_id), Some(7));
        assert_eq!(
            store
                .remove_subtitle_download("d1", "local:1")
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .subtitle_rejections("plex:movie:1", "opensubtitles")
                .await
                .unwrap(),
            [7]
        );
        assert_eq!(
            store.subtitle_downloads_today("local:1").await.unwrap(),
            [now]
        );
        store
            .put_subtitle_download(&download("d3", 7, "local:2", now))
            .await
            .unwrap();
        assert!(
            store
                .subtitle_rejections("plex:movie:1", "opensubtitles")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_day_later_a_download_no_longer_counts() {
        let store = test_store("subtitle-downloads-day").await;
        let now = store.now();
        store
            .put_subtitle_download(&download("d1", 7, "local:1", now))
            .await
            .unwrap();
        store
            .clock()
            .advance(Duration::from_secs(DAY_SECS as u64 + 1));
        assert!(
            store
                .subtitle_downloads_today("local:1")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn an_admin_takes_downloading_away_and_gives_it_back() {
        let store = test_store("subtitle-downloads-denied").await;
        assert!(store.may_download_subtitles("local:2").await.unwrap());
        store
            .set_may_download_subtitles("local:2", false, "local:1")
            .await
            .unwrap();
        assert!(!store.may_download_subtitles("local:2").await.unwrap());
        assert!(store.may_download_subtitles("local:3").await.unwrap());
        assert_eq!(store.subtitle_download_denied().await.unwrap(), ["local:2"]);
        store
            .set_may_download_subtitles("local:2", true, "local:1")
            .await
            .unwrap();
        assert!(store.may_download_subtitles("local:2").await.unwrap());
    }
}
