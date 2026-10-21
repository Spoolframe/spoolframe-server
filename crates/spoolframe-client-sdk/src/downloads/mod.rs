//! Downloads for offline viewing (PX-38, feature `spoolframe:downloads`):
//! films and episodes kept on this device to play with no server.
//!
//! [`Downloads::start`] asks the server to prepare the file
//! (`PrepareDownload`) and keeps the item's details, markers, tracks and
//! artwork beside it. A manager task the [`Client`] owns then follows the
//! server's preparation (`DownloadChanged` events, and `DownloadStatus`
//! every few seconds), and once the file is ready reads it in 8 MiB
//! pieces into `media.part`, from where it stopped. The whole file's
//! SHA-256 is checked against the server's before it becomes
//! `media.mp4` or `media.mkv`; then the server is told it is finished
//! (`CancelDownload`). One download moves at a time, the oldest first.
//!
//! The manager runs while the client is open, and stops at
//! [`Client::close`]: a transfer cut off, by that or by a crash, goes on
//! from the last saved byte the next time. [`Downloads::run_until_done`]
//! is for a background task's time (iOS): it returns once nothing is left
//! to move.
//!
//! Everything about a download is on disk
//! ([`crate::ClientOptions::downloads_dir`]), so [`Downloads::list`] needs
//! no server. On iOS the app gives a directory in Application Support and
//! marks it excluded from backup; on Linux,
//! `$XDG_DATA_HOME/spoolframe/downloads`.
//!
//! Watching offline: [`Downloads::record_position`] and
//! [`Downloads::set_watched`] keep where the user got to, and
//! [`Downloads::sync_watch`] sends it (`SyncOfflineWatch`); the manager
//! sends it by itself when a session of that user on that server is made
//! or found, and when it starts. The server decides what counts as
//! watched; what is kept here is for showing.
//!
//! Music (PX-51, `spoolframe:music-downloads`) rides the same entries
//! and manager: see the `music` module for downloads of albums, artists,
//! playlists and Liked, and `listening` for tracks kept as they play.
//! On a mobile network ([`Downloads::set_network`]) nothing moves unless
//! [`DownloadSettings::allow_cellular`].

mod listening;
mod manager;
mod music;
mod space;
mod store;
mod writer;

pub(crate) use listening::Keeper;
pub use music::{
    CacheSpace, DEFAULT_CACHE_MAX_BYTES, DownloadSettings, MusicDownload, MusicQuality,
    MusicSynced, MusicTarget, NetworkKind, OfflineAlbum, OfflineArtist, OfflineMark, OfflineMusic,
    OfflinePlaylist, OfflineTrack, SyncedPlay,
};

use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use iroh::EndpointId;
use serde::{Deserialize, Serialize};
use spoolframe_protocol::downloads::{
    DownloadFile, DownloadOptionsInfo, DownloadSource, DownloadState, DownloadTracks, OfflineWatch,
    OfflineWatchOutcome,
};
use spoolframe_protocol::features::DOWNLOADS;
use spoolframe_protocol::{Reply, Request};
use spoolframe_types::{ArtworkKind, Item, Marker, MediaId, MediaTracks};
use tokio::sync::{Notify, broadcast, watch};

use crate::client::{Client, Inner};
use crate::portable::unix_now as now;
use crate::session::SessionParts;
use crate::{SdkError, Session};
use store::{EntryKind, Hold, Manifest, Store};

/// Where a download is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DownloadPhase {
    /// The server is making the file: `server` is where it says it is
    /// (waiting, converting, writing).
    Preparing { server: DownloadState },
    /// The file is coming to this device: [`LocalDownload::received`] of
    /// [`LocalDownload::size`] bytes.
    Transferring,
    /// Stopped by the user ([`Downloads::pause`]) until
    /// [`Downloads::resume`].
    Paused,
    /// On this device, whole and checked: play
    /// [`LocalDownload::file_path`].
    Done,
    /// It could not be made or kept, for `reason`, said to the user as it
    /// is. [`Downloads::retry`] tries again.
    Failed { reason: String },
}

impl DownloadPhase {
    /// The manager moves it on.
    fn is_active(&self) -> bool {
        matches!(self, Self::Preparing { .. } | Self::Transferring)
    }
}

