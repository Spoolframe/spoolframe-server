//! Playback debug (P3-15): what the admin API's Server › Playback and an
//! item's Debug tab read. Every playback with its plan and why, its
//! conversion's command line and stderr, its reads; any item's file as
//! ffprobe sees it, and the plan the server would make for a device
//! without playing anything. Nothing here changes a playback but
//! [`PlaybackDebug::stop`] and [`PlaybackDebug::end_conversion`].

use std::sync::Arc;

use spoolframe_admin_api as api;
use spoolframe_catalog::{Catalog, CatalogError};
use spoolframe_media::encoders::{Encoders, Hardware, ToneMapper, VulkanDevice};

use crate::transcode::Detected;
use spoolframe_media::{MediaDescriptor, Prober, SubtitleKind, SubtitleSource, codecs};
use spoolframe_protocol::{
    AudioAction, ConvertedVideo, PlaybackPlan, Selection, SubtitleAction, SubtitleDelivery,
};
use spoolframe_types::{
    AudioCodec, AudioRole, BitmapPreference, ClientCapabilities, Container, Delivery, MediaId,
    SubtitleFormat, SubtitleRole, VideoCodec,
};

use crate::connected::Connected;
use crate::encode::{Decoder, FilterOn, Load};
use crate::planner;
use crate::playback::{
    EndConversionError, EndReason, EndedPlayback, FirstSegment, Found, Furthest, KeyframesFrom,
    Playbacks, READS_KEPT, Snapshot, StartTimes,
};
use crate::previews::{PreviewPeek, Previews};
use crate::subtitles::{Subtitles, TextPeek, TextSource, TextState};
use crate::transcode::Transcoding;

/// What playback debug reads: the server's parts that play.
pub struct PlaybackDebug {
    pub playbacks: Arc<Playbacks>,
    pub connected: Arc<Connected>,
    pub catalog: Arc<dyn Catalog>,
    pub prober: Arc<Prober>,
    pub transcoding: Arc<Transcoding>,
    pub subtitles: Arc<Subtitles>,
    pub previews: Arc<Previews>,
    /// For `transcode.gpu_filters`, which decides where a plan's video
    /// encode runs (PGPU-02); none in tests that run no encode.
    pub jobs: Option<Arc<crate::encode::job::Jobs>>,
}

impl std::fmt::Debug for PlaybackDebug {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlaybackDebug").finish_non_exhaustive()
    }
}

/// Why playback debug could not answer.
#[derive(Debug, thiserror::Error)]
pub enum DebugError {
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    #[error("the media file could not be read: {0}")]
    Unreadable(String),
    #[error("{0}")]
    NoSuchDevice(String),
}

/// A stored capability profile for "Plan for…" (P3-15): what a kind of
/// client says it plays, for an item nobody has played on one yet.
#[derive(Debug, Clone, Copy)]
pub struct Profile {
    /// `profile:<key>` in [`api::DeviceChoice::id`].
    pub key: &'static str,
    pub name: &'static str,
    pub note: &'static str,
    pub capabilities: fn() -> ClientCapabilities,
}

/// The stored profiles, as the clients send them.
pub const PROFILES: [Profile; 5] = [
    Profile {
        key: "linux",
        name: "Linux app",
        note: "The Linux app with GStreamer's usual plugins: nearly every codec and \
               container, every subtitle format, the file or a progressive stream.",
        capabilities: linux,
    },
    Profile {
        key: "apple-tv",
        name: "Apple TV",
        note: "AVPlayer: H.264 and HEVC, AAC, AC-3 and E-AC-3, MP4 and TS, WebVTT, HLS only.",
        capabilities: apple_tv,
    },
    Profile {
        key: "iphone",
        name: "iPhone",
        note: "AVPlayer as on Apple TV, with stereo sound.",
        capabilities: iphone,
    },
    Profile {
        key: "mac",
        name: "Mac",
        note: "AVPlayer as on Apple TV.",
        capabilities: apple_tv,
    },
    Profile {
        key: "cli",
        name: "CLI",
        note: "spoolframe's command-line tools: no player, so nothing plays.",
        capabilities: ClientCapabilities::default,
    },
];

/// What the Linux app's capabilities say on a machine with GStreamer's
/// good, bad, ugly and libav plugins (spoolframe-linux `capabilities()`).
pub fn linux() -> ClientCapabilities {
    ClientCapabilities {
        video: [
            VideoCodec::H264,
            VideoCodec::Hevc,
            VideoCodec::Av1,
            VideoCodec::Vp9,
            VideoCodec::Vp8,
            VideoCodec::Mpeg2,
            VideoCodec::Mpeg4,
            VideoCodec::Vc1,
        ]
        .into(),
        audio: [
            AudioCodec::Aac,
            AudioCodec::Mp3,
            AudioCodec::Ac3,
            AudioCodec::Eac3,
            AudioCodec::Dts,
            AudioCodec::TrueHd,
            AudioCodec::Flac,
            AudioCodec::Alac,
            AudioCodec::Opus,
            AudioCodec::Vorbis,
            AudioCodec::Pcm,
        ]
        .into(),
        containers: [
            Container::Matroska,
            Container::Mp4,
            Container::MpegTs,
            Container::Avi,
            Container::Flac,
            Container::Mp3,
            Container::Ogg,
        ]
        .into(),
        subtitles: [
            SubtitleFormat::Srt,
            SubtitleFormat::Ass,
            SubtitleFormat::WebVtt,
            SubtitleFormat::MovText,
            SubtitleFormat::Pgs,
            SubtitleFormat::VobSub,
            SubtitleFormat::DvbSub,
        ]
        .into(),
        delivery: vec![Delivery::Range, Delivery::Progressive],
        ..ClientCapabilities::default()
    }
}

