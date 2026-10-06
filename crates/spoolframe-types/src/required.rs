//! A required `Option` field (PX-15): the key must be on the wire, with
//! `null` for none.
//!
//! serde fills a missing `Option` field with `None` even without
//! `#[serde(default)]`, so a peer that leaves the key out is read as if it
//! sent none. A field with its own `deserialize_with` does not get that,
//! so `#[serde(deserialize_with = "spoolframe_types::required::option")]`,
//! which is only `Option::deserialize`, makes a missing key an error.

use serde::{Deserialize, Deserializer};

pub fn option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}
