//! The playback planner (ARCHITECTURE §6, §8): from what a file is, what
//! the client can decode and take in, and which tracks it picked, how it
//! will play. Cheapest first:
//!
//! 1. `Direct`: the container, the picked streams and the file's bitrate
//!    fit, and the client takes range reads.
//! 2. `Converted` (P2-01): copy every stream that fits, encode only the
//!    ones that do not. A subtitle the client cannot show and that cannot
//!    become text is burned in, which forces a video encode.
//! 3. `Unsupported`: nothing above works, e.g. a Phase 1 client (range
//!    reads only) with a file it cannot play.
//!
//! The picked subtitle is decided first, since it decides whether the file
//! can play directly (P2-07). A bitmap one (PGS; VobSub and DVB since
//! P3-08): the client draws it, the server reads it as text (sent as an
//! external text subtitle, so the file still plays directly), or it is
//! burned in. The client's [`BitmapSubtitles`] mode picks; for `Auto` the
//! viewer's [`BitmapPreference`] does: text when the server can read the
//! track, else the client draws it, else it is burned in; or, for
//! pictures, the client draws it, else it is burned in. A text one the client cannot show is converted to
//! SubRip or WebVTT and sent beside the file the same way.

use spoolframe_media::encoders::Encoders;
use spoolframe_media::ocr::Ocr;
use spoolframe_media::{
    MediaDescriptor, SubtitleKind, SubtitleSource, SubtitleStream, VideoStream, codecs,
};
use spoolframe_protocol::{
    AudioAction, BitmapSubtitles, ConvertedAudio, ConvertedSubtitle, ConvertedVideo, PlannedAudio,
    PlannedSubtitle, PlaybackPlan, Selection, SubtitleAction, SubtitleDelivery,
};
use spoolframe_types::{
    AudioCodec, BitmapPreference, ClientCapabilities, Container, Delivery, SubtitleFormat,
    VideoCodec,
};

/// How a picked bitmap subtitle is shown: as the client asked, or, when it
/// left it to the server (`BitmapSubtitles::Auto`), as the viewer prefers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bitmap {
    /// Read as text, or the plan is unsupported.
    Text,
    /// Burned into the video.
    BurnIn,
    /// The viewer's preference, falling back to pictures.
    Preferred(BitmapPreference),
}

impl Bitmap {
    /// What the client asked for, or `None` when it left it to the
    /// viewer's preference.
    pub fn asked(requested: BitmapSubtitles) -> Option<Self> {
        match requested {
            BitmapSubtitles::Text => Some(Bitmap::Text),
            BitmapSubtitles::BurnIn => Some(Bitmap::BurnIn),
            BitmapSubtitles::Auto | BitmapSubtitles::Unknown => None,
        }
    }
}

/// Plan `descriptor` for a client with `capabilities` (`None`: it sent
/// none) and `selection`, showing a bitmap subtitle as `bitmap` says,
/// encoding with `encoders` and reading pictures as text with `ocr` if it
/// must. Only the container, the video and the selected tracks count.
pub fn plan(
    descriptor: &MediaDescriptor,
    capabilities: Option<&ClientCapabilities>,
    selection: Selection,
    bitmap: Bitmap,
    encoders: &Encoders,
    ocr: &Ocr,
) -> PlaybackPlan {
    let Some(caps) = capabilities else {
        return plan_without_capabilities();
    };

    // Tracks that do not exist: no plan fixes that.
    let mut missing = Vec::new();
    let audio = match audio_index(descriptor, selection.audio) {
        Ok(index) => index,
        Err(reason) => {
            missing.push(reason);
            None
        }
    };
    let subtitle = selection.subtitle.filter(|&picked| {
        let exists = (picked as usize) < descriptor.subtitles.len();
        if !exists {
            missing.push(format!("subtitle {picked}: no such track"));
        }
        exists
    });
    if !missing.is_empty() {
        return PlaybackPlan::Unsupported { reasons: missing };
    }
    let shown = match subtitle
        .map(|picked| {
            subtitle_shown(
                &descriptor.subtitles[picked as usize],
                picked,
                caps,
                bitmap,
                ocr,
            )
        })
        .transpose()
    {
        Ok(shown) => shown.flatten(),
        Err(reason) => {
            return PlaybackPlan::Unsupported {
                reasons: vec![reason],
            };
        }
    };

    let fit = Fit::check(descriptor, caps, audio, subtitle, shown);
    let deliveries = caps.deliveries();
    if fit.reasons().is_empty() && deliveries.contains(&Delivery::Range) {
        return direct(descriptor, audio, subtitle, shown);
    }
    // The first way in the client takes that is not the file itself.
    let Some(delivery) = deliveries
        .iter()
        .copied()
        .find(|d| matches!(d, Delivery::Hls | Delivery::Progressive))
    else {
        return PlaybackPlan::Unsupported {
            reasons: fit.reasons(),
        };
    };
    match converted(descriptor, caps, encoders, delivery, audio, subtitle, &fit) {
        Ok(plan) => plan,
        Err(reason) => {
            let mut reasons = fit.reasons();
            reasons.push(reason);
            PlaybackPlan::Unsupported { reasons }
        }
    }
}

/// The plan for a client that sent no capabilities: nothing can be
/// promised to play.
pub fn plan_without_capabilities() -> PlaybackPlan {
    PlaybackPlan::Unsupported {
        reasons: vec!["the client sent no capabilities".into()],
    }
}

/// How the picked subtitle is shown, when the server has a say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shown {
    /// The client draws the pictures.
    Native,
    /// The server makes text of it (reads the pictures, or converts the
    /// text), sent beside the file as `to`.
    Text { to: SubtitleFormat },
    /// Into the video; `asked` if the client asked for it rather than it
    /// being the only way.
    BurnIn { asked: bool },
}

/// How subtitle `stream` is shown: a bitmap one as `mode` says; a text one
/// the client cannot show as text it can (`None` when it shows it as it
/// is, or cannot take any text). An error is a `Text` the server cannot
/// do: the plan is unsupported, rather than quietly something else.
fn subtitle_shown(
    stream: &SubtitleStream,
    picked: u32,
    caps: &ClientCapabilities,
    mode: Bitmap,
    ocr: &Ocr,
) -> Result<Option<Shown>, String> {
    if stream.kind == SubtitleKind::Text {
        return Ok(converted_text(stream, caps));
    }
    let format = codecs::subtitle_format(&stream.codec);
    let native = format != SubtitleFormat::Unknown && caps.subtitles.contains(&format);
    let text = as_text(stream, format, caps, ocr);
    let pictures = if native {
        Shown::Native
    } else {
        Shown::BurnIn { asked: false }
    };
    Ok(Some(match mode {
        Bitmap::Text => match text {
            Ok(to) => Shown::Text { to },
            Err(why) => {
                return Err(format!(
                    "subtitle {picked}: {} cannot be read as text: {why}",
                    name(format.as_str(), &stream.codec)
                ));
            }
        },
        Bitmap::BurnIn => Shown::BurnIn { asked: true },
        Bitmap::Preferred(BitmapPreference::Text) => match text {
            Ok(to) => Shown::Text { to },
            Err(_) => pictures,
        },
        Bitmap::Preferred(BitmapPreference::Pictures) => pictures,
    }))
}

