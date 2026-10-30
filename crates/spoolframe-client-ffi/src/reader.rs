//! A playback's bytes for a player that reads them itself (Media3's
//! `DataSource`), with no bridge: the SDK's readers (`media-readers`), a
//! direct playback's file by range with read-ahead, or a progressive
//! conversion's output in order as it grows. A `SpoolPlayback` holds one
//! for the plan it is on.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use spoolframe_client_sdk::progressive::ProgressiveReader;
use spoolframe_client_sdk::reader::{ByteSource, MediaReader, ReadError};
use spoolframe_client_sdk::types::Delivery;
use spoolframe_client_sdk::{PlaybackPlan, SdkError};

use crate::error::SpoolError;
use crate::playback::Registered;
use crate::runtime::spawner;

/// One open playback's bytes. Reads block their thread until the bytes
/// are here: call them from the player's loading thread.
pub(crate) struct PlaybackReader {
    source: Box<dyn ByteSource>,
    playback: Arc<Registered>,
}

/// The type a player reads `plan` as: the file's, by its container, as
/// the bridge serves it; a progressive conversion's Matroska.
pub(crate) fn content_type(plan: &PlaybackPlan) -> &'static str {
    match plan {
        PlaybackPlan::Direct { container, .. } => container.mime(),
        _ => "video/x-matroska",
    }
}

/// A reader of `playback`'s bytes, the plan registered: a direct playback's
/// file, or a progressive conversion's output. `length` is the item's
/// running time, for how far ahead a file is read. An HLS conversion has
/// no bytes to read here (`CannotPlay`): it is read by name.
pub(crate) fn reader(
    playback: Arc<Registered>,
    length: Option<Duration>,
) -> Result<PlaybackReader, SpoolError> {
    // The SDK's readers refuse what they read once the playback moves on
    // from the generation they are made in: made in the one registered
    // under `token`, or not at all, even should it move as they are made.
    let generation = playback.generation();
    let plan = playback.managed.snapshot_at(generation)?.plan;
    let managed = playback.managed.clone();
    let source: Box<dyn ByteSource> = match &plan {
        PlaybackPlan::Direct { .. } => Box::new(MediaReader::for_generation(
            managed,
            spawner(),
            length,
            generation,
        )?),
        PlaybackPlan::Converted {
            delivery: Delivery::Progressive,
            ..
        } => {
            let bitrate = plan.summary().video.and_then(|v| v.max_bitrate);
            Box::new(ProgressiveReader::for_generation(
                managed,
                spawner(),
                bitrate,
                generation,
            )?)
        }
        _ => {
            return Err(SpoolError::CannotPlay {
                reasons: vec![format!("this plan has no bytes to read: {plan:?}")],
            });
        }
    };
    Ok(PlaybackReader { source, playback })
}

impl PlaybackReader {
    /// Up to `length` bytes from `offset`; none at the end. Blocks until
    /// they are here. `Cancelled` while `set_flushing(true)` holds;
    /// `Replanned` once the playback moved on (planned again, or started
    /// again by the SDK after the server ended it); one the admin stopped
    /// is `PlaybackStopped`.
    pub(crate) fn read(&self, offset: u64, length: u64) -> Result<Vec<u8>, SpoolError> {
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
    pub(crate) fn set_flushing(&self, flushing: bool) {
        self.source.set_flushing(flushing);
    }
}
