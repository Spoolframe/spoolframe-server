//! The Server page's GPUs (PGPU-05): each card encodes run on, as the
//! server's start lines say it; the host's other cards and why they do
//! nothing; and where typical conversions would run, asked of the jobs'
//! own pick ([`crate::encode::pick::routes`]).

use std::collections::HashMap;

use spoolframe_admin_api as api;
use spoolframe_media::encoders::{Hardware, ToneMapSetting};
use spoolframe_media::gpus::Kind;
use spoolframe_types::VideoCodec;

use crate::encode::args::Share;
use crate::encode::pick::{self, RouteJob};
use crate::playback_debug::{encoder_used, filter_place, gpu_api, tone_mapper, video_decoder};
use crate::transcode::{Detected, Gpu, OtherWhy};

/// The cards encodes run on, in the order jobs try them, with the GPU
/// video encodes running on each now (`running`, by card key) and the
/// limit each is held to (`max_gpu_jobs` where its entry sets none).
pub fn cards(
    detected: &Detected,
    entries: &[spoolframe_media::gpus::GpuEntry],
    running: &HashMap<String, usize>,
    max_gpu_jobs: usize,
) -> Vec<api::GpuCard> {
    detected
        .gpus
        .iter()
        .map(|gpu| card(gpu, entries, running, max_gpu_jobs))
        .collect()
}

fn card(
    gpu: &Gpu,
    entries: &[spoolframe_media::gpus::GpuEntry],
    running: &HashMap<String, usize>,
    max_gpu_jobs: usize,
) -> api::GpuCard {
    let found = &gpu.found;
    let card = &found.card;
    let api = card.api;
    let encoders = &found.encoders;
    let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
    let key = card.key();
    let does = crate::transcode::card_does(encoders, api);
    api::GpuCard {
        jobs: count(running.get(&key).copied().unwrap_or(0)),
        key,
        name: found.name.clone(),
        api: gpu_api(api),
        place: card.place(),
        kind: kind(card.host.class.kind),
        classified: card.host.class.how.clone(),
        entry: crate::transcode::entry_label(entries, card.entry),
        max_jobs: count(gpu.max_jobs.unwrap_or(max_gpu_jobs)),
        own_limit: gpu.max_jobs.is_some(),
        encodes: does.encodes,
        decodes: does.decodes,
        filters: does.filters.into_iter().map(str::to_owned).collect(),
        tone_maps: does.tone_mappers.into_iter().map(tone_mapper).collect(),
        lacks: lacks(encoders, api),
    }
}

/// What `api`'s failed tests say it lacks, a filter named as the API's
/// own: VA-API's `tone map` is `tonemap_vaapi`, which libplacebo does not
/// need, so "VA-API tone map (tonemap_vaapi)" sits beside "Tone maps:
/// libplacebo (own)" without contradicting it.
fn lacks(encoders: &spoolframe_media::encoders::Encoders, api: Hardware) -> Vec<String> {
    let name = match api {
        Hardware::Vaapi => "VA-API",
        Hardware::Nvenc => "NVIDIA",
        Hardware::Qsv => "Quick Sync",
    };
    encoders
        .failed
        .iter()
        .filter(|f| f.hardware == api)
        .map(|f| match f.lacks.as_str() {
            filter @ ("tone map" | "overlay" | "scale") => format!("{name} {filter} ({})", f.test),
            lacks => lacks.to_owned(),
        })
        .collect()
}

/// The host's GPUs that encode nothing here.
pub fn others(detected: &Detected) -> Vec<api::OtherGpu> {
    detected
        .others
        .iter()
        .map(|other| api::OtherGpu {
            name: other.name.clone(),
            pci: other.host.pci.clone(),
            driver: other.host.driver.clone(),
            kind: kind(other.host.class.kind),
            why: match &other.why {
                OtherWhy::NoEntry => api::OtherGpuWhy::NoEntry,
                OtherWhy::NothingWorks { api, entry, test } => api::OtherGpuWhy::NothingWorks {
                    api: gpu_api(*api),
                    entry: entry.clone(),
                    test: test.clone(),
                },
            },
        })
        .collect()
}

