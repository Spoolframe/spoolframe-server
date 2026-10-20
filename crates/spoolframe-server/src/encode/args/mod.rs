//! ffmpeg's arguments for a converted plan (P2-03).
//!
//! Filters run on the CPU and the frames go to the encoder after them, so
//! scaling, tone mapping and burn-in work with any encoder; a VA-API
//! encoder gets its frames uploaded at the end of the chain. The video is
//! decoded on the GPU where it can be ([`decode`]), the frames coming
//! back to memory for those filters, or staying on the GPU for a VA-API
//! or NVENC encode whose filters all run there. An HDR source's tone map
//! may run on the GPU with libplacebo (PGPU-04, [`libplacebo`]), which
//! scales too, through Vulkan.

mod argv;
mod graph;
mod place;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::time::Duration;

use spoolframe_media::MediaDescriptor;
use spoolframe_media::encoders::{Encoders, ToneMapSetting};
use spoolframe_protocol::{ConvertedAudio, ConvertedSubtitle, ConvertedVideo};

pub use self::argv::*;
pub(crate) use self::graph::*;
pub use self::place::*;

/// HLS segment length (P2-05): Apple's recommendation.
pub const SEGMENT_SECS: u32 = 6;

/// An encoded video's first segment (PX-26), and its second: what the
/// player waits for before the first frame. See `hls::Segments::encoded`.
pub const FIRST_SEGMENT_SECS: u32 = 2;

/// ffmpeg's own list of the segments it has finished, in the job's
/// directory: the HLS muxer's playlist, or the segment muxer's list.
pub const PLAYLIST: &str = "ffmpeg.m3u8";

/// Whether `spec`'s run starts at the film's start: a run of HLS
/// segment 1, which cuts the encoded grid's short head.
fn from_start(spec: &EncodeSpec<'_>) -> bool {
    spec.start.is_none_or(|start| start.is_zero())
}

/// `-force_key_frames` for an HLS encode: a keyframe at each cut of the
/// grid, from the run's first frame on. ffmpeg's `t` is the time since
/// that frame, the same origin the HLS muxer measures its cuts from, so
/// each target is where the muxer cuts and the keyframes do not drift off
/// the grid (`prev_forced_t + 6` lands a frame late each time: at
/// 23.976 fps, 6 ms a segment, 7 s over two hours). A run from 0 cuts at
/// 0, 2, 4, 8, 14… s (PX-26); any other at 0, 6, 12… s from its start.
fn forced_keyframes(from_start: bool) -> String {
    let (first, step) = (FIRST_SEGMENT_SECS, SEGMENT_SECS);
    if from_start {
        // The n-th forced keyframe (from 0) at `first × n` for n < 3,
        // then at `first + step × (n - 2)`.
        format!("expr:gte(t,if(lt(n_forced,3),{first}*n_forced,{first}+{step}*(n_forced-2)))")
    } else {
        format!("expr:gte(t,{step}*n_forced)")
    }
}

/// The most of `-force_key_frames`' list one run is given, in bytes: one
/// argument is held to 128 KiB by Linux (`MAX_ARG_STRLEN`). A two-hour
/// film keyed every second is about 60 KiB from its start; a run of a
/// longer or denser one stops forcing past the list's end, and a player
/// switching rungs there may land a little off until the next seek.
const MAX_KEYFRAME_LIST: usize = 120 * 1024;

/// `-force_key_frames` at `cuts` (the film's times, ms) from the run's
/// start on. The times are the output's: the film's own after a seek
/// (`-copyts`), and from 0 for a run from the start, as the copy's are.
fn keyframes_at(cuts: &[u64], start: Option<Duration>) -> String {
    forced_list(cuts, start).0
}

/// The last of `cuts` a run from `start` forces a keyframe at, when its
/// list ([`keyframes_at`]) stops short of the last cut: past it the run
/// cuts where its encoder keys, not where the playlist says, and its
/// segments there are not to be read (`Conversion::job_for_segment`
/// starts another run, with a list of its own). `None` when the list
/// holds every cut from `start` on.
pub(crate) fn forced_through(cuts: &[u64], start: Option<Duration>) -> Option<u64> {
    forced_list(cuts, start).1
}

fn forced_list(cuts: &[u64], start: Option<Duration>) -> (String, Option<u64>) {
    let from = start.map_or(0, |d| d.as_millis() as u64);
    time_list(cuts.iter().filter(|&&ms| ms >= from), 0)
}

