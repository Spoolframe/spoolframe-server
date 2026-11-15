//! Diagnostics an app sends to its server for the admin to read (feature
//! `spoolframe:diagnostics`): Settings › About › "Send diagnostics".
//!
//! A report is one request, and a request is one frame of at most
//! [`MAX_FRAME`](crate::codec::MAX_FRAME) (1 MiB). So the log is capped
//! below that, at [`MAX_LOG_BYTES`], with room left for the rest. A report
//! over any limit is refused whole, saying which part and by how much:
//! nothing is cut on the way, so what the admin reads is what the app
//! sent. The app keeps the newest lines when it trims its own log.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The most bytes of `log` a report carries: 960 KiB.
pub const MAX_LOG_BYTES: usize = 960 * 1024;

/// The most bytes of `note` a report carries: 8 KiB.
pub const MAX_NOTE_BYTES: usize = 8 * 1024;

/// The most bytes of each of a report's names and versions.
pub const MAX_FIELD_BYTES: usize = 256;

/// What an app sends (`SendDiagnostics`): what it runs on, its playback
/// log, and what the user wrote. Its `Debug` gives the log's length, not
/// the log.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct DiagnosticsReport {
    /// The device's model, as the platform names it (`iPhone16,2`,
    /// `AppleTV14,1`).
    pub device_model: String,
    /// The operating system (`iOS`, `tvOS`, `Android`).
    pub os_name: String,
    pub os_version: String,
    /// The app (`Spoolframe`).
    pub app_name: String,
    /// The app's version and build (`1.4.0 (212)`).
    pub app_version: String,
    /// The app's playback log, as it keeps it: free-form text.
    pub log: String,
    /// What the user wrote about it, if anything.
    pub note: Option<String>,
}

/// A text's length, for `Debug` in place of the text.
struct Bytes(usize);

impl fmt::Debug for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{} bytes>", self.0)
    }
}

impl fmt::Debug for DiagnosticsReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DiagnosticsReport")
            .field("device_model", &self.device_model)
            .field("os_name", &self.os_name)
            .field("os_version", &self.os_version)
            .field("app_name", &self.app_name)
            .field("app_version", &self.app_version)
            .field("log", &Bytes(self.log.len()))
            .field("note", &self.note.as_ref().map(|n| Bytes(n.len())))
            .finish()
    }
}

impl DiagnosticsReport {
    /// Why the report cannot be sent, if it cannot: a part over its limit,
    /// named, with its size and the limit. What the server refuses with
    /// `InvalidInput`, and the SDK before sending.
    pub fn refusal(&self) -> Option<String> {
        let parts = [
            ("device_model", self.device_model.len(), MAX_FIELD_BYTES),
            ("os_name", self.os_name.len(), MAX_FIELD_BYTES),
            ("os_version", self.os_version.len(), MAX_FIELD_BYTES),
            ("app_name", self.app_name.len(), MAX_FIELD_BYTES),
            ("app_version", self.app_version.len(), MAX_FIELD_BYTES),
            ("log", self.log.len(), MAX_LOG_BYTES),
            (
                "note",
                self.note.as_ref().map_or(0, String::len),
                MAX_NOTE_BYTES,
            ),
        ];
        parts.into_iter().find(|&(_, len, max)| len > max).map(|(name, len, max)| {
            format!(
                "the diagnostics' {name} is {len} bytes; at most {max} are sent: keep the newest"
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{MAX_FRAME, encode};
    use crate::rpc::{Operation, RpcRequest};
    use crate::{Request, RequestEnvelope, SessionId};

    fn report(log: String) -> DiagnosticsReport {
        DiagnosticsReport {
            device_model: "iPhone16,2".into(),
            os_name: "iOS".into(),
            os_version: "26.1".into(),
            app_name: "Spoolframe".into(),
            app_version: "1.4.0 (212)".into(),
            log,
            note: Some("It froze at the credits".into()),
        }
    }

    #[test]
    fn debug_gives_the_log_s_length_not_the_log() {
        let text = format!("{:?}", report("secret line\n".repeat(3)));
        assert!(!text.contains("secret line"), "{text}");
        assert!(text.contains("<36 bytes>"), "{text}");
        assert!(text.contains("iPhone16,2"), "{text}");
    }

    #[test]
    fn a_log_at_the_cap_is_sent() {
        assert_eq!(report("a".repeat(MAX_LOG_BYTES)).refusal(), None);
    }

    #[test]
    fn a_log_over_the_cap_is_refused_naming_the_log() {
        let refusal = report("a".repeat(MAX_LOG_BYTES + 1)).refusal().unwrap();
        assert!(refusal.contains("log is 983041 bytes"), "{refusal}");
    }

    #[test]
    fn a_long_note_or_name_is_refused() {
        let mut long_note = report(String::new());
        long_note.note = Some("n".repeat(MAX_NOTE_BYTES + 1));
        assert!(long_note.refusal().unwrap().contains("note"));
        let mut long_model = report(String::new());
        long_model.device_model = "m".repeat(MAX_FIELD_BYTES + 1);
        assert!(long_model.refusal().unwrap().contains("device_model"));
    }

    /// Every part at its limit still fits in one request frame, with its
    /// session and operation: the limits are below the frame's.
    #[test]
    fn a_report_at_every_limit_fits_one_frame() {
        let field = "f".repeat(MAX_FIELD_BYTES);
        let full = DiagnosticsReport {
            device_model: field.clone(),
            os_name: field.clone(),
            os_version: field.clone(),
            app_name: field.clone(),
            app_version: field,
            log: "l".repeat(MAX_LOG_BYTES),
            note: Some("n".repeat(MAX_NOTE_BYTES)),
        };
        assert_eq!(full.refusal(), None);
        let wire = RpcRequest {
            envelope: RequestEnvelope {
                session: Some(SessionId("0".repeat(64))),
                request: Request::SendDiagnostics { report: full },
            },
            operation: Some(Operation {
                id: "0".repeat(64),
                instance: "0".repeat(64),
                expires_at_ms: u64::MAX,
            }),
            expected_revision: Some(u64::MAX),
        };
        let len = encode(&wire).unwrap().len();
        assert!(len <= MAX_FRAME as usize, "{len} bytes");
    }
}
