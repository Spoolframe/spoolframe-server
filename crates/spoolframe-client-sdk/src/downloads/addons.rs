//! Download add-ons (feature `spoolframe:download-addons`): subtitles
//! and audio tracks added to a film's or an episode's download kept here,
//! without fetching the film again.
//!
//! [`Downloads::add_tracks`] fetches a subtitle in the call, as WebVTT
//! text (`ReadTrackSubtitle`): it is small. An audio track is queued: the
//! download manager moves it as it moves a film, a download of its own on
//! the server (`PrepareAudioAddon`) followed while it is made, then read
//! into `addons/audio-<place>.part` from where it stopped, the bytes kept
//! saved in the manifest after each read, so a transfer cut off (a hotel's
//! network, the app quit) goes on from there, after a restart too. Whole,
//! its SHA-256 is checked, it is kept, and the server is told it may let
//! it go. Pausing, resuming and deleting the download do the same to its
//! add-ons; adding a failed one again goes on with what arrived.
//!
//! Each is kept in the download's own directory, under `addons/`, so it
//! goes with the download. The manifest lists them with their state, and
//! each move is told as the download's
//! [`DownloadEvent::Changed`](super::DownloadEvent::Changed).

use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};
use spoolframe_protocol::downloads::{DownloadFailure, DownloadInfo, DownloadState};
use spoolframe_protocol::{Reply, Request};
use spoolframe_types::{Container, MediaId};

use super::manager::{self, Step};
use super::store::{EntryKind, Manifest, Store};
use super::writer::{BlockWriter, Unfinished};
use super::{DownloadPhase, Downloads, LocalDownload, Shared, disk};
use crate::gate::{DownloadAddons, On};
use crate::{SdkError, Session};

/// The directory of a download's add-ons, in its own.
pub(super) const ADDONS: &str = "addons";

/// What an add-on is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum AddonKind {
    Audio,
    Subtitle,
}

/// What an add-on's file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum AddonFormat {
    /// A subtitle, which the app draws itself.
    WebVtt,
    /// An audio track as an MP4 audio file, for an Apple device.
    M4a,
    /// An audio track as a Matroska audio file, for Linux.
    Mka,
    /// One this build does not know.
    #[serde(other)]
    Unknown,
}

impl AddonFormat {
    fn extension(self) -> &'static str {
        match self {
            Self::WebVtt => "vtt",
            Self::M4a => "m4a",
            Self::Mka => "mka",
            Self::Unknown => "bin",
        }
    }

    /// An audio add-on's, as the server's file is.
    fn of_audio(container: Option<Container>) -> Self {
        match container {
            Some(Container::Matroska) => Self::Mka,
            _ => Self::M4a,
        }
    }
}

/// Where an add-on is, as [`DownloadPhase`] is for its download.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AddonState {
    /// The server is making an audio track's file: `server` is where it
    /// says it is (waiting, converting, writing).
    Preparing { server: DownloadState },
    /// Coming here: `done_percent` of it, once known
    /// ([`DownloadAddon::received`] of [`DownloadAddon::size`]).
    #[serde(alias = "fetching")]
    Transferring { done_percent: Option<u8> },
    /// Stopped with its download ([`Downloads::pause`]) until
    /// [`Downloads::resume`]. What arrived is kept.
    Paused,
    /// Here: play [`DownloadAddon::path`].
    Ready,
    /// It could not be added, for `reason`, said to the user as it is,
    /// and why. [`Downloads::add_tracks`] of it again tries again, going on
    /// from what arrived.
    Failed {
        reason: String,
        #[serde(default)]
        cause: DownloadFailure,
    },
}

impl DownloadAddon {
    /// The add-on of `kind` at `place`.
    pub(super) fn is(&self, kind: AddonKind, place: u32) -> bool {
        self.kind == kind && self.place == place
    }

    /// An audio add-on under way: the manager's to move
    /// ([`managed`]).
    pub(super) fn is_managed(&self) -> bool {
        self.kind == AddonKind::Audio && self.state.is_moving()
    }
}

impl From<DownloadAddon> for StoredAddon {
    /// Not prepared on the server yet: no id, no SHA-256.
    fn from(addon: DownloadAddon) -> Self {
        Self {
            addon,
            id: None,
            sha256: None,
        }
    }
}

