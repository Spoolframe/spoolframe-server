//! Downloads for offline viewing (PX-38): the file a device fetches, made
//! from a media file and, where the device needs it, PX-37's copy.
//!
//! A download is asked for ([`Downloads::prepare`]) by a user, for a
//! media file, a source (original quality or a profile), its tracks and
//! the device class; [`plan`] says how its file is written. The file is
//! named by its **content key**: a hash of all it is made of (the media
//! file as it is now, the tracks, the container, how each subtitle is
//! carried, and the ids of the stored tracks it reads, as a track remade
//! from a GPU encoder may not be the same bytes). Two users' same request
//! is one file; a remade track is a new file with a new SHA-256, so a
//! device starts over rather than resuming into another file.
//!
//! The ids are kept in the database (migration 0040), so a restart does
//! not write a ready file again. A download is `Waiting` (for its copy,
//! for room in the area, or for a place in the budget), `Converting` (its
//! copy, or its subtitles read as text, being made), `Writing`, then
//! `Ready` with its size and SHA-256, or `Failed`; preparing it again
//! tries again. Sent as it is, the file is the media file itself, hashed
//! once (and remembered by its path and stamp).
//!
//! The area (`downloads.dir`, within `downloads.max_size_mb`): each file
//! is written as `making/<key>/<key>.part` and renamed `<key>.mp4` or
//! `<key>.mkv`. Over the limit a new file waits until another goes. A
//! file being read is never removed; one goes once every download of it
//! is cancelled, or a day after it was last read (by the store's clock).
//! `making/` is emptied at start, and what a stopped server was preparing
//! fails.
//!
//! The pool is always here, whatever `optimized.binge_ahead` says (the
//! user's decision, 2026-10-20).
//!
//! A track is downloaded the same way (PX-51, [`Downloads::prepare_music`]):
//! [`music`] says how its file is written, and the pool's copy for its
//! class and quality converts its one audio track. What an artist, a
//! playlist or Liked a device follows holds is [`followed`]'s.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, broadcast, watch};

use spoolframe_media::{MediaDescriptor, SubtitleSource, codecs};
use spoolframe_protocol::downloads::{DownloadEstimate, DownloadReadiness, DownloadSource};
use spoolframe_types::{AudioCodec, BitmapPreference, ClientCapabilities, Container, Platform};

use crate::config::DownloadSettings;
use crate::encode::job::{JobError, Jobs, Load, remove_file, stopped};
use crate::optimized::mux::{DefaultSubtitle, MuxFormat, MuxPicks};
use crate::optimized::{CopyFiles, Pool};
use crate::principal::Principal;
use crate::store::downloads::{DownloadRow, NewDownload, Of, StoredState};
use crate::store::optimized::{CopyState, NewCopy};
use crate::store::{Store, StoreError};
use crate::subtitle_sync::SubtitleTimings;
use crate::subtitles::{Subtitles, TextSource, TextState};

pub mod followed;
pub mod music;
pub mod plan;
mod write;

use music::{MusicFormat, MusicQuality};
use plan::{Decision, PlanError, Source, Subtitle, Tracks};
use write::{Run, RunError, SubtitleFile};

/// How long a ready file stays after it was last read.
pub const KEPT_AFTER_READ: Duration = Duration::from_secs(24 * 60 * 60);

/// How often the area looks for files to remove besides when a download
/// is cancelled or the last read of a file it kept ends.
pub const SWEEP_EVERY: Duration = Duration::from_secs(60 * 60);

/// A SHA-256 being made, or made, once for everyone who asks.
type Hashing = Arc<tokio::sync::OnceCell<String>>;

/// The most hashes of files sent as they are kept in memory; a ready
/// download's row keeps its own.
const MAX_HASHES: usize = 16;

/// How stale a download's last read may be before a read writes it
/// again: a film is read in many chunks, and a day's keep needs no
/// closer count.
const READ_NOTED_EVERY: Duration = Duration::from_secs(10 * 60);

/// Whether a read now writes the download's last read, `last`.
fn note_read(last: Option<i64>, now: i64) -> bool {
    last.is_none_or(|last| last + READ_NOTED_EVERY.as_secs() as i64 <= now)
}

/// The directory in the area files are written in: emptied at start.
pub const MAKING_DIR: &str = "making";

/// How often a download waiting for its copy looks at it.
const COPY_LOOK: Duration = Duration::from_millis(250);

/// How long a file waiting for room or a place waits with no word.
const WAIT_FALLBACK: Duration = Duration::from_secs(30);

/// What a download that failed because the server stopped says.
const STOPPED: &str = "the server stopped while the download was being prepared: prepare it again";

/// What a download sent as it is says once its media file changed: the
/// file is no longer the one its size and SHA-256 are of.
const MEDIA_CHANGED: &str = "the media file changed since the download was made: prepare it again";

/// Where a download is.
#[derive(Debug, Clone, PartialEq)]
pub enum DownloadState {
    /// For its copy's turn, for room in the area, or for a place.
    Waiting,
    /// Its copy (or its subtitles' text) is being made: of it, 0 to 1,
    /// and how long it has yet to go, once known.
    Converting {
        progress: Option<f64>,
        eta: Option<Duration>,
    },
    /// Its file is being written: of the film, 0 to 1, once known.
    Writing { progress: Option<f64> },
    /// Its file is whole: `size` bytes, with this SHA-256 (hex).
    Ready { size: u64, sha256: String },
    /// It could not be made; preparing it again tries again.
    Failed { reason: String },
}

impl DownloadState {
    fn of(stored: &StoredState) -> Self {
        match stored {
            StoredState::Waiting => Self::Waiting,
            StoredState::Converting => Self::Converting {
                progress: None,
                eta: None,
            },
            StoredState::Writing => Self::Writing { progress: None },
            StoredState::Ready {
                size_bytes, sha256, ..
            } => Self::Ready {
                size: *size_bytes,
                sha256: sha256.clone(),
            },
            StoredState::Failed(reason) => Self::Failed {
                reason: reason.clone(),
            },
        }
    }

    /// As the wire says it (PX-38's `DownloadState`).
    pub fn to_wire(&self) -> spoolframe_protocol::downloads::DownloadState {
        use spoolframe_protocol::downloads::DownloadState as Wire;
        let percent = |p: &Option<f64>| p.map(|p| (p.clamp(0.0, 1.0) * 100.0) as u8);
        match self {
            Self::Waiting => Wire::Waiting,
            Self::Converting { progress, eta } => Wire::Converting {
                done_percent: percent(progress),
                eta_secs: eta.map(|eta| eta.as_secs()),
            },
            Self::Writing { progress } => Wire::Writing {
                done_percent: percent(progress),
            },
            Self::Ready { size, sha256 } => Wire::Ready {
                size: *size,
                sha256: sha256.clone(),
            },
            Self::Failed { reason } => Wire::Failed {
                reason: reason.clone(),
            },
        }
    }

    /// As the database keeps it, ready ones aside.
    fn stored(&self) -> Option<StoredState> {
        match self {
            Self::Waiting => Some(StoredState::Waiting),
            Self::Converting { .. } => Some(StoredState::Converting),
            Self::Writing { .. } => Some(StoredState::Writing),
            Self::Failed { reason } => Some(StoredState::Failed(reason.clone())),
            Self::Ready { .. } => None,
        }
    }
}

/// A download's state changed: for the events to come (PX-38's wire).
#[derive(Debug, Clone, PartialEq)]
pub struct DownloadChange {
    pub id: String,
    /// Whose, as [`Principal`] is stored.
    pub principal: String,
    pub state: DownloadState,
}

/// A download asked for.
#[derive(Debug, Clone)]
pub struct PrepareRequest<'a> {
    pub principal: Principal,
    pub media_id: &'a str,
    pub media: &'a Path,
    /// The file as playback sees it, with the subtitles beside it.
    pub descriptor: &'a MediaDescriptor,
    pub class: DeviceClass,
    /// What the device plays; the class's own when it said nothing.
    pub capabilities: Option<&'a ClientCapabilities>,
    pub source: Source,
    pub tracks: Tracks,
    /// The viewer's PX-13 choice for picture subtitles.
    pub bitmap: BitmapPreference,
    /// The subtitle playback picks for the viewer, by its position in
    /// the descriptor: the one the file marks default, if it carries it.
    pub default_subtitle: Option<u32>,
}

