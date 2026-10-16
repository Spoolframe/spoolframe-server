//! Running ffprobe, and remembering what it said.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use easy_ffprobe::FfProbe;

use crate::descriptor::{MediaDescriptor, SubtitleKind, SubtitleSource, SubtitleStream};
use crate::external;
use crate::keyframe_index::{self, IndexKind};
use crate::keyframe_store::{KeyframeStore, Made};
use crate::ocr::{self, OcrError};
use crate::roles;
use crate::sandbox::{Jail, Limits, Tool};
use spoolframe_types::SubtitleRole;

/// Why a file could not be described. A probe never answers a failure
/// with an empty descriptor.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("cannot read {path}: {source}")]
    Stat {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot run {ffprobe}: {source}")]
    Spawn {
        ffprobe: PathBuf,
        source: std::io::Error,
    },
    #[error("ffprobe on {path} took longer than {after:?} and was stopped")]
    TimedOut { path: PathBuf, after: Duration },
    #[error("ffprobe failed on {path} ({status}): {stderr}")]
    Failed {
        path: PathBuf,
        status: String,
        stderr: String,
    },
    #[error("ffprobe output for {path} is unreadable: {source}")]
    Unreadable {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("{path} has no audio or video stream")]
    NoMedia { path: PathBuf },
    /// Finding 51 (G-2): what ffprobe prints is read into the server, so
    /// it is capped; a crafted file can make it print gigabytes.
    #[error("ffprobe printed more than {limit} bytes for {path}")]
    TooMuch { path: PathBuf, limit: usize },
}

/// What a cached descriptor was made from. The directory's mtime is in it
/// so that adding or removing a subtitle file next to the media is noticed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    size: u64,
    modified: Option<SystemTime>,
    dir_modified: Option<SystemTime>,
}

/// Describes media files with ffprobe, and caches each descriptor until
/// its file changes.
#[derive(Debug)]
pub struct Prober {
    ffprobe: Tool,
    timeout: Duration,
    /// At most [`MAX_DESCRIPTORS`], the least used let go first.
    cache: Mutex<Lru<(Stamp, Arc<MediaDescriptor>)>>,
    /// At most [`MAX_KEYFRAME_LISTS`]: a film's list is 1 to 1.5 MB.
    keyframes: Mutex<Lru<(Stamp, Keyframes, KeyframesFrom)>>,
    /// The files whose keyframes are being read now, each with a turn a
    /// later caller waits on: one read of a file at a time, not one per
    /// caller (review 2026-10-11 M33). A scan reads the whole file.
    reading: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
    /// Where the lists that took a scan are kept across restarts; none
    /// without [`Prober::with_keyframe_store`].
    store: Option<KeyframeStore>,
}

/// A file's keyframe times, shared.
type Keyframes = Arc<KeyframeTimes>;

/// The main video stream's keyframes (PX-25): where a copied video can be
/// cut. Derefs to [`KeyframeTimes::times_ms`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyframeTimes {
    /// ms, ascending, deduplicated; one before 0 as 0.
    pub times_ms: Vec<u64>,
    /// The first keyframe's own time, ms, signed: before 0 in an MP4 cut
    /// with stream copy, which starts at the keyframe before the cut and
    /// shows from the cut on. A copy from the file's start begins there.
    /// 0 for no keyframe.
    pub first_ms: i64,
}

impl std::ops::Deref for KeyframeTimes {
    type Target = [u64];

    fn deref(&self) -> &[u64] {
        &self.times_ms
    }
}

/// The most descriptors kept: a film's is a few KB.
const MAX_DESCRIPTORS: usize = 4096;
/// The most keyframe lists kept: a film's is 1 to 1.5 MB, so about 100 MB.
const MAX_KEYFRAME_LISTS: usize = 64;

/// What a [`Prober`] remembers per file, at most `most` files, the one
/// used longest ago let go first (review 2026-10-11 M34). Entries for a
/// file that is gone are dropped when it is next asked for.
#[derive(Debug)]
struct Lru<V> {
    entries: HashMap<PathBuf, (u64, V)>,
    used: u64,
    most: usize,
}

impl<V> Lru<V> {
    fn new(most: usize) -> Self {
        Self {
            entries: HashMap::new(),
            used: 0,
            most,
        }
    }

