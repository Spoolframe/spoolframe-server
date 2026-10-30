//! Times and spans on the wire, and in what the SDK keeps on disk, as one
//! integer each, so every client language reads them without serde's
//! `{secs, nanos}` struct: `#[serde(with = "serde_time::unix_seconds")]`
//! on a `SystemTime`, `serde_time::millis` on a `Duration`, and each
//! one's `option` on an `Option`. A field keeps its old wire name with
//! `rename` (`position_ms`), so the wire and the stores do not change.
//!
//! An `Option` comes two ways, and the module says which:
//! `option_required` for a field the wire must carry (`null` for none),
//! `option` for one read as none when left out. Serde cannot give a
//! `with` field a default, so `option` goes with `#[serde(default)]`; a
//! test here finds any `option` field in the workspace without it.
//!
//! A time this platform cannot hold is refused when read, never a panic:
//! on wasm32 (the web UI) a `SystemTime` cannot go before the epoch, so a
//! peer's `-1` is a decode error there.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// `at` as a signed span from the Unix epoch, held to `i64` either way.
/// A time before the epoch truncates toward it: 1.5 s before is `-1`.
fn from_epoch(at: SystemTime, unit: fn(Duration) -> u128) -> i64 {
    match at.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(unit(after)).unwrap_or(i64::MAX),
        Err(before) => i64::try_from(unit(before.duration())).map_or(i64::MIN, |n| -n),
    }
}

/// `n` units from the Unix epoch; `None` when this platform's
/// `SystemTime` cannot hold it.
fn at_epoch(n: i64, unit: fn(u64) -> Duration) -> Option<SystemTime> {
    let span = unit(n.unsigned_abs());
    if n < 0 {
        UNIX_EPOCH.checked_sub(span)
    } else {
        UNIX_EPOCH.checked_add(span)
    }
}

/// The Unix epoch, for `#[serde(default = "...")]` on a `SystemTime`
/// (which has no `Default`): older than any time kept.
pub fn epoch() -> SystemTime {
    UNIX_EPOCH
}

/// `n` read off the wire, or the decode error for one this platform
/// cannot hold.
fn held<T, E: serde::de::Error>(value: Option<T>, n: impl std::fmt::Display) -> Result<T, E> {
    value.ok_or_else(|| E::custom(format_args!("time {n} is out of range here")))
}

