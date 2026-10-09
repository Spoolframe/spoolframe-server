//! What Settings › Telemetry decides (P3-14), apart from the tab: the
//! addresses `/metrics` answers, the OTLP headers as the setting writes
//! them, and how the export is going in words. Host-compiled, so `cargo
//! test` covers it.

use std::net::IpAddr;

use spoolframe_admin_api::{OtlpStatus, RefusedScrape};

use crate::activity::since;
use crate::logic::{host_port, thousands};

/// Loopback, which the tab keeps in `admin.metrics_from` so a scraper on
/// the server's own machine always works.
pub const LOOPBACK: [&str; 2] = ["127.0.0.0/8", "::1"];

/// An address or a CIDR range, as `admin.metrics_from` takes it, written
/// back the way it was typed less its spaces; `Err` says what is wrong.
pub fn check_range(typed: &str) -> Result<String, String> {
    let text = typed.trim();
    let bad = || {
        format!("{text:?} is not an address or a CIDR range, such as 10.10.0.2 or 10.10.0.0/24.")
    };
    if text.is_empty() {
        return Err("Type an address, such as 10.10.0.2, or a range, such as 10.10.0.0/24.".into());
    }
    let (ip, prefix) = match text.split_once('/') {
        Some((ip, prefix)) => (ip.trim(), Some(prefix.trim())),
        None => (text, None),
    };
    let ip: IpAddr = ip.parse().map_err(|_| bad())?;
    let most = if ip.is_ipv4() { 32 } else { 128 };
    match prefix {
        None => Ok(ip.to_string()),
        Some(p) => match p.parse::<u8>() {
            Ok(n) if n <= most => Ok(format!("{ip}/{n}")),
            _ => Err(bad()),
        },
    }
}

/// Whether `entry` is loopback, which the tab shows without a way to
/// remove it.
pub fn is_loopback(entry: &str) -> bool {
    let ip = entry.split('/').next().unwrap_or(entry).trim();
    ip.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// The addresses `/metrics` answers, from `admin.metrics_from` (`None`
/// when nothing sets it: loopback only), loopback first, and added when
/// the setting left it out.
pub fn allowed_from(value: Option<&[String]>) -> Vec<String> {
    let listed: Vec<String> = value
        .unwrap_or_default()
        .iter()
        .map(|e| e.trim())
        .filter(|e| !e.is_empty())
        .map(str::to_owned)
        .collect();
    let mut list: Vec<String> = listed.iter().filter(|e| is_loopback(e)).cloned().collect();
    for fixed in LOOPBACK {
        let v4 = fixed.contains('.');
        if !list.iter().any(|e| e.contains('.') == v4) {
            list.push(fixed.to_owned());
        }
    }
    list.extend(listed.into_iter().filter(|e| !is_loopback(e)));
    list
}

/// `list` with `typed` added; `Err` says why it was not.
pub fn add_address(list: &[String], typed: &str) -> Result<Vec<String>, String> {
    let entry = check_range(typed)?;
    if list.contains(&entry) {
        return Err(format!("{entry} is allowed already."));
    }
    if is_loopback(&entry) {
        return Err("Loopback is always allowed.".into());
    }
    let mut list = list.to_vec();
    list.push(entry);
    Ok(list)
}

/// `list` without `entry`; loopback stays.
pub fn remove_address(list: &[String], entry: &str) -> Vec<String> {
    list.iter()
        .filter(|e| e.as_str() != entry || is_loopback(e))
        .cloned()
        .collect()
}

/// The setting for `list`: `None` for loopback alone, which is what the
/// server does with nothing set.
pub fn metrics_from_value(list: &[String]) -> Option<Vec<String>> {
    if list.iter().all(|e| is_loopback(e)) {
        return None;
    }
    Some(list.to_vec())
}

/// The refused addresses the tab offers to allow: those not in `list`.
pub fn refused_to_offer<'r>(
    refused: &'r [RefusedScrape],
    list: &[String],
) -> Vec<&'r RefusedScrape> {
    refused
        .iter()
        .filter(|r| !list.contains(&r.address))
        .collect()
}

/// "3 scrapes refused from 10.10.0.9 in the last hour".
pub fn refused_line(refused: &RefusedScrape) -> String {
    let scrapes = match refused.count {
        1 => "1 scrape".to_owned(),
        n => format!("{} scrapes", thousands(n)),
    };
    format!(
        "{scrapes} refused from {} in the last hour",
        refused.address
    )
}

/// One OTLP header being typed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeaderRow {
    pub name: String,
    pub value: String,
}

