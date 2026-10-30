//! Downloads for offline viewing and music offline: the SDK's download
//! types as they are.

use std::sync::Arc;
use std::time::Duration;

use spoolframe_client_sdk::types::{Item, MediaId, MediaTracks};
use spoolframe_client_sdk::{
    self as sdk, CacheSpace, DownloadAction, DownloadFailure, DownloadLibrary, DownloadOptionsInfo,
    DownloadSettings, DownloadSource, DownloadSpace, DownloadTracks, DownloadsHere, ErrorKind,
    FailureRemedy, LocalDownload, MusicDownload, MusicSynced, MusicTarget, NetworkKind,
    OfflineMusic, RefreshReport, RunEnd, RunProgress, Selection, SkippedChild, SyncedWatch,
};
use tokio::sync::Mutex;
use tokio::sync::broadcast::error::RecvError;

use crate::client::SpoolClient;
use crate::error::SpoolError;
use crate::runtime::run;
use crate::session::SpoolSession;
use crate::values::count;

/// A download changed.
// No box: UniFFI passes enum fields by value, and a notice is sent at most
// once per piece read.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, uniffi::Enum)]
pub enum DownloadNotice {
    /// Its phase, progress, watch or note: show it again.
    Changed { download: LocalDownload },
    /// It was cancelled or deleted.
    Removed { key: String },
    /// A music download's tracks moved on: show it again.
    MusicChanged { music: MusicDownload },
    /// What `user` did here with no server reached `server`: these
    /// items. Show them from the server again.
    WatchSynced {
        server: String,
        user: String,
        items: Vec<MediaId>,
    },
    /// `missed` changes were dropped while the app was behind: read the
    /// lists again.
    Lagged { missed: u64 },
}

#[uniffi::export]
impl SpoolClient {
    /// Every film and episode here, oldest first, from disk alone.
    pub fn downloads(&self) -> Vec<LocalDownload> {
        self.client.downloads().list()
    }

    /// Download `key`.
    pub fn download(&self, key: String) -> Result<LocalDownload, SpoolError> {
        Ok(self.client.downloads().get(&key)?)
    }

    /// The music downloads, oldest first.
    pub fn music_downloads(&self) -> Vec<MusicDownload> {
        self.client.downloads().music_downloads()
    }

    /// Every track here, downloaded or kept from listening, in any phase.
    pub fn music_tracks(&self) -> Vec<LocalDownload> {
        self.client.downloads().music_tracks()
    }

    /// `user_id`'s music on `server` playable here.
    pub fn offline_music(&self, server: String, user_id: String) -> OfflineMusic {
        self.client.downloads().offline_music(&server, &user_id)
    }

    pub fn download_space(&self) -> Result<DownloadSpace, SpoolError> {
        Ok(self.client.downloads().space()?)
    }

    pub fn music_cache(&self) -> CacheSpace {
        self.client.downloads().cache()
    }

    pub fn download_settings(&self) -> DownloadSettings {
        self.client.downloads().settings()
    }

    /// Download film or episode `id` as `session`'s user.
    pub async fn start_download(
        &self,
        session: Arc<SpoolSession>,
        id: MediaId,
        source: DownloadSource,
        tracks: DownloadTracks,
    ) -> Result<LocalDownload, SpoolError> {
        let downloads = self.client.downloads();
        let session = session.session.clone();
        Ok(run(async move { downloads.start(&session, &id, source, tracks).await }).await?)
    }

    /// Download `target` (a track, album, artist, playlist or Liked) as
    /// `session`'s user.
    pub async fn start_music(
        &self,
        session: Arc<SpoolSession>,
        target: MusicTarget,
    ) -> Result<MusicDownload, SpoolError> {
        let downloads = self.client.downloads();
        let session = session.session.clone();
        Ok(run(async move { downloads.start_music(&session, target).await }).await?)
    }

    /// Send what `session`'s user watched here offline.
    pub async fn sync_watch(
        &self,
        session: Arc<SpoolSession>,
    ) -> Result<Vec<SyncedWatch>, SpoolError> {
        let downloads = self.client.downloads();
        let session = session.session.clone();
        Ok(run(async move { downloads.sync_watch(&session).await }).await?)
    }

