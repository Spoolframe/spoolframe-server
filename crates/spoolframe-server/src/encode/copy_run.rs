//! Where a copy's ffmpeg runs and what it weighs (PX-37): the GPU picked
//! as for a conversion (PGPU-05), its arguments ([`copy_args`]), and the
//! same run decoding in software for when decoding on the GPU fails.

use std::path::Path;

use spoolframe_media::MediaDescriptor;
use spoolframe_media::sandbox::{GpuUse, Tool, Vulkan};

use super::args::{self, EncodeSpec, Inputs, Output};
use super::copy_args::copy_args;
use super::job::{CardLimit, JobError, Jobs, Load};
use super::pick;
use super::run::{fallback_gpu, load, software_decode};
use crate::store::optimized::CopyVideo;
use crate::transcode::{Detected, Gpu};

/// What a copy is made from, and where it is written.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CopyInput<'a> {
    /// The media file, as a GPU decode failure is remembered by.
    pub(crate) file: &'a Path,
    /// The pool's link to it, which ffmpeg reads (PX-11).
    pub(crate) input: &'a Path,
    pub(crate) descriptor: &'a MediaDescriptor,
    pub(crate) video: CopyVideo,
    /// The copy's `.part`.
    pub(crate) part: &'a Path,
}

/// One ffmpeg run of a copy: its arguments and the GPU and Vulkan its
/// jail opens.
#[derive(Debug, Clone)]
pub(crate) struct Attempt {
    pub(crate) args: Vec<String>,
    /// The GPU it decodes or encodes on, if any.
    pub(crate) gpu: GpuUse,
    pub(crate) vulkan: Option<Vulkan>,
}

/// A copy's run, ready to launch.
#[derive(Debug, Clone)]
pub(crate) struct CopyRun {
    pub(crate) first: Attempt,
    /// For a run that decodes or tone maps on the GPU: the same run
    /// decoding in software ([`software_decode`]), still encoding on the
    /// encoder's GPU.
    pub(crate) software: Option<Attempt>,
    pub(crate) load: Load,
    /// For a GPU video encode, the card it counts against.
    pub(crate) card: Option<CardLimit>,
    /// The card's ffmpeg, confined to its nodes, or the jobs' own.
    pub(crate) ffmpeg: Tool,
}

/// `copy`'s run on what `detected` found. `Busy` when an encode that
/// keeps HDR finds no 10-bit encoder with room, as a playback would be:
/// the copy waits rather than being written in 8 bits.
pub(crate) fn prepare(
    jobs: &Jobs,
    detected: &Detected,
    copy: &CopyInput<'_>,
) -> Result<CopyRun, JobError> {
    let (gpu_filters, tone_map) = jobs.filters();
    let probe = EncodeSpec {
        inputs: Inputs::film(copy.input),
        descriptor: copy.descriptor,
        video: Some(copy.video.into()),
        audio: None,
        subtitle: None,
        encoders: &detected.software,
        vaapi_device: Path::new(""),
        hardware_decode: !jobs.hardware_decode_failed(copy.file),
        gpu_filters,
        tone_map,
        start: None,
        output: Output::Progressive {
            file: copy.part.to_path_buf(),
        },
    };
    let room = |gpu: &Gpu| jobs.card_has_room(&gpu.limit());
    let on = pick::run_on(detected, &probe, &room).map_err(|pick::NoTenBit| JobError::Busy)?;
    let spec = on.spec(&probe);
    let args = copy_args(&spec).map_err(JobError::Prepare)?;
    let placement = args::placement(&spec);
    let encoder = placement.and_then(|place| place.encoder.hardware);
    let gpu = placement.map_or(GpuUse::None, |place| place.gpu());
    let vulkan = args::vulkan(&spec);
    let software = software_decode(&spec)
        .map(|software| {
            Ok::<_, JobError>(Attempt {
                args: copy_args(&software).map_err(JobError::Prepare)?,
                gpu: fallback_gpu(encoder),
                vulkan: None,
            })
        })
        .transpose()?;
    Ok(CopyRun {
        first: Attempt { args, gpu, vulkan },
        software,
        load: load(spec.video, None, encoder.is_some(), args::all_on_gpu(&spec)),
        card: on.gpu().filter(|_| encoder.is_some()).map(|g| g.limit()),
        ffmpeg: on
            .gpu()
            .map_or_else(|| jobs.ffmpeg().clone(), |g| g.ffmpeg.clone()),
    })
}
