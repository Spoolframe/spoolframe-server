//! Subtitle sync (PX-03): a text subtitle made for another release of a
//! film, retimed to this one. Its cues are aligned ([`align`]) to a
//! reference timed to this file: a text subtitle inside it, in any
//! language ([`subtitle_reference`]), or else the speech in its audio
//! ([`speech`]). The subtitle's own file is then rewritten with the new
//! times ([`retime`]), its text untouched.
//!
//! The work on the film itself, decoding its audio or reading a track out
//! of it, is ffmpeg's, confined as every run of it is. What is done here,
//! reading cue times and aligning spans, runs on the caller's thread.

mod align;
mod speech;
mod text;

use std::path::Path;

pub use align::{Alignment, MIN_SCORE, align};
pub use speech::{SpeechDetector, TIMEOUT as SPEECH_TIMEOUT, speech};
pub use text::{Cue, MAX_CUES, MAX_TEXT_BYTES, Retiming, TextFormat, cues, retime, shift};

use crate::sandbox::Tool;

/// A stretch of time, ms from the start of the film.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Span {
    pub start_ms: i64,
    pub end_ms: i64,
}

impl Span {
    pub fn len(&self) -> i64 {
        (self.end_ms - self.start_ms).max(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl From<&Cue> for Span {
    fn from(cue: &Cue) -> Self {
        Self {
            start_ms: cue.start_ms,
            end_ms: cue.end_ms,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SyncError {
    /// The subtitle's (or the reference subtitle's) cues cannot be read.
    #[error("{0}")]
    Unreadable(String),
    /// Nothing to align to: no speech found, or a reference with no cues.
    #[error("no speech or subtitle to sync to was found in the file")]
    NoReference,
    /// ffmpeg did not run, or failed.
    #[error("{0}")]
    Tool(String),
    #[error("{0}")]
    Failed(String),
}

/// [`align`] on a thread of its own at idle CPU priority, as the speech
/// detector runs: seconds of work on a film that a playback's encode
/// never waits for.
pub async fn align_at_idle(
    reference: Vec<Span>,
    subtitle: Vec<Span>,
) -> Result<Alignment, SyncError> {
    let (done, aligned) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("subtitle-sync".into())
        .spawn(move || {
            speech::idle_thread();
            let _ = done.send(align(&reference, &subtitle));
        })
        .map_err(|e| SyncError::Failed(format!("cannot start the aligner: {e}")))?;
    aligned
        .await
        .map_err(|_| SyncError::Failed("the aligner failed".into()))?
}

/// The cue spans of `cues`.
pub fn spans(cues: &[Cue]) -> Vec<Span> {
    cues.iter().map(Span::from).collect()
}

/// The cues of text subtitle stream `index` of `media` (ffprobe's index),
/// as a reference: read out by `ffmpeg` as SubRip into `work`, a
/// directory of the server's own that the run may write, and parsed.
pub async fn subtitle_reference(
    ffmpeg: &Tool,
    media: &Path,
    index: u32,
    work: &Path,
) -> Result<Vec<Span>, SyncError> {
    let out = work.join("reference.srt");
    crate::ocr::convert_text(
        ffmpeg,
        media,
        index,
        spoolframe_types::SubtitleFormat::Srt,
        &out,
    )
    .await
    .map_err(|e| SyncError::Tool(e.to_string()))?;
    let bytes = tokio::fs::read(&out)
        .await
        .map_err(|e| SyncError::Tool(format!("cannot read the reference subtitle: {e}")))?;
    let cues = cues(&bytes, TextFormat::Srt).map_err(|_| SyncError::NoReference)?;
    Ok(spans(&cues))
}

#[cfg(test)]
mod tests;
