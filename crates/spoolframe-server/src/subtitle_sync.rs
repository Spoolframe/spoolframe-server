//! Subtitle sync and offset (PX-03): an external text subtitle (SRT,
//! WebVTT, ASS beside the film) retimed to the file, for everyone who
//! plays it.
//!
//! A track's timing is a row (`store::subtitle_timings`) and two files of
//! the server's own in `state_dir/subtitles`, named by the subtitle's
//! path: the synced subtitle, written once per sync, and the one served,
//! which is the synced one, or the original, with the manual offset on
//! top. The subtitle file itself is never written: it is someone else's,
//! in a library folder the server may not write to. The served file is
//! replaced whole (a new file renamed over it) at each change, so a
//! playback reading it gets `SubtitleChanged` and reads it again, and a
//! playback started before the track had a timing is served it too
//! ([`SubtitleTimings::resolve`]): neither restarts.
//!
//! A sync aligns the subtitle's cues ([`spoolframe_media::subsync`]) to a
//! text subtitle inside the file, in any language, when it has one, else
//! to the speech in its default audio track. Decoding the audio is most
//! of the time, so the speech found is kept per file and audio track, and
//! the next subtitle synced for the film starts at once. Syncs run in the
//! background, [`SyncSettings::at_once`] at a time (one by default), their
//! ffmpeg and alignment at idle priority. A sync that scores below
//! [`subsync::MIN_SCORE`] is not used: the track keeps its timing and
//! says `Uncertain`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use spoolframe_media::sandbox::Tool;
use spoolframe_media::subsync::{self, Retiming, Span, SyncError, TextFormat};
use spoolframe_media::{MediaDescriptor, SubtitleKind, SubtitleSource};
use spoolframe_types::{MediaId, SubtitleSync, SubtitleTiming};
use tokio::sync::{Semaphore, broadcast};

use crate::store::subtitle_timings::{SyncFindings, TimingRow};
use crate::store::{Store, StoreError};

/// The most a manual offset moves a subtitle, either way: ten minutes.
pub const MAX_OFFSET_MS: i64 = 10 * 60 * 1000;

/// `subtitles.sync_at_once` and where the files go, with the defaults
/// filled in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncSettings {
    /// Syncs that run at once; the rest wait their turn.
    pub at_once: usize,
    /// The synced and served subtitles: `state_dir/subtitles`.
    pub dir: PathBuf,
    /// The speech found in films' audio: `state_dir/cache/subtitle-speech`.
    pub speech_dir: PathBuf,
}

