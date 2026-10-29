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
use crate::playback::registered;
use crate::runtime::run;
use crate::session::SpoolSession;

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

/// `path` of `token`'s playback, read whole now and kept for its ranges,
/// in place of the one kept for `path` before (a playlist that grew);
/// the least recently used goes past [`KEPT`].
pub(crate) async fn fetch(token: String, path: String) -> Result<Arc<Fetched>, SpoolError> {
    let Some(playback) = registered(&token) else {
        return Err(SpoolError::PlaybackExpired);
    };
    let read = run({
        let (token, path) = (token.clone(), path.clone());
        async move { BRIDGE.resource(&token, &path).await }
    })
    .await;
    let (content_type, bytes) = match read {
        Ok(Some(found)) => found,
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

/// The kept `path` of `token`'s playback, else read now.
pub(crate) async fn kept(token: String, path: String) -> Result<Arc<Fetched>, SpoolError> {
    let kept = registered(&token).and_then(|p| {
        let mut kept = p.resources.lock().expect("never poisoned");
        let at = kept.iter().position(|f| f.path == path)?;
        let fetched = kept.remove(at);
        kept.push(fetched.clone());
        Some(fetched)
    });
    match kept {
        Some(fetched) => Ok(fetched),
        None => fetch(token, path).await,
    }
}

/// [`SpoolSession::open_playback_resource`]: a playlist read again, for
/// one that grew, and a subtitle, which a timing change rewrites under
/// the same name (and is small); a media segment or the init, which never
/// change under their names, from the one kept if it is.
pub(crate) async fn open(token: String, path: String) -> Result<Arc<Fetched>, SpoolError> {
    if path.ends_with(".m3u8") || path.ends_with(".vtt") {
        fetch(token, path).await
    } else {
        kept(token, path).await
    }
}

/// [`SpoolSession::read_playback_resource`].
pub(crate) async fn read_range(
    token: String,
    path: String,
    offset: u64,
    length: u64,
) -> Result<Vec<u8>, SpoolError> {
    let fetched = kept(token, path).await?;
    let size = fetched.bytes.len() as u64;
    let start = offset.min(size) as usize;
    let end = offset.saturating_add(length).min(size) as usize;
    Ok(fetched.bytes[start..end].to_vec())
}

#[uniffi::export]
impl SpoolSession {
    /// Open `path` of the open playback `token` (`master.m3u8`, or a name
    /// a playlist gives): it is read whole now, as a `DataSource`'s open
    /// does, and kept for `read_playback_resource`'s ranges: the last few
    /// opened, each by its path. A playlist or subtitle opened again is
    /// read again; a media segment kept is not.
    pub async fn open_playback_resource(
        &self,
        token: String,
        path: String,
    ) -> Result<PlaybackResource, SpoolError> {
        let fetched = open(token, path).await?;
        Ok(PlaybackResource {
            content_type: fetched.content_type.to_owned(),
            length: fetched.bytes.len() as u64,
        })
    }

    /// Up to `length` bytes of `path` of the playback `token` from
    /// `offset`, none past its end: from the body `open_playback_resource`
    /// kept for `path` (the one its length was), so a run of ranges reads
    /// it once.
    pub async fn read_playback_resource(
        &self,
        token: String,
        path: String,
        offset: u64,
        length: u64,
    ) -> Result<Vec<u8>, SpoolError> {
        read_range(token, path, offset, length).await
    }
}

#[cfg(test)]
#[path = "resources_tests.rs"]
mod tests;