/// AVPlayer on Apple TV (P2-05, P5): what the HLS work was made for.
pub fn apple_tv() -> ClientCapabilities {
    ClientCapabilities {
        video: [VideoCodec::H264, VideoCodec::Hevc].into(),
        audio: [AudioCodec::Aac, AudioCodec::Ac3, AudioCodec::Eac3].into(),
        containers: [Container::Mp4, Container::MpegTs].into(),
        subtitles: [SubtitleFormat::WebVtt].into(),
        delivery: vec![Delivery::Hls],
        ..ClientCapabilities::default()
    }
}

/// AVPlayer on an iPhone: as Apple TV, to its speakers or headphones.
pub fn iphone() -> ClientCapabilities {
    ClientCapabilities {
        max_audio_channels: Some(2),
        ..apple_tv()
    }
}

fn plan_kind(plan: &PlaybackPlan) -> api::PlanKind {
    match plan {
        PlaybackPlan::Direct { .. } => api::PlanKind::Direct,
        PlaybackPlan::Converted { video, audio, .. } => {
            let encodes = matches!(video, Some(ConvertedVideo::Encode { .. }))
                || audio.is_some_and(|a| matches!(a.action, AudioAction::Encode { .. }));
            if encodes {
                api::PlanKind::Transcode
            } else {
                api::PlanKind::Remux
            }
        }
        _ => api::PlanKind::Unsupported,
    }
}

fn plan_line(plan: &PlaybackPlan) -> api::PlanLine {
    let s = planner::summary(plan);
    api::PlanLine {
        kind: s.kind.to_owned(),
        delivery: s.delivery.to_string(),
        video: s.video,
        audio: s.audio,
        subtitle: s.subtitle,
    }
}

fn reasons(plan: &PlaybackPlan) -> Vec<String> {
    match plan {
        PlaybackPlan::Converted { reasons, .. } | PlaybackPlan::Unsupported { reasons } => {
            reasons.clone()
        }
        _ => Vec::new(),
    }
}

/// The stream a planner reason names by its first word: `container:`,
/// `delivery:`, `video:`, `bitrate:`, `audio 1:`, `subtitle 3:`.
fn named(reason: &str) -> Option<(api::StreamKind, Option<u32>)> {
    let (head, _) = reason.split_once(':')?;
    let mut words = head.split(' ');
    let kind = match words.next()? {
        "container" | "delivery" => api::StreamKind::Container,
        "video" | "bitrate" => api::StreamKind::Video,
        "audio" => api::StreamKind::Audio,
        "subtitle" => api::StreamKind::Subtitle,
        _ => return None,
    };
    let index = match kind {
        api::StreamKind::Audio | api::StreamKind::Subtitle => Some(words.next()?.parse().ok()?),
        _ => None,
    };
    Some((kind, index))
}

/// The video stream as a person reads it: `hevc main 10 2160p hdr10`.
fn video_source(d: &MediaDescriptor) -> String {
    let Some(v) = &d.video else {
        return String::new();
    };
    let mut parts = vec![v.codec.clone()];
    if let Some(profile) = &v.profile {
        parts.push(profile.to_lowercase());
    }
    parts.push(format!("{}p", v.height));
    if let Some(hdr) = v.hdr {
        parts.push(hdr.to_string());
    }
    parts.join(" ")
}

fn channels(n: Option<u32>, layout: Option<&str>) -> Option<String> {
    match (layout, n) {
        (Some(layout), _) => Some(layout.replace("(side)", "")),
        (None, Some(n)) => Some(format!("{n}ch")),
        (None, None) => None,
    }
}

fn audio_source(d: &MediaDescriptor, index: u32) -> String {
    let Some(a) = d.audio.get(index as usize) else {
        return String::new();
    };
    [
        Some(a.codec.clone()),
        channels(a.channels, a.channel_layout.as_deref()),
        a.language.clone(),
        (a.role != AudioRole::Main).then(|| audio_role(a.role).to_owned()),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ")
}

/// A subtitle track as a person reads it: `pgs eng forced`.
fn subtitle_label(d: &MediaDescriptor, index: u32) -> String {
    let Some(s) = d.subtitles.get(index as usize) else {
        return String::new();
    };
    let format = codecs::subtitle_format(&s.codec);
    [
        Some(if format == SubtitleFormat::Unknown {
            s.codec.clone()
        } else {
            format.to_string()
        }),
        s.language.clone(),
        (s.role != SubtitleRole::Full).then(|| subtitle_role(s.role).to_owned()),
        matches!(s.source, SubtitleSource::External { .. }).then(|| "file".to_owned()),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ")
}

fn audio_role(role: AudioRole) -> &'static str {
    match role {
        AudioRole::Main => "main",
        AudioRole::AudioDescription => "audio description",
        AudioRole::Commentary => "commentary",
        AudioRole::Other => "other",
    }
}

fn subtitle_role(role: SubtitleRole) -> &'static str {
    match role {
        SubtitleRole::Sdh => "sdh",
        SubtitleRole::Forced => "forced",
        SubtitleRole::Commentary => "commentary",
        SubtitleRole::Full => "full",
    }
}

