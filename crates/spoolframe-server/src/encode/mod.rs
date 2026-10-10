//! Converted playback (P2-03, P2-04, P2-05): ffmpeg jobs tied to playback
//! handles, and how their output is read.

pub mod args;
pub mod hls;
pub mod job;
pub mod pick;

use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use spoolframe_media::encoders::Hardware;
use spoolframe_media::sandbox::GpuUse;
use spoolframe_media::{MediaDescriptor, codecs};
use spoolframe_protocol::{
    AudioAction, ConvertedAudio, ConvertedSubtitle, ConvertedVideo, ManifestKind, PlaybackPlan,
    Rung, SubtitleAction,
};
use spoolframe_types::{Delivery, SubtitleFormat};

use self::args::{Decode, EncodeSpec, Output, decode, ffmpeg_args};
pub use self::args::{Decoder, FilterOn, Made};
use self::hls::Segments;
pub use self::job::{BudgetUse, Encoding, EncodingGuard, Load, Measured, Units};
use self::job::{EncodeJob, Fallback, JobError, Jobs, Run, Slot, Written};
use self::pick::RunOn;
use crate::transcode::{Detected, Gpu};

/// What the server allows encodes (`transcode { … }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobLimits {
    /// The encode budget (`transcode.max_jobs`): what the conversions
    /// running may weigh together ([`Load::weight`]). One that does not
    /// fit is refused with `Busy`, never queued.
    pub budget: Units,
    /// Remuxes at once (`transcode.max_remux`): they weigh nothing, and
    /// this bounds their file descriptors and disk.
    pub max_remux: usize,
    /// GPU video encodes at once (`transcode.max_gpu_jobs`), within the
    /// budget too.
    pub max_gpu_jobs: usize,
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
    /// Scale and burn in pictures on the GPU where the driver can
    /// (`transcode.gpu_filters`), rather than on the CPU; and under
    /// `tone_map = "auto"`, tone map there too.
    pub gpu_filters: bool,
    /// How HDR is tone mapped (`transcode.tone_map`, PGPU-04).
    pub tone_map: spoolframe_media::encoders::ToneMapSetting,
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
    /// The film itself, which a failed GPU decode is remembered by.
    file: PathBuf,
    input: PathBuf,
    sidecar: Option<PathBuf>,
    descriptor: Arc<MediaDescriptor>,
    pub delivery: Delivery,
    video: Option<ConvertedVideo>,
    audio: Option<ConvertedAudio>,
    subtitle: Option<ConvertedSubtitle>,
    /// The GPU its runs go to, picked once at its start (PGPU-05): a seek
    /// or another rung stays on it, as on one encoder (PGPU-02).
    run_on: RunOn,
    /// For HLS: where the segments are cut (P2-05).
    segments: Option<Segments>,
    /// For HLS under Auto (PX-21): the encodes a player switches between,
    /// highest first. Empty for one encode, `video`.
    ladder: Vec<Rung>,
    /// The rung it started on, listed first for the player.
    first_rung: u32,
    current: Mutex<Current>,
    /// Ended by an admin (P3-15): no encode starts again for it.
    ended: std::sync::atomic::AtomicBool,
}

/// The job running now, which rung of the ladder it encodes (0 for one
/// encode), and the last job of another rung, stopped (PX-21): what it
/// wrote stays readable, so a player that fetches a segment it has from
/// the rung it left gets it, and no restart. Only one is kept: each one's
/// segments count against the scratch space, and only the running job's
/// are reclaimed.
#[derive(Debug)]
struct Current {
    job: Arc<EncodeJob>,
    rung: u32,
    left: Option<(u32, Arc<EncodeJob>)>,
}