    /// Send what `session`'s user played and liked here offline, and
    /// bring their followed music up to date.
    pub async fn sync_music(&self, session: Arc<SpoolSession>) -> Result<MusicSynced, SpoolError> {
        let downloads = self.client.downloads();
        let session = session.session.clone();
        Ok(run(async move { downloads.sync_music(&session).await }).await?)
    }

    pub async fn pause_download(&self, key: String) -> Result<LocalDownload, SpoolError> {
        let downloads = self.client.downloads();
        Ok(run(async move { downloads.pause(&key).await }).await?)
    }

    pub async fn resume_download(&self, key: String) -> Result<LocalDownload, SpoolError> {
        let downloads = self.client.downloads();
        Ok(run(async move { downloads.resume(&key).await }).await?)
    }

    pub async fn retry_download(&self, key: String) -> Result<LocalDownload, SpoolError> {
        let downloads = self.client.downloads();
        Ok(run(async move { downloads.retry(&key).await }).await?)
    }

    /// Stop a download not finished and take it off this device.
    pub async fn cancel_download(&self, key: String) -> Result<(), SpoolError> {
        let downloads = self.client.downloads();
        Ok(run(async move { downloads.cancel(&key).await }).await?)
    }

    /// Take a download off this device, in any phase.
    pub async fn delete_download(&self, key: String) -> Result<(), SpoolError> {
        let downloads = self.client.downloads();
        Ok(run(async move { downloads.delete(&key).await }).await?)
    }

    /// Try music download `key`'s failed tracks again.
    pub async fn retry_music(&self, key: String) -> Result<MusicDownload, SpoolError> {
        let downloads = self.client.downloads();
        Ok(run(async move { downloads.retry_music(&key).await }).await?)
    }

    /// Stop moving music download `key`'s tracks until `resume_music`.
    pub async fn pause_music(&self, key: String) -> Result<MusicDownload, SpoolError> {
        let downloads = self.client.downloads();
        Ok(run(async move { downloads.pause_music(&key).await }).await?)
    }

    pub async fn resume_music(&self, key: String) -> Result<MusicDownload, SpoolError> {
        let downloads = self.client.downloads();
        Ok(run(async move { downloads.resume_music(&key).await }).await?)
    }

    pub async fn remove_music_download(&self, key: String) -> Result<(), SpoolError> {
        let downloads = self.client.downloads();
        Ok(run(async move { downloads.remove_music(&key).await }).await?)
    }

    /// The user got to `position` in download `key`, here.
    pub async fn record_download_position(
        &self,
        key: String,
        position: Duration,
    ) -> Result<LocalDownload, SpoolError> {
        let downloads = self.client.downloads();
        Ok(run(async move { downloads.record_position(&key, position).await }).await?)
    }

    pub async fn set_download_watched(
        &self,
        key: String,
        watched: bool,
    ) -> Result<LocalDownload, SpoolError> {
        let downloads = self.client.downloads();
        Ok(run(async move { downloads.set_watched(&key, watched).await }).await?)
    }

    /// Track `key` played here from its file for `played`: sent the next
    /// time its server is reached.
    pub async fn record_offline_play(
        &self,
        key: String,
        played: Duration,
    ) -> Result<(), SpoolError> {
        let downloads = self.client.downloads();
        Ok(run(async move { downloads.record_play(&key, played).await }).await?)
    }

    pub async fn record_offline_like(&self, key: String, liked: bool) -> Result<(), SpoolError> {
        let downloads = self.client.downloads();
        Ok(run(async move { downloads.record_like(&key, liked).await }).await?)
    }

    pub async fn set_download_settings(
        &self,
        settings: DownloadSettings,
    ) -> Result<(), SpoolError> {
        let downloads = self.client.downloads();
        Ok(run(async move { downloads.set_settings(settings).await }).await?)
    }