/// `telemetry.otlp.headers` for `rows`, a table of name to value: `None`
/// with none, which takes the setting out. `Err` names the header that
/// cannot be written, never its value.
pub fn headers_value(
    rows: &[HeaderRow],
) -> Result<Option<serde_json::Map<String, serde_json::Value>>, String> {
    let token = |c: char| c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c);
    let mut table = serde_json::Map::new();
    for row in rows {
        let name = row.name.trim();
        let value = row.value.trim();
        if name.is_empty() && value.is_empty() {
            continue;
        }
        if name.is_empty() || !name.chars().all(token) {
            return Err(format!(
                "{name:?} is not a header name: letters, digits and - only, such as Authorization."
            ));
        }
        if value.chars().any(char::is_control) {
            return Err(format!(
                "The value of {name} cannot have a control character."
            ));
        }
        // A table has one value per name, and HTTP names ignore case.
        if table.keys().any(|k: &String| k.eq_ignore_ascii_case(name)) {
            return Err(format!("{name} is there twice: keep one."));
        }
        table.insert(name.to_owned(), value.into());
    }
    Ok((!table.is_empty()).then_some(table))
}

/// How the OTLP export is going.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportState {
    /// No endpoint: nothing leaves the server.
    Off,
    /// On, and nothing has gone through or failed yet.
    Starting,
    /// Going through; `dropped` since the queue was last full.
    Exporting { last_at: i64, dropped: u64 },
    /// The collector answered no: its credentials, most likely.
    Rejected { error: String },
    /// The collector does not answer, or the export fails otherwise.
    Failing {
        host: String,
        unreachable: bool,
        error: String,
        dropped: u64,
        since: Option<i64>,
        every_secs: u32,
    },
}

/// What a failure's text says about the collector having refused the
/// data, by what HTTP and gRPC call a refusal of credentials.
fn refused_credentials(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    [
        "401",
        "403",
        "unauthorized",
        "unauthenticated",
        "forbidden",
        "permissiondenied",
        "permission denied",
    ]
    .iter()
    .any(|word| lower.contains(word))
}

/// Whether a failure's text says the collector was not reached.
fn not_reached(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    [
        "connect",
        "refused",
        "unreachable",
        "unavailable",
        "timed out",
        "no answer",
        "dns",
        "resolve",
        "error sending request",
    ]
    .iter()
    .any(|word| lower.contains(word))
}

/// The export's state from its status: failing when its last failure is
/// newer than its last export.
pub fn export_state(otlp: Option<&OtlpStatus>) -> ExportState {
    let Some(o) = otlp else {
        return ExportState::Off;
    };
    let failing = match (o.last_error_at, o.last_export_at) {
        (Some(failed), Some(went)) => failed > went,
        (Some(_), None) => true,
        (None, _) => false,
    };
    if failing {
        let error = o.last_error.clone().unwrap_or_default();
        if refused_credentials(&error) {
            return ExportState::Rejected { error };
        }
        return ExportState::Failing {
            host: host_port(&o.endpoint).to_owned(),
            unreachable: not_reached(&error),
            error,
            dropped: o.dropped,
            since: o.dropping_since,
            every_secs: o.metrics_every_secs,
        };
    }
    match o.last_export_at {
        Some(last_at) => ExportState::Exporting {
            last_at,
            dropped: o.dropped,
        },
        None => ExportState::Starting,
    }
}

/// "Exporting · last batch 3 s ago · 0 dropped".
pub fn exporting_line(now_ms: i64, last_at: i64, dropped: u64) -> String {
    format!(
        "Exporting · last batch {} · {} dropped",
        since(now_ms, last_at),
        thousands(dropped)
    )
}

/// The export in a few words, for the Activity page's card, and whether
/// it is in trouble, which the card shows in red.
pub fn export_summary(state: &ExportState, now_ms: i64) -> (String, bool) {
    match state {
        ExportState::Off => ("off · nothing is exported".into(), false),
        ExportState::Starting => ("on · nothing exported yet".into(), false),
        ExportState::Exporting { last_at, .. } => (
            format!("on · last batch {}", since(now_ms, *last_at)),
            false,
        ),
        ExportState::Rejected { .. } => ("refused by the collector".into(), true),
        ExportState::Failing { dropped, .. } => {
            (format!("failing · {} dropped", thousands(*dropped)), true)
        }
    }
}