/// Why a read past what an ended conversion wrote fails (P3-15).
pub const ENDED: &str = "the conversion was ended on the server";

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
    /// The GPUs detection found, and what they and the CPU encode.
    pub gpus: Arc<Detected>,
    /// For HLS of a copied video: the source's keyframes, its only cuts.
    pub keyframes: Option<Arc<Vec<u64>>>,
    /// The epoch of its first encode: 0 for a new playback; one past the
    /// last for a conversion that replaces another (a subtitle change), so
    /// reads of the old one are told to start over.
    pub first_epoch: u32,
    /// The slot of the conversion it replaces, which still runs: the two
    /// share it, so a subtitle change needs no second one.
    pub slot: Option<Slot>,
    /// For a ladder (PX-21), the rung its first encode is: the one the
    /// player is told to start on.
    pub start_rung: u32,
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
            ladder,
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
        // What fails from here leaves no directory behind (B-13), as a
        // failed spawn already did not.
        let prepared = (|| -> Result<_, JobError> {
            // Plain names for ffmpeg's filters (see `args::filter_path`).
            let input = dir.join("input");
            std::os::unix::fs::symlink(source.path, &input).map_err(prepare)?;
            let sidecar = match subtitle
                .and_then(|s| source.descriptor.subtitles.get(s.index as usize))
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
            let segments = match delivery {
                Delivery::Hls => {
                    let total =
                        hls::hls_total_ms(source.descriptor.duration).map_err(JobError::Prepare)?;
                    Some(match (video, &source.keyframes) {
                        (Some(ConvertedVideo::Copy { .. }), Some(keyframes)) => {
                            Segments::at_keyframes(keyframes, total)
                        }
                        (Some(ConvertedVideo::Encode { .. }), _) => Segments::encoded(total),
                        _ => Segments::grid(total),
                    })
                }
                _ => None,
            };
            Ok((input, sidecar, segments))
        })();
        let (input, sidecar, segments) = match prepared {
            Ok(prepared) => prepared,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(e);
            }
        };
        // An HLS start begins at the segment the position falls in.
        let (start, first_segment) = playback_seek(segments.as_ref(), start, *video);
        let start_rung = if ladder.is_empty() {
            0
        } else {
            source.start_rung.min(ladder.len() as u32 - 1)
        };
        let first_video = rung_video(*video, ladder, start_rung);
        let run_on = run_on(
            &jobs,
            &source,
            EncodeSpec {
                input: &input,
                sidecar: sidecar.as_deref(),
                descriptor: &source.descriptor,
                video: first_video,
                audio: *audio,
                subtitle: *subtitle,
                encoders: &source.gpus.software,
                vaapi_device: Path::new(""),
                hardware_decode: !jobs.hardware_decode_failed(source.path),
                gpu_filters: jobs.limits().gpu_filters,
                tone_map: jobs.limits().tone_map,
                start: None,
                output: Output::Progressive {
                    file: PathBuf::new(),
                },
            },
        );
        let job = spawn(
            &jobs,
            &dir,
            source.path,
            &input,
            sidecar.as_deref(),
            &source.descriptor,
            (*delivery, first_video, *audio, *subtitle),
            &run_on,
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
            file: source.path.to_path_buf(),
            input,
            sidecar,
            descriptor: source.descriptor,
            delivery: *delivery,
            video: *video,
            audio: *audio,
            subtitle: *subtitle,
            run_on,
            segments,
            ladder: ladder.clone(),
            first_rung: start_rung,
            current: Mutex::new(Current {
                job,
                rung: start_rung,
                left: None,
            }),
            ended: Default::default(),
        }))
    }

    /// End the conversion where it is (P3-15's End conversion): ffmpeg
    /// stops and lets go of its slot, what it wrote stays readable, and no
    /// read or seek starts it again. The playback goes on until it runs out
    /// of what was written.
    pub fn end(&self) {
        let current = self.current.lock();
        self.ended.store(true, std::sync::atomic::Ordering::SeqCst);
        current.job.end();
    }

    /// Whether [`Conversion::end`] ended it.
    pub fn ended(&self) -> bool {
        self.ended.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Whether it decodes and encodes a stream again, rather than only
    /// copying them into another container (P3-14's plans).
    pub fn encodes(&self) -> bool {
        encodes(self.video, self.audio)
    }

    /// What it weighs against the encode budget: its running job's load.
    pub fn load(&self) -> Load {
        self.current.lock().job.load
    }

    /// For a ladder (PX-21), its rungs; empty for one encode.
    pub fn ladder(&self) -> &[Rung] {
        &self.ladder
    }

    /// The rung being encoded now: 0 for one encode.
    pub fn rung(&self) -> u32 {
        self.current.lock().rung
    }

    /// The rungs a player may ask for: 1 for one encode.
    fn rungs(&self) -> u32 {
        self.ladder.len().max(1) as u32
    }

    /// For HLS, the segments.
    pub fn segments(&self) -> Option<&Segments> {
        self.segments.as_ref()
    }

    /// For HLS, a playlist; a media playlist of `variant`, a ladder's
    /// rung (PX-21), is the same for every rung: they share the grid.
    pub fn manifest(&self, kind: ManifestKind, variant: u32) -> Option<String> {
        let segments = self.segments.as_ref()?;
        if kind == ManifestKind::Media && variant >= self.rungs() {
            return None;
        }
        let rendition = self
            .subtitle_rendition()
            .and_then(|index| self.descriptor.subtitles.get(index as usize));
        Some(match kind {
            ManifestKind::Media => segments.media_playlist(),
            ManifestKind::Subtitles => {
                rendition?;
                segments.subtitle_playlist()
            }
            _ if self.ladder.is_empty() => {
                hls::multivariant_playlist(&self.descriptor, self.video, self.audio, rendition)
            }
            // The rung the conversion started on first, as the player
            // starts on the first; then the rest, highest first.
            _ => {
                let first = self.first_rung();
                let rungs: Vec<(u32, ConvertedVideo)> = std::iter::once(first)
                    .chain((0..self.rungs()).filter(|&n| n != first))
                    .filter_map(|n| Some((n, rung_video(self.video, &self.ladder, n)?)))
                    .collect();
                hls::ladder_playlist(&self.descriptor, &rungs, self.audio, rendition)
            }
        })
    }

    /// The rung the conversion started on: what the player is told to
    /// start with.
    fn first_rung(&self) -> u32 {
        self.first_rung
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

    /// The job that writes HLS segment `n` of `rung` (0 for one encode):
    /// the running one, or, if `n` is behind where it started (or behind
    /// what it has kept, with the scratch space full) or far past what it
    /// has written, a new one started at `n`.
    ///
    /// Another rung than the running one (PX-21) is the player switching:
    /// the job that left that rung serves `n` if it wrote it, and
    /// otherwise the encode moves to that rung at `n`, the running job
    /// kept as the one left, so its segments stay readable.
    pub fn job_for_segment(&self, n: u32, rung: u32) -> Result<Arc<EncodeJob>, JobError> {
        let segments = self
            .segments
            .as_ref()
            .ok_or_else(|| JobError::Prepare("not an HLS playback".into()))?;
        if rung >= self.rungs() {
            return Err(JobError::Prepare(format!(
                "no rung {rung} in this playback"
            )));
        }
        let mut current = self.current.lock();
        if rung != current.rung {
            if let Some((left, job)) = &current.left
                && *left == rung
                && job.has_segment(n)
            {
                return Ok(job.clone());
            }
            if self.ended() {
                return Err(JobError::Prepare(ENDED.into()));
            }
            return self.switch(&mut current, rung, n, segments);
        }
        let job = &current.job;
        let written = job.written();
        let head = written.unwrap_or(job.first_segment.saturating_sub(1));
        if self.ended() {
            // Only what it wrote: nothing more is coming.
            return if n >= job.first_kept() && written.is_some_and(|w| n <= w) {
                Ok(job.clone())
            } else {
                Err(JobError::Prepare(ENDED.into()))
            };
        }
        // A run whose GPU decode failed past its first segment has only
        // what it wrote: past that, a run decoding in software goes on.
        let failed_on_gpu = job.decodes_in_hardware()
            && matches!(job.progress().borrow().done, Some(Err(_)))
            && written.is_none_or(|w| n > w);
        if n >= job.first_kept() && n <= head + SEGMENTS_AHEAD && !failed_on_gpu {
            return Ok(job.clone());
        }
        let (start, from) = run_at(segments, n, self.video);
        let epoch = job.epoch + 1;
        let next = spawn(
            &self.jobs,
            &self.dir,
            &self.file,
            &self.input,
            self.sidecar.as_deref(),
            &self.descriptor,
            self.parts(rung),
            &self.run_on,
            epoch,
            start,
            from,
            job.slot(),
        )?;
        current.job.stop();
        current.job = next;
        tracing::debug!(
            segment = n,
            from,
            epoch,
            "HLS encode restarted at a segment"
        );
        Ok(current.job.clone())
    }

    /// The job whose init segment `rung` plays from: the running one, the
    /// one that left that rung, or, for a rung neither is, the encode
    /// moved to it just past what is written: where a player switching
    /// to it asks next.
    pub fn job_for_init(&self, rung: u32) -> Result<Arc<EncodeJob>, JobError> {
        let segments = self
            .segments
            .as_ref()
            .ok_or_else(|| JobError::Prepare("not an HLS playback".into()))?;
        if rung >= self.rungs() {
            return Err(JobError::Prepare(format!(
                "no rung {rung} in this playback"
            )));
        }
        let mut current = self.current.lock();
        if rung == current.rung {
            return Ok(current.job.clone());
        }
        // The one left, once it wrote a segment: its init was finished then.
        if let Some((left, job)) = &current.left
            && *left == rung
            && job.written().is_some()
        {
            return Ok(job.clone());
        }
        if self.ended() {
            return Err(JobError::Prepare(ENDED.into()));
        }
        let next = current
            .job
            .written()
            .map_or(current.job.first_segment, |w| w + 1)
            .min(segments.count())
            .max(1);
        self.switch(&mut current, rung, next, segments)
    }

    /// Move the encode to `rung` at segment `n`: started first in the
    /// running job's slot, then the running job stopped and kept as the
    /// one left (the one left before it goes, its files with it).
    fn switch(
        &self,
        current: &mut Current,
        rung: u32,
        n: u32,
        segments: &Segments,
    ) -> Result<Arc<EncodeJob>, JobError> {
        let (start, from) = run_at(segments, n, self.video);
        let epoch = current.job.epoch + 1;
        let next = spawn(
            &self.jobs,
            &self.dir,
            &self.file,
            &self.input,
            self.sidecar.as_deref(),
            &self.descriptor,
            self.parts(rung),
            &self.run_on,
            epoch,
            start,
            from,
            current.job.slot(),
        )?;
        let left = std::mem::replace(&mut current.job, next);
        left.stop();
        tracing::debug!(
            segment = n,
            epoch,
            from = current.rung,
            to = rung,
            "HLS encode moved to another rung"
        );
        current.left = Some((current.rung, left));
        current.rung = rung;
        Ok(current.job.clone())
    }

    /// What `rung`'s encode is made of.
    fn parts(&self, rung: u32) -> Parts {
        (
            self.delivery,
            rung_video(self.video, &self.ladder, rung),
            self.audio,
            self.subtitle,
        )
    }

    /// The job running now.
    pub fn job(&self) -> Arc<EncodeJob> {
        self.current.lock().job.clone()
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
        let mut current = self.current.lock();
        if self.ended() {
            return Err(JobError::Prepare(ENDED.into()));
        }
        let epoch = current.job.epoch + 1;
        let job = spawn(
            &self.jobs,
            &self.dir,
            &self.file,
            &self.input,
            self.sidecar.as_deref(),
            &self.descriptor,
            self.parts(current.rung),
            &self.run_on,
            epoch,
            Some(start),
            start_number,
            current.job.slot(),
        )?;
        current.job.stop();
        current.job = job;
        // What the rung left wrote is from before the seek: let it go.
        current.left = None;
        Ok(epoch)
    }
}

/// Where `source`'s runs go: the GPU [`pick::choose`] picks for `probe`,
/// its first run (PGPU-05), passing over a card with no room for another
/// GPU encode. The card of the conversion it replaces has room: that one
/// lets go of its place as this one starts.
fn run_on(jobs: &Jobs, source: &ConversionSource<'_>, probe: EncodeSpec<'_>) -> RunOn {
    let replaced = source.slot.as_ref().and_then(Slot::card);
    let room = |gpu: &Gpu| {
        let limit = gpu.limit();
        replaced.is_some_and(|r| r.key == limit.key) || jobs.card_has_room(&limit)
    };
    let gpu = pick::choose(&probe, &source.gpus.gpus, &room).cloned();
    RunOn::new(&source.gpus, gpu)
}

/// The video of `rung`: a ladder's rung at its own height and cap, the
/// rest of the encode as `video`'s; for one encode, `video`.
fn rung_video(video: Option<ConvertedVideo>, ladder: &[Rung], rung: u32) -> Option<ConvertedVideo> {
    match (video, ladder.get(rung as usize)) {
        (
            Some(ConvertedVideo::Encode {
                codec, tone_map, ..
            }),
            Some(r),
        ) => Some(ConvertedVideo::Encode {
            codec,
            height: r.height,
            max_bitrate: Some(r.max_bitrate),
            tone_map,
        }),
        _ => video,
    }
}

impl Drop for Conversion {
    fn drop(&mut self) {
        let current = self.current.lock();
        current.job.stop();
        if let Some((_, left)) = &current.left {
            left.stop();
        }
        drop(current);
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
            run_at(segments, n, video)
        }
        (Some(_), None) => (None, 1),
        (None, start) => (start, 0),
    }
}