/// Why a timing request was refused.
#[derive(Debug, thiserror::Error)]
pub enum TimingError {
    #[error("the item has no subtitle track {0}")]
    NoSuchTrack(u32),
    /// Embedded, or pictures: only an external text subtitle is retimed.
    #[error("only a subtitle file beside the media in SRT, WebVTT or ASS can be synced or offset")]
    NotExternalText,
    #[error("an offset is at most {MAX_OFFSET_MS} ms either way")]
    OffsetTooLarge,
    /// The subtitle file cannot be read or the served file written.
    #[error("{0}")]
    Io(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Where the speech of a film's audio comes from: ffmpeg and the voice
/// detector, or a stand-in in tests.
#[async_trait::async_trait]
pub trait SpeechSource: Send + Sync {
    async fn speech(&self, media: &Path, audio_index: u32) -> Result<Vec<Span>, SyncError>;
}

/// The film's audio decoded by `ffmpeg`.
pub struct FfmpegSpeech(pub Tool);

#[async_trait::async_trait]
impl SpeechSource for FfmpegSpeech {
    async fn speech(&self, media: &Path, audio_index: u32) -> Result<Vec<Span>, SyncError> {
        subsync::speech(&self.0, media, audio_index).await
    }
}

/// A track's timing changed: what a client showing it is told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimingNews {
    pub id: MediaId,
    pub track: u32,
    pub timing: SubtitleTiming,
}

/// One subtitle track of an item, to retime.
#[derive(Debug, Clone)]
pub struct TrackRef {
    pub id: MediaId,
    /// The media file.
    pub media: PathBuf,
    pub descriptor: Arc<MediaDescriptor>,
    /// Its place in the item's subtitles, as the client knows it.
    pub index: u32,
}

impl TrackRef {
    /// The track's file and format: an external text subtitle, or why not.
    fn sidecar(&self) -> Result<(PathBuf, TextFormat), TimingError> {
        let stream = self
            .descriptor
            .subtitles
            .get(self.index as usize)
            .ok_or(TimingError::NoSuchTrack(self.index))?;
        match (&stream.source, stream.kind) {
            (SubtitleSource::External { path }, SubtitleKind::Text) => {
                let format =
                    TextFormat::of_codec(&stream.codec).ok_or(TimingError::NotExternalText)?;
                Ok((path.clone(), format))
            }
            _ => Err(TimingError::NotExternalText),
        }
    }
}

pub struct SubtitleTimings {
    store: Store,
    ffmpeg: Tool,
    speech: Arc<dyn SpeechSource>,
    dir: PathBuf,
    speech_dir: PathBuf,
    turns: Arc<Semaphore>,
    /// One change to a row at a time: each reads it, changes it, writes
    /// it whole, and writes the served file to match. Shared, so a
    /// change's writes hold it on their own task ([`SubtitleTimings::change`]).
    writing: Arc<tokio::sync::Mutex<()>>,
    /// Subtitle files whose sync is waiting or running; each entry is held
    /// by a [`Syncing`] claim, so a sync cancelled anywhere lets go of it.
    syncing: Arc<Mutex<HashSet<PathBuf>>>,
    news: broadcast::Sender<TimingNews>,
    /// Where a test holds each change, before its lock and its writes: a
    /// request's work to race its connection's close against (Y8-02).
    #[cfg(any(test, feature = "test-support"))]
    pub held: crate::test_holds::Held,
}

impl std::fmt::Debug for SubtitleTimings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubtitleTimings")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

/// What a sync came to.
enum Outcome {
    Synced {
        bytes: Vec<u8>,
        findings: SyncFindings,
    },
    Uncertain(SyncFindings),
    Failed(String),
}

impl SubtitleTimings {
    /// Syncing with `ffmpeg` and the speech `speech` finds, as
    /// `settings` say. Work a stopped server left half done is removed.
    pub fn new(
        store: Store,
        ffmpeg: Tool,
        speech: Arc<dyn SpeechSource>,
        settings: &SyncSettings,
    ) -> std::io::Result<Self> {
        crate::files::private_dir(&settings.dir)?;
        crate::files::private_dir(&settings.speech_dir)?;
        let work = settings.dir.join("work");
        if work.exists() {
            std::fs::remove_dir_all(&work)?;
        }
        Ok(Self {
            store,
            ffmpeg,
            speech,
            dir: settings.dir.clone(),
            speech_dir: settings.speech_dir.clone(),
            turns: Arc::new(Semaphore::new(settings.at_once.max(1))),
            writing: Arc::default(),
            syncing: Arc::default(),
            news: broadcast::channel(64).0,
            #[cfg(any(test, feature = "test-support"))]
            held: crate::test_holds::Held::default(),
        })
    }

    /// Where the served subtitles are: files the server's own.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Every timing change, as it is made.
    pub fn changes(&self) -> broadcast::Receiver<TimingNews> {
        self.news.subscribe()
    }

    /// The row of `sidecar` if it still holds: the file is as it was when
    /// the row was written.
    async fn current(&self, sidecar: &Path) -> Result<Option<TimingRow>, StoreError> {
        let Some(row) = self.store.subtitle_timing(&path_text(sidecar)).await? else {
            return Ok(None);
        };
        Ok(match crate::files::stamp_text(sidecar).await {
            Ok(now) if now == row.stamp => Some(row),
            _ => None,
        })
    }

