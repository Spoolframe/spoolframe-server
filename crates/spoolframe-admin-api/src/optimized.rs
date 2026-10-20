//! Server › Optimized (PX-37): the profiles copies are made in, the pool
//! of copies against its budget, the copy being made, and the on-demand
//! requests an admin made for an item, a season, a show or a library.

use serde::{Deserialize, Serialize};

/// A profile: what its copies' video is converted to. Every user's
/// downloads in it share its copies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OptimizedProfile {
    /// `Mobile`, `TV`: at most 40 characters, no space at either end.
    pub name: String,
    /// The protocol's codec name: `h264`, `hevc` or `av1`.
    pub video_codec: String,
    /// Lines in the output, 144 to 4320.
    pub height: u32,
    /// Bits per second, at least 100 kbit/s.
    pub max_bitrate: u64,
}

/// What `optimizedProfilePut` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OptimizedProfileWritten {
    /// It was not there before.
    pub created: bool,
    /// Its copies now outdated, as its video changed: never played or
    /// downloaded again, and the first to go.
    pub outdated: u64,
    /// The items of its on-demand requests queued again, to be made with
    /// its new video.
    pub requeued: u64,
}

/// Where a copy is in its making.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OptimizedCopyState {
    /// Asked for, waiting for a place in the encode budget.
    Waiting,
    /// Its ffmpeg is running.
    Making,
    /// Its file is whole.
    Ready,
    /// Its ffmpeg failed; `failure` says why.
    Failed,
}

/// A copy in the pool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OptimizedCopy {
    pub id: String,
    /// The catalog item it is a copy of.
    pub media_id: String,
    /// The item as the Playback page names it (`Alien`, `Severance · S2
    /// E4`), when the catalog answered in time; else the page shows the
    /// file's name.
    pub title: Option<String>,
    /// An episode's own title.
    pub subtitle: Option<String>,
    /// The media file it is made from.
    pub path: String,
    /// Its profile's name, or `ahead` for binge-ahead's.
    pub target: String,
    /// Never used again: its profile's video changed, the profile is
    /// gone, or the file changed since.
    pub outdated: bool,
    pub state: OptimizedCopyState,
    /// Its file's size, once made.
    pub size_bytes: Option<u64>,
    pub failure: Option<String>,
    /// A playback or a download reads it now: a delete waits for them.
    pub reading: bool,
    /// Unix seconds.
    pub requested_at: i64,
    pub made_at: Option<i64>,
    pub last_used_at: i64,
    /// How far it is, for the one copy being made now.
    pub progress: Option<OptimizedProgress>,
    /// The tracks it stores (the track store): each converted stream a
    /// file of its own. Its other tracks are read from the media file.
    /// Empty for a copy that converts nothing.
    #[serde(default)]
    pub tracks: Vec<OptimizedTrack>,
    /// A pinned on-demand request wants it: it is never evicted.
    pub pinned: bool,
}

/// One stored track of a copy: a converted stream in a file of its own,
/// shared by every copy that needs it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OptimizedTrack {
    pub id: String,
    /// `video` or `audio`.
    pub kind: String,
    /// What it is converted to: `h264 1080p 4000 kbit/s`, `eac3 6 ch
    /// 640 kbit/s`.
    pub label: String,
    pub state: OptimizedCopyState,
    /// Its file's size, once made.
    pub size_bytes: Option<u64>,
    /// Another copy is made with it too.
    pub shared: bool,
}

/// How far the copy being made is.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OptimizedProgress {
    /// Of the film written, 0 to 1, once its length is known.
    pub fraction: Option<f64>,
    /// How long it has yet to go, at the speed it has gone so far.
    pub eta_secs: Option<u64>,
}

/// Server › Optimized, as `optimizedOverview` answers it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OptimizedOverview {
    /// Where the copies are kept (`optimized.dir`).
    pub dir: String,
    /// What the budget counts: the copies made, and the one being made
    /// at what it is expected to grow to.
    pub used_bytes: u64,
    /// `optimized.max_size_mb`, in bytes.
    pub max_size_bytes: u64,
    /// In the order the admin put them.
    pub profiles: Vec<OptimizedProfile>,
    /// In the order they would be evicted: outdated first, then `ahead`,
    /// then the least recently used. Pinned ones are never evicted.
    pub copies: Vec<OptimizedCopy>,
    /// The on-demand requests, newest first.
    pub requests: Vec<OptimizedRequest>,
}