/// Where the user got to in a download while watching it here. Sent to
/// the server by [`Downloads::sync_watch`] while `pending`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalWatch {
    /// Where it stopped, in milliseconds; `None` when only `watched` was
    /// set, which starts it over.
    pub position_ms: Option<u64>,
    /// What the user marked it on this device, or the server's watched
    /// as of the download or the last sync; `None` once a position is
    /// recorded after it. For showing only: with a position the server
    /// judges watched by its own rules (the credits), which this SDK does
    /// not know.
    pub watched: Option<bool>,
    /// When, Unix seconds by this device's clock.
    pub at: i64,
    /// Not sent to the server yet.
    pub pending: bool,
}

impl LocalWatch {
    /// The server's state of `item`, as of now.
    fn of(item: &Item) -> Self {
        let playback = match item {
            Item::Movie(m) => Some(&m.playback),
            Item::Episode(e) => Some(&e.playback),
            _ => None,
        };
        Self {
            position_ms: playback
                .and_then(|p| p.position)
                .map(|p| p.as_millis() as u64),
            watched: playback.map(|p| p.watched),
            at: now(),
            pending: false,
        }
    }

    /// The entry `SyncOfflineWatch` sends for `item`; `None` with nothing
    /// to send.
    pub(crate) fn entry(&self, item: &Item) -> Option<OfflineWatch> {
        (self.position_ms.is_some() || self.watched.is_some()).then(|| OfflineWatch {
            id: item.id().clone(),
            position_ms: self.position_ms,
            watched: self.watched,
            at: self.at,
        })
    }
}

/// A picture kept with a download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalArtwork {
    /// What it is of: the download's item, or an episode's show or
    /// season.
    pub item: MediaId,
    pub kind: ArtworkKind,
    pub path: PathBuf,
}

/// A download on this device, as [`Downloads::list`] gives it: all an app
/// needs to show and play it with no server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalDownload {
    /// Its key here: what the other calls take.
    pub key: String,
    /// The server's key.
    pub server: String,
    pub user_id: String,
    pub username: String,
    /// The film or episode, as the server had it when it was downloaded.
    pub item: Item,
    /// An episode's show and season.
    pub show: Option<Item>,
    pub season: Option<Item>,
    /// Its intro and credits.
    pub markers: Vec<Marker>,
    pub artwork: Vec<LocalArtwork>,
    /// The item's tracks: the file's are [`DownloadFile::tracks`], by
    /// their places in these.
    pub tracks: MediaTracks,
    pub source: DownloadSource,
    /// The tracks asked for.
    pub chosen: DownloadTracks,
    /// What the file is, once the server said.
    pub file: Option<DownloadFile>,
    pub phase: DownloadPhase,
    /// How many bytes of the file are here.
    pub received: u64,
    /// The file's size, once the server has it ready.
    pub size: Option<u64>,
    /// The file, once [`DownloadPhase::Done`].
    pub file_path: Option<PathBuf>,
    /// `file_path` as a `file://` URL, for GStreamer's `uri` and
    /// AVPlayer.
    pub file_url: Option<String>,
    pub watch: LocalWatch,
    /// Why it is not moving, while the manager waits to try again (the
    /// server cannot be reached, the user must sign in again).
    pub note: Option<String>,
    /// When it was started, Unix seconds by this device's clock.
    pub created_at: i64,
}

/// A download changed: what [`Client::download_events`] gives.
// Owned values, no boxes: what a UniFFI layer passes on as it is. An
// event is sent at most once per piece read.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadEvent {
    /// Its phase, progress, watch or note: show it again.
    Changed(LocalDownload),
    /// It was cancelled or deleted.
    Removed { key: String },
}

/// The room downloads have ([`Downloads::space`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadSpace {
    /// Free bytes where the downloads are: the app's own figure
    /// ([`Downloads::set_free_space`]) if it gave one, else the file
    /// system's.
    pub available: u64,
    /// Bytes the downloads take.
    pub used: u64,
}