/// A track's download asked for (PX-51).
#[derive(Debug, Clone)]
pub struct MusicRequest<'a> {
    pub principal: Principal,
    pub media_id: &'a str,
    pub media: &'a Path,
    pub descriptor: &'a MediaDescriptor,
    pub class: DeviceClass,
    pub quality: MusicQuality,
}

/// A download prepared.
#[derive(Debug, Clone, PartialEq)]
pub struct Prepared {
    pub id: String,
    /// The file it is written as.
    pub content_key: String,
    pub container: Container,
    pub decision: Decision,
    /// The subtitles chosen that are not in the file (by position), and
    /// why: picture subtitles in an Apple file, say.
    pub left_out: Vec<(u32, String)>,
    /// The tracks the file carries.
    pub carried: Carried,
    pub state: DownloadState,
}

/// The tracks a download's file carries, by their position in the file's
/// description, in the file's order after its video.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Carried {
    pub audio: Vec<u32>,
    pub subtitles: Vec<u32>,
    /// What the file is, when not the class's film container: a track's
    /// (PX-51).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<Container>,
}

/// A download as its user may know it.
#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub state: DownloadState,
    /// The class it is written for; `None` for a request this build does
    /// not read.
    pub class: Option<DeviceClass>,
    /// The tracks it carries; `None` for a row this build cannot read.
    pub carried: Option<Carried>,
}

/// A ready download's file, open to read. While it is held the file is
/// not removed. Opening it counts as a read: the file stays another day.
#[derive(Debug)]
pub struct DownloadRead {
    pub file: std::fs::File,
    pub path: PathBuf,
    pub size: u64,
    pub sha256: String,
    _reading: Reading,
}

/// A read of content `key` in progress.
#[derive(Debug)]
struct Reading {
    inner: Arc<Inner>,
    key: String,
}

impl Drop for Reading {
    fn drop(&mut self) {
        let wake = self.inner.state.lock().read_ended(&self.key);
        if wake {
            self.inner.sweep_wake.notify_one();
        }
    }
}

/// Why a download cannot be prepared, read or cancelled.
#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error("no profile is called {0}")]
    NoProfile(String),
    #[error("the media file cannot be read: {0}")]
    Media(std::io::Error),
    /// No such download of this user's.
    #[error("no such download")]
    NotFound,
    #[error("the download is not ready")]
    NotReady,
    #[error("the download's file cannot be opened: {0}")]
    Open(std::io::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Why the downloads could not start.
#[derive(Debug, thiserror::Error)]
pub enum DownloadsStartError {
    #[error("cannot prepare {dir}: {source}")]
    Dir {
        dir: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// What downloads work with.
#[derive(Debug, Clone)]
pub struct DownloadSources {
    pub store: Store,
    /// The copy pool.
    pub pool: Arc<Pool>,
    pub jobs: Arc<Jobs>,
    /// Picture subtitles read as text.
    pub subtitles: Arc<Subtitles>,
    /// Subtitles beside the file as they are served (PX-03).
    pub timings: Arc<SubtitleTimings>,
}

/// Downloads. Dropping it stops what it writes.
#[derive(Debug)]
pub struct Downloads {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    store: Store,
    pool: Arc<Pool>,
    jobs: Arc<Jobs>,
    subtitles: Arc<Subtitles>,
    timings: Arc<SubtitleTimings>,
    dir: PathBuf,
    making: PathBuf,
    max_size: u64,
    state: Mutex<State>,
    /// A file went: one waiting for room looks again.
    room: Notify,
    /// A file was made, or the last read of one the sweep kept ended:
    /// the sweep looks again.
    sweep_wake: Notify,
    changes: broadcast::Sender<DownloadChange>,
    /// The SHA-256 of files sent as they are, by path and stamp, each
    /// hashed once however many ask at a time; at most [`MAX_HASHES`]
    /// once made.
    hashes: Mutex<HashMap<(PathBuf, String), Hashing>>,
    /// Bytes of download files handed to reads since start.
    sent: std::sync::atomic::AtomicU64,
    stop: watch::Sender<bool>,
}

#[derive(Debug, Default)]
struct State {
    /// The files being made, by content key.
    making: HashMap<String, Making>,
    /// The files being read, and by how many.
    readers: HashMap<String, usize>,
    /// The files the sweep would have removed but for a read: it looks
    /// again once the last read of one ends, and no other read's end
    /// wakes it (about one per 8 MiB chunk sent).
    held: HashSet<String>,
}

/// A file being made.
#[derive(Debug)]
struct Making {
    state: DownloadState,
    /// What it may grow to, once it is being written: the budget keeps
    /// room for it.
    reserved: u64,
    /// Told when no download wants it any more.
    cancel: watch::Sender<bool>,
    /// When its progress was last told.
    told: Option<(Instant, Option<u32>)>,
    /// Joined by a download after `cancel` was sent: made again, from
    /// the start, once the cancelled run has stopped.
    again: bool,
    /// Which run of it this is: a progress of a run that stopped is not
    /// told.
    run: u32,
}

/// What a file is made of, once planned.
#[derive(Debug, Clone)]
struct Job {
    key: String,
    media: PathBuf,
    stamp: String,
    descriptor: MediaDescriptor,
    decision: Decision,
    /// The pool's copy it reads, for a conversion.
    copy: Option<String>,
    /// The copy was not ready when the download was asked for: it is
    /// converted for it, and says so however quickly that goes.
    converts: bool,
    writes: Writes,
    picks: MuxPicks,
    /// The subtitles beside the file, by position.
    sidecars: Vec<SubtitleFile>,
    /// The picture subtitles to read as text, by position.
    ocr: Vec<u32>,
}

/// What a file is written as.
#[derive(Debug, Clone, Copy)]
enum Writes {
    /// A film or an episode, with its tracks and subtitles.
    Film(MuxFormat),
    /// A track (PX-51), its tags and cover.
    Track(MusicFormat),
}

impl Writes {
    /// Every format a file is written as, once each.
    fn all() -> impl Iterator<Item = Self> {
        [MuxFormat::Matroska, MuxFormat::Mp4 { hvc1: false }]
            .map(Self::Film)
            .into_iter()
            .chain(MusicFormat::ALL.map(Self::Track))
    }

    /// The file's extension: what the writer names it with and the sweep
    /// knows it by.
    fn extension(self) -> &'static str {
        match self {
            Self::Film(MuxFormat::Matroska) => "mkv",
            Self::Film(MuxFormat::Mp4 { .. }) => "mp4",
            Self::Track(format) => format.extension(),
        }
    }
}

/// A download asked for, as [`Downloads::register`] keeps it.
struct Asked<'a> {
    principal: Principal,
    media_id: &'a str,
    /// As the database keeps it ([`RequestKey`], [`MusicKey`]).
    request: String,
    container: Container,
    left_out: Vec<(u32, String)>,
    carried: Carried,
}

/// The request as the database keeps it: the same one is the same id.
#[derive(Serialize)]
struct RequestKey<'a> {
    class: &'static str,
    source: &'a Source,
    tracks: &'a Tracks,
}

/// A track's request as the database keeps it (PX-51).
#[derive(Serialize)]
struct MusicKey {
    class: &'static str,
    music: MusicQuality,
}