/// The codec a 4K HDR10 film would be converted to now for each kind of
/// device (PGPU-06), asked of the planner itself with what it plans with
/// ([`pick::planning`], every card counted free): an HLS player that
/// plays H.264, HEVC and AV1, showing HDR10 or none, decoding as each
/// kind does in hardware.
pub fn picks(detected: &Detected, video_codecs: &[VideoCodec]) -> Vec<api::CodecPick> {
    use spoolframe_protocol::{ConvertedVideo, PlaybackPlan, Selection};
    use spoolframe_types::{ClientCapabilities, Container, Delivery, HdrFormat, VideoLimits};
    let encoders = pick::planning(detected, video_codecs, &|_| true);
    let film = pick::sample(RouteJob::KeepHdr, VideoCodec::Hevc);
    let all = [VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1];
    [
        (api::PickDevice::HdrAv1, true, all.to_vec()),
        (
            api::PickDevice::HdrHevc,
            true,
            vec![VideoCodec::H264, VideoCodec::Hevc],
        ),
        (api::PickDevice::Sdr, false, all.to_vec()),
    ]
    .into_iter()
    .map(|(device, hdr, hardware)| {
        let caps = ClientCapabilities {
            video: all.into(),
            containers: [Container::Mp4].into(),
            delivery: vec![Delivery::Hls],
            // 1080p, so a 4K film is converted.
            max_video: Some(VideoLimits {
                height: Some(1080),
                hdr: Some(if hdr {
                    [HdrFormat::Hdr10].into()
                } else {
                    Default::default()
                }),
                ..VideoLimits::default()
            }),
            hardware_video: Some(hardware.into_iter().collect()),
            ..ClientCapabilities::default()
        };
        let plan = crate::planner::plan(
            &film,
            Some(&caps),
            Selection {
                audio: None,
                subtitle: None,
            },
            crate::planner::Bitmap::Text,
            &encoders,
            &spoolframe_media::ocr::Ocr::disabled(),
        );
        let (codec, hdr_kept) = match plan {
            PlaybackPlan::Converted {
                video:
                    Some(ConvertedVideo::Encode {
                        codec, tone_map, ..
                    }),
                ..
            } => (Some(codec.as_str().to_owned()), !tone_map),
            _ => (None, false),
        };
        api::CodecPick {
            device,
            codec,
            hdr_kept,
        }
    })
    .collect()
}

/// Where typical conversions would run now ([`pick::routes`]), with the
/// server's GPU filters and tone map setting.
pub fn routes(
    detected: &Detected,
    gpu_filters: bool,
    tone_map: ToneMapSetting,
) -> Vec<api::GpuRoute> {
    pick::routes(detected, gpu_filters, tone_map)
        .into_iter()
        .map(|route| api::GpuRoute {
            job: match route.job {
                RouteJob::Convert => api::RouteJob::Convert,
                RouteJob::ToneMap => api::RouteJob::ToneMap,
                RouteJob::KeepHdr => api::RouteJob::KeepHdr,
                RouteJob::BurnIn => api::RouteJob::BurnIn,
            },
            source: route.source.as_str().to_owned(),
            target: route.target.as_str().to_owned(),
            steps: route.steps.into_iter().map(step).collect(),
            busy_after: route.busy_after,
        })
        .collect()
}