impl AddonState {
    /// The manager moves it on (or, for a subtitle, the call adding it).
    pub(super) fn is_moving(&self) -> bool {
        matches!(self, Self::Preparing { .. } | Self::Transferring { .. })
    }
}

/// A subtitle or an audio track added to a download.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct DownloadAddon {
    pub kind: AddonKind,
    /// Its place in the item's tracks: the index into
    /// [`LocalDownload::tracks`]'s `audio` (an audio add-on) or
    /// `subtitles` (a subtitle add-on), the same lists
    /// [`spoolframe_protocol::downloads::DownloadFile::tracks`] names the
    /// file's tracks by.
    pub place: u32,
    /// Its file once [`AddonState::Ready`]: in [`LocalDownload::addons`]
    /// the whole path; in the manifest, from the download's directory.
    pub path: Option<String>,
    pub format: AddonFormat,
    /// The track's language, as the item's tracks say.
    pub language: Option<String>,
    pub state: AddonState,
    /// How many bytes of an audio add-on's file are here.
    #[serde(default)]
    pub received: u64,
    /// An audio add-on's size, once the server has its file ready.
    #[serde(default)]
    pub size: Option<u64>,
}

/// An add-on as the manifest keeps it: what the app sees, and what moves
/// an audio add-on on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredAddon {
    #[serde(flatten)]
    pub addon: DownloadAddon,
    /// The server's id for an audio add-on's download; `None` until
    /// prepared, and once the server no longer knows it.
    #[serde(default)]
    pub id: Option<String>,
    /// The SHA-256 of the server's file, once ready.
    #[serde(default)]
    pub sha256: Option<String>,
}

impl DownloadAddon {
    /// Of `kind` at `place` in `m`'s item, `state`.
    fn new(m: &Manifest, kind: AddonKind, place: u32, state: AddonState) -> Self {
        let (format, language) = match kind {
            AddonKind::Audio => (
                AddonFormat::of_audio(m.file.as_ref().map(|f| f.container)),
                m.tracks
                    .audio
                    .get(place as usize)
                    .and_then(|t| t.language.clone()),
            ),
            AddonKind::Subtitle => (
                AddonFormat::WebVtt,
                m.tracks
                    .subtitles
                    .get(place as usize)
                    .and_then(|t| t.language.clone()),
            ),
        };
        Self {
            kind,
            place,
            path: None,
            format,
            language,
            state,
            received: 0,
            size: None,
        }
    }

    /// Its file, from the download's directory.
    fn file(kind: AddonKind, place: u32, format: AddonFormat) -> String {
        let name = match kind {
            AddonKind::Audio => "audio",
            AddonKind::Subtitle => "sub",
        };
        format!("{ADDONS}/{name}-{place}.{}", format.extension())
    }

    /// As an app sees it, of the download at `dir`: its whole path, and a
    /// subtitle left fetching by a call that ended with the client
    /// (`fetching` false) failed. An audio add-on moving is the
    /// manager's, which goes on with it: `live` bytes of it may be here
    /// ahead of its manifest, and its done percent is of those.
    pub(super) fn view(&self, dir: &Path, fetching: bool, live: Option<u64>) -> Self {
        let received = live.unwrap_or(0).max(self.received);
        let state = match &self.state {
            AddonState::Transferring { .. } if self.kind == AddonKind::Subtitle && !fetching => {
                AddonState::Failed {
                    reason: "Stopped before it was added; add it again".to_owned(),
                    cause: DownloadFailure::Other,
                }
            }
            AddonState::Transferring { .. } => AddonState::Transferring {
                done_percent: self.size.map(|size| percent(received, size)),
            },
            state => state.clone(),
        };
        Self {
            path: self
                .path
                .as_ref()
                .map(|p| dir.join(p).display().to_string()),
            state,
            received,
            ..self.clone()
        }
    }
}

/// The subtitle add-ons being fetched now, by download key and place.
pub(crate) type Running = HashSet<(String, AddonKind, u32)>;

/// Lets go of the subtitles a call fetches, however it ends.
struct Fetching<'a> {
    shared: &'a Shared,
    taken: Vec<(String, AddonKind, u32)>,
}