impl Downloads {
    /// Downloads over `settings.dir`: what a stopped server was writing is
    /// removed and its downloads fail, a ready file that is gone fails,
    /// and a file no download names, or kept a day past its last read, is
    /// removed.
    pub async fn start(
        sources: DownloadSources,
        settings: &DownloadSettings,
    ) -> Result<Self, DownloadsStartError> {
        let dir = settings.dir.clone();
        let making = dir.join(MAKING_DIR);
        if crate::files::fresh_making_dir(&dir, &making)
            .map_err(|(dir, source)| DownloadsStartError::Dir { dir, source })?
        {
            tracing::info!(dir = %making.display(), "removed the download files a stopped server was writing");
        }
        let store = sources.store;
        let failed = store.fail_unfinished_downloads(STOPPED).await?;
        if failed > 0 {
            tracing::info!(
                downloads = failed,
                "downloads a stopped server was preparing failed"
            );
        }
        for row in store.downloads().await? {
            let StoredState::Ready { file, .. } = &row.state else {
                continue;
            };
            let file = Path::new(file);
            let why = if !file.exists() {
                "its file is gone: prepare it again"
            } else if !file.starts_with(&dir) && !unchanged(file, &row.stamp).await {
                MEDIA_CHANGED
            } else {
                continue;
            };
            tracing::warn!(
                download = row.id,
                why,
                "a ready download is no longer ready"
            );
            store
                .set_download_state(Of::Id(&row.id), &StoredState::Failed(why.into()))
                .await?;
        }
        let inner = Arc::new(Inner {
            store,
            pool: sources.pool,
            jobs: sources.jobs,
            subtitles: sources.subtitles,
            timings: sources.timings,
            dir,
            making,
            max_size: settings.max_size_bytes,
            state: Mutex::default(),
            room: Notify::new(),
            sweep_wake: Notify::new(),
            changes: broadcast::Sender::new(256),
            hashes: Mutex::default(),
            sent: Default::default(),
            stop: watch::Sender::new(false),
        });
        inner.sweep().await?;
        tokio::spawn(inner.clone().sweep_every(SWEEP_EVERY));
        Ok(Self { inner })
    }

    /// The copy pool a download's conversion is made in.
    pub fn pool(&self) -> &Arc<Pool> {
        &self.inner.pool
    }

    /// The area's directory.
    pub fn dir(&self) -> &Path {
        &self.inner.dir
    }

    /// `bytes` of a download's file are on their way to a device.
    pub fn sending(&self, bytes: u64) {
        self.inner
            .sent
            .fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
    }

    /// How many bytes of download files reads were sent since start: what
    /// shows a transfer broken off went on from where it stopped.
    pub fn sent(&self) -> u64 {
        self.inner.sent.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Every download's state change from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<DownloadChange> {
        self.inner.changes.subscribe()
    }

    /// Prepare `request`: its id (the same as before for the same user
    /// and request) and its state now. A ready file of the same content,
    /// whoever asked for it, is ready at once; one being made is joined;
    /// else it is made, from now, in the background. One that failed is
    /// tried again.
    pub async fn prepare(&self, request: PrepareRequest<'_>) -> Result<Prepared, DownloadError> {
        let inner = &self.inner;
        let descriptor = request.descriptor;
        let tracks = request.tracks.normalized();
        let class = request.class;
        let default_caps;
        let caps = match request.capabilities {
            Some(caps) => caps,
            None => {
                default_caps = class.capabilities();
                &default_caps
            }
        };
        let decision = match &request.source {
            Source::Original => plan::original(descriptor, class, caps, &tracks)?,
            Source::Profile(name) => {
                let profile = inner
                    .store
                    .optimized_profile(name)
                    .await?
                    .ok_or_else(|| DownloadError::NoProfile(name.clone()))?;
                plan::profile(descriptor, &profile, caps, &tracks)?
            }
        };
        let stamp = crate::files::stamp_text(request.media)
            .await
            .map_err(DownloadError::Media)?;
        let audio = tracks.audio(descriptor);
        let ways = plan::subtitles(
            descriptor,
            class,
            &tracks.subtitles(descriptor),
            request.bitmap,
            inner.subtitles.ocr(),
        );
        let decision = plan::settled(decision, &ways);
        let mut left_out = Vec::new();
        let mut written = Vec::new();
        let mut ocr = Vec::new();
        let mut sidecars = Vec::new();
        let mut parts = vec![
            "download:1".to_owned(),
            request.media.display().to_string(),
            stamp.clone(),
            class.as_str().to_owned(),
            format!("{decision:?}"),
            format!("audio:{audio:?}"),
        ];
        for (position, way) in &ways {
            match way {
                Subtitle::LeftOut(why) => {
                    left_out.push((*position, why.clone()));
                    continue;
                }
                Subtitle::ReadAsText => ocr.push(*position),
                Subtitle::Written => {}
            }
            written.push(*position);
            let stream = &descriptor.subtitles[*position as usize];
            let mut part = format!("subtitle:{position}:{way:?}");
            if let SubtitleSource::External { path } = &stream.source {
                // As it is served now (PX-03): its timing is in the file.
                let served = inner.timings.resolve(path).await;
                let served_stamp = crate::files::stamp_text(&served)
                    .await
                    .map_err(DownloadError::Media)?;
                part.push_str(&format!(":{}:{served_stamp}", served.display()));
                if *way == Subtitle::Written {
                    sidecars.push(SubtitleFile {
                        position: *position,
                        own: inner.jobs.is_own_subtitle(&served),
                        vobsub: codecs::subtitle_format(&stream.codec)
                            == spoolframe_types::SubtitleFormat::VobSub,
                        path: served,
                    });
                }
            }
            parts.push(part);
        }
        if !left_out.is_empty() {
            tracing::info!(media = %request.media.display(), left_out = ?left_out, "subtitles left out of a download");
        }
        let (copy, converts) = self
            .copy_for(
                request.media_id,
                request.media,
                &stamp,
                &decision,
                &mut parts,
            )
            .await?;
        let format = match class {
            DeviceClass::Linux => MuxFormat::Matroska,
            DeviceClass::Apple => {
                let codec = match &decision {
                    Decision::Convert { video, .. } if !video.copied => video.codec,
                    _ => descriptor
                        .video
                        .as_ref()
                        .map_or(spoolframe_types::VideoCodec::Unknown, |v| {
                            codecs::video_codec(&v.codec)
                        }),
                };
                MuxFormat::Mp4 {
                    hvc1: codec == spoolframe_types::VideoCodec::Hevc,
                }
            }
        };
        // The viewer's pick is the file's default: two users who pick
        // differently get two files.
        let default_subtitle = request
            .default_subtitle
            .filter(|position| written.contains(position));
        if let Some(position) = default_subtitle {
            parts.push(format!("default-subtitle:{position}"));
        }
        let content_key = crate::files::hash_parts(&parts).to_hex().to_string();
        let carried = Carried {
            audio: audio.clone(),
            subtitles: written.clone(),
            container: None,
        };
        let request_text = serde_json::to_string(&RequestKey {
            class: class.as_str(),
            source: &request.source,
            tracks: &tracks,
        })
        .expect("a request is JSON");
        let job = Job {
            key: content_key,
            media: request.media.to_path_buf(),
            stamp,
            descriptor: descriptor.clone(),
            decision: decision.clone(),
            copy,
            converts,
            writes: Writes::Film(format),
            picks: MuxPicks {
                audio: Some(audio),
                subtitles: Some(written),
                default_subtitle: DefaultSubtitle::Only(default_subtitle),
            },
            sidecars,
            ocr,
        };
        self.register(
            Asked {
                principal: request.principal,
                media_id: request.media_id,
                request: request_text,
                container: class.container(),
                left_out,
                carried,
            },
            job,
        )
        .await
    }

