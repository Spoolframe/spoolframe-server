//! A download's file written (PX-38): one jailed ffmpeg, a remux with no
//! encode, in a run directory of its own in the area's `making/`.
//!
//! The run reads links in its directory: `input` to the media file, the
//! copy's stored tracks (lent, in the pool's directory), and each
//! subtitle from a file of its own. The server's own files, which are in
//! its hidden state directory (PX-02's downloaded subtitles, PX-03's
//! served ones, text read from pictures), are copied in instead: no run
//! may open one there as an input. ffmpeg writes `<key>.part` there; once
//! it exits cleanly the part is hashed and renamed into the area.

use std::collections::VecDeque;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::sync::watch;

use spoolframe_media::MediaDescriptor;

use crate::encode::job::{
    Jobs, drain_stderr, jail, keep, launch, link, out_time_us, signal, stopped,
};
use crate::optimized::CopyFiles;
use crate::optimized::mux::{MuxFormat, MuxInputs, MuxPicks, mux_args_as};

/// A subtitle from a file of its own, for one run.
#[derive(Debug, Clone)]
pub(super) struct SubtitleFile {
    /// Its position in the descriptor.
    pub position: u32,
    /// The file: beside the media file, or the server's own.
    pub path: PathBuf,
    /// The server's own (copied into the run, not linked).
    pub own: bool,
    /// VobSub: its pictures are in the `.sub` beside the `.idx`.
    pub vobsub: bool,
}

/// What one run writes.
pub(super) struct Run<'a> {
    pub jobs: &'a Jobs,
    /// The run's own directory, made here.
    pub dir: &'a Path,
    pub media: &'a Path,
    pub descriptor: &'a MediaDescriptor,
    /// The copy's stored tracks, lent for the run.
    pub stored: &'a CopyFiles,
    pub subtitles: &'a [SubtitleFile],
    pub picks: &'a MuxPicks,
    pub format: MuxFormat,
    /// The most any file of the run may grow to.
    pub max_size: u64,
}

/// How a run ended other than in a file.
#[derive(Debug)]
pub(super) enum RunError {
    /// Told to stop: the download was cancelled or the server stops.
    Stopped,
    /// It failed, for this reason (the user's to read).
    Failed(String),
}

/// Write `run` into `part`, telling `progress` the fraction of the film
/// written, until it ends or `stop` says to stop.
pub(super) async fn write(
    run: &Run<'_>,
    part: &Path,
    progress: impl Fn(Option<f64>),
    stop: &mut watch::Receiver<bool>,
) -> Result<(), RunError> {
    let prepare = |e: std::io::Error| RunError::Failed(format!("cannot prepare the file: {e}"));
    std::fs::create_dir_all(run.dir).map_err(prepare)?;
    // What the jail lets ffmpeg open as inputs: the real files the links
    // lead to.
    let mut reads = vec![run.media.to_path_buf()];
    let input = run.dir.join("input");
    link(run.media, &input).map_err(prepare)?;
    let mut files = CopyFiles::default();
    if let Some(video) = &run.stored.video {
        let at = run.dir.join("video.mkv");
        link(video, &at).map_err(prepare)?;
        reads.push(video.clone());
        files.video = Some(at);
    }
    for (stream, audio) in &run.stored.audio {
        let at = run.dir.join(format!("audio-{stream}.mkv"));
        link(audio, &at).map_err(prepare)?;
        reads.push(audio.clone());
        files.audio.insert(*stream, at);
    }
    let mut inputs = MuxInputs::new(&input, &files);
    for sub in run.subtitles {
        let ext = sub
            .path
            .extension()
            .map_or_else(String::new, |e| format!(".{}", e.to_string_lossy()));
        let at = run.dir.join(format!("subtitle-{}{ext}", sub.position));
        if sub.own {
            std::fs::copy(&sub.path, &at).map_err(prepare)?;
        } else {
            link(&sub.path, &at).map_err(prepare)?;
            reads.push(sub.path.clone());
        }
        if sub.vobsub {
            // ffmpeg finds the pictures by the `.idx`'s name.
            let pictures = sub.path.with_extension("sub");
            let beside = at.with_extension("sub");
            if sub.own {
                std::fs::copy(&pictures, &beside).map_err(prepare)?;
            } else {
                link(&pictures, &beside).map_err(prepare)?;
                reads.push(pictures);
            }
        }
        inputs = inputs.with_subtitle(sub.position, at);
    }
    let args = mux_args_as(run.descriptor, &inputs, run.picks, run.format, part);
    let jail = jail(run.max_size, run.dir, &[], &reads);
    let ffmpeg = run.jobs.ffmpeg();
    let mut child = launch(ffmpeg, &jail, &args).map_err(|e| {
        RunError::Failed(format!(
            "ffmpeg could not be run: {}: {e}",
            ffmpeg.program().display()
        ))
    })?;
    let pid = child.id().unwrap_or(0);
    let duration_us = run
        .descriptor
        .duration
        .map(|d| d.as_micros() as u64)
        .filter(|&us| us > 0);
    let mut out = BufReader::new(child.stdout.take().expect("piped")).lines();
    let mut err = BufReader::new(child.stderr.take().expect("piped")).lines();
    let mut last = VecDeque::new();
    let (mut out_open, mut err_open) = (true, true);
    let status = loop {
        tokio::select! {
            status = child.wait() => break status,
            line = out.next_line(), if out_open => match line {
                Ok(Some(line)) => {
                    if let Some(us) = out_time_us(&line) {
                        progress(duration_us.map(|total| (us as f64 / total as f64).min(1.0)));
                    }
                }
                _ => out_open = false,
            },
            line = err.next_line(), if err_open => match line {
                Ok(Some(line)) => keep(&mut last, line),
                _ => err_open = false,
            },
            () = stopped(stop) => {
                signal(pid, libc::SIGKILL);
                let _ = child.wait().await;
                return Err(RunError::Stopped);
            }
        }
    };
    drain_stderr(&mut err, &mut last).await;
    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => {
            tracing::warn!(stderr = ?last, %status, "a download's file could not be written");
            Err(RunError::Failed(match last.back() {
                Some(line) => format!("the file could not be written: {line}"),
                None => format!("the file could not be written: ffmpeg ended: {status}"),
            }))
        }
        Err(e) => Err(RunError::Failed(format!(
            "ffmpeg could not be waited for: {e}"
        ))),
    }
}

/// `file`'s SHA-256 in hex, and its size, read on a blocking thread: a
/// film is gigabytes.
pub(super) async fn sha256(file: &Path) -> std::io::Result<(String, u64)> {
    let file = file.to_path_buf();
    tokio::task::spawn_blocking(move || {
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        let mut reader = std::fs::File::open(&file)?;
        let mut buf = vec![0u8; 1 << 20];
        let mut size = 0u64;
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hash.update(&buf[..n]);
            size += n as u64;
        }
        let hex: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
        Ok((hex, size))
    })
    .await
    .map_err(std::io::Error::other)?
}
