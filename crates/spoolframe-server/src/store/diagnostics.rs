//! Diagnostics apps sent (`SendDiagnostics`): kept for the admins to
//! read, one a minute from a session, the newest [`KEEP_PER_DEVICE`] of
//! each device, for [`KEEP_FOR`] at most.

use std::time::Duration;

use spoolframe_protocol::diagnostics::DiagnosticsReport;
use uuid::Uuid;

use super::{Store, StoreError};
use crate::principal::Principal;

/// How many reports of one device are kept: an app that sends one after
/// each failed play keeps its latest.
pub const KEEP_PER_DEVICE: i64 = 20;

/// How long a report is kept.
pub const KEEP_FOR: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// One report a session may send in this long, so an app's bug cannot
/// fill the disk.
pub const ONE_EACH: Duration = Duration::from_secs(60);

/// Who sent a report, and from where, as it arrived.
#[derive(Debug, Clone)]
pub struct Sender {
    pub principal: Principal,
    /// What the user was called then.
    pub user_name: String,
    /// The device's hex EndpointId.
    pub endpoint_id: String,
    /// The device's name from its `Hello`, if it gave one.
    pub device_name: Option<String>,
    /// The session's hash, as `sessions.token_hash` keys it.
    pub session_hash: String,
}

/// A report kept, without its log.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct DiagnosticsRow {
    pub id: String,
    /// The sender's principal, as stored.
    pub principal: String,
    pub user_name: String,
    pub endpoint_id: String,
    pub device_name: Option<String>,
    pub device_model: String,
    pub os_name: String,
    pub os_version: String,
    pub app_name: String,
    pub app_version: String,
    pub note: Option<String>,
    /// The log's length in bytes.
    pub log_bytes: i64,
    /// Unix seconds.
    pub received_at: i64,
}

