//! One player's playbacks, one after another ([`PlayerSession`]).

use super::{ManagedPlayback, RestartCause};
use crate::{Playback, PlaybackBackend, PlaybackOptions, SdkError};
use parking_lot::Mutex;
use spoolframe_protocol::Selection;
use spoolframe_types::MediaId;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// What a player plays, started, replaced, restarted and ended in the
/// order asked: each waits for the one before. A start that a later
/// start or end overtook while it was asked is ended as its answer comes,
/// and fails with [`SdkError::PlaybackClosed`]: a slow server's late
/// reply never plays over what the viewer picked since.
#[derive(Clone)]
pub struct PlayerSession(Arc<Inner>);

struct Inner {
    backend: Arc<dyn PlaybackBackend>,
    /// Held through each step, so they run in order.
    line: tokio::sync::Mutex<()>,
    /// One more for each start and end asked: a start whose number is not
    /// the last one asked was overtaken.
    asked: AtomicU64,
    current: Mutex<Option<ManagedPlayback>>,
}

impl PlayerSession {
    pub fn new(backend: Arc<dyn PlaybackBackend>) -> Self {
        Self(Arc::new(Inner {
            backend,
            line: tokio::sync::Mutex::new(()),
            asked: AtomicU64::new(0),
            current: Mutex::new(None),
        }))
    }

    /// What plays now.
    pub fn current(&self) -> Option<ManagedPlayback> {
        self.0.current.lock().clone()
    }

    /// Play `item` in place of what plays: that one reports where it was
    /// and stops first.
    pub async fn start(
        &self,
        item: MediaId,
        selection: Option<Selection>,
        options: PlaybackOptions,
    ) -> Result<ManagedPlayback, SdkError> {
        let mine = self.0.asked.fetch_add(1, Ordering::SeqCst) + 1;
        let _line = self.0.line.lock().await;
        self.end_current().await;
        if self.0.asked.load(Ordering::SeqCst) != mine {
            return Err(SdkError::PlaybackClosed);
        }
        let playback =
            ManagedPlayback::open(self.0.backend.clone(), item, selection, options).await?;
        if self.0.asked.load(Ordering::SeqCst) != mine {
            // Overtaken while it was asked: it never plays.
            let _ = playback.close().await;
            return Err(SdkError::PlaybackClosed);
        }
        *self.0.current.lock() = Some(playback.clone());
        Ok(playback)
    }

    /// Start what plays again ([`ManagedPlayback::restart`]), in line.
    pub async fn restart(&self, cause: RestartCause) -> Result<Playback, SdkError> {
        let _line = self.0.line.lock().await;
        let current = self.current().ok_or(SdkError::PlaybackClosed)?;
        current.restart(cause).await
    }

    /// Stop what plays, after its last report; a start still being asked
    /// is ended as its answer comes.
    pub async fn end(&self) {
        self.0.asked.fetch_add(1, Ordering::SeqCst);
        let _line = self.0.line.lock().await;
        self.end_current().await;
    }

    async fn end_current(&self) {
        let current = self.0.current.lock().take();
        if let Some(current) = current
            && let Err(error) = current.flush_and_close().await
        {
            tracing::debug!(%error, "ending the playback");
        }
    }
}