/// What a failing export costs and what happens next: "1,204 dropped
/// since 18:20. Trying again every 30 s." `since` is the time it began
/// dropping, as the page shows it.
pub fn dropping_line(dropped: u64, since: Option<&str>, every_secs: u32) -> String {
    let lost = match (dropped, since) {
        (0, _) => "Nothing dropped yet.".to_owned(),
        (n, Some(at)) => format!("{} dropped since {at}.", thousands(n)),
        (n, None) => format!("{} dropped.", thousands(n)),
    };
    format!("{lost} Trying again every {every_secs} s.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn an_address_or_a_range_is_taken_and_anything_else_is_not() {
        assert_eq!(check_range(" 10.10.0.2 "), Ok("10.10.0.2".into()));
        assert_eq!(check_range("10.10.0.0 / 24"), Ok("10.10.0.0/24".into()));
        assert_eq!(check_range("fd00::/8"), Ok("fd00::/8".into()));
        for bad in [
            "prometheus.lan",
            "10.10.0.0/33",
            "fd00::/129",
            "10.10.0/24",
            "1.2.3.4/x",
        ] {
            let e = check_range(bad).unwrap_err();
            assert!(e.contains("is not an address"), "{bad}: {e}");
        }
        assert!(
            check_range("  ")
                .unwrap_err()
                .starts_with("Type an address")
        );
    }

    #[test]
    fn nothing_set_is_loopback_alone() {
        assert_eq!(allowed_from(None), strings(&["127.0.0.0/8", "::1"]));
        assert_eq!(metrics_from_value(&allowed_from(None)), None);
    }

    #[test]
    fn loopback_comes_first_and_is_put_back_when_left_out() {
        assert_eq!(
            allowed_from(Some(&strings(&["10.10.0.2", "::1", "127.0.0.1"]))),
            strings(&["::1", "127.0.0.1", "10.10.0.2"])
        );
        assert_eq!(
            allowed_from(Some(&strings(&["10.10.0.2"]))),
            strings(&["127.0.0.0/8", "::1", "10.10.0.2"])
        );
    }

    #[test]
    fn an_added_address_is_saved_with_loopback() {
        let list = add_address(&allowed_from(None), "10.10.0.2").unwrap();
        assert_eq!(
            metrics_from_value(&list),
            Some(strings(&["127.0.0.0/8", "::1", "10.10.0.2"]))
        );
        assert_eq!(
            add_address(&list, "10.10.0.2").unwrap_err(),
            "10.10.0.2 is allowed already."
        );
        assert_eq!(
            add_address(&list, "127.0.0.2").unwrap_err(),
            "Loopback is always allowed."
        );
    }

    #[test]
    fn removing_an_address_leaves_loopback() {
        let list = strings(&["127.0.0.0/8", "::1", "10.10.0.2"]);
        assert_eq!(
            remove_address(&list, "10.10.0.2"),
            strings(&["127.0.0.0/8", "::1"])
        );
        assert_eq!(remove_address(&list, "::1"), list);
    }

    fn refused(address: &str, count: u64) -> RefusedScrape {
        RefusedScrape {
            address: address.into(),
            count,
            last_at: 0,
        }
    }

    #[test]
    fn a_refused_address_is_offered_until_it_is_allowed() {
        let all = [refused("10.10.0.9", 3), refused("10.10.0.2", 1)];
        let list = strings(&["127.0.0.0/8", "::1", "10.10.0.2"]);
        let offered = refused_to_offer(&all, &list);
        assert_eq!(offered.len(), 1);
        assert_eq!(
            refused_line(offered[0]),
            "3 scrapes refused from 10.10.0.9 in the last hour"
        );
        assert_eq!(
            refused_line(&all[1]),
            "1 scrape refused from 10.10.0.2 in the last hour"
        );
    }

    fn header(name: &str, value: &str) -> HeaderRow {
        HeaderRow {
            name: name.into(),
            value: value.into(),
        }
    }

    #[test]
    fn headers_are_written_as_the_setting_reads_them() {
        assert_eq!(
            headers_value(&[
                header(" Authorization ", " Bearer k "),
                header("", ""),
                header("x-scope-orgid", "home"),
            ]),
            Ok(Some(
                serde_json::json!({ "Authorization": "Bearer k", "x-scope-orgid": "home" })
                    .as_object()
                    .unwrap()
                    .clone()
            ))
        );
        // A comma was refused only by the comma-separated string it was.
        assert_eq!(
            headers_value(&[header("Accept", "a, b")]),
            Ok(Some(
                serde_json::json!({ "Accept": "a, b" })
                    .as_object()
                    .unwrap()
                    .clone()
            ))
        );
        assert_eq!(headers_value(&[]), Ok(None));
        assert_eq!(headers_value(&[header("", "")]), Ok(None));
    }

    #[test]
    fn a_header_that_cannot_be_written_is_refused_without_its_value() {
        let e = headers_value(&[header("bad name", "s3cr3t")]).unwrap_err();
        assert!(e.starts_with("\"bad name\" is not a header name"), "{e}");
        assert!(!e.contains("s3cr3t"));
        let e = headers_value(&[header("", "s3cr3t")]).unwrap_err();
        assert!(!e.contains("s3cr3t"), "{e}");
        let e = headers_value(&[header("Authorization", "s3\u{7}cr3t")]).unwrap_err();
        assert_eq!(
            e,
            "The value of Authorization cannot have a control character."
        );
        let e = headers_value(&[
            header("Authorization", "s3cr3t"),
            header("authorization", "s3cr3t"),
        ])
        .unwrap_err();
        assert_eq!(e, "authorization is there twice: keep one.");
    }

    fn status() -> OtlpStatus {
        OtlpStatus {
            endpoint: "http://alloy.home.arpa:4317".into(),
            protocol: "grpc".into(),
            traces: true,
            metrics: true,
            logs: true,
            header_names: vec![],
            last_export_at: None,
            dropped: 0,
            dropping_since: None,
            last_error: None,
            last_error_at: None,
            metrics_every_secs: 30,
        }
    }

    #[test]
    fn no_endpoint_is_off_and_a_new_one_is_starting() {
        assert_eq!(export_state(None), ExportState::Off);
        assert_eq!(export_state(Some(&status())), ExportState::Starting);
    }

    #[test]
    fn an_export_newer_than_the_last_failure_is_exporting() {
        let s = OtlpStatus {
            last_export_at: Some(2_000),
            last_error: Some("traces: timed out".into()),
            last_error_at: Some(1_000),
            ..status()
        };
        assert_eq!(
            export_state(Some(&s)),
            ExportState::Exporting {
                last_at: 2_000,
                dropped: 0
            }
        );
        assert_eq!(
            exporting_line(5_400, 2_000, 0),
            "Exporting · last batch 3 s ago · 0 dropped"
        );
    }

    #[test]
    fn a_collector_that_does_not_answer_is_unreachable_and_dropping() {
        let s = OtlpStatus {
            last_export_at: Some(1_000),
            last_error: Some("traces: no answer within 10s".into()),
            last_error_at: Some(2_000),
            dropped: 1_204,
            dropping_since: Some(1_500),
            ..status()
        };
        assert_eq!(
            export_state(Some(&s)),
            ExportState::Failing {
                host: "alloy.home.arpa:4317".into(),
                unreachable: true,
                error: "traces: no answer within 10s".into(),
                dropped: 1_204,
                since: Some(1_500),
                every_secs: 30,
            }
        );
        assert_eq!(
            dropping_line(1_204, Some("18:20"), 30),
            "1,204 dropped since 18:20. Trying again every 30 s."
        );
        assert_eq!(
            dropping_line(0, None, 30),
            "Nothing dropped yet. Trying again every 30 s."
        );
    }

    #[test]
    fn the_activity_card_says_the_export_in_a_few_words() {
        assert_eq!(
            export_summary(&ExportState::Off, 0),
            ("off · nothing is exported".into(), false)
        );
        assert_eq!(
            export_summary(
                &ExportState::Exporting {
                    last_at: 1_000,
                    dropped: 0
                },
                4_000
            ),
            ("on · last batch 3 s ago".into(), false)
        );
        let failing = export_state(Some(&OtlpStatus {
            last_error: Some("traces: tcp connect error".into()),
            last_error_at: Some(1),
            dropped: 1_204,
            ..status()
        }));
        assert_eq!(
            export_summary(&failing, 0),
            ("failing · 1,204 dropped".into(), true)
        );
    }

    #[test]
    fn a_refusal_of_credentials_is_told_apart() {
        for error in [
            "logs: HTTP export failed. Status: 401 Unauthorized",
            "traces: status: Unauthenticated, message: \"bad token\"",
            "metrics: status: PermissionDenied",
        ] {
            let s = OtlpStatus {
                last_error: Some(error.into()),
                last_error_at: Some(1),
                ..status()
            };
            assert_eq!(
                export_state(Some(&s)),
                ExportState::Rejected {
                    error: error.into()
                },
                "{error}"
            );
        }
    }

    #[test]
    fn another_failure_is_failing_but_not_unreachable() {
        let s = OtlpStatus {
            last_error: Some("traces: HTTP export failed. Status: 404 Not Found".into()),
            last_error_at: Some(1),
            ..status()
        };
        assert!(matches!(
            export_state(Some(&s)),
            ExportState::Failing {
                unreachable: false,
                ..
            }
        ));
    }
}