    /// The file served for subtitle file `sidecar`, when it has a timing;
    /// `None` to serve it as it is.
    pub async fn serving(&self, sidecar: &Path) -> Option<PathBuf> {
        let row = self.current(sidecar).await.ok()??;
        let served = self.served_path(sidecar, &row.sidecar);
        tokio::fs::try_exists(&served)
            .await
            .unwrap_or(false)
            .then_some(served)
    }

    /// What to read for a playback's external subtitle `path`: its served
    /// file when it has a timing, whether the playback began before the
    /// timing or after (then `path` already is that file).
    pub async fn resolve(&self, path: &Path) -> PathBuf {
        // A served file is right in the directory; a downloaded subtitle
        // (PX-02) is in one under it, and is served as any other.
        if path.parent() == Some(self.dir.as_path()) {
            return path.to_owned();
        }
        self.serving(path).await.unwrap_or_else(|| path.to_owned())
    }

    /// Each subtitle track's timing, by its place in `descriptor`: `None`
    /// for one that is not an external text subtitle.
    pub async fn timings(&self, descriptor: &MediaDescriptor) -> Vec<Option<SubtitleTiming>> {
        let mut out = Vec::with_capacity(descriptor.subtitles.len());
        for stream in &descriptor.subtitles {
            let timing = match (&stream.source, stream.kind) {
                (SubtitleSource::External { path }, SubtitleKind::Text)
                    if TextFormat::of_codec(&stream.codec).is_some() =>
                {
                    Some(self.timing_of(path).await)
                }
                _ => None,
            };
            out.push(timing);
        }
        out
    }

    async fn timing_of(&self, sidecar: &Path) -> SubtitleTiming {
        let row = match self.current(sidecar).await {
            Ok(row) => row,
            Err(e) => {
                tracing::warn!(error = %e, "cannot read a subtitle's timing");
                None
            }
        };
        let mut timing = wire(row.as_ref());
        if self.syncing.lock().contains(sidecar) {
            timing.sync = SubtitleSync::Syncing;
        }
        timing
    }

    /// Sync `track` in the background: its timing now, `Syncing`. A track
    /// already syncing is left to it. The claim on the file is a guard,
    /// carried into the worker: a request cancelled before the worker is
    /// spawned (aborted with its connection, or past its wait) leaves no
    /// file claimed for ever.
    pub async fn sync(self: &Arc<Self>, track: TrackRef) -> Result<SubtitleTiming, TimingError> {
        let (sidecar, format) = track.sidecar()?;
        let stamp = crate::files::stamp_text(&sidecar)
            .await
            .map_err(|e| TimingError::Io(format!("the subtitle file cannot be read: {e}")))?;
        let Some(claim) = Syncing::claim(&self.syncing, &sidecar) else {
            return Ok(self.timing_of(&sidecar).await);
        };
        self.change(&track, &sidecar, |row| {
            row.state = "syncing".into();
            row.failure = None;
            Ok(())
        })
        .await?;
        let timing = self.timing_of(&sidecar).await;
        self.tell(&track, timing.clone());
        let this = self.clone();
        tokio::spawn(async move {
            let outcome = {
                let _turn = this
                    .turns
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("never closed");
                this.align(&track, &sidecar, format).await
            };
            this.finish(&track, &sidecar, &stamp, outcome, claim).await;
        });
        Ok(timing)
    }

    /// Serve `track` with its original timing again, for everyone: the
    /// sync is set aside; the offset stays.
    pub async fn use_original(&self, track: TrackRef) -> Result<SubtitleTiming, TimingError> {
        let (sidecar, _) = track.sidecar()?;
        self.change(&track, &sidecar, |row| {
            if row.state != "syncing" {
                row.state = "original".into();
                row.sync = None;
                row.failure = None;
            }
            row.serving_synced = false;
            Ok(())
        })
        .await?;
        let timing = self.timing_of(&sidecar).await;
        self.tell(&track, timing.clone());
        Ok(timing)
    }

