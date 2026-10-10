//! Which GPU a conversion runs on (PGPU-05).
//!
//! PGPU-02's [`args::placement`] picks the family that does the most of a
//! job, of one detection. With several cards, each card has a detection
//! of its own (its API's encoders and decoders, and software's), and the
//! card is picked first: by how much of the job its own placement does,
//! then by the order of `transcode.gpus` and the cards' rank within an
//! entry. The conversion's runs then use that card's detection, so
//! everything PGPU-02 and PGPU-03 decide follows from it as before.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use spoolframe_media::encoders::{Encoders, VideoEncoder};
use spoolframe_media::{
    MediaDescriptor, SubtitleKind, SubtitleSource, SubtitleStream, VideoStream,
};
use spoolframe_protocol::{ConvertedSubtitle, ConvertedVideo, SubtitleAction};
use spoolframe_types::{HdrFormat, SubtitleFormat, SubtitleRole, VideoCodec};

use super::args::{self, EncodeSpec, Share};
use crate::transcode::{Detected, Gpu};

/// The GPU `spec`'s video encode runs on, of `gpus` (in the order jobs
/// try them, `gpus::rank`'s):
///
/// - the cards whose own detection encodes the output codec, by how much
///   of the job each does ([`Share`]: decode, filters and encode; decode
///   and encode; encode only), then their order. A card with no `room`
///   is passed over for the next; with none that has room, the first,
///   whose start is then refused as busy, as when the GPU jobs ran out
///   before.
/// - none encodes it: a software encode, decoding on the first card that
///   decodes the source; `None` when none does, or when there is no video
///   encode.
///
/// `spec`'s own encoders and render node are not read: each card's are
/// put in their place.
pub fn choose<'a>(
    spec: &EncodeSpec<'_>,
    gpus: &'a [Arc<Gpu>],
    room: &dyn Fn(&Gpu) -> bool,
) -> Option<&'a Arc<Gpu>> {
    if !matches!(spec.video, Some(ConvertedVideo::Encode { .. })) {
        return None;
    }
    let mut encoding: Vec<(Share, usize, &'a Arc<Gpu>)> = Vec::new();
    for (order, gpu) in gpus.iter().enumerate() {
        let node = gpu.vaapi_device();
        if let Some(share) = args::share(&on_card(spec, gpu, &node)) {
            encoding.push((share, order, gpu));
        }
    }
    encoding.sort_by_key(|(share, order, _)| (*share, *order));
    if let Some((_, _, best)) = encoding.first() {
        let free = encoding.iter().find(|(_, _, gpu)| room(gpu));
        return Some(free.map_or(*best, |(_, _, gpu)| *gpu));
    }
    gpus.iter().find(|gpu| {
        let node = gpu.vaapi_device();
        args::placement(&on_card(spec, gpu, &node)).is_some_and(|place| place.decoder.is_some())
    })
}

/// `spec` with `gpu`'s detection and render node in place of its own.
fn on_card<'a>(spec: &EncodeSpec<'a>, gpu: &'a Gpu, node: &'a Path) -> EncodeSpec<'a> {
    EncodeSpec {
        encoders: gpu.encoders(),
        vaapi_device: node,
        ..spec.clone()
    }
}

/// The video encoder a run of `video`, an encode of `descriptor`'s video
/// with `subtitle`, would use: on the card [`choose`] picks, as if every
/// card had room. What Server › Playback's plan names (PGPU-02).
pub fn planned_encoder(
    detected: &Detected,
    descriptor: &MediaDescriptor,
    video: ConvertedVideo,
    subtitle: Option<ConvertedSubtitle>,
    gpu_filters: bool,
) -> Option<VideoEncoder> {
    let probe = EncodeSpec {
        input: Path::new(""),
        sidecar: None,
        descriptor,
        video: Some(video),
        audio: None,
        subtitle,
        encoders: &detected.software,
        vaapi_device: Path::new(""),
        hardware_decode: true,
        gpu_filters,
        // As `args::planned_encoder` plans (PGPU-04).
        tone_map: spoolframe_media::encoders::ToneMapSetting::Auto,
        start: None,
        output: args::Output::Progressive {
            file: PathBuf::new(),
        },
    };
    let encoders = match choose(&probe, &detected.gpus, &|_| true) {
        Some(gpu) => gpu.encoders(),
        None => &detected.software,
    };
    args::planned_encoder(encoders, descriptor, video, subtitle, gpu_filters)
}

