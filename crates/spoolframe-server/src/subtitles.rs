//! Subtitles as a text file a client fetches whole (P2-07), made once per
//! track: a bitmap track (PGS; VobSub and DVB since P3-08) read as text
//! (OCR), or a text track converted to another text format (ASS or
//! `mov_text` to WebVTT, say).
//!
//! The first playback that needs a track as text starts making it; the
//! text is kept in the cache, so every later playback of that track gets it
//! at once. Playbacks of the same track meanwhile share the one job. It
//! runs to the end even if the playback that started it ends: the next one
//! would start it again.
//!
//! One bitmap track is read at a time, with `threads` tesseract processes:
//! a second track waits, rather than both taking twice as long. A text
//! conversion is only ffmpeg reading the file, and waits for nothing.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use spoolframe_media::ocr::{self, Ocr};
use spoolframe_media::sandbox::Tool;
use spoolframe_media::{SubtitleKind, SubtitleSource, SubtitleStream, codecs, dvb, pgs, vobsub};
use spoolframe_types::SubtitleFormat;

use crate::metrics::Counter;
use tokio::sync::{Semaphore, watch};

/// Text conversions (ffmpeg reading a track out of a file) at once
/// (PX-09 finding 15).
const MAX_CONVERSIONS: usize = 4;

/// Tracks being made or waiting their turn, of every kind, at once
/// (PX-09 finding 15).
const MAX_MAKING: usize = 64;

/// `subtitles { … }`, with the defaults filled in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OcrSettings {
    pub enabled: bool,
    pub threads: usize,
    pub cache_dir: PathBuf,
}

/// Where making a track's text has got to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextState {
    /// Behind another track.
    Waiting,
    Reading {
        done_percent: u8,
    },
    /// The text, in the cache.
    Ready(PathBuf),
    Failed(String),
}

impl TextState {
    pub fn finished(&self) -> bool {
        matches!(self, Self::Ready(_) | Self::Failed(_))
    }
}

/// Where a track's text is, looked at without making it (P3-15).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextPeek {
    NotMade,
    Making(TextState),
    Ready,
    /// The server cannot make it, and why.
    Unavailable(String),
}

/// A track to make text of: stream `stream` of the file at `path`, as `to`.
pub struct TextSource<'a> {
    pub path: &'a Path,
    pub stream: &'a SubtitleStream,
    pub to: SubtitleFormat,
}

pub struct Subtitles {
    ocr: Ocr,
    ffmpeg: Tool,
    ffprobe: Tool,
    threads: usize,
    cache_dir: PathBuf,
    making: Mutex<HashMap<String, watch::Receiver<TextState>>>,
    one_track: Arc<Semaphore>,
    /// [`MAX_CONVERSIONS`] places for text conversions.
    conversions: Arc<Semaphore>,
    /// Bitmap tracks waiting to be read or being read (P3-14).
    ocr_pending: std::sync::atomic::AtomicU64,
    ocr_done: Counter,
    ocr_failed: Counter,
}

/// Text recognition's work now, and since the server started (P3-14).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OcrCounts {
    /// Tracks waiting their turn or being read: one is read at a time.
    pub queued: u64,
    pub done: u64,
    pub failed: u64,
}

impl std::fmt::Debug for Subtitles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subtitles")
            .field("languages", self.ocr.languages())
            .field("cache_dir", &self.cache_dir)
            .finish_non_exhaustive()
    }
}

impl Subtitles {
    /// Reading with `ocr` (or not at all, if `settings` turn it off), and
    /// tracks with ffmpeg and ffprobe. Work a stopped server left half done
    /// is removed.
    pub fn new(
        ocr: Ocr,
        ffmpeg: Tool,
        ffprobe: Tool,
        settings: &OcrSettings,
    ) -> std::io::Result<Self> {
        crate::files::private_dir(&settings.cache_dir)?;
        let work = settings.cache_dir.join("work");
        if work.exists() {
            std::fs::remove_dir_all(&work)?;
        }
        Ok(Self {
            ocr: if settings.enabled {
                ocr
            } else {
                Ocr::disabled()
            },
            ffmpeg,
            ffprobe,
            threads: settings.threads,
            cache_dir: settings.cache_dir.clone(),
            making: Mutex::new(HashMap::new()),
            one_track: Arc::new(Semaphore::new(1)),
            conversions: Arc::new(Semaphore::new(MAX_CONVERSIONS)),
            ocr_pending: Default::default(),
            ocr_done: Counter::default(),
            ocr_failed: Counter::default(),
        })
    }

