//! Subtitles downloaded from a subtitle site (PX-02): storage. Fetching,
//! the files and who may do what are in `crate::subtitle_downloads`.

use sqlx::Row;

use super::{Store, StoreError};

/// One downloaded subtitle (migrations 0036 and 0037).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadRow {
    /// The server's own id: its file's name.
    pub id: String,
    pub media_id: String,
    /// The media file it was downloaded for.
    pub media_path: String,
    /// `opensubtitles` or `subdl`: [`crate::subtitle_site::Source::id`].
    pub source: String,
    /// The site's id of the file: `5338574`, `3172974-3188291`.
    pub file_id: String,
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

/// One download in the log of the last day (migrations 0036 and 0044).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoggedDownload {
    /// Who downloaded it.
    pub principal: String,
    /// Their display name then; empty in a row from before 0044.
    pub name: String,
    /// `opensubtitles` or `subdl`; empty in a row from before 0044.
    pub source: String,
    pub at: i64,
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
        file_id: r.try_get("file_id")?,
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
        file_id: &str,
    ) -> Result<Option<DownloadRow>, StoreError> {
        let found = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM subtitle_downloads \
             WHERE media_id = ?1 AND source = ?2 AND file_id = ?3"
        ))
        .bind(media_id)
        .bind(source)
        .bind(file_id)
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
        .bind(&download.file_id)
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
        sqlx::query(
            "INSERT INTO subtitle_download_log (principal, at, source, name) \
             VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(&download.downloaded_by)
        .bind(download.downloaded_at)
        .bind(&download.source)
        .bind(&download.downloaded_name)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM subtitle_rejections WHERE media_id = ?1 AND source = ?2 AND file_id = ?3",
        )
        .bind(&download.media_id)
        .bind(&download.source)
        .bind(&download.file_id)
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
        .bind(&removed.file_id)
        .bind(by)
        .bind(self.now())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(removed))
    }

    /// The results removed as bad for item `media_id`, every site's:
    /// (site, the site's file id).
    pub async fn subtitle_rejections(
        &self,
        media_id: &str,
    ) -> Result<Vec<(String, String)>, StoreError> {
        let rows =
            sqlx::query("SELECT source, file_id FROM subtitle_rejections WHERE media_id = ?1")
                .bind(media_id)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows
            .iter()
            .map(|r| Ok((r.try_get("source")?, r.try_get("file_id")?)))
            .collect::<Result<_, sqlx::Error>>()?)
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

    /// Every download of the last day, everyone's, oldest first: what
    /// counts against each user's daily limit and each site's account.
    /// Removed ones too.
    pub async fn subtitle_download_log(&self) -> Result<Vec<LoggedDownload>, StoreError> {
        let rows = sqlx::query(
            "SELECT principal, name, source, at FROM subtitle_download_log \
             WHERE at > ?1 ORDER BY at, rowid",
        )
        .bind(self.now() - DAY_SECS)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| {
                Ok(LoggedDownload {
                    principal: r.try_get("principal")?,
                    name: r.try_get("name")?,
                    source: r.try_get("source")?,
                    at: r.try_get("at")?,
                })
            })
            .collect::<Result<_, sqlx::Error>>()?)
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

    /// Item 1's rejected file ids at `source`.
    async fn rejected(store: &Store, source: &str) -> Vec<String> {
        store
            .subtitle_rejections("plex:movie:1")
            .await
            .unwrap()
            .into_iter()
            .filter(|(s, _)| s == source)
            .map(|(_, id)| id)
            .collect()
    }

    fn download(id: &str, file_id: &str, by: &str, at: i64) -> DownloadRow {
        DownloadRow {
            id: id.into(),
            media_id: "plex:movie:1".into(),
            media_path: "/films/Film.mkv".into(),
            source: "opensubtitles".into(),
            file_id: file_id.into(),
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
            .put_subtitle_download(&download("d1", "7", "local:1", now))
            .await
            .unwrap();
        assert_eq!(
            store
                .subtitle_download_of("plex:movie:1", "opensubtitles", "7")
                .await
                .unwrap(),
            Some(download("d1", "7", "local:1", now))
        );
        assert_eq!(
            store
                .subtitle_download_of("plex:movie:2", "opensubtitles", "7")
                .await
                .unwrap(),
            None
        );
        // One result once per item.
        assert!(
            store
                .put_subtitle_download(&download("d2", "7", "local:1", now))
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
            .put_subtitle_download(&download("d1", "7", "local:1", now))
            .await
            .unwrap();
        let removed = store
            .remove_subtitle_download("d1", "local:1")
            .await
            .unwrap();
        assert_eq!(removed.map(|r| r.file_id).as_deref(), Some("7"));
        assert_eq!(
            store
                .remove_subtitle_download("d1", "local:1")
                .await
                .unwrap(),
            None
        );
        assert_eq!(rejected(&store, "opensubtitles").await, ["7"]);
        assert_eq!(
            store.subtitle_downloads_today("local:1").await.unwrap(),
            [now]
        );
        store
            .put_subtitle_download(&download("d3", "7", "local:2", now))
            .await
            .unwrap();
        assert!(rejected(&store, "opensubtitles").await.is_empty());
    }

    /// SubDL's ids are text; one site's id is not another's.
    #[tokio::test]
    async fn a_subdl_id_is_kept_as_it_is_and_apart_from_opensubtitles() {
        let store = test_store("subtitle-downloads-subdl").await;
        let now = store.now();
        let mut subdl = download("d1", "3172974-3188291", "local:1", now);
        subdl.source = "subdl".into();
        store.put_subtitle_download(&subdl).await.unwrap();
        store
            .put_subtitle_download(&download("d2", "3172974-3188291", "local:1", now))
            .await
            .unwrap();
        assert_eq!(
            store
                .subtitle_download_of("plex:movie:1", "subdl", "3172974-3188291")
                .await
                .unwrap(),
            Some(subdl)
        );
        store
            .remove_subtitle_download("d1", "local:1")
            .await
            .unwrap();
        assert_eq!(rejected(&store, "subdl").await, ["3172974-3188291"]);
        assert!(rejected(&store, "opensubtitles").await.is_empty());
    }

    /// Migration 0037 keeps the downloads and rejections 0036 made, their
    /// OpenSubtitles ids as text, in their order.
    #[tokio::test]
    async fn the_migration_keeps_every_download_its_id_as_text() {
        let dir = super::super::tests::state_dir("subtitle-ids-as-text").await;
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(dir.as_ref().join(super::super::DB_FILE))
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = sqlx::pool::PoolOptions::<sqlx::Sqlite>::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        let mut before = sqlx::migrate!("./migrations");
        before.migrations = before
            .iter()
            .filter(|m| m.version < 37)
            .cloned()
            .collect::<Vec<_>>()
            .into();
        before.run(&pool).await.unwrap();
        sqlx::raw_sql(
            "INSERT INTO subtitle_downloads (id, media_id, media_path, source, file_id, \
               language, release, format, hearing_impaired, origin, hash_match, \
               downloaded_by, downloaded_name, downloaded_at) VALUES \
               ('b', 'plex:movie:1', '/f.mkv', 'opensubtitles', 5338574, 'en', 'R', 'srt', \
                0, 'human', 0, 'local:1', 'Alice', 5), \
               ('a', 'plex:movie:1', '/f.mkv', 'opensubtitles', 42, 'en', 'R', 'srt', \
                0, 'human', 0, 'local:1', 'Alice', 5); \
             INSERT INTO subtitle_rejections (media_id, source, file_id, rejected_by, \
               rejected_at) VALUES ('plex:movie:1', 'opensubtitles', 7, 'local:1', 6);",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        let store = Store::open(dir.as_ref()).await.unwrap();
        let kept = store.subtitle_downloads("plex:movie:1").await.unwrap();
        assert_eq!(
            kept.iter()
                .map(|r| (r.id.as_str(), r.file_id.as_str()))
                .collect::<Vec<_>>(),
            [("b", "5338574"), ("a", "42")]
        );
        assert_eq!(rejected(&store, "opensubtitles").await, ["7"]);
    }

    /// The day's log says each download's site and downloader, removed
    /// ones too, and forgets them a day later.
    #[tokio::test]
    async fn the_log_says_each_download_s_site_and_downloader() {
        let store = test_store("subtitle-downloads-log").await;
        let now = store.now();
        store
            .put_subtitle_download(&download("d1", "7", "local:1", now))
            .await
            .unwrap();
        let mut subdl = download("d2", "3172974-3188291", "local:2", now);
        subdl.source = "subdl".into();
        subdl.downloaded_name = "Bob".into();
        store.put_subtitle_download(&subdl).await.unwrap();
        store
            .remove_subtitle_download("d1", "local:1")
            .await
            .unwrap();
        let logged = |principal: &str, name: &str, source: &str| LoggedDownload {
            principal: principal.into(),
            name: name.into(),
            source: source.into(),
            at: now,
        };
        assert_eq!(
            store.subtitle_download_log().await.unwrap(),
            [
                logged("local:1", "Alice", "opensubtitles"),
                logged("local:2", "Bob", "subdl"),
            ]
        );
        store
            .clock()
            .advance(Duration::from_secs(DAY_SECS as u64 + 1));
        assert!(store.subtitle_download_log().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_day_later_a_download_no_longer_counts() {
        let store = test_store("subtitle-downloads-day").await;
        let now = store.now();
        store
            .put_subtitle_download(&download("d1", "7", "local:1", now))
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