    /// Empty the cache of tracks kept from listening.
    pub async fn clear_music_cache(&self) -> Result<(), SpoolError> {
        let downloads = self.client.downloads();
        Ok(run(async move { downloads.clear_cache().await }).await?)
    }

    /// Read the downloads directory again: call it when the app returns
    /// to the foreground and when background work that downloads
    /// completes (an iOS background `URLSession` or share extension,
    /// Android's `WorkManager`), so what another process wrote shows.
    pub fn rescan_downloads(&self) {
        self.client.downloads().rescan();
    }

    /// The free bytes to go by (iOS's
    /// `volumeAvailableCapacityForImportantUsage`), or `None` for the file
    /// system's.
    pub fn set_download_free_space(&self, available: Option<u64>) {
        self.client.downloads().set_free_space(available);
    }

    pub fn set_network(&self, kind: NetworkKind) {
        self.client.downloads().set_network(kind);
    }

    /// The app shows `server`'s `user_id`: the downloads, their watch
    /// state and events are theirs from now on. Another profile than
    /// before starts locked if its PIN is asked for on this device.
    pub fn set_download_profile(&self, server: String, user_id: String) {
        self.client.downloads().set_profile(&server, &user_id);
    }

    /// No profile shown: every profile's downloads.
    pub fn clear_download_profile(&self) {
        self.client.downloads().clear_profile();
    }

    /// The active profile is locked on this device: ask for its PIN, or
    /// the owner's unlock, before showing its downloads.
    pub fn download_profile_locked(&self) -> bool {
        self.client.downloads().profile_locked()
    }

    /// Unlock the active profile's downloads with its PIN, with no
    /// server.
    pub async fn unlock_downloads_offline(&self, pin: String) -> Result<(), SpoolError> {
        let downloads = self.client.downloads();
        Ok(run(async move { downloads.unlock_offline(&pin).await }).await?)
    }

    /// Unlock the active profile's downloads: the device's owner passed
    /// the system's own check (Face ID, the passcode).
    pub fn unlock_downloads_by_owner(&self) -> Result<(), SpoolError> {
        Ok(self.client.downloads().unlock_offline_by_owner()?)
    }

    pub fn watch_downloads(&self) -> Arc<DownloadWatch> {
        Arc::new(DownloadWatch(Arc::new(Mutex::new(
            self.client.download_events(),
        ))))
    }

    /// How many transfers are under way (preparing or transferring),
    /// every profile's and music's, not only the shown profile's
    /// (`downloads()` is): what `close` would stop. Each music track is
    /// one, so an album counts as its tracks; any count above 0 is reason
    /// to keep running. Ask it before quitting, and to keep a
    /// background task or foreground service going.
    pub fn running_downloads(&self) -> u32 {
        count(self.client.downloads().running())
    }

    /// `running_downloads()` now and on each change, for any profile.
    /// It does not keep the client: `next` ends once the client is gone.
    pub fn watch_running_downloads(&self) -> Arc<RunningDownloadsWatch> {
        let current = self.client.downloads().watch_running();
        Arc::new(RunningDownloadsWatch {
            changes: Arc::new(Mutex::new(current.clone())),
            current,
        })
    }

    /// Move every download on until none is under way: for a background
    /// task's time.
    pub fn run_downloads(&self) -> Arc<DownloadRun> {
        Arc::new(DownloadRun {
            client: self.client.clone(),
            cancel: sdk::RunCancel::new(),
            tally: std::sync::Mutex::default(),
        })
    }

    /// The active profile's downloads as an offline library: shows,
    /// films, and those in progress.
    pub fn download_library(&self) -> DownloadLibrary {
        self.client.downloads().library()
    }

    /// Continue watching, from the downloads alone: one per show.
    pub fn downloads_to_continue(&self) -> Vec<LocalDownload> {
        self.client.downloads().continue_watching()
    }

    /// The episode of `show` to play next from the downloads.
    pub fn show_resume_download(&self, show: MediaId) -> Option<LocalDownload> {
        self.client.downloads().show_resume(&show)
    }

    /// The download of `id` on `server` to play in place of the
    /// server's: the finished copy wins.
    pub fn download_of_item(&self, server: String, id: MediaId) -> Option<LocalDownload> {
        self.client.downloads().of_item(&server, &id)
    }