/// `cuts` as ffmpeg's comma-separated seconds, each less `base_ms`, as
/// many as one argument holds ([`MAX_KEYFRAME_LIST`]); and the last
/// listed when the list stops short of them all.
fn time_list<'a>(cuts: impl Iterator<Item = &'a u64>, base_ms: i64) -> (String, Option<u64>) {
    let mut list = String::new();
    let mut last = None;
    for &ms in cuts {
        let at = (ms as i64).saturating_sub(base_ms).max(0) as u64;
        let entry = format!("{}.{:03}", at / 1000, at % 1000);
        if list.len() + entry.len() + 1 > MAX_KEYFRAME_LIST {
            return (list, last);
        }
        if !list.is_empty() {
            list.push(',');
        }
        list.push_str(&entry);
        last = Some(ms);
    }
    (list, None)
}

/// A cut past any film (`MAX_DURATION` is 48 hours): the segment
/// muxer's list for a run of the last segment, which has no cut after it.
const NO_CUT: &str = "1000000.000";

/// `-segment_times` for a run of a copied video from segment `from` of
/// `starts` (numbered from 1), cut by the segment muxer. It measures each time from
/// the run's first video packet: the segment's own cut for a run from a
/// seek (which starts exactly there, `segment_seek`), the film's first
/// keyframe (`from_ms`, before 0 in an MP4 cut with stream copy) for a run
/// from the start. Only the cuts after the run's first segment: the muxer
/// cuts at its first time once a segment has a packet.
fn segment_times(starts: &[u64], from: u32, from_ms: i64) -> (String, Option<u64>) {
    let first = from.max(1) as usize;
    let base = match first {
        1 => from_ms,
        n => starts.get(n - 1).map_or(from_ms, |&ms| ms as i64),
    };
    time_list(starts.iter().skip(first), base)
}

/// The last of a listed copy's cuts a run from segment `from` is given,
/// when its list ([`segment_times`]) stops short of the last cut: the
/// segment from there is not cut where the playlist says. `None` when the
/// list holds every later cut.
pub(crate) fn listed_through(starts: &[u64], from: u32, from_ms: i64) -> Option<u64> {
    segment_times(starts, from, from_ms).1
}

/// What keeps `encoder` from adding keyframes of its own between the
/// forced ones: a scene cut or a GOP's end would be a cut the playlist
/// (or the copy) does not have. The GOP is long (40 s at 25 fps) rather
/// than unbounded: a source keyed less often than that is cut where the
/// copy is not. Each encoder in its own words: ffmpeg's `-sc_threshold`
/// reaches libx264 alone, and every other encoder reports it unused.
///
/// - libx265: `scenecut=0`, and `open-gop=0`. Its forced keyframes are
///   otherwise CRA pictures (`-forced-idr` notwithstanding), whose
///   leading pictures refer to the segment before: a player starting at
///   one, after a switch or a seek, cannot decode them.
/// - libsvtav1: `scd=0`, its scene change detection (off by default).
/// - NVENC: `-no-scenecut`, and IDR at the forced ones.
/// - VA-API and QSV add no keyframe at a scene change (measured on a clip
///   cut every 1.5 s): the long GOP is all they need.
fn only_forced_keyframes(encoder: &str) -> Vec<String> {
    let mut args = vec![s("-g"), s("1000")];
    match encoder {
        "libx264" => args.extend([s("-sc_threshold"), s("0")]),
        "libx265" => args.extend([s("-x265-params"), s("scenecut=0:open-gop=0")]),
        "libsvtav1" => args.extend([s("-svtav1-params"), s("scd=0")]),
        nvenc if nvenc.ends_with("_nvenc") => {
            args.extend([s("-no-scenecut"), s("1"), s("-forced-idr"), s("1")]);
        }
        _ => {}
    }
    args
}

/// For an HLS run of an encoded video from a seek, a copied audio track
/// cut to start where the video does. A seek lands the demuxer on the
/// source's keyframe before the time, and a copied track starts there,
/// up to a source GOP early (1.6 s in American Sniper, 10 s in a film
/// keyed that rarely), while the decoded video starts at the time. The
/// HLS muxer measures its cuts from the first packet it gets, the audio:
/// its thresholds came that much early, so a cut landed on whatever
/// keyframe came first past one, and with a lead over a segment's length
/// the run began with a segment of audio alone and every number after it
/// was one off the playlist. The `noise` filter drops the packets before
/// the time: the rest keep their own times (`-copyts`), where an output
/// `-ss` would move them back to 0.
fn copied_audio_from_start(spec: &EncodeSpec<'_>) -> Vec<String> {
    let encoded = matches!(spec.video, Some(ConvertedVideo::Encode { .. }));
    match (&spec.output, spec.start) {
        (Output::Hls { .. }, Some(start)) if encoded && !start.is_zero() => vec![
            s("-bsf:a"),
            // `\,`: a bare comma would end the filter's options.
            format!("noise=drop=lt(pts*tb\\,{})", seconds(start)),
        ],
        _ => Vec::new(),
    }
}