/// A typical conversion, for the Overview's routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteJob {
    /// A 1080p 8-bit SDR source scaled to 720p.
    Convert,
    /// A 4K HDR10 source tone mapped to 1080p SDR.
    ToneMap,
    /// A 1080p source with picture subtitles burned in.
    BurnIn,
}

/// Where `job` of a `source` video to `target` runs now: the card
/// [`choose`] picks (as if every card had room), how much of the job it
/// does, and what [`args::made`] says each part runs on there.
#[derive(Debug, Clone)]
pub struct Route {
    pub job: RouteJob,
    pub source: VideoCodec,
    pub target: VideoCodec,
    pub gpu: Option<Arc<Gpu>>,
    /// `None` for a software encode, or a codec nothing here encodes
    /// (`made.encoder` is then `None`).
    pub share: Option<Share>,
    pub made: args::Made,
}

/// The routes the Overview shows: H.264, HEVC and AV1 sources each
/// converted to H.264, HEVC and AV1; an HDR10 HEVC film tone mapped to
/// each; and an H.264 film with picture subtitles burned in to each.
/// Each asked of the jobs' own pick ([`choose`], [`args::share`],
/// [`args::made`]) with the server's GPU filters and tone map setting, so
/// the table cannot say other than what a job would do.
pub fn routes(
    detected: &Detected,
    gpu_filters: bool,
    tone_map: spoolframe_media::encoders::ToneMapSetting,
) -> Vec<Route> {
    use VideoCodec::{Av1, H264, Hevc};
    let cases = [
        (RouteJob::Convert, H264),
        (RouteJob::Convert, Hevc),
        (RouteJob::Convert, Av1),
        (RouteJob::ToneMap, Hevc),
        (RouteJob::BurnIn, H264),
    ];
    let mut routes = Vec::new();
    for (job, source) in cases {
        let descriptor = sample(job, source);
        for target in [H264, Hevc, Av1] {
            let (height, tone_map_video, subtitle) = match job {
                RouteJob::Convert => (720, false, None),
                RouteJob::ToneMap => (1080, true, None),
                RouteJob::BurnIn => (
                    1080,
                    false,
                    Some(ConvertedSubtitle {
                        index: 0,
                        action: SubtitleAction::BurnIn {
                            format: SubtitleFormat::Pgs,
                        },
                    }),
                ),
            };
            let probe = EncodeSpec {
                input: Path::new(""),
                sidecar: None,
                descriptor: &descriptor,
                video: Some(ConvertedVideo::Encode {
                    codec: target,
                    height,
                    max_bitrate: None,
                    tone_map: tone_map_video,
                }),
                audio: None,
                subtitle,
                encoders: &detected.software,
                vaapi_device: Path::new(""),
                hardware_decode: true,
                gpu_filters,
                tone_map,
                start: None,
                output: args::Output::Progressive {
                    file: PathBuf::new(),
                },
            };
            let gpu = choose(&probe, &detected.gpus, &|_| true).cloned();
            let node = gpu.as_ref().map(|g| g.vaapi_device()).unwrap_or_default();
            let spec = match &gpu {
                Some(gpu) => on_card(&probe, gpu, &node),
                None => probe.clone(),
            };
            routes.push(Route {
                job,
                source,
                target,
                share: args::share(&spec),
                made: args::made(&spec),
                gpu,
            });
        }
    }
    routes
}

/// The source [`routes`] asks about for `job`: `codec` video, 1080p 8-bit
/// SDR, or 4K 10-bit HDR10 to tone map; with a PGS track to burn in.
fn sample(job: RouteJob, codec: VideoCodec) -> MediaDescriptor {
    let hdr = job == RouteJob::ToneMap;
    let (width, height) = if hdr { (3840, 2160) } else { (1920, 1080) };
    let subtitles = match job {
        RouteJob::BurnIn => vec![SubtitleStream {
            source: SubtitleSource::Embedded { index: 2 },
            codec: "hdmv_pgs_subtitle".into(),
            kind: SubtitleKind::Bitmap,
            language: None,
            title: None,
            forced: false,
            default: false,
            role: SubtitleRole::Full,
        }],
        RouteJob::Convert | RouteJob::ToneMap => Vec::new(),
    };
    MediaDescriptor {
        container: "matroska,webm".into(),
        duration: Some(std::time::Duration::from_secs(3600)),
        bit_rate: None,
        video: Some(VideoStream {
            index: 0,
            codec: codec.as_str().into(),
            profile: None,
            level: None,
            width,
            height,
            pixel_format: Some(if hdr { "yuv420p10le" } else { "yuv420p" }.into()),
            color_transfer: hdr.then(|| "smpte2084".into()),
            frame_rate_millis: Some(23_976),
            bit_depth: if hdr { 10 } else { 8 },
            hdr: hdr.then_some(HdrFormat::Hdr10),
            bit_rate: None,
        }),
        audio: Vec::new(),
        subtitles,
    }
}