/// A module's `serialize`/`deserialize` for `$ty` as `$wire`, and its
/// `option` and `option_required` modules.
macro_rules! serde_with {
    ($ty:ty, $wire:ty) => {
        pub fn serialize<S: Serializer>(value: &$ty, s: S) -> Result<S::Ok, S::Error> {
            to_wire(*value).serialize(s)
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<$ty, D::Error> {
            let n = <$wire>::deserialize(d)?;
            held(try_from_wire(n), n)
        }

        /// An `Option` read as none when left out: with
        /// `#[serde(default)]`.
        pub mod option {
            use super::*;

            pub fn serialize<S: Serializer>(value: &Option<$ty>, s: S) -> Result<S::Ok, S::Error> {
                value.map(to_wire).serialize(s)
            }

            pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<$ty>, D::Error> {
                Option::<$wire>::deserialize(d)?
                    .map(|n| held(try_from_wire(n), n))
                    .transpose()
            }
        }

        /// An `Option` the wire must carry, `null` for none.
        pub mod option_required {
            pub use super::option::{deserialize, serialize};
        }
    };
}

/// A point in time as a whole count of `$unit` since the Unix epoch.
macro_rules! unix_time {
    ($(#[$doc:meta])* $name:ident, $as_units:expr, $from_units:path) => {
        $(#[$doc])*
        pub mod $name {
            use super::*;

            pub fn to_wire(at: SystemTime) -> i64 {
                from_epoch(at, $as_units)
            }

            /// `n` as a time; `None` when this platform cannot hold it.
            pub fn try_from_wire(n: i64) -> Option<SystemTime> {
                at_epoch(n, $from_units)
            }

            /// `n` as a time, for a value of this program's own (a
            /// database row, a store): the epoch for one this platform
            /// cannot hold. The one way from a stored count to a time.
            pub fn from_wire(n: i64) -> SystemTime {
                try_from_wire(n).unwrap_or(UNIX_EPOCH)
            }

            /// Now, to this unit: what is kept reads back equal.
            pub fn now() -> SystemTime {
                from_wire(to_wire(SystemTime::now()))
            }

            serde_with!(SystemTime, i64);
        }
    };
}

unix_time!(
    /// A point in time as whole seconds since the Unix epoch.
    unix_seconds,
    |d| u128::from(d.as_secs()),
    Duration::from_secs
);

unix_time!(
    /// A point in time as whole milliseconds since the Unix epoch.
    unix_millis,
    |d| d.as_millis(),
    Duration::from_millis
);

/// A span as whole milliseconds.
pub mod millis {
    use super::*;

    pub fn to_wire(span: Duration) -> u64 {
        u64::try_from(span.as_millis()).unwrap_or(u64::MAX)
    }

    pub fn from_wire(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    /// Every count of milliseconds is a `Duration`.
    fn try_from_wire(millis: u64) -> Option<Duration> {
        Some(from_wire(millis))
    }

    /// `span` to the whole millisecond: what is kept reads back equal.
    pub fn whole(span: Duration) -> Duration {
        from_wire(to_wire(span))
    }

    serde_with!(Duration, u64);
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};
    use serde_json::json;

    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Times {
        #[serde(rename = "at", with = "unix_seconds")]
        seconds: SystemTime,
        #[serde(with = "unix_seconds::option_required")]
        seconds_none: Option<SystemTime>,
        #[serde(with = "unix_millis")]
        millis_at: SystemTime,
        #[serde(default, with = "unix_millis::option")]
        millis_at_some: Option<SystemTime>,
        #[serde(with = "millis")]
        span: Duration,
        #[serde(default, with = "millis::option")]
        span_some: Option<Duration>,
    }

    fn times() -> Times {
        Times {
            seconds: UNIX_EPOCH + Duration::from_secs(1_761_700_000),
            seconds_none: None,
            millis_at: UNIX_EPOCH + Duration::from_millis(1_761_700_000_123),
            millis_at_some: Some(UNIX_EPOCH - Duration::from_millis(1_500)),
            span: Duration::from_millis(258_865),
            span_some: Some(Duration::from_millis(7)),
        }
    }

    /// Each helper writes one integer in its unit and reads it back.
    #[test]
    fn each_helper_writes_one_integer_and_reads_it_back() {
        let wire = json!({
            "at": 1_761_700_000,
            "seconds_none": null,
            "millis_at": 1_761_700_000_123_i64,
            "millis_at_some": -1_500,
            "span": 258_865,
            "span_some": 7,
        });
        assert_eq!(serde_json::to_value(times()).unwrap(), wire);
        assert_eq!(serde_json::from_value::<Times>(wire).unwrap(), times());
    }

    /// A time before the epoch is negative, truncated toward the epoch.
    #[test]
    fn a_time_before_the_epoch_is_negative() {
        let at = UNIX_EPOCH - Duration::from_secs(90);
        assert_eq!(unix_seconds::to_wire(at), -90);
        assert_eq!(unix_seconds::from_wire(-90), at);
        let between = UNIX_EPOCH - Duration::from_millis(1_500);
        assert_eq!(unix_seconds::to_wire(between), -1);
    }

    /// A time the platform cannot hold is a decode error, not a panic.
    /// On wasm32 that is any time before the epoch; this platform holds
    /// every `i64` of seconds, so the extremes decode here, and the
    /// refusal is checked where it is made.
    #[test]
    fn a_time_out_of_range_is_refused_not_a_panic() {
        for n in [i64::MIN, -1, i64::MAX] {
            let mut wire = serde_json::to_value(times()).unwrap();
            wire["at"] = json!(n);
            wire["seconds_none"] = json!(n);
            wire["millis_at"] = json!(n);
            serde_json::from_value::<Times>(wire).unwrap();
        }
        let refused = held::<SystemTime, serde_json::Error>(None, -1).unwrap_err();
        assert_eq!(refused.to_string(), "time -1 is out of range here");
    }

    /// An `Option` with `with` is required, as `required::option` makes
    /// one: left out, it is refused, not read as none.
    #[test]
    fn an_option_left_out_is_refused() {
        let mut wire = serde_json::to_value(times()).unwrap();
        wire.as_object_mut().unwrap().remove("seconds_none");
        let error = serde_json::from_value::<Times>(wire).unwrap_err();
        assert!(error.to_string().contains("missing field"), "{error}");
    }

    /// Whole units read back equal: `now` and `whole` drop what a store
    /// would.
    #[test]
    fn whole_units_read_back_equal() {
        let now = unix_seconds::now();
        assert_eq!(unix_seconds::from_wire(unix_seconds::to_wire(now)), now);
        let span = Duration::from_micros(61_234_567);
        assert_eq!(millis::whole(span), Duration::from_millis(61_234));
    }

    /// Every `option` field in the workspace has `default`: serde gives a
    /// `with` field none of its own, and one left out would fail to read.
    /// A field the wire must carry says `option_required` instead.
    #[test]
    fn every_optional_time_field_has_a_default() {
        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut missing = Vec::new();
        let mut stack = vec![crates];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                let name = entry.file_name();
                if path.is_dir() && name != "target" && name != "node_modules" {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let text = std::fs::read_to_string(&path).unwrap();
                    let lines: Vec<&str> = text.lines().collect();
                    for (n, line) in lines.iter().enumerate() {
                        let optional = ["seconds::option\"", "millis::option\""]
                            .iter()
                            .any(|m| line.contains(m));
                        // The attribute, over the lines it takes.
                        let from = (0..=n)
                            .rev()
                            .take(5)
                            .find(|&i| lines[i].contains("#[serde("));
                        let defaulted = from.is_some_and(|from| {
                            lines[from..=n].iter().any(|l| l.contains("default"))
                        });
                        if optional && from.is_some() && !defaulted {
                            missing.push(format!("{}:{}", path.display(), n + 1));
                        }
                    }
                }
            }
        }
        assert!(
            missing.is_empty(),
            "`option` without `default`: {missing:?}"
        );
    }
}