/// The audio track a plan without one picked plays: the file's default.
fn default_audio(d: &MediaDescriptor) -> Option<u32> {
    (!d.audio.is_empty()).then(|| d.audio.iter().position(|a| a.default).unwrap_or(0) as u32)
}

/// `plan` laid out by stream, the file described by `d` (when it could
/// be read), the tracks picked `selection`.
pub fn plan_view(
    plan: &PlaybackPlan,
    d: Option<&MediaDescriptor>,
    selection: Selection,
    detected: &Detected,
    gpu_filters: bool,
) -> api::PlanView {
    let encoders = &detected.encoders;
    use api::{StreamKind as K, StreamOut as Out};
    let empty = MediaDescriptor {
        container: String::new(),
        duration: None,
        bit_rate: None,
        video: None,
        audio: Vec::new(),
        subtitles: Vec::new(),
    };
    let d = d.unwrap_or(&empty);
    let audio_encoder = |codec: AudioCodec| encoders.audio_encoder(codec).map(|e| e.name.clone());
    let mut rows: Vec<api::PlanStream> = Vec::new();
    let mut row = |stream, index, source: String, out| {
        rows.push(api::PlanStream {
            stream,
            index,
            source,
            out,
            reasons: Vec::new(),
        });
    };
    match plan {
        PlaybackPlan::Direct {
            video,
            audio,
            subtitle,
            ..
        } => {
            if video.is_some() {
                row(K::Video, None, video_source(d), Out::Direct);
            }
            if let Some(a) = audio {
                row(
                    K::Audio,
                    Some(a.index),
                    audio_source(d, a.index),
                    Out::Direct,
                );
            }
            if let Some(s) = subtitle {
                let own = d
                    .subtitles
                    .get(s.index as usize)
                    .map(|t| (codecs::subtitle_format(&t.codec), t.kind));
                let out = match own {
                    Some((from, kind)) if from != s.format => {
                        let (from, to) = (from.to_string(), s.format.to_string());
                        match kind {
                            SubtitleKind::Bitmap => Out::ReadAsText { from, to },
                            SubtitleKind::Text => Out::Convert { from, to },
                        }
                    }
                    _ if s.delivery == SubtitleDelivery::External => Out::Beside {
                        format: s.format.to_string(),
                    },
                    _ => Out::Direct,
                };
                row(K::Subtitle, Some(s.index), subtitle_label(d, s.index), out);
            }
        }
        PlaybackPlan::Converted {
            container,
            video,
            audio,
            subtitle,
            ..
        } => {
            row(
                K::Container,
                None,
                d.container.clone(),
                Out::Remux {
                    to: container.to_string(),
                },
            );
            match video {
                Some(ConvertedVideo::Copy { codec }) => row(
                    K::Video,
                    None,
                    video_source(d),
                    Out::Copy {
                        codec: codec.to_string(),
                    },
                ),
                Some(ConvertedVideo::Encode {
                    codec,
                    height,
                    max_bitrate,
                    tone_map,
                }) => row(
                    K::Video,
                    None,
                    video_source(d),
                    Out::Encode {
                        codec: codec.to_string(),
                        height: Some(*height),
                        channels: None,
                        bitrate: *max_bitrate,
                        tone_map: *tone_map,
                        // The one the run picks for this film (PGPU-02).
                        encoder: crate::encode::pick::planned_encoder(
                            detected,
                            d,
                            ConvertedVideo::Encode {
                                codec: *codec,
                                height: *height,
                                max_bitrate: *max_bitrate,
                                tone_map: *tone_map,
                            },
                            *subtitle,
                            gpu_filters,
                        )
                        .map(|e| e.name),
                    },
                ),
                _ => {}
            }
            if let Some(a) = audio {
                let out = match a.action {
                    AudioAction::Copy { codec } => Out::Copy {
                        codec: codec.to_string(),
                    },
                    AudioAction::Encode {
                        codec,
                        channels,
                        bitrate,
                    } => Out::Encode {
                        codec: codec.to_string(),
                        height: None,
                        channels: Some(channels),
                        bitrate: Some(bitrate),
                        tone_map: false,
                        encoder: audio_encoder(codec),
                    },
                    AudioAction::Unknown => Out::Unplayable,
                };
                row(K::Audio, Some(a.index), audio_source(d, a.index), out);
            }
            if let Some(s) = subtitle {
                let out = match s.action {
                    SubtitleAction::Copy { format } => Out::Copy {
                        codec: format.to_string(),
                    },
                    SubtitleAction::Convert { from, to } => Out::Convert {
                        from: from.to_string(),
                        to: to.to_string(),
                    },
                    SubtitleAction::BurnIn { format } => Out::BurnIn {
                        format: format.to_string(),
                    },
                    SubtitleAction::Ocr { from, to } => Out::ReadAsText {
                        from: from.to_string(),
                        to: to.to_string(),
                    },
                    SubtitleAction::Unknown => Out::Unplayable,
                };
                row(K::Subtitle, Some(s.index), subtitle_label(d, s.index), out);
            }
        }
        PlaybackPlan::Unsupported { .. } => {
            if d.video.is_some() {
                row(K::Video, None, video_source(d), Out::Direct);
            }
            if let Some(a) = selection.audio.or_else(|| default_audio(d)) {
                row(K::Audio, Some(a), audio_source(d, a), Out::Direct);
            }
            if let Some(s) = selection.subtitle {
                row(K::Subtitle, Some(s), subtitle_label(d, s), Out::Direct);
            }
        }
        _ => {}
    }
    // Each reason goes to the stream it names; one that names none, or a
    // stream with no row, stays the plan's.
    let unsupported = matches!(plan, PlaybackPlan::Unsupported { .. });
    let mut left = Vec::new();
    for reason in reasons(plan) {
        let found = named(&reason).and_then(|(kind, index)| {
            if unsupported && kind == K::Container && !rows.iter().any(|r| r.stream == kind) {
                rows.insert(
                    0,
                    api::PlanStream {
                        stream: K::Container,
                        index: None,
                        source: d.container.clone(),
                        out: Out::Unplayable,
                        reasons: Vec::new(),
                    },
                );
            }
            rows.iter_mut()
                .find(|r| r.stream == kind && r.index == index)
        });
        match found {
            Some(row) => {
                if unsupported {
                    row.out = Out::Unplayable;
                }
                row.reasons.push(reason);
            }
            None => left.push(reason),
        }
    }
    api::PlanView {
        kind: plan_kind(plan),
        line: plan_line(plan),
        streams: rows,
        reasons: left,
        plan: serde_json::to_value(plan).unwrap_or_default(),
    }
}