impl Drop for Fetching<'_> {
    fn drop(&mut self) {
        let mut running = self.shared.addons_running.lock();
        for taken in &self.taken {
            running.remove(taken);
        }
    }
}

impl Downloads {
    /// Add `subtitles` and `audio` (places in the item's tracks, as
    /// [`DownloadAddon::place`] is) to film or episode download `key`,
    /// with `session`, which must be its user's on its server.
    ///
    /// Each audio track is queued, and this returns once it is: the
    /// manager prepares and fetches it as it does a film, going on from
    /// what arrived after a cut or a restart, and tells each move as the
    /// download's [`super::DownloadEvent::Changed`]. One here already is
    /// left as it is; one queued is left in its place; one failed or paused
    /// is queued again (the way to retry it). One paused, or failed on this
    /// device, goes on from what arrived; one the server failed is prepared
    /// on it anew, keeping what arrived only if the server's new file is
    /// the same.
    ///
    /// Each subtitle is fetched in the call, as WebVTT, again if it was
    /// here. A track the item does not have is
    /// [`SdkError::InvalidInput`], and nothing is added.
    pub async fn add_tracks(
        &self,
        key: &str,
        session: &On<DownloadAddons>,
        audio: Vec<u32>,
        subtitles: Vec<u32>,
    ) -> Result<LocalDownload, SdkError> {
        let shared = self.shared();
        let store = shared.store()?;
        let m = self.find(key)?;
        let user = &session.session().user().user_id;
        if m.kind == EntryKind::Track {
            return Err(SdkError::InvalidInput(format!(
                "download {key} is a track: only films and episodes have add-ons"
            )));
        }
        if m.server != session.session().server() || !m.held_by(user) {
            return Err(SdkError::InvalidInput(format!(
                "download {key} is not this session's user's"
            )));
        }
        let audio = places(audio, m.tracks.audio.len(), "audio track")?;
        let subtitles = places(subtitles, m.tracks.subtitles.len(), "subtitle")?;
        shared.session_made(session.session());
        // The audio first: the manager is under way while the subtitles
        // come.
        if !audio.is_empty() {
            shared
                .update(key, |m| {
                    for &place in &audio {
                        queue(m, place);
                    }
                    Ok(())
                })
                .await?;
            shared.rouse(key);
        }
        let mut fetching = Fetching {
            shared,
            taken: Vec::new(),
        };
        {
            let mut running = shared.addons_running.lock();
            for &place in &subtitles {
                let taken = (key.to_owned(), AddonKind::Subtitle, place);
                if running.insert(taken.clone()) {
                    fetching.taken.push(taken);
                }
            }
        }
        let mine: Vec<u32> = fetching.taken.iter().map(|(_, _, p)| *p).collect();
        if !mine.is_empty() {
            shared
                .update(key, |m| {
                    for &place in &mine {
                        let moving = AddonState::Transferring { done_percent: None };
                        set(m, DownloadAddon::new(m, AddonKind::Subtitle, place, moving));
                    }
                    Ok(())
                })
                .await?;
        }
        let id = m.item.id().clone();
        for &place in &mine {
            let fetched = subtitle(store, session, &m, &id, place).await;
            settle_subtitle(shared, store, &m, place, fetched).await?;
        }
        drop(fetching);
        Ok(shared.view(&store.find(key).unwrap_or(m)))
    }
}

/// `places` (of tracks there are `len` of) in order, each once; one past
/// the end is [`SdkError::InvalidInput`].
fn places(mut places: Vec<u32>, len: usize, what: &str) -> Result<Vec<u32>, SdkError> {
    if let Some(bad) = places.iter().find(|&&p| p as usize >= len) {
        return Err(SdkError::InvalidInput(format!(
            "{what} {bad}: no such track"
        )));
    }
    places.sort_unstable();
    places.dedup();
    Ok(places)
}

