//! Starting a conversion's runs (P2-03, PGPU-05): where they go, what
//! they weigh, their inputs and fallback, and their `conversion` span.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use spoolframe_media::MediaDescriptor;
use spoolframe_media::encoders::Hardware;
use spoolframe_media::sandbox::GpuUse;
use spoolframe_protocol::{AudioAction, ConvertedAudio, ConvertedVideo};
use spoolframe_types::Delivery;

use super::args::{Cuts, Decode, EncodeSpec, Inputs, Output, decode, ffmpeg_args};
use super::job::{EncodeJob, Fallback, JobError, Jobs, Run, Slot, Written};
use super::pick::RunOn;
use crate::playback::PlaybackTrace;
use crate::transcode::Gpu;

use super::*;

/// Where `source`'s runs go: the GPU [`pick::choose`] picks for `probe`,
/// its first run (PGPU-05), passing over a card with no room for another
/// GPU encode, and with none left a software encode ([`pick::spill`]).
/// The card of the conversion it replaces has room: that one lets go of
/// its place as this one starts.
///
/// Neither a spill nor a refusal is silent: a spill is logged once, as the
/// conversion starts, with the encoder it runs on; an encode that keeps
/// HDR and finds no 10-bit encoder with room is refused `Busy`, with why.
pub(crate) fn run_on(
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

/// Where the job's link `link` leads, or why that cannot be read.
pub(crate) fn link_target(link: &Path) -> Result<PathBuf, JobError> {
    std::fs::read_link(link)
        .map_err(|e| JobError::Prepare(format!("cannot read the link {}: {e}", link.display())))
}

#[expect(
    clippy::too_many_arguments,
    reason = "the call sites name each part; a struct would only repeat them"
)]
pub(super) fn spawn(
    jobs: &Jobs,
    dir: &Path,
    file: &Path,
    inputs: Inputs<'_>,
    descriptor: &MediaDescriptor,
    (delivery, video, audio, subtitle): Parts,
    run_on: &RunOn,
    epoch: u32,
    start: Option<Duration>,
    start_number: u32,
    slot: Option<Slot>,
    cuts: Cuts,
    pad_audio: bool,
    why: RunWhy<'_>,
) -> Result<Arc<EncodeJob>, JobError> {
    let job_dir = dir.join(epoch.to_string());
    let aligned_until = aligned_until(&cuts, start, start_number);
    let (output, written) = match delivery {
        Delivery::Hls => (
            Output::Hls {
                dir: job_dir.clone(),
                start_number,
                cuts,
                pad_audio,
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
    let (gpu_filters, tone_map) = jobs.filters();
    let spec = EncodeSpec {
        inputs,
        descriptor,
        video,
        audio,
        subtitle,
        encoders: run_on.encoders(),
        vaapi_device: run_on.vaapi_device(),
        hardware_decode: !jobs.hardware_decode_failed(file),
        gpu_filters,
        tone_map,
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
    let vulkan = args::vulkan(&spec);
    let fallback = software_fallback(&spec, file, encoder).map_err(JobError::Prepare)?;
    let made = Some(args::made(&spec));
    let first_segment = if delivery == Delivery::Hls {
        start_number
    } else {
        0
    };
    let start_ms = start.map_or(0, |s| s.as_millis() as u64);
    // ffmpeg reads the job's links (PX-11 finding 34): the conversion's
    // directory, and the files they lead to, bound in confined.
    let read = run_inputs(inputs)?;
    let load = load(video, audio, gpu_encode, args::all_on_gpu(&spec));
    let span = conversion_span(
        &why,
        &spec,
        delivery,
        made.as_ref(),
        run_on.gpu().filter(|_| gpu != GpuUse::None),
        load,
        (epoch, start_ms),
    );
    let started = jobs.start(
        job_dir,
        Run {
            args,
            inputs: read,
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
            span: Some(span.clone()),
        },
        written,
        load,
        epoch,
        first_segment,
        start_ms,
        slot,
    );
    if let Err(e) = &started {
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;
        let outcome = match e {
            JobError::Busy => "busy",
            JobError::Prepare(_) => "prepare_failed",
            JobError::Spawn(_) => "spawn_failed",
        };
        span.set_attribute("outcome", outcome);
    } else {
        jobs.started_for(why.reason);
    }
    started
}

/// What a run's links `inputs` lead to: the files it reads that someone
/// else may have put there. A link that cannot be read is a failure to prepare, said as
/// such, not an input left out of the jail for ffmpeg to fail on in its
/// own words. Only a VobSub's pictures may be missing: an .idx names
/// them, other subtitle files have none.
pub(crate) fn run_inputs(links: Inputs<'_>) -> Result<Vec<PathBuf>, JobError> {
    let vobsub_pictures = links.sidecar.map(|s| s.with_extension("sub"));
    let mut inputs = vec![link_target(links.film)?];
    if let Some(sidecar) = links.sidecar {
        inputs.push(link_target(sidecar)?);
    }
    if let Some(copy) = links.audio {
        inputs.push(link_target(copy)?);
    }
    if let Some(pictures) = vobsub_pictures.as_deref()
        && pictures.symlink_metadata().is_ok()
    {
        inputs.push(link_target(pictures)?);
    }
    Ok(inputs)
}

/// `spec` decoding in software, its tone map on the CPU: decoding on the
/// GPU may fail for a stream detection's test did not cover, and
/// libplacebo for a GPU or a stream its test did not (PGPU-04). `None`
/// for a run that decodes in software and uses no Vulkan already. A
/// conversion's run and a copy's (PX-37) each make their arguments from
/// it; it still encodes on its encoder's GPU ([`fallback_gpu`]).
pub(crate) fn software_decode<'a>(spec: &EncodeSpec<'a>) -> Option<EncodeSpec<'a>> {
    match decode(spec) {
        Decode::Software if args::vulkan(spec).is_none() => None,
        Decode::Software | Decode::Hardware | Decode::Gpu => Some(EncodeSpec {
            hardware_decode: false,
            ..spec.clone()
        }),
    }
}

/// The GPU a run decoding in software opens: its encoder's, if any.
pub(crate) fn fallback_gpu(encoder: Option<Hardware>) -> GpuUse {
    encoder.map_or(GpuUse::None, Hardware::gpu)
}

/// The conversion's run of `spec` decoding in software
/// ([`software_decode`]), for `file`. The error is why its arguments
/// cannot be made.
fn software_fallback(
    spec: &EncodeSpec<'_>,
    file: &Path,
    encoder: Option<Hardware>,
) -> Result<Option<Fallback>, String> {
    software_decode(spec)
        .map(|software| {
            Ok(Fallback {
                args: ffmpeg_args(&software)?,
                gpu: fallback_gpu(encoder),
                file: file.to_path_buf(),
                made: Some(args::made(&software)),
            })
        })
        .transpose()
}

/// Why a run starts, for its `conversion` span.
pub(super) struct RunWhy<'a> {
    pub(super) playback: &'a PlaybackTrace,
    /// `start`, `change` (the playback's subtitle or quality changed),
    /// `seek`, `switch` (to another rung) or `restart` (at a segment far
    /// from where the run was).
    pub(super) reason: &'static str,
    pub(super) rung: u32,
}

/// A run's `conversion` span, from ffmpeg's spawn to its exit or kill: a
/// root linked to its playback's start, as a conversion can last a whole
/// film. What it is: why it started, where, with which encoder, on which
/// GPU, decoding how, through which filters. How it went is added as it
/// runs ([`job::EncodeJob`]).
fn conversion_span(
    why: &RunWhy<'_>,
    spec: &EncodeSpec<'_>,
    delivery: Delivery,
    made: Option<&Made>,
    gpu: Option<&Arc<Gpu>>,
    load: Load,
    (epoch, start_ms): (u32, u64),
) -> tracing::Span {
    let encoder = made.and_then(|m| m.encoder.as_ref());
    let decode = match made.map(|m| m.decoder) {
        None | Some(Decoder::None) => "none",
        Some(Decoder::Software) => "sw",
        Some(Decoder::Hardware(_) | Decoder::Vulkan) => "hw",
    };
    let filters: Vec<String> = made
        .into_iter()
        .flat_map(|m| [("tonemap", m.tone_map), ("burn_in", m.burn_in)])
        .filter_map(|(filter, on)| match on {
            FilterOn::None => None,
            FilterOn::Gpu => Some(format!("{filter}:gpu")),
            FilterOn::Cpu => Some(format!("{filter}:cpu")),
        })
        .collect();
    let span = tracing::info_span!(
        parent: None,
        "conversion",
        conversion.reason = why.reason,
        conversion.rung = i64::from(why.rung),
        conversion.epoch = i64::from(epoch),
        conversion.start_ms = start_ms as i64,
        conversion.delivery = %delivery,
        conversion.plan = if encodes(spec.video, spec.audio) { "transcode" } else { "remux" },
        conversion.load = load.as_str(),
        conversion.encoder = encoder.map_or("copy", |e| e.name.as_str()),
        conversion.hardware = if encoder.is_some_and(|e| e.hardware.is_some()) { "gpu" } else { "cpu" },
        conversion.gpu = gpu.map(|g| g.card().place()).as_deref(),
        conversion.decode = decode,
        conversion.filters = if filters.is_empty() { "none".to_owned() } else { filters.join(",") },
        conversion.tone_mapper = made.and_then(|m| m.tone_mapper).map(|t| t.label()),
    );
    why.playback.mark(&span);
    span
}

/// What a conversion weighs: its video encode, on the GPU or not, and if
/// on the GPU all of it there (`all_gpu`: `args::all_on_gpu`) or not, if
/// it has one (a burned-in subtitle is one); else an audio encode; else
/// a remux.
///
/// One all on the GPU whose decode fails and runs again in software
/// keeps its weight until it ends: that is rare, and the film decodes in
/// software from the next run on.
pub(crate) fn load(
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
pub(super) fn encodes(video: Option<ConvertedVideo>, audio: Option<ConvertedAudio>) -> bool {
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

    /// A link that cannot be read fails the preparation, naming it.
    #[tokio::test]
    async fn a_link_that_cannot_be_read_fails_the_preparation() {
        let dir = spoolframe_test_dir::TestDir::new("spoolframe-encode-links").await;
        let link = dir.as_ref().join("input");
        std::os::unix::fs::symlink("/somewhere/film.mkv", &link).unwrap();
        assert_eq!(
            link_target(&link).unwrap(),
            PathBuf::from("/somewhere/film.mkv")
        );
        let missing = dir.as_ref().join("missing");
        let Err(JobError::Prepare(why)) = link_target(&missing) else {
            panic!("a missing link is a failure to prepare");
        };
        assert!(why.contains("missing"), "{why}");
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
}