    /// Prepare track `request` (PX-51), as [`Downloads::prepare`] does a
    /// film: its one audio track, as [`music::plan`] says.
    pub async fn prepare_music(
        &self,
        request: MusicRequest<'_>,
    ) -> Result<Prepared, DownloadError> {
        let class = request.class;
        let planned = music::plan(request.descriptor, class, request.quality)?;
        let stamp = crate::files::stamp_text(request.media)
            .await
            .map_err(DownloadError::Media)?;
        let mut parts = vec![
            "music-download:1".to_owned(),
            request.media.display().to_string(),
            stamp.clone(),
            class.as_str().to_owned(),
            request.quality.as_str().to_owned(),
            format!("{:?}", planned.decision),
            format!("{:?}", planned.format),
        ];
        let (copy, converts) = self
            .copy_for(
                request.media_id,
                request.media,
                &stamp,
                &planned.decision,
                &mut parts,
            )
            .await?;
        let carried = Carried {
            audio: vec![0],
            subtitles: Vec::new(),
            container: Some(planned.container),
        };
        let request_text = serde_json::to_string(&MusicKey {
            class: class.as_str(),
            music: request.quality,
        })
        .expect("a request is JSON");
        let job = Job {
            key: crate::files::hash_parts(&parts).to_hex().to_string(),
            media: request.media.to_path_buf(),
            stamp,
            descriptor: request.descriptor.clone(),
            decision: planned.decision,
            copy,
            converts,
            // As it is, the file is the track's own: nothing written.
            writes: Writes::Track(planned.format.unwrap_or(MusicFormat::Matroska)),
            picks: MuxPicks {
                audio: Some(vec![0]),
                subtitles: Some(Vec::new()),
                default_subtitle: DefaultSubtitle::Only(None),
            },
            sidecars: Vec::new(),
            ocr: Vec::new(),
        };
        self.register(
            Asked {
                principal: request.principal,
                media_id: request.media_id,
                request: request_text,
                container: planned.container,
                left_out: Vec::new(),
                carried,
            },
            job,
        )
        .await
    }

    /// About each option of a download of `media`, with every track:
    /// original quality, which would be `original`, then each of
    /// `profiles` in their order. The device plays every one of them, as
    /// `caps` says; an option the plan refuses is left out.
    pub async fn estimates(
        &self,
        media: &Path,
        descriptor: &MediaDescriptor,
        original: Decision,
        profiles: &[crate::store::optimized::Profile],
        caps: &ClientCapabilities,
    ) -> Result<Vec<DownloadEstimate>, DownloadError> {
        let size = tokio::fs::metadata(media)
            .await
            .map_err(DownloadError::Media)?
            .len();
        let stamp = crate::files::stamp_text(media)
            .await
            .map_err(DownloadError::Media)?;
        let mut options = vec![(DownloadSource::Original, original)];
        for profile in profiles {
            if let Ok(decision) = plan::profile(descriptor, profile, caps, &Tracks::All) {
                let source = DownloadSource::Profile {
                    name: profile.name.clone(),
                };
                options.push((source, decision));
            }
        }
        let mut estimates = Vec::with_capacity(options.len());
        for (source, decision) in options {
            let (rate, readiness) = match &decision {
                Decision::AsIs | Decision::Remux => (None, DownloadReadiness::Ready),
                Decision::Convert { target, video, .. } => {
                    let source_rate = descriptor.video.as_ref().and_then(|v| v.bit_rate);
                    let rate = (!video.copied)
                        .then(|| video.max_bitrate.or(source_rate))
                        .flatten();
                    let made = self
                        .inner
                        .store
                        .live_optimized_copy(&media.display().to_string(), target, video)
                        .await?
                        .is_some_and(|c| c.state == CopyState::Ready && c.source_stamp == stamp);
                    let readiness = if made {
                        DownloadReadiness::Ready
                    } else {
                        DownloadReadiness::ConvertsFirst
                    };
                    (rate, readiness)
                }
            };
            estimates.push(DownloadEstimate {
                source,
                size_bytes: plan::estimate(descriptor, size, rate),
                readiness,
            });
        }
        Ok(estimates)
    }

    /// Keep `asked`, a download of `job`'s file, as the user's, and begin
    /// the file: what [`Downloads::prepare`] and
    /// [`Downloads::prepare_music`] end with.
    async fn register(&self, asked: Asked<'_>, job: Job) -> Result<Prepared, DownloadError> {
        let carried = serde_json::to_string(&asked.carried).expect("tracks are JSON");
        let row = self
            .inner
            .store
            .put_download(NewDownload {
                principal: &asked.principal.to_string(),
                media_id: asked.media_id,
                request: &asked.request,
                content_key: &job.key,
                carried: &carried,
                stamp: &job.stamp,
            })
            .await?;
        let (content_key, decision) = (job.key.clone(), job.decision.clone());
        let state = self.begin(&row, job).await?;
        Ok(Prepared {
            id: row.id,
            content_key,
            container: asked.container,
            decision,
            left_out: asked.left_out,
            carried: asked.carried,
            state,
        })
    }

    /// The pool's copy a `decision` to convert reads, asked for now, and
    /// whether it is not made yet; each stored track it reads, by id, goes
    /// into the content key's `parts`: one made again (its file evicted,
    /// say) is another file.
    async fn copy_for(
        &self,
        media_id: &str,
        media: &Path,
        stamp: &str,
        decision: &Decision,
        parts: &mut Vec<String>,
    ) -> Result<(Option<String>, bool), DownloadError> {
        let Decision::Convert { target, video, .. } = decision else {
            return Ok((None, false));
        };
        let inner = &self.inner;
        let row = inner
            .pool
            .request(NewCopy {
                media_id: media_id.to_owned(),
                media_path: media.display().to_string(),
                source_stamp: stamp.to_owned(),
                target: target.clone(),
                video: *video,
            })
            .await?;
        for track in inner.store.optimized_copy_tracks(&row.id).await? {
            parts.push(format!("track:{}", track.id));
        }
        let converts = row.state != CopyState::Ready;
        Ok((Some(row.id), converts))
    }

    /// `row`'s file: ready already (its own, or another download's of the
    /// same content), being made, or made from now as `job` says.
    async fn begin(&self, row: &DownloadRow, job: Job) -> Result<DownloadState, DownloadError> {
        let inner = &self.inner;
        if let StoredState::Ready { file, .. } = &row.state
            && Path::new(file).exists()
        {
            return Ok(DownloadState::of(&row.state));
        }
        if let Some(making) = inner.state.lock().making.get_mut(&row.content_key) {
            return Ok(making.join());
        }
        let shared = inner
            .store
            .downloads_of_content(&row.content_key)
            .await?
            .into_iter()
            .find(
                |r| matches!(&r.state, StoredState::Ready { file, .. } if Path::new(file).exists()),
            );
        if let Some(ready) = shared {
            inner
                .store
                .set_download_state(Of::Id(&row.id), &ready.state)
                .await?;
            return Ok(DownloadState::of(&ready.state));
        }
        let (cancel, cancelled) = watch::channel(false);
        {
            let mut state = inner.state.lock();
            // Begun by another prepare meanwhile: joined.
            if let Some(making) = state.making.get_mut(&row.content_key) {
                return Ok(making.join());
            }
            state
                .making
                .insert(row.content_key.clone(), Making::new(cancel, 0));
        }
        inner
            .store
            .set_download_state(Of::Content(&row.content_key), &StoredState::Waiting)
            .await?;
        tokio::spawn(inner.clone().make(job, cancelled));
        Ok(DownloadState::Waiting)
    }

    /// Download `id`'s state, if it is `principal`'s.
    pub async fn state(
        &self,
        principal: &Principal,
        id: &str,
    ) -> Result<Option<DownloadState>, DownloadError> {
        Ok(self.status(principal, id).await?.map(|status| status.state))
    }

    /// Download `id` as `principal` may know it, if it is theirs: its
    /// state, its class and the tracks it carries.
    pub async fn status(
        &self,
        principal: &Principal,
        id: &str,
    ) -> Result<Option<Status>, DownloadError> {
        Ok(self
            .own(principal, id)
            .await?
            .map(|row| self.status_of(&row)))
    }

    /// Downloads `ids` as [`Downloads::status`] says, in one query: those
    /// that are `principal`'s, by id.
    pub async fn statuses(
        &self,
        principal: &Principal,
        ids: &[String],
    ) -> Result<HashMap<String, Status>, DownloadError> {
        let principal = principal.to_string();
        Ok(self
            .inner
            .store
            .downloads_by_id(ids)
            .await?
            .into_iter()
            .filter(|row| row.principal == principal)
            .map(|row| {
                let status = self.status_of(&row);
                (row.id, status)
            })
            .collect())
    }

