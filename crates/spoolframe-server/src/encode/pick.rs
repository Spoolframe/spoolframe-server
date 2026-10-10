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

use spoolframe_media::MediaDescriptor;
use spoolframe_media::encoders::{Encoders, VideoEncoder};
use spoolframe_protocol::{ConvertedSubtitle, ConvertedVideo};

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
