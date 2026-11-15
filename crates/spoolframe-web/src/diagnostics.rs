//! Server › Diagnostics: what the list and a report's page write of the
//! reports apps sent. Host-compiled, so `cargo test` covers it.

use spoolframe_admin_api::{DiagnosticsDetail, DiagnosticsSummary};

use crate::catalog::bytes;
use crate::logic::date;

/// The device as the list names it: its name from the app, else its
/// model.
pub fn device_line(report: &DiagnosticsSummary) -> String {
    match &report.device_name {
        Some(name) if !name.trim().is_empty() => format!("{name} · {}", report.device_model),
        _ => report.device_model.clone(),
    }
}

/// What it runs: "iOS 26.1 · Spoolframe 1.4.0 (212)".
pub fn software_line(report: &DiagnosticsSummary) -> String {
    format!(
        "{} {} · {} {}",
        report.os_name, report.os_version, report.app_name, report.app_version
    )
}

/// The log's size: "12 KB", "no log".
pub fn size_line(report: &DiagnosticsSummary) -> String {
    match report.log_bytes {
        0 => "no log".to_owned(),
        n => bytes(n),
    }
}

/// The file Download saves: the date it arrived, the user and the start
/// of its id, with no character a file name may not hold.
pub fn download_name(report: &DiagnosticsSummary) -> String {
    let user: String = report
        .user_name
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    let id: String = report.id.chars().take(8).collect();
    format!(
        "spoolframe-diagnostics-{}-{user}-{id}.txt",
        date(report.received_at)
    )
}

/// The file's text: what the report says of itself, then its note, then
/// its log as it was sent.
pub fn download_text(report: &DiagnosticsDetail) -> String {
    let s = &report.summary;
    let mut text = format!(
        "Report: {}\nUser: {} ({})\nDevice: {}\nDevice key: {}\nSoftware: {}\nReceived (UTC): {}\n",
        s.id,
        s.user_name,
        s.user_id,
        device_line(s),
        s.endpoint_id,
        software_line(s),
        date(s.received_at),
    );
    if let Some(note) = &report.note {
        text.push_str(&format!("Note: {note}\n"));
    }
    text.push('\n');
    text.push_str(&report.log);
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary() -> DiagnosticsSummary {
        DiagnosticsSummary {
            id: "0192f1c4-5b6e-7a3d-9c41-2e8f7b6a5d4c".into(),
            user_id: "local:u".into(),
            user_name: "alice b".into(),
            device_name: Some("Alice's iPhone".into()),
            endpoint_id: "ab12".into(),
            device_model: "iPhone16,2".into(),
            os_name: "iOS".into(),
            os_version: "26.1".into(),
            app_name: "Spoolframe".into(),
            app_version: "1.4.0 (212)".into(),
            // 2026-11-15T00:00:00Z.
            received_at: 1_794_700_800,
            log_bytes: 12_400,
            has_note: true,
        }
    }

    #[test]
    fn a_device_is_named_and_its_model_given() {
        assert_eq!(device_line(&summary()), "Alice's iPhone · iPhone16,2");
        let unnamed = DiagnosticsSummary {
            device_name: None,
            ..summary()
        };
        assert_eq!(device_line(&unnamed), "iPhone16,2");
    }

    #[test]
    fn software_and_size_read_as_the_page_shows_them() {
        assert_eq!(
            software_line(&summary()),
            "iOS 26.1 · Spoolframe 1.4.0 (212)"
        );
        assert_eq!(size_line(&summary()), "12.4 KB");
        let empty = DiagnosticsSummary {
            log_bytes: 0,
            ..summary()
        };
        assert_eq!(size_line(&empty), "no log");
    }

    #[test]
    fn the_file_is_named_by_date_user_and_id() {
        assert_eq!(
            download_name(&summary()),
            "spoolframe-diagnostics-2026-11-15-alice-b-0192f1c4.txt"
        );
    }

    #[test]
    fn the_file_says_who_and_what_then_the_log_as_sent() {
        let text = download_text(&DiagnosticsDetail {
            summary: summary(),
            note: Some("froze".into()),
            log: "line 1\nline 2\n".into(),
        });
        assert!(text.starts_with("Report: 0192f1c4"), "{text}");
        assert!(text.contains("User: alice b (local:u)\n"), "{text}");
        assert!(text.contains("Note: froze\n"), "{text}");
        assert!(text.ends_with("\n\nline 1\nline 2\n"), "{text}");
    }
}