    /// Download `row` as its user may know it.
    fn status_of(&self, row: &DownloadRow) -> Status {
        let state = match self.inner.state.lock().making.get(&row.content_key) {
            Some(making) => making.state.clone(),
            None => DownloadState::of(&row.state),
        };
        let class = serde_json::from_str::<serde_json::Value>(&row.request)
            .ok()
            .and_then(|request| request["class"].as_str().and_then(DeviceClass::parse));
        let carried = serde_json::from_str(&row.carried).ok();
        Status {
            state,
            class,
            carried,
        }
    }

    /// Download `id`'s file, to read, if it is `principal`'s and ready.
    pub async fn open_read(
        &self,
        principal: &Principal,
        id: &str,
    ) -> Result<DownloadRead, DownloadError> {
        let row = self
            .own(principal, id)
            .await?
            .ok_or(DownloadError::NotFound)?;
        let StoredState::Ready {
            file,
            size_bytes,
            sha256,
        } = row.state
        else {
            return Err(DownloadError::NotReady);
        };
        // Held before the file is opened: the sweep skips it from now.
        *self
            .inner
            .state
            .lock()
            .readers
            .entry(row.content_key.clone())
            .or_default() += 1;
        let reading = Reading {
            inner: self.inner.clone(),
            key: row.content_key.clone(),
        };
        let path = PathBuf::from(file);
        let opened = std::fs::File::open(&path).map_err(DownloadError::Open)?;
        // Sent as it is, the file is the media file itself: one changed
        // since is not the file its size and SHA-256 are of. Checked once
        // open, so a file replaced before is caught too.
        if !path.starts_with(&self.inner.dir) && !unchanged(&path, &row.stamp).await {
            tracing::warn!(download = id, file = %path.display(), "a download's media file changed");
            self.inner.fail(&row.content_key, MEDIA_CHANGED).await?;
            return Err(DownloadError::NotReady);
        }
        if note_read(row.last_read_at, self.inner.store.now()) {
            self.inner.store.download_read(id).await?;
        }
        Ok(DownloadRead {
            file: opened,
            path,
            size: size_bytes,
            sha256,
            _reading: reading,
        })
    }

    /// Cancel download `id`, if it is `principal`'s (the device also does
    /// once it has the file): whether it was. Its file goes once no
    /// download of it is left and nobody reads it; one being made stops.
    pub async fn cancel(&self, principal: &Principal, id: &str) -> Result<bool, DownloadError> {
        if self.own(principal, id).await?.is_none() {
            return Ok(false);
        }
        let Some(row) = self.inner.store.delete_download(id).await? else {
            return Ok(false);
        };
        let left = self
            .inner
            .store
            .downloads_of_content(&row.content_key)
            .await?;
        if left.is_empty()
            && let Some(making) = self.inner.state.lock().making.get(&row.content_key)
        {
            making.cancel.send_replace(true);
        }
        self.inner.sweep().await?;
        Ok(true)
    }

    /// Remove the files no download names, and those kept a day past
    /// their last read, now: as the area does every [`SWEEP_EVERY`], and
    /// whenever a download is cancelled or the last read of a file it
    /// kept ends.
    pub async fn sweep(&self) -> Result<(), StoreError> {
        self.inner.sweep().await
    }

    /// Download `id`, if it is `principal`'s.
    async fn own(
        &self,
        principal: &Principal,
        id: &str,
    ) -> Result<Option<DownloadRow>, StoreError> {
        let principal = principal.to_string();
        Ok(self
            .inner
            .store
            .download(id)
            .await?
            .filter(|row| row.principal == principal))
    }
}

impl Drop for Downloads {
    fn drop(&mut self) {
        self.inner.stop.send_replace(true);
    }
}

/// How making a file ended other than ready.
enum Ended {
    /// Cancelled, or the server stops: nothing to say.
    Stopped,
    Failed(String),
}

impl From<RunError> for Ended {
    fn from(e: RunError) -> Self {
        match e {
            RunError::Stopped => Self::Stopped,
            RunError::Failed(why) => Self::Failed(why),
        }
    }
}

impl From<StoreError> for Ended {
    fn from(e: StoreError) -> Self {
        Self::Failed(format!("the server's database failed: {e}"))
    }
}

impl Inner {
    /// Make `job`'s file, in the background, until it is ready, fails, or
    /// `cancelled` says no download wants it.
    async fn make(self: Arc<Self>, job: Job, mut cancelled: watch::Receiver<bool>) {
        let mut stop = self.stop.subscribe();
        let ended = loop {
            let ended = tokio::select! {
                made = self.made(&job, &mut cancelled) => made,
                () = stopped(&mut stop) => Err(Ended::Stopped),
            };
            let _ = std::fs::remove_dir_all(self.making.join(&job.key));
            if !matches!(ended, Err(Ended::Stopped)) || *stop.borrow() {
                break ended;
            }
            // Cancelled. Joined meanwhile by a download prepared since, it
            // is made again for it; else it goes, in the same look, so a
            // download prepared from now begins its own.
            let again = {
                let mut state = self.state.lock();
                match state.making.get_mut(&job.key) {
                    Some(making) if making.again => {
                        let (cancel, receiver) = watch::channel(false);
                        *making = Making::new(cancel, making.run.wrapping_add(1));
                        Some(receiver)
                    }
                    _ => {
                        state.making.remove(&job.key);
                        None
                    }
                }
            };
            let Some(receiver) = again else {
                break ended;
            };
            cancelled = receiver;
            tracing::info!(
                key = job.key,
                "a cancelled download's file is made again for a download asked for since"
            );
            let waiting = self
                .store
                .set_download_state(Of::Content(&job.key), &StoredState::Waiting)
                .await;
            if let Err(e) = waiting {
                tracing::warn!(error = %e, "cannot record a download's wait");
            }
            self.tell(&job.key, DownloadState::Waiting).await;
        };
        let stored = match ended {
            Ok(ready) => Some(ready),
            Err(Ended::Failed(reason)) => {
                tracing::warn!(key = job.key, media = %job.media.display(), reason, "a download failed");
                Some(StoredState::Failed(reason))
            }
            Err(Ended::Stopped) => None,
        };
        let state = match &stored {
            Some(stored) => Some(self.finish(&job.key, stored).await),
            None => None,
        };
        self.state.lock().making.remove(&job.key);
        if let Some(stored) = &stored {
            // Again: a download asked for while the first was recorded
            // joined this file, and its row may have missed it.
            if let Err(e) = self
                .store
                .set_download_state(Of::Content(&job.key), stored)
                .await
            {
                tracing::warn!(error = %e, "cannot record a download's end");
            }
        }
        if let Some(state) = state {
            self.tell(&job.key, state).await;
        }
        // Its reserved room is free, and a cancelled one's file may go.
        self.room.notify_waiters();
        self.sweep_wake.notify_one();
    }

    /// File `key` ended `stored` (ready or failed): recorded, and the
    /// state to tell.
    async fn finish(&self, key: &str, stored: &StoredState) -> DownloadState {
        match self
            .store
            .set_download_state(Of::Content(key), stored)
            .await
        {
            Ok(()) => DownloadState::of(stored),
            Err(e) => {
                tracing::warn!(error = %e, "cannot record a download's end");
                DownloadState::Failed {
                    reason: format!("the server's database failed: {e}"),
                }
            }
        }
    }

    /// Every download of file `key`, sent as it is, fails for `why`: its
    /// media file changed. Recorded and told.
    async fn fail(&self, key: &str, why: &str) -> Result<(), StoreError> {
        let failed = StoredState::Failed(why.to_owned());
        self.store
            .set_download_state(Of::Content(key), &failed)
            .await?;
        self.tell(key, DownloadState::of(&failed)).await;
        Ok(())
    }