/// Audio add-on `place` of `m` queued for the manager: a new one, or one
/// failed or paused, which keeps what arrived. One here or under way
/// already is left as it is.
///
/// One paused, or failed on this device ([`manager::device_failed`]), keeps its
/// server id and goes on from what arrived. One the server failed is
/// prepared anew: asking about its id again would hear the same failure.
/// What arrived of it is kept only if the new file is the same one (its
/// size and SHA-256, checked once the server has it ready).
fn queue(m: &mut Manifest, place: u32) {
    let waiting = AddonState::Preparing {
        server: DownloadState::Waiting,
    };
    match find(m, AddonKind::Audio, place) {
        Some(there) if there.addon.state == AddonState::Ready || there.addon.state.is_moving() => {}
        Some(there) => {
            if let AddonState::Failed { cause, .. } = &there.addon.state
                && !manager::device_failed(cause)
            {
                there.id = None;
            }
            there.addon.state = waiting;
        }
        None => {
            let addon = DownloadAddon::new(m, AddonKind::Audio, place, waiting);
            m.addons.push(addon.into());
        }
    }
}

/// `addon` in `m`, in place of one of the same kind and place (which
/// keeps its place in the list: the order added), else last.
fn set(m: &mut Manifest, addon: DownloadAddon) {
    match find(m, addon.kind, addon.place) {
        Some(there) => there.addon = addon,
        None => m.addons.push(addon.into()),
    }
}

fn find(m: &mut Manifest, kind: AddonKind, place: u32) -> Option<&mut StoredAddon> {
    m.addons.iter_mut().find(|a| a.addon.is(kind, place))
}

fn find_ref(m: &Manifest, kind: AddonKind, place: u32) -> Option<&StoredAddon> {
    m.addons.iter().find(|a| a.addon.is(kind, place))
}

/// Subtitle `place` of `m` is `fetched` (its file) or failed: kept, and
/// told. A download deleted meanwhile is gone, with what was written for
/// it.
async fn settle_subtitle(
    shared: &Shared,
    store: &Store,
    m: &Manifest,
    place: u32,
    fetched: Result<String, SdkError>,
) -> Result<(), SdkError> {
    let updated = shared
        .update(&m.key, |m| {
            let state = match &fetched {
                Ok(_) => AddonState::Ready,
                Err(e) => {
                    tracing::info!(key = m.key, place, error = %e, "a subtitle was not added");
                    AddonState::Failed {
                        reason: e.to_string(),
                        cause: manager::cause_of(e),
                    }
                }
            };
            let mut addon = DownloadAddon::new(m, AddonKind::Subtitle, place, state);
            addon.path = fetched.as_ref().ok().cloned();
            set(m, addon);
            Ok(())
        })
        .await;
    if let Err(SdkError::NotFound(_)) = &updated
        && store.find(&m.key).is_none()
    {
        // Deleted while it was fetched: what was written goes too.
        let _ = std::fs::remove_dir_all(store.dir(m));
    }
    updated.map(|_| ())
}

/// Subtitle `place` of `m`'s item `id`, kept as WebVTT: its file, from
/// the download's directory.
async fn subtitle(
    store: &Store,
    session: &On<DownloadAddons>,
    m: &Manifest,
    id: &MediaId,
    place: u32,
) -> Result<String, SdkError> {
    let text = session.read_track_subtitle(id, place).await?;
    let path = DownloadAddon::file(AddonKind::Subtitle, place, AddonFormat::WebVtt);
    crate::storage::files::write(&store.dir(m).join(&path), text.as_bytes()).map_err(disk)?;
    Ok(path)
}

/// `PrepareAudioAddon`.
async fn prepare(session: &Session, id: &MediaId, place: u32) -> Result<DownloadInfo, SdkError> {
    let request = Request::PrepareAudioAddon {
        id: id.clone(),
        audio: place,
    };
    match session.call(request).await? {
        Reply::Download(info) => Ok(info),
        other => Err(crate::session::unexpected(other)),
    }
}

/// `m`'s first add-on the manager moves: an audio one under way. A
/// subtitle is fetched by the call adding it, never by the manager: one
/// left transferring when that call ended (the client closed) has no
/// download id, and is shown failed ([`DownloadAddon::view`]).
pub(super) fn managed(m: &Manifest) -> Option<&StoredAddon> {
    m.addons.iter().find(|a| a.addon.is_managed())
}

