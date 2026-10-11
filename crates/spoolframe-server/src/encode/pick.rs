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
///   which [`run_on`] then spills to a software encode.
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
    let keeps_hdr = keeps_hdr(spec);
    for (order, gpu) in gpus.iter().enumerate() {
        let node = gpu.vaapi_device();
        let on = on_card(spec, gpu, &node);
        // An encode that keeps HDR goes only to a card that writes 10
        // bits of its codec (PGPU-06).
        if keeps_hdr
            && !args::placement(&on).is_some_and(|p| on.encoders.encodes_ten_bit(p.encoder))
        {
            continue;
        }
        if let Some(share) = args::share(&on) {
            encoding.push((share, order, gpu));
        }
    }
    // Dolby Vision profile 5 is right only through libplacebo (PGPU-06):
    // a card that runs it, where one encodes the codec.
    if spec
        .descriptor
        .video
        .as_ref()
        .is_some_and(|v| v.needs_rpu())
    {
        let placebo = |gpu: &Gpu| {
            let node = gpu.vaapi_device();
            args::libplacebo(&on_card(spec, gpu, &node)).is_some()
        };
        if encoding.iter().any(|(_, _, gpu)| placebo(gpu)) {
            encoding.retain(|(_, _, gpu)| placebo(gpu));
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

/// What a playback is planned with (PGPU-06): `detected`'s encoders, the
/// video ones in the order the server prefers them. First each codec of
/// `video_codecs` in its order, with the encoder of every card that
/// encodes it and has `room` for another encode now; then software's,
/// which is H.264 alone. [`Encoders::ten_bit`] is those cards' 10-bit
/// encoders. The planner takes the first the client plays, so a codec
/// whose cards are all full falls to the next, and with none left to
/// software H.264, which the jobs' spill encodes too.
pub fn planning(
    detected: &Detected,
    video_codecs: &[VideoCodec],
    room: &dyn Fn(&Gpu) -> bool,
) -> Encoders {
    let mut encoders = (*detected.encoders).clone();
    let mut video: Vec<VideoEncoder> = Vec::new();
    let mut ten_bit: Vec<VideoEncoder> = Vec::new();
    for &codec in video_codecs {
        for gpu in detected.gpus.iter().filter(|gpu| room(gpu)) {
            let theirs = gpu.encoders();
            for encoder in theirs
                .video
                .iter()
                .filter(|e| e.codec == codec && e.hardware.is_some())
            {
                if !video.iter().any(|v| v.name == encoder.name) {
                    video.push(encoder.clone());
                }
                if theirs.encodes_ten_bit(encoder)
                    && !ten_bit.iter().any(|t| t.name == encoder.name)
                {
                    ten_bit.push(encoder.clone());
                }
            }
        }
    }
    video.extend(
        detected
            .software
            .video
            .iter()
            .filter(|e| e.hardware.is_none() && e.codec == VideoCodec::H264)
            .cloned(),
    );
    encoders.video = video;
    encoders.ten_bit = ten_bit;
    encoders
}

/// Whether `spec`'s video encode keeps its film's HDR (PGPU-06): an HEVC
/// or AV1 encode, not tone mapped, of HDR that an encode can keep. Only a
/// card whose encoder took a 10-bit test frame takes it.
pub fn keeps_hdr(spec: &EncodeSpec<'_>) -> bool {
    match (spec.video, &spec.descriptor.video) {
        (
            Some(ConvertedVideo::Encode {
                codec,
                tone_map: false,
                ..
            }),
            Some(stream),
        ) => codec != VideoCodec::H264 && stream.keepable_hdr().is_some(),
        _ => false,
    }
}

/// Where `spec`'s runs go (PGPU-05): on the card [`choose`] picks; or,
/// when every card that encodes the output codec is full, encoded in
/// software instead ([`spill`]), so the playback starts on the CPU's
/// budget rather than being refused (the user's choice, 2026-10-10: a
/// slower conversion over none). Only with the CPU's budget full too is
/// it refused as busy, when its run takes its place.
pub fn run_on(detected: &Detected, spec: &EncodeSpec<'_>, room: &dyn Fn(&Gpu) -> bool) -> RunOn {
    match choose(spec, &detected.gpus, room) {
        Some(gpu) if !room(gpu) => spill(detected, spec),
        gpu => RunOn::new(detected, gpu.cloned()),
    }
}

/// `spec` encoded in software, as when every card that encodes its codec
/// is full: decoding (and filtering, where it can) on the first card in
/// order that decodes the source, software's video encoders in place of
/// its own, as for a codec no card encodes; with none, all on the CPU.
/// A GPU decode takes no card's slot: those count encodes.
pub fn spill(detected: &Detected, spec: &EncodeSpec<'_>) -> RunOn {
    for gpu in &detected.gpus {
        let encoders = software_encode(gpu.encoders(), &detected.software);
        let node = gpu.vaapi_device();
        let on = EncodeSpec {
            encoders: &encoders,
            vaapi_device: &node,
            ..spec.clone()
        };
        if args::placement(&on).is_some_and(|place| place.decoder.is_some()) {
            return RunOn {
                encoders: Some(Arc::new(encoders)),
                spilled: true,
                ..RunOn::new(detected, Some(gpu.clone()))
            };
        }
    }
    RunOn {
        spilled: true,
        ..RunOn::new(detected, None)
    }
}

/// A card's `encoders` with software's video encoders in place of its
/// own: a run made from it decodes and filters as the card does and
/// encodes in software.
fn software_encode(encoders: &Encoders, software: &Encoders) -> Encoders {
    let mut encoders = encoders.clone();
    encoders.video = software.video.clone();
    encoders
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

/// A typical conversion, for the Server page's routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteJob {
    /// A 1080p 8-bit SDR source scaled to 720p.
    Convert,
    /// A 4K HDR10 source tone mapped to 1080p SDR.
    ToneMap,
    /// A 4K HDR10 source to 1080p, its HDR kept (PGPU-06): HEVC and AV1
    /// only, H.264 carrying none.
    KeepHdr,
    /// A 1080p source with picture subtitles burned in.
    BurnIn,
}

/// Where `job` of a `source` video to `target` runs now: each card in
/// the order [`choose`] takes them as the ones before fill up, and
/// whether a job is then refused as busy.
#[derive(Debug, Clone)]
pub struct Route {
    pub job: RouteJob,
    pub source: VideoCodec,
    pub target: VideoCodec,
    /// Never empty: the first choice first.
    pub steps: Vec<RouteStep>,
    /// Nothing is left once every step is full: software, which a job
    /// spills to ([`spill`]), encodes H.264 alone (PGPU-06). A playback
    /// planned then takes the next codec of `transcode.video_codecs`
    /// instead ([`planning`]). Otherwise the last step is that software
    /// encode, refused as busy only when the CPU's budget is full too,
    /// which the routes do not count.
    pub busy_after: bool,
}

/// One step of a [`Route`]: the card, how much of the job it does, what
/// [`args::made`] says each part runs on there, and why a filter that
/// runs on the CPU does.
#[derive(Debug, Clone)]
pub struct RouteStep {
    pub gpu: Option<Arc<Gpu>>,
    /// `None` for a software encode, or a codec nothing here encodes
    /// (`made.encoder` is then `None`).
    pub share: Option<Share>,
    pub made: args::Made,
    pub cpu_why: Vec<&'static str>,
}

/// The routes the Server page shows: H.264, HEVC and AV1 sources each
/// converted to H.264, HEVC and AV1; an HDR10 HEVC film tone mapped to
/// each, and its HDR kept in HEVC and AV1 (PGPU-06); and an H.264 film
/// with picture subtitles burned in to each.
/// Each asked of the jobs' own pick ([`choose`], [`args::share`],
/// [`args::made`]) with the server's GPU filters and tone map setting,
/// then asked again with each card it chose counted full, so the table
/// cannot say other than what a job would do.
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
        (RouteJob::KeepHdr, Hevc),
        (RouteJob::BurnIn, H264),
    ];
    let mut routes = Vec::new();
    for (job, source) in cases {
        let descriptor = sample(job, source);
        for target in [H264, Hevc, Av1] {
            if job == RouteJob::KeepHdr && target == H264 {
                continue;
            }
            let (height, tone_map_video, subtitle) = match job {
                RouteJob::Convert => (720, false, None),
                RouteJob::ToneMap => (1080, true, None),
                RouteJob::KeepHdr => (1080, false, None),
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
            let mut steps: Vec<RouteStep> = Vec::new();
            let mut full: Vec<String> = Vec::new();
            let busy_after = loop {
                let room = |gpu: &Gpu| !full.contains(&gpu.card().key());
                let run = run_on(detected, &probe, &room);
                let step = step_on(&probe, &run);
                // Every card full, the run spills to a software encode:
                // the last step, or busy when software cannot encode the
                // codec either.
                if run.spilled() {
                    if step.made.encoder.is_none() {
                        break true;
                    }
                    steps.push(step);
                    break false;
                }
                let takes_a_slot = step.share.is_some();
                let key = step.gpu.as_ref().map(|g| g.card().key());
                steps.push(step);
                match key {
                    Some(key) if takes_a_slot => full.push(key),
                    _ => break false,
                }
            };
            routes.push(Route {
                job,
                source,
                target,
                steps,
                busy_after,
            });
        }
    }
    routes
}

/// `probe` run as `run` says: a [`RouteStep`].
fn step_on(probe: &EncodeSpec<'_>, run: &RunOn) -> RouteStep {
    let spec = EncodeSpec {
        encoders: run.encoders(),
        vaapi_device: run.vaapi_device(),
        ..probe.clone()
    };
    let made = args::made(&spec);
    RouteStep {
        share: args::share(&spec),
        cpu_why: cpu_why(&spec, &made),
        made,
        gpu: run.gpu().cloned(),
    }
}

/// Why `made`, `spec`'s video, tone maps or burns in on the CPU, when it
/// does: a software encode, or a CPU decode, takes every filter there;
/// then GPU filters being off, where `spec` with them on would run the
/// filter on the GPU ([`args::made`] asked again); then what
/// [`args::cpu_instead_of_gpu`] and [`args::tone_map_note`] say, as the
/// server notes them for a job.
fn cpu_why(spec: &EncodeSpec<'_>, made: &args::Made) -> Vec<&'static str> {
    use super::FilterOn;
    let on_cpu = |made: &args::Made| {
        (
            made.tone_map == FilterOn::Cpu,
            made.burn_in == FilterOn::Cpu,
        )
    };
    if on_cpu(made) == (false, false) {
        return Vec::new();
    }
    match &made.encoder {
        Some(encoder) if encoder.hardware.is_none() => return vec!["encoded in software"],
        None => return Vec::new(),
        Some(_) => {}
    }
    if made.decoder == super::Decoder::Software {
        return vec!["decoded on the CPU"];
    }
    let mut why = Vec::new();
    let with_filters = EncodeSpec {
        gpu_filters: true,
        ..spec.clone()
    };
    if !spec.gpu_filters {
        let (tone_map, burn_in) = on_cpu(made);
        let (tone_map_on, burn_in_on) = on_cpu(&args::made(&with_filters));
        if (tone_map && !tone_map_on) || (burn_in && !burn_in_on) {
            why.push("GPU filters are off (transcode.gpu_filters)");
        }
    }
    for reason in args::cpu_instead_of_gpu(&with_filters)
        .into_iter()
        .chain(args::tone_map_note(spec))
    {
        if !why.contains(&reason) {
            why.push(reason);
        }
    }
    why
}

/// The source [`routes`] asks about for `job`: `codec` video, 1080p 8-bit
/// SDR, or 4K 10-bit HDR10 to tone map or keep; with a PGS track to burn
/// in.
pub fn sample(job: RouteJob, codec: VideoCodec) -> MediaDescriptor {
    let hdr = matches!(job, RouteJob::ToneMap | RouteJob::KeepHdr);
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
        RouteJob::Convert | RouteJob::ToneMap | RouteJob::KeepHdr => Vec::new(),
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
            dolby_vision: None,
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
    /// In place of the card's detection: [`spill`]'s, the card's with its
    /// video encoders left out.
    encoders: Option<Arc<Encoders>>,
    /// [`spill`]'s: encoded in software because every card that encodes
    /// the codec was full.
    spilled: bool,
}

impl RunOn {
    pub fn new(detected: &Detected, gpu: Option<Arc<Gpu>>) -> Self {
        Self {
            software: detected.software.clone(),
            vaapi_device: gpu.as_ref().map(|g| g.vaapi_device()).unwrap_or_default(),
            gpu,
            encoders: None,
            spilled: false,
        }
    }

    /// The detection a run's arguments are made from.
    pub fn encoders(&self) -> &Encoders {
        match (&self.encoders, &self.gpu) {
            (Some(encoders), _) => encoders,
            (None, Some(gpu)) => gpu.encoders(),
            (None, None) => &self.software,
        }
    }

    pub fn vaapi_device(&self) -> &Path {
        &self.vaapi_device
    }

    pub fn gpu(&self) -> Option<&Arc<Gpu>> {
        self.gpu.as_ref()
    }

    /// Whether it is [`spill`]'s.
    pub fn spilled(&self) -> bool {
        self.spilled
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
                dolby_vision: None,
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

    /// Every card that encodes the codec full: the run spills to a
    /// software encode rather than being refused, decoding on the first
    /// card that decodes the source; with a card free, it runs there.
    #[test]
    fn a_full_card_spills_to_a_software_encode() {
        let software = Encoders::software_baseline();
        let gpus = vec![
            card(1, Hardware::Vaapi, Kind::Discrete, &[H264], &[H264, HEVC]),
            card(2, Hardware::Vaapi, Kind::Integrated, &[H264], &[HEVC]),
        ];
        let detected = Detected {
            encoders: Arc::new(software.clone()),
            gpus,
            software: Arc::new(software.clone()),
            others: Vec::new(),
            notes: Vec::new(),
        };
        let hevc = film("hevc", 1080);
        let probe = spec(&hevc, 720, &software);
        let pci = |run: &RunOn| run.gpu().map(|g| g.card().host.pci.clone());
        let full = |n: &'static [&'static str]| {
            move |gpu: &Gpu| !n.iter().any(|p| gpu.card().host.pci == *p)
        };

        let free = run_on(&detected, &probe, &full(&[]));
        assert!(!free.spilled());
        assert_eq!(pci(&free).as_deref(), Some("0000:01:00.0"));

        let one_full = run_on(&detected, &probe, &full(&["0000:01:00.0"]));
        assert!(!one_full.spilled());
        assert_eq!(pci(&one_full).as_deref(), Some("0000:02:00.0"));

        let both = run_on(&detected, &probe, &full(&["0000:01:00.0", "0000:02:00.0"]));
        assert!(both.spilled());
        assert_eq!(pci(&both).as_deref(), Some("0000:01:00.0"));
        let on = EncodeSpec {
            encoders: both.encoders(),
            vaapi_device: both.vaapi_device(),
            ..probe.clone()
        };
        let place = args::placement(&on).unwrap();
        assert_eq!(place.encoder.hardware, None, "encoded in software");
        assert!(place.decoder.is_some(), "decoded on the card");
    }

    /// A spill with no card that decodes the source runs all on the CPU.
    #[test]
    fn a_spill_no_card_decodes_runs_on_the_cpu() {
        let software = Encoders::software_baseline();
        let detected = Detected {
            encoders: Arc::new(software.clone()),
            gpus: vec![card(1, Hardware::Vaapi, Kind::Discrete, &[H264], &[H264])],
            software: Arc::new(software.clone()),
            others: Vec::new(),
            notes: Vec::new(),
        };
        let av1 = film("av1", 1080);
        let run = run_on(&detected, &spec(&av1, 720, &software), &|_| false);
        assert!(run.spilled());
        assert!(run.gpu().is_none());
    }

    /// PGPU-06: the planning view lists each codec of `video_codecs` in
    /// its order, with the encoder of every card that encodes it and has
    /// room, then software H.264 alone; a codec whose cards are all full
    /// is left out, so the planner takes the next.
    #[test]
    fn the_plan_takes_the_codecs_in_order_from_cards_with_room() {
        let mut software = Encoders::software_baseline();
        software.video.push(VideoEncoder {
            codec: HEVC,
            name: "libx265".into(),
            hardware: None,
        });
        let amd = {
            let mut encoders = card(
                1,
                Hardware::Vaapi,
                Kind::Discrete,
                &[H264, HEVC, AV1],
                &[HEVC],
            )
            .encoders()
            .clone();
            encoders.ten_bit = encoders
                .video
                .iter()
                .filter(|e| e.hardware.is_some() && e.codec != H264)
                .cloned()
                .collect();
            Arc::new(test_gpu(1, Hardware::Vaapi, Kind::Discrete, encoders))
        };
        let nvidia = card(2, Hardware::Nvenc, Kind::Discrete, &[H264, HEVC], &[HEVC]);
        let detected = Detected {
            encoders: Arc::new(software.clone()),
            gpus: vec![amd, nvidia],
            software: Arc::new(software),
            others: Vec::new(),
            notes: Vec::new(),
        };
        let names = |e: &Encoders| e.video.iter().map(|v| v.name.clone()).collect::<Vec<_>>();
        let order = [AV1, HEVC, H264];
        let all = planning(&detected, &order, &|_| true);
        assert_eq!(
            names(&all),
            [
                "av1_vaapi",
                "hevc_vaapi",
                "hevc_nvenc",
                "h264_vaapi",
                "h264_nvenc",
                "libx264"
            ]
        );
        let ten: Vec<&str> = all.ten_bit.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(ten, ["av1_vaapi", "hevc_vaapi"]);
        // The AMD card full: AV1 is gone, HEVC and H.264 from NVIDIA.
        let amd_full = planning(&detected, &order, &|gpu: &Gpu| {
            gpu.card().api != Hardware::Vaapi
        });
        assert_eq!(names(&amd_full), ["hevc_nvenc", "h264_nvenc", "libx264"]);
        assert!(amd_full.ten_bit.is_empty());
        // `video_codecs = []`: software H.264 alone.
        assert_eq!(names(&planning(&detected, &[], &|_| true)), ["libx264"]);
    }

    /// An encode that keeps HDR goes only to a card whose encoder writes
    /// 10 bits of its codec, even one later in order; tone mapped, any.
    #[test]
    fn an_encode_that_keeps_hdr_goes_to_a_ten_bit_card() {
        let software = Encoders::software_baseline();
        let eight = card(1, Hardware::Vaapi, Kind::Discrete, &[HEVC], &[HEVC]);
        let ten = {
            let mut encoders = card(2, Hardware::Vaapi, Kind::Integrated, &[HEVC], &[])
                .encoders()
                .clone();
            encoders.ten_bit = encoders
                .video
                .iter()
                .filter(|e| e.codec == HEVC)
                .cloned()
                .collect();
            Arc::new(test_gpu(2, Hardware::Vaapi, Kind::Integrated, encoders))
        };
        let gpus = [eight, ten];
        let mut hdr = film("hevc", 2160);
        {
            let v = hdr.video.as_mut().unwrap();
            v.bit_depth = 10;
            v.hdr = Some(spoolframe_types::HdrFormat::Hdr10);
            v.color_transfer = Some("smpte2084".into());
        }
        let encode = |tone_map| EncodeSpec {
            video: Some(ConvertedVideo::Encode {
                codec: HEVC,
                height: 1080,
                max_bitrate: None,
                tone_map,
            }),
            ..spec(&hdr, 1080, &software)
        };
        assert!(keeps_hdr(&encode(false)));
        assert_eq!(
            picked(&gpus, &encode(false), &[]).as_deref(),
            Some("0000:02:00.0")
        );
        assert!(!keeps_hdr(&encode(true)));
        assert_eq!(
            picked(&gpus, &encode(true), &[]).as_deref(),
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
            video: Some(ConvertedVideo::Copy {
                codec: H264,
                dolby_vision: false,
            }),
            ..spec(&h264, 1080, &software)
        };
        assert_eq!(picked(&gpus, &copy, &[]), None);
    }

    /// The Server page's routes are the jobs' own pick: an AV1 film to H.264
    /// on the card that decodes AV1, HEVC on the first card; AV1 out, which
    /// no card encodes and software no longer does (PGPU-06), nowhere; six
    /// jobs, seventeen routes, in order.
    #[test]
    fn routes_say_what_a_job_would_pick() {
        let software = Encoders::software_baseline();
        let ten_bit = |gpu: Arc<Gpu>, n: u8| {
            let mut encoders = gpu.encoders().clone();
            encoders.ten_bit = encoders
                .video
                .iter()
                .filter(|e| e.hardware.is_some() && e.codec != H264)
                .cloned()
                .collect();
            encoders
                .hardware_decoders
                .iter_mut()
                .for_each(|d| d.bit_depth = 10);
            Arc::new(test_gpu(
                n,
                gpu.card().api,
                gpu.card().host.class.kind,
                encoders,
            ))
        };
        let gpus = vec![
            ten_bit(
                card(
                    1,
                    Hardware::Nvenc,
                    Kind::Discrete,
                    &[H264, HEVC],
                    &[H264, HEVC],
                ),
                1,
            ),
            card(2, Hardware::Vaapi, Kind::Integrated, &[H264], &[H264, AV1]),
        ];
        let detected = Detected {
            encoders: Arc::new(software.clone()),
            gpus,
            software: Arc::new(software),
            others: Vec::new(),
            notes: Vec::new(),
        };
        let routes = routes(&detected, false, ToneMapSetting::Auto);
        assert_eq!(routes.len(), 17);
        let at = |job, source, target| {
            routes
                .iter()
                .find(|r| r.job == job && r.source == source && r.target == target)
                .unwrap()
        };
        let chain = |r: &Route| {
            r.steps
                .iter()
                .map(|s| (s.gpu.as_ref().map(|g| g.card().host.pci.clone()), s.share))
                .collect::<Vec<_>>()
        };
        let on = |n: u8| Some(format!("0000:{n:02x}:00.0"));
        // AV1 in: the card that decodes it does it all; the 1060 next,
        // encoding only; then, both full, a software H.264 encode decoding
        // on the card that decodes AV1.
        let av1_h264 = at(RouteJob::Convert, AV1, H264);
        assert_eq!(
            chain(av1_h264),
            [
                (on(2), Some(Share::Whole)),
                (on(1), Some(Share::EncodeOnly)),
                (on(2), None)
            ]
        );
        assert!(!av1_h264.busy_after);
        assert_eq!(
            av1_h264.steps[2].made.encoder.as_ref().map(|e| e.hardware),
            Some(None),
            "the spill encodes in software"
        );
        // HEVC out: the 1060 alone encodes it; full, a playback planned
        // takes the next codec, software encoding H.264 alone.
        let hevc_hevc = at(RouteJob::Convert, HEVC, HEVC);
        assert_eq!(chain(hevc_hevc), [(on(1), Some(Share::DecodeAndEncode))]);
        assert!(hevc_hevc.busy_after);
        // AV1 out, which nothing here encodes: no encoder at all.
        let h264_av1 = at(RouteJob::Convert, H264, AV1);
        assert_eq!(h264_av1.steps[0].made.encoder, None);
        // The HDR film tone mapped, kept in HEVC on the 10-bit 1060, and
        // the picture subtitles burned in.
        let hdr = at(RouteJob::ToneMap, HEVC, H264);
        assert!(hdr.steps[0].made.tone_mapper.is_some());
        let kept = at(RouteJob::KeepHdr, HEVC, HEVC);
        assert_eq!(
            kept.steps[0]
                .gpu
                .as_ref()
                .map(|g| g.card().host.pci.clone()),
            on(1)
        );
        assert_eq!(kept.steps[0].made.tone_mapper, None);
        assert!(
            !routes
                .iter()
                .any(|r| r.job == RouteJob::KeepHdr && r.target == H264),
            "H.264 keeps no HDR"
        );
        let burn = at(RouteJob::BurnIn, H264, HEVC);
        assert_ne!(burn.steps[0].made.burn_in, crate::encode::FilterOn::None);
    }

    /// A card whose only tone map is its driver's own (a GPU filter),
    /// with GPU filters off under `tone_map = "auto"`: the HDR film tone
    /// maps on the CPU, and the route says it is the setting; with them
    /// on, on the card, and no reason is given.
    #[test]
    fn a_route_says_why_its_tone_map_runs_on_the_cpu() {
        let software = Encoders::software_baseline();
        let mut encoders = card(1, Hardware::Vaapi, Kind::Discrete, &[H264], &[HEVC])
            .encoders()
            .clone();
        encoders.hardware_decoders[0].bit_depth = 10;
        // The driver's own tone map only, no libplacebo: a GPU filter,
        // so GPU filters decide.
        encoders.gpu_tonemap = true;
        let detected = Detected {
            encoders: Arc::new(software.clone()),
            gpus: vec![Arc::new(test_gpu(
                1,
                Hardware::Vaapi,
                Kind::Discrete,
                encoders,
            ))],
            software: Arc::new(software),
            others: Vec::new(),
            notes: Vec::new(),
        };
        let hdr = |gpu_filters| {
            routes(&detected, gpu_filters, ToneMapSetting::Auto)
                .into_iter()
                .find(|r| r.job == RouteJob::ToneMap && r.target == H264)
                .unwrap()
        };
        let off = hdr(false);
        assert_eq!(off.steps[0].made.tone_map, crate::encode::FilterOn::Cpu);
        assert_eq!(
            off.steps[0].cpu_why,
            ["GPU filters are off (transcode.gpu_filters)"]
        );
        let on = hdr(true);
        assert_eq!(on.steps[0].made.tone_map, crate::encode::FilterOn::Gpu);
        assert!(on.steps[0].cpu_why.is_empty(), "{:?}", on.steps[0].cpu_why);
    }

    /// PGPU-04 per card: an HDR film to SDR goes, under `tone_map =
    /// "auto"`, to the card whose libplacebo test passed, which does the
    /// whole job, over one earlier in order that would tone map on the
    /// CPU, with GPU filters on or off (auto's libplacebo is not a GPU
    /// filter).
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
            Some("0000:02:00.0")
        );
    }
}
