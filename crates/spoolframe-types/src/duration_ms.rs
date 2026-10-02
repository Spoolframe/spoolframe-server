//! Durations on the wire are whole milliseconds, so every client language
//! reads them as one integer instead of serde's `{secs, nanos}` struct.

use std::time::Duration;

use serde::{Deserialize, Deserializer, Serializer};

fn millis(d: &Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

pub mod option {
    use super::*;

    pub fn serialize<S: Serializer>(d: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
        match d {
            Some(d) => s.serialize_some(&millis(d)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
        Ok(Option::<u64>::deserialize(d)?.map(Duration::from_millis))
    }
}