/// What became of one item's offline watch ([`Downloads::sync_watch`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncedWatch {
    pub item: MediaId,
    /// `Older`: the server has something newer, which is now the
    /// download's [`LocalWatch`].
    pub outcome: OfflineWatchOutcome,
}

/// This device's downloads ([`Client::downloads`]). Cheap to clone.
#[derive(Clone)]
pub struct Downloads {
    client: Client,
}

impl std::fmt::Debug for Downloads {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Downloads").finish_non_exhaustive()
    }
}

impl Client {
    /// This device's downloads (PX-38), in
    /// [`crate::ClientOptions::downloads_dir`]. Without one, every call
    /// but [`Downloads::list`] (empty) is [`SdkError::Downloads`].
    pub fn downloads(&self) -> Downloads {
        Downloads {
            client: self.clone(),
        }
    }

    /// Each change of a download: its phase, its progress (after each
    /// piece read), its watch, or its removal.
    pub fn download_events(&self) -> broadcast::Receiver<DownloadEvent> {
        self.inner.downloads.events.subscribe()
    }
}

impl Session {
    /// [`SdkError::Unsupported`] unless the session has downloads.
    fn downloads_offered(&self) -> Result<(), SdkError> {
        if self.features.downloads {
            Ok(())
        } else {
            Err(SdkError::Unsupported {
                feature: DOWNLOADS.into(),
            })
        }
    }

    /// What this device may download of `id`, a film or an episode: its
    /// tracks, the choice to tick first, the profiles, what original
    /// quality would be, and the subtitles a download would leave out.
    pub async fn download_options(&self, id: &MediaId) -> Result<DownloadOptionsInfo, SdkError> {
        self.downloads_offered()?;
        match self
            .call(Request::DownloadOptions { id: id.clone() })
            .await?
        {
            Reply::DownloadOptions(info) => Ok(info),
            other => Err(SdkError::Protocol(format!("unexpected reply {other:?}"))),
        }
    }
}

impl Downloads {
    fn shared(&self) -> &Arc<Shared> {
        &self.client.inner.downloads
    }

    /// Download `id` with `tracks` from `source`, as `session`'s user: it
    /// is prepared on the server now (so a refusal is this call's error),
    /// its details and artwork are kept, and the manager moves it on.
    /// Started again, the same download as before.
    pub async fn start(
        &self,
        session: &Session,
        id: &MediaId,
        source: DownloadSource,
        tracks: DownloadTracks,
    ) -> Result<LocalDownload, SdkError> {
        let shared = self.shared();
        let store = shared.store()?;
        session.downloads_offered()?;
        let kind = EntryKind::of(id)
            .filter(|kind| *kind != EntryKind::Track)
            .ok_or_else(|| {
                SdkError::InvalidInput(format!(
                    "{id}: only films and episodes download here; music with start_music"
                ))
            })?;
        let tracks = tracks.normalized();
        let server = session.server();
        let user = session.user().clone();
        let key = store::key_of(&server, &user.user_id, kind, id, &source, &tracks);
        shared.session_made(session);
        if store.find(&key).is_some() {
            // Held by something else so far, it is the user's download
            // now too.
            let m = shared
                .update(&key, |m| {
                    m.add_hold(&Hold::download());
                    Ok(())
                })
                .await?;
            return Ok(shared.view(&m));
        }
        let detail = session.item(id).await?;
        let media_tracks = session.tracks(id).await?;
        let info = manager::prepare(session, id, &source, &tracks).await?;
        let mut m = Store::manifest(
            kind,
            key,
            server,
            user.user_id,
            user.username,
            detail.item.clone(),
            media_tracks,
            source,
            tracks,
            LocalWatch::of(&detail.item),
            now(),
        );
        m.markers = detail.markers;
        (m.show, m.season) = manager::parents(session, &detail.item).await;
        manager::take(&mut m, info);
        m.artwork = manager::fetch_artwork(session, store, &m, &mut HashMap::new()).await;
        {
            let _writing = shared.manifests.lock().await;
            store.write(&m).map_err(disk)?;
        }
        shared.emit(&m);
        shared.wake.notify_one();
        Ok(shared.view(&m))
    }