    fn get(&mut self, path: &Path) -> Option<&V> {
        self.used += 1;
        let used = self.used;
        self.entries.get_mut(path).map(|(at, v)| {
            *at = used;
            &*v
        })
    }

    fn insert(&mut self, path: PathBuf, value: V) {
        self.used += 1;
        if self.entries.len() >= self.most
            && !self.entries.contains_key(&path)
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(p, _)| p.clone())
        {
            self.entries.remove(&oldest);
        }
        self.entries.insert(path, (self.used, value));
    }

    fn remove(&mut self, path: &Path) {
        self.entries.remove(path);
    }
}

/// A caller's place in line to read a file's keyframes; the last one out,
/// done or given up, takes the file off [`Prober::reading`].
struct Turn<'a> {
    prober: &'a Prober,
    path: &'a Path,
    gate: Arc<tokio::sync::Mutex<()>>,
}

impl<'a> Turn<'a> {
    fn take(prober: &'a Prober, path: &'a Path) -> Self {
        let gate = prober
            .reading
            .lock()
            .entry(path.to_owned())
            .or_default()
            .clone();
        Self { prober, path, gate }
    }
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        let mut reading = self.prober.reading.lock();
        // The map's and this one's: no one else is in line.
        if Arc::strong_count(&self.gate) == 2 {
            reading.remove(self.path);
        }
    }
}

/// Where a file's keyframes were read from (PX-25).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyframesFrom {
    /// The file's own index, checked against what ffprobe reads around a
    /// few of its entries.
    Index(IndexKind),
    /// Every packet of the file, and why not the index.
    Scan { why: String },
    /// A scan of the file as it is now, kept on disk
    /// ([`Prober::with_keyframe_store`]).
    Stored,
}

/// What [`Prober::keyframes_ahead`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ahead {
    /// Nothing: the file's keyframes are in memory or kept already.
    Known,
    /// Nothing: the file has an index the reader follows, which a play
    /// reads in moments.
    Indexed,
    /// Every packet was read, `bytes` of file, and the list kept.
    Scanned { bytes: u64 },
}

/// The intervals between indexed keyframes that are read to check an
/// index: the first, two between, and the last.
const CHECKED_INTERVALS: usize = 4;

/// The longest interval between indexed keyframes the check reads, and
/// that an index may have: a GOP longer than this is rare, and an index
/// with one is more likely missing keyframes.
const LONGEST_INDEXED_GOP_MS: u64 = 30_000;

/// How long listing a file's keyframes may take: it reads the whole file,
/// which on a network mount is minutes for a large remux.
const KEYFRAME_TIMEOUT: Duration = Duration::from_secs(600);

/// The most of ffprobe's JSON description read. A film's is tens of KB;
/// one with hundreds of streams is a few MB.
const MAX_DESCRIPTION: usize = 64 << 20;

/// The most of ffprobe's keyframe listing read: a line of about 16 bytes
/// (`1234.567000,K__`) per video packet. A 100-minute 24 fps film's is
/// 2.3 MB (143,764 lines, measured); a 4-hour 60 fps one's about 14 MB.
/// 64 MiB is about 4 million packets: 48 hours at 24 fps.
const MAX_KEYFRAME_LISTING: usize = 64 << 20;

impl Prober {
    /// How long one ffprobe run may take by default. Generous because the
    /// media may be on a network mount that has to spin up.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

    /// A prober running `ffprobe`.
    pub fn new(ffprobe: Tool, timeout: Duration) -> Self {
        Self {
            ffprobe,
            timeout,
            cache: Mutex::new(Lru::new(MAX_DESCRIPTORS)),
            keyframes: Mutex::new(Lru::new(MAX_KEYFRAME_LISTS)),
            reading: Mutex::new(HashMap::new()),
            store: None,
        }
    }

    /// Keep each list that took a scan in `store`, and look there before
    /// reading a file: a restart does not read a whole file again.
    pub fn with_keyframe_store(mut self, store: KeyframeStore) -> Self {
        self.store = Some(store);
        self
    }

