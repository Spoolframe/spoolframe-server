//! Converted playback (P2-03, P2-04, P2-05): ffmpeg jobs tied to playback
//! handles, and how their output is read.

pub mod args;
pub mod hls;
pub mod job;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spoolframe_media::encoders::Encoders;
use spoolframe_media::{MediaDescriptor, codecs};
use spoolframe_protocol::{
    AudioAction, ConvertedAudio, ConvertedSubtitle, ConvertedVideo, ManifestKind, PlaybackPlan,
    SubtitleAction,
};
use spoolframe_types::{Delivery, SubtitleFormat};

use self::args::{EncodeSpec, Output, ffmpeg_args};
use self::hls::Segments;
use self::job::{EncodeJob, JobError, Jobs, Slot, Written};
pub use self::job::{Encoding, EncodingGuard};

/// What the server allows encodes (`transcode { … }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobLimits {
    /// Encodes at once, copy-only ones too. One more is refused with
    /// `Busy`, never queued.
    pub max_jobs: usize,
    /// Where jobs write, in a directory of their own
    /// ([`job::JOBS_DIR`]). Only that is emptied at start.
    pub scratch_dir: PathBuf,
    /// The most all jobs may write together.
    pub scratch_limit_bytes: u64,
    /// How far an encode may run ahead of its reader before it is paused.
    pub read_ahead: Duration,
    /// How long an encode that is not held back may go without writing or
    /// reporting more encoded time before it is ended as stalled
    /// ([`job::STALLED_AFTER`]; tests shorten it).
    pub stalled_after: Duration,
}

/// A playback's conversion: what to encode, and the encode running now.
/// A seek replaces the job; dropping the conversion (the handle ended)
/// stops it and removes everything it wrote.
///
/// The job that replaces another is started first, in the same slot, and
/// only then is the old one stopped: a replacement that cannot start
/// leaves the old one running, and the playback as it was.
#[derive(Debug)]
pub struct Conversion {
    jobs: Arc<Jobs>,
    /// `<jobs>/<handle>`: the links to the input, and one directory per
    /// epoch.
    dir: PathBuf,
    input: PathBuf,
    sidecar: Option<PathBuf>,
    descriptor: Arc<MediaDescriptor>,
    pub delivery: Delivery,
    video: Option<ConvertedVideo>,
    audio: Option<ConvertedAudio>,
    subtitle: Option<ConvertedSubtitle>,
    encoders: Arc<Encoders>,
    vaapi_device: PathBuf,
    /// For HLS: where the segments are cut (P2-05).
    segments: Option<Segments>,
    current: Mutex<Arc<EncodeJob>>,
}

/// How far past what is written a segment may be asked for before the
/// encode restarts there rather than the reader waiting.
const SEGMENTS_AHEAD: u32 = 3;

/// What a conversion is made from.
#[derive(Debug, Clone)]
pub struct ConversionSource<'a> {
    pub handle: &'a str,
    pub path: &'a Path,
    pub descriptor: Arc<MediaDescriptor>,
    pub plan: &'a PlaybackPlan,
    pub encoders: Arc<Encoders>,
    pub vaapi_device: &'a Path,
    /// For HLS of a copied video: the source's keyframes, its only cuts.
    pub keyframes: Option<Arc<Vec<u64>>>,
    /// The epoch of its first encode: 0 for a new playback; one past the
    /// last for a conversion that replaces another (a subtitle change), so
    /// reads of the old one are told to start over.
    pub first_epoch: u32,
    /// The slot of the conversion it replaces, which still runs: the two
    /// share it, so a subtitle change needs no second one.
    pub slot: Option<Slot>,
}

