//! The SDK's event streams, each an object with an async `next()`: the
//! foreign side loops over it in a task (`while let n = await w.next()`)
//! and stops when it returns `None`, once the client is closed.
//!
//! A stream that fell behind says so (`Lagged`, with how many it missed)
//! rather than skipping silently: read the state again.

use std::sync::Arc;

use spoolframe_client_sdk::{self as sdk, CatalogChanges, CatalogKey};
use spoolframe_client_sdk::{LibrariesChanged, PlaylistEvent, ProfileEvent, SubtitleTimingEvent};
use spoolframe_client_sdk::{OwnProfileEvents, ProfileChange};
use tokio::sync::Mutex;
use tokio::sync::broadcast::{Receiver, error::RecvError};

use crate::client::SpoolClient;
use crate::runtime::run;
use crate::session::SpoolSession;

/// One step of a broadcast stream: an event, how many were missed, or
/// `None` once the client is gone.
enum Step<T> {
    Event(T),
    Lagged(u64),
}

impl<T> Step<T> {
    fn of(received: Result<T, RecvError>) -> Option<Self> {
        match received {
            Ok(event) => Some(Step::Event(event)),
            Err(RecvError::Lagged(missed)) => Some(Step::Lagged(missed)),
            Err(RecvError::Closed) => None,
        }
    }
}

async fn step<T: Clone + Send + 'static>(events: Arc<Mutex<Receiver<T>>>) -> Option<Step<T>> {
    Step::of(run(async move { events.lock().await.recv().await }).await)
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

/// A change to a session's own profile ([`SpoolSession::watch_own_profile`]).
#[derive(Debug, Clone, uniffi::Enum)]
pub enum OwnProfileNotice {
    /// `PinChanged` with `required` false is the PIN cleared; `Removed`,
    /// leave the profile.
    Change { change: ProfileChange },
    /// `missed` changes were dropped while the app was behind: read the
    /// profile again.
    Lagged { missed: u64 },
}

/// The changes to one session's own profile.
#[derive(uniffi::Object)]
pub struct OwnProfileWatch(Arc<Mutex<OwnProfileEvents>>);

#[uniffi::export]
impl OwnProfileWatch {
    /// The next change; `None` once the client is gone.
    pub async fn next(&self) -> Option<OwnProfileNotice> {
        let events = self.0.clone();
        let received = run(async move { events.lock().await.next().await }).await;
        Some(match Step::of(received)? {
            Step::Event(change) => OwnProfileNotice::Change { change },
            Step::Lagged(missed) => OwnProfileNotice::Lagged { missed },
        })
    }
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
/// It holds the playback weakly: kept after the app drops the playback,
/// it ends instead of keeping it, its session and the client.
#[derive(uniffi::Object)]
pub struct PlaybackShareWatch {
    /// Read by `current`, never waited on: a `next` waiting doesn't hold
    /// it up.
    playback: sdk::WeakPlayback,
    changes: Arc<Mutex<sdk::ShareWatch>>,
}

impl PlaybackShareWatch {
    pub(crate) fn new(playback: sdk::WeakPlayback, changes: sdk::ShareWatch) -> Self {
        Self {
            playback,
            changes: Arc::new(Mutex::new(changes)),
        }
    }
}

#[uniffi::export]
impl PlaybackShareWatch {
    /// The share now; `None` before the server said, or once the
    /// playback is gone.
    pub fn current(&self) -> Option<UplinkShare> {
        self.playback.share().map(|s| UplinkShare {
            max_bitrate: s.max_bitrate,
        })
    }

    /// The next move of this playback's share; `None` once the playback
    /// or the client is gone. Moves missed while behind are skipped: the
    /// next one is the share now.
    pub async fn next(&self) -> Option<UplinkShare> {
        let changes = self.changes.clone();
        run(async move { changes.lock().await.next().await })
            .await
            .map(|share| UplinkShare {
                max_bitrate: share.max_bitrate,
            })
    }
}

/// A session's kept catalog reads whose re-read behind the screen came
/// back different (`DeviceSpec.catalog_cache`): the screen showing one
/// reads it again, and gets the new value at once.
#[derive(uniffi::Object)]
pub struct CatalogWatch {
    changes: Arc<Mutex<CatalogChanges>>,
}

impl CatalogWatch {
    pub(crate) fn new(changes: CatalogChanges) -> Self {
        Self {
            changes: Arc::new(Mutex::new(changes)),
        }
    }
}

#[uniffi::export]
impl CatalogWatch {
    /// The next read that changed; `None` once the client is gone, and at
    /// once with the cache off. Changes missed while not listening are
    /// skipped: read again what the screen shows.
    pub async fn next(&self) -> Option<CatalogKey> {
        let changes = self.changes.clone();
        run(async move { changes.lock().await.recv().await.map(|c| c.key) }).await
    }
}

#[uniffi::export]
impl SpoolSession {
    /// This session's kept catalog reads that changed
    /// (`DeviceSpec.catalog_cache`); nothing comes with the cache off.
    pub fn watch_catalog(&self) -> Arc<CatalogWatch> {
        Arc::new(CatalogWatch::new(self.session.catalog_changes()))
    }

    /// Forget this session's kept catalog reads: the next reads ask the
    /// server. For a pull to refresh.
    pub fn forget_catalog(&self) {
        self.session.forget_catalog();
    }

    /// The changes to this session's own profile: its PIN set or
    /// cleared, it removed, its session ended. No other user's.
    pub fn watch_own_profile(&self) -> Arc<OwnProfileWatch> {
        Arc::new(OwnProfileWatch(Arc::new(Mutex::new(
            self.session.own_profile_events(),
        ))))
    }
}
