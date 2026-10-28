//! Subtitle search on a paired server's item: the user's home server
//! passes `SearchSubtitles`, `DownloadSubtitle` and
//! `RemoveSubtitleDownload` of a `fed:` item to the item's server (the
//! server whose item it is). That server plans the play and burns
//! subtitles in, so the file must be there.
//!
//! The item's server searches with its own sites and ranking, ordered by
//! the user's own SDH and forced choices on the home server, and keeps
//! what it downloads as its own, for everyone. All the home server's users
//! together get one daily allowance on the item's server
//! (`subtitles { daily_limit_per_peer }`). It is counted under the pair,
//! never under the home server's users, whom the item's server never
//! names. The home server holds each of its users to its own per-user
//! limit first.
//!
//! Each ask arrives on the pair's connection, which proves who sends it,
//! as an assertion does: the user is the home server's local account, by
//! its stable id.

use serde::{Deserialize, Serialize};
use spoolframe_types::SubtitlePreferences;

/// `SearchSubtitles` of the item's server's `item` for the home server's
/// `user`: answered with `SubtitleResults`, or `Failed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubtitleSearchAsk {
    /// The user's stable id on the home server, as an assertion names
    /// them.
    pub user: String,
    /// Their subtitle choices on the home server: what the results are
    /// ordered by.
    #[serde(default)]
    pub subtitles: SubtitlePreferences,
    /// The item, by the item's server's own id.
    pub item: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

/// `DownloadSubtitle` of result `result` of a search for the item's
/// server's `item`, for the home server's `user`: answered with
/// `SubtitleDownloaded`, or `Failed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubtitleDownloadAsk {
    pub user: String,
    pub item: String,
    pub result: String,
}

/// `RemoveSubtitleDownload` of download `download` of the item's server's
/// `item`, for the home server's `user`: answered with `Done`, or
/// `Failed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubtitleRemovalAsk {
    pub user: String,
    pub item: String,
    pub download: String,
}