impl Conversion {
    /// Start converting `source` at `start`, as epoch `source.first_epoch`.
    pub fn start(
        jobs: Arc<Jobs>,
        source: ConversionSource<'_>,
        start: Option<Duration>,
    ) -> Result<Arc<Self>, JobError> {
        let PlaybackPlan::Converted {
            delivery,
            video,
            audio,
            subtitle,
            ..
        } = source.plan
        else {
            return Err(JobError::Prepare("not a converted plan".into()));
        };
        let prepare = |e: std::io::Error| JobError::Prepare(e.to_string());
        // A replacing conversion gets a directory of its own: the one it
        // replaces removes its own when the last read of it ends.
        let dir = match source.first_epoch {
            0 => jobs.dir().join(source.handle),
            epoch => jobs.dir().join(format!("{}~{epoch}", source.handle)),
        };
        std::fs::create_dir_all(&dir).map_err(prepare)?;
        // Plain names for ffmpeg's filters (see `args::filter_path`).
        let input = dir.join("input");
        std::os::unix::fs::symlink(source.path, &input).map_err(prepare)?;
        let sidecar = match subtitle.and_then(|s| source.descriptor.subtitles.get(s.index as usize))
        {
            Some(spoolframe_media::SubtitleStream {
                source: spoolframe_media::SubtitleSource::External { path },
                codec,
                ..
            }) if codecs::subtitle_format(codec) == SubtitleFormat::VobSub => {
                // VobSub is two files: the `.idx`, and the `.sub` with the
                // pictures, which ffmpeg finds by the `.idx`'s name. Both
                // are linked, under names that match.
                let link = dir.join("sidecar.idx");
                std::os::unix::fs::symlink(path, &link).map_err(prepare)?;
                std::os::unix::fs::symlink(path.with_extension("sub"), dir.join("sidecar.sub"))
                    .map_err(prepare)?;
                Some(link)
            }
            Some(spoolframe_media::SubtitleStream {
                source: spoolframe_media::SubtitleSource::External { path },
                ..
            }) => {
                let link = dir.join("sidecar");
                std::os::unix::fs::symlink(path, &link).map_err(prepare)?;
                Some(link)
            }
            _ => None,
        };
        let segments = (*delivery == Delivery::Hls).then(|| {
            let total = source
                .descriptor
                .duration
                .map_or(0, |d| d.as_millis() as u64);
            match (video, &source.keyframes) {
                (Some(ConvertedVideo::Copy { .. }), Some(keyframes)) => {
                    Segments::at_keyframes(keyframes, total)
                }
                _ => Segments::grid(total),
            }
        });
        // An HLS start begins at the segment the position falls in.
        let (start, first_segment) = playback_seek(segments.as_ref(), start, *video);
        let job = spawn(
            &jobs,
            &dir,
            &input,
            sidecar.as_deref(),
            &source.descriptor,
            (*delivery, *video, *audio, *subtitle),
            &source.encoders,
            source.vaapi_device,
            source.first_epoch,
            start,
            first_segment,
            source.slot,
        );
        let job = match job {
            Ok(job) => job,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(e);
            }
        };
        Ok(Arc::new(Self {
            jobs,
            dir,
            input,
            sidecar,
            descriptor: source.descriptor,
            delivery: *delivery,
            video: *video,
            audio: *audio,
            subtitle: *subtitle,
            encoders: source.encoders,
            vaapi_device: source.vaapi_device.to_path_buf(),
            segments,
            current: Mutex::new(job),
        }))
    }

    /// For HLS, the segments.
    pub fn segments(&self) -> Option<&Segments> {
        self.segments.as_ref()
    }

    /// For HLS, a playlist.
    pub fn manifest(&self, kind: ManifestKind) -> Option<String> {
        let segments = self.segments.as_ref()?;
        let rendition = self
            .subtitle_rendition()
            .and_then(|index| self.descriptor.subtitles.get(index as usize));
        Some(match kind {
            ManifestKind::Media => segments.media_playlist(),
            ManifestKind::Subtitles => {
                rendition?;
                segments.subtitle_playlist()
            }
            _ => hls::multivariant_playlist(&self.descriptor, self.video, self.audio, rendition),
        })
    }

    /// For HLS, the subtitle track that goes as a WebVTT rendition (P2-07):
    /// any text the plan keeps, copied, converted or read from pictures,
    /// since segments carry no text.
    pub fn subtitle_rendition(&self) -> Option<u32> {
        let subtitle = self.subtitle?;
        let text = matches!(
            subtitle.action,
            SubtitleAction::Copy { .. }
                | SubtitleAction::Convert { .. }
                | SubtitleAction::Ocr { .. }
        );
        (self.delivery == Delivery::Hls && text).then_some(subtitle.index)
    }

    /// For HLS, where segment `n` (from 1) starts and ends.
    pub fn segment_span(&self, n: u32) -> Option<(u64, u64)> {
        let segments = self.segments.as_ref()?;
        Some((segments.start(n)?, segments.end(n)?))
    }

    /// The job that writes HLS segment `n`: the running one, or, if `n` is
    /// behind where it started (or behind what it has kept, with the
    /// scratch space full) or far past what it has written, a new one
    /// started at `n`.
    pub fn job_for_segment(&self, n: u32) -> Result<Arc<EncodeJob>, JobError> {
        let segments = self
            .segments
            .as_ref()
            .ok_or_else(|| JobError::Prepare("not an HLS playback".into()))?;
        let mut current = self.current.lock().expect("never poisoned");
        let written = current.progress().borrow().last_segment;
        let head = written.unwrap_or(current.first_segment.saturating_sub(1));
        if n >= current.first_kept() && n <= head + SEGMENTS_AHEAD {
            return Ok(current.clone());
        }
        let start = segment_seek(segments, n, self.video);
        let epoch = current.epoch + 1;
        let job = spawn(
            &self.jobs,
            &self.dir,
            &self.input,
            self.sidecar.as_deref(),
            &self.descriptor,
            (self.delivery, self.video, self.audio, self.subtitle),
            &self.encoders,
            &self.vaapi_device,
            epoch,
            start,
            n,
            current.slot(),
        )?;
        current.stop();
        *current = job;
        tracing::debug!(segment = n, epoch, "HLS encode restarted at a segment");
        Ok(current.clone())
    }

    /// The job running now.
    pub fn job(&self) -> Arc<EncodeJob> {
        self.current.lock().expect("never poisoned").clone()
    }

    /// Seek using the same timeline and keyframe alignment as a new
    /// playback. HLS segment numbers always refer to the full film.
    pub fn seek(&self, position: Duration) -> Result<u32, JobError> {
        let (start, number) = playback_seek(self.segments.as_ref(), Some(position), self.video);
        self.restart(start.unwrap_or(Duration::ZERO), number)
    }

    /// Start the next epoch at `start` and stop the job (progressive; HLS
    /// restarts at a segment, [`Conversion::job_for_segment`]). Returns the
    /// new epoch.
    pub fn restart(&self, start: Duration, start_number: u32) -> Result<u32, JobError> {
        let mut current = self.current.lock().expect("never poisoned");
        let epoch = current.epoch + 1;
        let job = spawn(
            &self.jobs,
            &self.dir,
            &self.input,
            self.sidecar.as_deref(),
            &self.descriptor,
            (self.delivery, self.video, self.audio, self.subtitle),
            &self.encoders,
            &self.vaapi_device,
            epoch,
            Some(start),
            start_number,
            current.slot(),
        )?;
        current.stop();
        *current = job;
        Ok(epoch)
    }
}

