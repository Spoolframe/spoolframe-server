//! Items the FFI's tests build, the same in each.

use spoolframe_client_sdk::types::Item;

/// "Arrival": a film of 2016, 100 minutes long, with a poster, played
/// for a minute.
pub fn arrival() -> Item {
    serde_json::from_value(serde_json::json!({
        "kind": "movie", "id": "plex:movie:1", "title": "Arrival", "year": 2016,
        "release_date": null, "summary": null, "content_rating": null,
        "duration": 6_000_000,
        "artwork": [{ "id": "plex:/library/metadata/1/thumb/9", "kind": "poster" }],
        "playback": { "position": 60_000, "watched": false },
    }))
    .expect("a movie")
}
