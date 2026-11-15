//! Server › Diagnostics: the `diagnostics*` methods, over the reports
//! apps sent (`SendDiagnostics`).

use jsonrpsee::core::RpcResult;
use jsonrpsee::types::ErrorObjectOwned;
use spoolframe_admin_api::{DiagnosticsDetail, DiagnosticsSummary, error_code};

use super::{AdminApi, error, internal};
use crate::store::diagnostics::DiagnosticsRow;

impl AdminApi {
    pub(super) async fn diagnostics_rows(&self) -> RpcResult<Vec<DiagnosticsSummary>> {
        let store = self.auth.store();
        // Pruned here too, so none past its time is listed while no app
        // sends one.
        store.prune_diagnostics().await.map_err(internal)?;
        let rows = store.diagnostics_list().await.map_err(internal)?;
        Ok(rows.into_iter().map(summary).collect())
    }

    pub(super) async fn diagnostics_detail(&self, id: &str) -> RpcResult<DiagnosticsDetail> {
        let (row, log) = self
            .auth
            .store()
            .diagnostics(id)
            .await
            .map_err(internal)?
            .ok_or_else(|| no_such_report(id))?;
        let note = row.note.clone();
        Ok(DiagnosticsDetail {
            summary: summary(row),
            note,
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

fn summary(row: DiagnosticsRow) -> DiagnosticsSummary {
    DiagnosticsSummary {
        has_note: row.note.is_some(),
        id: row.id,
        user_id: row.principal,
        user_name: row.user_name,
        device_name: row.device_name,
        endpoint_id: row.endpoint_id,
        device_model: row.device_model,
        os_name: row.os_name,
        os_version: row.os_version,
        app_name: row.app_name,
        app_version: row.app_version,
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