fn row(s: &Snapshot) -> api::PlaybackRow {
    api::PlaybackRow {
        id: s.id.clone(),
        user_id: s.viewer.user_id.clone(),
        username: s.viewer.username.clone(),
        display_name: s.viewer.display_name.clone(),
        device: s.viewer.device(),
        platform: s.viewer.platform,
        item: s.item.to_string(),
        title: s.shown.title.clone(),
        subtitle: s.shown.subtitle.clone(),
        duration_ms: s.shown.duration_ms,
        plan: plan_kind(&s.planned),
        line: plan_line(&s.planned),
        reasons: reasons(&s.planned),
        epoch: s.epoch,
        revision: s.revision,
        started_at: s.started_at,
        position_ms: s.position_ms,
        read_to_ms: s.read_to_ms,
        furthest: s.furthest.map(|f| match f {
            Furthest::Byte(offset) => api::ReadMark::Byte { offset },
            Furthest::Segment(n) => api::ReadMark::Segment { n },
        }),
        last_read_at: s.last_read_at,
        conversion: s.conversion.as_ref().map(|c| api::ConversionState {
            converted_ms: c.converted_ms,
            bytes: c.bytes,
            last_segment: c.last_segment,
            paused: c.paused,
            running: c.running,
            ended: c.ended,
            failed: c.failed.clone(),
            encodes: c.encodes,
            load: match c.load {
                Load::Remux => api::ConversionLoad::Remux,
                Load::Audio => api::ConversionLoad::Audio,
                Load::Gpu => api::ConversionLoad::Gpu,
                Load::GpuOnly => api::ConversionLoad::GpuOnly,
                Load::Software => api::ConversionLoad::Software,
            },
            weight_quarters: c.load.weight().get(),
        }),
        made: made(s),
        start: start(&s.start),
    }
}

pub(crate) fn gpu_api(hardware: Hardware) -> api::GpuApi {
    match hardware {
        Hardware::Vaapi => api::GpuApi::Vaapi,
        Hardware::Nvenc => api::GpuApi::Nvidia,
        Hardware::Qsv => api::GpuApi::Qsv,
    }
}

/// A tone map (PGPU-04), as the admin API names it.
pub(crate) fn tone_mapper(mapper: ToneMapper) -> api::ToneMapper {
    match mapper {
        ToneMapper::Cpu => api::ToneMapper::Cpu,
        ToneMapper::Vaapi => api::ToneMapper::Vaapi,
        ToneMapper::Libplacebo(VulkanDevice::Shared) => api::ToneMapper::LibplaceboShared,
        ToneMapper::Libplacebo(VulkanDevice::Own) => api::ToneMapper::LibplaceboOwn,
    }
}