impl Drop for Conversion {
    fn drop(&mut self) {
        self.current.lock().expect("never poisoned").stop();
        if let Err(e) = std::fs::remove_dir_all(&self.dir)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(dir = %self.dir.display(), error = %e, "cannot remove a conversion's files");
        }
    }
}

fn playback_seek(
    segments: Option<&Segments>,
    start: Option<Duration>,
    video: Option<ConvertedVideo>,
) -> (Option<Duration>, u32) {
    match (segments, start) {
        (Some(segments), Some(at)) => {
            let n = segments
                .starts_ms
                .partition_point(|&s| u128::from(s) <= at.as_millis())
                .max(1) as u32;
            (segment_seek(segments, n, video), n)
        }
        (Some(_), None) => (None, 1),
        (None, start) => (start, 0),
    }
}

/// Where to seek for segment `n`.
///
/// An encoded video seeks exactly: decoding lands on any frame. A copied
/// one can only start at a keyframe, and ffmpeg's seek in a copied
/// Matroska file lands on the keyframe some 0.2 s before where it is
/// aimed, so aiming at the keyframe itself starts a GOP early and every
/// segment number after it is off by one. Aiming halfway to the next
/// keyframe lands on this one for any GOP over 0.4 s.
fn segment_seek(segments: &Segments, n: u32, video: Option<ConvertedVideo>) -> Option<Duration> {
    let start = segments.start(n)?;
    let ms = match video {
        Some(ConvertedVideo::Copy { .. }) if start > 0 => {
            let next = segments.start(n + 1).unwrap_or(segments.total_ms);
            start + next.saturating_sub(start) / 2
        }
        _ => start,
    };
    (ms > 0).then(|| Duration::from_millis(ms))
}

