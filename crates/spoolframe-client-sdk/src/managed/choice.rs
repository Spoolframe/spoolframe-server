//! The viewer picks other tracks while it plays
//! ([`ManagedPlayback::change_tracks`]).

use super::{ManagedPlayback, Restart};
use crate::{Playback, SdkError};
use spoolframe_protocol::Selection;

impl ManagedPlayback {
    /// Play `selection` from where the viewer is. A new subtitle with the
    /// same audio is changed on the playback, as
    /// [`Self::change_subtitle`]; other audio needs a new playback with
    /// the tracks, which replaces this one behind every clone (it spends
    /// none of the restart budget, and a refusal leaves the old one
    /// playing). With `remember`, the pick is also kept for the item's
    /// series or movie ([`crate::Session::set_track_override`]), first.
    /// Build a new reader from the answer.
    pub async fn change_tracks(
        &self,
        selection: Selection,
        remember: bool,
    ) -> Result<Playback, SdkError> {
        if remember {
            self.0
                .backend
                .set_track_override(&self.0.item, selection)
                .await?;
        }
        let (audio, position, bitmap) = {
            let state = self.0.state.lock();
            (
                state.selection.and_then(|s| s.audio),
                state.position.or(state.options.start).unwrap_or_default(),
                state.options.bitmap,
            )
        };
        if selection.audio.is_none() || selection.audio == audio {
            return self
                .change_subtitle(selection.subtitle, bitmap, position)
                .await;
        }
        let this = self.clone();
        self.0
            .runtime
            .spawn(async move {
                let _control = this.0.control.lock().await;
                let old = this.live()?;
                this.flush_locked().await?;
                this.restart_locked(
                    &old.handle,
                    Restart::Reselect {
                        at: position,
                        selection,
                    },
                )
                .await?;
                this.0.state.lock().position = Some(position);
                Ok(this.snapshot())
            })
            .await
            .map_err(|e| SdkError::Protocol(format!("playback control task ended: {e}")))?
    }
}