/// The tone maps each GPU runs (PGPU-04), for the transcoding status.
pub(crate) fn gpu_tone_maps(encoders: &Encoders) -> Vec<api::GpuToneMap> {
    [Hardware::Vaapi, Hardware::Nvenc]
        .into_iter()
        .flat_map(|family| {
            encoders
                .gpu_tone_mappers(family)
                .into_iter()
                .map(move |mapper| api::GpuToneMap {
                    gpu: gpu_api(family),
                    mapper: tone_mapper(mapper),
                })
        })
        .collect()
}

fn filter_place(on: FilterOn) -> api::FilterPlace {
    match on {
        FilterOn::None => api::FilterPlace::None,
        FilterOn::Gpu => api::FilterPlace::Gpu,
        FilterOn::Cpu => api::FilterPlace::Cpu,
    }
}

/// How `s` is made (PX-24): its running encode's facts, as the server
/// built its graph; for direct play, the file as it is.
fn made(s: &Snapshot) -> api::PlaybackMade {
    let conversion = s.conversion.as_ref();
    let Some(made) = conversion.and_then(|c| c.made.as_ref()) else {
        let (width, height) = s.video_size.unzip();
        return api::PlaybackMade {
            decoder: api::VideoDecoder::None,
            encoder: None,
            tone_map: api::FilterPlace::None,
            tone_mapper: None,
            burn_in: api::FilterPlace::None,
            width,
            height,
            target_bitrate: None,
            measured_bitrate: conversion.and_then(|c| c.output.bitrate()),
        };
    };
    let (width, height) = made.size.unzip();
    api::PlaybackMade {
        decoder: match made.decoder {
            Decoder::None => api::VideoDecoder::None,
            Decoder::Software => api::VideoDecoder::Software,
            Decoder::Hardware(hardware) => api::VideoDecoder::Gpu {
                api: gpu_api(hardware),
            },
            Decoder::Vulkan => api::VideoDecoder::Gpu {
                api: api::GpuApi::Vulkan,
            },
        },
        encoder: made.encoder.as_ref().map(|e| api::VideoEncoderUsed {
            codec: e.codec.to_string(),
            name: e.name.clone(),
            gpu: e.hardware.map(gpu_api),
        }),
        tone_map: filter_place(made.tone_map),
        tone_mapper: made.tone_mapper.map(tone_mapper),
        burn_in: filter_place(made.burn_in),
        width,
        height,
        target_bitrate: made.target_bitrate,
        measured_bitrate: conversion.and_then(|c| c.output.bitrate()),
    }
}

fn start(t: &StartTimes) -> api::PlaybackStart {
    api::PlaybackStart {
        plan_ms: t.plan.as_millis() as u64,
        keyframes: match t.keyframes {
            None => api::KeyframesRead::None,
            Some((took, KeyframesFrom::Index)) => api::KeyframesRead::Index {
                ms: took.as_millis() as u64,
            },
            Some((took, KeyframesFrom::Scan)) => api::KeyframesRead::Scan {
                ms: took.as_millis() as u64,
            },
        },
        first_segment: match t.first_segment {
            FirstSegment::Direct => api::FirstSegment::NotApplicable,
            FirstSegment::Waiting => api::FirstSegment::Waiting,
            FirstSegment::Ready(took) => api::FirstSegment::Ready {
                ms: took.as_millis() as u64,
            },
            FirstSegment::Never => api::FirstSegment::Never,
        },
    }
}

fn why(reason: &EndReason) -> api::EndedWhy {
    match reason {
        EndReason::Finished => api::EndedWhy::Finished,
        EndReason::StoppedByAdmin => api::EndedWhy::Stopped,
        EndReason::Idle => api::EndedWhy::Idle,
        EndReason::Replaced => api::EndedWhy::Replaced,
        EndReason::DeviceGone => api::EndedWhy::DeviceGone,
        EndReason::SignedOut => api::EndedWhy::SignedOut,
        EndReason::AccessRemoved => api::EndedWhy::AccessRemoved,
        EndReason::Failed(reason) => api::EndedWhy::Failed {
            reason: reason.clone(),
        },
    }
}

fn ended(e: &EndedPlayback) -> api::EndedPlayback {
    api::EndedPlayback {
        at: e.at,
        why: why(&e.why),
        playback: row(&e.last),
    }
}

fn unix_ms() -> i64 {
    crate::metrics::history::unix_ms()
}

impl PlaybackDebug {
    fn gpu_filters(&self) -> bool {
        self.jobs.as_ref().is_some_and(|j| j.limits().gpu_filters)
    }

    /// Server › Playback.
    pub fn list(&self) -> api::PlaybackList {
        api::PlaybackList {
            now: unix_ms(),
            open: self.playbacks.snapshots().iter().map(row).collect(),
            ended: self.playbacks.ended().iter().map(ended).collect(),
        }
    }