type Parts = (
    Delivery,
    Option<ConvertedVideo>,
    Option<ConvertedAudio>,
    Option<ConvertedSubtitle>,
);

#[expect(
    clippy::too_many_arguments,
    reason = "the call sites name each part; a struct would only repeat them"
)]
fn spawn(
    jobs: &Jobs,
    dir: &Path,
    input: &Path,
    sidecar: Option<&Path>,
    descriptor: &MediaDescriptor,
    (delivery, video, audio, subtitle): Parts,
    encoders: &Encoders,
    vaapi_device: &Path,
    epoch: u32,
    start: Option<Duration>,
    start_number: u32,
    slot: Option<Slot>,
) -> Result<Arc<EncodeJob>, JobError> {
    let job_dir = dir.join(epoch.to_string());
    let (output, written) = match delivery {
        Delivery::Hls => (
            Output::Hls {
                dir: job_dir.clone(),
                start_number,
            },
            Written::Hls,
        ),
        _ => {
            let file = job_dir.join("out.mkv");
            (
                Output::Progressive { file: file.clone() },
                Written::Progressive { file },
            )
        }
    };
    let args = ffmpeg_args(&EncodeSpec {
        input,
        sidecar,
        descriptor,
        video,
        audio,
        subtitle,
        encoders,
        vaapi_device,
        start,
        output,
    })
    .map_err(JobError::Prepare)?;
    let first_segment = if delivery == Delivery::Hls {
        start_number
    } else {
        0
    };
    let start_ms = start.map_or(0, |s| s.as_millis() as u64);
    jobs.start(
        job_dir,
        args,
        written,
        encodes(video, audio),
        epoch,
        first_segment,
        start_ms,
        slot,
    )
}

/// Whether a conversion decodes and encodes a stream again, rather than
/// only copying them into another container: what holds back frame
/// previews made ahead (P3-13). A burned-in subtitle is an encoded video.
fn encodes(video: Option<ConvertedVideo>, audio: Option<ConvertedAudio>) -> bool {
    matches!(video, Some(ConvertedVideo::Encode { .. }))
        || matches!(
            audio,
            Some(ConvertedAudio {
                action: AudioAction::Encode { .. },
                ..
            })
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_stream_encoded_again_is_encoding() {
        use spoolframe_types::{AudioCodec, VideoCodec};
        let copy_video = Some(ConvertedVideo::Copy {
            codec: VideoCodec::H264,
        });
        let encode_video = Some(ConvertedVideo::Encode {
            codec: VideoCodec::H264,
            height: 720,
            max_bitrate: None,
            tone_map: false,
        });
        let audio = |action| Some(ConvertedAudio { index: 1, action });
        let copy_audio = audio(AudioAction::Copy {
            codec: AudioCodec::Aac,
        });
        // A remux: into another container as it is.
        assert!(!encodes(copy_video, copy_audio));
        assert!(!encodes(copy_video, None));
        assert!(encodes(encode_video, copy_audio));
        let encode_audio = audio(AudioAction::Encode {
            codec: AudioCodec::Aac,
            channels: 2,
            bitrate: 192_000,
        });
        assert!(encodes(copy_video, encode_audio));
        assert!(encodes(None, encode_audio));
    }

    #[test]
    fn a_copied_restart_aims_halfway_into_the_segment() {
        let segments = Segments::at_keyframes(&[0, 2000, 4000], 6000);
        let copy = Some(ConvertedVideo::Copy {
            codec: spoolframe_types::VideoCodec::H264,
        });
        assert_eq!(
            segment_seek(&segments, 2, copy),
            Some(Duration::from_millis(3000))
        );
        assert_eq!(
            segment_seek(&segments, 3, copy),
            Some(Duration::from_millis(5000))
        );
        assert_eq!(segment_seek(&segments, 1, copy), None);
        // Encoded: exactly at the start.
        let grid = Segments::grid(30_000);
        assert_eq!(segment_seek(&grid, 2, None), Some(Duration::from_secs(6)));
    }
}
