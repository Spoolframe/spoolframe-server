//! What happens to a playback that its app hears of
//! ([`ManagedPlayback::events`]), and the subtitle timing changes it
//! follows.

use super::{Inner, ManagedPlayback};
use crate::tracks::{SubtitleHandling, map_selection};
use crate::{SdkError, SubtitleTimingEvent};
use spoolframe_protocol::{PlaybackHandle, PlaybackPlan};
use spoolframe_types::{MediaTracks, SubtitleSync, SubtitleTiming};
use std::sync::Arc;
use tokio::sync::broadcast;

/// Something the app acts on, from [`ManagedPlayback::events`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaybackEvent {
    /// The playback runs under a new handle (a resume, a restart, other
    /// audio): a URL for it ([`ManagedPlayback::http_playback_url`]),
    /// handed to an AirPlay or Chromecast receiver, is dead. Ask again.
    HandleChanged { handle: PlaybackHandle },
    /// The subtitle that shows was timed anew and comes as a file: read
    /// it again ([`ManagedPlayback::subtitle_file`]) and swap it in. One
    /// in the output or burned in is planned again by the SDK instead,
    /// which [`ManagedPlayback::changes`] shows.
    SubtitleRetimed { track: u32 },
}

/// Whether a new timing changes what the server serves: not a sync
/// that has only begun.
fn rereads(before: Option<&SubtitleTiming>, now: &SubtitleTiming) -> bool {
    let Some(before) = before else {
        return true;
    };
    if before == now {
        return false;
    }
    before.offset_ms != now.offset_ms || !matches!(now.sync, SubtitleSync::Syncing)
}

impl ManagedPlayback {
    /// What the app acts on: a new handle, a subtitle timed anew.
    pub fn events(&self) -> broadcast::Receiver<PlaybackEvent> {
        self.0.events.subscribe()
    }

    /// A track's timing changed, on any device: what shows is brought
    /// in line with it.
    async fn retimed(&self, event: SubtitleTimingEvent) -> Result<(), SdkError> {
        let (plan, track, position, bitmap) = {
            let mut state = self.0.state.lock();
            let showing = state.selection.and_then(|s| s.subtitle);
            if state.closed || event.id != self.0.item || showing != Some(event.track) {
                return Ok(());
            }
            let before = state.timings.insert(event.track, event.timing.clone());
            if !rereads(before.as_ref(), &event.timing) {
                return Ok(());
            }
            (
                state.playback.plan.clone(),
                event.track,
                state.position.unwrap_or_default(),
                state.options.bitmap,
            )
        };
        let selection = spoolframe_protocol::Selection {
            audio: None,
            subtitle: Some(track),
        };
        // The tracks matter only to a direct play's embedded subtitle,
        // which has no timing of its own.
        match map_selection(&MediaTracks::default(), &plan, selection).subtitle {
            SubtitleHandling::InOutput | SubtitleHandling::Burned => {
                tracing::debug!(track, "the subtitle's timing changed; planning again");
                self.change_subtitle(Some(track), bitmap, position)
                    .await
                    .map(drop)
            }
            _ if matches!(plan, PlaybackPlan::Direct { .. })
                || crate::tracks::subtitle_comes_as_file(&plan) =>
            {
                let _ = self.0.events.send(PlaybackEvent::SubtitleRetimed { track });
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

/// Follow the server's subtitle timing changes for the playback, until
/// it is dropped.
pub(super) fn follow(inner: &Arc<Inner>) {
    let Some((server, mut timings)) = inner.backend.subtitle_timings() else {
        return;
    };
    let weak = Arc::downgrade(inner);
    let mut gone = inner.gone.subscribe();
    inner.runtime.spawn(async move {
        loop {
            let received = tokio::select! {
                received = timings.recv() => received,
                // Only ever an error: the playback was dropped.
                _ = gone.changed() => return,
            };
            let event = match received {
                Ok(event) => event,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            };
            if event.server != server {
                continue;
            }
            let Some(inner) = weak.upgrade() else { return };
            let this = ManagedPlayback(inner);
            if let Err(error) = this.retimed(event).await {
                let _ = this.0.errors.send(error);
            }
        }
    });
}