    /// The descriptor of the media file at `path`, with the subtitle files
    /// beside it.
    pub async fn describe(&self, path: &Path) -> Result<Arc<MediaDescriptor>, ProbeError> {
        let stamp = self.stamp_or_forget(path).await?;
        if let Some(hit) = self.cached(path, stamp) {
            return Ok(hit);
        }

        let mut descriptor = MediaDescriptor::from_probe(&self.run(path).await?);
        if descriptor.video.is_none() && descriptor.audio.is_empty() {
            return Err(ProbeError::NoMedia {
                path: path.to_owned(),
            });
        }
        // Listing the directory and resolving its links is file-system
        // work, on a network mount at that: off the async workers (review
        // 2026-10-11 L60).
        let media = path.to_owned();
        let external = tokio::task::spawn_blocking(move || external::find(&media))
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "looking for subtitle files did not finish");
                Vec::new()
            });
        for file in external {
            // A broken subtitle file loses that subtitle, not the media.
            match self.external_subtitle(file).await {
                Ok(Some(sub)) => descriptor.subtitles.push(sub),
                Ok(None) => {}
                Err(e) => tracing::warn!(error = %e, "skipping subtitle file"),
            }
        }

        let descriptor = Arc::new(descriptor);
        self.cache
            .lock()
            .insert(path.to_owned(), (stamp, descriptor.clone()));
        Ok(descriptor)
    }

    fn cached(&self, path: &Path, stamp: Stamp) -> Option<Arc<MediaDescriptor>> {
        self.cache
            .lock()
            .get(path)
            .filter(|(cached, _)| *cached == stamp)
            .map(|(_, d)| d.clone())
    }

    async fn external_subtitle(
        &self,
        file: external::ExternalFile,
    ) -> Result<Option<SubtitleStream>, ProbeError> {
        let probed = MediaDescriptor::from_probe(&self.run(&file.path).await?);
        let name = file.name;
        Ok(probed.subtitles.into_iter().next().map(|s| {
            // The file name is what people set; the file rarely says.
            let flags = roles::Flags {
                forced: name.forced || s.role == SubtitleRole::Forced,
                hearing_impaired: name.sdh || s.role == SubtitleRole::Sdh,
                comment: s.role == SubtitleRole::Commentary,
                ..roles::Flags::default()
            };
            let role = roles::subtitle_role(flags, s.title.as_deref());
            SubtitleStream {
                kind: SubtitleKind::of_codec(&s.codec),
                codec: s.codec,
                language: name.language.or(s.language),
                title: s.title,
                forced: role == SubtitleRole::Forced,
                default: name.default || s.default,
                role,
                source: SubtitleSource::External { path: file.path },
            }
        }))
    }

    /// The times of the main video stream's keyframes, in milliseconds,
    /// ascending, one before time 0 as 0: where a copied video can be cut
    /// (P2-05's HLS segments). Remembered until the file changes.
    pub async fn keyframes(&self, path: &Path) -> Result<Keyframes, ProbeError> {
        Ok(self.keyframes_from(path).await?.0)
    }

    /// [`Prober::keyframes`], and where they were read from.
    ///
    /// PX-25: from the file's own index (Matroska `Cues`, MP4 `stss`,
    /// `keyframe_index`) where it lists them all, so a copied video's HLS
    /// starts without reading the whole file first. An index is a claim:
    /// a muxer may index only some keyframes. It is used only if ffprobe,
    /// reading the packets of a few intervals between its entries, finds
    /// a keyframe at each entry and none between; otherwise, and for a
    /// file with no index the reader follows, every packet is read
    /// ([`Prober::scan_keyframes`]). A scan is kept in the keyframe store,
    /// where one is, and taken from there while the file is unchanged.
    pub async fn keyframes_from(
        &self,
        path: &Path,
    ) -> Result<(Keyframes, KeyframesFrom), ProbeError> {
        let stamp = self.stamp_or_forget(path).await?;
        if let Some(hit) = self.keyframes_cached(path, stamp) {
            return Ok(hit);
        }
        // Two starts of one film (or a client asking again during a slow
        // scan) wait for one read; the second finds it cached.
        let turn = Turn::take(self, path);
        let _mine = turn.gate.lock().await;
        if let Some(hit) = self.keyframes_cached(path, stamp) {
            return Ok(hit);
        }
        self.read_keyframes(path, stamp).await
    }

    /// Read `path`'s keyframes ahead of any play, if a play would read
    /// every packet: at the lowest CPU and I/O priority, kept in the
    /// keyframe store rather than in memory. A play of the file that
    /// starts meanwhile waits for this read and takes what it kept, as a
    /// second start of a film waits for the first's.
    ///
    /// The file's index is read but not checked against its packets: a
    /// play does that in seconds, and scans only if it fails.
    pub async fn keyframes_ahead(&self, path: &Path) -> Result<Ahead, ProbeError> {
        let stamp = self.stamp_or_forget(path).await?;
        if self.keyframes_cached(path, stamp).is_some() {
            return Ok(Ahead::Known);
        }
        let turn = Turn::take(self, path);
        let _mine = turn.gate.lock().await;
        if self.keyframes_cached(path, stamp).is_some() || self.is_stored(path, stamp).await {
            return Ok(Ahead::Known);
        }
        if read_index(path).await.is_ok() {
            return Ok(Ahead::Indexed);
        }
        let times = self.scan(path, true).await?;
        if !self.keep(path, stamp, &times).await {
            // Nowhere to keep it but memory.
            self.keyframes.lock().insert(
                path.to_owned(),
                (
                    stamp,
                    times,
                    KeyframesFrom::Scan {
                        why: "read ahead".into(),
                    },
                ),
            );
        }
        Ok(Ahead::Scanned { bytes: stamp.size })
    }

    /// `path`'s stamp; one that cannot be had (the file is gone or
    /// renamed) drops what is remembered of it.
    async fn stamp_or_forget(&self, path: &Path) -> Result<Stamp, ProbeError> {
        let stamped = stamp(path).await;
        if stamped.is_err() {
            self.cache.lock().remove(path);
            self.keyframes.lock().remove(path);
            if let Some(store) = self.store.clone() {
                let path = path.to_owned();
                let _ = tokio::task::spawn_blocking(move || store.forget(&path)).await;
            }
        }
        stamped
    }

    /// The list kept for `path` as `stamp` says it is, read on a blocking
    /// thread.
    async fn stored(&self, path: &Path, stamp: Stamp) -> Option<KeyframeTimes> {
        let store = self.store.clone()?;
        let made = Made::of(stamp.size, stamp.modified)?;
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || store.get(&path, made))
            .await
            .ok()
            .flatten()
    }

    /// Whether a list is kept for `path` as `stamp` says it is.
    async fn is_stored(&self, path: &Path, stamp: Stamp) -> bool {
        let (Some(store), Some(made)) = (self.store.clone(), Made::of(stamp.size, stamp.modified))
        else {
            return false;
        };
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || store.has(&path, made))
            .await
            .unwrap_or(false)
    }

    /// Keep `times` for `path` as `stamp` says it is: whether it was.
    async fn keep(&self, path: &Path, stamp: Stamp, times: &Keyframes) -> bool {
        let (Some(store), Some(made)) = (self.store.clone(), Made::of(stamp.size, stamp.modified))
        else {
            return false;
        };
        let (owned, times) = (path.to_owned(), times.clone());
        let kept = tokio::task::spawn_blocking(move || store.put(&owned, made, &times))
            .await
            .map_err(std::io::Error::other)
            .and_then(|kept| kept);
        match kept {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "cannot keep the keyframes read");
                false
            }
        }
    }

    fn keyframes_cached(&self, path: &Path, stamp: Stamp) -> Option<(Keyframes, KeyframesFrom)> {
        self.keyframes
            .lock()
            .get(path)
            .filter(|(cached, _, _)| *cached == stamp)
            .map(|(_, times, from)| (times.clone(), from.clone()))
    }

    /// `path`'s keyframes read, and remembered under `stamp`.
    async fn read_keyframes(
        &self,
        path: &Path,
        stamp: Stamp,
    ) -> Result<(Keyframes, KeyframesFrom), ProbeError> {
        if let Some(times) = self.stored(path, stamp).await {
            let times = Arc::new(times);
            self.keyframes.lock().insert(
                path.to_owned(),
                (stamp, times.clone(), KeyframesFrom::Stored),
            );
            return Ok((times, KeyframesFrom::Stored));
        }
        let indexed = match read_index(path).await {
            Ok(indexed) => self
                .check_index(path, &indexed.times_ms)
                .await
                .map(|()| indexed),
            Err(why) => Err(why),
        };
        let (times, from) = match indexed {
            Ok(indexed) => (
                Arc::new(KeyframeTimes {
                    times_ms: indexed.times_ms,
                    first_ms: indexed.first_ms,
                }),
                KeyframesFrom::Index(indexed.kind),
            ),
            Err(why) => {
                tracing::debug!(path = %path.display(), %why, "keyframes from every packet");
                let times = self.scan(path, false).await?;
                self.keep(path, stamp, &times).await;
                (times, KeyframesFrom::Scan { why })
            }
        };
        self.keyframes
            .lock()
            .insert(path.to_owned(), (stamp, times.clone(), from.clone()));
        Ok((times, from))
    }

    /// Whether ffprobe finds `index`'s keyframes, and only those, in a few
    /// of the intervals between them: the first, the last and two
    /// between, each read from its keyframe (ffmpeg seeks there by the
    /// same index) to the next. A muxer that indexes only some keyframes
    /// (one per cluster, one every few seconds) leaves some inside the
    /// intervals, in every part of the film alike; a keyframe left out
    /// here and there, nowhere near the intervals read, is not found.
    async fn check_index(&self, path: &Path, index: &[u64]) -> Result<(), String> {
        let gaps = index.windows(2).map(|w| w[1] - w[0]);
        match gaps.max() {
            None => return Err("the index lists one keyframe".into()),
            Some(gap) if gap > LONGEST_INDEXED_GOP_MS => {
                return Err(format!("the index has {gap} ms between two keyframes"));
            }
            Some(_) => {}
        }
        let last = index.len() - 2;
        let mut picked: Vec<usize> = (0..CHECKED_INTERVALS)
            .map(|k| last * k / (CHECKED_INTERVALS - 1))
            .collect();
        picked.dedup();
        let windows: Vec<(u64, u64)> = picked.iter().map(|&i| (index[i], index[i + 1])).collect();
        // From half a millisecond in, so a keyframe the index rounds up a
        // little is still the one sought to.
        let intervals = windows
            .iter()
            .map(|&(from, to)| {
                format!(
                    "{:.4}%{:.4}",
                    (from as f64 + 0.5) / 1000.0,
                    to as f64 / 1000.0
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let mut cmd = self.command(path).await.map_err(|e| e.to_string())?;
        keyframe_listing(&mut cmd, path, Some(&intervals));
        let output = self
            .output(cmd, path, MAX_KEYFRAME_LISTING, self.timeout)
            .await
            .map_err(|e| format!("checking the index: {e}"))?;
        let read = keyframe_times(&String::from_utf8_lossy(&output)).times_ms;
        for (from, to) in windows {
            let found: Vec<u64> = read
                .iter()
                .copied()
                .filter(|t| (from..to).contains(t))
                .collect();
            if found != [from] {
                return Err(format!(
                    "the index lists a keyframe at {from} ms and the next at {to} ms; \
                     the packets between have keyframes at {found:?}"
                ));
            }
        }
        Ok(())
    }

    /// The keyframes from every packet of the file, read without
    /// decoding: all of the file is read. Not cached.
    pub async fn scan_keyframes(&self, path: &Path) -> Result<Keyframes, ProbeError> {
        self.scan(path, false).await
    }

    /// [`Prober::scan_keyframes`], at the lowest priority if `low`.
    async fn scan(&self, path: &Path, low: bool) -> Result<Keyframes, ProbeError> {
        let mut cmd = self.command(path).await?;
        if low {
            crate::sandbox::low_priority(&mut cmd);
        }
        keyframe_listing(&mut cmd, path, None);
        let output = self
            .output(cmd, path, MAX_KEYFRAME_LISTING, KEYFRAME_TIMEOUT)
            .await?;
        Ok(Arc::new(keyframe_times(&String::from_utf8_lossy(&output))))
    }

    /// When stream `index`'s first packet is, in ms from the file's
    /// start; `None` for a stream with no packet. Only the file up to that
    /// packet is read. A Matroska track's start says 0 even where its
    /// first packet is seconds in (a dub muxed late): the packet is what
    /// counts. Not cached.
    pub async fn first_packet_ms(
        &self,
        path: &Path,
        index: u32,
    ) -> Result<Option<u64>, ProbeError> {
        let mut cmd = self.command(path).await?;
        cmd.args(["-v", "error", "-select_streams"])
            .arg(index.to_string())
            .args([
                "-read_intervals",
                "%+#1",
                "-show_entries",
                "packet=pts_time",
                "-of",
                "csv=p=0",
                "-i",
            ])
            .arg(path);
        let output = self.output(cmd, path, 4096, self.timeout).await?;
        Ok(first_time_ms(&String::from_utf8_lossy(&output)))
    }

    /// ffprobe confined to media `path` ([`Jail::media`]), made off the
    /// async workers ([`crate::sandbox::Tool::command_off_thread`]).
    async fn command(&self, path: &Path) -> Result<crate::sandbox::Command, ProbeError> {
        self.ffprobe
            .command_off_thread(Jail::new(Limits::PROBE).media(path))
            .await
            .map_err(|source| ProbeError::Spawn {
                ffprobe: self.ffprobe.program().to_owned(),
                source,
            })
    }

    async fn run(&self, path: &Path) -> Result<FfProbe, ProbeError> {
        let mut cmd = self.command(path).await?;
        cmd.args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
            // Each stream's codec record as a hex dump: whether a video's
            // holds its parameter sets (`VideoStream::parameter_sets_in_record`).
            "-show_data",
            // `-i` so a path is never read as an option.
            "-i",
        ])
        .arg(path);
        let output = self
            .output(cmd, path, MAX_DESCRIPTION, self.timeout)
            .await?;
        serde_json::from_slice(&output).map_err(|source| ProbeError::Unreadable {
            path: path.to_owned(),
            source,
        })
    }

    /// `cmd` run to its end: its stdout, up to `max` bytes, if it
    /// succeeded. Killed past `max` or `timeout`. Only the first 64 KiB
    /// of stderr are kept.
    async fn output(
        &self,
        cmd: crate::sandbox::Command,
        path: &Path,
        max: usize,
        timeout: Duration,
    ) -> Result<Vec<u8>, ProbeError> {
        let ran = ocr::run(cmd, "ffprobe", ocr::Feed::Nothing, max, timeout)
            .await
            .map_err(|e| match e {
                OcrError::TimedOut { after, .. } => ProbeError::TimedOut {
                    path: path.to_owned(),
                    after,
                },
                OcrError::TooMuch { limit, .. } => ProbeError::TooMuch {
                    path: path.to_owned(),
                    limit,
                },
                OcrError::Spawn { source, .. } | OcrError::Io(source) => ProbeError::Spawn {
                    ffprobe: self.ffprobe.program().to_owned(),
                    source,
                },
                // `run` gives no other.
                other => ProbeError::Spawn {
                    ffprobe: self.ffprobe.program().to_owned(),
                    source: std::io::Error::other(other.to_string()),
                },
            })?;
        if !ran.status.success() {
            return Err(ProbeError::Failed {
                path: path.to_owned(),
                status: ran.status.to_string(),
                stderr: String::from_utf8_lossy(&ran.stderr).trim().to_owned(),
            });
        }
        Ok(ran.stdout)
    }
}