    /// What each profile's downloads take on this device.
    pub fn downloads_here(&self) -> DownloadsHere {
        self.client.downloads().profiles_here()
    }

    /// What downloading show or season `id` would start, asking the
    /// server; starts nothing.
    pub async fn plan_download_children(
        &self,
        session: Arc<SpoolSession>,
        id: MediaId,
        only_unwatched: bool,
    ) -> Result<ChildPlan, SpoolError> {
        let downloads = self.client.downloads();
        let session = session.session.clone();
        let plan = run(async move { downloads.plan_children(&session, &id, only_unwatched).await })
            .await?;
        Ok(ChildPlan {
            to_start: plan.to_start,
            skipped: plan.skipped,
            failed: plan.failed.into_iter().map(ChildFailure::from).collect(),
        })
    }

    /// Download every episode of show or season `id`: one failing to
    /// start never stops the rest.
    pub async fn start_download_children(
        &self,
        session: Arc<SpoolSession>,
        id: MediaId,
        source: DownloadSource,
        only_unwatched: bool,
    ) -> Result<BatchStart, SpoolError> {
        let downloads = self.client.downloads();
        let session = session.session.clone();
        let batch = run(async move {
            downloads
                .start_children(&session, &id, source, only_unwatched)
                .await
        })
        .await?;
        Ok(BatchStart {
            started: batch.started,
            skipped: batch.skipped,
            failed: batch.failed.into_iter().map(ChildFailure::from).collect(),
        })
    }

    /// A background refresh has something to send or music to follow.
    pub fn downloads_need_refresh(&self) -> bool {
        self.client.downloads().needs_refresh()
    }

    /// The background refresh: every profile with something to send, or
    /// music followed, brought up to date.
    pub async fn refresh_downloads(&self) -> Result<RefreshReport, SpoolError> {
        let downloads = self.client.downloads();
        Ok(run(async move { downloads.refresh_all().await }).await?)
    }

    /// How soon the next background refresh is worth asking for; `None`
    /// for none.
    pub fn download_refresh_hint(&self) -> Option<Duration> {
        self.client.downloads().refresh_hint()
    }

    /// A background run is worth asking the system for now.
    pub fn downloads_worth_background(&self) -> bool {
        self.client.downloads().worth_background()
    }
}

/// An episode, or a season whose episodes could not be listed, that a
/// batch could not start.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ChildFailure {
    pub item: MediaId,
    pub kind: ErrorKind,
    pub message: String,
}

impl From<sdk::FailedChild> for ChildFailure {
    fn from(failed: sdk::FailedChild) -> Self {
        let error = SpoolError::from(failed.error);
        Self {
            item: failed.item,
            kind: error.kind(),
            message: error.to_string(),
        }
    }
}

/// What downloading a season or a show would start.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ChildPlan {
    pub to_start: Vec<Item>,
    pub skipped: Vec<SkippedChild>,
    pub failed: Vec<ChildFailure>,
}

/// What `start_download_children` did.
#[derive(Debug, Clone, uniffi::Record)]
pub struct BatchStart {
    pub started: Vec<LocalDownload>,
    pub skipped: Vec<SkippedChild>,
    pub failed: Vec<ChildFailure>,
}

/// What to offer for `download` now, in order.
#[uniffi::export]
pub fn download_actions(download: LocalDownload) -> Vec<DownloadAction> {
    download.actions()
}

/// How far along `download` is, 0 to 1; `None` with nothing to show.
#[uniffi::export]
pub fn download_progress(download: LocalDownload) -> Option<f64> {
    download.progress()
}

/// A failed `download` worth trying again.
#[uniffi::export]
pub fn download_can_retry(download: LocalDownload) -> bool {
    download.can_retry()
}

/// Where `download` plays from when its item page asked for `asked`.
#[uniffi::export]
pub fn download_resume_from(download: LocalDownload, asked: Option<Duration>) -> Option<Duration> {
    download.resume_from(asked)
}

