//! What Server › Logs decides (P3-14), apart from the page: which lines
//! it asks for, following new ones by the server's cursor, holding them
//! back while paused, and the text Download saves. Host-compiled, so
//! `cargo test` covers it.

use spoolframe_admin_api::{LogLevel, LogLine, LogPage, LogQuery};

use crate::logic::{date, thousands};

/// The level control: each the least severe level it shows.
pub const LEVELS: [(LogLevel, &str); 4] = [
    (LogLevel::Debug, "DEBUG+"),
    (LogLevel::Info, "INFO+"),
    (LogLevel::Warn, "WARN+"),
    (LogLevel::Error, "ERROR"),
];

/// How many lines one question asks for; more are asked for at once
/// when a page comes back full.
pub const PAGE: u32 = 500;

/// The most lines the page keeps when the server does not say how many it
/// keeps, and the most it keeps however many that is.
const DEFAULT_KEPT: usize = 5_000;
const MOST_KEPT: usize = 20_000;

/// A level as a line shows it: "WARN".
pub fn level_name(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Error => "ERROR",
        LogLevel::Warn => "WARN",
        LogLevel::Info => "INFO",
        LogLevel::Debug => "DEBUG",
        LogLevel::Trace => "TRACE",
    }
}

/// Which lines are shown: at least this severe, and containing this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogFilter {
    pub level: LogLevel,
    pub text: String,
}

/// The lines the page has, and where it is in the server's.
#[derive(Debug, Clone, PartialEq)]
pub struct LogView {
    filter: LogFilter,
    shown: Vec<LogLine>,
    /// New lines that came while paused, shown at Follow.
    held: Vec<LogLine>,
    /// The cursor to ask `after` next; `None` before the first answer.
    next: Option<u64>,
    /// Lines came faster than the server keeps them, and some were gone
    /// before the page asked.
    missed: bool,
    capacity: Option<u32>,
    following: bool,
}

impl LogView {
    /// Nothing yet, for `filter`.
    pub fn new(filter: LogFilter, following: bool) -> Self {
        Self {
            filter,
            shown: Vec::new(),
            held: Vec::new(),
            next: None,
            missed: false,
            capacity: None,
            following,
        }
    }

    pub fn filter(&self) -> &LogFilter {
        &self.filter
    }

    pub fn shown(&self) -> &[LogLine] {
        &self.shown
    }

    pub fn following(&self) -> bool {
        self.following
    }

    pub fn missed(&self) -> bool {
        self.missed
    }

    /// Whether the server has answered yet.
    pub fn loaded(&self) -> bool {
        self.next.is_some()
    }

    /// What to ask next: the last lines at first, then those after the
    /// cursor, with the filter the server applies.
    pub fn query(&self) -> LogQuery {
        let text = self.filter.text.trim();
        LogQuery {
            after: self.next,
            level: Some(self.filter.level),
            text: (!text.is_empty()).then(|| text.to_owned()),
            limit: Some(PAGE),
        }
    }

    /// Whether the answer to `asked` changes anything: an empty page at
    /// the same cursor does not, and the page need not be drawn again.
    pub fn is_news(&self, asked: &LogQuery, page: &LogPage) -> bool {
        *asked == self.query()
            && (self.next != Some(page.next)
                || !page.lines.is_empty()
                || self.capacity != Some(page.capacity)
                || (page.missed && !self.missed))
    }

    /// Take the answer to `asked`. An answer to a question this view no
    /// longer asks (the filter changed, or another answer moved the
    /// cursor) is dropped. `true` when the page came back full, so more
    /// may be waiting.
    pub fn take(&mut self, asked: &LogQuery, page: LogPage) -> bool {
        if *asked != self.query() {
            return false;
        }
        let first = self.next.is_none();
        let full = page.lines.len() >= PAGE as usize;
        self.next = Some(page.next);
        self.capacity = Some(page.capacity);
        // The first page is the last lines: nothing was asked before it.
        if page.missed && !first {
            self.missed = true;
        }
        if first || self.following {
            self.shown.extend(page.lines);
        } else {
            self.held.extend(page.lines);
        }
        self.trim();
        full
    }

    pub fn pause(&mut self) {
        self.following = false;
    }

    /// Follow again: what came while paused is shown at once.
    pub fn follow(&mut self) {
        self.following = true;
        self.shown.append(&mut self.held);
        self.trim();
    }

    /// The page keeps what the server keeps, the newest.
    fn kept(&self) -> usize {
        self.capacity
            .map_or(DEFAULT_KEPT, |c| c as usize)
            .clamp(PAGE as usize, MOST_KEPT)
    }

    fn trim(&mut self) {
        let kept = self.kept();
        for lines in [&mut self.shown, &mut self.held] {
            if lines.len() > kept {
                lines.drain(..lines.len() - kept);
            }
        }
    }

    /// The line beside Pause: "Following new lines", "Paused · 4 new lines".
    pub fn follow_line(&self) -> String {
        if self.following {
            "Following new lines".to_owned()
        } else {
            format!("Paused · {}", new_lines(self.held.len()))
        }
    }

