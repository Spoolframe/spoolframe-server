//! Items the SDK's tests build, the same in each.

use spoolframe_protocol::SearchHit;
use spoolframe_types::{Item, LibraryServer};

/// A film of 2016, 100 minutes long, not played.
pub fn movie(id: &str, title: &str) -> Item {
    serde_json::from_value(serde_json::json!({
        "kind": "movie", "id": id, "title": title, "year": 2016,
        "release_date": null, "summary": null, "content_rating": null,
        "duration": 6_000_000, "artwork": [],
        "playback": { "position": null, "watched": false },
    }))
    .expect("a movie")
}

/// `item` found by a federated search, on the paired server `origin`
/// (online) or on the user's own.
pub fn hit(item: Item, origin: Option<&str>) -> SearchHit {
    SearchHit {
        play_from: item.id().clone(),
        origin: origin.map(|o| LibraryServer {
            server_id: o.into(),
            name: o.to_uppercase(),
            online: true,
        }),
        also_on: Vec::new(),
        item,
    }
}
