//! The Prometheus text exposition format (0.0.4), written by hand: the
//! server's families are few and fixed, and the format is a page of rules.

use std::fmt::Write as _;

use super::{Family, Kind, Sample, Value};

/// What `GET /metrics` answers with.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// `families` as Prometheus text: `# HELP` and `# TYPE` for each, then
/// one line per sample (a histogram's buckets, sum and count).
pub fn render(families: &[Family]) -> String {
    let mut out = String::new();
    for family in families {
        let kind = match family.kind {
            Kind::Counter => "counter",
            Kind::Gauge => "gauge",
            Kind::Histogram => "histogram",
        };
        let _ = writeln!(out, "# HELP {} {}", family.name, help(family.help));
        let _ = writeln!(out, "# TYPE {} {kind}", family.name);
        for sample in &family.samples {
            write_sample(&mut out, family.name, sample);
        }
    }
    out
}

fn write_sample(out: &mut String, name: &str, sample: &Sample) {
    match &sample.value {
        Value::Number(v) => {
            let _ = writeln!(out, "{name}{} {}", labels(&sample.labels, None), number(*v));
        }
        Value::Histogram(h) => {
            for (le, count) in &h.buckets {
                let _ = writeln!(
                    out,
                    "{name}_bucket{} {count}",
                    labels(&sample.labels, Some(&number(*le)))
                );
            }
            let _ = writeln!(
                out,
                "{name}_bucket{} {}",
                labels(&sample.labels, Some("+Inf")),
                h.count
            );
            let _ = writeln!(
                out,
                "{name}_sum{} {}",
                labels(&sample.labels, None),
                number(h.sum)
            );
            let _ = writeln!(
                out,
                "{name}_count{} {}",
                labels(&sample.labels, None),
                h.count
            );
        }
    }
}

/// `{a="x",b="y"}`, with `le` last for a bucket; nothing for no labels.
fn labels(labels: &[(&'static str, String)], le: Option<&str>) -> String {
    let mut parts: Vec<String> = labels
        .iter()
        .map(|(k, v)| format!("{k}=\"{}\"", label_value(v)))
        .collect();
    if let Some(le) = le {
        parts.push(format!("le=\"{le}\""));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("{{{}}}", parts.join(","))
    }
}

/// A label value: backslash, double quote and line feed escaped.
fn label_value(v: &str) -> String {
    v.replace('\\', r"\\")
        .replace('"', "\\\"")
        .replace('\n', r"\n")
}

/// Help text: backslash and line feed escaped.
fn help(text: &str) -> String {
    text.replace('\\', r"\\").replace('\n', r"\n")
}

fn number(v: f64) -> String {
    if v.is_nan() {
        "NaN".into()
    } else if v == f64::INFINITY {
        "+Inf".into()
    } else if v == f64::NEG_INFINITY {
        "-Inf".into()
    } else {
        format!("{v}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::HistogramValue;

    #[test]
    fn a_family_is_its_help_its_type_and_a_line_per_sample() {
        let text = render(&[Family {
            name: "spoolframe_connections_open",
            help: "Open connections.",
            kind: Kind::Gauge,
            samples: vec![
                Sample::labelled(&[("kind", "app")], 2.0),
                Sample::labelled(&[("kind", "web")], 0.5),
            ],
        }]);
        assert_eq!(
            text,
            "# HELP spoolframe_connections_open Open connections.\n\
             # TYPE spoolframe_connections_open gauge\n\
             spoolframe_connections_open{kind=\"app\"} 2\n\
             spoolframe_connections_open{kind=\"web\"} 0.5\n"
        );
    }

    #[test]
    fn label_values_and_help_are_escaped() {
        let text = render(&[Family {
            name: "x_total",
            help: "a \\ b\nc",
            kind: Kind::Counter,
            samples: vec![Sample::labelled(&[("server", "a\"b\\c\nd")], 1.0)],
        }]);
        assert!(text.contains("# HELP x_total a \\\\ b\\nc\n"), "{text}");
        assert!(
            text.contains("x_total{server=\"a\\\"b\\\\c\\nd\"} 1\n"),
            "{text}"
        );
    }

    #[test]
    fn a_histogram_has_cumulative_buckets_up_to_inf_a_sum_and_a_count() {
        let text = render(&[Family {
            name: "d_seconds",
            help: "h",
            kind: Kind::Histogram,
            samples: vec![Sample {
                labels: vec![("server", "plex".into())],
                value: Value::Histogram(HistogramValue {
                    buckets: vec![(0.1, 1), (1.0, 3)],
                    count: 4,
                    sum: 7.25,
                }),
            }],
        }]);
        assert_eq!(
            text,
            "# HELP d_seconds h\n\
             # TYPE d_seconds histogram\n\
             d_seconds_bucket{server=\"plex\",le=\"0.1\"} 1\n\
             d_seconds_bucket{server=\"plex\",le=\"1\"} 3\n\
             d_seconds_bucket{server=\"plex\",le=\"+Inf\"} 4\n\
             d_seconds_sum{server=\"plex\"} 7.25\n\
             d_seconds_count{server=\"plex\"} 4\n"
        );
    }
}