    /// Text recognition's work (P3-14).
    pub fn ocr_counts(&self) -> OcrCounts {
        use std::sync::atomic::Ordering;
        OcrCounts {
            queued: self.ocr_pending.load(Ordering::Relaxed),
            done: self.ocr_done.get(),
            failed: self.ocr_failed.get(),
        }
    }

    /// What the planner may read as text.
    pub fn ocr(&self) -> &Ocr {
        &self.ocr
    }

    /// Where the text of `source` is, as playback debug shows it (P3-15):
    /// unlike [`Subtitles::text`], looking starts nothing.
    pub async fn peek(&self, source: TextSource<'_>) -> TextPeek {
        let how = match source.stream.kind {
            SubtitleKind::Bitmap => {
                if self.ocr.languages().is_empty() {
                    return TextPeek::Unavailable(
                        "the server has no text recognition (tesseract)".into(),
                    );
                }
                let Some(language) = self.ocr.language_for(source.stream.language.as_deref())
                else {
                    return TextPeek::Unavailable(format!(
                        "the server has no text recognition data for {}",
                        source.stream.language.as_deref().unwrap_or("English")
                    ));
                };
                How::Ocr { language }
            }
            SubtitleKind::Text => How::Convert,
        };
        let Ok(key) = key(&source, &how).await else {
            return TextPeek::Unavailable("the media file could not be read".into());
        };
        let text = self
            .cache_dir
            .join(format!("{key}.{}", extension(source.to)));
        if tokio::fs::try_exists(&text).await.unwrap_or(false) {
            return TextPeek::Ready;
        }
        match self.making.lock().get(&key) {
            Some(state) => TextPeek::Making(state.borrow().clone()),
            None => TextPeek::NotMade,
        }
    }

