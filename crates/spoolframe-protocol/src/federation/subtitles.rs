//! Subtitle search on a paired server's item (P8-07): the server the user
//! is at home on (SB) passes `SearchSubtitles`, `DownloadSubtitle` and
//! `RemoveSubtitleDownload` of a `fed:` item to the server whose item it
//! is (SA), which plans the play and burns subtitles in, so the file must
//! be there.
//!
//! SA searches with its own sites and ranking, ordered by the user's own
//! SDH and forced choices on SB, and keeps what it downloads as its own,
//! for everyone. Every user of SB's together gets one daily allowance on
//! SA (`subtitles { daily_limit_per_peer }`), counted under the pair and
//! never under SB's users, whom SA never names; SB holds each of its users
//! to its own per-user limit first.
//!
//! Each ask arrives on the pair's connection, which proves who sends it,
//! as an assertion does (P8-03): the user is SB's local account, by its
//! stable id.

use serde::{Deserialize, Serialize};
use spoolframe_types::SubtitlePreferences;

/// `SearchSubtitles` of SA's item `item` for SB's `user`: `SubtitleResults`,
/// or `Failed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubtitleSearchAsk {
    /// The user's stable id on SB, as an assertion names them.
    pub user: String,
    /// Their subtitle choices on SB: what the results are ordered by.
    #[serde(default)]
    pub subtitles: SubtitlePreferences,
    /// The item, by SA's own id.
    pub item: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

/// `DownloadSubtitle` of result `result` of a search for SA's `item`, for
/// SB's `user`: `SubtitleDownloaded`, or `Failed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubtitleDownloadAsk {
    pub user: String,
    pub item: String,
    pub result: String,
}

/// `RemoveSubtitleDownload` of download `download` of SA's `item`, for SB's
/// `user`: `Done`, or `Failed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubtitleRemovalAsk {
    pub user: String,
    pub item: String,
    pub download: String,
}