    /// Stop moving download `key` until [`Downloads::resume`]. What
    /// arrived is kept.
    pub async fn pause(&self, key: &str) -> Result<LocalDownload, SdkError> {
        let shared = self.shared();
        let m = shared
            .update(key, |m| {
                if !m.phase.is_active() {
                    return Err(SdkError::InvalidInput(format!(
                        "download {key} is not under way"
                    )));
                }
                m.phase = DownloadPhase::Paused;
                Ok(())
            })
            .await?;
        shared.stop(key);
        Ok(shared.view(&m))
    }

    /// Go on with paused download `key`.
    pub async fn resume(&self, key: &str) -> Result<LocalDownload, SdkError> {
        self.restart(
            key,
            |phase| matches!(phase, DownloadPhase::Paused),
            "paused",
        )
        .await
    }

    /// Try failed download `key` again: it is prepared again on the
    /// server, and goes on from what arrived if the file is the same. One
    /// under way waiting to try again tries now.
    pub async fn retry(&self, key: &str) -> Result<LocalDownload, SdkError> {
        self.restart(
            key,
            |phase| matches!(phase, DownloadPhase::Failed { .. }) || phase.is_active(),
            "failed",
        )
        .await
    }

    async fn restart(
        &self,
        key: &str,
        may: impl FnOnce(&DownloadPhase) -> bool,
        not: &str,
    ) -> Result<LocalDownload, SdkError> {
        let shared = self.shared();
        let m = shared
            .update(key, |m| {
                if !may(&m.phase) {
                    return Err(SdkError::InvalidInput(format!(
                        "download {key} is not {not}"
                    )));
                }
                if !m.phase.is_active() {
                    m.phase = DownloadPhase::Preparing {
                        server: DownloadState::Waiting,
                    };
                }
                m.note = None;
                Ok(())
            })
            .await?;
        shared.stopping.lock().remove(key);
        shared.retry_at.lock().remove(key);
        shared.wake.notify_one();
        Ok(shared.view(&m))
    }

    /// Stop download `key`, not finished, and take it off this device;
    /// the server is told, if it can be reached now. A finished one is
    /// [`Downloads::delete`]d.
    pub async fn cancel(&self, key: &str) -> Result<(), SdkError> {
        let m = self.find(key)?;
        if m.phase == DownloadPhase::Done {
            return Err(SdkError::InvalidInput(format!(
                "download {key} is finished: delete it"
            )));
        }
        self.release(m, &Hold::download()).await
    }

    /// Take download `key` off this device, in any phase. One not
    /// finished is cancelled on the server, if it can be reached now. Its
    /// offline watch not sent yet is kept for the next sync. A file that
    /// something else still holds (a track in an album downloaded, or
    /// kept from listening) stays, for it.
    pub async fn delete(&self, key: &str) -> Result<(), SdkError> {
        let m = self.find(key)?;
        self.release(m, &Hold::download()).await
    }

    fn find(&self, key: &str) -> Result<Manifest, SdkError> {
        self.shared()
            .store()?
            .find(key)
            .ok_or_else(|| SdkError::NotFound(format!("download {key}")))
    }

    /// Let go of `hold` on `m`: with nothing left holding it, it is taken
    /// off this device, and one not finished is cancelled on the server
    /// if it can be reached now.
    async fn release(&self, m: Manifest, hold: &Hold) -> Result<(), SdkError> {
        let shared = self.shared();
        let store = shared.store()?;
        let key = m.key.clone();
        if !m.holds.iter().all(|h| h == hold) {
            // Something else holds it: only the hold goes, whatever the
            // manager is doing with it.
            shared
                .update(&key, |m| {
                    m.holds.retain(|h| h != hold);
                    Ok(())
                })
                .await?;
            return Ok(());
        }
        shared.stop(&key);
        let claim = shared.claim(&key);
        let _claimed = claim.lock().await;
        let m = store.find(&key).unwrap_or(m);
        if m.phase != DownloadPhase::Done
            && let Some(id) = &m.id
        {
            manager::cancel_if_reachable(&self.client, shared, &m, id).await;
        }
        let mut m = m;
        let removed = {
            let _writing = shared.manifests.lock().await;
            store.release(&mut m, hold).map_err(disk)?
        };
        shared.stopping.lock().remove(&key);
        shared.retry_at.lock().remove(&key);
        if removed {
            let _ = shared.events.send(DownloadEvent::Removed { key });
        } else {
            shared.emit(&m);
        }
        Ok(())
    }

