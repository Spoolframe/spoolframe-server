//! The last hour of the server's activity, in memory (P3-14): a reading of
//! the metrics every [`SAMPLE_EVERY`], for the Activity page's small
//! graphs. Nothing is written anywhere; a restart starts it again.

use parking_lot::Mutex;
use std::collections::VecDeque;
use std::time::Duration;

use spoolframe_admin_api::{ActivityPoint, ActivityTotals, MemoryPart, MemoryParts};

use super::Family;

/// How often the metrics are read.
pub const SAMPLE_EVERY: Duration = Duration::from_secs(10);
/// How far back the graphs go.
pub const KEPT: Duration = Duration::from_secs(60 * 60);

/// The numbers a point is made from: gauges as they were, counters as
/// they had got to.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reading {
    /// Unix milliseconds.
    pub at: i64,
    pub connections_app: u64,
    pub connections_web: u64,
    pub playbacks_direct: u64,
    pub playbacks_remux: u64,
    pub playbacks_transcode: u64,
    pub conversions_running: u64,
    pub conversions_paused: u64,
    pub previews_queued: u64,
    pub ocr_queued: u64,
    pub memory_bytes: Option<u64>,
    pub memory: Option<MemoryParts>,
    pub open_files: Option<u64>,
    pub output_seconds: f64,
    pub plex_requests: u64,
    pub plex_errors: u64,
    pub totals: ActivityTotals,
}

fn find<'a>(families: &'a [Family], name: &str) -> Option<&'a Family> {
    families.iter().find(|f| f.name == name)
}

fn number(families: &[Family], name: &str, labels: &[(&str, &str)]) -> Option<f64> {
    find(families, name)?.value(labels)
}

fn whole(families: &[Family], name: &str, labels: &[(&str, &str)]) -> u64 {
    number(families, name, labels).unwrap_or(0.0) as u64
}

/// The resident memory's parts, and each cache's memory tier by the name
/// the Activity page shows.
fn memory(families: &[Family]) -> Option<MemoryParts> {
    let kind = |kind| {
        number(
            families,
            "spoolframe_process_memory_bytes",
            &[("kind", kind)],
        )
    };
    let caches = find(families, "spoolframe_cache_memory_bytes").map_or_else(Vec::new, |f| {
        f.samples
            .iter()
            .filter_map(|s| {
                let (_, cache) = s.labels.iter().find(|(k, _)| *k == "cache")?;
                let super::Value::Number(bytes) = s.value else {
                    return None;
                };
                Some(MemoryPart {
                    name: cache_name(cache).to_owned(),
                    bytes: bytes as u64,
                })
            })
            .collect()
    });
    Some(MemoryParts {
        heap_bytes: kind("heap")? as u64,
        files_bytes: kind("files")? as u64,
        shared_bytes: kind("shared")? as u64,
        caches,
    })
}

/// A cache's `cache` label as the Activity page names it.
fn cache_name(label: &str) -> &str {
    match label {
        "artwork" => "Artwork",
        "previews" => "Previews",
        "plex_replies" => "Plex replies",
        other => other,
    }
}

/// Every number sample of `name` added up: a counter across its labels.
fn sum(families: &[Family], name: &str) -> u64 {
    find(families, name).map_or(0.0, Family::total) as u64
}

