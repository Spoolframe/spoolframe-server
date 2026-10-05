//! What Server › Activity decides (P3-14), apart from the page: the small
//! graphs' lines and how its numbers read. Host-compiled, so `cargo test`
//! covers it.

use spoolframe_admin_api::{ActivityPoint, PlexServerActivity, UserSessionCount};

use crate::logic::thousands;

/// What a graph covers: the last hour, as the server keeps it.
pub const WINDOW_MS: i64 = 3_600_000;

/// A small graph's line and the area under it, as SVG `points`.
#[derive(Debug, Clone, PartialEq)]
pub struct Spark {
    pub line: String,
    pub area: String,
}

/// The values of `pick` over `history`, with `now` after them when it is
/// newer than the last: `(at, value)`, oldest first.
pub fn series(
    history: &[ActivityPoint],
    now: &ActivityPoint,
    pick: impl Fn(&ActivityPoint) -> f64,
) -> Vec<(i64, f64)> {
    let mut points: Vec<(i64, f64)> = history.iter().map(|p| (p.at, pick(p))).collect();
    if points.last().is_none_or(|(at, _)| *at < now.at) {
        points.push((now.at, pick(now)));
    }
    points
}

/// A graph `width` by `height` of `points` over the hour up to `now_ms`:
/// time across, so after a restart the line starts part-way, and from 0 at
/// the bottom to the highest value near the top. `None` with fewer than
/// two points in the hour: there is no line to draw yet.
pub fn sparkline(points: &[(i64, f64)], now_ms: i64, width: f64, height: f64) -> Option<Spark> {
    // The stroke's half-width, so a line at either edge is not cut.
    const PAD: f64 = 2.0;
    let start = now_ms - WINDOW_MS;
    let shown: Vec<(i64, f64)> = points
        .iter()
        .copied()
        .filter(|(at, _)| *at >= start && *at <= now_ms)
        .collect();
    if shown.len() < 2 {
        return None;
    }
    let max = shown.iter().map(|(_, v)| *v).fold(0.0, f64::max);
    let xy: Vec<String> = shown
        .iter()
        .map(|(at, v)| {
            let x = (at - start) as f64 / WINDOW_MS as f64 * width;
            let share = if max > 0.0 { v.max(0.0) / max } else { 0.0 };
            let y = (height - PAD) - share * (height - 2.0 * PAD);
            format!("{x:.1},{y:.1}")
        })
        .collect();
    let line = xy.join(" ");
    let first_x = xy[0].split(',').next().unwrap_or("0");
    let last_x = xy[xy.len() - 1].split(',').next().unwrap_or("0");
    let area = format!("{first_x},{height} {line} {last_x},{height}");
    Some(Spark { line, area })
}

/// A count per second or minute as a card's row says it: tenths below 10,
/// whole numbers with their thousands apart above.
pub fn rate(n: f64) -> String {
    let n = n.max(0.0);
    if n == 0.0 {
        "0".to_owned()
    } else if n < 10.0 {
        let tenths = (n * 10.0).round() / 10.0;
        if tenths.fract() == 0.0 {
            format!("{tenths:.0}")
        } else {
            format!("{tenths:.1}")
        }
    } else {
        thousands(n.round() as u64)
    }
}

/// Bytes per second as the big number and its unit: "52" and "MB/s".
pub fn bytes_per_sec(n: f64) -> (String, &'static str) {
    let n = n.max(0.0);
    let (value, unit) = match n {
        n if n < 1e3 => (n, "B/s"),
        n if n < 1e6 => (n / 1e3, "KB/s"),
        n if n < 1e9 => (n / 1e6, "MB/s"),
        n => (n / 1e9, "GB/s"),
    };
    (rate(value), unit)
}

/// Bytes of memory in megabytes: "412".
pub fn megabytes(n: u64) -> String {
    thousands((n as f64 / 1e6).round() as u64)
}

/// How long the server has run: its two largest units, "6 d 4 h".
pub fn uptime(secs: u64) -> String {
    let (d, h, m) = (secs / 86_400, secs / 3600 % 24, secs / 60 % 60);
    match (d, h, m) {
        (0, 0, 0) => format!("{secs} s"),
        (0, 0, m) => format!("{m} min"),
        (0, h, m) => format!("{h} h {m} min"),
        (d, h, _) => format!("{d} d {h} h"),
    }
}

/// How fast the encodes go together: "2.4× real time", "—" with none.
pub fn output_speed(rate_secs_per_sec: f64, running: u64) -> String {
    if running == 0 {
        return "—".to_owned();
    }
    format!("{:.1}× real time", rate_secs_per_sec.max(0.0))
}