    /// [`Inner::make`]'s work: the file ready, as the database keeps it.
    async fn made(
        self: &Arc<Self>,
        job: &Job,
        cancelled: &mut watch::Receiver<bool>,
    ) -> Result<StoredState, Ended> {
        match crate::files::stamp_text(&job.media).await {
            Ok(stamp) if stamp == job.stamp => {}
            Ok(_) => {
                return Err(Ended::Failed(
                    "the media file changed since the download was asked for: prepare it again"
                        .into(),
                ));
            }
            Err(e) => return Err(Ended::Failed(format!("the media file cannot be read: {e}"))),
        }
        if job.decision == Decision::AsIs {
            self.set(&job.key, DownloadState::Writing { progress: None })
                .await?;
            let sha256 = self.hash_as_is(&job.media, &job.stamp).await?;
            let size = std::fs::metadata(&job.media).map_or(0, |m| m.len());
            return Ok(StoredState::Ready {
                file: job.media.display().to_string(),
                size_bytes: size,
                sha256,
            });
        }
        let mut stored_size = 0;
        if let Some(copy) = &job.copy {
            stored_size = self
                .copy_made(&job.key, copy, job.converts, cancelled)
                .await?;
        }
        let mut subtitles = job.sidecars.clone();
        for &position in &job.ocr {
            let text = self.read_as_text(job, position, cancelled).await?;
            subtitles.push(SubtitleFile {
                position,
                path: text,
                own: true,
                vobsub: false,
            });
        }
        let media_size = std::fs::metadata(&job.media).map_or(0, |m| m.len());
        let expected = media_size + stored_size;
        if expected > self.max_size {
            return Err(Ended::Failed(format!(
                "the file may grow to {} MiB, more than the download area holds \
                 (downloads.max_size_mb)",
                expected >> 20
            )));
        }
        self.room_for(&job.key, expected, cancelled).await?;
        let _slot = self.place(&job.key, cancelled).await?;
        let lease = match &job.copy {
            Some(copy) => Some(self.pool.lease(copy).await?.ok_or_else(|| {
                Ended::Failed("the converted copy was removed: prepare it again".into())
            })?),
            None => None,
        };
        let stored = lease
            .as_ref()
            .map_or_else(CopyFiles::default, |l| l.files().clone());
        let dir = self.making.join(&job.key);
        let part = dir.join(format!("{}.part", job.key));
        let run = Run {
            jobs: &self.jobs,
            dir: &dir,
            media: &job.media,
            descriptor: &job.descriptor,
            stored: &stored,
            subtitles: &subtitles,
            picks: &job.picks,
            max_size: self.max_size,
        };
        self.set(&job.key, DownloadState::Writing { progress: None })
            .await?;
        tracing::info!(key = job.key, media = %job.media.display(), "writing a download's file");
        let key = job.key.clone();
        let progress = |fraction: Option<f64>| self.progress(&key, fraction);
        match job.writes {
            Writes::Film(format) => write::write(&run, format, &part, progress, cancelled).await?,
            Writes::Track(format) => {
                write::write_music(&run, format, &part, progress, cancelled).await?;
            }
        }
        drop(lease);
        let (sha256, size) = write::sha256(&part)
            .await
            .map_err(|e| Ended::Failed(format!("the file cannot be read back: {e}")))?;
        let ext = job.writes.extension();
        let file = self.dir.join(format!("{}.{ext}", job.key));
        std::fs::rename(&part, &file)
            .map_err(|e| Ended::Failed(format!("cannot keep the file: {e}")))?;
        tracing::info!(key = job.key, bytes = size, "a download's file is ready");
        Ok(StoredState::Ready {
            file: file.display().to_string(),
            size_bytes: size,
            sha256,
        })
    }

    /// Until `copy` is made: the size of its stored tracks. Converting
    /// while it is made, waiting while it waits. One made for this
    /// download (`converts`) is told converted, whole, once made, so a
    /// conversion done between two looks is still told.
    async fn copy_made(
        &self,
        key: &str,
        copy: &str,
        converts: bool,
        cancelled: &mut watch::Receiver<bool>,
    ) -> Result<u64, Ended> {
        loop {
            let Some(progress) = self.pool.progress(copy).await? else {
                return Err(Ended::Failed(
                    "the converted copy was removed: prepare it again".into(),
                ));
            };
            match progress.state {
                CopyState::Ready => {
                    if converts {
                        self.set(
                            key,
                            DownloadState::Converting {
                                progress: Some(1.0),
                                eta: Some(Duration::ZERO),
                            },
                        )
                        .await?;
                    }
                    let row = self.store.optimized_copy(copy).await?;
                    return Ok(row.and_then(|r| r.size_bytes).unwrap_or(0));
                }
                CopyState::Failed => {
                    let why = self
                        .store
                        .optimized_copy(copy)
                        .await?
                        .and_then(|r| r.failure)
                        .unwrap_or_else(|| "the conversion failed".into());
                    return Err(Ended::Failed(format!("the conversion failed: {why}")));
                }
                CopyState::Waiting => self.set(key, DownloadState::Waiting).await?,
                CopyState::Making => {
                    self.set(
                        key,
                        DownloadState::Converting {
                            progress: progress.fraction,
                            eta: progress.eta,
                        },
                    )
                    .await?;
                }
            }
            pause(COPY_LOOK, cancelled).await?;
        }
    }

    /// Picture subtitle `position` of `job`'s file read as text: the text
    /// file, in the subtitles' cache.
    async fn read_as_text(
        &self,
        job: &Job,
        position: u32,
        cancelled: &mut watch::Receiver<bool>,
    ) -> Result<PathBuf, Ended> {
        let stream = &job.descriptor.subtitles[position as usize];
        let mut text = self
            .subtitles
            .text(TextSource {
                path: &job.media,
                stream,
                to: spoolframe_types::SubtitleFormat::Srt,
            })
            .await;
        loop {
            let now = text.borrow_and_update().clone();
            match now {
                TextState::Ready(path) => return Ok(path),
                TextState::Failed(why) => {
                    return Err(Ended::Failed(format!(
                        "subtitle {position} could not be read as text: {why}"
                    )));
                }
                TextState::Waiting => self.set(&job.key, DownloadState::Waiting).await?,
                TextState::Reading { done_percent } => {
                    self.set(
                        &job.key,
                        DownloadState::Converting {
                            progress: Some(f64::from(done_percent) / 100.0),
                            eta: None,
                        },
                    )
                    .await?;
                }
            }
            tokio::select! {
                changed = text.changed() => {
                    if changed.is_err() && !text.borrow().finished() {
                        return Err(Ended::Failed(format!(
                            "subtitle {position} could not be read as text"
                        )));
                    }
                }
                () = stopped(cancelled) => return Err(Ended::Stopped),
            }
        }
    }

    /// Until the area has room for `expected` more bytes beside its
    /// files and those being written, then held for file `key`.
    async fn room_for(
        &self,
        key: &str,
        expected: u64,
        cancelled: &mut watch::Receiver<bool>,
    ) -> Result<(), Ended> {
        loop {
            let room = self.room.notified();
            let used = self.used(key).await?;
            if used + expected <= self.max_size {
                if let Some(making) = self.state.lock().making.get_mut(key) {
                    making.reserved = expected;
                }
                return Ok(());
            }
            self.set(key, DownloadState::Waiting).await?;
            tokio::select! {
                () = room => {}
                () = tokio::time::sleep(WAIT_FALLBACK) => {}
                () = stopped(cancelled) => return Err(Ended::Stopped),
            }
        }
    }

    /// What the area holds besides file `key`: its ready files, and what
    /// those being written may grow to.
    async fn used(&self, key: &str) -> Result<u64, StoreError> {
        let mut counted = HashSet::new();
        let mut used = 0;
        for row in self.store.downloads().await? {
            if let StoredState::Ready {
                file, size_bytes, ..
            } = &row.state
                && Path::new(file).starts_with(&self.dir)
                && counted.insert(row.content_key.clone())
            {
                used += size_bytes;
            }
        }
        let state = self.state.lock();
        used += state
            .making
            .iter()
            .filter(|(k, _)| k.as_str() != key)
            .map(|(_, m)| m.reserved)
            .sum::<u64>();
        Ok(used)
    }

    /// A place in the encode budget for a remux, once there is one.
    async fn place(
        &self,
        key: &str,
        cancelled: &mut watch::Receiver<bool>,
    ) -> Result<crate::encode::job::Slot, Ended> {
        loop {
            match self.jobs.hold(Load::Remux, None) {
                Ok(slot) => return Ok(slot),
                Err(JobError::Busy) => {
                    self.set(key, DownloadState::Waiting).await?;
                    pause(Duration::from_secs(1), cancelled).await?;
                }
                Err(e) => return Err(Ended::Failed(e.to_string())),
            }
        }
    }