/// Where an HLS run cuts its segments.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Cuts {
    /// Where the HLS muxer cuts by itself: an encode on its own grid, a
    /// copied video at every keyframe (its parameter sets only in the
    /// stream, or a sound the segment muxer cannot write first).
    #[default]
    Own,
    /// An encode with a keyframe forced at each of these and nowhere
    /// else, the HLS muxer cutting at each: a remux ladder's lower rung,
    /// cut where its copy is (PX-21). The film's times, ms.
    Forced(std::sync::Arc<[u64]>),
    /// A copied video cut by ffmpeg's segment muxer at these, the film's
    /// times in ms (`hls::Segments::copied`, review 2026-10-11 M2). A run
    /// from the start begins at `from_ms`, the film's first keyframe.
    Listed {
        starts: std::sync::Arc<[u64]>,
        from_ms: i64,
    },
}

/// Where and how the job writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    /// One Matroska file, readable while it grows (P2-04).
    Progressive { file: PathBuf },
    /// fMP4 segments `seg<n>.m4s` and `init.mp4` in `dir`, numbered from
    /// `start_number` (P2-05), cut as `cuts` says.
    Hls {
        dir: PathBuf,
        start_number: u32,
        cuts: Cuts,
        /// The encoded audio is filled with silence from where the run
        /// starts, for a track whose first packet comes after the run's
        /// first cut (`encode::late_audio`).
        pad_audio: bool,
    },
}

/// The files an encode reads, each a job's link: plain paths (see
/// `filter_path`), and the files under them may be someone else's. Which
/// of them ffmpeg reads as which input is [`input_index`]'s to say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inputs<'a> {
    /// The film.
    pub film: &'a Path,
    /// The selected external subtitle, if it is one; the descriptor's
    /// own path is not used, for the same reason.
    pub sidecar: Option<&'a Path>,
    /// An optimized copy's stored video (PX-37's track store), copied:
    /// `descriptor`'s video is its, stream 0 of its file. `None`: the
    /// video is the film's.
    pub video: Option<&'a Path>,
    /// An optimized copy's stored audio track the picked audio is (PX-37):
    /// `descriptor`'s track at `audio`'s index is its, stream 0 of its
    /// file. `None`: the audio is the film's.
    pub audio: Option<&'a Path>,
}

impl<'a> Inputs<'a> {
    /// The film alone.
    pub fn film(film: &'a Path) -> Self {
        Self {
            film,
            sidecar: None,
            video: None,
            audio: None,
        }
    }

    /// Whether it reads a stored track, whose times are the film's own:
    /// every input then keeps its own (`-copyts`), or ffmpeg would move
    /// each input's to start at 0 and a late track would lose its lead.
    pub fn stored(&self) -> bool {
        self.video.is_some() || self.audio.is_some()
    }
}

/// One encode: the plan's parts and what they run on.
#[derive(Debug, Clone)]
pub struct EncodeSpec<'a> {
    /// The files it reads.
    pub inputs: Inputs<'a>,
    pub descriptor: &'a MediaDescriptor,
    pub video: Option<ConvertedVideo>,
    pub audio: Option<ConvertedAudio>,
    pub subtitle: Option<ConvertedSubtitle>,
    pub encoders: &'a Encoders,
    pub vaapi_device: &'a Path,
    /// The video may be decoded, and tone mapped by libplacebo, on the
    /// GPU: false once that failed for this file (see [`decode`]).
    pub hardware_decode: bool,
    /// Run the filters on the GPU where it can (`transcode.gpu_filters`):
    /// VA-API's tone map and burn-in, NVIDIA's scale and burn-in (PGPU-03).
    /// Under `tone_map = "auto"`, the tone map on the GPU too.
    pub gpu_filters: bool,
    /// How HDR is tone mapped (`transcode.tone_map`, PGPU-04).
    pub tone_map: ToneMapSetting,
    pub start: Option<Duration>,
    pub output: Output,
}

/// Where the video is decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decode {
    /// In software.
    Software,
    /// On the GPU, with the frames back in memory for the filters on the
    /// CPU.
    Hardware,
    /// On the GPU, and the frames stay there: filtered on it and encoded
    /// from it, for a video that needs no filter on the CPU.
    Gpu,
}
