//! A conversion's HLS by name, for a player that reads it itself (Media3's
//! `HlsMediaSource`, through the app's `DataSource`), with no bridge
//! server: the same reads the loopback bridge answers a URL player with
//! (`spoolframe-hls-bridge`'s `Bridge::resource`). The server's playlists
//! name their resources relatively (`media.m3u8`, `seg3.m4s`), so the
//! player resolves each back under the playback's own scheme and token.
//!
//! Every open playback the bridge can serve is registered with it under
//! its token, served or not (`playback.rs`): the HTTP side (`bridge.rs`,
//! the `hls-bridge` feature) answers from the same bridge.

use std::sync::{Arc, LazyLock};

use spoolframe_hls_bridge::{Bridge, BridgeOptions, SdkSource};

use crate::error::SpoolError;
use crate::playback::{Registered, registered};
use crate::runtime::block;

/// The app's one bridge: the playbacks it reads, by token.
/// Debugging switches from the environment: `SPOOLFRAME_BRIDGE_LOG=1`
/// logs refused playlist references and failed reads;
/// `SPOOLFRAME_MASTER_REWRITE="from>to;…"` edits the master playlist.
static BRIDGE: LazyLock<Bridge<SdkSource>> = LazyLock::new(|| {
    Bridge::with_options(BridgeOptions {
        log: std::env::var_os("SPOOLFRAME_BRIDGE_LOG").is_some(),
        master_rewrite: std::env::var("SPOOLFRAME_MASTER_REWRITE").ok(),
    })
});

/// The bridge, to register a playback with and to serve.
pub(crate) fn bridge() -> &'static Bridge<SdkSource> {
    &BRIDGE
}

/// How many resources a playback keeps: Media3 reads a playlist, a
/// segment and a subtitle segment on separate `DataSource`s at once, each
/// opening its own.
const KEPT: usize = 4;

/// One resource read whole, kept for the ranges read from it.
pub(crate) struct Fetched {
    path: String,
    content_type: &'static str,
    bytes: Vec<u8>,
}

impl Fetched {
    /// It as a `DataSource` opens it.
    pub(crate) fn resource(&self) -> PlaybackResource {
        PlaybackResource {
            content_type: self.content_type.to_owned(),
            length: self.bytes.len() as u64,
        }
    }
}

/// A resource as a `DataSource` opens it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PlaybackResource {
    /// `application/vnd.apple.mpegurl`, `video/mp4`, `text/vtt`…
    pub content_type: String,
    /// Its length in bytes.
    pub length: u64,
}

fn failed(path: &str, error: spoolframe_hls_bridge::Error) -> SpoolError {
    use spoolframe_hls_bridge::Error;
    match error {
        Error::Restarted { epoch } => SpoolError::PlaybackRestarted { epoch },
        Error::Resumed { at } => SpoolError::PlaybackResumed { at },
        Error::NotReady { done_percent } => SpoolError::SubtitleNotReady { done_percent },
        Error::Stopped => SpoolError::PlaybackStopped,
        Error::Expired => SpoolError::PlaybackExpired,
        Error::NotFound => SpoolError::NotFound { what: path.into() },
        Error::Replanned => SpoolError::Replanned,
        other => SpoolError::PlaybackFailed {
            reason: other.to_string(),
        },
    }
}

/// `path` of `playback`, read whole now and kept for its ranges, in place
/// of the one kept for `path` before (a playlist that grew); the least
/// recently used goes past [`KEPT`]. Runs on the SDK's runtime
/// ([`block`]).
pub(crate) async fn fetch(
    playback: Arc<Registered>,
    path: String,
) -> Result<Arc<Fetched>, SpoolError> {
    // Closed (ended, or the player moved on): the bridge has no stream
    // under its token to read.
    if registered(&playback.token).is_none() {
        return Err(SpoolError::PlaybackExpired);
    }
    let (content_type, bytes) = match BRIDGE.resource(&playback.token, &path).await {
        Ok(Some(found)) => found,
        // Unregistered since the check above (ended, or the player
        // moved on): expired, which the player follows, not a resource
        // missing from a plan still on.
        Ok(None) if registered(&playback.token).is_none() => {
            return Err(SpoolError::PlaybackExpired);
        }
        Ok(None) => return Err(SpoolError::NotFound { what: path }),
        Err(error) => return Err(failed(&path, error)),
    };
    let fetched = Arc::new(Fetched {
        path,
        content_type,
        bytes,
    });
    let mut kept = playback.resources.lock().expect("never poisoned");
    kept.retain(|f| f.path != fetched.path);
    kept.push(fetched.clone());
    if kept.len() > KEPT {
        kept.remove(0);
    }
    Ok(fetched)
}

/// The kept `path` of `playback`, now the most recently used.
fn kept(playback: &Registered, path: &str) -> Option<Arc<Fetched>> {
    let mut kept = playback.resources.lock().expect("never poisoned");
    let at = kept.iter().position(|f| f.path == path)?;
    let fetched = kept.remove(at);
    kept.push(fetched.clone());
    Some(fetched)
}

/// The kept `path` of `playback` on this thread, else read now, waiting
/// for it.
fn kept_or_fetched(playback: &Arc<Registered>, path: String) -> Result<Arc<Fetched>, SpoolError> {
    match kept(playback, &path) {
        Some(fetched) => Ok(fetched),
        None => block(fetch(playback.clone(), path)),
    }
}

/// `SpoolPlayback::open_resource`: a playlist read again, for one that
/// grew, and a subtitle, which a timing change rewrites under the same
/// name (and is small); a media segment or the init, which never change
/// under their names, from the one kept if it is. Blocks on a read.
pub(crate) fn open(playback: &Arc<Registered>, path: String) -> Result<Arc<Fetched>, SpoolError> {
    if path.ends_with(".m3u8") || path.ends_with(".vtt") {
        block(fetch(playback.clone(), path))
    } else {
        kept_or_fetched(playback, path)
    }
}

/// `SpoolPlayback::read_resource`: from the kept body, else read now.
pub(crate) fn read_range(
    playback: &Arc<Registered>,
    path: String,
    offset: u64,
    length: u64,
) -> Result<Vec<u8>, SpoolError> {
    let fetched = kept_or_fetched(playback, path)?;
    let size = fetched.bytes.len() as u64;
    let start = offset.min(size) as usize;
    let end = offset.saturating_add(length).min(size) as usize;
    Ok(fetched.bytes[start..end].to_vec())
}

#[cfg(test)]
#[path = "resources_tests.rs"]
mod tests;
