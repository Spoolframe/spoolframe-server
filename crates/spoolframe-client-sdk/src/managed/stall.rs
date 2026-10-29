//! A direct play the app's own player found stalled
//! ([`ManagedPlayback::stalled`]).

use super::ManagedPlayback;
use crate::{Playback, PlaybackQuality, SdkError};
use spoolframe_protocol::PlaybackPlan;
use spoolframe_protocol::playback::LINK_SHARE;
use std::time::Duration;

impl ManagedPlayback {
    /// How long a stall lasts before the link is looked at.
    pub const STALL_LIMIT_AFTER: Duration = Duration::from_secs(3);

    /// The app's player has waited `stalled_for` for bytes of a direct
    /// play of an item `length` long. Past [`Self::STALL_LIMIT_AFTER`],
    /// if the link's share ([`LINK_SHARE`] of its measured rate) is under
    /// the file's bitrate, the playback is planned again for the link,
    /// once: the new playback, for a new reader, else `None`. The SDK's
    /// own direct reader does this itself. An ask that fails may be made
    /// again at the next stall.
    pub async fn stalled(
        &self,
        stalled_for: Duration,
        length: Duration,
    ) -> Result<Option<Playback>, SdkError> {
        let Some(link_bitrate) = self.too_slow_for_file(stalled_for, length)? else {
            return Ok(None);
        };
        tracing::info!(
            link_bitrate,
            "the link cannot carry the file; asking for a plan it can"
        );
        match self.limit(link_bitrate).await {
            Ok(next) if !matches!(next.plan, PlaybackPlan::Direct { .. }) => Ok(Some(next)),
            Ok(_) => Ok(None),
            Err(error) => {
                self.0.state.lock().stall_limited = false;
                Err(error)
            }
        }
    }

    /// The link's rate, bits/s, when it cannot carry the file and no ask
    /// was made yet; it marks the ask made.
    fn too_slow_for_file(
        &self,
        stalled_for: Duration,
        length: Duration,
    ) -> Result<Option<u64>, SdkError> {
        let playback = self.live()?;
        if stalled_for < Self::STALL_LIMIT_AFTER
            || length.is_zero()
            || !matches!(playback.plan, PlaybackPlan::Direct { .. })
            || self.quality() != PlaybackQuality::Auto
        {
            return Ok(None);
        }
        let Some(rate) = self
            .0
            .backend
            .link_meter()
            .and_then(|meter| meter.rate(std::time::Instant::now()))
        else {
            return Ok(None);
        };
        // Bytes per second, as the meter counts.
        let file = playback.size as f64 / length.as_secs_f64();
        if rate * LINK_SHARE >= file {
            return Ok(None);
        }
        let mut state = self.0.state.lock();
        if std::mem::replace(&mut state.stall_limited, true) {
            return Ok(None);
        }
        Ok(Some((rate * 8.0) as u64))
    }
}
