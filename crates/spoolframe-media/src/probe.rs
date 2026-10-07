//! Running ffprobe, and remembering what it said.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use easy_ffprobe::FfProbe;

use crate::descriptor::{MediaDescriptor, SubtitleKind, SubtitleSource, SubtitleStream};
use crate::external;
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
    cache: Mutex<HashMap<PathBuf, (Stamp, Arc<MediaDescriptor>)>>,
    keyframes: Mutex<HashMap<PathBuf, (Stamp, Keyframes)>>,
}

/// A file's keyframe times, ms.
type Keyframes = Arc<Vec<u64>>;

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
            cache: Mutex::new(HashMap::new()),
            keyframes: Mutex::new(HashMap::new()),
        }
    }

    /// The descriptor of the media file at `path`, with the subtitle files
    /// beside it.
    pub async fn describe(&self, path: &Path) -> Result<Arc<MediaDescriptor>, ProbeError> {
        let stamp = stamp(path).await?;
        if let Some(hit) = self.cached(path, stamp) {
            return Ok(hit);
        }

        let mut descriptor = MediaDescriptor::from_probe(&self.run(path).await?);
        if descriptor.video.is_none() && descriptor.audio.is_empty() {
            return Err(ProbeError::NoMedia {
                path: path.to_owned(),
            });
        }
        for file in external::find(path) {
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
        let cache = self.cache.lock();
        cache
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

    /// One ffprobe run. The process is killed if it outlives the timeout.
    /// The times of the main video stream's keyframes, in milliseconds,
    /// ascending, one before time 0 as 0: where a copied video can be cut
    /// (P2-05's HLS segments).
    /// Read once per file from the packets' flags, without decoding, and
    /// remembered until the file changes.
    pub async fn keyframes(&self, path: &Path) -> Result<Keyframes, ProbeError> {
        let stamp = stamp(path).await?;
        if let Some((cached, times)) = self.keyframes.lock().get(path)
            && *cached == stamp
        {
            return Ok(times.clone());
        }
        let mut cmd = self.command(path)?;
        cmd.args(["-v", "error", "-select_streams", "v:0"])
            .args([
                "-show_entries",
                "packet=pts_time,flags",
                "-of",
                "csv=p=0",
                "-i",
            ])
            .arg(path);
        let output = self
            .output(cmd, path, MAX_KEYFRAME_LISTING, KEYFRAME_TIMEOUT)
            .await?;
        let times = Arc::new(keyframe_times(&String::from_utf8_lossy(&output)));
        self.keyframes
            .lock()
            .insert(path.to_owned(), (stamp, times.clone()));
        Ok(times)
    }

    /// ffprobe confined to `path`: a VobSub `.idx` with its `.sub`, which
    /// ffmpeg opens by the `.idx`'s name.
    fn command(&self, path: &Path) -> Result<crate::sandbox::Command, ProbeError> {
        let mut jail = Jail::new(Limits::PROBE).input(path);
        if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("idx"))
        {
            jail = jail.input(path.with_extension("sub"));
        }
        self.ffprobe
            .command(&jail)
            .map_err(|source| ProbeError::Spawn {
                ffprobe: self.ffprobe.program().to_owned(),
                source,
            })
    }

    async fn run(&self, path: &Path) -> Result<FfProbe, ProbeError> {
        let mut cmd = self.command(path)?;
        cmd.args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
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

/// `pts_time,flags` lines (`12.345000,K__`) → the keyframes' times in ms,
/// ascending. A keyframe before time 0 counts as at 0: it is still the
/// file's first keyframe (an MP4 cut with stream copy starts at the
/// keyframe before the cut), and a list without it would start at the
/// second.
fn keyframe_times(csv: &str) -> Vec<u64> {
    let mut times: Vec<u64> = csv
        .lines()
        .filter_map(|line| {
            let (time, flags) = line.trim().split_once(',')?;
            if !flags.starts_with('K') {
                return None;
            }
            let seconds: f64 = time.parse().ok()?;
            Some((seconds.max(0.0) * 1000.0).round() as u64)
        })
        .collect();
    times.sort_unstable();
    times.dedup();
    times
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
    use super::keyframe_times;

    #[test]
    fn only_keyframes_count_in_order() {
        let csv = "2.000000,K__\n0.040000,___\n0.000000,K__\nN/A,K__\n4.000000,K_D\n";
        assert_eq!(keyframe_times(csv), [0, 2000, 4000]);
    }

    /// An MP4 cut with stream copy starts at the keyframe before the cut,
    /// a little before its time 0. It is the file's first keyframe.
    #[test]
    fn a_keyframe_before_zero_is_the_first_at_zero() {
        let csv = "-0.040000,KD_\n0.000000,___\n1.960000,K__\n3.960000,K__\n";
        assert_eq!(keyframe_times(csv), [0, 1960, 3960]);
    }
}