    /// Move `track` by `offset_ms` on top of its timing, for everyone, as
    /// set by `by` (a principal), whose name is `name`. 0 resets it.
    pub async fn set_offset(
        &self,
        track: TrackRef,
        offset_ms: i64,
        by: &str,
        name: &str,
    ) -> Result<SubtitleTiming, TimingError> {
        if offset_ms.abs() > MAX_OFFSET_MS {
            return Err(TimingError::OffsetTooLarge);
        }
        let (sidecar, _) = track.sidecar()?;
        let now = self.store.now();
        self.change(&track, &sidecar, |row| {
            row.offset_ms = offset_ms;
            row.offset_by = Some(by.to_owned());
            row.offset_name = Some(name.to_owned());
            row.offset_at = Some(now);
            Ok(())
        })
        .await?;
        let timing = self.timing_of(&sidecar).await;
        self.tell(&track, timing.clone());
        Ok(timing)
    }

    /// Forget subtitle file `sidecar`'s timing and its files: it is gone
    /// (PX-02, a download removed). A sync of it still running fails.
    pub async fn forget(&self, sidecar: &Path) -> Result<(), TimingError> {
        let _writing = self.writing.lock().await;
        let key = path_text(sidecar);
        for file in [
            self.served_path(sidecar, &key),
            self.synced_path(sidecar, &key),
        ] {
            match tokio::fs::remove_file(&file).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(TimingError::Io(format!(
                        "cannot remove {}: {e}",
                        file.display()
                    )));
                }
            }
        }
        self.store.delete_subtitle_timing(&key).await?;
        Ok(())
    }

    fn tell(&self, track: &TrackRef, timing: SubtitleTiming) {
        // None listening is not a failure.
        let _ = self.news.send(TimingNews {
            id: track.id.clone(),
            track: track.index,
            timing,
        });
    }

    /// `sidecar`'s row changed by `edit` (a new one if it has none, or if
    /// the file changed since), written whole, and its served file
    /// written to match. The two writes run on a task of their own, with
    /// the lock, so a request aborted with its connection (Y8-02) leaves
    /// the served file and the row as a pair, not the file changed and the
    /// row as it was.
    async fn change(
        &self,
        track: &TrackRef,
        sidecar: &Path,
        edit: impl FnOnce(&mut TimingRow) -> Result<(), TimingError>,
    ) -> Result<(), TimingError> {
        #[cfg(any(test, feature = "test-support"))]
        self.held.wait().await;
        let writing = self.writing.clone().lock_owned().await;
        let stamp = crate::files::stamp_text(sidecar)
            .await
            .map_err(|e| TimingError::Io(format!("the subtitle file cannot be read: {e}")))?;
        let mut row = self
            .current(sidecar)
            .await?
            .unwrap_or_else(|| TimingRow::new(&path_text(sidecar), &track.id.to_string(), &stamp));
        edit(&mut row)?;
        row.media_id = track.id.to_string();
        let (store, dir, sidecar) = (self.store.clone(), self.dir.clone(), sidecar.to_owned());
        tokio::spawn(async move {
            let _writing = writing;
            write_served(&dir, &sidecar, &row).await?;
            store.put_subtitle_timing(&row).await?;
            Ok(())
        })
        .await
        .map_err(|e| TimingError::Io(format!("the timing change was not written: {e}")))?
    }

    fn served_path(&self, sidecar: &Path, key: &str) -> PathBuf {
        served_path(&self.dir, sidecar, key)
    }

    fn synced_path(&self, sidecar: &Path, key: &str) -> PathBuf {
        synced_path(&self.dir, sidecar, key)
    }

    /// The sync's work, in its turn: the subtitle's cues aligned to the
    /// file's reference, and its new text when the alignment holds.
    async fn align(&self, track: &TrackRef, sidecar: &Path, format: TextFormat) -> Outcome {
        let started = std::time::Instant::now();
        let bytes = match read_subtitle(sidecar).await {
            Ok(bytes) => bytes,
            Err(e) => return Outcome::Failed(e.to_string()),
        };
        let cues = match subsync::cues(&bytes, format) {
            Ok(cues) => cues,
            Err(e) => return Outcome::Failed(e.to_string()),
        };
        let (reference, kind) = match self.reference(track, sidecar).await {
            Ok(found) => found,
            Err(e) => return Outcome::Failed(e.to_string()),
        };
        let alignment = match subsync::align_at_idle(reference, subsync::spans(&cues)).await {
            Ok(alignment) => alignment,
            Err(e) => return Outcome::Failed(e.to_string()),
        };
        tracing::info!(
            id = %track.id,
            subtitle = %sidecar.display(),
            reference = kind,
            offset_ms = alignment.offset_ms,
            factor = alignment.factor,
            splits = alignment.splits,
            score = alignment.score,
            secs = started.elapsed().as_secs_f32(),
            "synced a subtitle"
        );
        let findings = SyncFindings {
            offset_ms: alignment.offset_ms,
            factor: alignment.factor,
            splits: alignment.splits,
            score: alignment.score,
            reference: kind.to_owned(),
        };
        if !alignment.certain() {
            return Outcome::Uncertain(findings);
        }
        let retiming = Retiming {
            factor: alignment.factor,
            deltas: alignment.deltas,
        };
        Outcome::Synced {
            bytes: subsync::retime(&bytes, format, &cues, &retiming),
            findings,
        }
    }

    /// What `track` is aligned to, and which kind it is: a text subtitle
    /// inside the file (not a forced one, which has a few lines only),
    /// else the speech in its default audio.
    async fn reference(
        &self,
        track: &TrackRef,
        sidecar: &Path,
    ) -> Result<(Vec<Span>, &'static str), SyncError> {
        let embedded = track
            .descriptor
            .subtitles
            .iter()
            .find_map(|s| match s.source {
                SubtitleSource::Embedded { index } if s.kind == SubtitleKind::Text && !s.forced => {
                    Some(index)
                }
                _ => None,
            });
        if let Some(index) = embedded {
            let work = self.dir.join("work").join(name(&path_text(sidecar)));
            tokio::fs::create_dir_all(&work)
                .await
                .map_err(|e| SyncError::Failed(e.to_string()))?;
            let found = subsync::subtitle_reference(&self.ffmpeg, &track.media, index, &work).await;
            let _ = tokio::fs::remove_dir_all(&work).await;
            match found {
                Ok(spans) => return Ok((spans, "subtitle")),
                // A track ffmpeg cannot read out: the audio still can be.
                Err(e) => {
                    tracing::warn!(id = %track.id, error = %e, "cannot read an embedded subtitle to sync to; using the speech");
                }
            }
        }
        let audio = track
            .descriptor
            .audio
            .iter()
            .find(|a| a.default)
            .or_else(|| track.descriptor.audio.first())
            .ok_or(SyncError::NoReference)?;
        Ok((self.speech_of(&track.media, audio.index).await?, "speech"))
    }

    /// The speech of audio stream `index` of `media`: kept from an earlier
    /// sync of the same file as it is now, else found and kept.
    async fn speech_of(&self, media: &Path, index: u32) -> Result<Vec<Span>, SyncError> {
        let (size, modified) = crate::files::stamp(media)
            .await
            .map_err(|e| SyncError::Tool(format!("the media file cannot be read: {e}")))?;
        let key = crate::files::hash_parts(&[
            media.display().to_string(),
            size.to_string(),
            modified.to_string(),
            index.to_string(),
        ])
        .to_hex()
        .to_string();
        let kept = self.speech_dir.join(format!("{key}.json"));
        if let Ok(json) = tokio::fs::read(&kept).await
            && let Ok(spans) = serde_json::from_slice::<Vec<Span>>(&json)
        {
            return Ok(spans);
        }
        let spans = self.speech.speech(media, index).await?;
        let json = serde_json::to_vec(&spans).expect("spans always serialize");
        if let Err(e) = write_into_place(&kept, &json).await {
            tracing::warn!(error = %e, "cannot keep a film's speech");
        }
        Ok(spans)
    }

    /// The sync's outcome written, unless the subtitle file changed while
    /// it ran (its row then no longer holds, and the outcome is of a file
    /// that is gone).
    async fn finish(
        &self,
        track: &TrackRef,
        sidecar: &Path,
        began: &str,
        outcome: Outcome,
        claim: Syncing,
    ) {
        let changed = crate::files::stamp_text(sidecar).await.ok().as_deref() != Some(began);
        let written = if changed {
            Err(TimingError::Io(
                "the subtitle file changed during the sync".into(),
            ))
        } else {
            match &outcome {
                Outcome::Synced { bytes, .. } => {
                    let synced = self.synced_path(sidecar, &path_text(sidecar));
                    write_into_place(&synced, bytes).await
                }
                _ => Ok(()),
            }
        };
        let result = self
            .change(track, sidecar, |row| {
                match (written, outcome) {
                    (Err(e), _) => {
                        row.state = "failed".into();
                        row.failure = Some(e.to_string());
                    }
                    (Ok(()), Outcome::Synced { findings, .. }) => {
                        row.state = "synced".into();
                        row.serving_synced = true;
                        row.sync = Some(findings);
                    }
                    (Ok(()), Outcome::Uncertain(findings)) => {
                        // A wrong sync is worse than none: the timing
                        // served before stays.
                        row.state = "uncertain".into();
                        row.serving_synced = false;
                        row.sync = Some(findings);
                    }
                    (Ok(()), Outcome::Failed(reason)) => {
                        row.state = "failed".into();
                        row.failure = Some(reason);
                    }
                }
                Ok(())
            })
            .await;
        drop(claim);
        if let Err(e) = result {
            tracing::warn!(subtitle = %sidecar.display(), error = %e, "cannot record a subtitle's sync");
        }
        let timing = self.timing_of(sidecar).await;
        self.tell(track, timing);
    }
}

