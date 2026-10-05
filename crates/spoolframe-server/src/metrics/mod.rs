//! The server's metrics (P3-14): what `GET /metrics` on the admin listener
//! answers a Prometheus scraper with.
//!
//! A small registry of our own rather than the `metrics` crate's global
//! recorder: every test in a process starts servers of its own, and a
//! global recorder would add up all of theirs. Each [`Metrics`] is one
//! server's, and its families are read from that server's parts when
//! asked for, so nothing is counted twice and nothing is kept that the
//! parts do not already know.

pub mod process;
mod prometheus;

use std::net::IpAddr;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

pub use prometheus::{CONTENT_TYPE, render};

use crate::admin::AddressRanges;

/// A count that only goes up.
#[derive(Debug, Default)]
pub struct Counter(AtomicU64);

impl Counter {
    pub fn inc(&self) {
        self.add(1);
    }

    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// What a family's samples are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Only goes up (until the server restarts).
    Counter,
    /// Goes up and down.
    Gauge,
    Histogram,
}

/// One metric, by its Prometheus name, with all its labelled samples.
#[derive(Debug, Clone, PartialEq)]
pub struct Family {
    pub name: &'static str,
    pub help: &'static str,
    pub kind: Kind,
    pub samples: Vec<Sample>,
}

impl Family {
    /// A family with one sample and no labels.
    pub fn single(name: &'static str, help: &'static str, kind: Kind, value: f64) -> Self {
        Self {
            name,
            help,
            kind,
            samples: vec![Sample::labelled(&[], value)],
        }
    }

    /// The value of the sample with exactly `labels`, if it is a number.
    pub fn value(&self, labels: &[(&str, &str)]) -> Option<f64> {
        self.samples
            .iter()
            .find(|s| {
                s.labels.len() == labels.len()
                    && labels
                        .iter()
                        .all(|(k, v)| s.labels.iter().any(|(sk, sv)| sk == k && sv == v))
            })
            .and_then(|s| match s.value {
                Value::Number(v) => Some(v),
                Value::Histogram(_) => None,
            })
    }

    /// Every number sample added up.
    pub fn total(&self) -> f64 {
        self.samples
            .iter()
            .map(|s| match s.value {
                Value::Number(v) => v,
                Value::Histogram(_) => 0.0,
            })
            .sum()
    }
}

/// One series of a family.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub labels: Vec<(&'static str, String)>,
    pub value: Value,
}

impl Sample {
    pub fn labelled(labels: &[(&'static str, &str)], value: f64) -> Self {
        Self {
            labels: labels.iter().map(|(k, v)| (*k, (*v).to_owned())).collect(),
            value: Value::Number(value),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Number(f64),
    Histogram(HistogramValue),
}

/// A histogram as Prometheus has it: cumulative counts at or under each
/// upper bound (`le`), the `+Inf` bucket being `count`.
#[derive(Debug, Clone, PartialEq)]
pub struct HistogramValue {
    pub buckets: Vec<(f64, u64)>,
    pub count: u64,
    pub sum: f64,
}

/// Whether `GET /metrics` answers, and to whom (`admin.metrics`,
/// `admin.metrics_from`): live settings.
#[derive(Debug)]
pub struct MetricsAccess {
    enabled: AtomicBool,
    from: RwLock<AddressRanges>,
}

impl Default for MetricsAccess {
    /// Off; from loopback once on.
    fn default() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            from: RwLock::new(AddressRanges::loopback()),
        }
    }
}

impl MetricsAccess {
    pub fn set(&self, enabled: bool, from: AddressRanges) {
        *self.from.write().expect("never held across a panic") = from;
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Whether a scraper at `ip` may read them.
    pub fn allows(&self, ip: IpAddr) -> bool {
        self.from
            .read()
            .expect("never held across a panic")
            .contains(ip)
    }
}

/// One server's metrics.
#[derive(Debug)]
pub struct Metrics {
    started: Instant,
    access: MetricsAccess,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            access: MetricsAccess::default(),
        }
    }
}

impl Metrics {
    pub fn access(&self) -> &MetricsAccess {
        &self.access
    }

    /// Every family, as it is now.
    pub fn families(&self) -> Vec<Family> {
        let mut families = Vec::new();
        self.process(&mut families);
        families
    }

    fn process(&self, families: &mut Vec<Family>) {
        families.push(Family::single(
            "spoolframe_process_uptime_seconds",
            "Seconds since the server started.",
            Kind::Gauge,
            self.started.elapsed().as_secs_f64(),
        ));
        if let Some(bytes) = process::resident_bytes() {
            families.push(Family::single(
                "spoolframe_process_resident_memory_bytes",
                "The server's resident memory.",
                Kind::Gauge,
                bytes as f64,
            ));
        }
        if let Some(files) = process::open_files() {
            families.push(Family::single(
                "spoolframe_process_open_files",
                "Files, sockets and pipes the server has open.",
                Kind::Gauge,
                files as f64,
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_are_off_and_for_loopback_until_set() {
        let access = MetricsAccess::default();
        assert!(!access.enabled());
        assert!(access.allows("127.0.0.1".parse().unwrap()));
        assert!(access.allows("::1".parse().unwrap()));
        assert!(!access.allows("192.0.2.7".parse().unwrap()));
        access.set(true, "10.0.0.0/8".parse().unwrap());
        assert!(access.enabled());
        assert!(access.allows("10.1.2.3".parse().unwrap()));
        assert!(!access.allows("127.0.0.1".parse().unwrap()));
    }

    #[test]
    fn the_process_is_measured() {
        let families = Metrics::default().families();
        let names: Vec<_> = families.iter().map(|f| f.name).collect();
        assert!(
            names.contains(&"spoolframe_process_uptime_seconds"),
            "{names:?}"
        );
        #[cfg(target_os = "linux")]
        for name in [
            "spoolframe_process_resident_memory_bytes",
            "spoolframe_process_open_files",
        ] {
            assert!(names.contains(&name), "{names:?}");
        }
    }
}
