//! Opening the next item while one plays
//! ([`Session::open_playback_next`]).
//!
//! The server keeps a device's playing playback and the next one. Opened
//! together, the next could end the one playing before its player had
//! read a byte; skipping fast, a third is `Busy` until one is let go.

use super::ManagedPlayback;
use crate::{PlaybackBackend, PlaybackOptions, SdkError, Session};
use spoolframe_protocol::Selection;
use spoolframe_types::MediaId;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

/// The longest the next waits for the playing one's first read.
const FIRST_READ_WAIT: Duration = Duration::from_secs(10);
/// How many times a `Busy` start is made, waiting 500 ms more each time.
const BUSY_TRIES: u32 = 4;
const BUSY_STEP: Duration = Duration::from_millis(500);

impl ManagedPlayback {
    /// Once its player has read any of it, or it ended or was closed.
    async fn settled(&self) {
        let mut settled = self.0.settled.subscribe();
        let _ = settled.wait_for(|settled| *settled).await;
    }

    /// [`Self::open`] for the item after `after`, which plays: once
    /// `after` has been read (or 10 s passed), and through `Busy`, tried
    /// 4 times 500 ms, 1 s and 1.5 s apart.
    pub async fn open_after(
        backend: Arc<dyn PlaybackBackend>,
        item: MediaId,
        after: &ManagedPlayback,
        selection: Option<Selection>,
        options: PlaybackOptions,
    ) -> Result<Self, SdkError> {
        open_next(after, || {
            Self::open(backend.clone(), item.clone(), selection, options.clone())
        })
        .await
    }
}

impl Session {
    /// [`Session::open_playback`] for the item after `after`, which
    /// plays, as [`ManagedPlayback::open_after`]: to play gapless, or to
    /// have the next ready.
    pub async fn open_playback_next(
        &self,
        item: &MediaId,
        after: &ManagedPlayback,
        selection: Option<Selection>,
        options: PlaybackOptions,
    ) -> Result<ManagedPlayback, SdkError> {
        open_next(after, || {
            self.open_playback(item, selection, options.clone())
        })
        .await
    }
}

async fn open_next<F, Fut>(after: &ManagedPlayback, open: F) -> Result<ManagedPlayback, SdkError>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<ManagedPlayback, SdkError>>,
{
    tokio::select! {
        () = after.settled() => {}
        () = after.delay(FIRST_READ_WAIT) => {}
    }
    let mut tries = 1;
    loop {
        match open().await {
            Err(SdkError::Busy) if tries < BUSY_TRIES => {
                after.delay(BUSY_STEP * tries).await;
                tries += 1;
            }
            answer => return answer,
        }
    }
}