/// A share as a percentage, "94 %"; "—" when there was nothing to share.
pub fn percent(share: Option<f64>) -> String {
    match share {
        Some(s) => format!("{} %", (s.clamp(0.0, 1.0) * 100.0).round()),
        None => "—".to_owned(),
    }
}

/// One Plex server's row: "142 requests · 0 errors · mean 18 ms", and
/// whether it has errors, which the row shows in red.
pub fn plex_line(server: &PlexServerActivity) -> (String, bool) {
    let mut parts = vec![
        counted(server.requests, "request"),
        counted(server.errors, "error"),
    ];
    if let Some(ms) = server.mean_latency_ms {
        parts.push(format!("mean {} ms", ms.round()));
    }
    (parts.join(" · "), server.errors > 0)
}

/// `n` of `unit`, its thousands apart: "1 error", "1,204 requests".
fn counted(n: u64, unit: &str) -> String {
    if n == 1 {
        format!("1 {unit}")
    } else {
        format!("{} {unit}s", thousands(n))
    }
}

/// A kind of background work: "12 queued · 340 done · 2 failed".
pub fn work_line(queued: u64, done: u64, failed: u64) -> String {
    format!(
        "{} queued · {} done · {} failed",
        thousands(queued),
        thousands(done),
        thousands(failed)
    )
}

/// The Connections card's users: "Sessions · <username>" and how many,
/// the most first.
pub fn session_rows(sessions: &[UserSessionCount]) -> Vec<(String, u64)> {
    let mut rows: Vec<(String, u64)> = sessions
        .iter()
        .map(|s| (s.username.clone(), s.app + s.web))
        .filter(|(_, n)| *n > 0)
        .collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rows.into_iter()
        .map(|(name, n)| (format!("Sessions · {name}"), n))
        .collect()
}

/// How long ago `at_ms` was at `now_ms`, in the unit that fits: "2 s
/// ago", "3 min ago", "5 h ago".
pub fn since(now_ms: i64, at_ms: i64) -> String {
    let secs = (now_ms - at_ms).max(0) / 1000;
    match secs {
        0..60 => format!("{secs} s ago"),
        60..3600 => format!("{} min ago", secs / 60),
        _ => format!("{} h ago", secs / 3600),
    }
}

