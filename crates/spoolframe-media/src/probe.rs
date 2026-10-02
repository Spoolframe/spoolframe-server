//! Running ffprobe, and remembering what it said.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use easy_ffprobe::FfProbe;

use crate::descriptor::{MediaDescriptor, SubtitleKind, SubtitleSource, SubtitleStream};
use crate::external;

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
    ffprobe: PathBuf,
    timeout: Duration,
    cache: Mutex<HashMap<PathBuf, (Stamp, Arc<MediaDescriptor>)>>,
}

impl Prober {
    /// How long one ffprobe run may take by default. Generous because the
    /// media may be on a network mount that has to spin up.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

    /// A prober running `ffprobe` (a path, or a name looked up in `PATH`).
    pub fn new(ffprobe: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self {
            ffprobe: ffprobe.into(),
            timeout,
            cache: Mutex::new(HashMap::new()),
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
            .expect("the cache lock is never held across a panic")
            .insert(path.to_owned(), (stamp, descriptor.clone()));
        Ok(descriptor)
    }

    fn cached(&self, path: &Path, stamp: Stamp) -> Option<Arc<MediaDescriptor>> {
        let cache = self
            .cache
            .lock()
            .expect("the cache lock is never held across a panic");
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
        Ok(probed.subtitles.into_iter().next().map(|s| SubtitleStream {
            kind: SubtitleKind::of_codec(&s.codec),
            codec: s.codec,
            // The file name is what people set; the file rarely says.
            language: file.language.or(s.language),
            title: s.title,
            forced: file.forced || s.forced,
            default: file.default || s.default,
            source: SubtitleSource::External { path: file.path },
        }))
    }

    /// One ffprobe run. The process is killed if it outlives the timeout.
    async fn run(&self, path: &Path) -> Result<FfProbe, ProbeError> {
        let mut cmd = tokio::process::Command::new(&self.ffprobe);
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
        .arg(path)
        .stdin(Stdio::null())
        .kill_on_drop(true);

        let output = match tokio::time::timeout(self.timeout, cmd.output()).await {
            Err(_) => {
                return Err(ProbeError::TimedOut {
                    path: path.to_owned(),
                    after: self.timeout,
                });
            }
            Ok(Err(source)) => {
                return Err(ProbeError::Spawn {
                    ffprobe: self.ffprobe.clone(),
                    source,
                });
            }
            Ok(Ok(output)) => output,
        };
        if !output.status.success() {
            return Err(ProbeError::Failed {
                path: path.to_owned(),
                status: output.status.to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        serde_json::from_slice(&output.stdout).map_err(|source| ProbeError::Unreadable {
            path: path.to_owned(),
            source,
        })
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