    /// The text of `source`: ready at once from the cache, or being made.
    pub async fn text(self: &Arc<Self>, source: TextSource<'_>) -> watch::Receiver<TextState> {
        let failed = |reason: String| watch::channel(TextState::Failed(reason)).1;
        let how = match source.stream.kind {
            SubtitleKind::Bitmap => {
                let Some(language) = self.ocr.language_for(source.stream.language.as_deref())
                else {
                    return failed("no text recognition for this language".into());
                };
                How::Ocr { language }
            }
            SubtitleKind::Text => How::Convert,
        };
        let key = match key(&source, &how).await {
            Ok(key) => key,
            Err(e) => {
                tracing::warn!(path = %source.path.display(), error = %e, "cannot read a subtitle's file");
                return failed("the media file could not be read".into());
            }
        };
        let text = self
            .cache_dir
            .join(format!("{key}.{}", extension(source.to)));
        if tokio::fs::try_exists(&text).await.unwrap_or(false) {
            return watch::channel(TextState::Ready(text)).1;
        }
        let mut making = self.making.lock();
        if let Some(state) = making.get(&key) {
            return state.clone();
        }
        // Each waits as a task (PX-09 finding 15): a consumer asking for
        // every track of a library is told to try later past this many.
        if making.len() >= MAX_MAKING {
            drop(making);
            tracing::warn!(
                tracks = MAX_MAKING,
                "too many subtitles being made; one more refused for now"
            );
            return failed("the server is making too many subtitles; try again later".into());
        }
        let (tx, rx) = watch::channel(TextState::Waiting);
        making.insert(key.clone(), rx.clone());
        drop(making);

        let job = Job {
            subtitles: self.clone(),
            input: source.path.to_owned(),
            track: source.stream.source.clone(),
            format: codecs::subtitle_format(&source.stream.codec),
            how,
            to: source.to,
            work: self.cache_dir.join("work").join(&key),
            text,
        };
        let ocr = matches!(job.how, How::Ocr { .. });
        if ocr {
            self.ocr_pending
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        tokio::spawn(async move {
            let state = match job.run(&tx).await {
                Ok(path) => TextState::Ready(path),
                Err(reason) => {
                    tracing::warn!(input = %job.input.display(), reason, "cannot make a subtitle's text");
                    TextState::Failed(reason)
                }
            };
            if ocr {
                let subtitles = &job.subtitles;
                match &state {
                    TextState::Ready(_) => subtitles.ocr_done.inc(),
                    _ => subtitles.ocr_failed.inc(),
                }
                subtitles
                    .ocr_pending
                    .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            }
            let _ = tokio::fs::remove_dir_all(&job.work).await;
            let _ = tx.send(state);
            job.subtitles.making.lock().remove(&key);
        });
        rx
    }
}

/// How a track becomes text.
#[derive(Debug, Clone, PartialEq, Eq)]
enum How {
    /// Pictures read with tesseract, in `language`.
    Ocr { language: String },
    /// Text in another format: ffmpeg converts it.
    Convert,
}

struct Job {
    subtitles: Arc<Subtitles>,
    input: PathBuf,
    track: SubtitleSource,
    format: SubtitleFormat,
    how: How,
    to: SubtitleFormat,
    work: PathBuf,
    text: PathBuf,
}

impl Job {
    async fn run(&self, state: &watch::Sender<TextState>) -> Result<PathBuf, String> {
        tokio::fs::create_dir_all(&self.work)
            .await
            .map_err(|e| e.to_string())?;
        let partial = self.work.join("text");
        match &self.how {
            How::Ocr { language } => self.read(state, language, &partial).await?,
            How::Convert => {
                let _turn = self
                    .subtitles
                    .conversions
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("never closed");
                let _ = state.send(TextState::Reading { done_percent: 0 });
                // A sidecar is its own input, its one stream the track.
                let (input, index) = match &self.track {
                    SubtitleSource::Embedded { index } => (&self.input, *index),
                    SubtitleSource::External { path } => {
                        crate::files::is_regular(path)
                            .await
                            .map_err(|e| e.to_string())?;
                        (path, 0)
                    }
                };
                ocr::convert_text(&self.subtitles.ffmpeg, input, index, self.to, &partial)
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }
        // Written aside, then moved in: a half-written file in the cache
        // would be served as the whole text.
        tokio::fs::rename(&partial, &self.text)
            .await
            .map_err(|e| e.to_string())?;
        Ok(self.text.clone())
    }

    /// OCR of a bitmap track into `out`.
    async fn read(
        &self,
        state: &watch::Sender<TextState>,
        language: &str,
        out: &Path,
    ) -> Result<(), String> {
        let _turn = self
            .subtitles
            .one_track
            .clone()
            .acquire_owned()
            .await
            .expect("never closed");
        let _ = state.send(TextState::Reading { done_percent: 0 });
        let started = std::time::Instant::now();
        let cues = self.cues().await?;
        let total = cues.len().max(1);
        let texts = self
            .subtitles
            .ocr
            .recognise(
                &cues,
                language,
                &self.work.join("pictures"),
                self.subtitles.threads,
                &|done| {
                    let _ = state.send(TextState::Reading {
                        done_percent: (done * 100 / total) as u8,
                    });
                },
            )
            .await
            .map_err(|e| e.to_string())?;
        let cues = ocr::text_cues(&cues, texts);
        let text = match self.to {
            SubtitleFormat::WebVtt => ocr::webvtt(&cues),
            _ => ocr::srt(&cues),
        };
        tokio::fs::write(out, text)
            .await
            .map_err(|e| e.to_string())?;
        tracing::info!(
            input = %self.input.display(),
            cues = cues.len(),
            language,
            secs = started.elapsed().as_secs_f32(),
            "read a subtitle as text"
        );
        Ok(())
    }

    /// The track's cues. PGS is copied out as a `.sup` (a file beside the
    /// film already is one); VobSub and DVB are read as packets, a `.idx`
    /// beside the film with its `.sub` as its first stream.
    async fn cues(&self) -> Result<Vec<spoolframe_media::bitmap::Cue>, String> {
        if let SubtitleFormat::VobSub | SubtitleFormat::DvbSub = self.format {
            // An embedded track counts from the file's start; a file of its
            // own already does.
            let (input, index, from_start) = match &self.track {
                SubtitleSource::Embedded { index } => (&self.input, *index, true),
                SubtitleSource::External { path } => {
                    crate::files::is_regular(path)
                        .await
                        .map_err(|e| e.to_string())?;
                    (path, 0, false)
                }
            };
            let track = ocr::read_track(&self.subtitles.ffprobe, input, index, from_start)
                .await
                .map_err(|e| e.to_string())?;
            return match self.format {
                SubtitleFormat::VobSub => {
                    vobsub::cues(&track.extradata, &track.packets).map_err(|e| e.to_string())
                }
                _ => Ok(dvb::cues(&track.packets)),
            };
        }
        let sup = match &self.track {
            SubtitleSource::Embedded { index } => {
                let sup = self.work.join("track.sup");
                ocr::extract_sup(&self.subtitles.ffmpeg, &self.input, *index, &sup)
                    .await
                    .map_err(|e| e.to_string())?;
                sup
            }
            SubtitleSource::External { path } => path.clone(),
        };
        // Not through a symlink (PX-09 B-1): a file beside the film is
        // anyone's who can write to the library folder.
        let file = crate::files::open_regular(&sup)
            .await
            .map_err(|e| e.to_string())?;
        // Read to a cap (PX-09 B-3): the file is whatever is beside the
        // film, or what ffmpeg copied out of it.
        let limit = spoolframe_media::ocr::MAX_SUP_BYTES;
        let mut bytes = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(
            &mut tokio::io::AsyncReadExt::take(file, limit as u64 + 1),
            &mut bytes,
        )
        .await
        .map_err(|e| e.to_string())?;
        if bytes.len() > limit {
            return Err(format!("the PGS track is larger than {limit} bytes"));
        }
        pgs::cues(&bytes).map_err(|e| e.to_string())
    }
}

/// The track's name in the cache: the file as it is now (path, size,
/// modification time), the track, how it is made (the OCR language) and
/// the format. A file replaced by a new rip is made again; so is a
/// subtitle file beside it that was edited, which is named as it is now
/// too.
async fn key(source: &TextSource<'_>, how: &How) -> std::io::Result<String> {
    let (size, modified) = stamp(source.path).await?;
    let track = match &source.stream.source {
        SubtitleSource::Embedded { index } => format!("stream {index}"),
        SubtitleSource::External { path } => {
            let (size, modified) = stamp(path).await?;
            format!("file {} {size} {modified}", path.display())
        }
    };
    let mut hash = blake3::Hasher::new();
    for part in [
        source.path.display().to_string(),
        size.to_string(),
        modified.to_string(),
        track,
        match how {
            How::Ocr { language } => format!("ocr {language}"),
            How::Convert => "convert".to_owned(),
        },
        extension(source.to).to_owned(),
    ] {
        hash.update(part.as_bytes());
        hash.update(&[0]);
    }
    Ok(hash.finalize().to_hex().to_string())
}

/// A file's size and modification time (nanoseconds).
async fn stamp(path: &Path) -> std::io::Result<(u64, u128)> {
    let meta = tokio::fs::metadata(path).await?;
    let modified = meta
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    Ok((meta.len(), modified))
}

fn extension(format: SubtitleFormat) -> &'static str {
    match format {
        SubtitleFormat::WebVtt => "vtt",
        _ => "srt",
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use spoolframe_media::sandbox::Tool;
    use spoolframe_types::SubtitleRole;

    use super::*;

    /// A stand-in ffmpeg, in `bin`, that marks itself running in `dir`
    /// for `secs`, then writes its output (its last argument).
    fn slow_ffmpeg(bin: &Path, dir: &Path, secs: &str) -> PathBuf {
        let script = bin.join("ffmpeg");
        crate::test_support::write_script(
            &script,
            &format!(
                "#!/bin/sh\nfor out; do :; done\nmark='{}'/running.$$\n: > \"$mark\"\nsleep {secs}\n\
                 printf 'WEBVTT\\n\\n' > \"$out\"\nrm \"$mark\"\n",
                dir.display()
            ),
        );
        script
    }

    /// Subtitles converted by `slow_ffmpeg(secs)`, a film, and where the
    /// stand-in marks itself running.
    async fn converting(
        test: &str,
        secs: &str,
    ) -> (
        spoolframe_test_dir::TestDir,
        Arc<Subtitles>,
        PathBuf,
        PathBuf,
    ) {
        let tmp = spoolframe_test_dir::TestDir::new(&format!("spoolframe-subtitles-{test}")).await;
        let dir = tmp.as_ref().to_path_buf();
        let (bin, marks) = (dir.join("bin"), dir.join("marks"));
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&marks).unwrap();
        let subtitles = Arc::new(
            Subtitles::new(
                Ocr::disabled(),
                Tool::unconfined(slow_ffmpeg(&bin, &marks, secs)),
                Tool::unconfined("ffprobe"),
                &OcrSettings {
                    enabled: false,
                    threads: 1,
                    cache_dir: dir.join("cache"),
                },
            )
            .unwrap(),
        );
        let film = dir.join("Film.mkv");
        std::fs::write(&film, b"a film").unwrap();
        (tmp, subtitles, film, marks)
    }

