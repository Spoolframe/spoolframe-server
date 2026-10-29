//! A playback's bytes for a player that reads them itself (Media3's
//! `DataSource`), with no bridge: the SDK's readers (`media-readers`), a
//! direct playback's file by range with read-ahead, or a progressive
//! conversion's output in order as it grows.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use spoolframe_client_sdk::progressive::ProgressiveReader;
use spoolframe_client_sdk::reader::{ByteSource, MediaReader, ReadError};
use spoolframe_client_sdk::types::Delivery;
use spoolframe_client_sdk::{PlaybackPlan, SdkError};

use crate::error::SpoolError;
use crate::playback::{Registered, registered};
use crate::runtime::spawner;
use crate::session::SpoolSession;

/// One open playback's bytes. Reads block their thread until the bytes
/// are here: call them from the player's loading thread.
#[derive(uniffi::Object)]
pub struct PlaybackReader {
    source: Box<dyn ByteSource>,
    playback: Arc<Registered>,
    content_type: &'static str,
}

/// The type a player reads `plan` as: the file's, by its container, as
/// the bridge serves it; a progressive conversion's Matroska.
pub(crate) fn content_type(plan: &PlaybackPlan) -> &'static str {
    match plan {
        PlaybackPlan::Direct { container, .. } => container.mime(),
        _ => "video/x-matroska",
    }
}

#[uniffi::export]
impl SpoolSession {
    /// A reader of the open playback `token`'s bytes: a direct playback's
    /// file, or a progressive conversion's output. `length` is the item's
    /// running time, for how far ahead a file is read. An HLS conversion
    /// has no bytes to read here (`CannotPlay`): it is served on the
    /// bridge.
    pub fn playback_reader(
        &self,
        token: String,
        length: Option<Duration>,
    ) -> Result<Arc<PlaybackReader>, SpoolError> {
        reader(&token, length).map(Arc::new)
    }
}

pub(crate) fn reader(token: &str, length: Option<Duration>) -> Result<PlaybackReader, SpoolError> {
    let Some(playback) = registered(token) else {
        return Err(SpoolError::PlaybackExpired);
    };
    let plan = playback.managed.snapshot().plan;
    let managed = playback.managed.clone();
    let source: Box<dyn ByteSource> = match &plan {
        PlaybackPlan::Direct { .. } => Box::new(MediaReader::new(managed, spawner(), length)),
        PlaybackPlan::Converted {
            delivery: Delivery::Progressive,
            ..
        } => {
            let bitrate = plan.summary().video.and_then(|v| v.max_bitrate);
            Box::new(ProgressiveReader::new(managed, spawner(), bitrate))
        }
        _ => {
            return Err(SpoolError::CannotPlay {
                reasons: vec![format!("this plan has no bytes to read: {plan:?}")],
            });
        }
    };
    Ok(PlaybackReader {
        source,
        playback,
        content_type: content_type(&plan),
    })
}

#[uniffi::export]
impl PlaybackReader {
    /// Up to `length` bytes from `offset`; none at the end. Blocks until
    /// they are here. `Cancelled` while `set_flushing(true)` holds; a
    /// playback the server ended behind it is started again by the SDK
    /// and the read goes on, one the admin stopped is `PlaybackStopped`.
    pub fn read(&self, offset: u64, length: u64) -> Result<Vec<u8>, SpoolError> {
        match self.source.read_reporting(offset, length, &mut |_| {}) {
            Ok(bytes) => Ok(bytes),
            Err(ReadError::Flushing) => Err(SpoolError::Cancelled),
            Err(ReadError::Backend(error)) => {
                // As a read through the bridge does: the player asks.
                match error {
                    SdkError::PlaybackStopped => {
                        self.playback.stopped.store(true, Ordering::Relaxed)
                    }
                    SdkError::PlaybackClosed | SdkError::PlaybackExpired => {
                        self.playback.expired.store(true, Ordering::Relaxed)
                    }
                    _ => {}
                }
                Err(error.into())
            }
        }
    }

    /// Waiting reads return `Cancelled` until called with `false`: a
    /// `DataSource` closed or a seek.
    pub fn set_flushing(&self, flushing: bool) {
        self.source.set_flushing(flushing);
    }

    /// The size in bytes, for a file; `None` for a conversion's output,
    /// which grows.
    pub fn size(&self) -> Option<u64> {
        self.source.size()
    }

    /// The type a player reads it as (`video/mp4`).
    pub fn content_type(&self) -> String {
        self.content_type.to_owned()
    }
}