/// A text subtitle the client cannot show, converted to SubRip or WebVTT
/// and sent beside the file (P2-07), so the file can still play directly.
fn converted_text(stream: &SubtitleStream, caps: &ClientCapabilities) -> Option<Shown> {
    let format = codecs::subtitle_format(&stream.codec);
    // Shown as it is, or a format ffmpeg's name did not map to.
    if format == SubtitleFormat::Unknown || caps.subtitles.contains(&format) {
        return None;
    }
    [SubtitleFormat::Srt, SubtitleFormat::WebVtt]
        .into_iter()
        .find(|f| caps.subtitles.contains(f))
        .map(|to| Shown::Text { to })
}

/// The text format the server would send `stream` as, read with `ocr`;
/// or why it cannot.
fn as_text(
    stream: &SubtitleStream,
    format: SubtitleFormat,
    caps: &ClientCapabilities,
    ocr: &Ocr,
) -> Result<SubtitleFormat, String> {
    if !matches!(
        format,
        SubtitleFormat::Pgs | SubtitleFormat::VobSub | SubtitleFormat::DvbSub
    ) {
        return Err("only PGS, VobSub and DVB are read".into());
    }
    if ocr.languages().is_empty() {
        return Err("the server has no text recognition (tesseract)".into());
    }
    if ocr.language_for(stream.language.as_deref()).is_none() {
        return Err(format!(
            "the server has no text recognition data for {}",
            stream.language.as_deref().unwrap_or("English")
        ));
    }
    [SubtitleFormat::Srt, SubtitleFormat::WebVtt]
        .into_iter()
        .find(|f| caps.subtitles.contains(f))
        .ok_or_else(|| "the client shows neither SubRip nor WebVTT".into())
}

/// What does not fit the client as the file is, per part, each with the
/// reason. Empty everywhere: it plays directly.
#[derive(Debug, Default)]
struct Fit {
    container: Vec<String>,
    video: Vec<String>,
    audio: Vec<String>,
    subtitle: Vec<String>,
    /// How the picked subtitle is shown.
    shown: Option<Shown>,
}

impl Fit {
    fn check(
        descriptor: &MediaDescriptor,
        caps: &ClientCapabilities,
        audio: Option<usize>,
        subtitle: Option<u32>,
        shown: Option<Shown>,
    ) -> Self {
        let mut fit = Fit {
            shown,
            ..Fit::default()
        };
        let container = codecs::container(&descriptor.container);
        if !supported(&caps.containers, container, Container::Unknown) {
            fit.container.push(format!(
                "container: {} not supported",
                name(container.as_str(), &descriptor.container)
            ));
        }
        if let Some(v) = &descriptor.video {
            let codec = codecs::video_codec(&v.codec);
            if !supported(&caps.video, codec, VideoCodec::Unknown) {
                fit.video.push(format!(
                    "video: {} not supported",
                    name(codec.as_str(), &v.codec)
                ));
            }
            fit.video.extend(over_limits(v, caps));
            if let (Some(max), Some(rate)) = (caps.max_bitrate, descriptor.bit_rate)
                && rate > max
            {
                fit.video.push(format!(
                    "bitrate: {} over the client's {}",
                    mbits(rate),
                    mbits(max)
                ));
            }
        }
        if let Some(index) = audio {
            let stream = &descriptor.audio[index];
            let codec = codecs::audio_codec(&stream.codec);
            if !supported(&caps.audio, codec, AudioCodec::Unknown) {
                fit.audio.push(format!(
                    "audio {index}: {} not supported",
                    name(codec.as_str(), &stream.codec)
                ));
            }
            if let (Some(max), Some(channels)) = (caps.max_audio_channels, stream.channels)
                && channels > max
            {
                fit.audio.push(format!(
                    "audio {index}: {channels} channels, the client takes {max}"
                ));
            }
        }
        if let Some(picked) = subtitle {
            let stream = &descriptor.subtitles[picked as usize];
            let format = codecs::subtitle_format(&stream.codec);
            if shown == Some(Shown::BurnIn { asked: true }) {
                fit.subtitle.push(format!(
                    "subtitle {picked}: {} burned into the video, as asked",
                    name(format.as_str(), &stream.codec)
                ));
            } else if matches!(shown, Some(Shown::Text { .. })) {
                // Read as text and sent beside the file: it fits.
            } else if !supported(&caps.subtitles, format, SubtitleFormat::Unknown) {
                fit.subtitle.push(format!(
                    "subtitle {picked}: {} not supported",
                    name(format.as_str(), &stream.codec)
                ));
            }
        }
        fit
    }

    /// In the order a person reads a file: container, video, audio,
    /// subtitle.
    fn reasons(&self) -> Vec<String> {
        [&self.container, &self.video, &self.audio, &self.subtitle]
            .into_iter()
            .flatten()
            .cloned()
            .collect()
    }
}

/// Where `video` goes past the client's `max_video`.
fn over_limits(video: &VideoStream, caps: &ClientCapabilities) -> Vec<String> {
    let Some(limits) = &caps.max_video else {
        return Vec::new();
    };
    let mut over = Vec::new();
    if let Some(max) = limits.height
        && video.height > max
    {
        over.push(format!("video: {}p, the client shows {max}p", video.height));
    }
    if let (Some(max), Some(rate)) = (limits.frame_rate, video.frame_rate_millis)
        && rate > max.saturating_mul(1000)
    {
        over.push(format!(
            "video: {:.3} fps, the client shows {max}",
            f64::from(rate) / 1000.0
        ));
    }
    if let Some(max) = limits.bit_depth
        && video.bit_depth > max
    {
        over.push(format!(
            "video: {}-bit, the client shows {max}-bit",
            video.bit_depth
        ));
    }
    if let (Some(shown), Some(hdr)) = (&limits.hdr, video.hdr)
        && !shown.contains(&hdr)
    {
        over.push(format!("video: {hdr} is not shown as HDR; tone-mapped"));
    }
    over
}

fn mbits(bits: u64) -> String {
    format!("{:.1} Mbit/s", bits as f64 / 1e6)
}

fn direct(
    descriptor: &MediaDescriptor,
    audio: Option<usize>,
    subtitle: Option<u32>,
    shown: Option<Shown>,
) -> PlaybackPlan {
    PlaybackPlan::Direct {
        container: codecs::container(&descriptor.container),
        video: descriptor
            .video
            .as_ref()
            .map(|v| codecs::video_codec(&v.codec)),
        audio: audio.map(|index| PlannedAudio {
            index: index as u32,
            codec: codecs::audio_codec(&descriptor.audio[index].codec),
        }),
        subtitle: subtitle.map(|picked| {
            let stream = &descriptor.subtitles[picked as usize];
            if let Some(Shown::Text { to }) = shown {
                return PlannedSubtitle {
                    index: picked,
                    format: to,
                    delivery: SubtitleDelivery::External,
                };
            }
            PlannedSubtitle {
                index: picked,
                format: codecs::subtitle_format(&stream.codec),
                delivery: match stream.source {
                    SubtitleSource::Embedded { .. } => SubtitleDelivery::Embedded,
                    SubtitleSource::External { .. } => SubtitleDelivery::External,
                },
            }
        }),
    }
}