impl Reading {
    /// What `families` say, at `at`.
    pub fn of(families: &[Family], at: i64) -> Self {
        let f = families;
        Reading {
            at,
            connections_app: whole(f, "spoolframe_connections_open", &[("kind", "app")]),
            connections_web: whole(f, "spoolframe_connections_open", &[("kind", "web")]),
            playbacks_direct: whole(f, "spoolframe_playbacks_active", &[("plan", "direct")]),
            playbacks_remux: whole(f, "spoolframe_playbacks_active", &[("plan", "remux")]),
            playbacks_transcode: whole(f, "spoolframe_playbacks_active", &[("plan", "transcode")]),
            conversions_running: sum(f, "spoolframe_conversions_running"),
            conversions_paused: whole(f, "spoolframe_conversions_paused", &[]),
            previews_queued: sum(f, "spoolframe_previews_queued"),
            ocr_queued: whole(f, "spoolframe_ocr_queued", &[]),
            memory_bytes: number(f, "spoolframe_process_resident_memory_bytes", &[])
                .map(|v| v as u64),
            memory: memory(f),
            open_files: number(f, "spoolframe_process_open_files", &[]).map(|v| v as u64),
            output_seconds: number(f, "spoolframe_conversion_output_seconds_total", &[])
                .unwrap_or(0.0),
            plex_requests: sum(f, "spoolframe_plex_requests_total"),
            plex_errors: sum(f, "spoolframe_plex_errors_total"),
            totals: ActivityTotals {
                uptime_secs: whole(f, "spoolframe_process_uptime_seconds", &[]),
                playbacks_started: whole(f, "spoolframe_playbacks_started_total", &[]),
                playbacks_ended: whole(f, "spoolframe_playbacks_ended_total", &[]),
                playbacks_expired: whole(
                    f,
                    "spoolframe_playback_errors_total",
                    &[("error", "expired")],
                ),
                playbacks_failed: whole(
                    f,
                    "spoolframe_playback_errors_total",
                    &[("error", "failed")],
                ),
                conversions_refused: whole(f, "spoolframe_conversions_refused_total", &[]),
                conversions_stalled: whole(f, "spoolframe_conversions_stalled_total", &[]),
                media_reads: whole(f, "spoolframe_media_reads_total", &[]),
                media_read_bytes: whole(f, "spoolframe_media_read_bytes_total", &[]),
                artwork_requests: whole(f, "spoolframe_artwork_requests_total", &[]),
                artwork_hits: whole(f, "spoolframe_artwork_cache_hits_total", &[]),
                subtitle_reads: whole(f, "spoolframe_subtitle_reads_total", &[]),
                sync_runs_ok: whole(f, "spoolframe_plex_sync_runs_total", &[("outcome", "ok")]),
                sync_runs_failed: whole(
                    f,
                    "spoolframe_plex_sync_runs_total",
                    &[("outcome", "failed")],
                ),
                previews_done: whole(f, "spoolframe_previews_total", &[("outcome", "done")]),
                previews_failed: whole(f, "spoolframe_previews_total", &[("outcome", "failed")]),
                ocr_done: whole(f, "spoolframe_ocr_total", &[("outcome", "done")]),
                ocr_failed: whole(f, "spoolframe_ocr_total", &[("outcome", "failed")]),
            },
        }
    }

    /// The point at this reading: its gauges, and its counters' rates
    /// since `before`. A counter that went down (a Plex server's, when its
    /// settings changed) counts as nothing in that interval.
    pub fn point(&self, before: Option<&Reading>) -> ActivityPoint {
        let secs = before.map_or(0.0, |b| (self.at - b.at).max(0) as f64 / 1000.0);
        let rate = |now: u64, then: fn(&Reading) -> u64| match before {
            Some(b) if secs > 0.0 => now.saturating_sub(then(b)) as f64 / secs,
            _ => 0.0,
        };
        let output_rate = match before {
            Some(b) if secs > 0.0 => (self.output_seconds - b.output_seconds).max(0.0) / secs,
            _ => 0.0,
        };
        let asked = before.map_or(0, |b| {
            self.totals
                .artwork_requests
                .saturating_sub(b.totals.artwork_requests)
        });
        let hit = before.map_or(0, |b| {
            self.totals
                .artwork_hits
                .saturating_sub(b.totals.artwork_hits)
        });
        ActivityPoint {
            at: self.at,
            connections_app: self.connections_app,
            connections_web: self.connections_web,
            playbacks_direct: self.playbacks_direct,
            playbacks_remux: self.playbacks_remux,
            playbacks_transcode: self.playbacks_transcode,
            conversions_running: self.conversions_running,
            conversions_paused: self.conversions_paused,
            output_rate,
            media_reads_per_sec: rate(self.totals.media_reads, |r| r.totals.media_reads),
            media_bytes_per_sec: rate(self.totals.media_read_bytes, |r| r.totals.media_read_bytes),
            artwork_requests_per_sec: rate(self.totals.artwork_requests, |r| {
                r.totals.artwork_requests
            }),
            artwork_hit_rate: (asked > 0).then(|| hit.min(asked) as f64 / asked as f64),
            subtitle_reads_per_sec: rate(self.totals.subtitle_reads, |r| r.totals.subtitle_reads),
            plex_requests_per_sec: rate(self.plex_requests, |r| r.plex_requests),
            plex_errors_per_sec: rate(self.plex_errors, |r| r.plex_errors),
            previews_queued: self.previews_queued,
            ocr_queued: self.ocr_queued,
            memory_bytes: self.memory_bytes,
            memory: self.memory.clone(),
            open_files: self.open_files,
        }
    }
}

/// The readings of the last [`KEPT`].
#[derive(Debug, Default)]
pub struct History {
    readings: Mutex<VecDeque<Reading>>,
}

impl History {
    /// One reading per [`SAMPLE_EVERY`] over [`KEPT`], and the one before
    /// the first, which its rates are from.
    pub const CAPACITY: usize = (KEPT.as_secs() / SAMPLE_EVERY.as_secs()) as usize + 1;

    pub fn record(&self, reading: Reading) {
        let mut readings = self.readings.lock();
        if readings.len() == Self::CAPACITY {
            readings.pop_front();
        }
        readings.push_back(reading);
    }

