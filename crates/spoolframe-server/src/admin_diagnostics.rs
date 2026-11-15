//! Server › Diagnostics: the `diagnostics*` methods, over the reports
//! apps sent (`SendDiagnostics`).

use jsonrpsee::core::RpcResult;
use jsonrpsee::types::ErrorObjectOwned;
use spoolframe_admin_api::{DiagnosticsDetail, DiagnosticsSummary, error_code};

use super::{AdminApi, error, internal};
use crate::store::diagnostics::{DiagnosticsRow, DiagnosticsWithLog};

impl AdminApi {
    pub(super) async fn diagnostics_rows(&self) -> RpcResult<Vec<DiagnosticsSummary>> {
        let rows = self
            .auth
            .store()
            .diagnostics_list()
            .await
            .map_err(internal)?;
        Ok(rows.iter().map(summary).collect())
    }

    pub(super) async fn diagnostics_detail(&self, id: &str) -> RpcResult<DiagnosticsDetail> {
        let DiagnosticsWithLog { row, log } = self
            .auth
            .store()
            .diagnostics(id)
            .await
            .map_err(internal)?
            .ok_or_else(|| no_such_report(id))?;
        Ok(DiagnosticsDetail {
            summary: summary(&row),
            note: row.note,
            log,
        })
    }

    pub(super) async fn diagnostics_remove(&self, id: &str) -> RpcResult<()> {
        if self
            .auth
            .store()
            .delete_diagnostics(id)
            .await
            .map_err(internal)?
        {
            Ok(())
        } else {
            Err(no_such_report(id))
        }
    }
}

fn summary(row: &DiagnosticsRow) -> DiagnosticsSummary {
    DiagnosticsSummary {
        has_note: row.note.is_some(),
        id: row.id.clone(),
        user_id: row.principal.clone(),
        user_name: row.user_name.clone(),
        device_name: row.device_name.clone(),
        endpoint_id: row.endpoint_id.clone(),
        device_model: row.device_model.clone(),
        os_name: row.os_name.clone(),
        os_version: row.os_version.clone(),
        app_name: row.app_name.clone(),
        app_version: row.app_version.clone(),
        received_at: row.received_at,
        log_bytes: u64::try_from(row.log_bytes).unwrap_or(0),
    }
}

fn no_such_report(id: &str) -> ErrorObjectOwned {
    error(
        error_code::NO_SUCH_REPORT,
        format!("no diagnostics report {id}: it was deleted, or kept past 30 days"),
    )
}