    /// Every film and episode downloaded on this device, oldest first,
    /// from disk alone: no server is asked. None without a downloads
    /// directory. Music is [`Downloads::music_downloads`] and
    /// [`Downloads::music_tracks`].
    pub fn list(&self) -> Vec<LocalDownload> {
        let shared = self.shared();
        match &shared.store {
            Some(store) => store
                .scan()
                .iter()
                .filter(|m| m.kind != EntryKind::Track)
                .map(|m| shared.view(m))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Download `key`, from disk.
    pub fn get(&self, key: &str) -> Result<LocalDownload, SdkError> {
        Ok(self.shared().view(&self.find(key)?))
    }

    /// Move every download on until none is preparing or transferring
    /// (each is done, failed or paused): for a background task's time.
    /// Ones waiting to try again try now. Cancel it (drop the future)
    /// when the time is up; nothing is lost.
    pub async fn run_until_done(&self) -> Result<(), SdkError> {
        let shared = self.shared();
        let store = shared.store()?;
        shared.retry_at.lock().clear();
        shared.wake.notify_one();
        let mut closed = shared.closed.subscribe();
        loop {
            let mut events = shared.events.subscribe();
            if store.active().is_empty() {
                return Ok(());
            }
            tokio::select! {
                _ = events.recv() => {}
                () = tokio::time::sleep(Duration::from_secs(1)) => {}
                _ = closed.wait_for(|closed| *closed) => {
                    return Err(SdkError::Downloads("the client was closed".into()));
                }
            }
        }
    }

    /// How much room there is for downloads, and how much they take.
    pub fn space(&self) -> Result<DownloadSpace, SdkError> {
        let shared = self.shared();
        let store = shared.store()?;
        Ok(DownloadSpace {
            available: shared.available(store)?,
            used: store.used(),
        })
    }

    /// The free bytes to go by instead of the file system's, or `None` to
    /// go by it again: on iOS, `volumeAvailableCapacityForImportantUsage`,
    /// which counts what the system would purge. A download needs its
    /// size and 1 GiB more free before it is transferred.
    pub fn set_free_space(&self, available: Option<u64>) {
        *self.shared().free_space.lock() = available;
    }

    /// The user got to `position` in download `key`, playing it here.
    pub async fn record_position(
        &self,
        key: &str,
        position: Duration,
    ) -> Result<LocalDownload, SdkError> {
        self.watched(key, Some(position.as_millis() as u64), None)
            .await
    }

    /// The user marked download `key` watched or not, here. Unwatched
    /// starts it over.
    pub async fn set_watched(&self, key: &str, watched: bool) -> Result<LocalDownload, SdkError> {
        self.watched(key, None, Some(watched)).await
    }

    async fn watched(
        &self,
        key: &str,
        position_ms: Option<u64>,
        watched: Option<bool>,
    ) -> Result<LocalDownload, SdkError> {
        let shared = self.shared();
        let m = shared
            .update(key, |m| {
                m.watch = LocalWatch {
                    position_ms,
                    watched,
                    at: now(),
                    pending: true,
                };
                Ok(())
            })
            .await?;
        Ok(shared.view(&m))
    }

    /// Send what `session`'s user watched offline on its server, for every
    /// download of theirs there (and ones deleted since): what became of
    /// each item. Newest wins on the server; one it has newer (`Older`)
    /// takes the server's state here. Nothing to send asks nothing.
    pub async fn sync_watch(&self, session: &Session) -> Result<Vec<SyncedWatch>, SdkError> {
        session.downloads_offered()?;
        manager::sync(self.shared(), session).await
    }
}

/// A server's `DownloadChanged`, for the manager.
#[derive(Debug, Clone)]
struct ServerChange {
    server: EndpointId,
    id: String,
}

/// What the client keeps for its downloads, and the manager shares.
pub(crate) struct Shared {
    store: Option<Store>,
    /// Held across each read, change and write of a manifest.
    manifests: tokio::sync::Mutex<()>,
    /// Something for the manager to do.
    wake: Notify,
    events: broadcast::Sender<DownloadEvent>,
    server_events: broadcast::Sender<ServerChange>,
    /// Each user's session on each server, as last made or found.
    sessions: Mutex<HashMap<(EndpointId, String), SessionParts>>,
    /// Downloads the manager is to let go of (paused, being removed).
    stopping: Mutex<HashSet<String>>,
    /// Bumped to cut the manager's waits short.
    interrupt: watch::Sender<u64>,
    /// Held while a download is moved or removed.
    claims: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Downloads waiting to try again: when, and the wait after that.
    retry_at: Mutex<HashMap<String, (tokio::time::Instant, Duration)>>,
    /// Bytes on disk of the download being transferred, ahead of its
    /// manifest.
    received: Mutex<HashMap<String, u64>>,
    /// Servers and users whose offline watch is to be sent.
    sync_wanted: Mutex<HashSet<(EndpointId, String)>>,
    /// [`Downloads::set_free_space`].
    free_space: Mutex<Option<u64>>,
    /// [`Downloads::settings`], once read.
    settings: Mutex<Option<DownloadSettings>>,
    /// [`Downloads::set_network`].
    network: Mutex<NetworkKind>,
    /// Followed music downloads to ask about again while connected
    /// (PX-51).
    follow_due: Mutex<music::FollowDue>,
    /// Something for the follower to do.
    follow_wake: Notify,
    /// Pictures of tracks kept from listening: the next track of an album
    /// links to its cover here instead of fetching it.
    kept_art: Mutex<manager::KeptArt>,
    /// Held while a user's followed downloads are brought up to date: two
    /// at once would each compare with the same tracks kept.
    following: tokio::sync::Mutex<()>,
    closed: watch::Sender<bool>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// The follower ([`music::follow_while_connected`]).
    follower: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Shared {
    pub(crate) fn new(dir: Option<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            store: dir.map(Store::new),
            manifests: tokio::sync::Mutex::default(),
            wake: Notify::new(),
            events: broadcast::channel(64).0,
            server_events: broadcast::channel(64).0,
            sessions: Mutex::default(),
            stopping: Mutex::default(),
            interrupt: watch::Sender::new(0),
            claims: Mutex::default(),
            retry_at: Mutex::default(),
            received: Mutex::default(),
            sync_wanted: Mutex::default(),
            free_space: Mutex::default(),
            settings: Mutex::default(),
            network: Mutex::default(),
            follow_due: Mutex::default(),
            follow_wake: Notify::new(),
            kept_art: Mutex::default(),
            following: tokio::sync::Mutex::default(),
            closed: watch::Sender::new(false),
            task: Mutex::default(),
            follower: Mutex::default(),
        })
    }

    /// Start the manager and the follower, with a downloads directory:
    /// each holds the client only while it moves a download or asks the
    /// server.
    pub(crate) fn start(self: &Arc<Self>, client: std::sync::Weak<Inner>) {
        if self.store.is_some() {
            *self.task.lock() = Some(tokio::spawn(manager::run(client.clone(), self.clone())));
            *self.follower.lock() = Some(tokio::spawn(music::follow_while_connected(
                client,
                self.clone(),
            )));
        }
    }

    /// Stop the manager: it lets go between two steps, saving where the
    /// transfer got to.
    pub(crate) async fn close(&self) {
        self.closed.send_replace(true);
        self.interrupt.send_modify(|n| *n += 1);
        self.wake.notify_one();
        self.follow_wake.notify_one();
        let tasks = [self.task.lock().take(), self.follower.lock().take()];
        for mut task in tasks.into_iter().flatten() {
            if tokio::time::timeout(Duration::from_secs(5), &mut task)
                .await
                .is_err()
            {
                task.abort();
            }
        }
    }

    fn store(&self) -> Result<&Store, SdkError> {
        self.store
            .as_ref()
            .ok_or_else(|| SdkError::Downloads("no ClientOptions::downloads_dir".into()))
    }

    /// A session was made or found: the manager may use it, and sends its
    /// user's offline watch.
    pub(crate) fn session_made(&self, session: &Session) {
        if self.store.is_none() || !session.features.downloads {
            return;
        }
        let key = (session.server, session.user.user_id.clone());
        self.sessions.lock().insert(key.clone(), session.parts());
        self.sync_wanted.lock().insert(key);
        self.wake.notify_one();
    }

    /// Server `server` told that its download `id` moved.
    pub(crate) fn server_event(&self, server: EndpointId, id: String) {
        let _ = self.server_events.send(ServerChange { server, id });
    }

    /// The manager is to let go of `key`.
    fn stop(&self, key: &str) {
        self.stopping.lock().insert(key.to_owned());
        self.interrupt.send_modify(|n| *n += 1);
    }

    fn should_stop(&self, key: &str) -> bool {
        *self.closed.borrow() || self.stopping.lock().contains(key) || self.network_held()
    }

    fn claim(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.claims
            .lock()
            .entry(key.to_owned())
            .or_default()
            .clone()
    }

    /// Change download `key`'s manifest with `change`, write it and tell
    /// it; `change`'s error writes nothing.
    async fn update(
        &self,
        key: &str,
        change: impl FnOnce(&mut Manifest) -> Result<(), SdkError>,
    ) -> Result<Manifest, SdkError> {
        let store = self.store()?;
        let m = {
            let _writing = self.manifests.lock().await;
            let mut m = store
                .find(key)
                .ok_or_else(|| SdkError::NotFound(format!("download {key}")))?;
            change(&mut m)?;
            store.write(&m).map_err(disk)?;
            m
        };
        self.emit(&m);
        Ok(m)
    }

    fn emit(&self, m: &Manifest) {
        let _ = self.events.send(DownloadEvent::Changed(self.view(m)));
    }

    /// `m` as an app sees it.
    fn view(&self, m: &Manifest) -> LocalDownload {
        let store = self.store.as_ref().expect("a download is on disk");
        let done = m.phase == DownloadPhase::Done;
        let file_path = done.then(|| store.media(m)).flatten();
        let file_url = file_path
            .as_ref()
            .and_then(|p| url::Url::from_file_path(p).ok())
            .map(String::from);
        let received = self.received_of(m);
        let art = store.art_dir(m);
        LocalDownload {
            key: m.key.clone(),
            server: m.server.clone(),
            user_id: m.user_id.clone(),
            username: m.username.clone(),
            item: m.item.clone(),
            show: m.show.clone(),
            season: m.season.clone(),
            markers: m.markers.clone(),
            artwork: m
                .artwork
                .iter()
                .map(|a| LocalArtwork {
                    item: a.item.clone(),
                    kind: a.kind,
                    path: art.join(&a.file),
                })
                .collect(),
            tracks: m.tracks.clone(),
            source: m.source.clone(),
            chosen: m.chosen.clone(),
            file: m.file.clone(),
            phase: m.phase.clone(),
            received,
            size: m.size,
            file_path,
            file_url,
            watch: m.watch.clone(),
            note: if m.phase.is_active() && self.network_held() {
                Some(WAITING_FOR_NETWORK.to_owned())
            } else {
                m.note.clone()
            },
            created_at: m.created_at,
        }
    }

    /// How many bytes of `m`'s file are here: those being transferred are
    /// ahead of its manifest.
    fn received_of(&self, m: &Manifest) -> u64 {
        if m.phase == DownloadPhase::Done {
            m.size.unwrap_or(m.offset)
        } else {
            let live = self.received.lock().get(&m.key).copied();
            live.unwrap_or(0).max(m.offset)
        }
    }

    /// Free bytes in the downloads directory.
    fn available(&self, store: &Store) -> Result<u64, SdkError> {
        if let Some(available) = *self.free_space.lock() {
            return Ok(available);
        }
        std::fs::create_dir_all(store.root()).map_err(disk)?;
        space::available(store.root()).map_err(disk)
    }
}

/// The note of a download under way while the device is on a mobile
/// network that downloads may not use.
const WAITING_FOR_NETWORK: &str = "Waiting for Wi-Fi or a wired network";

/// A file system error in the downloads directory.
fn disk(e: std::io::Error) -> SdkError {
    SdkError::Downloads(e.to_string())
}