/// The output container for `delivery`: fragmented MP4 for HLS where the
/// client plays MP4 (it carries HEVC, which TS segments do not for every
/// player), else TS; for a progressive stream Matroska, which carries
/// anything, then MP4, then TS.
fn output_container(caps: &ClientCapabilities, delivery: Delivery) -> Option<Container> {
    let order: &[Container] = match delivery {
        Delivery::Hls => &[Container::Mp4, Container::MpegTs],
        _ => &[Container::Matroska, Container::Mp4, Container::MpegTs],
    };
    order.iter().copied().find(|c| caps.containers.contains(c))
}

/// Text formats `container` can carry, preferred first.
fn text_formats(container: Container) -> &'static [SubtitleFormat] {
    match container {
        Container::Matroska => &[
            SubtitleFormat::Ass,
            SubtitleFormat::Srt,
            SubtitleFormat::WebVtt,
        ],
        Container::Mp4 => &[SubtitleFormat::WebVtt, SubtitleFormat::MovText],
        _ => &[SubtitleFormat::WebVtt],
    }
}

fn converted(
    descriptor: &MediaDescriptor,
    caps: &ClientCapabilities,
    encoders: &Encoders,
    delivery: Delivery,
    audio: Option<usize>,
    subtitle: Option<u32>,
    fit: &Fit,
) -> Result<PlaybackPlan, String> {
    let treatment = fit.shown;
    let container = output_container(caps, delivery)
        .ok_or_else(|| format!("delivery: no container this client plays can carry {delivery}"))?;
    let mut reasons = fit.reasons();

    // Subtitles first: burning one in decides the video.
    let subtitle = subtitle.map(|picked| {
        let stream = &descriptor.subtitles[picked as usize];
        let format = codecs::subtitle_format(&stream.codec);
        let shown = caps.subtitles.contains(&format) && format != SubtitleFormat::Unknown;
        let carried = match stream.kind {
            SubtitleKind::Bitmap => container == Container::Matroska,
            SubtitleKind::Text => text_formats(container).contains(&format),
        };
        let action = if let (Some(Shown::Text { to }), SubtitleKind::Bitmap) =
            (treatment, stream.kind)
        {
            // HLS sends text as a WebVTT rendition (P2-07).
            let to =
                if delivery == Delivery::Hls && caps.subtitles.contains(&SubtitleFormat::WebVtt) {
                    SubtitleFormat::WebVtt
                } else {
                    to
                };
            SubtitleAction::Ocr { from: format, to }
        } else if treatment == Some(Shown::BurnIn { asked: true }) {
            // The fit's reasons already say so.
            SubtitleAction::BurnIn { format }
        } else if shown && carried {
            SubtitleAction::Copy { format }
        } else if let (SubtitleKind::Text, Some(to)) = (
            stream.kind,
            text_formats(container)
                .iter()
                .copied()
                .find(|f| caps.subtitles.contains(f)),
        ) {
            SubtitleAction::Convert { from: format, to }
        } else {
            reasons.push(format!(
                "subtitle {picked}: {} cannot be shown; burned into the video",
                name(format.as_str(), &stream.codec)
            ));
            SubtitleAction::BurnIn { format }
        };
        ConvertedSubtitle {
            index: picked,
            action,
        }
    });
    let burn_in = matches!(
        subtitle,
        Some(ConvertedSubtitle {
            action: SubtitleAction::BurnIn { .. },
            ..
        })
    );

    let audio = match audio {
        None => None,
        Some(index) if fit.audio.is_empty() => Some(ConvertedAudio {
            index: index as u32,
            action: AudioAction::Copy {
                codec: codecs::audio_codec(&descriptor.audio[index].codec),
            },
        }),
        Some(index) => Some(ConvertedAudio {
            index: index as u32,
            action: encode_audio(descriptor, caps, encoders, index)?,
        }),
    };

    let video = match &descriptor.video {
        None => None,
        Some(v) if fit.video.is_empty() && !burn_in => Some(ConvertedVideo::Copy {
            codec: codecs::video_codec(&v.codec),
        }),
        Some(v) => Some(encode_video(v, caps, encoders, audio.as_ref())?),
    };

    if reasons.is_empty() {
        reasons.push(format!(
            "delivery: the client takes {delivery}, not the file"
        ));
    }
    Ok(PlaybackPlan::Converted {
        delivery,
        container,
        video,
        audio,
        subtitle,
        reasons,
    })
}

/// The first codec, in the server's order of preference (hardware first),
/// that the client plays.
fn encode_video(
    video: &VideoStream,
    caps: &ClientCapabilities,
    encoders: &Encoders,
    audio: Option<&ConvertedAudio>,
) -> Result<ConvertedVideo, String> {
    let codec = encoders
        .video
        .iter()
        .map(|e| e.codec)
        .find(|c| caps.video.contains(c))
        .ok_or("video: the server can encode no codec this client plays")?;
    let limits = caps.max_video.clone().unwrap_or_default();
    let height = limits
        .height
        .map_or(video.height, |max| video.height.min(max));
    let hdr_shown = match (&limits.hdr, video.hdr) {
        (_, None) => true,
        (None, Some(_)) => true,
        (Some(shown), Some(hdr)) => shown.contains(&hdr),
    };
    // 8-bit output where the client asks for it; H.264 carries no HDR.
    let eight_bit = limits.bit_depth.is_some_and(|b| b < 10);
    let tone_map = video.hdr.is_some() && (!hdr_shown || eight_bit || codec == VideoCodec::H264);
    // Leave room for the audio inside the client's limit.
    let audio_bits = match audio.map(|a| a.action) {
        Some(AudioAction::Encode { bitrate, .. }) => bitrate,
        Some(_) => 640_000,
        None => 0,
    };
    let max_bitrate = caps
        .max_bitrate
        .map(|max| max.saturating_sub(audio_bits).max(500_000));
    Ok(ConvertedVideo::Encode {
        codec,
        height,
        max_bitrate,
        tone_map,
    })
}

/// Surround to the first surround codec the client plays and the server
/// encodes, else stereo; each at a bitrate fit for its channels.
fn encode_audio(
    descriptor: &MediaDescriptor,
    caps: &ClientCapabilities,
    encoders: &Encoders,
    index: usize,
) -> Result<AudioAction, String> {
    let source = descriptor.audio[index].channels.unwrap_or(2);
    let channels = caps
        .max_audio_channels
        .map_or(source, |max| source.min(max))
        .max(1);
    let order: &[AudioCodec] = if channels > 2 {
        &[
            AudioCodec::Eac3,
            AudioCodec::Ac3,
            AudioCodec::Aac,
            AudioCodec::Opus,
        ]
    } else {
        &[
            AudioCodec::Aac,
            AudioCodec::Opus,
            AudioCodec::Eac3,
            AudioCodec::Ac3,
        ]
    };
    let codec = order
        .iter()
        .copied()
        .find(|c| caps.audio.contains(c) && encoders.audio_encoder(*c).is_some())
        .ok_or_else(|| {
            format!("audio {index}: the server can encode no codec this client plays")
        })?;
    // AC-3 tops out at 5.1; AAC and Opus beyond stereo stay at 5.1 too.
    let channels = channels.min(6);
    let bitrate = match (codec, channels > 2) {
        (AudioCodec::Eac3 | AudioCodec::Ac3, true) => 640_000,
        (AudioCodec::Aac, true) => 384_000,
        (AudioCodec::Opus, true) => 256_000,
        (AudioCodec::Opus, false) => 128_000,
        _ => 192_000,
    };
    Ok(AudioAction::Encode {
        codec,
        channels,
        bitrate,
    })
}

