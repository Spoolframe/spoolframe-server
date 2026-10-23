//! Downloads (PX-38): storage. Which user prepared which file, by a
//! random id of their own (migration 0040), so a restart does not write a
//! ready file again. No admin API reads this: downloads are the user's
//! business (the user's decision, 2026-10-20). Writing, serving and
//! removing the files is [`crate::downloads`]'s.

use password_hash::rand_core::{OsRng, RngCore};

use super::{Store, StoreError};

/// Where a download's file is, as the database keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoredState {
    /// Asked for; waiting for its copy, room in the area, or a place.
    Waiting,
    /// Its copy (or its subtitles' text) is being made.
    Converting,
    /// Its file is being written.
    Writing,
    /// Whole: `file` is where (the media file, for one sent as it is).
    Ready {
        file: String,
        size_bytes: u64,
        sha256: String,
    },
    /// It could not be made, for `reason`; preparing it again tries
    /// again.
    Failed { reason: String, cause: FailureCause },
}

/// Why a download failed (migration 0043), for the device to choose what
/// to show; the reason says it in words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureCause {
    /// Its conversion failed, or the server stopped while it was being
    /// prepared.
    ServerStoppedConverting,
    /// Its media file is gone, or changed since it was asked for.
    SourceMissing,
    /// A paired server's item no longer shared (P8-07): only told of a
    /// download the user's server keeps for a paired server's item.
    NoLongerAvailable,
    /// Anything else, and every failure recorded before the cause was.
    Other,
}

impl FailureCause {
    /// As the database keeps it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ServerStoppedConverting => "server_stopped_converting",
            Self::SourceMissing => "source_missing",
            Self::NoLongerAvailable => "no_longer_available",
            Self::Other => "other",
        }
    }

    /// The cause `s` names; `other` for none, or one this build does not
    /// know.
    fn parse(s: Option<&str>) -> Self {
        match s {
            Some("server_stopped_converting") => Self::ServerStoppedConverting,
            Some("source_missing") => Self::SourceMissing,
            Some("no_longer_available") => Self::NoLongerAvailable,
            _ => Self::Other,
        }
    }
}

impl StoredState {
    /// Failed for `reason`, of `cause`.
    pub fn failed(cause: FailureCause, reason: impl Into<String>) -> Self {
        Self::Failed {
            reason: reason.into(),
            cause,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Converting => "converting",
            Self::Writing => "writing",
            Self::Ready { .. } => "ready",
            Self::Failed { .. } => "failed",
        }
    }
}

/// A download's row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadRow {
    /// Random, 32 hex digits.
    pub id: String,
    /// Whose: [`crate::principal::Principal`] as it is stored.
    pub principal: String,
    pub media_id: String,
    /// The request, as JSON: the same principal asking for the same one
    /// gets the same id.
    pub request: String,
    /// The file it is written as, shared with every other download of the
    /// same content.
    pub content_key: String,
    pub state: StoredState,
    pub created_at: i64,
    /// When it was last read, or made ready.
    pub last_read_at: Option<i64>,
    /// The tracks its file carries, as JSON.
    pub carried: String,
    /// The media file's stamp ([`crate::files::stamp_text`]) its content
    /// key was made from.
    pub stamp: String,
    /// About how many bytes its file will be, as it was prepared; `None`
    /// where that was not known (migration 0046).
    pub expected_size: Option<u64>,
}

/// A download asked for.
#[derive(Debug, Clone, Copy)]
pub struct NewDownload<'a> {
    pub principal: &'a str,
    pub media_id: &'a str,
    pub request: &'a str,
    pub content_key: &'a str,
    /// The tracks its file carries, as JSON.
    pub carried: &'a str,
    /// The media file's stamp the content key was made from.
    pub stamp: &'a str,
    /// About how many bytes its file will be, if known.
    pub expected_size: Option<u64>,
}