    /// One playback's page; `None` for an id neither open nor ended in
    /// the last hour.
    pub async fn detail(&self, id: &str) -> Option<api::PlaybackDetail> {
        let (snapshot, gone) = match self.playbacks.find(id)? {
            Found::Open(s) => (s, None),
            Found::Ended(e) => (e.last.clone(), Some(ended(&e))),
        };
        // Cached by the prober: what it described at the start.
        let descriptor = self.prober.describe(&snapshot.path).await.ok();
        let plan = plan_view(
            &snapshot.planned,
            descriptor.as_deref(),
            snapshot.selection,
            &self.transcoding.detected(),
            self.gpu_filters(),
        );
        Some(api::PlaybackDetail {
            now: unix_ms(),
            playback: row(&snapshot),
            ended: gone,
            plan,
            command: snapshot.conversion.as_ref().map(|c| c.command.clone()),
            stderr: snapshot
                .conversion
                .as_ref()
                .map(|c| c.stderr.clone())
                .unwrap_or_default(),
            reads: snapshot
                .reads
                .iter()
                .map(|r| api::ReadPoint {
                    at: r.at,
                    read_ms: r.read_ms,
                    converted_ms: r.converted_ms,
                })
                .collect(),
            reads_window_secs: READS_KEPT.as_secs() as u32,
        })
    }

    /// An admin's Stop: whether it was open. The end is logged with
    /// every other ([`Playbacks`]).
    pub fn stop(&self, id: &str) -> bool {
        self.playbacks.stop(id)
    }

    /// An admin's End conversion.
    pub fn end_conversion(&self, id: &str) -> Result<(), EndConversionError> {
        self.playbacks.end_conversion(id)?;
        tracing::info!(playback = id, "an admin ended a playback's conversion");
        Ok(())
    }

    /// The devices "Plan for…" offers: those connected now, then the
    /// stored profiles.
    pub fn devices(&self) -> Vec<api::DeviceChoice> {
        let connected = self.connected.devices().into_iter().map(|d| {
            let note = match &d.capabilities {
                Some(_) => format!(
                    "Connected now: {} {} on {}",
                    d.client_name,
                    d.client_version,
                    d.platform.as_str()
                ),
                None => format!(
                    "Connected now: {} {}; it sent no capabilities, so nothing plays",
                    d.client_name, d.client_version
                ),
            };
            api::DeviceChoice {
                id: format!("device:{}", d.endpoint),
                name: d.label(),
                connected: true,
                note,
            }
        });
        let stored = PROFILES.iter().map(|p| api::DeviceChoice {
            id: format!("profile:{}", p.key),
            name: p.name.to_owned(),
            connected: false,
            note: p.note.to_owned(),
        });
        connected.chain(stored).collect()
    }

    /// What `device` (a [`api::DeviceChoice::id`]) says it plays: `None`
    /// when it sent nothing, as a client may.
    fn capabilities(&self, device: &str) -> Result<Option<ClientCapabilities>, DebugError> {
        if let Some(endpoint) = device.strip_prefix("device:") {
            return self
                .connected
                .device(endpoint)
                .map(|d| d.capabilities)
                .ok_or_else(|| {
                    DebugError::NoSuchDevice("that device is not connected now".into())
                });
        }
        let key = device.strip_prefix("profile:").unwrap_or(device);
        PROFILES
            .iter()
            .find(|p| p.key == key)
            .map(|p| Some((p.capabilities)()))
            .ok_or_else(|| DebugError::NoSuchDevice(format!("no device or profile {device}")))
    }

    async fn describe(
        &self,
        id: &MediaId,
    ) -> Result<(std::path::PathBuf, Arc<MediaDescriptor>), DebugError> {
        let path = self.catalog.media_path(id).await?;
        let descriptor = self
            .prober
            .describe(&path)
            .await
            .map_err(|e| DebugError::Unreadable(e.to_string()))?;
        Ok((path, descriptor))
    }

    /// The plan the server would make for item `id` on `request`'s
    /// device: what `PlaybackPlan` answers that device for those tracks.
    pub async fn plan_for(
        &self,
        id: &MediaId,
        request: &api::PlanFor,
    ) -> Result<api::PlanView, DebugError> {
        let caps = self.capabilities(&request.device)?;
        let (_, descriptor) = self.describe(id).await?;
        let selection = Selection {
            audio: request.audio,
            subtitle: request.subtitle,
        };
        // `Auto` is planned as for a viewer who never chose.
        let bitmap = match request.pictures {
            api::PictureSubtitles::Auto => planner::Bitmap::Preferred(BitmapPreference::default()),
            api::PictureSubtitles::Text => planner::Bitmap::Text,
            // As the client protocol's `burn_in`: pictures.
            api::PictureSubtitles::BurnIn => planner::Bitmap::Preferred(BitmapPreference::Pictures),
        };
        let detected = self.transcoding.detected();
        let encoders = detected.encoders.clone();
        // As the client protocol plans it: without capabilities, nothing.
        let plan = match &caps {
            None => planner::plan_without_capabilities(),
            Some(caps) => planner::plan(
                &descriptor,
                Some(caps),
                selection,
                bitmap,
                &encoders,
                self.subtitles.ocr(),
            ),
        };
        Ok(plan_view(
            &plan,
            Some(&descriptor),
            selection,
            &detected,
            self.gpu_filters(),
        ))
    }

