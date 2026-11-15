//! Server › Diagnostics: the reports apps sent (`SendDiagnostics`, feature
//! `spoolframe:diagnostics`), for the admin to read, save and delete.

use serde::{Deserialize, Serialize};

/// A report in the list, without its log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticsSummary {
    pub id: String,
    /// The sender's user id (`local:…`, `portable:…`), and what they were
    /// called when it arrived.
    pub user_id: String,
    pub user_name: String,
    /// The device's name from its app, if it gave one, and its key.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub device_name: Option<String>,
    pub endpoint_id: String,
    /// The device's model, as the platform names it (`iPhone16,2`).
    pub device_model: String,
    pub os_name: String,
    pub os_version: String,
    pub app_name: String,
    pub app_version: String,
    /// When it arrived, Unix seconds.
    pub received_at: i64,
    /// The log's length in bytes.
    pub log_bytes: u64,
    /// The user wrote a note.
    pub has_note: bool,
}

/// One report in full (`diagnostics`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticsDetail {
    pub summary: DiagnosticsSummary,
    /// What the user wrote, if anything.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub note: Option<String>,
    /// The app's playback log, as it was sent.
    pub log: String,
}
