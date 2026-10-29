//! Watch state changed on another device (feature
//! `spoolframe:watch-events`).

use spoolframe_types::MediaId;

/// The user's watch state of `items` changed on another device of
/// theirs, as server `server` told it: read again what shows them (Home,
/// an item's page). A season's or a show's id stands for its episodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchEvent {
    /// The server's key.
    pub server: String,
    pub items: Vec<MediaId>,
}