fn step(step: pick::RouteStep) -> api::RouteStep {
    let encoded = step.made.encoder.as_ref();
    let share = match (step.share, encoded) {
        (Some(Share::Whole), _) => api::RouteShare::Whole,
        (Some(Share::DecodeAndEncode), _) => api::RouteShare::DecodeAndEncode,
        (Some(Share::EncodeOnly), _) => api::RouteShare::EncodeOnly,
        (None, None) => api::RouteShare::None,
        (None, Some(_)) if step.gpu.is_some() => api::RouteShare::DecodeOnly,
        (None, Some(_)) => api::RouteShare::Cpu,
    };
    api::RouteStep {
        // A route not offered runs nowhere, whatever card decodes.
        gpu: step
            .gpu
            .filter(|_| share != api::RouteShare::None)
            .map(|gpu| gpu.card().key()),
        share,
        decoder: video_decoder(step.made.decoder),
        encoder: encoded.map(encoder_used),
        tone_map: filter_place(step.made.tone_map),
        tone_mapper: step.made.tone_mapper.map(tone_mapper),
        burn_in: filter_place(step.made.burn_in),
        cpu_why: step.cpu_why.into_iter().map(str::to_owned).collect(),
    }
}

fn kind(kind: Kind) -> api::GpuKind {
    match kind {
        Kind::Discrete => api::GpuKind::Discrete,
        Kind::Integrated => api::GpuKind::Integrated,
    }
}

#[cfg(test)]
mod tests {

    use spoolframe_media::encoders::{Encoders, Hardware, HardwareDecoder, VideoEncoder};
    use spoolframe_types::VideoCodec;

    use super::*;
    use crate::transcode::test_gpu;

    /// A GTX 1060's like: NVENC encodes H.264 and HEVC and NVDEC decodes
    /// them; software H.264 beside, the only software video encoder
    /// (PGPU-06).
    fn pascal() -> Encoders {
        let mut encoders = Encoders::software_baseline();
        encoders.video = [VideoCodec::H264, VideoCodec::Hevc]
            .into_iter()
            .map(|codec| VideoEncoder {
                codec,
                name: format!("{}_nvenc", codec.as_str()),
                hardware: Some(Hardware::Nvenc),
            })
            .chain(encoders.video)
            .collect();
        encoders.hardware_decoders = [VideoCodec::H264, VideoCodec::Hevc]
            .into_iter()
            .map(|codec| HardwareDecoder {
                codec,
                hardware: Hardware::Nvenc,
                bit_depth: 8,
            })
            .collect();
        encoders
    }

    /// The routes name the card and what of the job it does: H.264 out
    /// on NVENC, decoded there too; AV1 out, which NVENC has none of and
    /// software no longer encodes (PGPU-06), nowhere; and with no GPU,
    /// H.264 on the CPU.
    #[test]
    fn routes_name_the_card_and_its_share() {
        let gpu = test_gpu(
            1,
            Hardware::Nvenc,
            spoolframe_media::gpus::Kind::Discrete,
            pascal(),
        );
        let key = gpu.card().key();
        let detected = Detected::with_gpus(vec![gpu]);
        let routes = routes(&detected, false, ToneMapSetting::Auto);
        let at = |source: &str, target: &str| {
            routes
                .iter()
                .find(|r| {
                    r.job == api::RouteJob::Convert && r.source == source && r.target == target
                })
                .unwrap()
                .steps[0]
                .clone()
        };
        let h264 = at("hevc", "h264");
        assert_eq!(h264.gpu.as_deref(), Some(key.as_str()));
        assert_ne!(h264.share, api::RouteShare::Cpu);
        assert_eq!(h264.encoder.map(|e| e.name).as_deref(), Some("h264_nvenc"));
        let av1 = at("hevc", "av1");
        assert_eq!(av1.share, api::RouteShare::None);
        assert_eq!(av1.encoder, None);
        // AV1 in to H.264: NVDEC does not decode it; NVENC encodes it from
        // what the CPU decoded.
        assert_eq!(at("av1", "h264").share, api::RouteShare::EncodeOnly);

        let none = Detected::software_baseline();
        let routes = super::routes(&none, false, ToneMapSetting::Auto);
        assert!(
            routes
                .iter()
                .all(|r| r.steps.iter().all(|s| s.gpu.is_none()))
        );
        assert!(
            routes
                .iter()
                .filter(|r| r.target == "h264")
                .all(|r| !r.busy_after)
        );
        let hevc_out = routes.iter().find(|r| r.target == "hevc").unwrap();
        assert_eq!(hevc_out.steps.len(), 1);
        assert_eq!(hevc_out.steps[0].share, api::RouteShare::None);
    }