/// The page's live line: "Live · updated 2 s ago", or "Paused · …" while
/// it does not ask; "Live · waiting for the server" before any answer.
pub fn updated_line(paused: bool, now_ms: i64, updated_at: Option<i64>) -> String {
    let state = if paused { "Paused" } else { "Live" };
    match updated_at {
        Some(at) => format!("{state} · updated {}", since(now_ms, at)),
        None => format!("{state} · waiting for the server"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_791_000_000_000;

    fn point(at: i64, playbacks: u64) -> ActivityPoint {
        ActivityPoint {
            at,
            playbacks_direct: playbacks,
            ..ActivityPoint::default()
        }
    }

    #[test]
    fn a_graph_runs_from_the_hour_s_start_to_now_and_peaks_near_the_top() {
        let points = [
            (NOW - WINDOW_MS, 0.0),
            (NOW - WINDOW_MS / 2, 4.0),
            (NOW, 2.0),
        ];
        let spark = sparkline(&points, NOW, 220.0, 44.0).unwrap();
        assert_eq!(spark.line, "0.0,42.0 110.0,2.0 220.0,22.0");
        assert_eq!(spark.area, "0.0,44 0.0,42.0 110.0,2.0 220.0,22.0 220.0,44");
    }

    #[test]
    fn after_a_restart_the_line_starts_part_way_across() {
        // Ten minutes of samples since the server started.
        let points = [(NOW - 600_000, 1.0), (NOW, 1.0)];
        let spark = sparkline(&points, NOW, 220.0, 44.0).unwrap();
        assert!(spark.line.starts_with("183.3,"), "{}", spark.line);
    }

    #[test]
    fn nothing_but_zeros_lies_along_the_bottom() {
        let points = [(NOW - 10_000, 0.0), (NOW, 0.0)];
        let spark = sparkline(&points, NOW, 220.0, 44.0).unwrap();
        assert!(spark.line.ends_with(",42.0"), "{}", spark.line);
        assert!(spark.line.contains(",42.0 "), "{}", spark.line);
    }

    #[test]
    fn a_graph_needs_two_points_in_the_hour() {
        assert_eq!(sparkline(&[], NOW, 220.0, 44.0), None);
        assert_eq!(sparkline(&[(NOW, 3.0)], NOW, 220.0, 44.0), None);
        // One of the two is older than the hour.
        let old = [(NOW - WINDOW_MS - 1, 3.0), (NOW, 3.0)];
        assert_eq!(sparkline(&old, NOW, 220.0, 44.0), None);
    }

    #[test]
    fn the_series_ends_with_now_unless_the_history_has_it() {
        let history = [point(NOW - 20_000, 1), point(NOW - 10_000, 2)];
        let pick = |p: &ActivityPoint| p.playbacks_direct as f64;
        let newer = series(&history, &point(NOW, 3), pick);
        assert_eq!(
            newer,
            vec![(NOW - 20_000, 1.0), (NOW - 10_000, 2.0), (NOW, 3.0)]
        );
        let same = series(&history, &point(NOW - 10_000, 2), pick);
        assert_eq!(same.len(), 2);
        // Just after a restart there is no history yet.
        assert_eq!(series(&[], &point(NOW, 3), pick), vec![(NOW, 3.0)]);
    }

    #[test]
    fn rates_have_tenths_only_below_ten() {
        assert_eq!(rate(0.0), "0");
        assert_eq!(rate(0.04), "0");
        assert_eq!(rate(0.42), "0.4");
        assert_eq!(rate(3.0), "3");
        assert_eq!(rate(9.96), "10");
        assert_eq!(rate(38.4), "38");
        assert_eq!(rate(1204.0), "1,204");
        assert_eq!(rate(-1.0), "0");
    }

    #[test]
    fn bytes_per_second_take_the_unit_that_fits() {
        assert_eq!(bytes_per_sec(0.0), ("0".into(), "B/s"));
        assert_eq!(bytes_per_sec(512.0), ("512".into(), "B/s"));
        assert_eq!(bytes_per_sec(2_500.0), ("2.5".into(), "KB/s"));
        assert_eq!(bytes_per_sec(52_000_000.0), ("52".into(), "MB/s"));
        assert_eq!(bytes_per_sec(1.2e9), ("1.2".into(), "GB/s"));
    }

    #[test]
    fn uptime_reads_in_its_two_largest_units() {
        assert_eq!(uptime(40), "40 s");
        assert_eq!(uptime(12 * 60 + 5), "12 min");
        assert_eq!(uptime(3 * 3600 + 12 * 60), "3 h 12 min");
        assert_eq!(uptime(6 * 86_400 + 4 * 3600 + 59 * 60), "6 d 4 h");
    }

    #[test]
    fn the_numbers_read_as_the_board_writes_them() {
        assert_eq!(megabytes(412_300_000), "412");
        assert_eq!(output_speed(2.43, 1), "2.4× real time");
        assert_eq!(output_speed(0.0, 0), "—");
        assert_eq!(percent(Some(0.937)), "94 %");
        assert_eq!(percent(None), "—");
        assert_eq!(work_line(12, 340, 2), "12 queued · 340 done · 2 failed");
    }

    #[test]
    fn a_plex_server_with_errors_is_shown_as_bad() {
        let mut server = PlexServerActivity {
            id: "plex".into(),
            name: "Tower".into(),
            requests: 1204,
            errors: 0,
            mean_latency_ms: Some(18.4),
        };
        assert_eq!(
            plex_line(&server),
            ("1,204 requests · 0 errors · mean 18 ms".into(), false)
        );
        server.errors = 1;
        server.mean_latency_ms = None;
        assert_eq!(
            plex_line(&server),
            ("1,204 requests · 1 error".into(), true)
        );
    }

    #[test]
    fn users_with_sessions_are_listed_the_most_first() {
        let user = |name: &str, app, web| UserSessionCount {
            user_id: format!("local:{name}"),
            username: name.into(),
            app,
            web,
        };
        let rows = session_rows(&[
            user("sam", 1, 0),
            user("andrey", 2, 2),
            user("alice", 3, 0),
            user("bob", 0, 0),
            user("zed", 1, 0),
        ]);
        assert_eq!(
            rows,
            vec![
                ("Sessions · andrey".to_owned(), 4),
                ("Sessions · alice".to_owned(), 3),
                ("Sessions · sam".to_owned(), 1),
                ("Sessions · zed".to_owned(), 1),
            ]
        );
    }

    #[test]
    fn the_live_line_says_how_long_since_the_last_answer() {
        assert_eq!(
            updated_line(false, NOW, Some(NOW - 2_400)),
            "Live · updated 2 s ago"
        );
        assert_eq!(
            updated_line(true, NOW, Some(NOW - 125_000)),
            "Paused · updated 2 min ago"
        );
        assert_eq!(
            updated_line(false, NOW, None),
            "Live · waiting for the server"
        );
        assert_eq!(since(NOW, NOW - 7_300_000), "2 h ago");
        // A clock a little behind the server's.
        assert_eq!(since(NOW, NOW + 900), "0 s ago");
    }
}