/// One step of `m`'s first audio add-on under way, for the manager: its
/// download is a film's done (or not under way), so its add-ons move. An
/// error that may pass by itself is the manager's to wait on; another
/// fails the add-on alone.
pub(super) async fn step(
    shared: &Shared,
    session: &Session,
    m: Manifest,
) -> Result<Step, SdkError> {
    let Some(addon) = managed(&m).cloned() else {
        return Ok(Step::Stop);
    };
    let place = addon.addon.place;
    let moved = match addon.addon.state {
        AddonState::Preparing { .. } => preparing(shared, session, &m, &addon).await,
        _ => transferring(shared, session, &m, &addon).await,
    };
    match moved {
        Err(e) if !manager::waits(&e) => {
            tracing::info!(key = m.key, place, error = %e, "an audio add-on failed");
            let cause = manager::cause_of(&e);
            fail(shared, &m.key, place, e.to_string(), cause).await?;
            Ok(Step::Continue)
        }
        moved => moved,
    }
}

/// Every audio add-on of `m` under way is `state` now: paused with its
/// download, or failed with it for an error no wait mends (its server
/// forgotten), so none is left for the manager to take again.
pub(super) fn set_moving(m: &mut Manifest, state: &AddonState) {
    for a in m.addons.iter_mut().filter(|a| a.addon.is_managed()) {
        a.addon.state = state.clone();
    }
}

/// Audio add-on `place` of download `key` failed, for `reason`.
async fn fail(
    shared: &Shared,
    key: &str,
    place: u32,
    reason: String,
    cause: DownloadFailure,
) -> Result<(), SdkError> {
    shared
        .update(key, |m| {
            if let Some(a) = find(m, AddonKind::Audio, place) {
                a.addon.state = AddonState::Failed { reason, cause };
            }
            Ok(())
        })
        .await
        .map(|_| ())
}

/// Audio add-on `place` of download `key` is to be prepared again,
/// keeping what arrived; `forget` its server id, which the server no
/// longer knows.
async fn to_prepare(
    shared: &Shared,
    key: &str,
    place: u32,
    forget: bool,
) -> Result<Step, SdkError> {
    shared
        .update(key, |m| {
            if let Some(a) = find(m, AddonKind::Audio, place)
                && matches!(a.addon.state, AddonState::Transferring { .. })
            {
                if forget {
                    a.id = None;
                }
                a.addon.state = AddonState::Preparing {
                    server: DownloadState::Waiting,
                };
            }
            Ok(())
        })
        .await?;
    Ok(Step::Continue)
}

/// The done percent of `received` of `size`.
fn percent(received: u64, size: u64) -> u8 {
    u8::try_from(received.saturating_mul(100) / size.max(1)).unwrap_or(100)
}

/// Prepare `addon` on the server, or ask how it goes, as the manager does
/// a film's ([`manager`]'s `preparing`).
async fn preparing(
    shared: &Shared,
    session: &Session,
    m: &Manifest,
    addon: &StoredAddon,
) -> Result<Step, SdkError> {
    let place = addon.addon.place;
    let mut events = shared.server_events.subscribe();
    let info = match &addon.id {
        Some(id) => match manager::status(session, id).await {
            Err(SdkError::NotFound(_)) => prepare(session, m.item.id(), place).await?,
            other => other?,
        },
        None => prepare(session, m.item.id(), place).await?,
    };
    let id = info.id.clone();
    let m = shared
        .update(&m.key, |m| {
            let Some(a) = find(m, AddonKind::Audio, place) else {
                return Ok(());
            };
            // Paused or failed meanwhile: as it is.
            if !matches!(a.addon.state, AddonState::Preparing { .. }) {
                return Ok(());
            }
            a.id = Some(info.id);
            a.addon.state = match info.state {
                DownloadState::Ready { size, sha256 } => {
                    // Another file than the one coming: from the start.
                    if manager::other_file((a.addon.size, a.sha256.as_deref()), size, &sha256) {
                        a.addon.received = 0;
                    }
                    a.addon.size = Some(size);
                    a.sha256 = Some(sha256);
                    a.addon.format = AddonFormat::of_audio(info.file.map(|f| f.container));
                    // Its done percent is of what is here, as it is shown
                    // ([`DownloadAddon::view`]).
                    AddonState::Transferring { done_percent: None }
                }
                DownloadState::Failed { reason, cause } => AddonState::Failed { reason, cause },
                server => AddonState::Preparing { server },
            };
            m.note = None;
            Ok(())
        })
        .await?;
    match find_ref(&m, AddonKind::Audio, place).map(|a| &a.addon.state) {
        Some(AddonState::Preparing { .. }) => {}
        _ => return Ok(Step::Continue),
    }
    Ok(manager::poll_or_wait(
        shared,
        &mut events,
        session.server,
        &id,
        &m.key,
    ))
}