    /// The line under the last one.
    pub fn tail_line(&self) -> String {
        match (self.following, self.held.len()) {
            (true, _) => "▌ waiting for new lines…".to_owned(),
            (false, 0) => "Paused · Follow to show new lines as they come".to_owned(),
            (false, n) => format!("{} while paused · Follow to show them", new_lines(n)),
        }
    }

    /// "212 lines shown · the server keeps 5,000".
    pub fn count_line(&self) -> String {
        let shown = match self.shown.len() {
            1 => "1 line shown".to_owned(),
            n => format!("{} lines shown", thousands(n as u64)),
        };
        match self.capacity {
            Some(c) => format!("{shown} · the server keeps {}", thousands(u64::from(c))),
            None => shown,
        }
    }

    /// The lines shown as Download saves them: one a line, its time in
    /// UTC, so a bug report reads the same anywhere.
    pub fn download_text(&self) -> String {
        let mut text = String::new();
        for line in &self.shown {
            text.push_str(&format!(
                "{} {:<5} {}: {}\n",
                stamp(line.at),
                level_name(line.level),
                line.target,
                line.text
            ));
        }
        text
    }
}

fn new_lines(n: usize) -> String {
    match n {
        1 => "1 new line".to_owned(),
        n => format!("{} new lines", thousands(n as u64)),
    }
}

/// Where in the server a line comes from, as its row shows it: the last
/// part of its target, `spoolframe_server::playback` as `playback`.
pub fn short_target(target: &str) -> &str {
    target.rsplit("::").next().unwrap_or(target)
}