/// Unknown is never supported, whatever the client claims.
/// A plan in the words of one log line: what the server decided, each
/// part short enough to read in a row. `none` is no such stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanSummary {
    /// `direct`, `converted` or `unsupported`.
    pub kind: &'static str,
    pub delivery: Delivery,
    /// `direct:hevc`, `copy:hevc`, `encode:h264:1080p` (`:tonemap` after
    /// it when HDR is mapped to SDR).
    pub video: String,
    /// `direct:eac3`, `copy:eac3`, `encode:eac3:6ch`.
    pub audio: String,
    /// `embedded:srt` or `external:srt` beside a file played as it is;
    /// `copy:srt`, `convert:srt>web_vtt`, `burn_in:pgs`, `ocr:pgs>srt`.
    pub subtitle: String,
}

pub fn summary(plan: &PlaybackPlan) -> PlanSummary {
    let none = || "none".to_owned();
    match plan {
        PlaybackPlan::Direct {
            video,
            audio,
            subtitle,
            ..
        } => PlanSummary {
            kind: "direct",
            delivery: Delivery::Range,
            video: video.map_or_else(none, |codec| format!("direct:{codec}")),
            audio: audio.map_or_else(none, |a| format!("direct:{}", a.codec)),
            subtitle: subtitle.map_or_else(none, |s| {
                let from = match s.delivery {
                    SubtitleDelivery::Embedded => "embedded",
                    SubtitleDelivery::External => "external",
                    SubtitleDelivery::Unknown => "unknown",
                };
                format!("{from}:{}", s.format)
            }),
        },
        PlaybackPlan::Converted {
            delivery,
            video,
            audio,
            subtitle,
            ..
        } => PlanSummary {
            kind: "converted",
            delivery: *delivery,
            video: video.map_or_else(none, |video| match video {
                ConvertedVideo::Copy { codec } => format!("copy:{codec}"),
                ConvertedVideo::Encode {
                    codec,
                    height,
                    tone_map,
                    ..
                } => format!(
                    "encode:{codec}:{height}p{}",
                    if tone_map { ":tonemap" } else { "" }
                ),
                ConvertedVideo::Unknown => "unknown".to_owned(),
            }),
            audio: audio.map_or_else(none, |audio| match audio.action {
                AudioAction::Copy { codec } => format!("copy:{codec}"),
                AudioAction::Encode {
                    codec, channels, ..
                } => format!("encode:{codec}:{channels}ch"),
                AudioAction::Unknown => "unknown".to_owned(),
            }),
            subtitle: subtitle.map_or_else(none, |subtitle| match subtitle.action {
                SubtitleAction::Copy { format } => format!("copy:{format}"),
                SubtitleAction::Convert { from, to } => format!("convert:{from}>{to}"),
                SubtitleAction::BurnIn { format } => format!("burn_in:{format}"),
                SubtitleAction::Ocr { from, to } => format!("ocr:{from}>{to}"),
                SubtitleAction::Unknown => "unknown".to_owned(),
            }),
        },
        PlaybackPlan::Unsupported { .. } | PlaybackPlan::Unknown => PlanSummary {
            kind: if matches!(plan, PlaybackPlan::Unknown) {
                "unknown"
            } else {
                "unsupported"
            },
            delivery: Delivery::Unknown,
            video: none(),
            audio: none(),
            subtitle: none(),
        },
    }
}

fn supported<T: Ord>(set: &std::collections::BTreeSet<T>, value: T, unknown: T) -> bool {
    value != unknown && set.contains(&value)
}

/// Our name for a format, or ffprobe's when we have none.
fn name<'a>(ours: &'a str, ffprobe: &'a str) -> &'a str {
    if ours == "unknown" { ffprobe } else { ours }
}

