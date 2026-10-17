//! Where people speak in a film (PX-03): the reference a subtitle is
//! synced to when the file has no text subtitle of its own. ffmpeg
//! decodes one audio track to 16 kHz mono, confined as every run of it
//! is and at idle CPU and I/O priority, on one thread, so a playback's
//! encode never waits on it; the samples come through a pipe, and a
//! voice activity detector (earshot, pure Rust) reads them on a thread of
//! its own, at idle priority too. Nothing is written to disk.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt as _;

use super::{Span, SyncError};
use crate::sandbox::{Jail, Limits, Memory, Tool, low_priority};

/// The sample rate the detector takes.
const RATE: u64 = 16_000;

/// The detector's frame: 256 samples, 16 ms.
const FRAME: usize = 256;

/// A frame scored at least this is speech: earshot's own suggestion.
const VOICED: f32 = 0.5;

/// Silence this short between two runs of speech is a breath, not a gap:
/// the runs are one span. In frames.
const BRIDGE: u64 = 3;

/// A run of speech shorter than this is a click or a cough, not a line.
/// In frames.
const SHORTEST: u64 = 3;

/// The longest a decode may take: a long film at idle priority on a busy
/// machine.
pub const TIMEOUT: Duration = Duration::from_secs(4 * 60 * 60);

/// What a decode of one audio track may use: one thread, a few buffers;
/// it writes no file (its samples go to a pipe).
const LIMITS: Limits = Limits {
    memory: Memory::fixed(1 << 30),
    cpu_secs: 4 * 60 * 60,
    file_size: 1 << 20,
};

/// The most of ffmpeg's stderr kept, for its last line.
const MAX_STDERR: usize = 16 << 10;

/// Speech found in a stream of samples, fed in as they come.
pub struct SpeechDetector {
    detector: Box<earshot::Detector>,
    /// Samples short of a frame, kept for the next feed.
    pending: Vec<i16>,
    /// A byte short of a sample, kept for the next feed.
    odd: Option<u8>,
    frame: u64,
    /// The run of speech going on: its first and last voiced frames.
    run: Option<(u64, u64)>,
    spans: Vec<Span>,
}

impl Default for SpeechDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl SpeechDetector {
    pub fn new() -> Self {
        Self {
            detector: earshot::Detector::default_boxed(),
            pending: Vec::with_capacity(FRAME),
            odd: None,
            frame: 0,
            run: None,
            spans: Vec::new(),
        }
    }

    /// Samples as ffmpeg writes them: 16-bit little-endian, mono, at
    /// 16 kHz. A sample cut between two feeds is joined.
    pub fn push_bytes(&mut self, mut bytes: &[u8]) {
        let mut samples = Vec::with_capacity(bytes.len() / 2 + 1);
        if let (Some(low), Some((&high, rest))) = (self.odd, bytes.split_first()) {
            samples.push(i16::from_le_bytes([low, high]));
            self.odd = None;
            bytes = rest;
        }
        let (pairs, rest) = bytes.as_chunks::<2>();
        samples.extend(pairs.iter().map(|p| i16::from_le_bytes(*p)));
        if let [last] = rest {
            self.odd = Some(*last);
        }
        self.push(&samples);
    }

    pub fn push(&mut self, mut samples: &[i16]) {
        while !samples.is_empty() {
            let take = (FRAME - self.pending.len()).min(samples.len());
            self.pending.extend_from_slice(&samples[..take]);
            samples = &samples[take..];
            if self.pending.len() == FRAME {
                let voiced = self.detector.predict_i16(&self.pending) >= VOICED;
                self.pending.clear();
                self.frame(voiced);
            }
        }
    }

    fn frame(&mut self, voiced: bool) {
        let at = self.frame;
        self.frame += 1;
        match (self.run, voiced) {
            (Some((first, last)), true) if at - last <= BRIDGE => self.run = Some((first, at)),
            (Some(_), true) => {
                self.close();
                self.run = Some((at, at));
            }
            (None, true) => self.run = Some((at, at)),
            (Some((_, last)), false) if at - last > BRIDGE => self.close(),
            _ => {}
        }
    }

