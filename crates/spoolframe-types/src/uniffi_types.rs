//! The types UniFFI passes as another type (feature `uniffi`): the IDs and
//! the calendar date as the strings they are on the wire, so a foreign
//! language holds the same text the protocol sends and parses nothing.

use crate::{ArtworkId, CalendarDate, LibraryId, MediaId, PersonId};

uniffi::custom_type!(MediaId, String, {
    lower: |id| id.to_string(),
    try_lift: |text| Ok(text.parse()?),
});

uniffi::custom_type!(LibraryId, String, {
    lower: |id| id.to_string(),
    try_lift: |text| Ok(text.parse()?),
});

uniffi::custom_type!(ArtworkId, String, {
    lower: |id| id.to_string(),
    try_lift: |text| Ok(text.parse()?),
});

uniffi::custom_type!(PersonId, String, {
    lower: |id| id.to_string(),
    try_lift: |text| Ok(text.parse()?),
});

// `YYYY-MM-DD`.
uniffi::custom_type!(CalendarDate, String, {
    lower: |date| date.to_string(),
    try_lift: |text| Ok(text.parse()?),
});