/// A claim on a subtitle file among those syncing, let go of when dropped.
struct Syncing {
    syncing: Arc<Mutex<HashSet<PathBuf>>>,
    sidecar: PathBuf,
}

impl Syncing {
    /// The claim on `sidecar`, if no sync holds one.
    fn claim(syncing: &Arc<Mutex<HashSet<PathBuf>>>, sidecar: &Path) -> Option<Self> {
        syncing.lock().insert(sidecar.to_owned()).then(|| Self {
            syncing: syncing.clone(),
            sidecar: sidecar.to_owned(),
        })
    }
}

impl Drop for Syncing {
    fn drop(&mut self) {
        self.syncing.lock().remove(&self.sidecar);
    }
}

/// A row's timing as the wire says it; no row is the original timing.
fn wire(row: Option<&TimingRow>) -> SubtitleTiming {
    let Some(row) = row else {
        return SubtitleTiming {
            sync: SubtitleSync::Original,
            offset_ms: 0,
            offset_by: None,
            offset_at: None,
        };
    };
    let score = |s: &SyncFindings| (s.score * 100.0).round().clamp(0.0, 100.0) as u8;
    let sync = match (row.state.as_str(), &row.sync) {
        ("syncing", _) => SubtitleSync::Syncing,
        ("synced", Some(s)) => SubtitleSync::Synced {
            offset_ms: s.offset_ms,
            factor_ppm: (s.factor * 1e6).round() as u32,
            splits: s.splits,
            score: score(s),
        },
        ("uncertain", Some(s)) => SubtitleSync::Uncertain { score: score(s) },
        ("failed", _) => SubtitleSync::Failed {
            reason: row.failure.clone().unwrap_or_default(),
        },
        _ => SubtitleSync::Original,
    };
    SubtitleTiming {
        sync,
        offset_ms: row.offset_ms,
        offset_by: row.offset_name.clone(),
        offset_at: row.offset_at,
    }
}