    /// The text of the `n`th subtitle file beside `film`, asked for.
    async fn ask(subtitles: &Arc<Subtitles>, film: &Path, n: usize) -> watch::Receiver<TextState> {
        let srt = film.with_file_name(format!("Film.{n}.srt"));
        std::fs::write(&srt, format!("{n}")).unwrap();
        let stream = sidecar(srt);
        subtitles
            .text(TextSource {
                path: film,
                stream: &stream,
                to: SubtitleFormat::WebVtt,
            })
            .await
    }

    fn sidecar(path: PathBuf) -> SubtitleStream {
        SubtitleStream {
            source: SubtitleSource::External { path },
            codec: "subrip".into(),
            kind: SubtitleKind::Text,
            language: None,
            title: None,
            forced: false,
            default: false,
            role: SubtitleRole::Full,
        }
    }

    /// PX-09 finding 15 (B-8): a consumer asking for many text tracks at
    /// once started an ffmpeg for each. At most [`MAX_CONVERSIONS`] run
    /// at a time; the rest wait their turn, and all are made.
    #[tokio::test]
    async fn text_conversions_run_a_few_at_a_time() {
        let (_tmp, subtitles, film, marks) = converting("converting", "0.5").await;
        let mut states = Vec::new();
        for n in 0..8 {
            states.push(ask(&subtitles, &film, n).await);
        }
        let mut most = 0;
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !states.iter().all(|s| s.borrow().finished()) {
            most = most.max(std::fs::read_dir(&marks).unwrap().count());
            assert!(std::time::Instant::now() < deadline, "not all made");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(most <= MAX_CONVERSIONS, "{most} ran at once");
        assert!(
            states
                .iter()
                .all(|s| matches!(*s.borrow(), TextState::Ready(_)))
        );
    }

    /// Nor do they queue without end: past [`MAX_MAKING`] tracks being
    /// made or waiting, one more is refused for now, not queued.
    #[tokio::test]
    async fn past_a_limit_more_subtitles_are_refused_for_now() {
        let (_tmp, subtitles, film, _marks) = converting("too-many", "30").await;
        for n in 0..MAX_MAKING {
            let state = ask(&subtitles, &film, n).await;
            assert!(!state.borrow().finished(), "{n}: {:?}", *state.borrow());
        }
        let refused = ask(&subtitles, &film, MAX_MAKING).await;
        assert!(
            matches!(&*refused.borrow(), TextState::Failed(reason) if reason.contains("try again")),
            "{:?}",
            *refused.borrow()
        );
    }
}