/// The keyframes `path`'s index lists, read on a blocking thread.
async fn read_index(path: &Path) -> Result<keyframe_index::Indexed, String> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        // Unbuffered: the reader asks for what it reads, a header or a
        // whole element, and seeks between.
        let mut file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
        let len = file.metadata().map_err(|e| e.to_string())?.len();
        keyframe_index::read(&mut file, len).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// ffprobe's listing of `path`'s first video stream's packets, as
/// [`keyframe_times`] reads it; limited to `intervals`
/// (`-read_intervals`) when given.
fn keyframe_listing(cmd: &mut crate::sandbox::Command, path: &Path, intervals: Option<&str>) {
    cmd.args(["-v", "error", "-select_streams", "v:0"]);
    if let Some(intervals) = intervals {
        cmd.args(["-read_intervals", intervals]);
    }
    cmd.args([
        "-show_entries",
        "packet=pts_time,flags",
        "-of",
        "csv=p=0",
        "-i",
    ])
    .arg(path);
}

/// The first `pts_time` line's time in ms, one before 0 as 0; `None` for
/// none (or `N/A`).
fn first_time_ms(csv: &str) -> Option<u64> {
    let secs: f64 = csv
        .lines()
        .find_map(|line| line.trim().trim_end_matches(',').parse().ok())?;
    Some((secs.max(0.0) * 1000.0).round() as u64)
}