    /// The SHA-256 of `media` sent as it is: a ready download's of it at
    /// `stamp`, else hashed, once per path and stamp however many ask at
    /// a time.
    async fn hash_as_is(&self, media: &Path, stamp: &str) -> Result<String, Ended> {
        if let Some(sha) = self
            .store
            .ready_sha256_of(&media.display().to_string(), stamp)
            .await?
        {
            return Ok(sha);
        }
        let key = (media.to_path_buf(), stamp.to_owned());
        let once = self.hashes.lock().entry(key).or_default().clone();
        let sha = once
            .get_or_try_init(|| async {
                write::sha256(media)
                    .await
                    .map(|(sha, _)| sha)
                    .map_err(|e| Ended::Failed(format!("the media file cannot be read: {e}")))
            })
            .await?
            .clone();
        let mut hashes = self.hashes.lock();
        if hashes.len() > MAX_HASHES {
            // Those being hashed stay; the rows keep the others.
            hashes.retain(|_, once| !once.initialized());
        }
        Ok(sha)
    }

    /// File `key` is in `state` now: kept, recorded when it is a step
    /// (not a progress), and told.
    async fn set(&self, key: &str, state: DownloadState) -> Result<(), StoreError> {
        let step = {
            let mut guard = self.state.lock();
            let Some(making) = guard.making.get_mut(key) else {
                return Ok(());
            };
            if making.state == state {
                return Ok(());
            }
            let step = std::mem::discriminant(&making.state) != std::mem::discriminant(&state);
            if !step && !making.due(&state) {
                making.state = state;
                return Ok(());
            }
            making.state = state.clone();
            step
        };
        if step && let Some(stored) = state.stored() {
            self.store
                .set_download_state(Of::Content(key), &stored)
                .await?;
        }
        self.tell(key, state).await;
        Ok(())
    }

    /// File `key`'s writing has got to `fraction`, from ffmpeg's progress:
    /// told from a task of its own, as ffmpeg's progress cannot wait. It
    /// is told only while that run is still writing, checked as it is
    /// sent: one that comes once the file is ready or failed (recorded,
    /// its making gone) is dropped, so it never follows the end.
    fn progress(self: &Arc<Self>, key: &str, fraction: Option<f64>) {
        let state = DownloadState::Writing { progress: fraction };
        let run = {
            let mut guard = self.state.lock();
            let Some(making) = guard.making.get_mut(key) else {
                return;
            };
            let due = making.due(&state);
            making.state = state.clone();
            if !due {
                return;
            }
            making.run
        };
        let inner = self.clone();
        let key = key.to_owned();
        tokio::spawn(async move {
            let Some(rows) = rows_of(&inner.store, &key).await else {
                return;
            };
            let guard = inner.state.lock();
            if guard.writing(&key, run) {
                send(&inner.changes, rows, &state);
            }
        });
    }

    /// Tell every download of file `key` that it is in `state`.
    async fn tell(&self, key: &str, state: DownloadState) {
        tell(&self.store, &self.changes, key, state).await;
    }

    /// [`Downloads::sweep`] every `every`, and when woken, until stopped.
    async fn sweep_every(self: Arc<Self>, every: Duration) {
        let mut stop = self.stop.subscribe();
        loop {
            tokio::select! {
                () = tokio::time::sleep(every) => {}
                () = self.sweep_wake.notified() => {}
                () = stopped(&mut stop) => return,
            }
            if let Err(e) = self.sweep().await {
                tracing::warn!(error = %e, "cannot remove download files");
            }
        }
    }

    /// [`Downloads::sweep`].
    async fn sweep(&self) -> Result<(), StoreError> {
        let rows = self.store.downloads().await?;
        let now = self.store.now();
        let mut by_key: HashMap<&str, Vec<&DownloadRow>> = HashMap::new();
        for row in &rows {
            by_key.entry(&row.content_key).or_default().push(row);
        }
        let busy = |key: &str| self.state.lock().busy(key);
        let mut removed = false;
        // A file no download names: cancelled, or a key no longer asked.
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !entry.file_type().is_ok_and(|t| t.is_file()) {
                    continue;
                }
                let Some(key) = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .and_then(written_key)
                else {
                    continue;
                };
                let named = by_key.get(key).is_some_and(|rows| {
                    rows.iter().any(|r| {
                        matches!(&r.state, StoredState::Ready { file, .. } if Path::new(file) == path)
                    })
                });
                if !named && !busy(key) {
                    tracing::info!(file = %path.display(), "a download file no download wants is removed");
                    remove_file(&path);
                    removed = true;
                }
            }
        }
        // A file kept a day past its last read.
        let kept = KEPT_AFTER_READ.as_secs() as i64;
        for (key, rows) in &by_key {
            let Some(StoredState::Ready { file, .. }) = rows.first().map(|r| &r.state) else {
                continue;
            };
            let file = Path::new(file);
            let in_area = file.starts_with(&self.dir);
            let last = rows
                .iter()
                .map(|r| r.last_read_at.unwrap_or(r.created_at))
                .max()
                .unwrap_or(now);
            if !in_area || last + kept > now || busy(key) {
                continue;
            }
            tracing::info!(file = %file.display(), "a download file is removed a day after its last read");
            remove_file(file);
            removed = true;
            let gone = StoredState::Failed(
                "the file was removed a day after it was last read: prepare it again".into(),
            );
            self.store
                .set_download_state(Of::Content(key), &gone)
                .await?;
            tell(&self.store, &self.changes, key, DownloadState::of(&gone)).await;
        }
        if removed {
            self.room.notify_waiters();
        }
        Ok(())
    }
}

impl State {
    /// Whether file `key`, one the sweep would remove, is made or read
    /// now: kept. One kept for a read is looked at again when the read
    /// ends ([`State::read_ended`]).
    fn busy(&mut self, key: &str) -> bool {
        if self.readers.contains_key(key) {
            self.held.insert(key.to_owned());
            return true;
        }
        self.making.contains_key(key)
    }

    /// A read of file `key` ended: whether it was the last of a file the
    /// sweep kept for it, which it looks at again now.
    fn read_ended(&mut self, key: &str) -> bool {
        match self.readers.get_mut(key) {
            Some(n) if *n > 1 => {
                *n -= 1;
                false
            }
            _ => {
                self.readers.remove(key);
                self.held.remove(key)
            }
        }
    }

    /// Whether file `key` is still being written in `run`.
    fn writing(&self, key: &str, run: u32) -> bool {
        self.making
            .get(key)
            .is_some_and(|m| m.run == run && matches!(m.state, DownloadState::Writing { .. }))
    }
}

impl Making {
    /// A file to make from now, in `run`, until `cancel` says otherwise.
    fn new(cancel: watch::Sender<bool>, run: u32) -> Self {
        Self {
            state: DownloadState::Waiting,
            reserved: 0,
            cancel,
            told: None,
            again: false,
            run,
        }
    }

    /// Joined by a download asked for now: its state. One cancelled
    /// already is stopping, and is made again for it once stopped
    /// ([`Inner::make`]): it waits.
    fn join(&mut self) -> DownloadState {
        if *self.cancel.borrow() {
            self.again = true;
            return DownloadState::Waiting;
        }
        self.state.clone()
    }

    /// Whether a progress to `next` is worth telling: a percent more, or
    /// two seconds since the last.
    fn due(&mut self, next: &DownloadState) -> bool {
        let percent = match next {
            DownloadState::Converting { progress, .. } | DownloadState::Writing { progress } => {
                progress.map(|p| (p * 100.0) as u32)
            }
            _ => None,
        };
        let now = Instant::now();
        let due = match self.told {
            None => true,
            Some((at, told)) => told != percent || now.duration_since(at) >= Duration::from_secs(2),
        };
        if due {
            self.told = Some((now, percent));
        }
        due
    }
}