/// Read `addon`'s file from where it stopped, check it whole and keep it,
/// as the manager does a film's ([`manager`]'s `transferring`).
async fn transferring(
    shared: &Shared,
    session: &Session,
    m: &Manifest,
    addon: &StoredAddon,
) -> Result<Step, SdkError> {
    let store = shared.store()?;
    let place = addon.addon.place;
    let (Some(id), Some(size), Some(sha256)) =
        (addon.id.clone(), addon.addon.size, addon.sha256.clone())
    else {
        return to_prepare(shared, &m.key, place, false).await;
    };
    let path = DownloadAddon::file(AddonKind::Audio, place, addon.addon.format);
    let to = store.dir(m).join(&path);
    // Its bytes until it is whole: `addons/audio-<place>.part`.
    let part = to.with_extension("part");
    std::fs::create_dir_all(to.parent().expect("under the download's directory")).map_err(disk)?;
    let mut writer = BlockWriter::open(&part, size, addon.addon.received)
        .await
        .map_err(disk)?;
    let needed = (size - writer.contiguous()).saturating_add(manager::HEADROOM);
    if let Some(DownloadPhase::Failed { reason, cause }) =
        manager::no_room(needed, shared.available(store)?)
    {
        fail(shared, &m.key, place, reason, cause).await?;
        return Ok(Step::Continue);
    }
    // What is on disk, synced, is what the manifest keeps: a cut (or a
    // crash) goes on from there. As a film's, it is synced and kept every
    // [`manager::SYNC_EVERY`]; between, what came is shown from memory.
    let save = |received: u64| {
        let sha256 = sha256.clone();
        shared.update(&m.key, move |m| {
            if let Some(a) = find(m, AddonKind::Audio, place)
                && a.sha256.as_deref() == Some(sha256.as_str())
            {
                a.addon.received = received;
            }
            Ok(())
        })
    };
    let live = (m.key.clone(), Some(place));
    while !writer.is_complete() {
        let offset = writer.contiguous();
        if shared.should_stop(&m.key) {
            writer.sync().await.map_err(disk)?;
            save(offset).await?;
            return Ok(Step::Stop);
        }
        let (told_size, bytes) = match manager::read(session, &id, offset).await {
            Ok(read) => read,
            Err(e) => {
                writer.sync().await.map_err(disk)?;
                save(offset).await?;
                return match e {
                    // Gone from the server, or no longer ready there (its
                    // file removed a day after its last read, or its media
                    // changed): prepared anew, as asking about it would
                    // only hear it failed; the same file goes on from here.
                    SdkError::NotFound(_) | SdkError::DownloadNotReady => {
                        to_prepare(shared, &m.key, place, true).await
                    }
                    e => Err(e),
                };
            }
        };
        if told_size != size {
            writer.sync().await.map_err(disk)?;
            save(offset).await?;
            return to_prepare(shared, &m.key, place, false).await;
        }
        if bytes.is_empty() {
            return Err(SdkError::Protocol(format!(
                "an empty read at {offset} of {size} bytes"
            )));
        }
        writer.write_at(offset, &bytes).await.map_err(disk)?;
        shared
            .received
            .lock()
            .insert(live.clone(), writer.contiguous());
        if writer.unsynced() >= manager::SYNC_EVERY {
            writer.sync().await.map_err(disk)?;
            save(writer.contiguous()).await?;
        } else {
            shared.emit(m);
        }
    }
    match writer.finish(&to, Some(&sha256)).await {
        Ok(()) => {}
        Err(Unfinished::Damaged) => {
            save(0).await?;
            fail(
                shared,
                &m.key,
                place,
                "The file arrived damaged; try again".to_owned(),
                DownloadFailure::Network,
            )
            .await?;
            return Ok(Step::Continue);
        }
        Err(Unfinished::Io(e)) => return Err(disk(e)),
        Err(Unfinished::Incomplete) => unreachable!("the loop ends once it is whole"),
    }
    // Here whole: the server may let it go.
    if let Err(e) = manager::cancel(session, &id).await {
        tracing::info!(id, error = %e, "the server was not told an add-on finished");
    }
    shared
        .update(&m.key, |m| {
            if let Some(a) = find(m, AddonKind::Audio, place) {
                a.addon.state = AddonState::Ready;
                a.addon.path = Some(path);
                a.addon.received = size;
            }
            Ok(())
        })
        .await?;
    Ok(Step::Continue)
}

