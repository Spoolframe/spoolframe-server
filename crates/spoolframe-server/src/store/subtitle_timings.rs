//! How external text subtitles are timed (PX-03): storage. The syncing
//! and serving are in `crate::subtitle_sync`, which reads a row, changes
//! it and writes it whole, one change at a time.

use super::{Store, StoreError};

/// A subtitle file's row (migration 0035).
#[derive(Debug, Clone, PartialEq)]
pub struct TimingRow {
    /// The subtitle file's path.
    pub sidecar: String,
    pub media_id: String,
    /// The file's size and modification time when the row was written.
    pub stamp: String,
    /// `original`, `syncing`, `synced`, `uncertain` or `failed`.
    pub state: String,
    pub serving_synced: bool,
    pub sync: Option<SyncFindings>,
    pub failure: Option<String>,
    pub offset_ms: i64,
    pub offset_by: Option<String>,
    pub offset_name: Option<String>,
    pub offset_at: Option<i64>,
}

/// What a sync found.
#[derive(Debug, Clone, PartialEq)]
pub struct SyncFindings {
    pub offset_ms: i64,
    pub factor: f64,
    pub splits: u32,
    pub score: f64,
    /// `subtitle` or `speech`.
    pub reference: String,
}

impl TimingRow {
    /// A file no one has synced or offset yet.
    pub fn new(sidecar: &str, media_id: &str, stamp: &str) -> Self {
        Self {
            sidecar: sidecar.to_owned(),
            media_id: media_id.to_owned(),
            stamp: stamp.to_owned(),
            state: "original".into(),
            serving_synced: false,
            sync: None,
            failure: None,
            offset_ms: 0,
            offset_by: None,
            offset_name: None,
            offset_at: None,
        }
    }
}

fn row(r: &sqlx::sqlite::SqliteRow) -> Result<TimingRow, sqlx::Error> {
    use sqlx::Row;
    let sync = match (
        r.try_get::<Option<i64>, _>("sync_offset_ms")?,
        r.try_get::<Option<f64>, _>("sync_factor")?,
        r.try_get::<Option<i64>, _>("sync_splits")?,
        r.try_get::<Option<f64>, _>("sync_score")?,
        r.try_get::<Option<String>, _>("sync_reference")?,
    ) {
        (Some(offset_ms), Some(factor), Some(splits), Some(score), Some(reference)) => {
            Some(SyncFindings {
                offset_ms,
                factor,
                splits: u32::try_from(splits).unwrap_or(u32::MAX),
                score,
                reference,
            })
        }
        _ => None,
    };
    Ok(TimingRow {
        sidecar: r.try_get("sidecar")?,
        media_id: r.try_get("media_id")?,
        stamp: r.try_get("stamp")?,
        state: r.try_get("state")?,
        serving_synced: r.try_get("serving_synced")?,
        sync,
        failure: r.try_get("failure")?,
        offset_ms: r.try_get("offset_ms")?,
        offset_by: r.try_get("offset_by")?,
        offset_name: r.try_get("offset_name")?,
        offset_at: r.try_get("offset_at")?,
    })
}

impl Store {
    /// The row of subtitle file `sidecar`, if it has one.
    pub async fn subtitle_timing(&self, sidecar: &str) -> Result<Option<TimingRow>, StoreError> {
        let found = sqlx::query(
            "SELECT sidecar, media_id, stamp, state, serving_synced, sync_offset_ms, \
             sync_factor, sync_splits, sync_score, sync_reference, failure, offset_ms, \
             offset_by, offset_name, offset_at FROM subtitle_timings WHERE sidecar = ?1",
        )
        .bind(sidecar)
        .fetch_optional(&self.pool)
        .await?;
        Ok(found.as_ref().map(row).transpose()?)
    }

    /// Write `timing` whole, over any row of its file.
    pub async fn put_subtitle_timing(&self, timing: &TimingRow) -> Result<(), StoreError> {
        let sync = timing.sync.as_ref();
        sqlx::query(
            "INSERT OR REPLACE INTO subtitle_timings (sidecar, media_id, stamp, state, \
             serving_synced, sync_offset_ms, sync_factor, sync_splits, sync_score, \
             sync_reference, failure, offset_ms, offset_by, offset_name, offset_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        )
        .bind(&timing.sidecar)
        .bind(&timing.media_id)
        .bind(&timing.stamp)
        .bind(&timing.state)
        .bind(timing.serving_synced)
        .bind(sync.map(|s| s.offset_ms))
        .bind(sync.map(|s| s.factor))
        .bind(sync.map(|s| i64::from(s.splits)))
        .bind(sync.map(|s| s.score))
        .bind(sync.map(|s| s.reference.clone()))
        .bind(&timing.failure)
        .bind(timing.offset_ms)
        .bind(&timing.offset_by)
        .bind(&timing.offset_name)
        .bind(timing.offset_at)
        .bind(self.now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Syncs a stopped server left running end as failed, served as they
    /// were: how many.
    pub async fn end_interrupted_subtitle_syncs(&self) -> Result<u64, StoreError> {
        let done = sqlx::query(
            "UPDATE subtitle_timings SET state = 'failed', \
             failure = 'the server stopped during the sync', updated_at = ?1 \
             WHERE state = 'syncing'",
        )
        .bind(self.now())
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::test_store;
    use super::*;

    fn synced() -> TimingRow {
        TimingRow {
            state: "synced".into(),
            serving_synced: true,
            sync: Some(SyncFindings {
                offset_ms: -3_200,
                factor: 25.0 / 23.976,
                splits: 1,
                score: 0.87,
                reference: "speech".into(),
            }),
            offset_ms: 1_500,
            offset_by: Some("local:1".into()),
            offset_name: Some("Alice".into()),
            offset_at: Some(1_760_000_000),
            ..TimingRow::new("/films/Film.en.srt", "plex:movie:1", "100 5")
        }
    }

    #[tokio::test]
    async fn a_timing_is_kept_whole_and_replaced_whole() {
        let store = test_store("subtitle-timings").await;
        assert_eq!(
            store.subtitle_timing("/films/Film.en.srt").await.unwrap(),
            None
        );
        store.put_subtitle_timing(&synced()).await.unwrap();
        assert_eq!(
            store.subtitle_timing("/films/Film.en.srt").await.unwrap(),
            Some(synced())
        );
        let original = TimingRow::new("/films/Film.en.srt", "plex:movie:1", "100 5");
        store.put_subtitle_timing(&original).await.unwrap();
        assert_eq!(
            store.subtitle_timing("/films/Film.en.srt").await.unwrap(),
            Some(original)
        );
    }

    /// A sync a stopped server left running is failed at the next start,
    /// still served as it was.
    #[tokio::test]
    async fn a_sync_a_stopped_server_left_is_failed() {
        let store = test_store("subtitle-timings-interrupted").await;
        let syncing = TimingRow {
            state: "syncing".into(),
            ..synced()
        };
        store.put_subtitle_timing(&syncing).await.unwrap();
        store
            .put_subtitle_timing(&TimingRow::new("/b.srt", "plex:movie:2", "1 1"))
            .await
            .unwrap();
        assert_eq!(store.end_interrupted_subtitle_syncs().await.unwrap(), 1);
        let after = store
            .subtitle_timing("/films/Film.en.srt")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.state, "failed");
        assert!(after.serving_synced);
        assert_eq!(
            store
                .subtitle_timing("/b.srt")
                .await
                .unwrap()
                .unwrap()
                .state,
            "original"
        );
    }
}