    /// An item's Debug tab.
    pub async fn item(&self, id: &MediaId) -> Result<api::ItemDebug, DebugError> {
        let devices = self.devices();
        let previews = match self.previews.peek(id).await {
            Ok(PreviewPeek::Ready(meta)) => api::PreviewCache::Ready {
                count: meta.count,
                interval_ms: meta.interval_ms,
                sheets: meta.sheets,
            },
            Ok(PreviewPeek::Making { done_percent }) => api::PreviewCache::Making { done_percent },
            Ok(PreviewPeek::NotMade) => api::PreviewCache::NotMade,
            Ok(PreviewPeek::None(why)) => api::PreviewCache::None {
                reason: why.to_owned(),
            },
            Err(e) => api::PreviewCache::None {
                reason: e.to_string(),
            },
        };
        let (path, descriptor) = match self.describe(id).await {
            Ok(found) => found,
            Err(DebugError::Catalog(e)) => return Err(DebugError::Catalog(e)),
            Err(e) => {
                return Ok(api::ItemDebug {
                    probe: None,
                    probe_error: Some(e.to_string()),
                    devices,
                    subtitles: Vec::new(),
                    previews,
                });
            }
        };
        let size = tokio::fs::metadata(&path).await.ok().map(|m| m.len());
        Ok(api::ItemDebug {
            probe: Some(probe(&path, &descriptor, size)),
            probe_error: None,
            devices,
            subtitles: self.subtitle_caches(&path, &descriptor).await,
            previews,
        })
    }

    /// Each subtitle track's text as the cache has it: every picture
    /// track, and a text track once something of it is made.
    async fn subtitle_caches(
        &self,
        path: &std::path::Path,
        d: &MediaDescriptor,
    ) -> Vec<api::SubtitleCache> {
        let mut rows = Vec::new();
        for (track, stream) in d.subtitles.iter().enumerate() {
            let track = track as u32;
            let own = codecs::subtitle_format(&stream.codec);
            let ocr = stream.kind == SubtitleKind::Bitmap;
            if !ocr && own == SubtitleFormat::Unknown {
                continue;
            }
            let label = subtitle_label(d, track);
            let mut found = Vec::new();
            let mut unavailable = None;
            for to in [SubtitleFormat::Srt, SubtitleFormat::WebVtt] {
                if to == own {
                    continue;
                }
                let state = match self.subtitles.peek(TextSource { path, stream, to }).await {
                    TextPeek::NotMade => continue,
                    TextPeek::Ready => api::CacheState::Ready,
                    TextPeek::Making(TextState::Waiting) => api::CacheState::Waiting,
                    TextPeek::Making(TextState::Reading { done_percent }) => {
                        api::CacheState::Making { done_percent }
                    }
                    TextPeek::Making(TextState::Ready(_)) => api::CacheState::Ready,
                    TextPeek::Making(TextState::Failed(reason)) => {
                        api::CacheState::Failed { reason }
                    }
                    TextPeek::Unavailable(reason) => {
                        unavailable = Some(reason);
                        break;
                    }
                };
                found.push((to, state));
            }
            if let Some(reason) = unavailable {
                rows.push(api::SubtitleCache {
                    track,
                    label,
                    ocr,
                    to: None,
                    state: api::CacheState::Unavailable { reason },
                });
            } else if found.is_empty() {
                if ocr {
                    rows.push(api::SubtitleCache {
                        track,
                        label,
                        ocr,
                        to: None,
                        state: api::CacheState::NotMade,
                    });
                }
            } else {
                for (to, state) in found {
                    rows.push(api::SubtitleCache {
                        track,
                        label: label.clone(),
                        ocr,
                        to: Some(to.to_string()),
                        state,
                    });
                }
            }
        }
        rows
    }
}