/// The server ids of `m`'s audio add-ons not here yet: cancelled with
/// the download.
pub(super) fn unfinished_ids(m: &Manifest) -> Vec<String> {
    m.addons
        .iter()
        .filter(|a| a.addon.state != AddonState::Ready)
        .filter_map(|a| a.id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio(state: AddonState, path: Option<&str>) -> DownloadAddon {
        DownloadAddon {
            kind: AddonKind::Audio,
            place: 1,
            path: path.map(str::to_owned),
            format: AddonFormat::M4a,
            language: Some("fre".into()),
            state,
            received: 0,
            size: None,
        }
    }

    /// The manifest keeps the path from the download's directory; the
    /// app is given the whole path.
    #[test]
    fn an_app_is_given_an_add_on_s_whole_path() {
        let kept = audio(AddonState::Ready, Some("addons/audio-1.m4a"));
        assert_eq!(
            kept.view(Path::new("/downloads/abc"), false, None)
                .path
                .as_deref(),
            Some("/downloads/abc/addons/audio-1.m4a")
        );
    }

    /// A subtitle left fetching by a call that ended with the client is
    /// shown failed; an audio add-on under way is the manager's, and stays,
    /// with its done percent of the bytes here, those ahead of its
    /// manifest too.
    #[test]
    fn a_subtitle_left_fetching_is_failed_and_audio_goes_on() {
        let moving = AddonState::Transferring { done_percent: None };
        let subtitle = DownloadAddon {
            kind: AddonKind::Subtitle,
            ..audio(moving.clone(), None)
        };
        let coming = DownloadAddon {
            received: 10,
            size: Some(100),
            ..audio(moving.clone(), None)
        };
        let dir = Path::new("/d");
        let shown = coming.view(dir, false, Some(40));
        assert_eq!(
            (
                matches!(
                    subtitle.view(dir, false, None).state,
                    AddonState::Failed { .. }
                ),
                subtitle.view(dir, true, None).state,
                shown.state,
                shown.received,
            ),
            (
                true,
                moving,
                AddonState::Transferring {
                    done_percent: Some(40)
                },
                40
            )
        );
    }

    /// A subtitle left transferring (its call ended with the client) is
    /// not the manager's: the download is not under way for it, and the
    /// manager steps the audio add-on after it, not the subtitle forever.
    #[test]
    fn a_subtitle_left_transferring_is_not_the_managers() {
        let moving = AddonState::Transferring {
            done_percent: Some(40),
        };
        let stored = |kind, place, state| StoredAddon {
            addon: DownloadAddon {
                kind,
                place,
                ..audio(state, None)
            },
            id: None,
            sha256: None,
        };
        let mut m = crate::downloads::store::tests::manifest("ab12", 1);
        m.phase = DownloadPhase::Done;
        m.addons = vec![stored(AddonKind::Subtitle, 2, moving.clone())];
        let subtitle_alone = (m.under_way(), managed(&m).map(|a| a.addon.place));
        m.addons.push(stored(
            AddonKind::Audio,
            1,
            AddonState::Preparing {
                server: DownloadState::Waiting,
            },
        ));
        assert_eq!(
            (
                subtitle_alone,
                (m.under_way(), managed(&m).map(|a| a.addon.place))
            ),
            ((false, None), (true, Some(1)))
        );
    }

    #[test]
    fn an_add_on_s_file_is_named_for_its_kind_and_place() {
        assert_eq!(
            [
                DownloadAddon::file(AddonKind::Subtitle, 3, AddonFormat::WebVtt),
                DownloadAddon::file(AddonKind::Audio, 1, AddonFormat::M4a),
            ],
            ["addons/sub-3.vtt", "addons/audio-1.m4a"]
        );
    }

    #[test]
    fn places_are_in_order_once_and_each_one_the_item_has() {
        assert_eq!(places(vec![2, 0, 2], 3, "audio track"), Ok(vec![0, 2]));
        assert!(matches!(
            places(vec![3], 3, "subtitle"),
            Err(SdkError::InvalidInput(_))
        ));
    }

    /// An add-on kept while it was still "fetching" (the first layout)
    /// reads as transferring, with nothing here.
    #[test]
    fn an_add_on_kept_fetching_reads_as_transferring() {
        let read: StoredAddon = serde_json::from_value(serde_json::json!({
            "kind": "subtitle", "place": 2, "path": null, "format": "web_vtt",
            "language": "eng", "state": {"type": "fetching", "done_percent": null},
        }))
        .unwrap();
        assert_eq!(
            (read.addon.state, read.addon.received, read.id),
            (AddonState::Transferring { done_percent: None }, 0, None)
        );
    }

    /// Queued again, an audio add-on the server failed is prepared anew,
    /// without its server id; one failed on this device keeps it. Both
    /// keep what arrived until the new file is known.
    #[test]
    fn an_add_on_the_server_failed_is_prepared_anew_when_queued_again() {
        let mut m = crate::downloads::store::tests::manifest("ab12", 1);
        let failed = |place, cause| StoredAddon {
            addon: DownloadAddon {
                kind: AddonKind::Audio,
                place,
                received: 7,
                ..audio(
                    AddonState::Failed {
                        reason: "failed".into(),
                        cause,
                    },
                    None,
                )
            },
            id: Some("d1".into()),
            sha256: Some("aa".into()),
        };
        m.addons = vec![
            failed(1, DownloadFailure::ServerStoppedConverting),
            failed(2, DownloadFailure::SourceMissing),
            failed(3, DownloadFailure::Other),
            failed(4, DownloadFailure::Network),
            failed(5, DownloadFailure::NoSpace { needed_bytes: 1 }),
        ];
        for place in 1..=5 {
            queue(&mut m, place);
        }
        let ids: Vec<_> = m
            .addons
            .iter()
            .map(|a| (a.id.as_deref(), a.addon.received))
            .collect();
        assert_eq!(
            ids,
            [
                (None, 7),
                (None, 7),
                (None, 7),
                (Some("d1"), 7),
                (Some("d1"), 7)
            ]
        );
    }

    /// Queued again, a failed audio add-on goes on from what arrived, with
    /// its server id; one here, or under way, is left as it is.
    #[test]
    fn a_failed_audio_add_on_queued_again_keeps_what_arrived() {
        let mut m = crate::downloads::store::tests::manifest("ab12", 1);
        let stored = |state, received| StoredAddon {
            addon: DownloadAddon {
                kind: AddonKind::Audio,
                received,
                ..audio(state, None)
            },
            id: Some("d1".into()),
            sha256: Some("aa".into()),
        };
        let failed = AddonState::Failed {
            reason: "cut".into(),
            cause: DownloadFailure::Network,
        };
        let moving = AddonState::Transferring {
            done_percent: Some(10),
        };
        m.addons = vec![stored(failed, 9 << 20), stored(AddonState::Ready, 7)];
        m.addons[1].addon.place = 2;
        m.addons.push(stored(moving.clone(), 5));
        m.addons[2].addon.place = 3;
        for place in [1, 2, 3] {
            queue(&mut m, place);
        }
        let states: Vec<_> = m
            .addons
            .iter()
            .map(|a| (a.addon.state.clone(), a.addon.received, a.id.clone()))
            .collect();
        assert_eq!(
            states,
            [
                (
                    AddonState::Preparing {
                        server: DownloadState::Waiting
                    },
                    9 << 20,
                    Some("d1".into())
                ),
                (AddonState::Ready, 7, Some("d1".into())),
                (moving, 5, Some("d1".into())),
            ]
        );
    }
}
