//! The playback planner (ARCHITECTURE §6, §8): from what a file is, what
//! the client can decode and take in, and which tracks it picked, how it
//! will play. Cheapest first:
//!
//! 1. `Direct`: the container, the picked streams and the file's bitrate
//!    fit, and the client takes range reads.
//! 2. `Converted` (P2-01): copy every stream that fits, encode only the
//!    ones that do not. Picture subtitles the client cannot draw are
//!    burned in, which forces a video encode.
//! 3. `Unsupported`: nothing above works, e.g. a Phase 1 client (range
//!    reads only) with a file it cannot play.
//!
//! The picked subtitle is decided first, since it decides whether the file
//! can play directly (P2-07). The rules:
//!
//! - A text subtitle is never burned in. It is copied if the client shows
//!   it and the output carries it, else converted to a text format the
//!   client shows (SubRip or WebVTT beside a file played directly; WebVTT
//!   for HLS). If the client shows no text format the output carries, it
//!   is not shown: the plan has no subtitle, and a reason says why. The
//!   video is never encoded for a text subtitle.
//! - A picture subtitle (PGS; VobSub and DVB since P3-08) is read as text
//!   when text is wanted and the server can read the track (sent as a
//!   text subtitle, so the file can still play directly). Text is wanted
//!   when the client's [`BitmapSubtitles`] mode is `Text`, or for `Auto`
//!   when the viewer's [`BitmapPreference`] is `Text`.
//! - Otherwise (text recognition is off or cannot read the track, or
//!   pictures are wanted), the client draws the pictures if it can in
//!   this delivery, and only if it cannot are they burned in.
//! - `BitmapSubtitles::BurnIn` asks for pictures, the same as the
//!   `Pictures` preference: it no longer forces a burn-in on a client
//!   that can draw them. The wire value stays (protocol 7).

use spoolframe_media::encoders::{Encoders, VideoEncoder};
use spoolframe_media::ocr::Ocr;
use spoolframe_media::{
    DvBase, MediaDescriptor, SubtitleKind, SubtitleSource, SubtitleStream, VideoStream, codecs,
};
use spoolframe_protocol::{
    AudioAction, BitmapSubtitles, ConvertedAudio, ConvertedSubtitle, ConvertedVideo, PlannedAudio,
    PlannedSubtitle, PlaybackPlan, Quality, Selection, SubtitleAction, SubtitleDelivery,
};
use spoolframe_types::{
    AudioCodec, BitmapPreference, ClientCapabilities, Container, Delivery, HdrFormat,
    SubtitleFormat, VideoCodec,
};

/// How a picked bitmap subtitle is shown: as the client asked, or, when it
/// left it to the server (`BitmapSubtitles::Auto`), as the viewer prefers.
/// There is no burn-in mode: pictures are burned in only when the client
/// cannot draw them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bitmap {
    /// Read as text, or the plan is unsupported.
    Text,
    /// The viewer's preference, falling back to pictures.
    Preferred(BitmapPreference),
}