/// What an on-demand request covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OptimizedScopeKind {
    /// One film or episode.
    Item,
    /// Every episode of a season.
    Season,
    /// Every episode of a show, specials too.
    Show,
    /// Every film of a film library, or every episode of a show library.
    Library,
}

/// An item, season, show or library, by its id: a media id
/// (`plex:movie:1`) or, for a library, a library id (`plex:1`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OptimizedScope {
    pub kind: OptimizedScopeKind,
    pub id: String,
}

/// What `optimizedRequestPut` asks for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OptimizedRequestPut {
    pub scope: OptimizedScope,
    /// A profile's name.
    pub profile: String,
    /// Its copies are never evicted, and count against the budget until
    /// the request is deleted or unpinned.
    pub pinned: bool,
}

/// An on-demand request: a kept rule that makes a copy, in its profile,
/// of each film or episode its scope holds, and of each one the scope
/// gains later.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OptimizedRequest {
    pub id: String,
    pub scope: OptimizedScope,
    /// The scope as the pages name it (`Alien`, `The Expanse · Season 1`,
    /// `Movies`), when the catalog answered in time.
    pub title: Option<String>,
    pub profile: String,
    pub pinned: bool,
    /// The admin who asked, by username; `None` for one since removed.
    pub created_by: Option<String>,
    /// Unix seconds.
    pub created_at: i64,
    /// The films and episodes it covers now.
    pub items: u64,
    /// Of those, the ones whose copy is made.
    pub done: u64,
    /// Of those, the ones whose copy could not be made.
    pub failed: u64,
    /// The made copies' tracks together.
    pub size_bytes: u64,
}

/// What `optimizedOutdatedDelete` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OptimizedDeleted {
    /// Copies gone, with their files.
    pub now: u64,
    /// Copies being read, which go when their readers are done.
    pub deferred: u64,
}

/// What `optimizedCopyDelete` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OptimizedCopyDeleted {
    /// The copy and its file are gone.
    Now,
    /// It goes once its readers are done, or once its ffmpeg has
    /// stopped; nobody starts reading it from now on.
    Deferred,
    /// There was no such copy.
    NotFound,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PX-37: a server and a web UI that disagree about a shape fail on
    /// the call.
    #[test]
    fn the_overview_s_fields_are_required() {
        let overview = OptimizedOverview {
            dir: "/var/lib/spoolframe/optimized".into(),
            used_bytes: 1,
            max_size_bytes: 2,
            profiles: Vec::new(),
            copies: Vec::new(),
            requests: Vec::new(),
        };
        let json = serde_json::to_value(&overview).unwrap();
        for field in [
            "dir",
            "usedBytes",
            "maxSizeBytes",
            "profiles",
            "copies",
            "requests",
        ] {
            let mut without = json.clone();
            without.as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<OptimizedOverview>(without).is_err(),
                "{field} was filled in"
            );
        }
    }

    /// PX-37: the web UI reads the states and the delete's
    /// outcome by these names.
    #[test]
    fn states_and_deletes_are_named_in_camel_case() {
        assert_eq!(
            serde_json::to_value(OptimizedCopyState::Making).unwrap(),
            "making"
        );
        assert_eq!(
            serde_json::to_value(OptimizedCopyDeleted::NotFound).unwrap(),
            "notFound"
        );
    }

    /// The web UI names a request's scope as the server reads it.
    #[test]
    fn a_scope_reads_as_its_kind_and_id() {
        let scope: OptimizedScope =
            serde_json::from_value(serde_json::json!({"kind": "season", "id": "plex:season:11"}))
                .unwrap();
        assert_eq!(
            scope,
            OptimizedScope {
                kind: OptimizedScopeKind::Season,
                id: "plex:season:11".into()
            }
        );
    }
}
