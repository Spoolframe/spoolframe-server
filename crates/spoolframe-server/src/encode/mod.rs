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

use self::args::{Decode, EncodeSpec, Output, decode, ffmpeg_args, forced_through};
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
    /// The codecs a GPU encodes to, the first preferred
    /// (`transcode.video_codecs`, PGPU-06).
    pub video_codecs: Vec<spoolframe_types::VideoCodec>,
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
    /// The epoch the next run takes: each run its own, and its own
    /// directory, though two are started at once with the lock let go.
    epochs: std::sync::atomic::AtomicU32,
    /// Ended by an admin (P3-15): no encode starts again for it.
    ended: std::sync::atomic::AtomicBool,
    /// Every switch it logged, for tests to read: log capture races
    /// other tests' (tracing caches a callsite's interest process-wide).
    #[cfg(test)]
    switches: Arc<Mutex<Vec<Switch>>>,
}

/// One rung switch or restart of an HLS conversion, as it is logged.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Switch {
    playback: String,
    from: u32,
    to: u32,
    asked: String,
    init: &'static str,
    run_from: Option<u32>,
    epoch: Option<u32>,
    ready_ms: Option<u128>,
}

impl Switch {
    fn log(&self) {
        let what = if self.from == self.to {
            "hls run restarted"
        } else {
            "hls rung switch"
        };
        tracing::info!(
            target: "spoolframe_server::playback",
            playback = self.playback,
            from = self.from,
            to = self.to,
            asked = self.asked,
            init = self.init,
            run_from = self.run_from,
            epoch = self.epoch,
            ready_ms = self.ready_ms,
            "{what}"
        );
    }
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

/// Where a rung's init segment is read from ([`Conversion::init_for`]).
#[derive(Debug)]
pub enum Init {
    /// One a run of the rung wrote, kept: whole, read at once.
    Kept(PathBuf),
    /// A run's own, whole once it has finished its first segment.
    Job(Arc<EncodeJob>),
}

/// What a player asked a rung for, for the switch's log line.
#[derive(Debug, Clone, Copy)]
enum Asked {
    Init,
    Segment(u32),
}

/// What answered it, for the switch's log line.
#[derive(Debug)]
enum Served {
    /// The rung's kept init (no run).
    Kept,
    /// The run that left the rung, which has it.
    LeftRun,
    /// A run started for it.
    NewRun(Arc<EncodeJob>),
}

/// Why a read past what an ended conversion wrote fails (P3-15).
pub const ENDED: &str = "the conversion was ended on the server";

/// What a client is told of a segment past where the file ended: the
/// conversion finished the file before the playlist did.
pub const ENDED_SHORT: &str = "the film ends before this segment";

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
        let start_rung = if ladder.is_empty() {
            0
        } else {
            source.start_rung.min(ladder.len() as u32 - 1)
        };
        let first_video = rung_video(*video, ladder, start_rung);
        // An HLS start begins at the segment the position falls in, sought
        // as the rung it starts on is: a copy only at a keyframe.
        let (start, first_segment) = playback_seek(segments.as_ref(), start, first_video);
        // The GPU is picked for an encode (PGPU-05): a remux ladder that
        // starts on its copy has its encodes picked for, its highest.
        let encoded = (0..ladder.len() as u32)
            .map(|n| rung_video(*video, ladder, n))
            .find(|v| matches!(v, Some(ConvertedVideo::Encode { .. })));
        let picked_for = match first_video {
            Some(ConvertedVideo::Copy { .. }) => encoded.unwrap_or(first_video),
            _ => first_video,
        };
        let run_on = match run_on(
            &jobs,
            &source,
            EncodeSpec {
                input: &input,
                sidecar: sidecar.as_deref(),
                descriptor: &source.descriptor,
                video: picked_for,
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
        ) {
            Ok(run_on) => run_on,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(e);
            }
        };
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
            keyframe_cuts(*video, first_video, segments.as_ref()),
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
            epochs: std::sync::atomic::AtomicU32::new(job.epoch + 1),
            current: Mutex::new(Current {
                job,
                rung: start_rung,
                left: None,
            }),
            ended: Default::default(),
            #[cfg(test)]
            switches: Arc::default(),
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
            _ if self.ladder.is_empty() => hls::multivariant_playlist(
                &self.descriptor,
                self.video,
                self.video.and_then(|v| self.written_size(v)),
                self.audio,
                rendition,
            ),
            // The rung the conversion started on first, as the player
            // starts on the first; then the rest, highest first.
            _ => {
                let first = self.first_rung();
                let rungs: Vec<hls::LadderVariant> = std::iter::once(first)
                    .chain((0..self.rungs()).filter(|&n| n != first))
                    .filter_map(|n| {
                        let video = rung_video(self.video, &self.ladder, n)?;
                        Some((n, video, self.written_size(video)))
                    })
                    .collect();
                hls::ladder_playlist(&self.descriptor, &rungs, self.audio, rendition)
            }
        })
    }

    /// The picture an encode of `video` writes here, on the GPU its runs
    /// go to ([`args::written_size`]): what the playlist states, so a
    /// rung AV1 on VA-API writes at a size of its own (1296×540 written
    /// 1280×528) is not listed at the size asked for.
    fn written_size(&self, video: ConvertedVideo) -> Option<(u32, u32)> {
        let jobs = &self.jobs;
        args::written_size(&EncodeSpec {
            input: &self.input,
            sidecar: self.sidecar.as_deref(),
            descriptor: &self.descriptor,
            video: Some(video),
            audio: self.audio,
            subtitle: self.subtitle,
            encoders: self.run_on.encoders(),
            vaapi_device: self.run_on.vaapi_device(),
            hardware_decode: !jobs.hardware_decode_failed(&self.file),
            gpu_filters: jobs.limits().gpu_filters,
            tone_map: jobs.limits().tone_map,
            start: None,
            output: Output::Hls {
                dir: self.dir.clone(),
                start_number: 1,
                cuts: None,
            },
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
        let current = self.current.lock();
        if rung != current.rung {
            if let Some((left, job)) = &current.left
                && *left == rung
                && job.has_segment(n)
                && Self::aligned(job, n)
            {
                return Ok(job.clone());
            }
            if self.ended() {
                return Err(JobError::Prepare(ENDED.into()));
            }
            return self.switch(
                current,
                rung,
                n,
                Asked::Segment(n),
                segments,
                |job| job,
                || self.job_for_segment(n, rung),
            );
        }
        let job = &current.job;
        let written = job.written();
        let head = written.unwrap_or(job.first_segment.saturating_sub(1));
        if self.ended() {
            // Only what it wrote: nothing more is coming.
            return if n >= job.first_kept()
                && written.is_some_and(|w| n <= w)
                && Self::aligned(job, n)
            {
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
        // ffmpeg finished the file short of the playlist (a duration
        // overstated, a file cut short): nothing past what it wrote is
        // coming. Refused, where the finished run was returned for a
        // segment just past it and an empty answer had the client ask
        // again at its round trip for ever (review 2026-10-11 M1). One
        // further on is a new run's, from there; if that one ends short too,
        // it is refused then.
        if matches!(job.progress().borrow().done, Some(Ok(_)))
            && n >= job.first_segment
            && n <= head + SEGMENTS_AHEAD
            && written.is_none_or(|w| n > w)
        {
            return Err(JobError::Prepare(ENDED_SHORT.into()));
        }
        if n >= job.first_kept()
            && n <= head + SEGMENTS_AHEAD
            && !failed_on_gpu
            && Self::aligned(job, n)
        {
            return Ok(job.clone());
        }
        let (start, from) = run_at(segments, n, rung_video(self.video, &self.ladder, rung));
        let run = self.next_run(&current, rung, start, from);
        let replaces = current.job.epoch;
        drop(current);
        let next = self.start_run(run)?;
        let mut current = self.current.lock();
        if current.job.epoch != replaces || current.rung != rung || self.ended() {
            // Another read moved the encode meanwhile: this run goes
            // (stopped, its files removed), and the segment is asked of the
            // encode as it is now.
            drop(current);
            drop(next);
            return self.job_for_segment(n, rung);
        }
        current.job.stop();
        current.job = next;
        self.log_switch(
            rung,
            rung,
            Asked::Segment(n),
            Served::NewRun(current.job.clone()),
        );
        Ok(current.job.clone())
    }

    /// The run that starts `rung` at `start` (segment `from`) in place of
    /// `current`'s: worked out under the lock, started with it let go
    /// ([`Conversion::start_run`]), so the arguments, the links and the
    /// fork and exec of ffmpeg through its jail hold up no other reader of
    /// this playback (review 2026-10-11 M4).
    fn next_run(
        &self,
        current: &Current,
        rung: u32,
        start: Option<Duration>,
        from: u32,
    ) -> NextRun {
        NextRun {
            parts: self.parts(rung),
            epoch: self
                .epochs
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst),
            start,
            from,
            slot: current.job.slot(),
            cuts: self.cuts(rung),
        }
    }

    fn start_run(&self, run: NextRun) -> Result<Arc<EncodeJob>, JobError> {
        spawn(
            &self.jobs,
            &self.dir,
            &self.file,
            &self.input,
            self.sidecar.as_deref(),
            &self.descriptor,
            run.parts,
            &self.run_on,
            run.epoch,
            run.start,
            run.from,
            run.slot,
            run.cuts,
        )
    }

    /// Where `rung`'s init segment is read from (PX-21): one a run of that
    /// rung wrote already, kept; the running job, or the one that left
    /// that rung; or, for a rung neither is, the encode moved to it at the
    /// segment the player asks for next.
    ///
    /// A rung's init is the same from any run of it, whatever its start
    /// (its codecs and their settings, the same encoder for the whole
    /// playback): kept from the first run that wrote it, a player coming
    /// back to the rung has it at once, with no run started for it. And
    /// the run started for an init begins where the player reads on, the
    /// segment after the last it asked for: begun past what is written,
    /// as it was, the player's next segment came before it and a second
    /// run started there, two ffmpeg starts before the player had a
    /// picture (an iPhone gave up on the init after 6 s).
    pub fn init_for(&self, rung: u32) -> Result<Init, JobError> {
        let segments = self
            .segments
            .as_ref()
            .ok_or_else(|| JobError::Prepare("not an HLS playback".into()))?;
        if rung >= self.rungs() {
            return Err(JobError::Prepare(format!(
                "no rung {rung} in this playback"
            )));
        }
        let kept = self.kept_init(rung);
        let current = self.current.lock();
        if kept.exists() {
            if rung != current.rung {
                self.log_switch(current.rung, rung, Asked::Init, Served::Kept);
            }
            return Ok(Init::Kept(kept));
        }
        if rung == current.rung {
            self.keep_init(rung, &current.job);
            return Ok(Init::Job(current.job.clone()));
        }
        // The one left, once it wrote a segment: its init was finished then.
        if let Some((left, job)) = &current.left
            && *left == rung
            && job.written().is_some()
        {
            self.keep_init(rung, job);
            self.log_switch(current.rung, rung, Asked::Init, Served::LeftRun);
            return Ok(Init::Job(job.clone()));
        }
        if self.ended() {
            return Err(JobError::Prepare(ENDED.into()));
        }
        let job = &current.job;
        let next = match job.reader_segment() {
            0 => job.written().map_or(job.first_segment, |w| w + 1),
            read => read + 1,
        }
        .min(segments.count())
        .max(1);
        self.switch(
            current,
            rung,
            next,
            Asked::Init,
            segments,
            Init::Job,
            || self.init_for(rung),
        )
    }

    /// Where a run of `rung` that wrote its init has it kept: in the
    /// conversion's directory, which outlives each run's.
    fn kept_init(&self, rung: u32) -> PathBuf {
        self.dir.join(format!("init-v{rung}.mp4"))
    }

    /// Keep `job`'s init as `rung`'s, once it has written a segment (its
    /// init is whole then), if none is kept yet. Copied, then renamed into
    /// place: a reader never opens half of one.
    fn keep_init(&self, rung: u32, job: &EncodeJob) {
        let kept = self.kept_init(rung);
        if job.written().is_none() || kept.exists() {
            return;
        }
        let part = self.dir.join(format!("init-v{rung}.part"));
        let copied = std::fs::copy(job.dir().join("init.mp4"), &part)
            .and_then(|_| std::fs::rename(&part, &kept));
        if let Err(e) = copied {
            tracing::debug!(rung, error = %e, "a rung's init segment was not kept");
            let _ = std::fs::remove_file(&part);
        }
    }

    /// Move the encode to `rung` at segment `n`: started first in the
    /// running job's slot, then the running job stopped and kept as the
    /// one left (the one left before it goes, its files with it).
    ///
    /// The run starts with the lock let go (M4); if the encode moved
    /// meanwhile, it goes, and `again` asks of the encode as it is now.
    #[allow(clippy::too_many_arguments)]
    fn switch<T>(
        &self,
        current: parking_lot::MutexGuard<'_, Current>,
        rung: u32,
        n: u32,
        asked: Asked,
        segments: &Segments,
        done: impl FnOnce(Arc<EncodeJob>) -> T,
        again: impl FnOnce() -> Result<T, JobError>,
    ) -> Result<T, JobError> {
        let (start, from) = run_at(segments, n, rung_video(self.video, &self.ladder, rung));
        let run = self.next_run(&current, rung, start, from);
        let (replaces, was) = (current.job.epoch, current.rung);
        drop(current);
        let next = self.start_run(run)?;
        let mut current = self.current.lock();
        if current.job.epoch != replaces || current.rung != was || self.ended() {
            drop(current);
            drop(next);
            return again();
        }
        let left = std::mem::replace(&mut current.job, next);
        left.stop();
        self.keep_init(current.rung, &left);
        self.log_switch(
            current.rung,
            rung,
            asked,
            Served::NewRun(current.job.clone()),
        );
        current.left = Some((current.rung, left));
        current.rung = rung;
        Ok(done(current.job.clone()))
    }

    /// One line for a player moving to rung `to` from `from` (PX-21), or
    /// a run restarted on its rung for a segment it does not have: what
    /// was asked, and what answered it. For a new run, logged once it has
    /// written its first segment (its init with it, as ffmpeg writes
    /// both), with how long that took: what the player waited for.
    fn log_switch(&self, from: u32, to: u32, asked: Asked, served: Served) {
        let playback = self
            .dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let asked = match asked {
            Asked::Init => "init".to_owned(),
            Asked::Segment(n) => format!("segment {n}"),
        };
        let mut switch = Switch {
            playback,
            from,
            to,
            asked,
            init: "new run",
            run_from: None,
            epoch: None,
            ready_ms: None,
        };
        #[cfg(test)]
        let switches = self.switches.clone();
        let record = move |switch: Switch| {
            switch.log();
            #[cfg(test)]
            switches.lock().push(switch);
        };
        let job = match served {
            Served::Kept => {
                return record(Switch {
                    init: "kept",
                    ..switch
                });
            }
            Served::LeftRun => {
                return record(Switch {
                    init: "left run",
                    ..switch
                });
            }
            Served::NewRun(job) => job,
        };
        switch.run_from = Some(job.first_segment);
        switch.epoch = Some(job.epoch);
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return record(switch);
        };
        let started = std::time::Instant::now();
        let mut progress = job.progress();
        drop(job);
        runtime.spawn(async move {
            let written = tokio::time::timeout(
                Duration::from_secs(60),
                progress.wait_for(|p| p.last_segment.is_some() || p.done.is_some()),
            )
            .await;
            let ready = matches!(written, Ok(Ok(ref p)) if p.last_segment.is_some());
            switch.ready_ms = ready.then(|| started.elapsed().as_millis());
            record(switch);
        });
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

    /// Whether `job`, a run of `rung`, cut segment `n` where the playlist
    /// says. A remux ladder's encoded rung is cut at the keyframes it is
    /// told to force, and a run's list of them stops at what one argument
    /// holds (`args::forced_through`): past its last, the run cuts where
    /// its encoder keys, and its segments there have other times than
    /// their numbers say (a two-hour film keyed every 500 ms, from about
    /// 1:55:50 on). A segment past that is a new run's, from there.
    ///
    /// Worked out once per run, when it starts ([`aligned_until`]): the
    /// list is up to 120 KiB, and this is asked on every segment read.
    fn aligned(job: &EncodeJob, n: u32) -> bool {
        job.aligned_until.is_none_or(|s| n < s)
    }

    /// Where an encode of `rung` cuts, when it is not its own grid: see
    /// [`keyframe_cuts`].
    fn cuts(&self, rung: u32) -> Option<Vec<u64>> {
        keyframe_cuts(
            self.video,
            rung_video(self.video, &self.ladder, rung),
            self.segments.as_ref(),
        )
    }

    /// The job running now.
    pub fn job(&self) -> Arc<EncodeJob> {
        self.current.lock().job.clone()
    }

    /// Seek using the same timeline and keyframe alignment as a new
    /// playback. HLS segment numbers always refer to the full film.
    pub fn seek(&self, position: Duration) -> Result<u32, JobError> {
        let rung = self.rung();
        let video = rung_video(self.video, &self.ladder, rung);
        let (start, number) = playback_seek(self.segments.as_ref(), Some(position), video);
        self.restart(start.unwrap_or(Duration::ZERO), number)
    }

    /// Start the next epoch at `start` and stop the job (progressive; HLS
    /// restarts at a segment, [`Conversion::job_for_segment`]). Returns the
    /// new epoch.
    pub fn restart(&self, start: Duration, start_number: u32) -> Result<u32, JobError> {
        let current = self.current.lock();
        if self.ended() {
            return Err(JobError::Prepare(ENDED.into()));
        }
        let run = self.next_run(&current, current.rung, Some(start), start_number);
        let epoch = run.epoch;
        drop(current);
        // Started with the lock let go (M4). A seek is the client's last
        // word: it replaces whatever runs by then, on its rung.
        let job = self.start_run(run)?;
        let mut current = self.current.lock();
        if self.ended() {
            return Err(JobError::Prepare(ENDED.into()));
        }
        current.job.stop();
        current.job = job;
        // What the rung left wrote is from before the seek: let it go.
        current.left = None;
        Ok(epoch)
    }
}

/// Where `source`'s runs go: the GPU [`pick::choose`] picks for `probe`,
/// its first run (PGPU-05), passing over a card with no room for another
/// GPU encode, and with none left a software encode ([`pick::spill`]).
/// The card of the conversion it replaces has room: that one lets go of
/// its place as this one starts.
///
/// Neither a spill nor a refusal is silent: a spill is logged once, as the
/// conversion starts, with the encoder it runs on; an encode that keeps
/// HDR and finds no 10-bit encoder with room is refused `Busy`, with why.
fn run_on(
    jobs: &Jobs,
    source: &ConversionSource<'_>,
    probe: EncodeSpec<'_>,
) -> Result<RunOn, JobError> {
    let replaced = source.slot.as_ref().and_then(Slot::card);
    let room = |gpu: &Gpu| {
        let limit = gpu.limit();
        replaced.is_some_and(|r| r.key == limit.key) || jobs.card_has_room(&limit)
    };
    let full = || {
        source
            .gpus
            .gpus
            .iter()
            .filter(|gpu| !room(gpu))
            .map(|gpu| gpu.card().place())
            .collect::<Vec<_>>()
    };
    let codec = match probe.video {
        Some(ConvertedVideo::Encode { codec, .. }) => Some(codec.as_str()),
        _ => None,
    };
    match pick::run_on(&source.gpus, &probe, &room) {
        Ok(run) => {
            if run.spilled() {
                tracing::warn!(
                    handle = source.handle,
                    codec,
                    encoder = run.encoder_name(&probe).as_deref(),
                    decoder_gpu = run.gpu().map(|g| g.card().place()),
                    full = ?full(),
                    "every GPU that encodes this codec is full: the conversion encodes in software"
                );
            }
            Ok(run)
        }
        Err(pick::NoTenBit) => {
            tracing::warn!(
                handle = source.handle,
                codec,
                full = ?full(),
                "an encode that keeps HDR has no 10-bit encoder with room: refused as busy, not written in 8 bits"
            );
            Err(JobError::Busy)
        }
    }
}

/// What a playback is planned with now (PGPU-06): [`pick::planning`] with
/// `transcode.video_codecs` and the cards that have room for another GPU
/// encode.
pub fn planning_encoders(
    jobs: &Jobs,
    detected: &crate::transcode::Detected,
) -> spoolframe_media::encoders::Encoders {
    let room = |gpu: &Gpu| jobs.card_has_room(&gpu.limit());
    pick::planning(detected, &jobs.limits().video_codecs, &room)
}

/// The cuts an encode of a remux ladder's lower rung (PX-21) makes: the
/// copied original's, at the source's keyframes, so every variant has the
/// same segments and AVPlayer can move between them at any one. The
/// encode forces a keyframe at each and cuts there (`args`). `None` for an
/// encode on its own grid, a copy, and anything not HLS.
fn keyframe_cuts(
    plan: Option<ConvertedVideo>,
    run: Option<ConvertedVideo>,
    segments: Option<&Segments>,
) -> Option<Vec<u64>> {
    match (plan, run, segments) {
        (Some(ConvertedVideo::Copy { .. }), Some(ConvertedVideo::Encode { .. }), Some(s)) => {
            Some(s.starts_ms.clone())
        }
        _ => None,
    }
}

/// The video of `rung`: a ladder's rung at its own height and cap, the
/// rest of the encode as `video`'s; for one encode, `video`.
fn rung_video(video: Option<ConvertedVideo>, ladder: &[Rung], rung: u32) -> Option<ConvertedVideo> {
    // A rung with a video of its own: an encode under a copied original.
    if let Some(own @ Some(_)) = ladder.get(rung as usize).map(|r| r.video) {
        return own;
    }
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
        // Its runs' directories too, many files: on the blocking pool, not
        // the async worker that let go of it (review 2026-10-11 M3).
        let dir = std::mem::take(&mut self.dir);
        let remove = move || {
            if let Err(e) = std::fs::remove_dir_all(&dir)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(dir = %dir.display(), error = %e, "cannot remove a conversion's files");
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn_blocking(remove);
            }
            Err(_) => remove(),
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

/// A run worked out under a conversion's lock, to be started with it let
/// go ([`Conversion::next_run`]).
struct NextRun {
    parts: Parts,
    epoch: u32,
    start: Option<Duration>,
    from: u32,
    slot: Option<Slot>,
    cuts: Option<Vec<u64>>,
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
    cuts: Option<Vec<u64>>,
) -> Result<Arc<EncodeJob>, JobError> {
    let job_dir = dir.join(epoch.to_string());
    let aligned_until = aligned_until(cuts.as_deref(), start);
    let (output, written) = match delivery {
        Delivery::Hls => (
            Output::Hls {
                dir: job_dir.clone(),
                start_number,
                cuts,
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
            aligned_until,
        },
        written,
        load(video, audio, gpu_encode, args::all_on_gpu(&spec)),
        epoch,
        first_segment,
        start_ms,
        slot,
    )
}

/// The first segment a run from `start` cut at `cuts` does not cut where
/// the playlist says: its list of keyframes to force stops at what one
/// argument holds ([`forced_through`]), and segment `s` starts at the last
/// forced one, its end not forced. `None` for a run that forces every cut
/// from `start` on, and for one not cut at given keyframes.
fn aligned_until(cuts: Option<&[u64]>, start: Option<Duration>) -> Option<u32> {
    let cuts = cuts?;
    let last = forced_through(cuts, start)?;
    Some(cuts.partition_point(|&ms| ms < last) as u32 + 1)
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
pub(crate) mod tests {
    use super::*;
    use spoolframe_types::VideoCodec;

    const COPY: ConvertedVideo = ConvertedVideo::Copy {
        codec: VideoCodec::H264,
        dolby_vision: false,
    };

    fn under(height: u32, cap: u64) -> Rung {
        Rung {
            height,
            max_bitrate: cap,
            video: Some(ConvertedVideo::Encode {
                codec: VideoCodec::H264,
                height,
                max_bitrate: Some(cap),
                tone_map: false,
            }),
        }
    }

    /// A remux ladder's (PX-21) top rung is the plan's copy; each rung
    /// under it is its own encode, cut at the copy's keyframes; the copy
    /// cuts by itself.
    #[test]
    fn a_remux_ladder_s_rungs_are_the_copy_and_encodes_cut_where_it_is() {
        let ladder = [
            Rung {
                height: 1080,
                max_bitrate: 8_000_000,
                video: None,
            },
            under(720, 4_000_000),
            under(480, 2_000_000),
        ];
        assert_eq!(rung_video(Some(COPY), &ladder, 0), Some(COPY));
        assert_eq!(rung_video(Some(COPY), &ladder, 2), ladder[2].video);
        let segments = Segments::at_keyframes(&[0, 1_001, 2_002], 3_000);
        let encoded = rung_video(Some(COPY), &ladder, 1);
        assert_eq!(
            keyframe_cuts(Some(COPY), encoded, Some(&segments)),
            Some(segments.starts_ms.clone())
        );
        assert_eq!(keyframe_cuts(Some(COPY), Some(COPY), Some(&segments)), None);
        // A ladder of encodes is on its own grid.
        assert_eq!(keyframe_cuts(encoded, encoded, Some(&segments)), None);
    }
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

    /// A stand-in ffmpeg for a ladder: writes `init.mp4` (its own
    /// arguments, so a test can tell whose it is) and six segments from its
    /// `-start_number`, lists them, then runs on.
    const LADDER_FFMPEG: &str = "for a; do last=$a; done; d=$(dirname \"$last\")\n\
         n=$(printf '%s\\n' \"$@\" | grep -A1 -x -- -start_number | tail -n 1)\n\
         printf '%s' \"$*\" > \"$d/init.mp4\"\n\
         list='#EXTM3U'\n\
         for i in 0 1 2 3 4 5; do printf x > \"$d/seg$((n+i)).m4s\"\n\
           list=\"$list\nseg$((n+i)).m4s\"; done\n\
         printf '%s\\n' \"$list\" > \"$last\"\n\
         echo out_time_us=36000000\n\
         exec sleep 30";

    /// An H.264 ladder of four rungs (8, 4, 2 and 1 Mbit/s) of a two-hour
    /// film, resumed at 10 min on its lowest, run by [`LADDER_FFMPEG`].
    async fn resumed_ladder(test: &str) -> (Arc<Conversion>, spoolframe_test_dir::TestDir) {
        resumed_ladder_of(test, spoolframe_types::VideoCodec::H264, "h264_vaapi").await
    }

    /// [`resumed_ladder`] encoded in `codec` by VA-API's `encoder`.
    pub(crate) async fn resumed_ladder_of(
        test: &str,
        codec: spoolframe_types::VideoCodec,
        encoder: &str,
    ) -> (Arc<Conversion>, spoolframe_test_dir::TestDir) {
        resumed_ladder_run_by(test, codec, encoder, LADDER_FFMPEG).await
    }

    /// [`resumed_ladder_of`], its runs made by `script` in ffmpeg's place.
    async fn resumed_ladder_run_by(
        test: &str,
        codec: spoolframe_types::VideoCodec,
        encoder: &str,
        script: &str,
    ) -> (Arc<Conversion>, spoolframe_test_dir::TestDir) {
        use spoolframe_media::VideoStream;
        use spoolframe_media::encoders::{Hardware, VideoEncoder};

        let tmp = spoolframe_test_dir::TestDir::new(test).await;
        let ffmpeg = job::tests::ffmpeg(&tmp.as_ref().join("bin"), script);
        let jobs = Arc::new(
            Jobs::new(
                JobLimits {
                    budget: Units::jobs(4),
                    max_remux: 32,
                    max_gpu_jobs: 3,
                    scratch_dir: tmp.as_ref().join("scratch"),
                    scratch_limit_bytes: 1 << 30,
                    read_ahead: Duration::from_secs(3600),
                    stalled_after: Duration::from_secs(60),
                    gpu_filters: false,
                    tone_map: spoolframe_media::encoders::ToneMapSetting::Auto,
                    video_codecs: crate::config::DEFAULT_VIDEO_CODECS.to_vec(),
                },
                spoolframe_media::sandbox::Tool::unconfined(&ffmpeg),
            )
            .unwrap(),
        );
        let film = tmp.as_ref().join("film.mkv");
        std::fs::write(&film, b"x").unwrap();
        let descriptor = Arc::new(MediaDescriptor {
            container: "matroska,webm".into(),
            duration: Some(Duration::from_secs(7200)),
            bit_rate: None,
            video: Some(VideoStream {
                index: 0,
                codec: "h264".into(),
                profile: Some("High".into()),
                level: None,
                width: 1920,
                height: 1080,
                pixel_format: Some("yuv420p".into()),
                color_transfer: None,
                frame_rate_millis: Some(24_000),
                bit_depth: 8,
                hdr: None,
                dolby_vision: None,
                bit_rate: None,
            }),
            audio: Vec::new(),
            subtitles: Vec::new(),
        });
        let rungs = [
            (1080, 8_000_000),
            (720, 4_000_000),
            (540, 2_000_000),
            (360, 1_000_000),
        ];
        let plan = PlaybackPlan::Converted {
            delivery: Delivery::Hls,
            container: spoolframe_types::Container::Mp4,
            video: Some(ConvertedVideo::Encode {
                codec,
                height: 1080,
                max_bitrate: Some(8_000_000),
                tone_map: false,
            }),
            audio: None,
            subtitle: None,
            ladder: rungs
                .map(|(height, max_bitrate)| Rung {
                    height,
                    max_bitrate,
                    video: None,
                })
                .to_vec(),
            reasons: Vec::new(),
        };
        let encoders = Encoders {
            video: vec![VideoEncoder {
                codec,
                name: encoder.into(),
                hardware: Some(Hardware::Vaapi),
            }],
            ..Encoders::software_baseline()
        };
        let conversion = Conversion::start(
            jobs,
            ConversionSource {
                handle: "h1",
                path: &film,
                descriptor,
                plan: &plan,
                gpus: one_gpu(encoders, &ffmpeg),
                keyframes: None,
                first_epoch: 0,
                slot: None,
                start_rung: 3,
            },
            Some(Duration::from_secs(600)),
        )
        .unwrap();
        (conversion, tmp)
    }

    /// Two reads that each need a new run at one far segment, at once:
    /// both are answered by one run, the other started with the lock let
    /// go is let go too, and each run had a directory of its own. Review
    /// 2026-10-11 M4.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_reads_restarting_at_once_end_on_one_run() {
        let (conversion, _tmp) = resumed_ladder("spoolframe-conversion-two-restarts").await;
        let far = conversion.job().first_segment + 500;
        let read = || {
            let conversion = conversion.clone();
            tokio::task::spawn_blocking(move || conversion.job_for_segment(far, 3))
        };
        let (a, b) = tokio::join!(read(), read());
        let (a, b) = (a.unwrap().unwrap(), b.unwrap().unwrap());
        assert_eq!(a.epoch, b.epoch);
        assert_eq!(conversion.job().epoch, a.epoch);
        assert!(a.first_segment <= far);
        // Every run started had its own epoch: none was written over.
        let started = conversion.epochs.load(std::sync::atomic::Ordering::SeqCst);
        assert!(started > a.epoch, "{started} after {}", a.epoch);
    }

    /// A run that finishes the file short of the playlist (its duration
    /// overstated, the file cut short): a segment past what it wrote is
    /// refused, not answered empty for the client to ask again at its
    /// round trip for ever. Review 2026-10-11 M1.
    #[tokio::test]
    async fn a_segment_past_a_run_that_ended_short_is_refused() {
        const SHORT: &str = "for a; do last=$a; done; d=$(dirname \"$last\")\n\
             n=$(printf '%s\\n' \"$@\" | grep -A1 -x -- -start_number | tail -n 1)\n\
             printf x > \"$d/init.mp4\"\n\
             printf x > \"$d/seg$n.m4s\"\n\
             printf '#EXTM3U\\nseg%s.m4s\\n' \"$n\" > \"$last\"\n\
             echo out_time_us=6000000\n\
             echo progress=end";
        let (conversion, _tmp) = resumed_ladder_run_by(
            "spoolframe-conversion-ended-short",
            spoolframe_types::VideoCodec::H264,
            "h264_vaapi",
            SHORT,
        )
        .await;
        let job = conversion.job();
        let mut progress = job.progress();
        tokio::time::timeout(
            Duration::from_secs(10),
            progress.wait_for(|p| p.done.is_some()),
        )
        .await
        .expect("the run ends")
        .unwrap();
        let first = job.first_segment;
        assert_eq!(job.written(), Some(first));
        // What it wrote is served; the next segment, inside what a run
        // would be kept for, is refused.
        assert!(conversion.job_for_segment(first, 3).is_ok());
        let past = conversion.job_for_segment(first + 1, 3).unwrap_err();
        assert!(
            matches!(past, JobError::Prepare(ref why) if why == ENDED_SHORT),
            "{past:?}"
        );
    }

    /// An AV1 ladder on VA-API lists each rung at the size its encode
    /// writes: AMD's encoder writes 64×16 sizes only, so 1080p, 540p and
    /// 360p are scaled to whole ones (`args::whole_av1_size`), and the
    /// playlist said the sizes asked for.
    #[tokio::test]
    async fn an_av1_vaapi_ladder_lists_the_sizes_it_writes() {
        let (conversion, _tmp) = resumed_ladder_of(
            "spoolframe-conversion-av1-sizes",
            spoolframe_types::VideoCodec::Av1,
            "av1_vaapi",
        )
        .await;
        let text = conversion
            .manifest(ManifestKind::Multivariant, 0)
            .expect("a multivariant playlist");
        let Ok(m3u8_rs::Playlist::MasterPlaylist(p)) = m3u8_rs::parse_playlist_res(text.as_bytes())
        else {
            panic!("not a multivariant playlist: {text}");
        };
        let mut sizes: Vec<(u64, u64)> = p
            .variants
            .iter()
            .filter_map(|v| v.resolution.map(|r| (r.width, r.height)))
            .collect();
        sizes.sort_unstable();
        assert_eq!(
            sizes,
            [(640, 352), (960, 528), (1280, 720), (1920, 1072)],
            "{text}"
        );
    }

    /// A run's first segment off the playlist's grid: past its last
    /// forced keyframe when its list stops short; none when the list holds
    /// every cut, or the run is not cut at given keyframes.
    #[test]
    fn a_run_knows_where_its_forced_keyframes_end() {
        // Keyed every 500 ms for two hours: more than one argument holds.
        let long: Vec<u64> = (0..14_400).map(|n| n * 500).collect();
        let last = args::forced_through(&long, None).expect("cut short");
        let s = aligned_until(Some(&long), None).expect("cut short");
        // Segment `s` (from 1) starts at the last forced keyframe.
        assert_eq!(long[s as usize - 1], last);
        // From near the end the rest fits.
        assert_eq!(
            aligned_until(Some(&long), Some(Duration::from_secs(7_000))),
            None
        );
        let short = [0, 6_000, 12_000];
        assert_eq!(aligned_until(Some(&short), None), None);
        assert_eq!(aligned_until(None, None), None);
    }

    async fn written(job: &EncodeJob) -> u32 {
        let mut progress = job.progress();
        let p = progress
            .wait_for(|p| p.last_segment.is_some())
            .await
            .unwrap()
            .clone();
        p.last_segment.unwrap()
    }

    /// A player reading the lowest rung of a resumed ladder, then
    /// switching up: its init and its next segment come from one run, which
    /// starts at that segment. The init's run started past what the old
    /// rung had written, behind which the player's next segment was, and a
    /// second run started for it: two ffmpeg starts before a picture.
    #[tokio::test]
    async fn a_rung_switch_starts_one_run_where_the_player_reads_on() {
        let (conversion, _tmp) = resumed_ladder("spoolframe-conversion-switch").await;
        let first = conversion.job();
        let start = first.first_segment;
        assert!(start > 90, "a run from 10 min: {start}");
        assert_eq!(written(&first).await, start + 5);
        let segments = conversion.segments().unwrap().clone();
        for n in start..start + 3 {
            let job = conversion.job_for_segment(n, 3).unwrap();
            assert!(Arc::ptr_eq(&job, &first));
            job.reader_at(n, segments.end(n).unwrap());
        }
        let Init::Job(init) = conversion.init_for(1).unwrap() else {
            panic!("rung 1 had no run yet");
        };
        assert_eq!(init.first_segment, start + 3);
        written(&init).await;
        let next = conversion.job_for_segment(start + 3, 1).unwrap();
        assert!(
            Arc::ptr_eq(&next, &init),
            "the next segment started run {} after the init's {}",
            next.epoch,
            init.epoch
        );
    }

    /// A rung's init is kept from the first run of it that wrote one: a
    /// player coming back to the rung after that run is gone has it at
    /// once, and no run starts for it.
    #[tokio::test]
    async fn a_rung_s_init_is_read_from_a_run_that_wrote_it() {
        let (conversion, _tmp) = resumed_ladder("spoolframe-conversion-kept-init").await;
        let lowest = conversion.job();
        written(&lowest).await;
        let Init::Job(up) = conversion.init_for(1).unwrap() else {
            panic!("rung 1 had no run yet");
        };
        written(&up).await;
        // Up again: the rung left is 1 now, and the lowest one's run goes.
        let Init::Job(top) = conversion.init_for(0).unwrap() else {
            panic!("rung 0 had no run yet");
        };
        let epoch = top.epoch;
        let Init::Kept(file) = conversion.init_for(3).unwrap() else {
            panic!("the lowest rung's init was not kept");
        };
        let init = std::fs::read_to_string(&file).unwrap();
        assert!(init.contains("-b:v 1000000 "), "{init}");
        assert_eq!(conversion.job().epoch, epoch, "a run started for the init");
        assert_eq!(conversion.rung(), 0);
        // One line a switch: those to new runs once their first segment
        // is written, with how long it took. Read from what the conversion
        // logged, not captured from the log: tracing caches a callsite's
        // interest process-wide, and another test's thread could leave
        // this one's lines unseen.
        let switches = || -> Vec<Switch> {
            conversion
                .switches
                .lock()
                .iter()
                .filter(|s| s.from != s.to)
                .cloned()
                .collect()
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while switches().len() < 3 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let switches = switches();
        assert_eq!(switches.len(), 3, "{switches:#?}");
        assert!(
            switches.iter().any(|s| (s.from, s.to) == (3, 1)
                && s.asked == "init"
                && s.init == "new run"
                && s.ready_ms.is_some()),
            "{switches:#?}"
        );
        assert!(
            switches
                .iter()
                .any(|s| (s.from, s.to) == (0, 3) && s.init == "kept"),
            "{switches:#?}"
        );
    }

    /// A remux ladder's encoded rung of a two-hour film keyed every
    /// 500 ms: a run's list of keyframes to force stops at what one
    /// argument holds, near the end. A segment past its last is a new
    /// run's, from there, not the old run's, which cut it where its
    /// encoder keyed; one before it is still the old run's.
    #[tokio::test]
    async fn a_run_s_segments_past_its_forced_keyframes_are_a_new_run_s() {
        use spoolframe_media::VideoStream;
        use spoolframe_media::encoders::{Hardware, VideoEncoder};
        use spoolframe_types::VideoCodec;

        let tmp = spoolframe_test_dir::TestDir::new("spoolframe-conversion-forced-list").await;
        // Says it wrote far ahead: every segment is "written".
        let ffmpeg = job::tests::ffmpeg(
            &tmp.as_ref().join("bin"),
            "for a; do last=$a; done; d=$(dirname \"$last\")\n\
             printf i > \"$d/init.mp4\"\n\
             printf '#EXTM3U\\nseg20000.m4s\\n' > \"$last\"\n\
             echo out_time_us=36000000\n\
             exec sleep 30",
        );
        let jobs = Arc::new(
            Jobs::new(
                JobLimits {
                    budget: Units::jobs(4),
                    max_remux: 32,
                    max_gpu_jobs: 3,
                    scratch_dir: tmp.as_ref().join("scratch"),
                    scratch_limit_bytes: 1 << 30,
                    read_ahead: Duration::from_secs(36_000),
                    stalled_after: Duration::from_secs(60),
                    gpu_filters: false,
                    tone_map: spoolframe_media::encoders::ToneMapSetting::Auto,
                    video_codecs: crate::config::DEFAULT_VIDEO_CODECS.to_vec(),
                },
                spoolframe_media::sandbox::Tool::unconfined(&ffmpeg),
            )
            .unwrap(),
        );
        let film = tmp.as_ref().join("film.mkv");
        std::fs::write(&film, b"x").unwrap();
        let descriptor = Arc::new(MediaDescriptor {
            container: "matroska,webm".into(),
            duration: Some(Duration::from_secs(7200)),
            bit_rate: Some(30_000_000),
            video: Some(VideoStream {
                index: 0,
                codec: "h264".into(),
                profile: Some("High".into()),
                level: None,
                width: 1920,
                height: 1080,
                pixel_format: Some("yuv420p".into()),
                color_transfer: None,
                frame_rate_millis: Some(24_000),
                bit_depth: 8,
                hdr: None,
                dolby_vision: None,
                bit_rate: None,
            }),
            audio: Vec::new(),
            subtitles: Vec::new(),
        });
        let encode = ConvertedVideo::Encode {
            codec: VideoCodec::H264,
            height: 720,
            max_bitrate: Some(4_000_000),
            tone_map: false,
        };
        let plan = PlaybackPlan::Converted {
            delivery: Delivery::Hls,
            container: spoolframe_types::Container::Mp4,
            video: Some(ConvertedVideo::Copy {
                codec: VideoCodec::H264,
                dolby_vision: false,
            }),
            audio: None,
            subtitle: None,
            ladder: vec![
                Rung {
                    height: 1080,
                    max_bitrate: 30_000_000,
                    video: None,
                },
                Rung {
                    height: 720,
                    max_bitrate: 4_000_000,
                    video: Some(encode),
                },
            ],
            reasons: Vec::new(),
        };
        let encoders = Encoders {
            video: vec![VideoEncoder {
                codec: VideoCodec::H264,
                name: "h264_vaapi".into(),
                hardware: Some(Hardware::Vaapi),
            }],
            ..Encoders::software_baseline()
        };
        let keyframes: Vec<u64> = (0..14_400).map(|n| n * 500).collect();
        let conversion = Conversion::start(
            jobs,
            ConversionSource {
                handle: "h1",
                path: &film,
                descriptor,
                plan: &plan,
                gpus: one_gpu(encoders, &ffmpeg),
                keyframes: Some(Arc::new(keyframes.clone())),
                first_epoch: 0,
                slot: None,
                start_rung: 1,
            },
            None,
        )
        .unwrap();
        let first = conversion.job();
        written(&first).await;
        let segments = conversion.segments().unwrap().clone();
        let last = args::forced_through(&segments.starts_ms, None).expect("a list cut short");
        // About 1:55:50, as the review found.
        assert!((6_900_000..7_000_000).contains(&last), "{last}");
        let past = segments.starts_ms.partition_point(|&ms| ms < last) as u32 + 1;
        // Worked out once, when the run started, not on every read.
        assert_eq!(first.aligned_until, Some(past));
        let before = conversion.job_for_segment(past - 1, 1).unwrap();
        assert!(
            Arc::ptr_eq(&before, &first),
            "a segment it forced is its own"
        );
        let after = conversion.job_for_segment(past, 1).unwrap();
        assert!(
            !Arc::ptr_eq(&after, &first),
            "segment {past} came from the run whose cuts stopped at {last} ms"
        );
        assert_eq!(after.first_segment, past);
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
                    video_codecs: crate::config::DEFAULT_VIDEO_CODECS.to_vec(),
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
                    video_codecs: crate::config::DEFAULT_VIDEO_CODECS.to_vec(),
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
                dolby_vision: None,
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
                    video_codecs: crate::config::DEFAULT_VIDEO_CODECS.to_vec(),
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
                dolby_vision: None,
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
                    video_codecs: crate::config::DEFAULT_VIDEO_CODECS.to_vec(),
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
                dolby_vision: None,
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
                    video_codecs: crate::config::DEFAULT_VIDEO_CODECS.to_vec(),
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
                dolby_vision: None,
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
            dolby_vision: false,
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
            dolby_vision: false,
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
            dolby_vision: false,
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