    fn close(&mut self) {
        if let Some((first, last)) = self.run.take()
            && last + 1 - first >= SHORTEST
        {
            let ms = |frame: u64| (frame * FRAME as u64 * 1000 / RATE) as i64;
            self.spans.push(Span {
                start_ms: ms(first),
                end_ms: ms(last + 1),
            });
        }
    }

    /// The spans of speech, in order.
    pub fn finish(mut self) -> Vec<Span> {
        self.close();
        self.spans
    }
}

/// The speech in audio stream `index` of `media` (ffprobe's index), as
/// `ffmpeg` decodes it at idle priority.
pub async fn speech(ffmpeg: &Tool, media: &Path, index: u32) -> Result<Vec<Span>, SyncError> {
    let program = ffmpeg.program().display().to_string();
    let spawn_error = |e: std::io::Error| SyncError::Tool(format!("cannot run {program}: {e}"));
    let mut command = ffmpeg
        .command_off_thread(Jail::new(LIMITS).media(media))
        .await
        .map_err(spawn_error)?;
    low_priority(&mut command);
    command
        .args([
            "-hide_banner",
            "-nostdin",
            "-v",
            "error",
            "-threads",
            "1",
            "-i",
        ])
        .arg(media)
        .args(["-map", &format!("0:{index}"), "-vn", "-sn", "-dn"])
        .args([
            "-ac",
            "1",
            "-ar",
            &RATE.to_string(),
            "-f",
            "s16le",
            "pipe:1",
        ]);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(spawn_error)?;
    let mut stdout = child.stdout.take().expect("piped");
    let errors = tokio::spawn(crate::capped::read_some(child.stderr.take(), MAX_STDERR));
    // A few chunks in flight: ffmpeg waits on the pipe when the detector
    // falls behind, rather than the server holding the film's audio.
    let (chunks, mut received) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let (done, spans) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("speech".into())
        .spawn(move || {
            idle_thread();
            let mut detector = SpeechDetector::new();
            while let Some(chunk) = received.blocking_recv() {
                detector.push_bytes(&chunk);
            }
            let _ = done.send(detector.finish());
        })
        .map_err(|e| SyncError::Failed(format!("cannot start the speech detector: {e}")))?;
    let decoded = async {
        loop {
            let mut chunk = vec![0; 64 << 10];
            let n = stdout.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            chunk.truncate(n);
            // The detector's thread only ends early if it panicked.
            if chunks.send(chunk).await.is_err() {
                return Err(std::io::Error::other("the speech detector stopped"));
            }
        }
        child.wait().await
    };
    let status = match tokio::time::timeout(TIMEOUT, decoded).await {
        Ok(status) => status.map_err(|e| SyncError::Tool(format!("{program}: {e}")))?,
        Err(_) => {
            return Err(SyncError::Tool(format!(
                "{program} took longer than {} s",
                TIMEOUT.as_secs()
            )));
        }
    };
    drop(chunks);
    if !status.success() {
        let stderr = errors.await.unwrap_or_default();
        return Err(SyncError::Tool(format!(
            "{program} failed: {}",
            crate::ocr::last_line(&stderr)
        )));
    }
    let spans = spans
        .await
        .map_err(|_| SyncError::Failed("the speech detector failed".into()))?;
    if spans.is_empty() {
        return Err(SyncError::NoReference);
    }
    Ok(spans)
}

/// This thread at the lowest CPU priority there is (nice 19): on Linux a
/// thread's nice value is its own, so the server's other threads keep
/// theirs. A failure leaves it as it was.
pub(super) fn idle_thread() {
    // SAFETY: gettid(2) and setpriority(2) on this thread alone; no
    // memory is passed.
    unsafe {
        let tid = libc::gettid();
        libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, 19);
    }
}
