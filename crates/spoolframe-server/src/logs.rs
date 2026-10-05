//! The last log lines, in memory (P3-14), for the web UI's Logs page: a
//! `tracing` layer beside the one writing to stderr, under the same
//! filter, so it holds what the journal holds and nothing more.
//!
//! A page follows new lines by asking again with the cursor it was given
//! (`after`): the admin API has no subscriptions, and a poll every few
//! seconds is what the page's other numbers do too.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use spoolframe_admin_api::{LogLevel, LogLine, LogPage, LogQuery};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

/// `logs.buffer_lines` when left out.
pub const DEFAULT_LINES: usize = 5000;
/// What a page gets when it does not say.
pub const DEFAULT_PAGE: usize = 500;

/// The ring of the last lines.
#[derive(Debug)]
pub struct LogRing {
    inner: Mutex<Ring>,
}

#[derive(Debug)]
struct Ring {
    lines: VecDeque<LogLine>,
    capacity: usize,
    /// The next line's number: lines are numbered from 1, never again.
    next: u64,
}

impl Default for LogRing {
    fn default() -> Self {
        Self::new(DEFAULT_LINES)
    }
}

pub fn level_of(level: &Level) -> LogLevel {
    match *level {
        Level::ERROR => LogLevel::Error,
        Level::WARN => LogLevel::Warn,
        Level::INFO => LogLevel::Info,
        Level::DEBUG => LogLevel::Debug,
        Level::TRACE => LogLevel::Trace,
    }
}

/// How severe: lower is worse.
fn severity(level: LogLevel) -> u8 {
    match level {
        LogLevel::Error => 0,
        LogLevel::Warn => 1,
        LogLevel::Info => 2,
        LogLevel::Debug => 3,
        LogLevel::Trace => 4,
    }
}

impl LogRing {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Ring {
                lines: VecDeque::with_capacity(capacity.min(DEFAULT_LINES)),
                capacity: capacity.max(1),
                next: 1,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Ring> {
        self.inner.lock().expect("never held across a panic")
    }

    /// Keep the last `capacity` lines from now on: a smaller ring drops
    /// the oldest at once (`logs.buffer_lines` is live).
    pub fn set_capacity(&self, capacity: usize) {
        let mut ring = self.lock();
        ring.capacity = capacity.max(1);
        while ring.lines.len() > ring.capacity {
            ring.lines.pop_front();
        }
    }

    pub fn capacity(&self) -> usize {
        self.lock().capacity
    }

    pub fn push(&self, at: i64, level: LogLevel, target: &str, text: String) {
        let mut ring = self.lock();
        let seq = ring.next;
        ring.next += 1;
        if ring.lines.len() == ring.capacity {
            ring.lines.pop_front();
        }
        ring.lines.push_back(LogLine {
            seq,
            at,
            level,
            target: target.to_owned(),
            text,
        });
    }

    /// The lines `query` asks for, oldest first: after its cursor, or else
    /// the last ones; at least as severe as its level, and containing its
    /// text (any case).
    pub fn page(&self, query: &LogQuery) -> LogPage {
        let ring = self.lock();
        let limit = query
            .limit
            .map_or(DEFAULT_PAGE, |l| l as usize)
            .clamp(1, ring.capacity);
        let text = query
            .text
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_lowercase);
        let wanted = |line: &LogLine| {
            query
                .level
                .is_none_or(|level| severity(line.level) <= severity(level))
                && text.as_deref().is_none_or(|t| {
                    line.text.to_lowercase().contains(t) || line.target.to_lowercase().contains(t)
                })
        };
        let newest = ring.next - 1;
        let oldest = ring.lines.front().map_or(ring.next, |l| l.seq);
        let (lines, missed) = match query.after {
            Some(after) => {
                let lines: Vec<LogLine> = ring
                    .lines
                    .iter()
                    .filter(|l| l.seq > after)
                    .filter(|l| wanted(l))
                    .take(limit)
                    .cloned()
                    .collect();
                // Lines after the cursor that the ring no longer has.
                (lines, after + 1 < oldest && after < newest)
            }
            None => {
                let mut lines: Vec<LogLine> = ring
                    .lines
                    .iter()
                    .rev()
                    .filter(|l| wanted(l))
                    .take(limit)
                    .cloned()
                    .collect();
                lines.reverse();
                (lines, false)
            }
        };
        // Taken whole: where the next ask starts. Cut short by the limit:
        // the last line given.
        let next = match (query.after, lines.last()) {
            (Some(_), Some(last)) if lines.len() == limit => last.seq,
            _ => newest,
        };
        LogPage {
            lines,
            next,
            missed,
            capacity: ring.capacity as u32,
        }
    }
}

/// The layer that fills a [`LogRing`].
pub struct RingLayer(pub Arc<LogRing>);