/// `pts_time,flags` lines (`12.345000,K__`) → the keyframes' times in ms,
/// ascending. A keyframe before time 0 counts as at 0: it is still the
/// file's first keyframe (an MP4 cut with stream copy starts at the
/// keyframe before the cut), and a list without it would start at the
/// second. Its own time is kept as [`KeyframeTimes::first_ms`].
fn keyframe_times(csv: &str) -> KeyframeTimes {
    let seconds: Vec<f64> = csv
        .lines()
        .filter_map(|line| {
            let (time, flags) = line.trim().split_once(',')?;
            if !flags.starts_with('K') {
                return None;
            }
            time.parse().ok()
        })
        .collect();
    let mut times: Vec<u64> = seconds.iter().map(|&s| keyframe_index::ms(s)).collect();
    times.sort_unstable();
    times.dedup();
    // ffmpeg starts at the last keyframe at or before 0; with none, at
    // the first.
    let start = seconds
        .iter()
        .copied()
        .filter(|&t| t <= 0.0)
        .reduce(f64::max)
        .or_else(|| seconds.iter().copied().reduce(f64::min));
    let first_ms = start.map_or(0, keyframe_index::signed_ms);
    KeyframeTimes {
        times_ms: times,
        first_ms,
    }
}

async fn stamp(path: &Path) -> Result<Stamp, ProbeError> {
    let meta = tokio::fs::metadata(path)
        .await
        .map_err(|source| ProbeError::Stat {
            path: path.to_owned(),
            source,
        })?;
    let dir_modified = match path.parent() {
        Some(dir) => tokio::fs::metadata(dir)
            .await
            .ok()
            .and_then(|m| m.modified().ok()),
        None => None,
    };
    Ok(Stamp {
        size: meta.len(),
        modified: meta.modified().ok(),
        dir_modified,
    })
}