/// Where a run that is to write HLS segment `n` seeks, and the segment it
/// starts at: `n`, or 1 for a segment of an encoded video's head (PX-26),
/// which only a run from 0 cuts as the playlist says.
fn run_at(segments: &Segments, n: u32, video: Option<ConvertedVideo>) -> (Option<Duration>, u32) {
    let from = segments.run_from(n);
    (segment_seek(segments, from, video), from)
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
    file: &Path,
    input: &Path,
    sidecar: Option<&Path>,
    descriptor: &MediaDescriptor,
    (delivery, video, audio, subtitle): Parts,
    run_on: &RunOn,
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
    let spec = EncodeSpec {
        input,
        sidecar,
        descriptor,
        video,
        audio,
        subtitle,
        encoders: run_on.encoders(),
        vaapi_device: run_on.vaapi_device(),
        hardware_decode: !jobs.hardware_decode_failed(file),
        gpu_filters: jobs.limits().gpu_filters,
        tone_map: jobs.limits().tone_map,
        start,
        output,
    };
    let args = ffmpeg_args(&spec).map_err(JobError::Prepare)?;
    for why in args::cpu_instead_of_gpu(&spec) {
        if jobs.first_note(why) {
            tracing::info!(
                reason = why,
                "transcode.gpu_filters is on, but a conversion filters on the CPU"
            );
        }
    }
    if let Some(why) = args::tone_map_note(&spec)
        && jobs.first_note(why)
    {
        tracing::info!(reason = why, "a conversion tone maps on the CPU");
    }
    // One GPU family for the whole job (PGPU-02): the run may open its
    // device nodes and no other's.
    let placement = args::placement(&spec);
    let encoder = placement.and_then(|place| place.encoder.hardware);
    let gpu_encode = encoder.is_some();
    let gpu = placement.map_or(GpuUse::None, |place| place.gpu());
    // Decoding on the GPU may fail for a stream detection's test did not
    // cover, and libplacebo for a GPU or a stream its test did not
    // (PGPU-04): the same run decoding in software, its tone map on the
    // CPU, is ready for it.
    let decode = decode(&spec);
    let vulkan = args::vulkan(&spec);
    let fallback = match decode {
        Decode::Software if vulkan.is_none() => None,
        Decode::Software | Decode::Hardware | Decode::Gpu => {
            let software = EncodeSpec {
                hardware_decode: false,
                ..spec.clone()
            };
            Some(Fallback {
                args: ffmpeg_args(&software).map_err(JobError::Prepare)?,
                gpu: encoder.map_or(GpuUse::None, Hardware::gpu),
                file: file.to_path_buf(),
                made: Some(args::made(&software)),
            })
        }
    };
    let made = Some(args::made(&spec));
    let first_segment = if delivery == Delivery::Hls {
        start_number
    } else {
        0
    };
    let start_ms = start.map_or(0, |s| s.as_millis() as u64);
    // ffmpeg reads the job's links (PX-11 finding 34): the conversion's
    // directory, and the files they lead to, bound in confined.
    let vobsub_pictures = sidecar.map(|s| s.with_extension("sub"));
    let inputs = [Some(input), sidecar, vobsub_pictures.as_deref()]
        .into_iter()
        .flatten()
        .filter_map(|link| std::fs::read_link(link).ok())
        .collect();
    jobs.start(
        job_dir,
        Run {
            args,
            inputs,
            reads: vec![dir.to_path_buf()],
            gpu,
            vulkan,
            fallback,
            made,
            // A GPU encode counts against its card (PGPU-05); the run may
            // open that card's nodes, its ffmpeg confined to them.
            card: run_on.gpu().filter(|_| gpu_encode).map(|g| g.limit()),
            ffmpeg: run_on.gpu().map(|g| g.ffmpeg.clone()),
        },
        written,
        load(video, audio, gpu_encode, args::all_on_gpu(&spec)),
        epoch,
        first_segment,
        start_ms,
        slot,
    )
}