/// The hours, minutes, seconds and milliseconds of the day of `ms`.
fn time_of_day(ms: i64) -> (i64, i64, i64, i64) {
    let ms = ms.rem_euclid(86_400_000);
    (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

/// A line's time as its row shows it, `18:42:07.114`, from `local_ms`:
/// Unix milliseconds moved by the browser's offset from UTC.
pub fn clock(local_ms: i64) -> String {
    let (h, m, s, ms) = time_of_day(local_ms);
    format!("{h:02}:{m:02}:{s:02}.{ms:03}")
}

/// `18:20`, from `local_ms` as for [`clock`].
pub fn hours_minutes(local_ms: i64) -> String {
    let (h, m, _, _) = time_of_day(local_ms);
    format!("{h:02}:{m:02}")
}

/// Unix milliseconds as RFC 3339 in UTC: `2026-10-05T18:42:07.114Z`.
pub fn stamp(at_ms: i64) -> String {
    format!("{}T{}Z", date(at_ms.div_euclid(1000)), clock(at_ms))
}

/// What Download names its file, by when: `spoolframe-log-2026-10-05-184207.txt`.
pub fn download_name(at_ms: i64) -> String {
    let (h, m, s, _) = time_of_day(at_ms);
    format!(
        "spoolframe-log-{}-{h:02}{m:02}{s:02}.txt",
        date(at_ms.div_euclid(1000))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-05T18:42:07.114Z.
    const AT: i64 = 1_791_225_727_114;

    fn line(seq: u64, level: LogLevel, text: &str) -> LogLine {
        LogLine {
            seq,
            at: AT + seq as i64,
            level,
            target: "spoolframe_server::jobs".into(),
            text: text.into(),
        }
    }

    fn page(lines: Vec<LogLine>, next: u64) -> LogPage {
        LogPage {
            lines,
            next,
            missed: false,
            capacity: 5_000,
        }
    }

    fn info() -> LogFilter {
        LogFilter {
            level: LogLevel::Info,
            text: String::new(),
        }
    }

    /// The view after its first answer, `n` lines up to cursor `n`.
    fn loaded(following: bool, n: u64) -> LogView {
        let mut view = LogView::new(info(), following);
        let asked = view.query();
        let lines = (1..=n).map(|s| line(s, LogLevel::Info, "x")).collect();
        view.take(&asked, page(lines, n));
        view
    }

    #[test]
    fn the_first_question_is_for_the_last_lines_and_then_after_the_cursor() {
        let mut view = LogView::new(
            LogFilter {
                level: LogLevel::Warn,
                text: "  plex ".into(),
            },
            true,
        );
        let first = view.query();
        assert_eq!(
            first,
            LogQuery {
                after: None,
                level: Some(LogLevel::Warn),
                text: Some("plex".into()),
                limit: Some(PAGE),
            }
        );
        view.take(&first, page(vec![line(7, LogLevel::Warn, "a")], 7));
        assert_eq!(view.query().after, Some(7));
    }

    #[test]
    fn no_text_is_no_text_filter() {
        assert_eq!(LogView::new(info(), true).query().text, None);
    }

    #[test]
    fn following_new_lines_adds_them_after_the_others() {
        let mut view = loaded(true, 2);
        let asked = view.query();
        view.take(&asked, page(vec![line(3, LogLevel::Info, "new")], 3));
        let seqs: Vec<u64> = view.shown().iter().map(|l| l.seq).collect();
        assert_eq!(seqs, [1, 2, 3]);
        assert_eq!(view.follow_line(), "Following new lines");
    }

    #[test]
    fn while_paused_new_lines_are_counted_and_shown_at_follow() {
        let mut view = loaded(false, 2);
        let asked = view.query();
        let lines = (3..=6).map(|s| line(s, LogLevel::Info, "x")).collect();
        view.take(&asked, page(lines, 6));
        assert_eq!(view.shown().len(), 2);
        assert_eq!(view.follow_line(), "Paused · 4 new lines");
        assert_eq!(
            view.tail_line(),
            "4 new lines while paused · Follow to show them"
        );
        view.follow();
        assert_eq!(view.shown().len(), 6);
        assert_eq!(view.follow_line(), "Following new lines");
    }

    #[test]
    fn the_first_answer_is_shown_even_while_paused() {
        let view = loaded(false, 3);
        assert_eq!(view.shown().len(), 3);
        assert_eq!(view.follow_line(), "Paused · 0 new lines");
    }

    #[test]
    fn an_answer_to_an_old_question_is_dropped() {
        let mut view = loaded(true, 2);
        let asked = view.query();
        view.take(&asked, page(vec![line(3, LogLevel::Info, "x")], 3));
        // A second answer to the same question: the cursor has moved.
        assert!(!view.take(&asked, page(vec![line(3, LogLevel::Info, "x")], 3)));
        assert_eq!(view.shown().len(), 3);
    }

    #[test]
    fn an_empty_page_at_the_same_cursor_is_no_news() {
        let view = loaded(true, 2);
        let asked = view.query();
        assert!(!view.is_news(&asked, &page(vec![], 2)));
        assert!(view.is_news(&asked, &page(vec![line(3, LogLevel::Info, "x")], 3)));
        // Lines that did not pass the filter still move the cursor.
        assert!(view.is_news(&asked, &page(vec![], 9)));
        let fresh = LogView::new(info(), true);
        assert!(fresh.is_news(&fresh.query(), &page(vec![], 0)));
    }

    #[test]
    fn a_full_page_says_more_may_follow() {
        let mut view = LogView::new(info(), true);
        let asked = view.query();
        let lines = (1..=u64::from(PAGE))
            .map(|s| line(s, LogLevel::Info, "x"))
            .collect();
        assert!(view.take(&asked, page(lines, u64::from(PAGE))));
        let asked = view.query();
        assert!(!view.take(&asked, page(vec![], u64::from(PAGE))));
    }

    #[test]
    fn lines_dropped_between_questions_are_said_but_not_before_the_first() {
        let mut view = LogView::new(info(), true);
        let asked = view.query();
        let mut first = page(vec![line(900, LogLevel::Info, "x")], 900);
        first.missed = true;
        view.take(&asked, first);
        assert!(!view.missed());
        let asked = view.query();
        let mut gap = page(vec![line(990, LogLevel::Info, "x")], 990);
        gap.missed = true;
        view.take(&asked, gap);
        assert!(view.missed());
    }

    #[test]
    fn the_page_keeps_as_many_lines_as_the_server_does_the_newest() {
        let mut view = LogView::new(info(), true);
        let asked = view.query();
        let mut answer = page(
            (1..=600).map(|s| line(s, LogLevel::Info, "x")).collect(),
            600,
        );
        answer.capacity = 500;
        view.take(&asked, answer);
        assert_eq!(view.shown().len(), 500);
        assert_eq!(view.shown()[0].seq, 101);
        assert_eq!(view.count_line(), "500 lines shown · the server keeps 500");
    }

    #[test]
    fn download_saves_the_lines_shown_in_utc() {
        let mut view = loaded(true, 0);
        let asked = view.query();
        view.take(
            &asked,
            page(
                vec![
                    line(0, LogLevel::Info, "conversion 7c1e started"),
                    line(1, LogLevel::Error, "playback 51b0 failed"),
                ],
                1,
            ),
        );
        assert_eq!(
            view.download_text(),
            "2026-10-05T18:42:07.114Z INFO  spoolframe_server::jobs: conversion 7c1e started\n\
             2026-10-05T18:42:07.115Z ERROR spoolframe_server::jobs: playback 51b0 failed\n"
        );
        assert_eq!(download_name(AT), "spoolframe-log-2026-10-05-184207.txt");
    }

    #[test]
    fn a_line_shows_its_time_and_the_last_part_of_its_target() {
        assert_eq!(clock(AT), "18:42:07.114");
        // Two hours east of UTC.
        assert_eq!(clock(AT + 7_200_000), "20:42:07.114");
        assert_eq!(hours_minutes(AT), "18:42");
        assert_eq!(short_target("spoolframe_server::playback"), "playback");
        assert_eq!(short_target("spoolframe_server"), "spoolframe_server");
        assert_eq!(level_name(LogLevel::Warn), "WARN");
    }
}
