//! The SDK's event streams, each an object with an async `next()`: the
//! foreign side loops over it in a task (`while let n = await w.next()`)
//! and stops when it returns `None`, once the client is closed.
//!
//! A stream that fell behind says so (`Lagged`, with how many it missed)
//! rather than skipping silently: read the state again.

use std::sync::Arc;

use spoolframe_client_sdk::{LibrariesChanged, PlaylistEvent, ProfileEvent, SubtitleTimingEvent};
use spoolframe_client_sdk::{ManagedPlayback, PlaybackShare};
use tokio::sync::Mutex;
use tokio::sync::broadcast::{Receiver, error::RecvError};

use crate::client::SpoolClient;
use crate::runtime::run;

/// One step of a broadcast stream: an event, how many were missed, or
/// `None` once the client is gone.
enum Step<T> {
    Event(T),
    Lagged(u64),
}

async fn step<T: Clone + Send + 'static>(events: Arc<Mutex<Receiver<T>>>) -> Option<Step<T>> {
    run(async move {
        match events.lock().await.recv().await {
            Ok(event) => Some(Step::Event(event)),
            Err(RecvError::Lagged(missed)) => Some(Step::Lagged(missed)),
            Err(RecvError::Closed) => None,
        }
    })
    .await
}

/// A profile's change on a server: its PIN, its removal, a session
/// ended.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum ProfileNotice {
    Event {
        event: ProfileEvent,
    },
    /// `missed` events were dropped while the app was behind: read the
    /// profiles again.
    Lagged {
        missed: u64,
    },
}

/// A playlist's change on a server (with playlists on).
#[derive(Debug, Clone, uniffi::Enum)]
pub enum PlaylistNotice {
    Event {
        event: PlaylistEvent,
    },
    /// Read the playlists again.
    Lagged {
        missed: u64,
    },
}

/// A server's library list changed (a paired server's came or went).
#[derive(Debug, Clone, uniffi::Enum)]
pub enum LibrariesNotice {
    Event {
        event: LibrariesChanged,
    },
    /// Read the libraries again.
    Lagged {
        missed: u64,
    },
}

/// A subtitle's timing changed (synced, moved, reset).
#[derive(Debug, Clone, uniffi::Enum)]
pub enum SubtitleTimingNotice {
    Event {
        event: SubtitleTimingEvent,
    },
    /// Read the item's tracks again.
    Lagged {
        missed: u64,
    },
}

macro_rules! watch {
    ($(#[$doc:meta])* $watch:ident, $event:ty, $notice:ident) => {
        $(#[$doc])*
        #[derive(uniffi::Object)]
        pub struct $watch(Arc<Mutex<Receiver<$event>>>);

        #[uniffi::export]
        impl $watch {
            /// The next change; `None` once the client is gone.
            pub async fn next(&self) -> Option<$notice> {
                Some(match step(self.0.clone()).await? {
                    Step::Event(event) => $notice::Event { event },
                    Step::Lagged(missed) => $notice::Lagged { missed },
                })
            }
        }

        impl $watch {
            fn new(events: Receiver<$event>) -> Arc<Self> {
                Arc::new(Self(Arc::new(Mutex::new(events))))
            }
        }
    };
}

watch!(
    /// Changes of the profiles signed in on this device, on every server.
    ProfileWatch,
    ProfileEvent,
    ProfileNotice
);
watch!(
    /// Changes of the playlists the users here see, on every server.
    PlaylistWatch,
    PlaylistEvent,
    PlaylistNotice
);
watch!(
    /// Library list changes, on every server.
    LibrariesWatch,
    LibrariesChanged,
    LibrariesNotice
);
watch!(
    /// Subtitle timing changes, on every server.
    SubtitleTimingWatch,
    SubtitleTimingEvent,
    SubtitleTimingNotice
);

#[uniffi::export]
impl SpoolClient {
    /// Profile events (`spoolframe:profile-events`): a PIN set or
    /// cleared, a profile removed, a session ended, as servers say.
    pub fn watch_profiles(&self) -> Arc<ProfileWatch> {
        ProfileWatch::new(self.client.profile_events())
    }

    pub fn watch_playlists(&self) -> Arc<PlaylistWatch> {
        PlaylistWatch::new(self.client.playlist_events())
    }

    pub fn watch_libraries(&self) -> Arc<LibrariesWatch> {
        LibrariesWatch::new(self.client.libraries_events())
    }

    pub fn watch_subtitle_timing(&self) -> Arc<SubtitleTimingWatch> {
        SubtitleTimingWatch::new(self.client.subtitle_timing_events())
    }
}

/// A playback's share of the server's uplink: the bits per second it may
/// take, `None` for no limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct UplinkShare {
    pub max_bitrate: Option<u64>,
}

/// One playback's share of the server's uplink, as the server moves it.
#[derive(uniffi::Object)]
pub struct PlaybackShareWatch {
    playback: ManagedPlayback,
    shares: Arc<Mutex<Receiver<PlaybackShare>>>,
}

impl PlaybackShareWatch {
    pub(crate) fn new(playback: ManagedPlayback, shares: Receiver<PlaybackShare>) -> Self {
        Self {
            playback,
            shares: Arc::new(Mutex::new(shares)),
        }
    }
}

#[uniffi::export]
impl PlaybackShareWatch {
    /// The share now; `None` before the server said.
    pub fn current(&self) -> Option<UplinkShare> {
        self.playback.share().map(|s| UplinkShare {
            max_bitrate: s.max_bitrate,
        })
    }

    /// The next move of this playback's share; `None` once the client is
    /// gone. Moves missed while behind are skipped: the next one is the
    /// share now.
    pub async fn next(&self) -> Option<UplinkShare> {
        let playback = self.playback.clone();
        let shares = self.shares.clone();
        run(async move {
            let mut shares = shares.lock().await;
            loop {
                match shares.recv().await {
                    Ok(share) if playback.is_handle(&share.handle) => {
                        return Some(UplinkShare {
                            max_bitrate: share.max_bitrate,
                        });
                    }
                    Ok(_) | Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => return None,
                }
            }
        })
        .await
    }
}