/// Tell every download of file `key` that it is in `state`.
async fn tell(
    store: &Store,
    changes: &broadcast::Sender<DownloadChange>,
    key: &str,
    state: DownloadState,
) {
    if let Some(rows) = rows_of(store, key).await {
        send(changes, rows, &state);
    }
}

/// The downloads of file `key`, to tell; `None`, said, when they cannot
/// be read.
async fn rows_of(store: &Store, key: &str) -> Option<Vec<DownloadRow>> {
    match store.downloads_of_content(key).await {
        Ok(rows) => Some(rows),
        Err(e) => {
            tracing::warn!(error = %e, "cannot tell a download's change");
            None
        }
    }
}

/// Tell each of `rows` that it is in `state`.
fn send(
    changes: &broadcast::Sender<DownloadChange>,
    rows: Vec<DownloadRow>,
    state: &DownloadState,
) {
    for row in rows {
        // Nobody listening is fine.
        let _ = changes.send(DownloadChange {
            id: row.id,
            principal: row.principal,
            state: state.clone(),
        });
    }
}

/// Whether the media file at `path` is as its `stamp` says: a file sent
/// as it is is the one its size and SHA-256 are of only then.
async fn unchanged(path: &Path, stamp: &str) -> bool {
    crate::files::stamp_text(path)
        .await
        .is_ok_and(|now| now == stamp)
}

/// The content key of a file the area wrote, from its name: 64 hex
/// digits and an extension it writes, a film's or a track's (PX-51). The
/// sweep removes no other file: a file of someone else's in a directory
/// chosen wrongly stays.
fn written_key(name: &str) -> Option<&str> {
    let (key, extension) = name.split_once('.')?;
    let key_like = key.len() == 64 && key.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    let written = Writes::all().any(|w| w.extension() == extension);
    (key_like && written).then_some(key)
}

/// `wait`, unless `cancelled` says to stop first.
async fn pause(wait: Duration, cancelled: &mut watch::Receiver<bool>) -> Result<(), Ended> {
    tokio::select! {
        () = tokio::time::sleep(wait) => Ok(()),
        () = stopped(cancelled) => Err(Ended::Stopped),
    }
}

/// The kind of device a download is written for: its container, and what
/// it plays as it is. A platform family, from `Hello.platform`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeviceClass {
    /// iPhone, iPad and Mac: MP4, text subtitles as `mov_text`.
    Apple,
    /// The Linux app and the SDK's command-line clients: Matroska.
    Linux,
}

impl DeviceClass {
    /// The class of a client on `platform`; `None` for one that has no
    /// downloads (tvOS, the web UI, a platform this build does not know).
    pub fn of(platform: Platform) -> Option<Self> {
        match platform {
            Platform::Ios | Platform::Macos => Some(Self::Apple),
            Platform::Linux | Platform::Cli => Some(Self::Linux),
            Platform::Tvos | Platform::Web | Platform::Unknown => None,
        }
    }

    /// The name in a copy's target, in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Apple => "apple",
            Self::Linux => "linux",
        }
    }

    /// The class `s` names, as [`DeviceClass::as_str`] spells it.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "apple" => Some(Self::Apple),
            "linux" => Some(Self::Linux),
            _ => None,
        }
    }

    /// The audio codecs a copy for this class keeps as they are; any
    /// other is converted. Apple's players take no DTS or TrueHD; the
    /// Linux app plays every codec the server names.
    pub fn keep_audio(self) -> BTreeSet<AudioCodec> {
        let apple = [
            AudioCodec::Aac,
            AudioCodec::Ac3,
            AudioCodec::Eac3,
            AudioCodec::Alac,
            AudioCodec::Flac,
            AudioCodec::Mp3,
        ];
        match self {
            Self::Apple => apple.into(),
            Self::Linux => apple
                .into_iter()
                .chain([
                    AudioCodec::Dts,
                    AudioCodec::TrueHd,
                    AudioCodec::Opus,
                    AudioCodec::Vorbis,
                    AudioCodec::Pcm,
                ])
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_iphone_and_a_mac_download_for_apple() {
        assert_eq!(
            [Platform::Ios, Platform::Macos].map(DeviceClass::of),
            [Some(DeviceClass::Apple); 2]
        );
    }

    #[test]
    fn the_linux_app_and_the_cli_download_for_linux() {
        assert_eq!(
            [Platform::Linux, Platform::Cli].map(DeviceClass::of),
            [Some(DeviceClass::Linux); 2]
        );
    }

    #[test]
    fn an_apple_tv_and_the_web_have_no_downloads() {
        assert_eq!(
            [Platform::Tvos, Platform::Web, Platform::Unknown].map(DeviceClass::of),
            [None; 3]
        );
    }

    #[test]
    fn a_class_s_name_reads_back_as_the_class() {
        for class in [DeviceClass::Apple, DeviceClass::Linux] {
            assert_eq!(DeviceClass::parse(class.as_str()), Some(class));
        }
    }

    #[test]
    fn a_progress_is_told_once_per_percent() {
        let (cancel, _) = watch::channel(false);
        let mut making = Making::new(cancel, 0);
        let at = |p| DownloadState::Writing { progress: Some(p) };
        let told = [0.101, 0.104, 0.111].map(|p| making.due(&at(p)));
        assert_eq!(told, [true, false, true]);
    }

    /// Finding 2 of the 0.14.0 review: a file cancelled while it was made
    /// and then asked for again is made again, not left waiting.
    #[test]
    fn a_cancelled_file_joined_again_is_made_again() {
        let (cancel, _) = watch::channel(false);
        let mut making = Making::new(cancel, 0);
        making.state = DownloadState::Writing {
            progress: Some(0.5),
        };
        making.cancel.send_replace(true);
        assert_eq!(
            (making.join(), making.again),
            (DownloadState::Waiting, true)
        );
    }

    #[test]
    fn a_file_still_wanted_joined_is_not_made_again() {
        let (cancel, _) = watch::channel(false);
        let mut making = Making::new(cancel, 0);
        assert_eq!(
            (making.join(), making.again),
            (DownloadState::Waiting, false)
        );
    }

    /// A read writes the last read only once it is ten minutes old.
    #[test]
    fn a_read_is_noted_when_the_last_is_ten_minutes_old() {
        let now = 1_000_000;
        assert_eq!(
            [None, Some(now - 599), Some(now - 600)].map(|last| note_read(last, now)),
            [true, false, true]
        );
    }

    /// Only the end of the last read of a file the sweep kept for it
    /// wakes the sweep: every other read's end leaves it to its timer.
    #[test]
    fn only_the_last_read_of_a_kept_file_wakes_the_sweep() {
        let mut state = State::default();
        state.readers.insert("kept".into(), 2);
        state.readers.insert("wanted".into(), 1);
        assert!(state.busy("kept"));
        assert_eq!(
            [
                state.read_ended("kept"),
                state.read_ended("wanted"),
                state.read_ended("kept"),
            ],
            [false, false, true]
        );
    }

    /// Finding 5: a progress sent once its file is ready (its making
    /// gone), or from a run that stopped, is dropped.
    #[test]
    fn a_progress_after_the_end_or_of_a_stopped_run_is_not_told() {
        let mut state = State::default();
        let (cancel, _) = watch::channel(false);
        let mut making = Making::new(cancel, 1);
        making.state = DownloadState::Writing { progress: None };
        state.making.insert("k".into(), making);
        let now = [state.writing("k", 1), state.writing("k", 0)];
        state.making.remove("k");
        assert_eq!((now, state.writing("k", 1)), ([true, false], false));
    }

    /// Finding 1: the sweep takes only names the area writes.
    #[test]
    fn only_a_written_name_is_the_area_s() {
        let key = "0123456789abcdef".repeat(4);
        let names = [
            format!("{key}.mkv"),
            format!("{key}.mp4"),
            "Alien (1979).mkv".to_owned(),
            format!("{}.mkv", "0123456789ABCDEF".repeat(4)),
            format!("{key}.nfo"),
            format!("{key}.mkv.part"),
            format!("{key}.flac"),
        ];
        assert_eq!(
            names.map(|n| written_key(&n).is_some()),
            [true, true, false, false, false, false, true]
        );
    }
}