fn path_text(path: &Path) -> String {
    path.display().to_string()
}

/// A served file's name: the subtitle's path, hashed.
fn name(key: &str) -> String {
    crate::files::hash_parts(&[key.to_owned()]).to_hex()[..32].to_owned()
}

/// The subtitle's own extension, so a client or ffmpeg reading the
/// served file takes it for what it is.
fn extension(sidecar: &Path) -> String {
    sidecar
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| e.len() <= 4 && e.bytes().all(|b| b.is_ascii_alphanumeric()))
        .unwrap_or("srt")
        .to_ascii_lowercase()
}

fn format_of(sidecar: &Path) -> Option<TextFormat> {
    match extension(sidecar).as_str() {
        "srt" => Some(TextFormat::Srt),
        "vtt" => Some(TextFormat::WebVtt),
        "ass" | "ssa" => Some(TextFormat::Ass),
        _ => None,
    }
}

fn served_path(dir: &Path, sidecar: &Path, key: &str) -> PathBuf {
    dir.join(format!("{}.{}", name(key), extension(sidecar)))
}

fn synced_path(dir: &Path, sidecar: &Path, key: &str) -> PathBuf {
    dir.join(format!("{}.synced.{}", name(key), extension(sidecar)))
}

/// The file served for `row`, in `dir`: its synced file or `sidecar`,
/// moved by its offset; written aside and renamed in, so a reader has the
/// old one or the new one whole.
async fn write_served(dir: &Path, sidecar: &Path, row: &TimingRow) -> Result<(), TimingError> {
    let source = if row.serving_synced {
        synced_path(dir, sidecar, &row.sidecar)
    } else {
        sidecar.to_owned()
    };
    let bytes = read_subtitle(&source).await?;
    let bytes = if row.offset_ms == 0 {
        bytes
    } else {
        let format = format_of(sidecar).ok_or(TimingError::NotExternalText)?;
        subsync::shift(&bytes, format, row.offset_ms)
            .map_err(|e| TimingError::Io(format!("the subtitle cannot be offset: {e}")))?
    };
    write_into_place(&served_path(dir, sidecar, &row.sidecar), &bytes).await
}

