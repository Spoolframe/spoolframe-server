//! An item's subtitle files and how each is timed (PX-03), as its Debug
//! tab shows them: the words and the offset field, testable on the host.

use spoolframe_admin_api::{SubtitleSyncState, SubtitleTimingInfo};

/// A subtitle's sync as the tab says it, and its tone.
pub fn sync_text(state: &SubtitleSyncState) -> (String, &'static str) {
    match state {
        SubtitleSyncState::Original => ("Original timing".to_owned(), "cat-quiet"),
        SubtitleSyncState::Syncing => ("Syncing…".to_owned(), "state text-ctp-blue"),
        SubtitleSyncState::Synced {
            offset_ms,
            factor,
            splits,
            score,
        } => {
            let mut parts = vec![format!("{} s", seconds(*offset_ms))];
            if (factor - 1.0).abs() > 1e-6 {
                parts.push(format!("× {factor:.4}"));
            }
            if *splits > 0 {
                parts.push(format!(
                    "{splits} cut{}",
                    if *splits == 1 { "" } else { "s" }
                ));
            }
            parts.push(format!("score {score}"));
            (format!("Synced · {}", parts.join(" · ")), "state state-ok")
        }
        SubtitleSyncState::Uncertain { score } => (
            format!("Sync uncertain · score {score} · original timing kept"),
            "state state-warn",
        ),
        SubtitleSyncState::Failed { reason } => {
            (format!("Sync failed: {reason}"), "state state-bad")
        }
    }
}

/// Whether "Use original timing" changes anything: a sync is served, or
/// was found and set aside as uncertain.
pub fn has_sync(state: &SubtitleSyncState) -> bool {
    matches!(state, SubtitleSyncState::Synced { .. })
}

/// Whether the tab should ask again: a sync is running.
pub fn any_syncing(timings: &[SubtitleTimingInfo]) -> bool {
    timings.iter().any(|t| t.sync == SubtitleSyncState::Syncing)
}

/// `ms` as seconds with a sign and a tenth: `+1.5`, `-0.2`, `0.0`.
pub fn seconds(ms: i64) -> String {
    let tenths = (ms as f64 / 100.0).round() as i64;
    let sign = match tenths.signum() {
        1 => "+",
        -1 => "-",
        _ => "",
    };
    format!("{sign}{}.{}", tenths.abs() / 10, tenths.abs() % 10)
}

/// The offset field's text as ms, in tenths of a second: `1.5` or `+1.5`
/// is 1500, `-0.25` is -300 (rounded to the tenth the control steps by).
/// `None` for anything else.
pub fn offset_ms(text: &str) -> Option<i64> {
    let text = text.trim().replace(',', ".");
    let secs: f64 = text.strip_prefix('+').unwrap_or(&text).parse().ok()?;
    secs.is_finite().then(|| (secs * 10.0).round() as i64 * 100)
}

/// Who set the offset, when one is set: `by Alice`.
pub fn offset_by(t: &SubtitleTimingInfo) -> Option<String> {
    (t.offset_ms != 0).then(|| match &t.offset_by {
        Some(name) => format!("set by {name}"),
        None => "set".to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sync_says_what_it_found() {
        let (text, tone) = sync_text(&SubtitleSyncState::Synced {
            offset_ms: -3_200,
            factor: 25.0 / 23.976,
            splits: 1,
            score: 87,
        });
        assert_eq!(text, "Synced · -3.2 s · × 1.0427 · 1 cut · score 87");
        assert_eq!(tone, "state state-ok");
        let (text, _) = sync_text(&SubtitleSyncState::Synced {
            offset_ms: 1_000,
            factor: 1.0,
            splits: 0,
            score: 95,
        });
        assert_eq!(text, "Synced · +1.0 s · score 95");
        let (text, tone) = sync_text(&SubtitleSyncState::Uncertain { score: 8 });
        assert!(text.contains("original timing kept"), "{text}");
        assert_eq!(tone, "state state-warn");
    }

    #[test]
    fn the_offset_field_reads_seconds_in_tenths() {
        assert_eq!(offset_ms("1.5"), Some(1_500));
        assert_eq!(offset_ms("+1.5"), Some(1_500));
        assert_eq!(offset_ms(" -0,25 "), Some(-300));
        assert_eq!(offset_ms("0"), Some(0));
        assert_eq!(offset_ms("soon"), None);
        assert_eq!(offset_ms("inf"), None);
        assert_eq!(seconds(1_500), "+1.5");
        assert_eq!(seconds(-200), "-0.2");
        assert_eq!(seconds(0), "0.0");
    }

    #[test]
    fn the_tab_asks_again_while_a_sync_runs() {
        let mut t = SubtitleTimingInfo {
            track: 0,
            label: "Film.en.srt".into(),
            sync: SubtitleSyncState::Original,
            offset_ms: 0,
            offset_by: None,
            offset_at: None,
        };
        assert!(!any_syncing(std::slice::from_ref(&t)));
        assert_eq!(offset_by(&t), None);
        t.sync = SubtitleSyncState::Syncing;
        t.offset_ms = 1_500;
        t.offset_by = Some("Alice".into());
        assert!(any_syncing(&[t.clone()]));
        assert_eq!(offset_by(&t).as_deref(), Some("set by Alice"));
    }
}