/// What a conversion weighs: its video encode, on the GPU or not, and if
/// on the GPU all of it there (`all_gpu`: `args::all_on_gpu`) or not, if
/// it has one (a burned-in subtitle is one); else an audio encode; else
/// a remux.
///
/// One all on the GPU whose decode fails and runs again in software
/// keeps its weight until it ends: that is rare, and the film decodes in
/// software from the next run on.
fn load(
    video: Option<ConvertedVideo>,
    audio: Option<ConvertedAudio>,
    gpu: bool,
    all_gpu: bool,
) -> Load {
    match video {
        Some(ConvertedVideo::Encode { .. }) if gpu && all_gpu => Load::GpuOnly,
        Some(ConvertedVideo::Encode { .. }) if gpu => Load::Gpu,
        Some(ConvertedVideo::Encode { .. }) => Load::Software,
        _ if encodes(video, audio) => Load::Audio,
        _ => Load::Remux,
    }
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
    use spoolframe_media::encoders::Encoders;

    /// `encoders` as the one GPU detection found, on renderD128 for
    /// VA-API, its runs made with `ffmpeg`.
    fn one_gpu(encoders: Encoders, ffmpeg: &Path) -> Arc<Detected> {
        let api = encoders
            .video
            .iter()
            .find_map(|e| e.hardware)
            .expect("a GPU encoder");
        let gpu =
            crate::transcode::test_gpu(0, api, spoolframe_media::gpus::Kind::Discrete, encoders);
        Arc::new(Detected::with_gpus(vec![Gpu {
            ffmpeg: spoolframe_media::sandbox::Tool::unconfined(ffmpeg),
            ..gpu
        }]))
    }

    /// B-13: a conversion that fails before its encode starts (a link
    /// that cannot be made, an HLS grid refused for the file's length)
    /// left its job directory behind until the next start.
    #[tokio::test]
    async fn a_conversion_that_cannot_be_prepared_leaves_no_directory() {
        let tmp = spoolframe_test_dir::TestDir::new("spoolframe-conversion-prepare").await;
        let jobs = Arc::new(
            Jobs::new(
                JobLimits {
                    budget: Units::jobs(1),
                    max_remux: 32,
                    max_gpu_jobs: 3,
                    scratch_dir: tmp.as_ref().join("scratch"),
                    scratch_limit_bytes: 1 << 30,
                    read_ahead: Duration::from_secs(60),
                    stalled_after: Duration::from_secs(60),
                    gpu_filters: false,
                    tone_map: spoolframe_media::encoders::ToneMapSetting::Auto,
                },
                spoolframe_media::sandbox::Tool::unconfined("ffmpeg"),
            )
            .unwrap(),
        );
        let film = tmp.as_ref().join("film.mkv");
        std::fs::write(&film, b"x").unwrap();
        let descriptor = Arc::new(MediaDescriptor {
            container: "matroska,webm".into(),
            // Past the longest HLS is made of.
            duration: Some(spoolframe_media::MAX_DURATION + Duration::from_secs(3600)),
            bit_rate: None,
            video: None,
            audio: Vec::new(),
            subtitles: Vec::new(),
        });
        let plan = PlaybackPlan::Converted {
            delivery: Delivery::Hls,
            container: spoolframe_types::Container::Mp4,
            video: None,
            audio: None,
            subtitle: None,
            ladder: Vec::new(),
            reasons: Vec::new(),
        };
        let started = Conversion::start(
            jobs.clone(),
            ConversionSource {
                handle: "h1",
                path: &film,
                descriptor,
                plan: &plan,
                gpus: Arc::new(Detected::software_baseline()),
                keyframes: None,
                first_epoch: 0,
                slot: None,
                start_rung: 0,
            },
            None,
        );
        assert!(started.is_err());
        assert!(!jobs.dir().join("h1").exists(), "the job directory is left");
    }

    /// A playback whose GPU decode failed mid-film: once its job has
    /// failed, the next segment asked for past what it wrote starts a new
    /// run there, decoding in software, rather than the failure being all
    /// the playback gets.
    #[tokio::test]
    async fn a_gpu_decode_that_failed_mid_film_goes_on_in_software() {
        use spoolframe_media::VideoStream;
        use spoolframe_media::encoders::{Hardware, HardwareDecoder, VideoEncoder};
        use spoolframe_types::{HdrFormat, VideoCodec};

        let tmp = spoolframe_test_dir::TestDir::new("spoolframe-conversion-gpu-decode").await;
        // Decoding on the GPU: writes segment 1 and fails. In software:
        // runs on.
        let ffmpeg = job::tests::ffmpeg(
            &tmp.as_ref().join("bin"),
            "for a; do last=$a; done\n\
             case \"$*\" in *-hwaccel*)\n\
               d=$(dirname \"$last\"); printf x > \"$d/seg1.m4s\"\n\
               printf '#EXTM3U\\nseg1.m4s\\n' > \"$last\"; echo out_time_us=6000000\n\
               echo 'hardware decoding failed' >&2; exit 1;;\n\
             esac\n\
             exec sleep 30",
        );
        let jobs = Arc::new(
            Jobs::new(
                JobLimits {
                    budget: Units::jobs(1),
                    max_remux: 32,
                    max_gpu_jobs: 3,
                    scratch_dir: tmp.as_ref().join("scratch"),
                    scratch_limit_bytes: 1 << 30,
                    read_ahead: Duration::from_secs(60),
                    stalled_after: Duration::from_secs(60),
                    gpu_filters: false,
                    tone_map: spoolframe_media::encoders::ToneMapSetting::Auto,
                },
                spoolframe_media::sandbox::Tool::unconfined(&ffmpeg),
            )
            .unwrap(),
        );
        let film = tmp.as_ref().join("film.mkv");
        std::fs::write(&film, b"x").unwrap();
        let descriptor = Arc::new(MediaDescriptor {
            container: "matroska,webm".into(),
            duration: Some(Duration::from_secs(60)),
            bit_rate: None,
            video: Some(VideoStream {
                index: 0,
                codec: "hevc".into(),
                profile: Some("Main".into()),
                level: None,
                width: 1920,
                height: 1080,
                pixel_format: Some("yuv420p".into()),
                color_transfer: None,
                frame_rate_millis: Some(24_000),
                bit_depth: 8,
                hdr: None::<HdrFormat>,
                bit_rate: None,
            }),
            audio: Vec::new(),
            subtitles: Vec::new(),
        });
        let plan = PlaybackPlan::Converted {
            delivery: Delivery::Hls,
            container: spoolframe_types::Container::Mp4,
            video: Some(ConvertedVideo::Encode {
                codec: VideoCodec::H264,
                height: 1080,
                max_bitrate: None,
                tone_map: false,
            }),
            audio: None,
            subtitle: None,
            ladder: Vec::new(),
            reasons: Vec::new(),
        };
        let encoders = Encoders {
            video: vec![VideoEncoder {
                codec: VideoCodec::H264,
                name: "h264_vaapi".into(),
                hardware: Some(Hardware::Vaapi),
            }],
            hardware_decoders: vec![HardwareDecoder {
                codec: VideoCodec::Hevc,
                hardware: Hardware::Vaapi,
                bit_depth: 8,
            }],
            ..Encoders::software_baseline()
        };
        let conversion = Conversion::start(
            jobs.clone(),
            ConversionSource {
                handle: "h1",
                path: &film,
                descriptor,
                plan: &plan,
                gpus: one_gpu(encoders, &ffmpeg),
                keyframes: None,
                first_epoch: 0,
                slot: None,
                start_rung: 0,
            },
            None,
        )
        .unwrap();
        let first = conversion.job();
        assert!(first.command().contains(&"-hwaccel".to_owned()));
        let mut progress = first.progress();
        progress.wait_for(|p| p.done.is_some()).await.unwrap();
        assert!(jobs.hardware_decode_failed(&film));
        // Segment 1 was written: it is the failed run's still.
        assert_eq!(conversion.job_for_segment(1, 0).unwrap().epoch, 0);
        let next = conversion.job_for_segment(2, 0).unwrap();
        assert_eq!(next.epoch, 1);
        assert!(
            !next.command().contains(&"-hwaccel".to_owned()),
            "{:?}",
            next.command()
        );
        assert_eq!(next.progress().borrow().done, None);
    }

    /// PGPU-01: an NVENC encode decoding on NVDEC whose decode fails before
    /// any output runs again in software, still on NVENC, and PX-24 says
    /// which decoded: NVDEC first, software after.
    #[tokio::test]
    async fn an_nvdec_decode_that_fails_at_once_runs_again_in_software() {
        use crate::encode::{Decoder, Made};
        use spoolframe_media::VideoStream;
        use spoolframe_media::encoders::{Hardware, HardwareDecoder, VideoEncoder};
        use spoolframe_types::{HdrFormat, VideoCodec};

        let tmp = spoolframe_test_dir::TestDir::new("spoolframe-conversion-nvdec").await;
        // Decoding through CUDA: fails after a moment, writing nothing. In
        // software: runs on.
        let ffmpeg = job::tests::ffmpeg(
            &tmp.as_ref().join("bin"),
            "case \"$*\" in *'-hwaccel cuda'*)\n\
               sleep 1; echo 'cuInit failed' >&2; exit 1;;\n\
             esac\n\
             exec sleep 30",
        );
        let jobs = Arc::new(
            Jobs::new(
                JobLimits {
                    budget: Units::jobs(1),
                    max_remux: 32,
                    max_gpu_jobs: 3,
                    scratch_dir: tmp.as_ref().join("scratch"),
                    scratch_limit_bytes: 1 << 30,
                    read_ahead: Duration::from_secs(60),
                    stalled_after: Duration::from_secs(60),
                    gpu_filters: false,
                    tone_map: spoolframe_media::encoders::ToneMapSetting::Auto,
                },
                spoolframe_media::sandbox::Tool::unconfined(&ffmpeg),
            )
            .unwrap(),
        );
        let film = tmp.as_ref().join("film.mkv");
        std::fs::write(&film, b"x").unwrap();
        let descriptor = Arc::new(MediaDescriptor {
            container: "matroska,webm".into(),
            duration: Some(Duration::from_secs(60)),
            bit_rate: None,
            video: Some(VideoStream {
                index: 0,
                codec: "hevc".into(),
                profile: Some("Main 10".into()),
                level: None,
                width: 3840,
                height: 2160,
                pixel_format: Some("yuv420p10le".into()),
                color_transfer: Some("smpte2084".into()),
                frame_rate_millis: Some(24_000),
                bit_depth: 10,
                hdr: Some(HdrFormat::Hdr10),
                bit_rate: None,
            }),
            audio: Vec::new(),
            subtitles: Vec::new(),
        });
        let plan = PlaybackPlan::Converted {
            delivery: Delivery::Hls,
            container: spoolframe_types::Container::Mp4,
            video: Some(ConvertedVideo::Encode {
                codec: VideoCodec::H264,
                height: 1080,
                max_bitrate: None,
                tone_map: true,
            }),
            audio: None,
            subtitle: None,
            ladder: Vec::new(),
            reasons: Vec::new(),
        };
        let encoders = Encoders {
            video: vec![VideoEncoder {
                codec: VideoCodec::H264,
                name: "h264_nvenc".into(),
                hardware: Some(Hardware::Nvenc),
            }],
            hardware_decoders: vec![HardwareDecoder {
                codec: VideoCodec::Hevc,
                hardware: Hardware::Nvenc,
                bit_depth: 10,
            }],
            ..Encoders::software_baseline()
        };
        let conversion = Conversion::start(
            jobs.clone(),
            ConversionSource {
                handle: "h1",
                path: &film,
                descriptor,
                plan: &plan,
                gpus: one_gpu(encoders, &ffmpeg),
                keyframes: None,
                first_epoch: 0,
                slot: None,
                start_rung: 0,
            },
            None,
        )
        .unwrap();
        let job = conversion.job();
        let decoder = |job: &EncodeJob| job.made().map(|m: Made| m.decoder);
        let first = job.command().join(" ");
        assert!(first.contains("-hwaccel cuda -i"), "{first}");
        assert!(first.contains("-c:v h264_nvenc"), "{first}");
        assert_eq!(decoder(&job), Some(Decoder::Hardware(Hardware::Nvenc)));
        job::tests::eventually(|| !job.command().contains(&"-hwaccel".to_owned())).await;
        let rerun = job.command().join(" ");
        assert!(rerun.contains("-c:v h264_nvenc"), "{rerun}");
        assert_eq!(decoder(&job), Some(Decoder::Software));
        assert!(!job.decodes_in_hardware());
        assert!(jobs.hardware_decode_failed(&film));
        assert_eq!(job.progress().borrow().done, None);
    }

    /// PGPU-03: an NVENC encode filtering on CUDA weighs as one all on the
    /// GPU, and a CUDA filter that fails before any output (a driver
    /// without the PTX JIT, say) runs it again as a failed VA-API filter
    /// does: decoded in software, filtered on the CPU, still on NVENC.
    #[tokio::test]
    async fn a_cuda_filter_that_fails_at_once_runs_again_on_the_cpu() {
        use crate::encode::{Decoder, Made};
        use spoolframe_media::VideoStream;
        use spoolframe_media::encoders::{Hardware, HardwareDecoder, VideoEncoder};
        use spoolframe_types::VideoCodec;

        let tmp = spoolframe_test_dir::TestDir::new("spoolframe-conversion-cuda").await;
        let ffmpeg = job::tests::ffmpeg(
            &tmp.as_ref().join("bin"),
            "case \"$*\" in *scale_cuda*)\n\
               sleep 1; echo 'cuModuleLoadData failed' >&2; exit 1;;\n\
             esac\n\
             exec sleep 30",
        );
        let jobs = Arc::new(
            Jobs::new(
                JobLimits {
                    budget: Units::jobs(1),
                    max_remux: 32,
                    max_gpu_jobs: 3,
                    scratch_dir: tmp.as_ref().join("scratch"),
                    scratch_limit_bytes: 1 << 30,
                    read_ahead: Duration::from_secs(60),
                    stalled_after: Duration::from_secs(60),
                    gpu_filters: true,
                    tone_map: spoolframe_media::encoders::ToneMapSetting::Auto,
                },
                spoolframe_media::sandbox::Tool::unconfined(&ffmpeg),
            )
            .unwrap(),
        );
        let film = tmp.as_ref().join("film.mkv");
        std::fs::write(&film, b"x").unwrap();
        let descriptor = Arc::new(MediaDescriptor {
            container: "matroska,webm".into(),
            duration: Some(Duration::from_secs(60)),
            bit_rate: None,
            video: Some(VideoStream {
                index: 0,
                codec: "hevc".into(),
                profile: Some("Main".into()),
                level: None,
                width: 3840,
                height: 2160,
                pixel_format: Some("yuv420p".into()),
                color_transfer: None,
                frame_rate_millis: Some(24_000),
                bit_depth: 8,
                hdr: None,
                bit_rate: None,
            }),
            audio: Vec::new(),
            subtitles: Vec::new(),
        });
        let plan = PlaybackPlan::Converted {
            delivery: Delivery::Hls,
            container: spoolframe_types::Container::Mp4,
            video: Some(ConvertedVideo::Encode {
                codec: VideoCodec::H264,
                height: 1080,
                max_bitrate: None,
                tone_map: false,
            }),
            audio: None,
            subtitle: None,
            ladder: Vec::new(),
            reasons: Vec::new(),
        };
        let encoders = Encoders {
            video: vec![VideoEncoder {
                codec: VideoCodec::H264,
                name: "h264_nvenc".into(),
                hardware: Some(Hardware::Nvenc),
            }],
            hardware_decoders: vec![HardwareDecoder {
                codec: VideoCodec::Hevc,
                hardware: Hardware::Nvenc,
                bit_depth: 8,
            }],
            cuda_scale: true,
            cuda_overlay: true,
            ..Encoders::software_baseline()
        };
        let conversion = Conversion::start(
            jobs.clone(),
            ConversionSource {
                handle: "h1",
                path: &film,
                descriptor,
                plan: &plan,
                gpus: one_gpu(encoders, &ffmpeg),
                keyframes: None,
                first_epoch: 0,
                slot: None,
                start_rung: 0,
            },
            None,
        )
        .unwrap();
        let job = conversion.job();
        assert_eq!(job.slot().map(|slot| slot.load()), Some(Load::GpuOnly));
        let decoder = |job: &EncodeJob| job.made().map(|m: Made| m.decoder);
        let first = job.command().join(" ");
        assert!(
            first.contains("-hwaccel cuda -hwaccel_output_format cuda -i"),
            "{first}"
        );
        assert!(first.contains("scale_cuda=w=-2:h=1080"), "{first}");
        assert_eq!(decoder(&job), Some(Decoder::Hardware(Hardware::Nvenc)));
        job::tests::eventually(|| !job.command().contains(&"-hwaccel".to_owned())).await;
        let rerun = job.command().join(" ");
        assert!(rerun.contains("scale=-2:1080,format=yuv420p"), "{rerun}");
        assert!(!rerun.contains("scale_cuda"), "{rerun}");
        assert!(rerun.contains("-c:v h264_nvenc"), "{rerun}");
        assert_eq!(decoder(&job), Some(Decoder::Software));
        assert!(jobs.hardware_decode_failed(&film));
        assert_eq!(job.progress().borrow().done, None);
    }

    /// PGPU-04: an NVENC encode tone mapped by libplacebo on ffmpeg's
    /// Vulkan device weighs as one all on the GPU, its jail uses Vulkan,
    /// and a libplacebo failure before any output runs it again decoded
    /// in software with the CPU's tone map, still on NVENC.
    #[tokio::test]
    async fn a_libplacebo_failure_runs_again_with_the_cpu_tone_map() {
        use crate::encode::{Decoder, Made};
        use spoolframe_media::VideoStream;
        use spoolframe_media::encoders::{
            Hardware, HardwareDecoder, Libplacebo, ToneMapper, VideoEncoder, VulkanDevice,
        };
        use spoolframe_types::{HdrFormat, VideoCodec};

        let tmp = spoolframe_test_dir::TestDir::new("spoolframe-conversion-gpu-tone-map").await;
        let ffmpeg = job::tests::ffmpeg(
            &tmp.as_ref().join("bin"),
            "case \"$*\" in *libplacebo*)\n\
               sleep 1; echo 'Error initializing filters' >&2; exit 1;;\n\
             esac\n\
             exec sleep 30",
        );
        let jobs = Arc::new(
            Jobs::new(
                JobLimits {
                    budget: Units::jobs(1),
                    max_remux: 32,
                    max_gpu_jobs: 3,
                    scratch_dir: tmp.as_ref().join("scratch"),
                    scratch_limit_bytes: 1 << 30,
                    read_ahead: Duration::from_secs(60),
                    stalled_after: Duration::from_secs(60),
                    gpu_filters: false,
                    tone_map: spoolframe_media::encoders::ToneMapSetting::Libplacebo,
                },
                spoolframe_media::sandbox::Tool::unconfined(&ffmpeg),
            )
            .unwrap(),
        );
        let film = tmp.as_ref().join("film.mkv");
        std::fs::write(&film, b"x").unwrap();
        let descriptor = Arc::new(MediaDescriptor {
            container: "matroska,webm".into(),
            duration: Some(Duration::from_secs(60)),
            bit_rate: None,
            video: Some(VideoStream {
                index: 0,
                codec: "hevc".into(),
                profile: Some("Main 10".into()),
                level: None,
                width: 3840,
                height: 2160,
                pixel_format: Some("yuv420p10le".into()),
                color_transfer: Some("smpte2084".into()),
                frame_rate_millis: Some(24_000),
                bit_depth: 10,
                hdr: Some(HdrFormat::Hdr10),
                bit_rate: None,
            }),
            audio: Vec::new(),
            subtitles: Vec::new(),
        });
        let plan = PlaybackPlan::Converted {
            delivery: Delivery::Hls,
            container: spoolframe_types::Container::Mp4,
            video: Some(ConvertedVideo::Encode {
                codec: VideoCodec::H264,
                height: 1080,
                max_bitrate: None,
                tone_map: true,
            }),
            audio: None,
            subtitle: None,
            ladder: Vec::new(),
            reasons: Vec::new(),
        };
        let encoders = Encoders {
            video: vec![VideoEncoder {
                codec: VideoCodec::H264,
                name: "h264_nvenc".into(),
                hardware: Some(Hardware::Nvenc),
            }],
            hardware_decoders: vec![HardwareDecoder {
                codec: VideoCodec::Hevc,
                hardware: Hardware::Nvenc,
                bit_depth: 10,
            }],
            libplacebo: vec![Libplacebo {
                hardware: Hardware::Nvenc,
                device: VulkanDevice::Shared,
            }],
            ..Encoders::software_baseline()
        };
        let conversion = Conversion::start(
            jobs.clone(),
            ConversionSource {
                handle: "h1",
                path: &film,
                descriptor,
                plan: &plan,
                gpus: one_gpu(encoders, &ffmpeg),
                keyframes: None,
                first_epoch: 0,
                slot: None,
                start_rung: 0,
            },
            None,
        )
        .unwrap();
        let job = conversion.job();
        assert_eq!(job.slot().map(|slot| slot.load()), Some(Load::GpuOnly));
        let made = |job: &EncodeJob| job.made().map(|m: Made| (m.decoder, m.tone_mapper));
        let first = job.command().join(" ");
        assert!(
            first.contains("-hwaccel vulkan -hwaccel_device vk -hwaccel_output_format vulkan -i"),
            "{first}"
        );
        assert!(first.contains("libplacebo=w=-2:h=1080"), "{first}");
        assert_eq!(
            made(&job),
            Some((
                Decoder::Vulkan,
                Some(ToneMapper::Libplacebo(VulkanDevice::Shared))
            ))
        );
        job::tests::eventually(|| !job.command().join(" ").contains("libplacebo=")).await;
        let rerun = job.command().join(" ");
        assert!(rerun.contains("tonemap=tonemap=hable"), "{rerun}");
        assert!(!rerun.contains("vulkan"), "{rerun}");
        assert!(rerun.contains("-c:v h264_nvenc"), "{rerun}");
        assert_eq!(made(&job), Some((Decoder::Software, Some(ToneMapper::Cpu))));
        assert!(jobs.hardware_decode_failed(&film));
    }

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

    /// A burned-in subtitle is a video encode in the plan, so it weighs as
    /// one: only the video and audio actions decide.
    #[test]
    fn a_conversion_weighs_its_heaviest_encode() {
        use spoolframe_types::{AudioCodec, VideoCodec};
        let copy_video = Some(ConvertedVideo::Copy {
            codec: VideoCodec::H264,
        });
        let encode_video = Some(ConvertedVideo::Encode {
            codec: VideoCodec::H264,
            height: 1080,
            max_bitrate: None,
            tone_map: false,
        });
        let copy_audio = Some(ConvertedAudio {
            index: 1,
            action: AudioAction::Copy {
                codec: AudioCodec::Aac,
            },
        });
        let encode_audio = Some(ConvertedAudio {
            index: 1,
            action: AudioAction::Encode {
                codec: AudioCodec::Aac,
                channels: 2,
                bitrate: 192_000,
            },
        });
        let software = false;
        assert_eq!(load(copy_video, copy_audio, false, software), Load::Remux);
        assert_eq!(load(copy_video, None, false, software), Load::Remux);
        assert_eq!(load(copy_video, encode_audio, false, software), Load::Audio);
        assert_eq!(load(None, encode_audio, false, software), Load::Audio);
        assert_eq!(
            load(encode_video, encode_audio, false, software),
            Load::Software
        );
        assert_eq!(load(encode_video, copy_audio, true, software), Load::Gpu);
        // The GPU flag is the video encoder's: an audio encode stays one.
        assert_eq!(load(copy_video, encode_audio, true, software), Load::Audio);
        // All on the GPU (`args::all_on_gpu`): a quarter; a software
        // encode is one whatever decodes.
        assert_eq!(load(encode_video, copy_audio, true, true), Load::GpuOnly);
        assert_eq!(load(encode_video, copy_audio, true, false), Load::Gpu);
        assert_eq!(load(encode_video, copy_audio, false, true), Load::Software);
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

    /// PX-26: a run for a segment of an encoded video's head starts from
    /// 0, the only run that cuts the head as listed; past it, a run starts
    /// at its segment's start.
    #[test]
    fn an_encoded_restart_in_the_head_starts_from_zero() {
        let encoded = Segments::encoded(60_000);
        let video = Some(ConvertedVideo::Encode {
            codec: spoolframe_types::VideoCodec::H264,
            height: 720,
            max_bitrate: None,
            tone_map: false,
        });
        assert_eq!(run_at(&encoded, 2, video), (None, 1));
        assert_eq!(run_at(&encoded, 3, video), (None, 1));
        assert_eq!(
            run_at(&encoded, 4, video),
            (Some(Duration::from_secs(8)), 4)
        );
        assert_eq!(
            run_at(&encoded, 5, video),
            (Some(Duration::from_secs(14)), 5)
        );
        // A position 5 s in is in segment 3, 4 to 8 s: written from 0.
        let at = Some(Duration::from_secs(5));
        assert_eq!(playback_seek(Some(&encoded), at, video), (None, 1));
        let at = Some(Duration::from_secs(13));
        assert_eq!(
            playback_seek(Some(&encoded), at, video),
            (Some(Duration::from_secs(8)), 4)
        );
    }
}