impl<S> Layer<S> for RingLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let mut text = String::new();
        // Its spans, outermost first, as stderr shows them.
        if let Some(scope) = ctx.event_scope(event) {
            for span in scope.from_root() {
                let _ = write!(text, "{}: ", span.name());
            }
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        text.push_str(&fields.message);
        if !fields.rest.is_empty() {
            if !fields.message.is_empty() {
                text.push(' ');
            }
            text.push_str(&fields.rest);
        }
        let meta = event.metadata();
        self.0.push(
            crate::metrics::history::unix_ms(),
            level_of(meta.level()),
            meta.target(),
            text,
        );
    }
}

/// An event's message, and its other fields as `name=value`.
#[derive(Default)]
struct Fields {
    message: String,
    rest: String,
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            self.add(field, &format_args!("{value:?}"));
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            self.add(field, value);
        }
    }
}

impl Fields {
    fn add(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if !self.rest.is_empty() {
            self.rest.push(' ');
        }
        let _ = write!(self.rest, "{}={value:?}", field.name());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt as _;

    fn ring_of(lines: &[(LogLevel, &str)], capacity: usize) -> LogRing {
        let ring = LogRing::new(capacity);
        for (i, (level, text)) in lines.iter().enumerate() {
            ring.push(
                i as i64,
                *level,
                "spoolframe_server::test",
                (*text).to_owned(),
            );
        }
        ring
    }

    fn texts(page: &LogPage) -> Vec<&str> {
        page.lines.iter().map(|l| l.text.as_str()).collect()
    }

    #[test]
    fn the_last_lines_are_kept_and_the_buffer_stays_at_its_size() {
        let ring = LogRing::new(3);
        for i in 0..10 {
            ring.push(i, LogLevel::Info, "t", format!("line {i}"));
        }
        let page = ring.page(&LogQuery::default());
        assert_eq!(texts(&page), ["line 7", "line 8", "line 9"]);
        assert_eq!((page.next, page.capacity), (10, 3));
        ring.set_capacity(2);
        assert_eq!(
            texts(&ring.page(&LogQuery::default())),
            ["line 8", "line 9"]
        );
    }

    #[test]
    fn lines_are_filtered_by_level_and_by_text_in_any_case() {
        let ring = ring_of(
            &[
                (LogLevel::Info, "Plex catalog ready"),
                (LogLevel::Warn, "Plex server unavailable"),
                (LogLevel::Error, "the admin listener failed"),
                (LogLevel::Debug, "plex request"),
            ],
            10,
        );
        let warn = ring.page(&LogQuery {
            level: Some(LogLevel::Warn),
            ..LogQuery::default()
        });
        assert_eq!(
            texts(&warn),
            ["Plex server unavailable", "the admin listener failed"]
        );
        let plex = ring.page(&LogQuery {
            text: Some("PLEX".into()),
            level: Some(LogLevel::Info),
            ..LogQuery::default()
        });
        assert_eq!(
            texts(&plex),
            ["Plex catalog ready", "Plex server unavailable"]
        );
    }

    /// A page follows new lines by asking after its last `next`.
    #[test]
    fn a_follower_gets_each_new_line_once_and_is_told_when_it_fell_behind() {
        let ring = LogRing::new(4);
        ring.push(0, LogLevel::Info, "t", "a".into());
        let first = ring.page(&LogQuery::default());
        assert_eq!(texts(&first), ["a"]);
        let none = ring.page(&LogQuery {
            after: Some(first.next),
            ..LogQuery::default()
        });
        assert!(none.lines.is_empty() && !none.missed);
        assert_eq!(none.next, first.next);
        for text in ["b", "c"] {
            ring.push(0, LogLevel::Info, "t", text.into());
        }
        let next = ring.page(&LogQuery {
            after: Some(first.next),
            ..LogQuery::default()
        });
        assert_eq!(texts(&next), ["b", "c"]);
        // Cut short by the limit: on from the last line given.
        for text in ["d", "e", "f", "g", "h"] {
            ring.push(0, LogLevel::Info, "t", text.into());
        }
        let behind = ring.page(&LogQuery {
            after: Some(next.next),
            limit: Some(2),
            ..LogQuery::default()
        });
        assert!(behind.missed, "{behind:?}");
        assert_eq!(texts(&behind), ["e", "f"]);
        let rest = ring.page(&LogQuery {
            after: Some(behind.next),
            ..LogQuery::default()
        });
        assert_eq!(texts(&rest), ["g", "h"]);
        assert!(!rest.missed);
    }

    #[test]
    fn an_event_is_its_spans_its_message_and_its_fields() {
        let ring = Arc::new(LogRing::new(10));
        let subscriber = tracing_subscriber::registry().with(RingLayer(ring.clone()));
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("client request", request = "Items");
            let _in = span.enter();
            tracing::warn!(server = "plex", code = 3, "Plex server unavailable");
        });
        let page = ring.page(&LogQuery::default());
        assert_eq!(page.lines.len(), 1);
        let line = &page.lines[0];
        assert_eq!(line.level, LogLevel::Warn);
        assert_eq!(
            line.text,
            "client request: Plex server unavailable server=\"plex\" code=3"
        );
    }
}
