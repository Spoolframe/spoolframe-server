//! The Overview's GPUs (PGPU-05): each card encodes run on, as the
//! server's start lines say it; the host's other cards and why they do
//! nothing; and where typical conversions would run, asked of the jobs'
//! own pick ([`crate::encode::pick::routes`]).

use std::collections::HashMap;

use spoolframe_admin_api as api;
use spoolframe_media::encoders::ToneMapSetting;
use spoolframe_media::gpus::Kind;

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
        encodes: encoders
            .video
            .iter()
            .filter(|e| e.hardware == Some(api))
            .map(|e| e.name.clone())
            .collect(),
        decodes: encoders
            .hardware_decoders
            .iter()
            .filter(|d| d.hardware == api)
            .map(|d| d.label())
            .collect(),
        filters: encoders
            .gpu_filters(api)
            .into_iter()
            .map(str::to_owned)
            .collect(),
        tone_maps: encoders
            .gpu_tone_mappers(api)
            .into_iter()
            .map(tone_mapper)
            .collect(),
        lacks: encoders.lacks(api).into_iter().map(str::to_owned).collect(),
    }
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

/// Where typical conversions would run now ([`pick::routes`]), with the
/// server's GPU filters and tone map setting.
pub fn routes(
    detected: &Detected,
    gpu_filters: bool,
    tone_map: ToneMapSetting,
) -> Vec<api::GpuRoute> {
    pick::routes(detected, gpu_filters, tone_map)
        .into_iter()
        .map(|route| {
            let encoded = route.made.encoder.as_ref();
            let share = match (route.share, encoded) {
                (Some(Share::Whole), _) => api::RouteShare::Whole,
                (Some(Share::DecodeAndEncode), _) => api::RouteShare::DecodeAndEncode,
                (Some(Share::EncodeOnly), _) => api::RouteShare::EncodeOnly,
                (None, None) => api::RouteShare::None,
                (None, Some(_)) if route.gpu.is_some() => api::RouteShare::DecodeOnly,
                (None, Some(_)) => api::RouteShare::Cpu,
            };
            api::GpuRoute {
                job: match route.job {
                    RouteJob::Convert => api::RouteJob::Convert,
                    RouteJob::ToneMap => api::RouteJob::ToneMap,
                    RouteJob::BurnIn => api::RouteJob::BurnIn,
                },
                source: route.source.as_str().to_owned(),
                target: route.target.as_str().to_owned(),
                // A route not offered runs nowhere, whatever card decodes.
                gpu: route
                    .gpu
                    .filter(|_| share != api::RouteShare::None)
                    .map(|gpu| gpu.card().key()),
                share,
                decoder: video_decoder(route.made.decoder),
                encoder: encoded.map(encoder_used),
                tone_map: filter_place(route.made.tone_map),
                tone_mapper: route.made.tone_mapper.map(tone_mapper),
                burn_in: filter_place(route.made.burn_in),
            }
        })
        .collect()
}

fn kind(kind: Kind) -> api::GpuKind {
    match kind {
        Kind::Discrete => api::GpuKind::Discrete,
        Kind::Integrated => api::GpuKind::Integrated,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use spoolframe_media::encoders::{Encoders, Hardware, HardwareDecoder, VideoEncoder};
    use spoolframe_types::VideoCodec;

    use super::*;
    use crate::transcode::test_gpu;

    /// A GTX 1060's like: NVENC encodes H.264 and HEVC and NVDEC decodes
    /// them; SVT-AV1 in software beside, as a real ffmpeg has it.
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
            .chain([VideoEncoder {
                codec: VideoCodec::Av1,
                name: "libsvtav1".into(),
                hardware: None,
            }])
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
    /// on NVENC, decoded there too; AV1 out (NVENC has none) encoded in
    /// software, decoded on the card; and with no GPU, the CPU.
    #[test]
    fn routes_name_the_card_and_its_share() {
        let gpu = test_gpu(
            1,
            Hardware::Nvenc,
            spoolframe_media::gpus::Kind::Discrete,
            pascal(),
        );
        let key = gpu.card().key();
        let mut software = Encoders::software_baseline();
        software.video.push(VideoEncoder {
            codec: VideoCodec::Av1,
            name: "libsvtav1".into(),
            hardware: None,
        });
        let mut detected = Detected::with_gpus(vec![gpu]);
        detected.software = Arc::new(software);
        let routes = routes(&detected, false, ToneMapSetting::Auto);
        let at = |source: &str, target: &str| {
            routes
                .iter()
                .find(|r| {
                    r.job == api::RouteJob::Convert && r.source == source && r.target == target
                })
                .unwrap()
                .clone()
        };
        let h264 = at("hevc", "h264");
        assert_eq!(h264.gpu.as_deref(), Some(key.as_str()));
        assert_ne!(h264.share, api::RouteShare::Cpu);
        assert_eq!(h264.encoder.map(|e| e.name).as_deref(), Some("h264_nvenc"));
        let av1 = at("hevc", "av1");
        assert_eq!(av1.share, api::RouteShare::DecodeOnly);
        assert_eq!(av1.gpu.as_deref(), Some(key.as_str()));
        assert_eq!(av1.encoder.map(|e| e.name).as_deref(), Some("libsvtav1"));
        // AV1 in: NVDEC does not decode it, so all on the CPU.
        assert_eq!(at("av1", "av1").share, api::RouteShare::Cpu);
        assert_eq!(at("av1", "av1").gpu, None);

        let none = Detected::software_baseline();
        let routes = super::routes(&none, false, ToneMapSetting::Auto);
        assert!(routes.iter().all(|r| r.gpu.is_none()));
        let hevc_out = routes.iter().find(|r| r.target == "hevc").unwrap();
        assert_eq!(hevc_out.share, api::RouteShare::None);
    }

    /// A card's line in the Overview: what the start line says of it.
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
}