    /// Every point kept, oldest first: one fewer than the readings, the
    /// first reading being only what the second's rates are from.
    pub fn points(&self) -> Vec<ActivityPoint> {
        let readings = self.readings.lock();
        readings
            .iter()
            .zip(readings.iter().skip(1))
            .map(|(before, now)| now.point(Some(before)))
            .collect()
    }

    /// The point for `now`, its rates since the last reading at least a
    /// second before it.
    pub fn now(&self, now: &Reading) -> ActivityPoint {
        let readings = self.readings.lock();
        let before = readings.iter().rev().find(|r| now.at - r.at >= 1000);
        now.point(before)
    }
}

/// Unix milliseconds now.
pub fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(at: i64, reads: u64, requests: u64, hits: u64) -> Reading {
        Reading {
            at,
            output_seconds: reads as f64 / 2.0,
            totals: ActivityTotals {
                media_reads: reads,
                media_read_bytes: reads * 1000,
                artwork_requests: requests,
                artwork_hits: hits,
                ..ActivityTotals::default()
            },
            ..Reading::default()
        }
    }

    #[test]
    fn a_point_s_rates_are_per_second_since_the_reading_before() {
        let point = reading(20_000, 50, 10, 4).point(Some(&reading(10_000, 30, 6, 1)));
        assert_eq!(point.media_reads_per_sec, 2.0);
        assert_eq!(point.media_bytes_per_sec, 2000.0);
        assert_eq!(point.output_rate, 1.0);
        assert_eq!(point.artwork_requests_per_sec, 0.4);
        assert_eq!(point.artwork_hit_rate, Some(0.75));
        // With nothing before, or no artwork asked for, no rate.
        let alone = reading(20_000, 50, 10, 4).point(None);
        assert_eq!(
            (alone.media_reads_per_sec, alone.artwork_hit_rate),
            (0.0, None)
        );
        let idle = reading(20_000, 50, 6, 1).point(Some(&reading(10_000, 30, 6, 1)));
        assert_eq!(idle.artwork_hit_rate, None);
    }

    #[test]
    fn the_memory_is_read_with_each_cache_by_name() {
        use super::super::{Kind, Sample};
        let families = [
            Family {
                name: "spoolframe_process_memory_bytes",
                help: "",
                kind: Kind::Gauge,
                samples: vec![
                    Sample::labelled(&[("kind", "heap")], 300.0),
                    Sample::labelled(&[("kind", "files")], 20.0),
                    Sample::labelled(&[("kind", "shared")], 1.0),
                ],
            },
            Family {
                name: "spoolframe_cache_memory_bytes",
                help: "",
                kind: Kind::Gauge,
                samples: vec![
                    Sample::labelled(&[("cache", "artwork")], 100.0),
                    Sample::labelled(&[("cache", "plex_replies")], 50.0),
                ],
            },
        ];
        let memory = Reading::of(&families, 0).memory.unwrap();
        assert_eq!(
            (memory.heap_bytes, memory.files_bytes, memory.shared_bytes),
            (300, 20, 1)
        );
        assert_eq!(
            memory.caches,
            [
                MemoryPart {
                    name: "Artwork".into(),
                    bytes: 100
                },
                MemoryPart {
                    name: "Plex replies".into(),
                    bytes: 50
                },
            ]
        );
        assert_eq!(memory.other_heap_bytes(), 150);
        // Without the kinds (not Linux), no parts at all.
        assert_eq!(Reading::of(&families[1..], 0).memory, None);
    }

    #[test]
    fn a_counter_that_went_down_is_nothing_in_that_interval() {
        let before = Reading {
            at: 0,
            plex_requests: 100,
            ..Reading::default()
        };
        let after = Reading {
            at: 10_000,
            plex_requests: 3,
            ..Reading::default()
        };
        assert_eq!(after.point(Some(&before)).plex_requests_per_sec, 0.0);
    }

    #[test]
    fn the_history_keeps_an_hour_and_its_points_are_oldest_first() {
        let history = History::default();
        for i in 0..(History::CAPACITY as i64 + 10) {
            history.record(reading(i * 10_000, i as u64, 0, 0));
        }
        let points = history.points();
        assert_eq!(points.len(), 360);
        assert!(points.windows(2).all(|w| w[0].at < w[1].at));
        let last_at = (History::CAPACITY as i64 + 9) * 10_000;
        assert_eq!(points.last().unwrap().at, last_at);
        assert_eq!(points[0].at, last_at - 359 * 10_000);
        // Now is measured from the last reading a second or more before:
        // not the one half a second ago, but the one 10.5 s ago.
        let now = history.now(&reading(last_at + 500, 1_000, 0, 0));
        let then = History::CAPACITY as u64 + 8;
        assert_eq!(now.media_reads_per_sec, (1_000 - then) as f64 / 10.5);
    }
}
