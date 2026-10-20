//! Server › Optimized (PX-37): the profiles copies are made in, the pool
//! of copies against its budget, and the copy being made.

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
    /// then the least recently used.
    pub copies: Vec<OptimizedCopy>,
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
        };
        let json = serde_json::to_value(&overview).unwrap();
        for field in ["dir", "usedBytes", "maxSizeBytes", "profiles", "copies"] {
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
}