/// `d`, the file at `path`, as the probe table shows it.
fn probe(path: &std::path::Path, d: &MediaDescriptor, size: Option<u64>) -> api::Probe {
    let mut streams = Vec::new();
    if let Some(v) = &d.video {
        let mut details = Vec::new();
        if let Some(p) = &v.profile {
            details.push(p.to_lowercase());
        }
        details.push(format!("{}×{}", v.width, v.height));
        if let Some(rate) = v.frame_rate_millis {
            details.push(format!("{:.3} fps", f64::from(rate) / 1000.0));
        }
        details.push(format!("{}-bit", v.bit_depth));
        if let Some(hdr) = v.hdr {
            details.push(hdr.to_string());
        }
        streams.push(api::ProbeStream {
            index: Some(v.index),
            kind: api::StreamKind::Video,
            track: 0,
            codec: v.codec.clone(),
            details: details.join(" · "),
            language: None,
            title: None,
            role: None,
            default: false,
            forced: false,
            file: None,
        });
    }
    for (track, a) in d.audio.iter().enumerate() {
        let details = [
            a.profile.clone(),
            channels(a.channels, a.channel_layout.as_deref()),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
        streams.push(api::ProbeStream {
            index: Some(a.index),
            kind: api::StreamKind::Audio,
            track: track as u32,
            codec: a.codec.clone(),
            details,
            language: a.language.clone(),
            title: a.title.clone(),
            role: Some(audio_role(a.role).to_owned()),
            default: a.default,
            forced: false,
            file: None,
        });
    }
    for (track, s) in d.subtitles.iter().enumerate() {
        let (index, file) = match &s.source {
            SubtitleSource::Embedded { index } => (Some(*index), None),
            SubtitleSource::External { path } => (
                None,
                Some(path.file_name().map_or_else(
                    || path.display().to_string(),
                    |n| n.to_string_lossy().into_owned(),
                )),
            ),
        };
        streams.push(api::ProbeStream {
            index,
            kind: api::StreamKind::Subtitle,
            track: track as u32,
            codec: s.codec.clone(),
            details: match s.kind {
                SubtitleKind::Text => "text".to_owned(),
                SubtitleKind::Bitmap => "pictures".to_owned(),
            },
            language: s.language.clone(),
            title: s.title.clone(),
            role: Some(subtitle_role(s.role).to_owned()),
            default: s.default,
            forced: s.forced,
            file,
        });
    }
    api::Probe {
        path: path.display().to_string(),
        container: d.container.clone(),
        duration_ms: d.duration.map(|d| d.as_millis() as u64),
        bit_rate: d.bit_rate,
        size,
        streams,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reason_goes_to_the_stream_it_names() {
        assert_eq!(
            named("audio 1: dts not supported"),
            Some((api::StreamKind::Audio, Some(1)))
        );
        assert_eq!(
            named("subtitle 3: pgs cannot be shown; burned into the video"),
            Some((api::StreamKind::Subtitle, Some(3)))
        );
        assert_eq!(
            named("bitrate: 40.0 Mbit/s over the client's 8.0 Mbit/s"),
            Some((api::StreamKind::Video, None))
        );
        assert_eq!(
            named("delivery: the client takes hls, not the file"),
            Some((api::StreamKind::Container, None))
        );
        assert_eq!(named("the client sent no capabilities"), None);
    }

    /// PGPU-01: a run decoded on NVDEC reaches the page as NVIDIA's API,
    /// which the web names `NVDEC (CUDA)`.
    #[test]
    fn an_nvdec_decode_is_reported_as_nvidias() {
        let decoder = api::VideoDecoder::Gpu {
            api: gpu_api(Hardware::Nvenc),
        };
        assert_eq!(
            serde_json::to_value(decoder).unwrap(),
            serde_json::json!({ "kind": "gpu", "api": "nvidia" })
        );
    }

    /// PGPU-02: the plan names the encoder the run picks for this film, not
    /// the first for the codec: an AV1 film to H.264 on a host with a
    /// GTX 1060 (no AV1 decode) before an AMD GPU (AV1 decode) encodes on
    /// the AMD one.
    #[test]
    fn the_plan_names_the_encoder_the_run_picks() {
        use spoolframe_media::VideoStream;
        use spoolframe_media::encoders::{Encoders, HardwareDecoder, VideoEncoder};

        let encoder = |name: &str, hardware| VideoEncoder {
            codec: VideoCodec::H264,
            name: name.into(),
            hardware,
        };
        // A GTX 1060 first, an AMD GPU second (PGPU-05: two cards, each
        // its own detection).
        let nvidia = Encoders {
            video: vec![
                encoder("h264_nvenc", Some(Hardware::Nvenc)),
                encoder("libx264", None),
            ],
            ..Encoders::software_baseline()
        };
        let amd = Encoders {
            video: vec![
                encoder("h264_vaapi", Some(Hardware::Vaapi)),
                encoder("libx264", None),
            ],
            hardware_decoders: vec![HardwareDecoder {
                codec: VideoCodec::Av1,
                hardware: Hardware::Vaapi,
                bit_depth: 8,
            }],
            ..Encoders::software_baseline()
        };
        let kind = spoolframe_media::gpus::Kind::Discrete;
        let detected = Detected::with_gpus(vec![
            crate::transcode::test_gpu(1, Hardware::Nvenc, kind, nvidia),
            crate::transcode::test_gpu(2, Hardware::Vaapi, kind, amd),
        ]);
        let film = MediaDescriptor {
            container: "matroska,webm".into(),
            duration: None,
            bit_rate: None,
            video: Some(VideoStream {
                index: 0,
                codec: "av1".into(),
                profile: None,
                level: None,
                width: 1920,
                height: 1080,
                pixel_format: Some("yuv420p".into()),
                color_transfer: None,
                frame_rate_millis: None,
                bit_depth: 8,
                hdr: None,
            }),
            audio: Vec::new(),
            subtitles: Vec::new(),
        };
        let plan = PlaybackPlan::Converted {
            delivery: Delivery::Hls,
            container: Container::Mp4,
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
        let selection = Selection {
            audio: None,
            subtitle: None,
        };
        let view = plan_view(&plan, Some(&film), selection, &detected, false);
        let encoders: Vec<_> = view
            .streams
            .iter()
            .filter_map(|s| match &s.out {
                api::StreamOut::Encode { encoder, .. } => encoder.clone(),
                _ => None,
            })
            .collect();
        assert_eq!(encoders, ["h264_vaapi"]);
    }

    #[test]
    fn every_profile_has_a_distinct_key() {
        let mut keys: Vec<&str> = PROFILES.iter().map(|p| p.key).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), PROFILES.len());
    }
}