/// The audio track to plan: the one picked, or the file's default (the
/// first marked default, else the first), or none if there is no audio.
fn audio_index(descriptor: &MediaDescriptor, picked: Option<u32>) -> Result<Option<usize>, String> {
    match picked {
        Some(picked) if (picked as usize) < descriptor.audio.len() => Ok(Some(picked as usize)),
        Some(picked) => Err(format!("audio {picked}: no such track")),
        None if descriptor.audio.is_empty() => Ok(None),
        None => Ok(Some(
            descriptor.audio.iter().position(|a| a.default).unwrap_or(0),
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use spoolframe_media::{AudioStream, SubtitleKind, SubtitleStream, VideoStream};

    use std::collections::BTreeSet;

    use spoolframe_media::encoders::{AudioEncoder, Hardware, VideoEncoder};
    use spoolframe_media::sandbox::Tool;
    use spoolframe_types::{AudioRole, HdrFormat, SubtitleRole, VideoLimits};

    use super::*;

    /// The Phase 1 call: nothing detected, the software baseline.
    fn plan(d: &MediaDescriptor, caps: Option<&ClientCapabilities>, s: Selection) -> PlaybackPlan {
        plan_with(d, caps, s, &Encoders::software_baseline())
    }

    /// The default preference for bitmap subtitles, and no text
    /// recognition.
    fn plan_with(
        d: &MediaDescriptor,
        caps: Option<&ClientCapabilities>,
        s: Selection,
        encoders: &Encoders,
    ) -> PlaybackPlan {
        super::plan(
            d,
            caps,
            s,
            Bitmap::Preferred(BitmapPreference::Text),
            encoders,
            &Ocr::disabled(),
        )
    }

    /// What the dev machine detected (P2-02).
    fn dev_encoders() -> Encoders {
        let video = |codec, name: &str, hardware| VideoEncoder {
            codec,
            name: name.into(),
            hardware,
        };
        let audio = |codec, name: &str| AudioEncoder {
            codec,
            name: name.into(),
        };
        Encoders {
            video: vec![
                video(VideoCodec::H264, "h264_vaapi", Some(Hardware::Vaapi)),
                video(VideoCodec::H264, "libx264", None),
                video(VideoCodec::Hevc, "hevc_vaapi", Some(Hardware::Vaapi)),
                video(VideoCodec::Hevc, "libx265", None),
            ],
            audio: vec![
                audio(AudioCodec::Aac, "aac"),
                audio(AudioCodec::Eac3, "eac3"),
                audio(AudioCodec::Ac3, "ac3"),
            ],
            hardware_decoders: Vec::new(),
            unusable: Vec::new(),
        }
    }

    fn audio(index: u32, codec: &str, default: bool) -> AudioStream {
        AudioStream {
            index,
            codec: codec.into(),
            profile: None,
            channels: Some(6),
            channel_layout: None,
            language: Some("eng".into()),
            title: None,
            default,
            role: AudioRole::Main,
        }
    }

    fn subtitle(source: SubtitleSource, codec: &str) -> SubtitleStream {
        SubtitleStream {
            source,
            kind: SubtitleKind::of_codec(codec),
            codec: codec.into(),
            language: Some("eng".into()),
            title: None,
            forced: false,
            default: false,
            role: SubtitleRole::Full,
        }
    }

    /// The ticket's file: a DTS default track, an AAC commentary, PGS and
    /// an external SRT.
    fn dts_aac_pgs() -> MediaDescriptor {
        MediaDescriptor {
            container: "matroska,webm".into(),
            duration: Some(Duration::from_secs(7000)),
            bit_rate: None,
            video: Some(VideoStream {
                index: 0,
                codec: "h264".into(),
                profile: None,
                level: None,
                width: 1920,
                height: 1080,
                pixel_format: None,
                color_transfer: None,
                frame_rate_millis: Some(23_976),
                bit_depth: 8,
                hdr: None,
            }),
            audio: vec![audio(1, "dts", true), audio(2, "aac", false)],
            subtitles: vec![
                subtitle(SubtitleSource::Embedded { index: 3 }, "hdmv_pgs_subtitle"),
                subtitle(
                    SubtitleSource::External {
                        path: PathBuf::from("/media/Movie.en.srt"),
                    },
                    "subrip",
                ),
            ],
        }
    }

    /// A client without DTS or PGS.
    fn caps() -> ClientCapabilities {
        ClientCapabilities {
            video: [VideoCodec::H264].into(),
            audio: [AudioCodec::Aac, AudioCodec::Ac3].into(),
            containers: [Container::Matroska, Container::Mp4].into(),
            subtitles: [SubtitleFormat::Srt].into(),
            ..ClientCapabilities::default()
        }
    }

    fn reasons(plan: PlaybackPlan) -> Vec<String> {
        match plan {
            PlaybackPlan::Unsupported { reasons } => reasons,
            other => panic!("not unsupported: {other:?}"),
        }
    }

    #[test]
    fn the_playable_tracks_play_directly() {
        let plan = plan(
            &dts_aac_pgs(),
            Some(&caps()),
            Selection {
                audio: Some(1),
                subtitle: None,
            },
        );
        assert_eq!(
            plan,
            PlaybackPlan::Direct {
                container: Container::Matroska,
                video: Some(VideoCodec::H264),
                audio: Some(PlannedAudio {
                    index: 1,
                    codec: AudioCodec::Aac
                }),
                subtitle: None,
            }
        );
    }

    #[test]
    fn the_default_track_is_planned_when_none_is_picked() {
        // The default is DTS, which this client cannot play.
        assert_eq!(
            reasons(plan(&dts_aac_pgs(), Some(&caps()), Selection::default())),
            ["audio 0: dts not supported"]
        );
    }

    #[test]
    fn a_picked_subtitle_the_client_cannot_show_is_named() {
        let selection = Selection {
            audio: Some(1),
            subtitle: Some(0),
        };
        assert_eq!(
            reasons(plan(&dts_aac_pgs(), Some(&caps()), selection)),
            ["subtitle 0: pgs not supported"]
        );
    }

    #[test]
    fn an_external_subtitle_is_planned_as_external() {
        let selection = Selection {
            audio: Some(1),
            subtitle: Some(1),
        };
        let PlaybackPlan::Direct { subtitle, .. } = plan(&dts_aac_pgs(), Some(&caps()), selection)
        else {
            panic!("not direct");
        };
        assert_eq!(
            subtitle,
            Some(PlannedSubtitle {
                index: 1,
                format: SubtitleFormat::Srt,
                delivery: SubtitleDelivery::External,
            })
        );
    }

    #[test]
    fn unpicked_unsupported_tracks_never_block() {
        // PGS and DTS are in the file but not picked.
        assert!(matches!(
            plan(
                &dts_aac_pgs(),
                Some(&caps()),
                Selection {
                    audio: Some(1),
                    subtitle: None
                }
            ),
            PlaybackPlan::Direct { .. }
        ));
    }

    #[test]
    fn no_capabilities_is_unsupported_with_that_reason() {
        assert_eq!(
            reasons(plan(&dts_aac_pgs(), None, Selection::default())),
            ["the client sent no capabilities"]
        );
    }

    #[test]
    fn a_track_that_does_not_exist_is_named() {
        let selection = Selection {
            audio: Some(9),
            subtitle: Some(7),
        };
        assert_eq!(
            reasons(plan(&dts_aac_pgs(), Some(&caps()), selection)),
            ["audio 9: no such track", "subtitle 7: no such track"]
        );
    }

    #[test]
    fn an_unknown_format_is_never_supported_and_named_by_ffprobe() {
        let mut file = dts_aac_pgs();
        file.video.as_mut().unwrap().codec = "prores".into();
        let mut caps = caps();
        caps.video.insert(VideoCodec::Unknown);
        assert_eq!(
            reasons(plan(
                &file,
                Some(&caps),
                Selection {
                    audio: Some(1),
                    subtitle: None
                }
            )),
            ["video: prores not supported"]
        );
    }

    #[test]
    fn several_problems_are_all_named() {
        let mut caps = caps();
        caps.containers.clear();
        let selection = Selection {
            audio: Some(0),
            subtitle: Some(0),
        };
        assert_eq!(
            reasons(plan(&dts_aac_pgs(), Some(&caps), selection)),
            [
                "container: matroska not supported",
                "audio 0: dts not supported",
                "subtitle 0: pgs not supported"
            ]
        );
    }

    /// An Apple TV-like client: HEVC and H.264, AAC/AC-3/E-AC-3, MP4 and
    /// TS, WebVTT, HLS only.
    fn avplayer() -> ClientCapabilities {
        ClientCapabilities {
            video: [VideoCodec::H264, VideoCodec::Hevc].into(),
            audio: [AudioCodec::Aac, AudioCodec::Ac3, AudioCodec::Eac3].into(),
            containers: [Container::Mp4, Container::MpegTs].into(),
            subtitles: [SubtitleFormat::WebVtt].into(),
            delivery: vec![Delivery::Hls],
            ..ClientCapabilities::default()
        }
    }

    /// A 4K HDR10 HEVC MKV with 5.1 DTS, 40 Mbit/s, PGS and SRT.
    fn hevc_dts() -> MediaDescriptor {
        let mut d = dts_aac_pgs();
        d.bit_rate = Some(40_000_000);
        let v = d.video.as_mut().unwrap();
        v.codec = "hevc".into();
        v.width = 3840;
        v.height = 2160;
        v.bit_depth = 10;
        v.hdr = Some(HdrFormat::Hdr10);
        d
    }

    fn converted(
        plan: PlaybackPlan,
    ) -> (
        Delivery,
        Container,
        ConvertedVideo,
        ConvertedAudio,
        Option<ConvertedSubtitle>,
        Vec<String>,
    ) {
        match plan {
            PlaybackPlan::Converted {
                delivery,
                container,
                video,
                audio,
                subtitle,
                reasons,
            } => (
                delivery,
                container,
                video.unwrap(),
                audio.unwrap(),
                subtitle,
                reasons,
            ),
            other => panic!("not converted: {other:?}"),
        }
    }

    fn picked(audio: u32, subtitle: Option<u32>) -> Selection {
        Selection {
            audio: Some(audio),
            subtitle,
        }
    }

    #[test]
    fn avplayer_gets_hls_with_the_video_copied_and_dts_encoded() {
        let (delivery, container, video, audio, subtitle, reasons) = converted(plan_with(
            &hevc_dts(),
            Some(&avplayer()),
            picked(0, None),
            &dev_encoders(),
        ));
        assert_eq!((delivery, container), (Delivery::Hls, Container::Mp4));
        assert_eq!(
            video,
            ConvertedVideo::Copy {
                codec: VideoCodec::Hevc
            }
        );
        assert_eq!(
            audio.action,
            AudioAction::Encode {
                codec: AudioCodec::Eac3,
                channels: 6,
                bitrate: 640_000,
            }
        );
        assert_eq!(subtitle, None);
        assert_eq!(
            reasons,
            [
                "container: matroska not supported",
                "audio 0: dts not supported"
            ]
        );
    }

    #[test]
    fn a_pgs_subtitle_avplayer_cannot_show_is_burned_in_and_says_so() {
        let (_, _, video, _, subtitle, reasons) = converted(plan_with(
            &hevc_dts(),
            Some(&avplayer()),
            picked(1, Some(0)),
            &dev_encoders(),
        ));
        assert!(matches!(video, ConvertedVideo::Encode { .. }), "{video:?}");
        assert_eq!(
            subtitle.unwrap().action,
            SubtitleAction::BurnIn {
                format: SubtitleFormat::Pgs
            }
        );
        assert!(
            reasons.iter().any(|r| r.contains("burned into the video")),
            "{reasons:?}"
        );
    }

    #[test]
    fn a_text_subtitle_is_converted_to_webvtt_for_hls() {
        let (_, _, video, _, subtitle, _) = converted(plan_with(
            &hevc_dts(),
            Some(&avplayer()),
            picked(1, Some(1)),
            &dev_encoders(),
        ));
        assert_eq!(
            video,
            ConvertedVideo::Copy {
                codec: VideoCodec::Hevc
            }
        );
        assert_eq!(
            subtitle.unwrap().action,
            SubtitleAction::Convert {
                from: SubtitleFormat::Srt,
                to: SubtitleFormat::WebVtt
            }
        );
    }

    #[test]
    fn a_bitrate_cap_under_the_file_s_encodes_the_video_under_it() {
        let mut caps = avplayer();
        caps.max_bitrate = Some(8_000_000);
        let (_, _, video, _, _, reasons) = converted(plan_with(
            &hevc_dts(),
            Some(&caps),
            picked(1, None),
            &dev_encoders(),
        ));
        let ConvertedVideo::Encode { max_bitrate, .. } = video else {
            panic!("not encoded: {video:?}");
        };
        assert!(max_bitrate.unwrap() <= 8_000_000);
        assert!(
            reasons.iter().any(|r| r.starts_with("bitrate:")),
            "{reasons:?}"
        );
    }

    #[test]
    fn a_1080p_sdr_client_gets_4k_hdr10_scaled_and_tone_mapped() {
        let mut caps = avplayer();
        caps.max_video = Some(VideoLimits {
            height: Some(1080),
            hdr: Some(BTreeSet::new()),
            ..VideoLimits::default()
        });
        let (_, _, video, _, _, _) = converted(plan_with(
            &hevc_dts(),
            Some(&caps),
            picked(1, None),
            &dev_encoders(),
        ));
        assert_eq!(
            video,
            ConvertedVideo::Encode {
                // The server's first choice the client plays: h264_vaapi.
                codec: VideoCodec::H264,
                height: 1080,
                max_bitrate: None,
                tone_map: true,
            }
        );
    }

    #[test]
    fn a_progressive_client_gets_matroska_with_everything_that_fits_copied() {
        let caps = ClientCapabilities {
            delivery: vec![Delivery::Range, Delivery::Progressive],
            ..caps()
        };
        // DTS does not fit; the H.264 video and the container do.
        let (delivery, container, video, audio, _, _) = converted(plan_with(
            &dts_aac_pgs(),
            Some(&caps),
            picked(0, None),
            &dev_encoders(),
        ));
        assert_eq!(
            (delivery, container),
            (Delivery::Progressive, Container::Matroska)
        );
        assert_eq!(
            video,
            ConvertedVideo::Copy {
                codec: VideoCodec::H264
            }
        );
        assert_eq!(
            audio.action,
            AudioAction::Encode {
                codec: AudioCodec::Ac3,
                channels: 6,
                bitrate: 640_000,
            }
        );
    }

    #[test]
    fn a_client_that_takes_range_reads_still_plays_directly_when_it_can() {
        let caps = ClientCapabilities {
            delivery: vec![Delivery::Range, Delivery::Hls],
            ..caps()
        };
        assert!(matches!(
            plan_with(
                &dts_aac_pgs(),
                Some(&caps),
                picked(1, None),
                &dev_encoders()
            ),
            PlaybackPlan::Direct { .. }
        ));
    }

    #[test]
    fn an_hls_only_client_with_a_file_that_fits_gets_a_remux() {
        let caps = ClientCapabilities {
            delivery: vec![Delivery::Hls],
            ..caps()
        };
        let (_, container, video, audio, _, reasons) = converted(plan_with(
            &dts_aac_pgs(),
            Some(&caps),
            picked(1, None),
            &dev_encoders(),
        ));
        assert_eq!(container, Container::Mp4);
        assert_eq!(
            video,
            ConvertedVideo::Copy {
                codec: VideoCodec::H264
            }
        );
        assert!(matches!(audio.action, AudioAction::Copy { .. }));
        assert_eq!(reasons, ["delivery: the client takes hls, not the file"]);
    }

    #[test]
    fn a_range_only_client_that_cannot_play_directly_is_unsupported_as_before() {
        assert_eq!(
            reasons(plan_with(
                &hevc_dts(),
                Some(&caps()),
                picked(0, None),
                &dev_encoders()
            )),
            ["video: hevc not supported", "audio 0: dts not supported"]
        );
    }

    #[test]
    fn no_encoder_for_any_codec_the_client_plays_is_unsupported() {
        let mut caps = avplayer();
        caps.video = [VideoCodec::Av1].into();
        let all = reasons(plan_with(
            &hevc_dts(),
            Some(&caps),
            picked(1, None),
            &dev_encoders(),
        ));
        assert_eq!(
            all.last().unwrap(),
            "video: the server can encode no codec this client plays"
        );
    }

    /// tesseract with English data.
    fn english() -> Ocr {
        Ocr::new(Tool::unconfined("tesseract"), ["eng".to_owned()])
    }

    fn with_mode(
        d: &MediaDescriptor,
        caps: &ClientCapabilities,
        s: Selection,
        mode: Bitmap,
        ocr: &Ocr,
    ) -> PlaybackPlan {
        super::plan(d, Some(caps), s, mode, &dev_encoders(), ocr)
    }

    fn sent_as_srt() -> Option<PlannedSubtitle> {
        Some(PlannedSubtitle {
            index: 0,
            format: SubtitleFormat::Srt,
            delivery: SubtitleDelivery::External,
        })
    }

    fn direct_subtitle(plan: PlaybackPlan) -> Option<PlannedSubtitle> {
        match plan {
            PlaybackPlan::Direct { subtitle, .. } => subtitle,
            other => panic!("not direct: {other:?}"),
        }
    }

    #[test]
    fn text_preferred_reads_a_pgs_the_client_cannot_draw_and_the_file_plays_directly() {
        // AAC audio fits; the PGS track goes out beside it as SubRip.
        let plan = with_mode(
            &dts_aac_pgs(),
            &caps(),
            picked(1, Some(0)),
            Bitmap::Preferred(BitmapPreference::Text),
            &english(),
        );
        assert_eq!(direct_subtitle(plan), sent_as_srt());
    }

    /// A client that takes SubRip and draws PGS.
    fn caps_that_draw_pgs() -> ClientCapabilities {
        ClientCapabilities {
            subtitles: [SubtitleFormat::Srt, SubtitleFormat::Pgs].into(),
            ..caps()
        }
    }

    fn drawn_by_the_client() -> Option<PlannedSubtitle> {
        Some(PlannedSubtitle {
            index: 0,
            format: SubtitleFormat::Pgs,
            delivery: SubtitleDelivery::Embedded,
        })
    }

    #[test]
    fn the_default_preference_reads_a_pgs_even_for_a_client_that_draws_it() {
        let plan = with_mode(
            &dts_aac_pgs(),
            &caps_that_draw_pgs(),
            picked(1, Some(0)),
            Bitmap::Preferred(BitmapPreference::default()),
            &english(),
        );
        assert_eq!(direct_subtitle(plan), sent_as_srt());
    }

    #[test]
    fn preferring_pictures_lets_a_client_that_draws_pgs_draw_it() {
        let plan = with_mode(
            &dts_aac_pgs(),
            &caps_that_draw_pgs(),
            picked(1, Some(0)),
            Bitmap::Preferred(BitmapPreference::Pictures),
            &english(),
        );
        assert_eq!(direct_subtitle(plan), drawn_by_the_client());
    }

    #[test]
    fn preferring_pictures_burns_them_in_for_a_client_that_cannot_draw_them() {
        let (_, _, video, _, subtitle, reasons) = converted(with_mode(
            &dts_aac_pgs(),
            &caps_that_can_burn_in(),
            picked(1, Some(0)),
            Bitmap::Preferred(BitmapPreference::Pictures),
            &english(),
        ));
        assert!(matches!(video, ConvertedVideo::Encode { .. }), "{video:?}");
        assert_eq!(
            subtitle,
            Some(ConvertedSubtitle {
                index: 0,
                action: SubtitleAction::BurnIn {
                    format: SubtitleFormat::Pgs
                },
            })
        );
        assert!(
            reasons.contains(&"subtitle 0: pgs cannot be shown; burned into the video".to_owned()),
            "{reasons:?}"
        );
    }

    #[test]
    fn text_preferred_without_recognition_is_drawn_by_a_client_that_can() {
        let plan = with_mode(
            &dts_aac_pgs(),
            &caps_that_draw_pgs(),
            picked(1, Some(0)),
            Bitmap::Preferred(BitmapPreference::Text),
            &Ocr::disabled(),
        );
        assert_eq!(direct_subtitle(plan), drawn_by_the_client());
    }

    #[test]
    fn a_mode_the_client_asks_for_is_what_the_request_resolves_to() {
        assert_eq!(Bitmap::asked(BitmapSubtitles::Text), Some(Bitmap::Text));
        assert_eq!(Bitmap::asked(BitmapSubtitles::BurnIn), Some(Bitmap::BurnIn));
        assert_eq!(Bitmap::asked(BitmapSubtitles::Auto), None);
        assert_eq!(Bitmap::asked(BitmapSubtitles::Unknown), None);
    }

    #[test]
    fn text_reads_it_even_for_a_client_that_draws_pgs() {
        let plan = with_mode(
            &dts_aac_pgs(),
            &caps_that_draw_pgs(),
            picked(1, Some(0)),
            Bitmap::Text,
            &english(),
        );
        assert_eq!(direct_subtitle(plan), sent_as_srt());
    }

    #[test]
    fn text_the_server_cannot_read_is_unsupported_and_says_why() {
        let mut french = dts_aac_pgs();
        french.subtitles[0].language = Some("fre".into());
        let text = |d: &MediaDescriptor, ocr: &Ocr| {
            reasons(with_mode(d, &caps(), picked(1, Some(0)), Bitmap::Text, ocr))
        };
        assert_eq!(
            text(&french, &english()),
            [
                "subtitle 0: pgs cannot be read as text: the server has no text recognition data for fre"
            ]
        );
        assert_eq!(
            text(&dts_aac_pgs(), &Ocr::disabled()),
            [
                "subtitle 0: pgs cannot be read as text: the server has no text recognition (tesseract)"
            ]
        );
        // Preferred as text, the same file is burned in where it can be,
        // or the format is named as before.
        assert_eq!(
            reasons(with_mode(
                &french,
                &caps(),
                picked(1, Some(0)),
                Bitmap::Preferred(BitmapPreference::Text),
                &english(),
            )),
            ["subtitle 0: pgs not supported"]
        );
    }

    #[test]
    fn burn_in_asked_for_encodes_the_video_even_for_a_client_that_draws_pgs() {
        let caps = ClientCapabilities {
            subtitles: [SubtitleFormat::Srt, SubtitleFormat::Pgs].into(),
            delivery: vec![Delivery::Range, Delivery::Progressive],
            ..caps()
        };
        let (_, _, video, _, subtitle, reasons) = converted(with_mode(
            &dts_aac_pgs(),
            &caps,
            picked(1, Some(0)),
            Bitmap::BurnIn,
            &english(),
        ));
        assert!(matches!(video, ConvertedVideo::Encode { .. }), "{video:?}");
        assert_eq!(
            subtitle,
            Some(ConvertedSubtitle {
                index: 0,
                action: SubtitleAction::BurnIn {
                    format: SubtitleFormat::Pgs
                },
            })
        );
        assert_eq!(reasons, ["subtitle 0: pgs burned into the video, as asked"]);
    }

    #[test]
    fn a_converted_plan_reads_the_pgs_and_keeps_the_video_copied() {
        // DTS needs encoding; the PGS no longer forces a video encode.
        let caps = ClientCapabilities {
            delivery: vec![Delivery::Range, Delivery::Progressive],
            ..caps()
        };
        let (_, _, video, _, subtitle, _) = converted(with_mode(
            &dts_aac_pgs(),
            &caps,
            picked(0, Some(0)),
            Bitmap::Preferred(BitmapPreference::Text),
            &english(),
        ));
        assert_eq!(
            video,
            ConvertedVideo::Copy {
                codec: VideoCodec::H264
            }
        );
        assert_eq!(
            subtitle,
            Some(ConvertedSubtitle {
                index: 0,
                action: SubtitleAction::Ocr {
                    from: SubtitleFormat::Pgs,
                    to: SubtitleFormat::Srt,
                },
            })
        );
    }

    /// The ticket's file with its PGS track as DVD (VobSub) or DVB
    /// subtitles, tagged `language`.
    fn with_bitmap(codec: &str, language: &str) -> MediaDescriptor {
        let mut file = dts_aac_pgs();
        file.subtitles[0] = subtitle(SubtitleSource::Embedded { index: 3 }, codec);
        file.subtitles[0].language = Some(language.into());
        file
    }

    /// A client without DTS, PGS, VobSub or DVB that also takes a
    /// progressive stream, so burning in is open to it.
    fn caps_that_can_burn_in() -> ClientCapabilities {
        ClientCapabilities {
            delivery: vec![Delivery::Range, Delivery::Progressive],
            ..caps()
        }
    }

    #[test]
    fn text_reads_vobsub_and_dvb_for_a_client_that_draws_neither() {
        for codec in ["dvd_subtitle", "dvb_subtitle"] {
            let plan = with_mode(
                &with_bitmap(codec, "eng"),
                &caps(),
                picked(1, Some(0)),
                Bitmap::Text,
                &english(),
            );
            assert_eq!(direct_subtitle(plan), sent_as_srt(), "{codec}");
        }
    }

    #[test]
    fn text_preferred_reads_vobsub_and_dvb_rather_than_burn_them_in() {
        for codec in ["dvd_subtitle", "dvb_subtitle"] {
            let plan = with_mode(
                &with_bitmap(codec, "eng"),
                &caps_that_can_burn_in(),
                picked(1, Some(0)),
                Bitmap::Preferred(BitmapPreference::Text),
                &english(),
            );
            assert_eq!(direct_subtitle(plan), sent_as_srt(), "{codec}");
        }
    }

    #[test]
    fn hls_reads_vobsub_and_dvb_as_a_webvtt_rendition() {
        let caps = ClientCapabilities {
            subtitles: [SubtitleFormat::WebVtt].into(),
            delivery: vec![Delivery::Hls],
            ..caps()
        };
        for (codec, from) in [
            ("dvd_subtitle", SubtitleFormat::VobSub),
            ("dvb_subtitle", SubtitleFormat::DvbSub),
        ] {
            let (_, _, video, _, subtitle, _) = converted(with_mode(
                &with_bitmap(codec, "eng"),
                &caps,
                picked(1, Some(0)),
                Bitmap::Preferred(BitmapPreference::Text),
                &english(),
            ));
            assert!(matches!(video, ConvertedVideo::Copy { .. }), "{video:?}");
            assert_eq!(
                subtitle,
                Some(ConvertedSubtitle {
                    index: 0,
                    action: SubtitleAction::Ocr {
                        from,
                        to: SubtitleFormat::WebVtt,
                    },
                }),
                "{codec}"
            );
        }
    }

    #[test]
    fn vobsub_and_dvb_without_recognition_data_burn_in_or_say_why_not() {
        for (codec, name) in [("dvd_subtitle", "vob_sub"), ("dvb_subtitle", "dvb_sub")] {
            let french = with_bitmap(codec, "fre");
            let (_, _, video, _, subtitle, why) = converted(with_mode(
                &french,
                &caps_that_can_burn_in(),
                picked(1, Some(0)),
                Bitmap::Preferred(BitmapPreference::Text),
                &english(),
            ));
            assert!(matches!(video, ConvertedVideo::Encode { .. }), "{video:?}");
            assert!(
                matches!(
                    subtitle,
                    Some(ConvertedSubtitle {
                        action: SubtitleAction::BurnIn { .. },
                        ..
                    })
                ),
                "{subtitle:?}"
            );
            assert!(
                why.contains(&format!(
                    "subtitle 0: {name} cannot be shown; burned into the video"
                )),
                "{why:?}"
            );
            assert_eq!(
                reasons(with_mode(
                    &french,
                    &caps_that_can_burn_in(),
                    picked(1, Some(0)),
                    Bitmap::Text,
                    &english(),
                )),
                [format!(
                    "subtitle 0: {name} cannot be read as text: \
                     the server has no text recognition data for fre"
                )]
            );
        }
    }

    #[test]
    fn the_mode_leaves_text_subtitles_alone() {
        let plan = with_mode(
            &dts_aac_pgs(),
            &caps(),
            picked(1, Some(1)),
            Bitmap::BurnIn,
            &english(),
        );
        assert_eq!(
            direct_subtitle(plan),
            Some(PlannedSubtitle {
                index: 1,
                format: SubtitleFormat::Srt,
                delivery: SubtitleDelivery::External,
            })
        );
    }

    #[test]
    fn a_text_subtitle_the_client_cannot_show_is_converted_beside_the_file() {
        let mut file = dts_aac_pgs();
        file.subtitles[1] = subtitle(SubtitleSource::Embedded { index: 4 }, "ass");
        // Takes SubRip, not ASS: the file still plays directly.
        let plan = with_mode(
            &file,
            &caps(),
            picked(1, Some(1)),
            Bitmap::Preferred(BitmapPreference::Text),
            &Ocr::disabled(),
        );
        assert_eq!(
            direct_subtitle(plan),
            Some(PlannedSubtitle {
                index: 1,
                format: SubtitleFormat::Srt,
                delivery: SubtitleDelivery::External,
            })
        );
    }

    #[test]
    fn a_plan_is_summed_up_for_the_log() {
        // The film that did not start on an iPhone: video copied, DTS to
        // E-AC-3, the SRT as a WebVTT rendition, over HLS.
        let plan = PlaybackPlan::Converted {
            delivery: Delivery::Hls,
            container: Container::Mp4,
            video: Some(ConvertedVideo::Copy {
                codec: VideoCodec::Hevc,
            }),
            audio: Some(ConvertedAudio {
                index: 0,
                action: AudioAction::Encode {
                    codec: AudioCodec::Eac3,
                    channels: 6,
                    bitrate: 640_000,
                },
            }),
            subtitle: Some(ConvertedSubtitle {
                index: 0,
                action: SubtitleAction::Convert {
                    from: SubtitleFormat::Srt,
                    to: SubtitleFormat::WebVtt,
                },
            }),
            reasons: Vec::new(),
        };
        assert_eq!(
            summary(&plan),
            PlanSummary {
                kind: "converted",
                delivery: Delivery::Hls,
                video: "copy:hevc".into(),
                audio: "encode:eac3:6ch".into(),
                subtitle: "convert:srt>web_vtt".into(),
            }
        );

        let burned = PlaybackPlan::Converted {
            delivery: Delivery::Progressive,
            container: Container::Matroska,
            video: Some(ConvertedVideo::Encode {
                codec: VideoCodec::H264,
                height: 1080,
                max_bitrate: None,
                tone_map: true,
            }),
            audio: Some(ConvertedAudio {
                index: 0,
                action: AudioAction::Copy {
                    codec: AudioCodec::Aac,
                },
            }),
            subtitle: Some(ConvertedSubtitle {
                index: 0,
                action: SubtitleAction::BurnIn {
                    format: SubtitleFormat::Pgs,
                },
            }),
            reasons: Vec::new(),
        };
        let burned = summary(&burned);
        assert_eq!(burned.video, "encode:h264:1080p:tonemap");
        assert_eq!(burned.audio, "copy:aac");
        assert_eq!(burned.subtitle, "burn_in:pgs");

        let direct = PlaybackPlan::Direct {
            container: Container::Matroska,
            video: Some(VideoCodec::H264),
            audio: None,
            subtitle: Some(PlannedSubtitle {
                index: 0,
                format: SubtitleFormat::Srt,
                delivery: SubtitleDelivery::External,
            }),
        };
        assert_eq!(
            summary(&direct),
            PlanSummary {
                kind: "direct",
                delivery: Delivery::Range,
                video: "direct:h264".into(),
                audio: "none".into(),
                subtitle: "external:srt".into(),
            }
        );
    }
}