/// Which downloads a change applies to.
#[derive(Debug, Clone, Copy)]
pub enum Of<'a> {
    /// The one with this id.
    Id(&'a str),
    /// Every one with this content key: one file's.
    Content(&'a str),
}

macro_rules! columns {
    () => {
        "id, principal, media_id, request, content_key, state, file, size_bytes, sha256, \
         failure, failure_cause, created_at, last_read_at, carried, stamp, expected_size"
    };
}

fn row(r: &sqlx::sqlite::SqliteRow) -> Result<DownloadRow, sqlx::Error> {
    use sqlx::Row;
    let name: String = r.try_get("state")?;
    let state = match name.as_str() {
        "waiting" => StoredState::Waiting,
        "converting" => StoredState::Converting,
        "writing" => StoredState::Writing,
        "ready" => StoredState::Ready {
            file: r.try_get::<Option<String>, _>("file")?.unwrap_or_default(),
            size_bytes: u64::try_from(r.try_get::<Option<i64>, _>("size_bytes")?.unwrap_or(0))
                .unwrap_or(0),
            sha256: r
                .try_get::<Option<String>, _>("sha256")?
                .unwrap_or_default(),
        },
        "failed" => StoredState::Failed {
            reason: r
                .try_get::<Option<String>, _>("failure")?
                .unwrap_or_default(),
            cause: FailureCause::parse(r.try_get::<Option<String>, _>("failure_cause")?.as_deref()),
        },
        other => {
            return Err(sqlx::Error::ColumnDecode {
                index: "state".into(),
                source: format!("{other:?} is not a value this build knows").into(),
            });
        }
    };
    Ok(DownloadRow {
        id: r.try_get("id")?,
        principal: r.try_get("principal")?,
        media_id: r.try_get("media_id")?,
        request: r.try_get("request")?,
        content_key: r.try_get("content_key")?,
        state,
        created_at: r.try_get("created_at")?,
        last_read_at: r.try_get("last_read_at")?,
        carried: r.try_get("carried")?,
        stamp: r.try_get("stamp")?,
        expected_size: r
            .try_get::<Option<i64>, _>("expected_size")?
            .and_then(|n| u64::try_from(n).ok()),
    })
}

/// A new download id: 128 random bits.
fn new_id() -> String {
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Store {
    /// The download `new` asks for: the one its principal asked for with
    /// the same request, or a new one, waiting. One whose content key is
    /// no longer the request's (its media file changed, a track it reads
    /// was made again) takes the new key and waits again.
    pub async fn put_download(&self, new: NewDownload<'_>) -> Result<DownloadRow, StoreError> {
        let mut tx = self.begin_write().await?;
        let found: Option<(String, String)> = sqlx::query_as(
            "SELECT id, content_key FROM downloads \
             WHERE principal = ?1 AND media_id = ?2 AND request = ?3",
        )
        .bind(new.principal)
        .bind(new.media_id)
        .bind(new.request)
        .fetch_optional(&mut *tx)
        .await?;
        let id = match found {
            // The same content is the same tracks and stamp; its size is
            // estimated again, as the estimate may have a better basis.
            Some((id, key)) if key == new.content_key => {
                if let Some(size) = new.expected_size {
                    sqlx::query("UPDATE downloads SET expected_size = ?2 WHERE id = ?1")
                        .bind(&id)
                        .bind(i64::try_from(size).unwrap_or(i64::MAX))
                        .execute(&mut *tx)
                        .await?;
                }
                id
            }
            Some((id, _)) => {
                sqlx::query(
                    "UPDATE downloads SET content_key = ?2, state = 'waiting', file = NULL, \
                     size_bytes = NULL, sha256 = NULL, failure = NULL, carried = ?3, stamp = ?4, \
                     expected_size = ?5 WHERE id = ?1",
                )
                .bind(&id)
                .bind(new.content_key)
                .bind(new.carried)
                .bind(new.stamp)
                .bind(
                    new.expected_size
                        .map(|n| i64::try_from(n).unwrap_or(i64::MAX)),
                )
                .execute(&mut *tx)
                .await?;
                id
            }
            None => {
                let id = new_id();
                sqlx::query(
                    "INSERT INTO downloads (id, principal, media_id, request, content_key, \
                     state, created_at, carried, stamp, expected_size) \
                     VALUES (?1, ?2, ?3, ?4, ?5, 'waiting', ?6, ?7, ?8, ?9)",
                )
                .bind(&id)
                .bind(new.principal)
                .bind(new.media_id)
                .bind(new.request)
                .bind(new.content_key)
                .bind(self.now())
                .bind(new.carried)
                .bind(new.stamp)
                .bind(
                    new.expected_size
                        .map(|n| i64::try_from(n).unwrap_or(i64::MAX)),
                )
                .execute(&mut *tx)
                .await?;
                id
            }
        };
        tx.commit().await?;
        self.download(&id)
            .await?
            .ok_or(StoreError::Db(sqlx::Error::RowNotFound))
    }

    /// Download `id`, if there is one.
    pub async fn download(&self, id: &str) -> Result<Option<DownloadRow>, StoreError> {
        let found = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM downloads WHERE id = ?1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(found.as_ref().map(row).transpose()?)
    }

    /// The downloads of `ids` there are, in no order: one query, however
    /// many are asked about at once.
    pub async fn downloads_by_id(&self, ids: &[String]) -> Result<Vec<DownloadRow>, StoreError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut query = sqlx::QueryBuilder::new(concat!(
            "SELECT ",
            columns!(),
            " FROM downloads WHERE id IN "
        ));
        super::push_in_list(&mut query, ids);
        let rows = query.build().fetch_all(&self.pool).await?;
        Ok(rows.iter().map(row).collect::<Result<_, _>>()?)
    }

    /// The SHA-256 a ready download sent as it is has of `file` at
    /// `stamp`, if one has: a file hashed once is not hashed again, a
    /// restart included.
    pub async fn ready_sha256_of(
        &self,
        file: &str,
        stamp: &str,
    ) -> Result<Option<String>, StoreError> {
        Ok(sqlx::query_scalar(
            "SELECT sha256 FROM downloads \
             WHERE state = 'ready' AND file = ?1 AND stamp = ?2 AND sha256 IS NOT NULL LIMIT 1",
        )
        .bind(file)
        .bind(stamp)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Every download, oldest first.
    pub async fn downloads(&self) -> Result<Vec<DownloadRow>, StoreError> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM downloads ORDER BY created_at, id"
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row).collect::<Result<_, _>>()?)
    }

    /// The downloads of the principals `prefix` begins: a paired server's
    /// users' (`remote:<server id>:`, P8-07). A range of principals, so
    /// the index on them is used.
    pub async fn downloads_of_principals(
        &self,
        prefix: &str,
    ) -> Result<Vec<DownloadRow>, StoreError> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM downloads WHERE principal >= ?1 AND principal < ?2 ORDER BY created_at, id"
        ))
        .bind(prefix)
        .bind(past(prefix))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row).collect::<Result<_, _>>()?)
    }

    /// The downloads of content `key`: one file's.
    pub async fn downloads_of_content(&self, key: &str) -> Result<Vec<DownloadRow>, StoreError> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM downloads WHERE content_key = ?1 ORDER BY created_at, id"
        ))
        .bind(key)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row).collect::<Result<_, _>>()?)
    }

    /// `of` is in `state` now. Made ready, it counts as read now.
    pub async fn set_download_state(
        &self,
        of: Of<'_>,
        state: &StoredState,
    ) -> Result<(), StoreError> {
        let (file, size, sha, failure, cause) = match state {
            StoredState::Ready {
                file,
                size_bytes,
                sha256,
            } => (
                Some(file.as_str()),
                Some(i64::try_from(*size_bytes).unwrap_or(i64::MAX)),
                Some(sha256.as_str()),
                None,
                None,
            ),
            StoredState::Failed { reason, cause } => (
                None,
                None,
                None,
                Some(reason.as_str()),
                Some(cause.as_str()),
            ),
            _ => (None, None, None, None, None),
        };
        let read_at = matches!(state, StoredState::Ready { .. }).then(|| self.now());
        // One statement each, so neither is built at run time.
        let (sql, key) = match of {
            Of::Id(id) => (
                "UPDATE downloads SET state = ?2, file = ?3, size_bytes = ?4, sha256 = ?5, \
                 failure = ?6, failure_cause = ?8, last_read_at = COALESCE(?7, last_read_at) \
                 WHERE id = ?1",
                id,
            ),
            Of::Content(key) => (
                "UPDATE downloads SET state = ?2, file = ?3, size_bytes = ?4, sha256 = ?5, \
                 failure = ?6, failure_cause = ?8, last_read_at = COALESCE(?7, last_read_at) \
                 WHERE content_key = ?1",
                key,
            ),
        };
        sqlx::query(sql)
            .bind(key)
            .bind(state.name())
            .bind(file)
            .bind(size)
            .bind(sha)
            .bind(failure)
            .bind(read_at)
            .bind(cause)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Download `id` was read now: its file stays another day.
    pub async fn download_read(&self, id: &str) -> Result<(), StoreError> {
        sqlx::query("UPDATE downloads SET last_read_at = ?2 WHERE id = ?1")
            .bind(id)
            .bind(self.now())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Forget download `id`: its row, if there was one.
    pub async fn delete_download(&self, id: &str) -> Result<Option<DownloadRow>, StoreError> {
        let found = sqlx::query(concat!(
            "DELETE FROM downloads WHERE id = ?1 RETURNING ",
            columns!()
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(found.as_ref().map(row).transpose()?)
    }

    /// Fail every download a stopped server was preparing, for `why`, of
    /// `cause`: how many. Preparing one again starts it anew.
    pub async fn fail_unfinished_downloads(
        &self,
        why: &str,
        cause: FailureCause,
    ) -> Result<u64, StoreError> {
        let done = sqlx::query(
            "UPDATE downloads SET state = 'failed', failure = ?1, failure_cause = ?2 \
             WHERE state IN ('waiting', 'converting', 'writing')",
        )
        .bind(why)
        .bind(cause.as_str())
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected())
    }
}

/// The least string past every one `prefix` begins: its last character
/// one more. `prefix` is a principal's start, in ASCII.
fn past(prefix: &str) -> String {
    let mut past: Vec<char> = prefix.chars().collect();
    if let Some(last) = past.last_mut() {
        *last = char::from_u32(u32::from(*last) + 1).unwrap_or(char::MAX);
    }
    past.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::super::tests::test_store;
    use super::*;

    fn asked<'a>(principal: &'a str, request: &'a str, key: &'a str) -> NewDownload<'a> {
        NewDownload {
            principal,
            media_id: "plex:movie:1",
            request,
            content_key: key,
            carried: "{}",
            stamp: "1 2",
            expected_size: None,
        }
    }

    #[tokio::test]
    async fn the_expected_size_is_kept_and_made_again_with_the_content() {
        let store = test_store("downloads-expected-size").await;
        let sized = |key, size| NewDownload {
            expected_size: size,
            ..asked("local:a", "{}", key)
        };
        let first = store.put_download(sized("k", Some(5))).await.unwrap();
        assert_eq!(first.expected_size, Some(5));
        // Asked again without a size, the one known stays.
        let again = store.put_download(sized("k", None)).await.unwrap();
        assert_eq!(again.expected_size, Some(5));
        // Other content is another file, of another size.
        let changed = store.put_download(sized("k2", Some(9))).await.unwrap();
        assert_eq!(changed.expected_size, Some(9));
        let unknown = store.put_download(sized("k3", None)).await.unwrap();
        assert_eq!(unknown.expected_size, None);
    }

    #[tokio::test]
    async fn the_same_user_with_the_same_request_gets_the_same_id() {
        let store = test_store("downloads-same").await;
        let first = store
            .put_download(asked("local:a", "{}", "k"))
            .await
            .unwrap();
        let again = store
            .put_download(asked("local:a", "{}", "k"))
            .await
            .unwrap();
        assert_eq!(first.id, again.id);
    }

    #[tokio::test]
    async fn another_user_with_the_same_request_gets_an_id_of_their_own() {
        let store = test_store("downloads-other").await;
        let first = store
            .put_download(asked("local:a", "{}", "k"))
            .await
            .unwrap();
        let other = store
            .put_download(asked("local:b", "{}", "k"))
            .await
            .unwrap();
        assert_ne!(first.id, other.id);
    }

    /// A file sent as it is keeps its SHA-256 on its row: asked again at
    /// the same stamp, it is there; at another, it is not.
    #[tokio::test]
    async fn a_ready_file_s_sha256_is_found_by_its_file_and_stamp() {
        let store = test_store("downloads-sha-of").await;
        store
            .put_download(asked("local:a", "{}", "k"))
            .await
            .unwrap();
        let ready = StoredState::Ready {
            file: "/films/Alien.mkv".into(),
            size_bytes: 10,
            sha256: "ab".into(),
        };
        store
            .set_download_state(Of::Content("k"), &ready)
            .await
            .unwrap();
        assert_eq!(
            (
                store
                    .ready_sha256_of("/films/Alien.mkv", "1 2")
                    .await
                    .unwrap(),
                store
                    .ready_sha256_of("/films/Alien.mkv", "1 3")
                    .await
                    .unwrap(),
            ),
            (Some("ab".to_owned()), None)
        );
    }

    #[tokio::test]
    async fn an_id_is_128_random_bits_in_hex() {
        let store = test_store("downloads-id").await;
        let row = store
            .put_download(asked("local:a", "{}", "k"))
            .await
            .unwrap();
        assert!(
            row.id.len() == 32 && row.id.chars().all(|c| c.is_ascii_hexdigit()),
            "{}",
            row.id
        );
    }

    #[tokio::test]
    async fn a_request_whose_content_changed_waits_again_under_its_new_key() {
        let store = test_store("downloads-rekey").await;
        let row = store
            .put_download(asked("local:a", "{}", "k1"))
            .await
            .unwrap();
        let ready = StoredState::Ready {
            file: "/d/k1.mp4".into(),
            size_bytes: 5,
            sha256: "ab".into(),
        };
        store
            .set_download_state(Of::Id(&row.id), &ready)
            .await
            .unwrap();
        let again = store
            .put_download(asked("local:a", "{}", "k2"))
            .await
            .unwrap();
        assert_eq!(
            (again.id, again.content_key, again.state),
            (row.id, "k2".to_owned(), StoredState::Waiting)
        );
    }

    #[tokio::test]
    async fn a_file_made_ready_is_ready_for_every_download_of_it() {
        let store = test_store("downloads-shared").await;
        let a = store
            .put_download(asked("local:a", "{}", "k"))
            .await
            .unwrap();
        let b = store
            .put_download(asked("local:b", "{}", "k"))
            .await
            .unwrap();
        let ready = StoredState::Ready {
            file: "/d/k.mkv".into(),
            size_bytes: 9,
            sha256: "cd".into(),
        };
        store
            .set_download_state(Of::Content("k"), &ready)
            .await
            .unwrap();
        let states = [
            store.download(&a.id).await.unwrap().unwrap().state,
            store.download(&b.id).await.unwrap().unwrap().state,
        ];
        assert_eq!(states, [ready.clone(), ready]);
    }

    #[tokio::test]
    async fn downloads_a_stopped_server_was_preparing_fail() {
        let store = test_store("downloads-unfinished").await;
        let row = store
            .put_download(asked("local:a", "{}", "k"))
            .await
            .unwrap();
        store
            .fail_unfinished_downloads("stopped", FailureCause::ServerStoppedConverting)
            .await
            .unwrap();
        assert_eq!(
            store.download(&row.id).await.unwrap().unwrap().state,
            StoredState::failed(FailureCause::ServerStoppedConverting, "stopped")
        );
    }

    /// A failure's cause is kept beside its reason, and one recorded
    /// before causes were (NULL) reads as `other`.
    #[tokio::test]
    async fn a_failure_keeps_its_cause() {
        let store = test_store("downloads-cause").await;
        let row = store
            .put_download(asked("local:a", "{}", "k"))
            .await
            .unwrap();
        let gone = StoredState::failed(FailureCause::SourceMissing, "the media file is gone");
        store
            .set_download_state(Of::Id(&row.id), &gone)
            .await
            .unwrap();
        let kept = store.download(&row.id).await.unwrap().unwrap().state;
        sqlx::query("UPDATE downloads SET failure_cause = NULL WHERE id = ?1")
            .bind(&row.id)
            .execute(&store.pool)
            .await
            .unwrap();
        assert_eq!(
            (kept, store.download(&row.id).await.unwrap().unwrap().state),
            (
                gone,
                StoredState::failed(FailureCause::Other, "the media file is gone")
            )
        );
    }
}