#[cfg(test)]
mod tests {
    use super::{Lru, first_time_ms, keyframe_times};
    use std::path::{Path, PathBuf};

    /// A dub muxed 6.2 s in (Saw, 2004); one before 0 is at 0.
    #[test]
    fn a_first_packet_is_its_time_in_ms() {
        assert_eq!(first_time_ms("6.200000\n"), Some(6200));
        assert_eq!(first_time_ms("-0.021000,\n"), Some(0));
        assert_eq!(first_time_ms("N/A\n0.500000\n"), Some(500));
        assert_eq!(first_time_ms(""), None);
    }

    /// Past its bound the entry used longest ago goes, not the newest.
    /// Review 2026-10-11 M34.
    #[test]
    fn a_full_cache_lets_go_of_the_least_used() {
        let mut lru = Lru::new(2);
        lru.insert(PathBuf::from("/a"), 1);
        lru.insert(PathBuf::from("/b"), 2);
        assert_eq!(lru.get(Path::new("/a")), Some(&1));
        lru.insert(PathBuf::from("/c"), 3);
        assert_eq!(lru.get(Path::new("/b")), None);
        assert_eq!(lru.get(Path::new("/a")), Some(&1));
        assert_eq!(lru.get(Path::new("/c")), Some(&3));
        assert_eq!(lru.entries.len(), 2);
    }

    #[test]
    fn only_keyframes_count_in_order() {
        let csv = "2.000000,K__\n0.040000,___\n0.000000,K__\nN/A,K__\n4.000000,K_D\n";
        assert_eq!(keyframe_times(csv).times_ms, [0, 2000, 4000]);
    }

    /// An MP4 cut with stream copy starts at the keyframe before the cut,
    /// a little before its time 0. It is the file's first keyframe.
    #[test]
    fn a_keyframe_before_zero_is_the_first_at_zero() {
        let csv = "-0.040000,KD_\n0.000000,___\n1.960000,K__\n3.960000,K__\n";
        let times = keyframe_times(csv);
        assert_eq!(times.times_ms, [0, 1960, 3960]);
        assert_eq!(times.first_ms, -40);
    }
}
