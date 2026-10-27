//! Histograms for the OTLP export: each value recorded as it is observed.
//!
//! `/metrics` reads a histogram's buckets from the part that counts them,
//! as it reads everything else. OpenTelemetry has no histogram read that
//! way (no observable histogram), so the export gets each value when it
//! happens instead, into an instrument with the same bounds. A histogram
//! is declared once, as a [`Spec`] in [`ALL`]: its family in `/metrics`
//! takes the name and help from there, and the export makes its
//! instrument from there, so a new one only has to be recorded.
//!
//! A histogram no part of the server counts for itself (the Plex
//! client counts its own) is [`Histograms::observe`]d instead: kept here
//! for `/metrics` as well as recorded for the export. Those are in
//! [`KEPT`].

use std::collections::{BTreeMap, HashMap};

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Histogram, Meter};
use parking_lot::RwLock;

use super::{Family, HistogramValue, Kind, Sample, Value};

/// A histogram, as both `/metrics` and the OTLP export name and bucket it.
#[derive(Debug)]
pub struct Spec {
    pub name: &'static str,
    pub help: &'static str,
    /// Upper bounds, as `/metrics` has them: the export uses the same, not
    /// OpenTelemetry's defaults.
    pub bounds: &'static [f64],
}

pub const PLEX_REQUEST_DURATION: Spec = Spec {
    name: "spoolframe_plex_request_duration_seconds",
    help: "How long requests to each Plex server took.",
    bounds: &spoolframe_plex::stats::LATENCY_BUCKETS,
};

pub const CLIENT_REQUEST_DURATION: Spec = Spec {
    name: "spoolframe_client_request_duration_seconds",
    help: "How long the server took to answer each client request, by its type, from its \
           frame read to its answer made (the answer's sending not included).",
    bounds: &[
        0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
    ],
};

pub const PLAYBACK_FIRST_SEGMENT: Spec = Spec {
    name: "spoolframe_playback_first_segment_seconds",
    help: "From a StartPlayback request to the first read of the playback's media answered \
           with bytes (HLS: its first media segment, not the init segment), by plan: what a \
           player waits before its first frame, its own fetches included.",
    bounds: &[
        0.1, 0.25, 0.5, 1.0, 2.0, 3.0, 5.0, 7.5, 10.0, 15.0, 20.0, 30.0, 60.0,
    ],
};

pub const SEGMENT_READ_WAIT: Spec = Spec {
    name: "spoolframe_segment_read_wait_seconds",
    help: "How long each read of a conversion's output waited for the encode to write what it \
           asked for: 0 for a read the encode was already past (the le=\"0\" bucket), up to \
           the read wait of 10 s. Reads of a direct playback's file and of a kept init \
           segment never wait and are not counted.",
    bounds: &[0.0, 0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0],
};

pub const CONVERSION_SPEED: Spec = Spec {
    name: "spoolframe_conversion_speed",
    help: "Each ffmpeg run's speed at its end, by its kind: the media time it encoded over \
           the time it ran, time held back for its reader left out; 1 is real time. Runs \
           that ran under a second are left out.",
    bounds: &[
        0.25, 0.5, 0.8, 1.0, 1.5, 2.0, 3.0, 5.0, 10.0, 20.0, 50.0, 100.0,
    ],
};

pub const CONNECTION_RTT: Spec = Spec {
    name: "spoolframe_connection_rtt_seconds",
    help: "Client connections' round-trip time on the path their packets go by, by its \
           kind (lan, direct, relay): one reading per connection every 10 s.",
    bounds: &[
        0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.15, 0.2, 0.3, 0.5, 1.0, 2.0,
    ],
};

pub const PLEX_SYNC_DURATION: Spec = Spec {
    name: "spoolframe_plex_sync_duration_seconds",
    help: "How long each watch state sync run with a linked Plex account took, every \
           library it syncs on every Plex server, by outcome (ok, failed). Dry runs are \
           not counted.",
    bounds: &[
        0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0,
    ],
};

pub const FEDERATION_SYNC_DURATION: Spec = Spec {
    name: "spoolframe_federation_sync_duration_seconds",
    help: "How long each sync of a paired server's shared libraries took, by outcome (ok, \
           failed), from when it holds the pair's sync lock: the replica's and an admin's \
           Refresh.",
    bounds: &[
        0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0,
    ],
};

/// Every histogram the server has.
pub const ALL: [&Spec; 8] = [
    &PLEX_REQUEST_DURATION,
    &CLIENT_REQUEST_DURATION,
    &PLAYBACK_FIRST_SEGMENT,
    &SEGMENT_READ_WAIT,
    &CONVERSION_SPEED,
    &CONNECTION_RTT,
    &PLEX_SYNC_DURATION,
    &FEDERATION_SYNC_DURATION,
];

/// The histograms whose values are kept here for `/metrics`
/// ([`Histograms::observe`]): every one but those a part counts itself.
pub const KEPT: [&Spec; 7] = [
    &CLIENT_REQUEST_DURATION,
    &PLAYBACK_FIRST_SEGMENT,
    &SEGMENT_READ_WAIT,
    &CONVERSION_SPEED,
    &CONNECTION_RTT,
    &PLEX_SYNC_DURATION,
    &FEDERATION_SYNC_DURATION,
];