    /// A card's line on the Server page: what the start line says of it.
    #[test]
    fn a_card_says_what_it_does_and_its_limit() {
        let gpu = test_gpu(
            1,
            Hardware::Nvenc,
            spoolframe_media::gpus::Kind::Discrete,
            pascal(),
        );
        let key = gpu.card().key();
        let detected = Detected::with_gpus(vec![gpu]);
        let running = HashMap::from([(key.clone(), 2)]);
        let cards = cards(&detected, &[], &running, 3);
        assert_eq!(cards.len(), 1);
        let card = &cards[0];
        assert_eq!(card.key, key);
        assert_eq!(card.api, api::GpuApi::Nvidia);
        assert_eq!(card.kind, api::GpuKind::Discrete);
        assert_eq!(card.encodes, ["h264_nvenc", "hevc_nvenc"]);
        assert_eq!(card.decodes, ["h264 (nvdec)", "hevc (nvdec)"]);
        assert_eq!((card.jobs, card.max_jobs, card.own_limit), (2, 3, false));
        assert_eq!(card.entry, "transcode.gpus entry 1");
    }

    /// A filter a card lacks is named as its API's own, so it does not
    /// read as if the card could not tone map at all.
    #[test]
    fn a_lacking_filter_is_named_as_the_api_s_own() {
        use spoolframe_media::encoders::FailedTest;
        let mut encoders = pascal();
        let failed = |test: &str, lacks: &str| FailedTest {
            hardware: Hardware::Vaapi,
            test: test.into(),
            lacks: lacks.into(),
            stderr: String::new(),
        };
        encoders.failed = vec![
            failed("tonemap_vaapi", "tone map"),
            failed("av1_vaapi", "av1 encode"),
        ];
        assert_eq!(
            lacks(&encoders, Hardware::Vaapi),
            ["VA-API tone map (tonemap_vaapi)", "av1 encode"]
        );
    }

    /// PGPU-06: the codec the planner takes for each kind of device. A
    /// card that encodes AV1 and HEVC at 10 bits: AV1 kept for the device
    /// that decodes AV1, HEVC kept for the one that does not, AV1 tone
    /// mapped for an SDR screen; with `video_codecs = ["h264"]`, H.264
    /// tone mapped for all.
    #[test]
    fn picks_say_the_codec_for_each_kind_of_device() {
        let mut encoders = Encoders::software_baseline();
        encoders.video = [VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1]
            .into_iter()
            .map(|codec| VideoEncoder {
                codec,
                name: format!("{}_vaapi", codec.as_str()),
                hardware: Some(Hardware::Vaapi),
            })
            .chain(encoders.video)
            .collect();
        encoders.ten_bit = encoders
            .video
            .iter()
            .filter(|e| e.hardware.is_some() && e.codec != VideoCodec::H264)
            .cloned()
            .collect();
        let detected = Detected::with_gpus(vec![test_gpu(
            1,
            Hardware::Vaapi,
            spoolframe_media::gpus::Kind::Discrete,
            encoders,
        )]);
        let said = |codecs: &[VideoCodec]| {
            picks(&detected, codecs)
                .into_iter()
                .map(|p| (p.device, p.codec, p.hdr_kept))
                .collect::<Vec<_>>()
        };
        let some = |c: &str| Some(c.to_owned());
        assert_eq!(
            said(&crate::config::DEFAULT_VIDEO_CODECS),
            [
                (api::PickDevice::HdrAv1, some("av1"), true),
                (api::PickDevice::HdrHevc, some("hevc"), true),
                (api::PickDevice::Sdr, some("av1"), false),
            ]
        );
        assert_eq!(
            said(&[VideoCodec::H264]),
            [
                (api::PickDevice::HdrAv1, some("h264"), false),
                (api::PickDevice::HdrHevc, some("h264"), false),
                (api::PickDevice::Sdr, some("h264"), false),
            ]
        );
    }
}