/// A subtitle file, never through a symlink, at most
/// [`subsync::MAX_TEXT_BYTES`].
async fn read_subtitle(path: &Path) -> Result<Vec<u8>, TimingError> {
    use tokio::io::AsyncReadExt as _;
    let unreadable =
        |e: std::io::Error| TimingError::Io(format!("the subtitle cannot be read: {e}"));
    let file = crate::files::open_regular(path).await.map_err(unreadable)?;
    let mut bytes = Vec::new();
    file.take(subsync::MAX_TEXT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(unreadable)?;
    if bytes.len() > subsync::MAX_TEXT_BYTES {
        return Err(TimingError::Io(format!(
            "the subtitle is larger than {} bytes",
            subsync::MAX_TEXT_BYTES
        )));
    }
    Ok(bytes)
}

/// `bytes` at `path`, written aside and renamed over it.
async fn write_into_place(path: &Path, bytes: &[u8]) -> Result<(), TimingError> {
    let failed =
        |e: std::io::Error| TimingError::Io(format!("cannot write {}: {e}", path.display()));
    let partial = path.with_extension("partial");
    crate::files::write_synced(&partial, bytes)
        .await
        .map_err(failed)?;
    tokio::fs::rename(&partial, path).await.map_err(failed)
}

#[cfg(test)]
mod tests;