/// The tracks in `download`'s file, in the file's order.
#[uniffi::export]
pub fn download_file_tracks(download: LocalDownload) -> MediaTracks {
    download.file_tracks()
}

/// The file's tracks to play for `asked`, of the item's tracks; with
/// none, the file's first audio and no subtitles.
#[uniffi::export]
pub fn download_pick_tracks(download: LocalDownload, asked: Option<Selection>) -> Selection {
    download.pick_tracks(asked)
}

/// What to suggest for a download that failed of `failure`.
#[uniffi::export]
pub fn download_failure_remedy(failure: DownloadFailure) -> Option<FailureRemedy> {
    failure.suggested()
}

#[uniffi::export]
impl SpoolSession {
    /// What this device may download of `id`.
    pub async fn download_options(&self, id: MediaId) -> Result<DownloadOptionsInfo, SpoolError> {
        self.call(|s| async move { s.download_options(&id).await })
            .await
    }
}

/// Download changes, as they happen.
#[derive(uniffi::Object)]
pub struct DownloadWatch(Arc<Mutex<tokio::sync::broadcast::Receiver<sdk::DownloadEvent>>>);

#[uniffi::export]
impl DownloadWatch {
    /// The next change; `None` once the client is gone.
    pub async fn next(&self) -> Option<DownloadNotice> {
        let watch = self.0.clone();
        run(async move {
            Some(match watch.lock().await.recv().await {
                Ok(sdk::DownloadEvent::Changed(download)) => DownloadNotice::Changed { download },
                Ok(sdk::DownloadEvent::Removed { key }) => DownloadNotice::Removed { key },
                Ok(sdk::DownloadEvent::MusicChanged(music)) => {
                    DownloadNotice::MusicChanged { music }
                }
                Ok(sdk::DownloadEvent::WatchSynced {
                    server,
                    user,
                    items,
                }) => DownloadNotice::WatchSynced {
                    server,
                    user,
                    items,
                },
                Err(RecvError::Lagged(missed)) => DownloadNotice::Lagged { missed },
                Err(RecvError::Closed) => return None,
            })
        })
        .await
    }
}

/// `running_downloads()` as it changes.
#[derive(uniffi::Object)]
pub struct RunningDownloadsWatch {
    /// Read by `current`, never waited on: a `next` waiting doesn't hold
    /// it up.
    current: tokio::sync::watch::Receiver<usize>,
    changes: Arc<Mutex<tokio::sync::watch::Receiver<usize>>>,
}

#[uniffi::export]
impl RunningDownloadsWatch {
    /// The count now.
    pub fn current(&self) -> u32 {
        count(*self.current.borrow())
    }

    /// The count after its next change; `None` once the client is gone.
    pub async fn next(&self) -> Option<u32> {
        let watch = self.changes.clone();
        run(async move {
            let mut watch = watch.lock().await;
            watch.changed().await.ok()?;
            Some(count(*watch.borrow_and_update()))
        })
        .await
    }
}

/// Downloads moving until done, for a background task.
#[derive(uniffi::Object)]
pub struct DownloadRun {
    client: sdk::Client,
    cancel: sdk::RunCancel,
    tally: std::sync::Mutex<sdk::RunTally>,
}

#[uniffi::export]
impl DownloadRun {
    /// Returns once nothing is preparing or transferring (`Done`), or
    /// every download waits for a network downloads may use (`Held`).
    /// `Cancelled` is an error, once `cancel` stops it, before or while
    /// it runs; nothing is lost.
    pub async fn wait(&self) -> Result<RunEnd, SpoolError> {
        let downloads = self.client.downloads();
        let cancel = self.cancel.clone();
        match run(async move { downloads.run_until_cancelled(&cancel).await }).await? {
            RunEnd::Cancelled => Err(SpoolError::Cancelled),
            end => Ok(end),
        }
    }

    /// How far the run got, for the system's progress: never back.
    pub fn progress(&self) -> RunProgress {
        let latest = self.client.downloads().list();
        self.tally
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .progress(&latest)
    }

    /// Stop `wait`, from any thread: the background task's time is up.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}