impl Bitmap {
    /// What the client asked for, or `None` when it left it to the
    /// viewer's preference. `BitmapSubtitles::BurnIn` stays on the wire
    /// (protocol 7) but asks for pictures: the client draws them if it
    /// can, and they are burned in only if it cannot.
    pub fn asked(requested: BitmapSubtitles) -> Option<Self> {
        match requested {
            BitmapSubtitles::Text => Some(Bitmap::Text),
            BitmapSubtitles::BurnIn => Some(Bitmap::Preferred(BitmapPreference::Pictures)),
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
    plan_checked(
        descriptor,
        capabilities,
        selection,
        bitmap,
        encoders,
        ocr,
        false,
    )
}

/// [`plan`], with `copy_peak` asking a copied file to fit its peak, not
/// its average, under the bitrate cap ([`crate::encode::hls::copy_peak`]):
/// off the LAN in `Auto`, where the cap is the link's or the uplink's
/// share and a peak over it stalls the player (PX-46).
fn plan_checked(
    descriptor: &MediaDescriptor,
    capabilities: Option<&ClientCapabilities>,
    selection: Selection,
    bitmap: Bitmap,
    encoders: &Encoders,
    ocr: &Ocr,
    copy_peak: bool,
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

    let fit = Fit::check(descriptor, caps, audio, subtitle, shown, copy_peak);
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

pub use spoolframe_protocol::playback::LINK_SHARE;

/// `caps` for a playback of `quality` (P6-06): its `max_bitrate`, the
/// app's own limit, lowered to [`LINK_SHARE`] of the link for `Auto`, or
/// to the bitrate chosen, whichever is lower. `Auto` off the LAN is also
/// held to its `share` of the uplink (PX-46). `Original`, and `Auto` with
/// nothing measured and no share, keep the app's limit alone.
pub fn with_quality(
    caps: &ClientCapabilities,
    quality: Quality,
    share: Option<u64>,
) -> ClientCapabilities {
    let chosen = match quality {
        Quality::Auto { link_bitrate } => {
            let link = link_bitrate.map(|link| (link as f64 * LINK_SHARE) as u64);
            match (link, share) {
                (Some(link), Some(share)) => Some(link.min(share)),
                (link, share) => link.or(share),
            }
        }
        Quality::Bitrate { max_bitrate } => Some(max_bitrate),
        Quality::Original | Quality::Unknown => None,
    };
    let mut caps = caps.clone();
    if let Some(chosen) = chosen {
        caps.max_bitrate = Some(caps.max_bitrate.map_or(chosen, |own| own.min(chosen)));
    }
    caps
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
    /// Pictures the client cannot draw, burned into the video. Never a
    /// text subtitle.
    BurnIn,
}

/// How subtitle `stream` is shown: a bitmap one as `mode` says (as text
/// when that is wanted and the server can read it, else drawn by the
/// client if it can, else burned in); a text one the client cannot show
/// as text it can (`None` when it shows it as it is, or cannot take any
/// text). An error is a `Text` the server cannot do: the plan is
/// unsupported, rather than quietly something else.
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
    let pictures = if native { Shown::Native } else { Shown::BurnIn };
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
        copy_peak: bool,
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
            if let (Some(max), Some(rate)) = (caps.max_bitrate, descriptor.bit_rate) {
                if rate > max {
                    fit.video.push(format!(
                        "bitrate: {} over the client's {}",
                        mbits(rate),
                        mbits(max)
                    ));
                } else if copy_peak && crate::encode::hls::copy_peak(rate) > max {
                    fit.video.push(format!(
                        "bitrate: {} peaks near {}, over the {} this link takes",
                        mbits(rate),
                        mbits(crate::encode::hls::copy_peak(rate)),
                        mbits(max)
                    ));
                }
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
            if matches!(shown, Some(Shown::Text { .. })) {
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
    match (hdr_shown(video, caps), video.hdr) {
        (Showing::No, Some(HdrFormat::DolbyVision)) if video.needs_rpu() => over.push(
            "video: dolby_vision profile 5 is not shown, and has no base layer to show instead; \
             converted"
                .into(),
        ),
        (Showing::No, Some(hdr)) => {
            over.push(format!("video: {hdr} is not shown as HDR; tone-mapped"));
        }
        _ => {}
    }
    over
}

/// How a client shows `video`'s picture as it is (PGPU-06).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Showing {
    /// SDR, or HDR in a format it shows.
    AsIs,
    /// Dolby Vision profile 5 or 8, which it shows as Dolby Vision: a copy
    /// is labelled so.
    DolbyVision,
    /// Dolby Vision it does not show as such (or profile 7, which no
    /// streaming player does), whose base layer it shows: HDR10, HLG or
    /// SDR. A copy is that base layer, the RPU ignored.
    Base,
    /// Neither: the picture is converted.
    No,
}

fn hdr_shown(video: &VideoStream, caps: &ClientCapabilities) -> Showing {
    let Some(hdr) = video.hdr else {
        return Showing::AsIs;
    };
    let shown = caps
        .max_video
        .as_ref()
        .and_then(|limits| limits.hdr.as_ref());
    let shows = |format: HdrFormat| shown.is_none_or(|shown| shown.contains(&format));
    if hdr != HdrFormat::DolbyVision {
        return if shows(hdr) {
            Showing::AsIs
        } else {
            Showing::No
        };
    }
    let streamable = video.dolby_vision.is_none_or(|dv| dv.streamable());
    if streamable && shows(HdrFormat::DolbyVision) {
        return Showing::DolbyVision;
    }
    let base = match video.dolby_vision.map(|dv| dv.base()) {
        Some(DvBase::Hdr10) => shows(HdrFormat::Hdr10),
        Some(DvBase::Hlg) => shows(HdrFormat::Hlg),
        Some(DvBase::Sdr) => true,
        Some(DvBase::None) => false,
        // No record read: the transfer says what the base layer is.
        None => match video.range() {
            spoolframe_media::VideoRange::Pq => shows(HdrFormat::Hdr10),
            spoolframe_media::VideoRange::Hlg => shows(HdrFormat::Hlg),
            spoolframe_media::VideoRange::Sdr => true,
        },
    };
    if base { Showing::Base } else { Showing::No }
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

/// Every text subtitle format: a client that shows none of them is sent no
/// text subtitle.
const TEXT_FORMATS: [SubtitleFormat; 4] = [
    SubtitleFormat::Srt,
    SubtitleFormat::Ass,
    SubtitleFormat::WebVtt,
    SubtitleFormat::MovText,
];

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

    // Subtitles first: burning one in decides the video. Only pictures the
    // client cannot draw are burned in; a text one the client cannot show
    // in any format the output carries is left out, never burned in.
    let subtitle = subtitle.and_then(|picked| {
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
        } else if shown && carried {
            SubtitleAction::Copy { format }
        } else if stream.kind == SubtitleKind::Text {
            let Some(to) = text_formats(container)
                .iter()
                .copied()
                .find(|f| caps.subtitles.contains(f))
            else {
                reasons.push(if TEXT_FORMATS.iter().any(|f| caps.subtitles.contains(f)) {
                    format!(
                        "subtitle {picked}: {}: no text format the client shows goes in \
                         {container}; not shown",
                        name(format.as_str(), &stream.codec)
                    )
                } else {
                    format!("subtitle {picked}: the client shows no text subtitles; not shown")
                });
                return None;
            };
            SubtitleAction::Convert { from: format, to }
        } else {
            // Pictures the client cannot draw in this output.
            reasons.push(format!(
                "subtitle {picked}: {} cannot be shown; burned into the video",
                name(format.as_str(), &stream.codec)
            ));
            SubtitleAction::BurnIn { format }
        };
        Some(ConvertedSubtitle {
            index: picked,
            action,
        })
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
            dolby_vision: hdr_shown(v, caps) == Showing::DolbyVision,
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
        ladder: Vec::new(),
        reasons,
    })
}

/// Plan `descriptor` for a playback of `quality` (P6-06): [`plan`],
/// with the capabilities capped as [`with_quality`] says. Under `Auto`,
/// a plan that encodes the video over HLS becomes a ladder (PX-21): the
/// encodes run from the app's own cap (not the link's) down to
/// [`RUNG_FLOOR`], and the player moves between them as its link
/// changes. A plan that copies the video, or plays the file, stays as
/// it is: it fit the link. Off the LAN (`wan`) in `Auto`, a copy must
/// fit its peak under the cap, not only its average (PX-46).
#[allow(clippy::too_many_arguments)]
pub fn plan_for(
    descriptor: &MediaDescriptor,
    capabilities: Option<&ClientCapabilities>,
    quality: Quality,
    share: Option<u64>,
    wan: bool,
    selection: Selection,
    bitmap: Bitmap,
    encoders: &Encoders,
    ocr: &Ocr,
) -> PlaybackPlan {
    let capped = capabilities.map(|caps| with_quality(caps, quality, share));
    let plan = plan_checked(
        descriptor,
        capped.as_ref(),
        selection,
        bitmap,
        encoders,
        ocr,
        wan && matches!(quality, Quality::Auto { .. }),
    );
    let (Quality::Auto { .. }, Some(caps), Some(source)) =
        (quality, capabilities, descriptor.video.as_ref())
    else {
        return plan;
    };
    // Off the LAN, a copy over HLS is the top of a ladder of encodes under
    // it (PX-21): the player starts low and climbs to the original while
    // the link carries it, where one copied variant has nowhere lower to
    // go. On the LAN the copy plays alone, as it fits. An encode goes on
    // to its own ladder below, on the LAN or off it.
    let plan = if wan {
        remux_ladder(plan, descriptor, capped.as_ref().unwrap_or(caps), encoders)
    } else {
        plan
    };
    let PlaybackPlan::Converted {
        delivery: Delivery::Hls,
        container,
        video: Some(ConvertedVideo::Encode {
            codec, tone_map, ..
        }),
        audio,
        subtitle,
        reasons,
        ..
    } = plan
    else {
        return plan;
    };
    let tallest = caps
        .max_video
        .as_ref()
        .and_then(|limits| limits.height)
        .map_or(source.height, |max| source.height.min(max));
    let audio_bits = match audio.map(|a| a.action) {
        Some(AudioAction::Encode { bitrate, .. }) => bitrate,
        Some(_) => 640_000,
        None => 0,
    };
    // The top rung: the source's own rate, or what its height is worth,
    // under the app's own cap.
    let source_bits = descriptor
        .bit_rate
        .map(|bits| bits.saturating_sub(audio_bits))
        .unwrap_or_else(|| worth(tallest, codec));
    // Under the uplink share too (PX-46): the ladder's top is what this
    // playback may take of it.
    let own = match (caps.max_bitrate, share) {
        (Some(own), Some(share)) => Some(own.min(share)),
        (own, share) => own.or(share),
    };
    let top = own.map_or(source_bits, |max| {
        source_bits.min(max.saturating_sub(audio_bits))
    });
    let rungs = ladder(top, tallest, codec);
    let first = rungs[0];
    PlaybackPlan::Converted {
        delivery: Delivery::Hls,
        container,
        video: Some(ConvertedVideo::Encode {
            codec,
            height: first.height,
            max_bitrate: Some(first.max_bitrate),
            tone_map,
        }),
        audio,
        subtitle,
        // One rung is one encode, as any other plan.
        ladder: if rungs.len() > 1 { rungs } else { Vec::new() },
        reasons,
    }
}

/// `plan`, a copy over HLS (PX-21's remux ladder): the copy as the top
/// rung, then encodes under it as [`ladder`] spreads them from half the
/// copy's rate down, with the codec, range and size this client takes
/// ([`encode_video`]), at most [`MAX_RUNGS`] in all. Every rung has the
/// same audio and subtitle, and is cut where the copy is (`encode`). Any
/// other plan, or one no encode under the copy fits, is `plan` as it is.
fn remux_ladder(
    plan: PlaybackPlan,
    descriptor: &MediaDescriptor,
    caps: &ClientCapabilities,
    encoders: &Encoders,
) -> PlaybackPlan {
    let PlaybackPlan::Converted {
        delivery: Delivery::Hls,
        video: Some(ConvertedVideo::Copy { .. }),
        audio,
        ..
    } = &plan
    else {
        return plan;
    };
    let Some(source) = descriptor.video.as_ref() else {
        return plan;
    };
    let Ok(ConvertedVideo::Encode {
        codec,
        height: tallest,
        tone_map,
        ..
    }) = encode_video(source, caps, encoders, audio.as_ref())
    else {
        return plan;
    };
    let audio_bits = match audio.map(|a| a.action) {
        Some(AudioAction::Encode { bitrate, .. }) => bitrate,
        Some(_) => 640_000,
        None => 0,
    };
    let copy_bits = descriptor
        .bit_rate
        .map(|bits| bits.saturating_sub(audio_bits))
        .unwrap_or_else(|| worth(source.height, codec));
    let under: Vec<Rung> = ladder(copy_bits / 2, tallest, codec)
        .into_iter()
        .filter(|r| r.max_bitrate <= copy_bits / 2)
        .take(MAX_RUNGS - 1)
        .map(|r| Rung {
            video: Some(ConvertedVideo::Encode {
                codec,
                height: r.height,
                max_bitrate: Some(r.max_bitrate),
                tone_map,
            }),
            ..r
        })
        .collect();
    // The floor itself under a copy near it is no ladder worth starting.
    if under.is_empty() || copy_bits <= RUNG_FLOOR {
        return plan;
    }
    let PlaybackPlan::Converted {
        delivery,
        container,
        video,
        audio,
        subtitle,
        mut reasons,
        ..
    } = plan
    else {
        unreachable!("matched above");
    };
    reasons.push(format!(
        "video: off the LAN, the copy is the top of a ladder of {} encodes under it",
        under.len()
    ));
    let top = Rung {
        height: source.height,
        max_bitrate: copy_bits,
        video: None,
    };
    PlaybackPlan::Converted {
        delivery,
        container,
        video,
        audio,
        subtitle,
        ladder: std::iter::once(top).chain(under).collect(),
        reasons,
    }
}

/// The rung of `plan`'s ladder a player starts on (PX-21): the highest
/// whose cap is within [`LINK_SHARE`] of what `Auto` measured, else the
/// lowest, as with nothing measured. A copied rung counts at its peak
/// (`hls::copy_peak`), not its average. 0 for a plan with no ladder.
pub fn start_rung(plan: &PlaybackPlan, quality: Quality) -> u32 {
    let PlaybackPlan::Converted { ladder, video, .. } = plan else {
        return 0;
    };
    let copied = matches!(video, Some(ConvertedVideo::Copy { .. }));
    let rate = |r: &Rung| match r.video {
        None if copied => crate::encode::hls::copy_peak(r.max_bitrate),
        _ => r.max_bitrate,
    };
    let Some(lowest) = ladder.len().checked_sub(1) else {
        return 0;
    };
    let fits = match quality {
        Quality::Auto {
            link_bitrate: Some(link),
        } => {
            let share = (link as f64 * LINK_SHARE) as u64;
            ladder.iter().position(|r| rate(r) <= share)
        }
        _ => None,
    };
    fits.unwrap_or(lowest) as u32
}

/// What a picture `height` tall is worth in `codec`, bits per second,
/// when the source does not say its rate: twice the least its step
/// takes ([`height_for`]), a generous encode of it.
fn worth(height: u32, codec: VideoCodec) -> u64 {
    let least = HEIGHT_STEPS
        .iter()
        .rev()
        .find(|(_, step)| *step >= height)
        .map_or(HEIGHT_STEPS[0].0, |(least, _)| *least);
    2 * scaled(least, codec)
}

/// The video encode for `caps` (PGPU-06): the first of `encoders`' video
/// encoders, in the order the server prefers them (`transcode.video_codecs`
/// for the GPUs' codecs, then software H.264), that the client plays. A
/// GPU's codec counts only where the client decodes it in hardware, as it
/// says ([`ClientCapabilities::decodes_in_hardware`]); a software encode
/// only as H.264, the one software codec the server encodes. An HDR film
/// for a client that shows its HDR, with 10 bits allowed, takes the first
/// that keeps it: HEVC or AV1 on a GPU encoder that took a 10-bit test
/// frame. Anything else HDR is tone mapped to SDR.
fn encode_video(
    video: &VideoStream,
    caps: &ClientCapabilities,
    encoders: &Encoders,
    audio: Option<&ConvertedAudio>,
) -> Result<ConvertedVideo, String> {
    let limits = caps.max_video.clone().unwrap_or_default();
    let height = limits
        .height
        .map_or(video.height, |max| video.height.min(max));
    // 8-bit output where the client asks for it.
    let eight_bit = limits.bit_depth.is_some_and(|b| b < 10);
    // The HDR an encode of it keeps (Dolby Vision's base layer's), where
    // the client shows that. Dolby Vision profile 5 has no base layer to
    // keep, but libplacebo, applying its RPU, makes HDR10 of it.
    let keepable = if video.needs_rpu() {
        Some(HdrFormat::Hdr10)
    } else {
        video.keepable_hdr()
    };
    let keep = keepable
        .filter(|hdr| limits.hdr.as_ref().is_none_or(|shown| shown.contains(hdr)))
        .filter(|_| !eight_bit);
    let mut candidates = encoders.video.iter().filter(|e| match e.hardware {
        Some(_) => caps.decodes_in_hardware(e.codec),
        None => e.codec == VideoCodec::H264 && caps.video.contains(&VideoCodec::H264),
    });
    let keeps = |e: &VideoEncoder| {
        keep.is_some()
            && e.codec != VideoCodec::H264
            && e.hardware.is_some()
            && encoders.encodes_ten_bit(e)
            && (!video.needs_rpu() || e.hardware.is_some_and(|h| encoders.libplacebo(h).is_some()))
    };
    let chosen = candidates
        .clone()
        .find(|e| keeps(e))
        .or_else(|| candidates.next())
        .ok_or("video: the server can encode no codec this client plays")?;
    let codec = chosen.codec;
    let tone_map = video.hdr.is_some() && !keeps(chosen);
    // Leave room for the audio inside the client's limit.
    let audio_bits = match audio.map(|a| a.action) {
        Some(AudioAction::Encode { bitrate, .. }) => bitrate,
        Some(_) => 640_000,
        None => 0,
    };
    let max_bitrate = caps
        .max_bitrate
        .map(|max| max.saturating_sub(audio_bits).max(500_000));
    // A low cap is spent on fewer pixels, not on a blurred large picture.
    let height = max_bitrate.map_or(height, |bits| height.min(height_for(bits, codec)));
    Ok(ConvertedVideo::Encode {
        codec,
        height,
        max_bitrate,
        tone_map,
    })
}

/// The most rungs an HLS ladder has (PX-21).
pub const MAX_RUNGS: usize = 4;
/// No rung is planned under this: below it a picture is not worth
/// watching, and the bottom rung of a slow link's ladder is about here.
pub const RUNG_FLOOR: u64 = 1_000_000;

pub use spoolframe_protocol::playback::Rung;

/// The ladder of encodes from `top` bits per second of `codec` video,
/// for a picture no taller than `tallest` (the source's and the
/// device's), highest first. Its rungs are spread evenly in ratio from
/// `top` down to [`RUNG_FLOOR`], at most [`MAX_RUNGS`] and at least
/// about half apart, so a fast link and a slow one each find one near
/// them. Each rung's height is what its bitrate carries
/// ([`height_for`]), so a rung and a single encode at the same bitrate
/// look the same. Two rungs may share a height: the bitrate step is
/// what lets AVPlayer fit the link, the picture size only follows it.
pub fn ladder(top: u64, tallest: u32, codec: VideoCodec) -> Vec<Rung> {
    let top = top.max(RUNG_FLOOR);
    let span = top as f64 / RUNG_FLOOR as f64;
    // Steps of at least 2x between rungs, and no more rungs than allowed.
    let steps = (span.log2().floor() as usize).min(MAX_RUNGS - 1);
    let ratio = if steps == 0 {
        1.0
    } else {
        span.powf(1.0 / steps as f64)
    };
    let mut rungs: Vec<Rung> = Vec::new();
    for step in 0..=steps {
        let bits = if step == steps {
            if steps == 0 { top } else { RUNG_FLOOR }
        } else {
            (top as f64 / ratio.powi(step as i32)) as u64
        };
        rungs.push(Rung {
            height: tallest.min(height_for(bits, codec)),
            max_bitrate: bits,
            video: None,
        });
    }
    rungs
}

/// The tallest picture `bits` per second of `codec` video carries well
/// (P6-06): at a low bitrate a smaller picture is sharper than a large
/// one gone to blocks. Rough H.264 figures for film; HEVC and AV1 do as
/// well with about two thirds of the bits.
pub(crate) fn height_for(bits: u64, codec: VideoCodec) -> u32 {
    HEIGHT_STEPS
        .iter()
        .find(|(least, _)| bits >= scaled(*least, codec))
        .map_or(480, |(_, height)| *height)
}

/// The least H.264 bitrate for each height, tallest first: rough
/// figures for film.
const HEIGHT_STEPS: [(u64, u32); 4] = [
    (14_000_000, 2160),
    (4_500_000, 1080),
    (2_200_000, 720),
    (1_100_000, 540),
];

/// An H.264 bitrate as `codec` needs it: HEVC and AV1 do as well with
/// about two thirds of the bits.
fn scaled(h264: u64, codec: VideoCodec) -> u64 {
    if matches!(codec, VideoCodec::Hevc | VideoCodec::Av1) {
        h264 * 2 / 3
    } else {
        h264
    }
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
    /// `direct`; `remux` when a conversion encodes nothing, only copying
    /// its streams into another container (PX-24), or `transcode`; or
    /// `unsupported`.
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
            kind: if matches!(video, Some(ConvertedVideo::Encode { .. }))
                || audio.is_some_and(|a| matches!(a.action, AudioAction::Encode { .. }))
            {
                "transcode"
            } else {
                "remux"
            },
            delivery: *delivery,
            video: video.map_or_else(none, |video| match video {
                ConvertedVideo::Copy { codec, .. } => format!("copy:{codec}"),
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
    use spoolframe_types::{AudioRole, DisplayCapabilities, HdrFormat, SubtitleRole, VideoLimits};

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
            gpu_tonemap: false,
            gpu_overlay: false,
            cuda_scale: false,
            cuda_overlay: false,
            libplacebo: Vec::new(),
            ten_bit: Vec::new(),
            unusable: Vec::new(),
            failed: Vec::new(),
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
                dolby_vision: None,
                bit_rate: None,
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
                ..
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
                codec: VideoCodec::Hevc,
                dolby_vision: false,
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
                codec: VideoCodec::Hevc,
                dolby_vision: false,
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
    fn a_text_subtitle_a_client_shows_in_no_text_format_is_not_shown_and_the_video_is_copied() {
        let caps = ClientCapabilities {
            subtitles: BTreeSet::new(),
            ..avplayer()
        };
        let (_, _, video, _, subtitle, reasons) = converted(plan_with(
            &hevc_dts(),
            Some(&caps),
            picked(1, Some(1)),
            &dev_encoders(),
        ));
        assert_eq!(
            video,
            ConvertedVideo::Copy {
                codec: VideoCodec::Hevc,
                dolby_vision: false,
            }
        );
        assert_eq!(subtitle, None);
        assert!(
            reasons
                .contains(&"subtitle 1: the client shows no text subtitles; not shown".to_owned()),
            "{reasons:?}"
        );
    }

    #[test]
    fn a_text_subtitle_the_output_cannot_carry_in_a_format_the_client_shows_is_not_shown() {
        // SubRip only, over HLS: fragmented MP4 carries WebVTT or mov_text.
        let caps = ClientCapabilities {
            subtitles: [SubtitleFormat::Srt].into(),
            ..avplayer()
        };
        let (_, _, video, _, subtitle, reasons) = converted(plan_with(
            &hevc_dts(),
            Some(&caps),
            picked(1, Some(1)),
            &dev_encoders(),
        ));
        assert_eq!(
            video,
            ConvertedVideo::Copy {
                codec: VideoCodec::Hevc,
                dolby_vision: false,
            }
        );
        assert_eq!(subtitle, None);
        assert!(
            reasons.contains(
                &"subtitle 1: srt: no text format the client shows goes in mp4; not shown"
                    .to_owned()
            ),
            "{reasons:?}"
        );
    }

    fn on_link(link: u64) -> Quality {
        auto(link)
    }

    fn auto(link: u64) -> Quality {
        Quality::Auto {
            link_bitrate: Some(link),
        }
    }

    fn rung(height: u32, max_bitrate: u64) -> Rung {
        Rung {
            height,
            max_bitrate,
            video: None,
        }
    }

    /// American Sniper as the user's server has it: an H.264 1080p
    /// Blu-ray rip averaging 9.4 Mbit/s, with AAC picked (its second
    /// audio track, by position).
    fn sniper(quality: Quality, share: Option<u64>, wan: bool) -> PlaybackPlan {
        let mut d = dts_aac_pgs();
        d.bit_rate = Some(9_400_000);
        super::plan_for(
            &d,
            Some(&avplayer()),
            quality,
            share,
            wan,
            picked(1, None),
            Bitmap::Preferred(BitmapPreference::Text),
            &dev_encoders(),
            &Ocr::disabled(),
        )
    }

    fn copies_video(plan: &PlaybackPlan) -> bool {
        match plan {
            PlaybackPlan::Direct { .. } => true,
            PlaybackPlan::Converted { video, .. } => {
                matches!(video, Some(ConvertedVideo::Copy { .. }))
            }
            _ => false,
        }
    }

    /// Off the LAN in Auto, a copy fits its peak (half again over the
    /// average) under the cap: 9.4 Mbit/s peaks near 14.1, over a share of
    /// 10 and under one of 15 (PX-46, the cellular play that never
    /// started).
    #[test]
    fn off_the_lan_a_copy_fits_its_peak_under_the_cap() {
        let unmeasured = Quality::Auto { link_bitrate: None };
        assert!(!copies_video(&sniper(unmeasured, Some(10_000_000), true)));
        assert!(copies_video(&sniper(unmeasured, Some(15_000_000), true)));
        // A measured link of 18 Mbit/s is a 13.5 Mbit/s cap (LINK_SHARE):
        // under the peak, though over the average.
        assert!(!copies_video(&sniper(auto(18_000_000), None, true)));
    }

    /// Off the LAN, a copy that fits is the top of a ladder (PX-21's remux
    /// ladder): the copy, then encodes from half its rate down, each with
    /// a video of its own; the player starts on the lowest, as nothing was
    /// measured, and climbs. One that does not fit is a ladder of encodes
    /// as before, its top under the share.
    #[test]
    fn off_the_lan_a_copy_is_the_top_of_a_ladder() {
        let unmeasured = Quality::Auto { link_bitrate: None };
        let plan = sniper(unmeasured, Some(15_000_000), true);
        assert!(copies_video(&plan), "{plan:?}");
        let rungs = ladder_of(&plan);
        assert!(rungs.len() >= 2 && rungs.len() <= MAX_RUNGS, "{rungs:?}");
        // The copy: the source's height, its video's average, no video of
        // its own (the plan's, copied).
        assert_eq!(rungs[0].video, None);
        assert_eq!(rungs[0].height, 1080);
        let copy_bits = 9_400_000 - 640_000;
        assert_eq!(rungs[0].max_bitrate, copy_bits);
        for r in &rungs[1..] {
            let Some(ConvertedVideo::Encode {
                height,
                max_bitrate: Some(cap),
                ..
            }) = r.video
            else {
                panic!("a lower rung is an encode: {r:?}");
            };
            assert_eq!((height, cap), (r.height, r.max_bitrate));
            assert!(cap <= copy_bits / 2, "{r:?}");
        }
        assert_eq!(start_rung(&plan, unmeasured), rungs.len() as u32 - 1);
        // A link whose share takes the copy's peak starts on the copy; one
        // that takes only its average does not.
        let peak = crate::encode::hls::copy_peak(copy_bits);
        let link = |share: u64| (share as f64 / LINK_SHARE) as u64 + 1;
        assert_eq!(start_rung(&plan, auto(link(peak))), 0);
        assert_ne!(start_rung(&plan, auto(link(copy_bits))), 0);

        // Not fitting: encodes only, as before.
        let plan = sniper(unmeasured, Some(10_000_000), true);
        assert!(!copies_video(&plan));
        let rungs = ladder_of(&plan);
        // A ladder still, not one encode with nowhere lower to go (the
        // remux ladder's first cut left an encode off the LAN rungless).
        assert!(rungs.len() >= 2, "{plan:?}");
        assert!(rungs.iter().all(|r| r.video.is_none()), "{rungs:?}");
        assert!(rungs[0].max_bitrate <= 10_000_000 - 640_000, "{rungs:?}");
    }

    /// On the LAN a copy plays alone: it fits, and nothing is gained.
    #[test]
    fn on_the_lan_a_copy_has_no_ladder() {
        let plan = sniper(Quality::Auto { link_bitrate: None }, None, false);
        assert!(copies_video(&plan));
        assert!(ladder_of(&plan).is_empty(), "{plan:?}");
        // Nor does a quality the viewer chose, off the LAN.
        let chosen = sniper(
            Quality::Bitrate {
                max_bitrate: 20_000_000,
            },
            Some(20_000_000),
            true,
        );
        assert!(ladder_of(&chosen).is_empty(), "{chosen:?}");
    }

    /// On the LAN, and for a quality the viewer chose, the average is
    /// still what has to fit.
    #[test]
    fn on_the_lan_or_chosen_the_average_fits() {
        assert!(copies_video(&sniper(auto(18_000_000), None, false)));
        assert!(copies_video(&sniper(
            Quality::Auto { link_bitrate: None },
            None,
            false
        )));
        assert!(copies_video(&sniper(
            Quality::Bitrate {
                max_bitrate: 10_000_000
            },
            Some(10_000_000),
            true
        )));
        assert!(!copies_video(&sniper(
            Quality::Bitrate {
                max_bitrate: 9_000_000
            },
            None,
            true
        )));
    }

    fn plan_for_quality(caps: &ClientCapabilities, quality: Quality) -> PlaybackPlan {
        plan_for_share(caps, quality, None)
    }

    fn plan_for_share(
        caps: &ClientCapabilities,
        quality: Quality,
        share: Option<u64>,
    ) -> PlaybackPlan {
        super::plan_for(
            &hevc_dts(),
            Some(caps),
            quality,
            share,
            share.is_some(),
            picked(1, None),
            Bitmap::Preferred(BitmapPreference::Text),
            &dev_encoders(),
            &Ocr::disabled(),
        )
    }

    fn ladder_of(plan: &PlaybackPlan) -> &[Rung] {
        match plan {
            PlaybackPlan::Converted { ladder, .. } => ladder,
            _ => &[],
        }
    }

    /// A 4K HDR film that a slow link makes an encode: a ladder from its
    /// own rate, not the link's, with a single encode's codec and tone map.
    #[test]
    fn auto_over_hls_that_encodes_is_a_ladder_from_the_source_s_rate() {
        let plan = plan_for_quality(&avplayer(), on_link(5_000_000));
        let rungs = ladder_of(&plan);
        assert_eq!(rungs.len(), MAX_RUNGS, "{plan:?}");
        assert_eq!(rungs[0].height, 2160);
        assert!(rungs[0].max_bitrate > 30_000_000, "{rungs:?}");
        let (_, _, video, _, _, _) = converted(plan.clone());
        let ConvertedVideo::Encode { codec, .. } = video else {
            panic!("not encoded: {video:?}");
        };
        for r in rungs {
            assert_eq!(r.height, height_for(r.max_bitrate, codec).min(2160));
        }
        // The codec and tone map are a single encode's of this plan.
        let single = plan_with(
            &hevc_dts(),
            Some(&with_quality(&avplayer(), on_link(5_000_000), None)),
            picked(1, None),
            &dev_encoders(),
        );
        let (_, _, one, _, _, _) = converted(single);
        let (
            ConvertedVideo::Encode {
                codec: a,
                tone_map: x,
                height,
                max_bitrate,
            },
            ConvertedVideo::Encode {
                codec: b,
                tone_map: y,
                ..
            },
        ) = (video, one)
        else {
            panic!("not both encoded");
        };
        assert_eq!((a, x), (b, y));
        assert_eq!((height, max_bitrate), (2160, Some(rungs[0].max_bitrate)));
    }

    #[test]
    fn the_app_s_cap_and_height_bound_the_ladder() {
        let mut caps = avplayer();
        caps.max_bitrate = Some(6_000_000);
        caps.max_video = Some(VideoLimits {
            height: Some(1080),
            ..VideoLimits::default()
        });
        let plan = plan_for_quality(&caps, on_link(2_000_000));
        let rungs = ladder_of(&plan);
        assert!(!rungs.is_empty(), "{plan:?}");
        assert!(
            rungs
                .iter()
                .all(|r| r.max_bitrate <= 6_000_000 && r.height <= 1080),
            "{rungs:?}"
        );
    }

    /// Auto on a fast link copies the video, as it fits: no ladder.
    #[test]
    fn auto_that_copies_the_video_has_no_ladder() {
        let plan = plan_for_quality(&avplayer(), on_link(1_000_000_000));
        let (_, _, video, _, _, _) = converted(plan.clone());
        assert!(matches!(video, ConvertedVideo::Copy { .. }), "{video:?}");
        assert!(ladder_of(&plan).is_empty());
    }

    /// PX-46: Auto off the LAN with nothing measured, a 40 Mbit/s film
    /// and a 5 Mbit/s share: converted under the share, not copied.
    #[test]
    fn auto_off_the_lan_is_held_to_its_share() {
        let unmeasured = Quality::Auto { link_bitrate: None };
        let plan = plan_for_share(&avplayer(), unmeasured, Some(5_000_000));
        let (_, _, video, _, _, _) = converted(plan.clone());
        assert!(matches!(video, ConvertedVideo::Encode { .. }), "{video:?}");
        for rung in ladder_of(&plan) {
            assert!(rung.max_bitrate <= 5_000_000, "{rung:?}");
        }
        // Without a share, as before: the film's own rate.
        let open = plan_for_share(&avplayer(), unmeasured, None);
        let (_, _, video, _, _, _) = converted(open);
        assert!(matches!(video, ConvertedVideo::Copy { .. }), "{video:?}");
    }

    /// A measured link under the share is the cap; a share under the link
    /// is.
    #[test]
    fn auto_takes_the_lower_of_its_link_and_its_share() {
        assert_eq!(
            with_quality(&avplayer(), auto(4_000_000), Some(20_000_000)).max_bitrate,
            Some((4_000_000_f64 * LINK_SHARE) as u64)
        );
        assert_eq!(
            with_quality(&avplayer(), auto(100_000_000), Some(5_000_000)).max_bitrate,
            Some(5_000_000)
        );
    }

    /// A quality the viewer chose is theirs: the share does not touch it.
    #[test]
    fn a_chosen_quality_ignores_the_share() {
        assert_eq!(
            with_quality(&avplayer(), Quality::Original, Some(1_000_000)),
            with_quality(&avplayer(), Quality::Original, None)
        );
        let chosen = Quality::Bitrate {
            max_bitrate: 8_000_000,
        };
        assert_eq!(
            with_quality(&avplayer(), chosen, Some(1_000_000)).max_bitrate,
            Some(8_000_000)
        );
    }

    /// A chosen bitrate is one encode, as P6-06 plans it.
    #[test]
    fn a_chosen_bitrate_over_hls_is_one_encode() {
        let quality = Quality::Bitrate {
            max_bitrate: 5_000_000,
        };
        let plan = plan_for_quality(&avplayer(), quality);
        assert!(ladder_of(&plan).is_empty(), "{plan:?}");
        assert_eq!(
            plan,
            plan_with(
                &hevc_dts(),
                Some(&with_quality(&avplayer(), quality, None)),
                picked(1, None),
                &dev_encoders()
            )
        );
    }

    #[test]
    fn a_ladder_spans_a_fast_link_to_a_slow_one_in_four_rungs() {
        // 40 Mbit/s down to 1: about 3.4x a step.
        let rungs = ladder(40_000_000, 2160, VideoCodec::Hevc);
        let bits: Vec<u64> = rungs.iter().map(|r| r.max_bitrate).collect();
        let heights: Vec<u32> = rungs.iter().map(|r| r.height).collect();
        assert_eq!(bits, [40_000_000, 11_696_070, 3_419_951, RUNG_FLOOR]);
        assert_eq!(heights, [2160, 2160, 1080, 540]);
        // Each rung's height is a single encode's at its bitrate.
    }

    #[test]
    fn a_720p_source_s_rungs_step_down_in_bitrate_under_720p() {
        let rungs = ladder(8_000_000, 720, VideoCodec::H264);
        assert_eq!(
            rungs,
            [
                rung(720, 8_000_000),
                rung(720, 4_000_000),
                rung(540, 2_000_000),
                rung(480, RUNG_FLOOR)
            ]
        );
    }

    #[test]
    fn a_rung_s_height_is_a_single_encode_s_at_its_bitrate() {
        for r in ladder(16_000_000, 2160, VideoCodec::H264) {
            assert_eq!(
                r.height,
                height_for(r.max_bitrate, VideoCodec::H264).min(2160)
            );
        }
    }

    #[test]
    fn no_rung_is_taller_than_the_source() {
        assert!(
            ladder(30_000_000, 720, VideoCodec::H264)
                .iter()
                .all(|r| r.height <= 720)
        );
    }

    #[test]
    fn a_low_top_is_one_rung_at_the_floor() {
        assert_eq!(
            ladder(600_000, 1080, VideoCodec::H264),
            [rung(480, RUNG_FLOOR)]
        );
    }

    #[test]
    fn a_low_cap_lowers_the_picture_too() {
        assert_eq!(height_for(30_000_000, VideoCodec::H264), 2160);
        assert_eq!(height_for(6_000_000, VideoCodec::H264), 1080);
        assert_eq!(height_for(3_000_000, VideoCodec::H264), 720);
        assert_eq!(height_for(1_500_000, VideoCodec::H264), 540);
        assert_eq!(height_for(500_000, VideoCodec::H264), 480);
    }

    #[test]
    fn hevc_keeps_a_taller_picture_at_the_same_cap() {
        assert_eq!(height_for(3_000_000, VideoCodec::H264), 720);
        assert_eq!(height_for(3_000_000, VideoCodec::Hevc), 1080);
    }

    #[test]
    fn a_4k_file_capped_low_is_encoded_smaller() {
        let mut caps = avplayer();
        caps.max_bitrate = Some(3_000_000);
        let (_, _, video, _, _, _) = converted(plan_with(
            &hevc_dts(),
            Some(&caps),
            picked(1, None),
            &dev_encoders(),
        ));
        let ConvertedVideo::Encode { height, .. } = video else {
            panic!("not encoded: {video:?}");
        };
        assert!(height < 2160, "{height}");
    }

    #[test]
    fn a_link_lowers_the_cap_to_its_share() {
        let caps = avplayer();
        assert_eq!(
            with_quality(&caps, auto(20_000_000), None).max_bitrate,
            Some(15_000_000)
        );
    }

    #[test]
    fn a_fast_link_leaves_the_app_s_own_cap() {
        let mut caps = avplayer();
        caps.max_bitrate = Some(8_000_000);
        assert_eq!(
            with_quality(&caps, auto(100_000_000), None).max_bitrate,
            Some(8_000_000)
        );
    }

    #[test]
    fn no_measurement_leaves_the_caps_as_they_are() {
        let caps = avplayer();
        assert_eq!(with_quality(&caps, Quality::default(), None), caps);
    }

    #[test]
    fn a_chosen_bitrate_caps_the_playback_whatever_the_link() {
        let caps = avplayer();
        let quality = Quality::Bitrate {
            max_bitrate: 4_000_000,
        };
        assert_eq!(
            with_quality(&caps, quality, None).max_bitrate,
            Some(4_000_000)
        );
    }

    #[test]
    fn the_app_s_own_cap_stays_over_a_chosen_bitrate() {
        let mut caps = avplayer();
        caps.max_bitrate = Some(8_000_000);
        let quality = Quality::Bitrate {
            max_bitrate: 20_000_000,
        };
        assert_eq!(
            with_quality(&caps, quality, None).max_bitrate,
            Some(8_000_000)
        );
    }

    #[test]
    fn original_quality_leaves_the_caps_as_they_are() {
        let mut caps = avplayer();
        caps.max_bitrate = Some(8_000_000);
        assert_eq!(with_quality(&caps, Quality::Original, None), caps);
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

    /// What the server plans with on a GPU that encodes AV1, HEVC and
    /// H.264 (PGPU-06): `transcode.video_codecs`' order, then software
    /// H.264; `ten_bit` the encoders that took a 10-bit frame.
    fn gpu_encoders(order: &[(VideoCodec, &str)], ten_bit: &[&str]) -> Encoders {
        let mut encoders = dev_encoders();
        encoders.video = order
            .iter()
            .map(|&(codec, name)| VideoEncoder {
                codec,
                name: name.into(),
                hardware: (!name.starts_with("lib")).then_some(Hardware::Vaapi),
            })
            .collect();
        encoders.ten_bit = encoders
            .video
            .iter()
            .filter(|e| ten_bit.contains(&e.name.as_str()))
            .cloned()
            .collect();
        encoders
    }

    const AV1_HEVC_H264: [(VideoCodec, &str); 4] = [
        (VideoCodec::Av1, "av1_vaapi"),
        (VideoCodec::Hevc, "hevc_vaapi"),
        (VideoCodec::H264, "h264_vaapi"),
        (VideoCodec::H264, "libx264"),
    ];

    /// An Apple TV that plays AV1 too, showing HDR10, over HLS.
    fn hdr_tv() -> ClientCapabilities {
        ClientCapabilities {
            video: [VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1].into(),
            max_video: Some(VideoLimits {
                height: Some(1080),
                hdr: Some([HdrFormat::Hdr10].into()),
                ..VideoLimits::default()
            }),
            ..avplayer()
        }
    }

    fn encoded(
        caps: &ClientCapabilities,
        d: &MediaDescriptor,
        encoders: &Encoders,
    ) -> (VideoCodec, bool) {
        match converted(plan_with(d, Some(caps), picked(1, None), encoders)).2 {
            ConvertedVideo::Encode {
                codec, tone_map, ..
            } => (codec, tone_map),
            other => panic!("not an encode: {other:?}"),
        }
    }

    /// PGPU-06: an HDR10 film for a client that shows HDR10 and decodes
    /// AV1 in hardware keeps its HDR in AV1, the first of the order, on a
    /// GPU that writes 10 bits; not H.264 tone mapped.
    #[test]
    fn an_hdr_film_keeps_its_hdr_in_the_first_codec_of_the_order() {
        let encoders = gpu_encoders(&AV1_HEVC_H264, &["av1_vaapi", "hevc_vaapi"]);
        assert_eq!(
            encoded(&hdr_tv(), &hevc_dts(), &encoders),
            (VideoCodec::Av1, false)
        );
    }

    /// A client that says it decodes only HEVC and H.264 in hardware gets
    /// HEVC, HDR kept: AV1 would be decoded in software.
    #[test]
    fn a_codec_the_client_decodes_in_software_is_passed_over() {
        let encoders = gpu_encoders(&AV1_HEVC_H264, &["av1_vaapi", "hevc_vaapi"]);
        let caps = ClientCapabilities {
            hardware_video: Some([VideoCodec::H264, VideoCodec::Hevc].into()),
            ..hdr_tv()
        };
        assert_eq!(
            encoded(&caps, &hevc_dts(), &encoders),
            (VideoCodec::Hevc, false)
        );
    }

    /// A client that shows no HDR: the first codec, tone mapped.
    #[test]
    fn an_hdr_film_for_an_sdr_screen_is_tone_mapped_in_the_first_codec() {
        let encoders = gpu_encoders(&AV1_HEVC_H264, &["av1_vaapi", "hevc_vaapi"]);
        let mut caps = hdr_tv();
        caps.max_video.as_mut().unwrap().hdr = Some(BTreeSet::new());
        assert_eq!(
            encoded(&caps, &hevc_dts(), &encoders),
            (VideoCodec::Av1, true)
        );
        // Nor is HDR kept for a client limited to 8 bits.
        let mut caps = hdr_tv();
        caps.max_video.as_mut().unwrap().bit_depth = Some(8);
        assert_eq!(
            encoded(&caps, &hevc_dts(), &encoders),
            (VideoCodec::Av1, true)
        );
    }

    /// A start's display (PGPU-06) is planned for over Hello's: a TV that
    /// said HDR10 at Hello but is in an SDR mode now gets a tone map.
    #[test]
    fn a_display_without_hdr_tone_maps_over_hello_s_hdr() {
        let encoders = gpu_encoders(&AV1_HEVC_H264, &["av1_vaapi", "hevc_vaapi"]);
        let sdr_now = DisplayCapabilities {
            bit_depth: None,
            hdr: Some(BTreeSet::new()),
        };
        assert_eq!(
            encoded(
                &hdr_tv().with_display(Some(&sdr_now)),
                &hevc_dts(),
                &encoders
            ),
            (VideoCodec::Av1, true)
        );
    }

    /// And one that said no HDR at Hello, in an HDR10 mode now, keeps it.
    #[test]
    fn a_display_with_hdr_keeps_it_over_hello_s_sdr() {
        let encoders = gpu_encoders(&AV1_HEVC_H264, &["av1_vaapi", "hevc_vaapi"]);
        let mut sdr_at_hello = hdr_tv();
        sdr_at_hello.max_video.as_mut().unwrap().hdr = Some(BTreeSet::new());
        let hdr_now = DisplayCapabilities {
            bit_depth: Some(10),
            hdr: Some([HdrFormat::Hdr10].into()),
        };
        assert_eq!(
            encoded(&sdr_at_hello, &hevc_dts(), &encoders),
            (VideoCodec::Av1, true)
        );
        assert_eq!(
            encoded(
                &sdr_at_hello.with_display(Some(&hdr_now)),
                &hevc_dts(),
                &encoders
            ),
            (VideoCodec::Av1, false)
        );
    }

    /// An AV1 encoder that took no 10-bit frame does not keep HDR; HEVC's
    /// does, so HEVC is taken over AV1, first in the order.
    #[test]
    fn hdr_goes_to_the_first_codec_that_keeps_it() {
        let encoders = gpu_encoders(&AV1_HEVC_H264, &["hevc_vaapi"]);
        assert_eq!(
            encoded(&hdr_tv(), &hevc_dts(), &encoders),
            (VideoCodec::Hevc, false)
        );
        // None keeps it: the first, tone mapped.
        let encoders = gpu_encoders(&AV1_HEVC_H264, &[]);
        assert_eq!(
            encoded(&hdr_tv(), &hevc_dts(), &encoders),
            (VideoCodec::Av1, true)
        );
    }

    /// No GPU codec the client plays: software H.264, tone mapped. A
    /// software HEVC or AV1 encoder, were one listed, is never taken.
    #[test]
    fn with_no_gpu_codec_left_it_is_software_h264() {
        let encoders = gpu_encoders(
            &[
                (VideoCodec::Av1, "av1_vaapi"),
                (VideoCodec::Hevc, "libx265"),
                (VideoCodec::H264, "libx264"),
            ],
            &["av1_vaapi"],
        );
        let caps = ClientCapabilities {
            video: [VideoCodec::H264, VideoCodec::Hevc].into(),
            ..hdr_tv()
        };
        assert_eq!(
            encoded(&caps, &hevc_dts(), &encoders),
            (VideoCodec::H264, true)
        );
        // `video_codecs = ["h264"]`: H.264 on the GPU, tone mapped.
        let encoders = gpu_encoders(
            &[
                (VideoCodec::H264, "h264_vaapi"),
                (VideoCodec::H264, "libx264"),
            ],
            &[],
        );
        assert_eq!(
            encoded(&hdr_tv(), &hevc_dts(), &encoders),
            (VideoCodec::H264, true)
        );
        // A client that plays only HEVC, and no GPU encodes it: nothing.
        let hevc_only = ClientCapabilities {
            video: [VideoCodec::Hevc].into(),
            ..hdr_tv()
        };
        let encoders = gpu_encoders(
            &[(VideoCodec::Hevc, "libx265"), (VideoCodec::H264, "libx264")],
            &[],
        );
        let plan = plan_with(&hevc_dts(), Some(&hevc_only), picked(1, None), &encoders);
        assert!(
            reasons(plan)
                .iter()
                .any(|r| r == "video: the server can encode no codec this client plays")
        );
    }

    /// Dolby Vision keeps its base layer's HDR10 through an encode, for
    /// a client that shows HDR10; profile 5 has none, and is tone mapped.
    #[test]
    fn dolby_vision_keeps_its_hdr10_base_layer_but_profile_5() {
        let encoders = gpu_encoders(&AV1_HEVC_H264, &["av1_vaapi", "hevc_vaapi"]);
        let dv = |profile| {
            let mut d = hevc_dts();
            let v = d.video.as_mut().unwrap();
            v.hdr = Some(HdrFormat::DolbyVision);
            v.color_transfer = Some("smpte2084".into());
            v.dolby_vision = Some(spoolframe_media::DolbyVision {
                profile,
                level: 6,
                compatibility: if profile == 5 { 0 } else { 1 },
            });
            d
        };
        assert_eq!(
            encoded(&hdr_tv(), &dv(8), &encoders),
            (VideoCodec::Av1, false)
        );
        assert_eq!(
            encoded(&hdr_tv(), &dv(5), &encoders),
            (VideoCodec::Av1, true)
        );
    }

    /// A 4K Dolby Vision HEVC film of `profile` (and base-layer
    /// compatibility), its base layer PQ, with DTS sound and subtitles.
    fn dolby_vision(profile: u8, compatibility: u8) -> MediaDescriptor {
        let mut d = hevc_dts();
        let v = d.video.as_mut().unwrap();
        v.hdr = Some(HdrFormat::DolbyVision);
        v.color_transfer = Some("smpte2084".into());
        v.dolby_vision = Some(spoolframe_media::DolbyVision {
            profile,
            level: 6,
            compatibility,
        });
        d
    }

    /// An Apple TV at 4K that shows `shown`, over HLS.
    fn showing(shown: &[HdrFormat]) -> ClientCapabilities {
        ClientCapabilities {
            max_video: Some(VideoLimits {
                hdr: Some(shown.iter().copied().collect()),
                ..VideoLimits::default()
            }),
            ..avplayer()
        }
    }

    fn video_of(plan: PlaybackPlan) -> ConvertedVideo {
        converted(plan).2
    }

    /// PGPU-06: profile 8.1 for a client that shows HDR10 but not Dolby
    /// Vision is its base layer copied, not converted and tone mapped;
    /// for one that shows Dolby Vision, copied and labelled so; for one
    /// that shows neither, converted.
    #[test]
    fn dolby_vision_8_is_copied_as_its_base_layer_or_labelled() {
        let encoders = gpu_encoders(&AV1_HEVC_H264, &["av1_vaapi", "hevc_vaapi"]);
        let plan = |caps: &ClientCapabilities| {
            video_of(plan_with(
                &dolby_vision(8, 1),
                Some(caps),
                picked(1, None),
                &encoders,
            ))
        };
        assert_eq!(
            plan(&showing(&[HdrFormat::Hdr10])),
            ConvertedVideo::Copy {
                codec: VideoCodec::Hevc,
                dolby_vision: false,
            }
        );
        assert_eq!(
            plan(&showing(&[HdrFormat::Hdr10, HdrFormat::DolbyVision])),
            ConvertedVideo::Copy {
                codec: VideoCodec::Hevc,
                dolby_vision: true,
            }
        );
        assert!(matches!(
            plan(&showing(&[])),
            ConvertedVideo::Encode { tone_map: true, .. }
        ));
        // An HLG base layer (8.4) is shown where HLG is.
        let hlg = video_of(plan_with(
            &dolby_vision(8, 4),
            Some(&showing(&[HdrFormat::Hlg])),
            picked(1, None),
            &encoders,
        ));
        assert!(matches!(
            hlg,
            ConvertedVideo::Copy {
                dolby_vision: false,
                ..
            }
        ));
    }

    /// Profile 7, which no streaming player shows as Dolby Vision, is its
    /// HDR10 base layer even to a client that shows Dolby Vision.
    #[test]
    fn dolby_vision_7_is_its_hdr10_base_layer() {
        let encoders = gpu_encoders(&AV1_HEVC_H264, &["av1_vaapi", "hevc_vaapi"]);
        let plan = video_of(plan_with(
            &dolby_vision(7, 6),
            Some(&showing(&[HdrFormat::Hdr10, HdrFormat::DolbyVision])),
            picked(1, None),
            &encoders,
        ));
        assert_eq!(
            plan,
            ConvertedVideo::Copy {
                codec: VideoCodec::Hevc,
                dolby_vision: false,
            }
        );
    }

    /// Profile 5 has no base layer: copied and labelled for a client that
    /// shows Dolby Vision; for one that shows only HDR10, converted with
    /// libplacebo applying the RPU, HDR10 kept, where a GPU here runs it;
    /// tone mapped where none does.
    #[test]
    fn dolby_vision_5_is_labelled_or_made_hdr10_by_libplacebo() {
        use spoolframe_media::encoders::{Libplacebo, VulkanDevice};
        let mut encoders = gpu_encoders(&AV1_HEVC_H264, &["av1_vaapi", "hevc_vaapi"]);
        let p5 = dolby_vision(5, 0);
        let plan = |caps: &ClientCapabilities, encoders: &Encoders| {
            video_of(plan_with(&p5, Some(caps), picked(1, None), encoders))
        };
        assert_eq!(
            plan(&showing(&[HdrFormat::DolbyVision]), &encoders),
            ConvertedVideo::Copy {
                codec: VideoCodec::Hevc,
                dolby_vision: true,
            }
        );
        // No libplacebo: tone mapped.
        assert!(matches!(
            plan(&showing(&[HdrFormat::Hdr10]), &encoders),
            ConvertedVideo::Encode { tone_map: true, .. }
        ));
        encoders.libplacebo = vec![Libplacebo {
            hardware: Hardware::Vaapi,
            device: VulkanDevice::Own,
        }];
        assert!(matches!(
            plan(&showing(&[HdrFormat::Hdr10]), &encoders),
            // This Apple TV plays no AV1: HEVC.
            ConvertedVideo::Encode {
                codec: VideoCodec::Hevc,
                tone_map: false,
                ..
            }
        ));
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
                codec: VideoCodec::H264,
                dolby_vision: false,
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
                codec: VideoCodec::H264,
                dolby_vision: false,
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
        // Burn-in is no longer forced: it asks for pictures.
        assert_eq!(
            Bitmap::asked(BitmapSubtitles::BurnIn),
            Some(Bitmap::Preferred(BitmapPreference::Pictures))
        );
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
    fn burn_in_asked_for_lets_a_client_that_draws_pgs_draw_it() {
        let caps = ClientCapabilities {
            subtitles: [SubtitleFormat::Srt, SubtitleFormat::Pgs].into(),
            delivery: vec![Delivery::Range, Delivery::Progressive],
            ..caps()
        };
        // DTS needs encoding, so the plan is converted; the video is not.
        let (_, _, video, _, subtitle, reasons) = converted(with_mode(
            &dts_aac_pgs(),
            &caps,
            picked(0, Some(0)),
            Bitmap::asked(BitmapSubtitles::BurnIn).unwrap(),
            &english(),
        ));
        assert_eq!(
            video,
            ConvertedVideo::Copy {
                codec: VideoCodec::H264,
                dolby_vision: false,
            }
        );
        assert_eq!(
            subtitle,
            Some(ConvertedSubtitle {
                index: 0,
                action: SubtitleAction::Copy {
                    format: SubtitleFormat::Pgs
                },
            })
        );
        assert_eq!(reasons, ["audio 0: dts not supported"]);
    }

    #[test]
    fn burn_in_asked_for_burns_in_for_a_client_that_cannot_draw_pgs() {
        let (_, _, video, _, subtitle, _) = converted(with_mode(
            &dts_aac_pgs(),
            &caps_that_can_burn_in(),
            picked(1, Some(0)),
            Bitmap::asked(BitmapSubtitles::BurnIn).unwrap(),
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
    }

    #[test]
    fn with_text_recognition_off_pictures_a_client_cannot_draw_are_burned_in() {
        let (_, _, video, _, subtitle, _) = converted(with_mode(
            &dts_aac_pgs(),
            &caps_that_can_burn_in(),
            picked(1, Some(0)),
            Bitmap::Preferred(BitmapPreference::Text),
            &Ocr::disabled(),
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
                codec: VideoCodec::H264,
                dolby_vision: false,
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
            Bitmap::Text,
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
                dolby_vision: false,
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
            ladder: Vec::new(),
            reasons: Vec::new(),
        };
        assert_eq!(
            summary(&plan),
            PlanSummary {
                kind: "transcode",
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
            ladder: Vec::new(),
            reasons: Vec::new(),
        };
        let burned = summary(&burned);
        assert_eq!(burned.kind, "transcode");
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

    /// PX-24: an HLS playback that copies its video and audio is a remux,
    /// whatever it does with text beside them.
    #[test]
    fn a_copy_of_video_and_audio_is_a_remux() {
        let plan = PlaybackPlan::Converted {
            delivery: Delivery::Hls,
            container: Container::Mp4,
            video: Some(ConvertedVideo::Copy {
                codec: VideoCodec::H264,
                dolby_vision: false,
            }),
            audio: Some(ConvertedAudio {
                index: 0,
                action: AudioAction::Copy {
                    codec: AudioCodec::Aac,
                },
            }),
            subtitle: Some(ConvertedSubtitle {
                index: 0,
                action: SubtitleAction::Convert {
                    from: SubtitleFormat::Srt,
                    to: SubtitleFormat::WebVtt,
                },
            }),
            ladder: Vec::new(),
            reasons: Vec::new(),
        };
        assert_eq!(summary(&plan).kind, "remux");
    }
}