/// A series' labels, in the order they were given.
type Labels = Vec<(&'static str, String)>;

/// One series as kept: a count per bound (not cumulative), and the
/// count and sum of every value.
#[derive(Debug, Clone)]
struct Kept {
    per_bound: Vec<u64>,
    count: u64,
    sum: f64,
}

/// The export's instruments, by name, while it exports metrics; and the
/// values of [`KEPT`]'s histograms, by name and labels.
#[derive(Debug, Default)]
pub struct Histograms {
    export: RwLock<Option<HashMap<&'static str, Histogram<f64>>>>,
    kept: parking_lot::Mutex<HashMap<&'static str, BTreeMap<Labels, Kept>>>,
}

impl Histograms {
    /// Record into `meter`'s instruments from now on, or nowhere with
    /// `None`.
    pub fn export_to(&self, meter: Option<&Meter>) {
        let instruments = meter.map(|meter| {
            ALL.iter()
                .map(|spec| {
                    let instrument = meter
                        .f64_histogram(spec.name)
                        .with_description(spec.help)
                        .with_boundaries(spec.bounds.to_vec())
                        .build();
                    (spec.name, instrument)
                })
                .collect()
        });
        *self.export.write() = instruments;
    }

    /// `value` observed for `spec`'s series with `attributes`: recorded
    /// while the export runs, and dropped while it does not.
    pub fn record(&self, spec: &Spec, value: f64, attributes: &[KeyValue]) {
        if let Some(instrument) = self.export.read().as_ref().and_then(|i| i.get(spec.name)) {
            instrument.record(value, attributes);
        }
    }

    /// `value` observed for `spec`'s series with `labels`: kept for
    /// `/metrics`, and recorded while the export runs. `spec` is one of
    /// [`KEPT`]; the labels are bounded, and never name a user, a session,
    /// an address or a device.
    pub fn observe(&self, spec: &'static Spec, value: f64, labels: &[(&'static str, &str)]) {
        debug_assert!(KEPT.iter().any(|k| k.name == spec.name), "{}", spec.name);
        {
            let mut kept = self.kept.lock();
            let series = kept
                .entry(spec.name)
                .or_default()
                .entry(labels.iter().map(|(k, v)| (*k, (*v).to_owned())).collect())
                .or_insert_with(|| Kept {
                    per_bound: vec![0; spec.bounds.len()],
                    count: 0,
                    sum: 0.0,
                });
            if let Some(at) = spec.bounds.iter().position(|&le| value <= le) {
                series.per_bound[at] += 1;
            }
            series.count += 1;
            series.sum += value;
        }
        let attributes: Vec<KeyValue> = labels
            .iter()
            .map(|(k, v)| KeyValue::new(*k, (*v).to_owned()))
            .collect();
        self.record(spec, value, &attributes);
    }

    /// `spec`'s family as `/metrics` has it: a series per set of labels
    /// observed, none before the first value.
    pub fn family(&self, spec: &'static Spec) -> Family {
        let kept = self.kept.lock();
        let samples = kept
            .get(spec.name)
            .into_iter()
            .flatten()
            .map(|(labels, series)| {
                let mut below = 0;
                let buckets = spec
                    .bounds
                    .iter()
                    .zip(&series.per_bound)
                    .map(|(&le, &n)| {
                        below += n;
                        (le, below)
                    })
                    .collect();
                Sample {
                    labels: labels.clone(),
                    value: Value::Histogram(HistogramValue {
                        buckets,
                        count: series.count,
                        sum: series.sum,
                    }),
                }
            })
            .collect();
        Family {
            name: spec.name,
            help: spec.help,
            kind: Kind::Histogram,
            samples,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Kept values are bucketed as Prometheus counts them: cumulative,
    /// a value on a bound in that bound's bucket, one past the last only
    /// in the count.
    #[test]
    fn an_observed_value_lands_in_its_bucket_and_those_above() {
        let histograms = Histograms::default();
        let spec = &CLIENT_REQUEST_DURATION;
        for v in [0.001, 0.003, 60.0] {
            histograms.observe(spec, v, &[("request", "ping")]);
        }
        histograms.observe(spec, 0.2, &[("request", "hello")]);
        let family = histograms.family(spec);
        let ping = family
            .samples
            .iter()
            .find(|s| s.labels == [("request", "ping".to_owned())])
            .unwrap();
        let Value::Histogram(h) = &ping.value else {
            panic!("{ping:?}");
        };
        assert_eq!(h.count, 3);
        assert!((h.sum - 60.004).abs() < 1e-9, "{}", h.sum);
        assert_eq!(h.buckets[0], (0.001, 1));
        assert_eq!(h.buckets[2], (0.005, 2));
        assert_eq!(h.buckets.last(), Some(&(30.0, 2)));
        assert_eq!(family.samples.len(), 2);
    }
}