/// Why a report was not kept.
#[derive(Debug, thiserror::Error)]
pub enum KeepError {
    /// The session sent one less than [`ONE_EACH`] ago.
    #[error("one diagnostics report a minute; try again in {retry_after:?}")]
    TooSoon { retry_after: Duration },
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<sqlx::Error> for KeepError {
    fn from(e: sqlx::Error) -> Self {
        Self::Store(e.into())
    }
}

/// The row's columns, the log's length for the log. A literal, for
/// `concat!`.
macro_rules! columns {
    () => {
        "id, principal, user_name, endpoint_id, device_name, device_model, os_name, \
         os_version, app_name, app_version, note, length(CAST(log AS BLOB)) AS log_bytes, \
         received_at"
    };
}

impl Store {
    /// Keep `report` from `sender`, and its id; [`KeepError::TooSoon`]
    /// when the session sent one less than [`ONE_EACH`] ago. Then the
    /// device's beyond its newest [`KEEP_PER_DEVICE`], and every report
    /// older than [`KEEP_FOR`], are deleted.
    pub async fn keep_diagnostics(
        &self,
        sender: &Sender,
        report: &DiagnosticsReport,
    ) -> Result<String, KeepError> {
        let now = self.now();
        let id = Uuid::now_v7().hyphenated().to_string();
        let mut tx = self.pool.begin().await?;
        // The check and the write are one statement: two reports at once
        // from a session cannot both pass.
        let kept = sqlx::query(
            "INSERT INTO diagnostics_reports (id, principal, user_name, endpoint_id, \
             device_name, session_hash, device_model, os_name, os_version, app_name, \
             app_version, note, log, received_at) \
             SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14 \
             WHERE NOT EXISTS (SELECT 1 FROM diagnostics_reports \
             WHERE session_hash = ?6 AND received_at > ?14 - ?15)",
        )
        .bind(&id)
        .bind(sender.principal.to_string())
        .bind(&sender.user_name)
        .bind(&sender.endpoint_id)
        .bind(&sender.device_name)
        .bind(&sender.session_hash)
        .bind(&report.device_model)
        .bind(&report.os_name)
        .bind(&report.os_version)
        .bind(&report.app_name)
        .bind(&report.app_version)
        .bind(&report.note)
        .bind(&report.log)
        .bind(now)
        .bind(ONE_EACH.as_secs() as i64)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if kept == 0 {
            let last: Option<i64> = sqlx::query_scalar(
                "SELECT max(received_at) FROM diagnostics_reports WHERE session_hash = ?1",
            )
            .bind(&sender.session_hash)
            .fetch_one(&mut *tx)
            .await?;
            let next = last.unwrap_or(now) + ONE_EACH.as_secs() as i64;
            return Err(KeepError::TooSoon {
                retry_after: Duration::from_secs(next.saturating_sub(now).max(1) as u64),
            });
        }
        sqlx::query(
            "DELETE FROM diagnostics_reports WHERE endpoint_id = ?1 AND id NOT IN \
             (SELECT id FROM diagnostics_reports WHERE endpoint_id = ?1 \
             ORDER BY received_at DESC, id DESC LIMIT ?2)",
        )
        .bind(&sender.endpoint_id)
        .bind(KEEP_PER_DEVICE)
        .execute(&mut *tx)
        .await?;
        prune(&mut tx, now).await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Delete the reports older than [`KEEP_FOR`]; how many went.
    pub async fn prune_diagnostics(&self) -> Result<u64, StoreError> {
        let mut conn = self.pool.acquire().await?;
        Ok(prune(&mut conn, self.now()).await?)
    }

    /// Every report kept, newest first, without their logs.
    pub async fn diagnostics_list(&self) -> Result<Vec<DiagnosticsRow>, StoreError> {
        Ok(sqlx::query_as(concat!(
            "SELECT ",
            columns!(),
            " FROM diagnostics_reports ORDER BY received_at DESC, id DESC"
        ))
        .fetch_all(&self.pool)
        .await?)
    }

    /// Report `id` and its log, if it is kept.
    pub async fn diagnostics(
        &self,
        id: &str,
    ) -> Result<Option<(DiagnosticsRow, String)>, StoreError> {
        let Some(row) = sqlx::query_as::<_, DiagnosticsRow>(concat!(
            "SELECT ",
            columns!(),
            " FROM diagnostics_reports WHERE id = ?1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };
        let log: Option<String> =
            sqlx::query_scalar("SELECT log FROM diagnostics_reports WHERE id = ?1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        // Deleted between the two reads: gone.
        Ok(log.map(|log| (row, log)))
    }

    /// Delete report `id`; whether it was there.
    pub async fn delete_diagnostics(&self, id: &str) -> Result<bool, StoreError> {
        Ok(sqlx::query("DELETE FROM diagnostics_reports WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected()
            > 0)
    }
}

async fn prune(conn: &mut sqlx::SqliteConnection, now: i64) -> Result<u64, sqlx::Error> {
    Ok(
        sqlx::query("DELETE FROM diagnostics_reports WHERE received_at <= ?1")
            .bind(now - KEEP_FOR.as_secs() as i64)
            .execute(conn)
            .await?
            .rows_affected(),
    )
}

#[cfg(test)]
mod tests {
    use super::super::tests::test_store;
    use super::*;
    use crate::store::users::LocalUserId;

    fn sender(device: &str, session: &str) -> Sender {
        Sender {
            principal: Principal::LocalUser(LocalUserId::from_uuid(Uuid::from_u128(1))),
            user_name: "alice".into(),
            endpoint_id: device.into(),
            device_name: Some("Alice's iPhone".into()),
            session_hash: session.into(),
        }
    }

    fn report(log: &str) -> DiagnosticsReport {
        DiagnosticsReport {
            device_model: "iPhone16,2".into(),
            os_name: "iOS".into(),
            os_version: "26.1".into(),
            app_name: "Spoolframe".into(),
            app_version: "1.4.0 (212)".into(),
            log: log.into(),
            note: Some("froze".into()),
        }
    }

    #[tokio::test]
    async fn a_report_is_kept_listed_read_and_deleted() {
        let store = test_store("diagnostics-round-trip").await;
        let from = sender("d1", "s1");
        let id = store
            .keep_diagnostics(&from, &report("héllo\n"))
            .await
            .unwrap();
        let listed = store.diagnostics_list().await.unwrap();
        assert_eq!(listed.len(), 1);
        let row = &listed[0];
        assert_eq!(row.id, id);
        assert_eq!(row.principal, from.principal.to_string());
        assert_eq!(row.device_name.as_deref(), Some("Alice's iPhone"));
        assert_eq!(row.note.as_deref(), Some("froze"));
        // Bytes, not characters.
        assert_eq!(row.log_bytes, 7);
        let (read, log) = store.diagnostics(&id).await.unwrap().unwrap();
        assert_eq!((&read, log.as_str()), (row, "héllo\n"));
        assert!(store.delete_diagnostics(&id).await.unwrap());
        assert!(!store.delete_diagnostics(&id).await.unwrap());
        assert_eq!(store.diagnostics(&id).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_session_sends_one_a_minute() {
        let store = test_store("diagnostics-rate").await;
        let from = sender("d1", "s1");
        store.keep_diagnostics(&from, &report("1")).await.unwrap();
        store.clock().advance(Duration::from_secs(59));
        let refused = store.keep_diagnostics(&from, &report("2")).await;
        assert!(
            matches!(refused, Err(KeepError::TooSoon { retry_after }) if retry_after == Duration::from_secs(1)),
            "{refused:?}"
        );
        // Another session on the same device is not held back by it.
        store
            .keep_diagnostics(&sender("d1", "s2"), &report("3"))
            .await
            .unwrap();
        store.clock().advance(Duration::from_secs(1));
        store.keep_diagnostics(&from, &report("4")).await.unwrap();
        assert_eq!(store.diagnostics_list().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn a_device_keeps_its_newest_twenty() {
        let store = test_store("diagnostics-per-device").await;
        let mut ids = Vec::new();
        for n in 0..=KEEP_PER_DEVICE {
            let id = store
                .keep_diagnostics(&sender("d1", &format!("s{n}")), &report("x"))
                .await
                .unwrap();
            ids.push(id);
            store.clock().advance(Duration::from_secs(1));
        }
        // Another device's are its own.
        store
            .keep_diagnostics(&sender("d2", "other"), &report("x"))
            .await
            .unwrap();
        let listed: Vec<_> = store
            .diagnostics_list()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.endpoint_id == "d1")
            .map(|r| r.id)
            .collect();
        assert_eq!(listed.len(), KEEP_PER_DEVICE as usize);
        assert!(!listed.contains(&ids[0]), "the oldest is gone");
        assert_eq!(listed[0], *ids.last().unwrap(), "newest first");
    }

    #[tokio::test]
    async fn nothing_older_than_thirty_days_is_kept() {
        let store = test_store("diagnostics-age").await;
        let old = store
            .keep_diagnostics(&sender("d1", "s1"), &report("old"))
            .await
            .unwrap();
        store.clock().advance(KEEP_FOR - Duration::from_secs(1));
        let young = store
            .keep_diagnostics(&sender("d2", "s2"), &report("young"))
            .await
            .unwrap();
        assert_eq!(store.prune_diagnostics().await.unwrap(), 0);
        store.clock().advance(Duration::from_secs(1));
        // A report from anywhere prunes every device's.
        store
            .keep_diagnostics(&sender("d3", "s3"), &report("new"))
            .await
            .unwrap();
        let ids: Vec<_> = store
            .diagnostics_list()
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert!(!ids.contains(&old), "{ids:?}");
        assert!(ids.contains(&young), "{ids:?}");
        store.clock().advance(KEEP_FOR);
        assert_eq!(store.prune_diagnostics().await.unwrap(), 2);
    }
}