/// Where a conversion's runs go: the GPU [`choose`] picked, its
/// detection and its node; or none, software's detection alone.
#[derive(Debug, Clone)]
pub struct RunOn {
    software: Arc<Encoders>,
    gpu: Option<Arc<Gpu>>,
    vaapi_device: PathBuf,
}

impl RunOn {
    pub fn new(detected: &Detected, gpu: Option<Arc<Gpu>>) -> Self {
        Self {
            software: detected.software.clone(),
            vaapi_device: gpu.as_ref().map(|g| g.vaapi_device()).unwrap_or_default(),
            gpu,
        }
    }

    /// The detection a run's arguments are made from.
    pub fn encoders(&self) -> &Encoders {
        match &self.gpu {
            Some(gpu) => gpu.encoders(),
            None => &self.software,
        }
    }

    pub fn vaapi_device(&self) -> &Path {
        &self.vaapi_device
    }

    pub fn gpu(&self) -> Option<&Arc<Gpu>> {
        self.gpu.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use spoolframe_media::encoders::{Hardware, HardwareDecoder, ToneMapSetting, VideoEncoder};
    use spoolframe_media::gpus::Kind;
    use spoolframe_media::{MediaDescriptor, VideoStream};
    use spoolframe_types::VideoCodec;

    use super::*;
    use crate::encode::args::Output;
    use crate::transcode::test_gpu;

    /// A card of `api` whose detection encodes `encodes` and decodes
    /// `decodes` (8-bit), with VA-API's GPU filters or not.
    fn card(
        n: u8,
        api: Hardware,
        kind: Kind,
        encodes: &[VideoCodec],
        decodes: &[VideoCodec],
    ) -> Arc<Gpu> {
        let suffix = api.as_str();
        let encoders = Encoders {
            video: encodes
                .iter()
                .map(|&codec| VideoEncoder {
                    codec,
                    name: format!("{}_{suffix}", codec.as_str()),
                    hardware: Some(api),
                })
                .chain(Encoders::software_baseline().video)
                .collect(),
            hardware_decoders: decodes
                .iter()
                .map(|&codec| HardwareDecoder {
                    codec,
                    hardware: api,
                    bit_depth: 8,
                })
                .collect(),
            ..Encoders::software_baseline()
        };
        Arc::new(test_gpu(n, api, kind, encoders))
    }

    fn film(codec: &str, height: u32) -> MediaDescriptor {
        MediaDescriptor {
            container: "matroska,webm".into(),
            duration: Some(Duration::from_secs(60)),
            bit_rate: None,
            video: Some(VideoStream {
                index: 0,
                codec: codec.into(),
                profile: None,
                level: None,
                width: height * 16 / 9,
                height,
                pixel_format: Some("yuv420p".into()),
                color_transfer: None,
                frame_rate_millis: Some(24_000),
                bit_depth: 8,
                hdr: None,
                bit_rate: None,
            }),
            audio: Vec::new(),
            subtitles: Vec::new(),
        }
    }

    /// `film` encoded to H.264 at `height`.
    fn spec<'a>(film: &'a MediaDescriptor, height: u32, software: &'a Encoders) -> EncodeSpec<'a> {
        EncodeSpec {
            input: Path::new("in"),
            sidecar: None,
            descriptor: film,
            video: Some(ConvertedVideo::Encode {
                codec: VideoCodec::H264,
                height,
                max_bitrate: None,
                tone_map: false,
            }),
            audio: None,
            subtitle: None,
            encoders: software,
            vaapi_device: Path::new(""),
            tone_map: ToneMapSetting::Auto,
            hardware_decode: true,
            gpu_filters: false,
            start: None,
            output: Output::Progressive {
                file: PathBuf::from("out.mkv"),
            },
        }
    }

    fn picked(gpus: &[Arc<Gpu>], spec: &EncodeSpec<'_>, full: &[u8]) -> Option<String> {
        let room = |gpu: &Gpu| {
            !full
                .iter()
                .any(|n| gpu.card().host.pci.ends_with(&format!("{n:02x}:00.0")))
        };
        choose(spec, gpus, &room).map(|g| g.card().host.pci.clone())
    }

    const H264: VideoCodec = VideoCodec::H264;
    const HEVC: VideoCodec = VideoCodec::Hevc;
    const AV1: VideoCodec = VideoCodec::Av1;

    /// The card that does the whole job wins over one earlier in order
    /// that only encodes it, across entries: an AV1 film to H.264 goes to
    /// the second entry's card, which decodes AV1, not to the GTX 1060
    /// first in order.
    #[test]
    fn the_whole_job_card_wins_across_entries() {
        let software = Encoders::software_baseline();
        let gpus = [
            card(
                1,
                Hardware::Nvenc,
                Kind::Discrete,
                &[H264, HEVC],
                &[H264, HEVC],
            ),
            card(2, Hardware::Vaapi, Kind::Integrated, &[H264], &[H264, AV1]),
        ];
        let av1 = film("av1", 1080);
        assert_eq!(
            picked(&gpus, &spec(&av1, 1080, &software), &[]).as_deref(),
            Some("0000:02:00.0")
        );
        // A film both decode: the first in order.
        let hevc = film("hevc", 1080);
        assert_eq!(
            picked(&gpus, &spec(&hevc, 1080, &software), &[]).as_deref(),
            Some("0000:01:00.0")
        );
    }

    /// Equal shares go by the cards' order: the entries', then the rank
    /// within one.
    #[test]
    fn equal_cards_go_in_order() {
        let software = Encoders::software_baseline();
        let hevc = film("hevc", 1080);
        let gpus = [
            card(3, Hardware::Vaapi, Kind::Discrete, &[H264], &[HEVC]),
            card(1, Hardware::Vaapi, Kind::Discrete, &[H264], &[HEVC]),
        ];
        assert_eq!(
            picked(&gpus, &spec(&hevc, 1080, &software), &[]).as_deref(),
            Some("0000:03:00.0")
        );
    }

    /// A full card is passed over for the next that encodes the codec,
    /// even one that does less of the job; with every one full, the best,
    /// whose start is then refused.
    #[test]
    fn a_full_card_spills_to_the_next() {
        let software = Encoders::software_baseline();
        let hevc = film("hevc", 1080);
        let gpus = [
            card(1, Hardware::Vaapi, Kind::Discrete, &[H264], &[HEVC]),
            card(2, Hardware::Vaapi, Kind::Integrated, &[H264], &[]),
            card(3, Hardware::Vaapi, Kind::Integrated, &[HEVC], &[HEVC]),
        ];
        let spec = spec(&hevc, 1080, &software);
        assert_eq!(picked(&gpus, &spec, &[1]).as_deref(), Some("0000:02:00.0"));
        assert_eq!(
            picked(&gpus, &spec, &[1, 2]).as_deref(),
            Some("0000:01:00.0")
        );
    }

    /// No card encodes the codec: software, decoding on the first card
    /// that decodes the source; none at all when none does, or when the
    /// film's GPU decode failed before.
    #[test]
    fn a_software_encode_decodes_on_the_first_card_that_can() {
        let software = Encoders::software_baseline();
        let gpus = [
            card(1, Hardware::Nvenc, Kind::Discrete, &[HEVC], &[HEVC]),
            card(2, Hardware::Vaapi, Kind::Integrated, &[HEVC], &[AV1]),
        ];
        let av1 = film("av1", 1080);
        let spec_av1 = spec(&av1, 1080, &software);
        assert_eq!(
            picked(&gpus, &spec_av1, &[]).as_deref(),
            Some("0000:02:00.0")
        );
        // Full or not: a software encode takes no GPU slot.
        assert_eq!(
            picked(&gpus, &spec_av1, &[2]).as_deref(),
            Some("0000:02:00.0")
        );
        let vp9 = film("vp9", 1080);
        assert_eq!(picked(&gpus, &spec(&vp9, 1080, &software), &[]), None);
        let failed = EncodeSpec {
            hardware_decode: false,
            ..spec_av1.clone()
        };
        assert_eq!(picked(&gpus, &failed, &[]), None);
    }

    /// A copied video encodes nothing: no GPU.
    #[test]
    fn no_video_encode_takes_no_gpu() {
        let software = Encoders::software_baseline();
        let gpus = [card(1, Hardware::Vaapi, Kind::Discrete, &[H264], &[H264])];
        let h264 = film("h264", 1080);
        let copy = EncodeSpec {
            video: Some(ConvertedVideo::Copy { codec: H264 }),
            ..spec(&h264, 1080, &software)
        };
        assert_eq!(picked(&gpus, &copy, &[]), None);
    }

    /// The Overview's routes are the jobs' own pick: an AV1 film to H.264
    /// on the card that decodes AV1, HEVC on the first card, AV1 out
    /// (which no card encodes) in software decoding on the first card
    /// that decodes the source; five jobs, three targets each, in order.
    #[test]
    fn routes_say_what_a_job_would_pick() {
        // SVT-AV1 in software, as a real ffmpeg has it, here and on each
        // card's own detection.
        let svt = VideoEncoder {
            codec: AV1,
            name: "libsvtav1".into(),
            hardware: None,
        };
        let mut software = Encoders::software_baseline();
        software.video.push(svt.clone());
        let with_svt = |gpu: Arc<Gpu>, n: u8| {
            let mut encoders = gpu.encoders().clone();
            encoders.video.push(svt.clone());
            Arc::new(test_gpu(
                n,
                gpu.card().api,
                gpu.card().host.class.kind,
                encoders,
            ))
        };
        let gpus = vec![
            with_svt(
                card(
                    1,
                    Hardware::Nvenc,
                    Kind::Discrete,
                    &[H264, HEVC],
                    &[H264, HEVC],
                ),
                1,
            ),
            with_svt(
                card(2, Hardware::Vaapi, Kind::Integrated, &[H264], &[H264, AV1]),
                2,
            ),
        ];
        let detected = Detected {
            encoders: Arc::new(software.clone()),
            gpus,
            software: Arc::new(software),
            others: Vec::new(),
            notes: Vec::new(),
        };
        let routes = routes(&detected, false, ToneMapSetting::Auto);
        assert_eq!(routes.len(), 15);
        let at = |job, source, target| {
            routes
                .iter()
                .find(|r| r.job == job && r.source == source && r.target == target)
                .unwrap()
        };
        let pci = |r: &Route| r.gpu.as_ref().map(|g| g.card().host.pci.clone());
        let av1_h264 = at(RouteJob::Convert, AV1, H264);
        assert_eq!(pci(av1_h264).as_deref(), Some("0000:02:00.0"));
        assert_eq!(av1_h264.share, Some(Share::Whole));
        let hevc_h264 = at(RouteJob::Convert, HEVC, H264);
        assert_eq!(pci(hevc_h264).as_deref(), Some("0000:01:00.0"));
        let h264_av1 = at(RouteJob::Convert, H264, AV1);
        assert_eq!(pci(h264_av1).as_deref(), Some("0000:01:00.0"));
        assert_eq!(h264_av1.share, None);
        assert_eq!(
            h264_av1.made.encoder.as_ref().map(|e| e.hardware),
            Some(None),
            "a software AV1 encode"
        );
        // The HDR film is tone mapped; the burn-in's pictures are drawn.
        let hdr = at(RouteJob::ToneMap, HEVC, H264);
        assert!(hdr.made.tone_mapper.is_some());
        let burn = at(RouteJob::BurnIn, H264, HEVC);
        assert_ne!(burn.made.burn_in, crate::encode::FilterOn::None);
    }

    /// PGPU-04 per card: an HDR film to SDR goes, under `tone_map =
    /// "auto"` with GPU filters on, to the card whose libplacebo test
    /// passed, which does the whole job, over one earlier in order that
    /// would tone map on the CPU; with GPU filters off both tone map on
    /// the CPU, and the order decides.
    #[test]
    fn a_card_that_tone_maps_with_libplacebo_wins_an_hdr_job() {
        use spoolframe_media::encoders::{Libplacebo, VulkanDevice};
        let software = Encoders::software_baseline();
        let plain = card(1, Hardware::Vaapi, Kind::Integrated, &[H264], &[HEVC]);
        let placebo = {
            let mut encoders = plain.encoders().clone();
            encoders.libplacebo = vec![Libplacebo {
                hardware: Hardware::Vaapi,
                device: VulkanDevice::Own,
            }];
            Arc::new(test_gpu(2, Hardware::Vaapi, Kind::Discrete, encoders))
        };
        let gpus = [plain, placebo];
        let hevc = film("hevc", 2160);
        let hdr = |gpu_filters| EncodeSpec {
            video: Some(ConvertedVideo::Encode {
                codec: H264,
                height: 1080,
                max_bitrate: None,
                tone_map: true,
            }),
            gpu_filters,
            ..spec(&hevc, 1080, &software)
        };
        assert_eq!(
            picked(&gpus, &hdr(true), &[]).as_deref(),
            Some("0000:02:00.0")
        );
        assert_eq!(
            picked(&gpus, &hdr(false), &[]).as_deref(),
            Some("0000:01:00.0")
        );
    }
}
