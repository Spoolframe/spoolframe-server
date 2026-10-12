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
//!
//! A bitmap track's pictures are decoded by the server's own binary run
//! again, confined as the tools are (finding 45, [`decode`]): never in
//! this process, which holds only the cues' times.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use spoolframe_media::decode::{self, CueTime};
use spoolframe_media::ocr::{self, Ocr};
use spoolframe_media::sandbox::Tool;
use spoolframe_media::{SubtitleKind, SubtitleSource, SubtitleStream, codecs};
use spoolframe_types::SubtitleFormat;

use crate::metrics::Counter;
use tokio::sync::{Semaphore, watch};

/// Text conversions (ffmpeg reading a track out of a file) at once
/// (PX-09 finding 15).
const MAX_CONVERSIONS: usize = 4;

/// Tracks being made or waiting their turn, of every kind, at once
/// (PX-09 finding 15).
const MAX_MAKING: usize = 64;

/// Tracks remembered as failed at once (review 2026-10-11 M14).
const MAX_FAILED: usize = 4096;

/// The most the cache's texts take on disk (review 2026-10-11 M15). A
/// film's track is some 100 KB, so this is thousands of tracks; past it
/// the least recently used go, oldest by modification time, which a read
/// from the cache moves to now.
const CACHE_LIMIT: u64 = 512 * 1024 * 1024;

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
    /// The server's own binary, as the subtitle decoder.
    decoder: Tool,
    threads: usize,
    cache_dir: PathBuf,
    making: Mutex<HashMap<String, watch::Receiver<TextState>>>,
    /// Tracks whose text could not be made, and why, until a restart: a
    /// client asking again is told at once instead of running the
    /// extract, decode and OCR again on the one OCR turn. At most
    /// [`MAX_FAILED`].
    failed: Mutex<HashMap<String, String>>,
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
    /// Reading with `ocr` (or not at all, if `settings` turn it off),
    /// tracks with ffmpeg and ffprobe, and their pictures decoded by
    /// `decoder` (the server's own binary). Work a stopped server left
    /// half done is removed.
    pub fn new(
        ocr: Ocr,
        ffmpeg: Tool,
        ffprobe: Tool,
        decoder: Tool,
        settings: &OcrSettings,
    ) -> std::io::Result<Self> {
        crate::files::private_dir(&settings.cache_dir)?;
        let work = settings.cache_dir.join("work");
        if work.exists() {
            std::fs::remove_dir_all(&work)?;
        }
        trim(&settings.cache_dir, CACHE_LIMIT);
        Ok(Self {
            ocr: if settings.enabled {
                ocr
            } else {
                Ocr::disabled()
            },
            ffmpeg,
            ffprobe,
            decoder,
            threads: settings.threads,
            cache_dir: settings.cache_dir.clone(),
            making: Mutex::new(HashMap::new()),
            failed: Mutex::new(HashMap::new()),
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
        if let Some(reason) = self.failed.lock().get(&key) {
            return TextPeek::Making(TextState::Failed(reason.clone()));
        }
        match self.making.lock().get(&key) {
            Some(state) => TextPeek::Making(state.borrow().clone()),
            None => TextPeek::NotMade,
        }
    }

    /// `key`'s text could not be made, for `reason`: kept until a restart,
    /// as previews keep theirs. Past [`MAX_FAILED`] one is forgotten for
    /// it, so the map stays bounded; a forgotten one is only tried again.
    fn remember_failed(&self, key: &str, reason: &str) {
        let mut failed = self.failed.lock();
        if failed.len() >= MAX_FAILED
            && !failed.contains_key(key)
            && let Some(any) = failed.keys().next().cloned()
        {
            failed.remove(&any);
        }
        failed.insert(key.to_owned(), reason.to_owned());
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
            touch(&text).await;
            return watch::channel(TextState::Ready(text)).1;
        }
        if let Some(reason) = self.failed.lock().get(&key) {
            return failed(reason.clone());
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
            if let TextState::Failed(reason) = &state {
                job.subtitles.remember_failed(&key, reason);
            }
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
        let dir = self.subtitles.cache_dir.clone();
        let _ = tokio::task::spawn_blocking(move || trim(&dir, CACHE_LIMIT)).await;
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
        // The decoder writes here and nowhere else; the lists tesseract
        // reads go beside it, where the decoder cannot.
        let pictures = self.work.join("pictures");
        tokio::fs::create_dir_all(&pictures)
            .await
            .map_err(|e| e.to_string())?;
        let times = self.decode(&pictures).await?;
        let total = times.len().max(1);
        let texts = self
            .subtitles
            .ocr
            .recognise(
                &pictures,
                times.len(),
                &self.work.join("lists"),
                language,
                self.subtitles.threads,
                &|done| {
                    let _ = state.send(TextState::Reading {
                        done_percent: (done * 100 / total) as u8,
                    });
                },
            )
            .await
            .map_err(|e| e.to_string())?;
        let cues = ocr::text_cues(&times, texts);
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

    /// The track's cues, decoded by the subtitle decoder into `pictures`:
    /// their times. PGS is copied out as a `.sup` (a file beside the film
    /// already is one), and the decoder reads the file the server opened;
    /// VobSub and DVB are read as packets by ffprobe, a `.idx` beside the
    /// film with its `.sub` as its first stream, and the decoder reads
    /// ffprobe's JSON.
    async fn decode(&self, pictures: &Path) -> Result<Vec<CueTime>, String> {
        let decoder = &self.subtitles.decoder;
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
            let json = ocr::read_track_json(&self.subtitles.ffprobe, input, index)
                .await
                .map_err(|e| e.to_string())?;
            let format = match self.format {
                SubtitleFormat::VobSub => decode::Format::VobSub,
                _ => decode::Format::Dvb,
            };
            return decode::run(
                decoder,
                format,
                from_start,
                decode::Input::Bytes(json),
                pictures,
            )
            .await
            .map_err(|e| e.to_string());
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
        // anyone's who can write to the library folder. The decoder reads
        // the file opened here, never a path; it reads up to
        // `ocr::MAX_SUP_BYTES` (PX-09 B-3).
        let file = crate::files::open_regular(&sup)
            .await
            .map_err(|e| e.to_string())?
            .into_std()
            .await;
        decode::run(
            decoder,
            decode::Format::Pgs,
            false,
            decode::Input::File(file),
            pictures,
        )
        .await
        .map_err(|e| e.to_string())
    }
}

/// A read of `text` from the cache: it is used now, so it goes last.
async fn touch(text: &Path) {
    let text = text.to_path_buf();
    let _ = tokio::task::spawn_blocking(move || {
        std::fs::File::options()
            .append(true)
            .open(&text)
            .and_then(|f| f.set_modified(std::time::SystemTime::now()))
    })
    .await;
}

/// Remove the least recently used texts of `dir` (its files; the work
/// directory is not one) until they take at most `limit` bytes. An entry
/// that cannot be read is left alone and logged.
fn trim(dir: &Path, limit: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        match entry.metadata() {
            Ok(meta) if meta.is_file() => files.push((
                meta.modified().unwrap_or(std::time::UNIX_EPOCH),
                meta.len(),
                entry.path(),
            )),
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(path = %entry.path().display(), error = %e, "a subtitle text cannot be read");
            }
        }
    }
    let mut total: u64 = files.iter().map(|(_, len, _)| len).sum();
    if total <= limit {
        return;
    }
    files.sort();
    for (_, len, path) in files {
        if total <= limit {
            break;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => total = total.saturating_sub(len),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "cannot remove a subtitle text")
            }
        }
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
    /// Past its limit the cache lets the least recently used texts go,
    /// and a read moves a text to now (review 2026-10-11 M15).
    #[tokio::test]
    async fn the_text_cache_keeps_the_most_recently_used_under_its_limit() {
        let dir = spoolframe_test_dir::TestDir::new("spoolframe-subtitles-trim").await;
        std::fs::create_dir(dir.path().join("work")).unwrap();
        let at = |secs: u64| std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        for (name, secs) in [("a.vtt", 10), ("b.vtt", 20), ("c.vtt", 30)] {
            let path = dir.path().join(name);
            std::fs::write(&path, vec![0u8; 100]).unwrap();
            std::fs::File::options()
                .append(true)
                .open(&path)
                .unwrap()
                .set_modified(at(secs))
                .unwrap();
        }
        trim(dir.path(), 250);
        let left = |name: &str| dir.path().join(name).exists();
        assert!(!left("a.vtt"), "the oldest goes");
        assert!(left("b.vtt") && left("c.vtt"));
        assert!(dir.path().join("work").is_dir());
        trim(dir.path(), 1000);
        assert!(left("b.vtt"), "under the limit nothing goes");
    }

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
                Tool::unconfined("/nonexistent/spoolframe-server"),
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

    /// The text of a PGS file beside `film`, read with OCR in English by
    /// `subtitles`, once it is made or has failed.
    async fn read_pgs(subtitles: &Arc<Subtitles>, film: &Path) -> TextState {
        let sup = film.with_file_name("Film.en.sup");
        // Once: written again it would be another file to the cache.
        if !sup.exists() {
            std::fs::write(&sup, b"PG").unwrap();
        }
        let stream = SubtitleStream {
            source: SubtitleSource::External { path: sup },
            codec: "hdmv_pgs_subtitle".into(),
            kind: SubtitleKind::Bitmap,
            language: Some("eng".into()),
            title: None,
            forced: false,
            default: false,
            role: SubtitleRole::Full,
        };
        let mut state = subtitles
            .text(TextSource {
                path: film,
                stream: &stream,
                to: SubtitleFormat::Srt,
            })
            .await;
        tokio::time::timeout(Duration::from_secs(20), state.wait_for(TextState::finished))
            .await
            .expect("made or failed in time")
            .unwrap()
            .clone()
    }

    /// Subtitles whose decoder is a stand-in script, `script`, in `dir`;
    /// tesseract is never reached.
    fn decoding_with(dir: &Path, script: &str) -> Arc<Subtitles> {
        let decoder = dir.join("decoder");
        crate::test_support::write_script(&decoder, script);
        Arc::new(
            Subtitles::new(
                Ocr::new(Tool::unconfined("/nonexistent/tesseract"), ["eng".into()]),
                Tool::unconfined("/nonexistent/ffmpeg"),
                Tool::unconfined("/nonexistent/ffprobe"),
                Tool::unconfined(decoder),
                &OcrSettings {
                    enabled: true,
                    threads: 1,
                    cache_dir: dir.join("cache"),
                },
            )
            .unwrap(),
        )
    }

    /// Finding 45: a decoder that panics (a parser bug a crafted file
    /// found) ends its child, and the track fails with its message; the
    /// server goes on, and the next request is answered.
    #[tokio::test]
    async fn a_decoder_that_panics_fails_the_track_not_the_server() {
        let tmp = spoolframe_test_dir::TestDir::new("spoolframe-subtitles-panic").await;
        let dir = tmp.as_ref();
        let subtitles = decoding_with(
            dir,
            "#!/bin/sh
echo \"thread 'main' panicked at crates/x.rs:1:1:\" >&2
             echo 'index out of bounds' >&2
exit 101
",
        );
        let film = dir.join("Film.mkv");
        std::fs::write(&film, b"a film").unwrap();
        match read_pgs(&subtitles, &film).await {
            TextState::Failed(reason) => {
                assert!(reason.contains("index out of bounds"), "{reason}")
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(subtitles.ocr_counts().failed, 1);
    }

    /// A track that failed is not made again: asked a second time, its
    /// reason comes back at once and the decoder does not run. Review
    /// 2026-10-11 M14.
    #[tokio::test]
    async fn a_track_that_failed_is_not_made_again() {
        let tmp = spoolframe_test_dir::TestDir::new("spoolframe-subtitles-failed-once").await;
        let dir = tmp.as_ref();
        let runs = dir.join("runs");
        let subtitles = decoding_with(
            dir,
            &format!(
                "#!/bin/sh
echo run >> '{}'
echo 'not a subtitle stream' >&2
exit 1
",
                runs.display()
            ),
        );
        let film = dir.join("Film.mkv");
        std::fs::write(&film, b"a film").unwrap();
        let first = read_pgs(&subtitles, &film).await;
        let second = read_pgs(&subtitles, &film).await;
        assert!(matches!(first, TextState::Failed(_)), "{first:?}");
        assert_eq!(first, second);
        let ran = std::fs::read_to_string(&runs).unwrap();
        assert_eq!(ran.lines().count(), 1, "the decoder ran again");
    }

    /// One killed (past its memory limit, say) fails the same way.
    #[tokio::test]
    async fn a_decoder_that_is_killed_fails_the_track() {
        let tmp = spoolframe_test_dir::TestDir::new("spoolframe-subtitles-killed").await;
        let dir = tmp.as_ref();
        let subtitles = decoding_with(
            dir,
            "#!/bin/sh
kill -KILL $$
",
        );
        let film = dir.join("Film.mkv");
        std::fs::write(&film, b"a film").unwrap();
        match read_pgs(&subtitles, &film).await {
            TextState::Failed(reason) => assert!(reason.contains("signal 9"), "{reason}"),
            other => panic!("{other:?}"),
        }
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
