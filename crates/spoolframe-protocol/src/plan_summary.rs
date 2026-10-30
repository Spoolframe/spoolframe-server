//! A playback plan as a player's info panel shows it: what kind of play
//! it is, and what plays, flat ([`PlaybackPlan::summary`]).

use spoolframe_types::{AudioCodec, Delivery, SubtitleFormat, VideoCodec};

use crate::playback::{
    AudioAction, ConvertedVideo, PlannedSubtitle, PlaybackPlan, Rung, SubtitleAction,
    SubtitleDelivery,
};

/// What kind of play a plan is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum PlanKind {
    /// The file as it is.
    Direct,
    /// Converted with the picture and sound copied: a new container,
    /// nothing encoded. A text subtitle converted (to WebVTT for HLS) or
    /// read from a bitmap one (OCR) is still a remux: the picture and
    /// sound are the file's, and OCR takes the server little CPU. A
    /// ladder with an encoded rung is not one.
    Remux,
    /// Converted with the video or audio encoded, or a subtitle burned in
    /// (which encodes the video).
    Convert,
    /// The client cannot play this selection: see `reasons`.
    Unsupported,
    /// A plan kind this build does not know.
    Unknown,
}

/// The video that plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoSummary {
    pub codec: VideoCodec,
    /// Into the output as it is: the file's own.
    pub copied: bool,
    /// Shown as Dolby Vision: a copied profile 5 or 8 stream.
    pub dolby_vision: bool,
    /// An encode's cap, bits per second; `None` for a copy, or the
    /// encoder's own choice.
    pub max_bitrate: Option<u64>,
}

/// The audio that plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioSummary {
    pub codec: AudioCodec,
    /// The position of the track in the item's audio list.
    pub index: u32,
    /// Into the output (or played) as it is.
    pub copied: bool,
    /// An encode's channels; `None` for a copy, which keeps the file's.
    pub channels: Option<u32>,
    /// An encode's bits per second; `None` for a copy.
    pub bitrate: Option<u64>,
}

/// How the picked subtitle is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtitleHow {
    /// A stream of the file played as it is.
    Embedded,
    /// A separate file the client fetches whole.
    External,
    /// Into the output as it is.
    Copy,
    /// Converted to another text format into the output.
    Convert,
    /// A picture subtitle read as text, fetched whole.
    Ocr,
    /// Drawn into the video.
    BurnIn,
    /// A way this build does not know.
    Unknown,
}

/// The subtitle that shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubtitleSummary {
    /// The position of the track in the item's subtitle list.
    pub index: u32,
    pub how: SubtitleHow,
    /// The format the client gets: the output's after a conversion or
    /// a reading as text; the file's for a burn-in, which the client
    /// never gets.
    pub format: Option<SubtitleFormat>,
}

/// A plan, flat ([`PlaybackPlan::summary`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanSummary {
    pub kind: PlanKind,
    /// How a converted output is sent; `None` for the file as it is.
    pub delivery: Option<Delivery>,
    pub video: Option<VideoSummary>,
    pub audio: Option<AudioSummary>,
    pub subtitle: Option<SubtitleSummary>,
    /// Why it is not played as it is, or why it cannot play.
    pub reasons: Vec<String>,
    /// The lines an encode outputs; `None` where the picture is the
    /// file's own.
    pub target_height: Option<u32>,
    /// A ladder's heights, highest first; empty for one encode.
    pub ladder_heights: Vec<u32>,
    /// HDR mapped to SDR.
    pub tone_mapped: bool,
}

/// The parts of a plan a player reads, by value.
#[cfg_attr(feature = "uniffi", uniffi::export)]
impl PlaybackPlan {
    /// What kind of play it is ([`PlaybackPlan::summary`]'s).
    pub fn kind(&self) -> PlanKind {
        self.summary().kind
    }

    /// A converted plan's video: copied or encoded; `None` for any other
    /// plan, or no video.
    pub fn converted_video(&self) -> Option<ConvertedVideo> {
        match self {
            PlaybackPlan::Converted { video, .. } => *video,
            _ => None,
        }
    }

    /// What a converted plan does to the audio; `None` for any other
    /// plan, or no audio.
    pub fn audio_action(&self) -> Option<AudioAction> {
        match self {
            PlaybackPlan::Converted { audio, .. } => audio.map(|a| a.action),
            _ => None,
        }
    }

    /// What a converted plan does to the subtitle; `None` for any other
    /// plan, or none picked.
    pub fn subtitle_action(&self) -> Option<SubtitleAction> {
        match self {
            PlaybackPlan::Converted { subtitle, .. } => subtitle.map(|s| s.action),
            _ => None,
        }
    }

    /// The subtitle is drawn into the video: the player shows none of
    /// its own, and turning it off plans again.
    pub fn burns_in_subtitle(&self) -> bool {
        matches!(self.subtitle_action(), Some(SubtitleAction::BurnIn { .. }))
    }
}

/// [`PlaybackPlan`]'s borrowing reads, owned, for Swift and Kotlin under
/// the same names.
#[cfg(feature = "uniffi")]
#[uniffi::export]
impl PlaybackPlan {
    /// [`PlaybackPlan::ladder`].
    #[uniffi::method(name = "ladder")]
    pub fn ladder_owned(&self) -> Vec<Rung> {
        self.ladder().to_vec()
    }

    /// [`PlaybackPlan::source_codec`].
    #[uniffi::method(name = "source_codec")]
    pub fn source_codec_owned(&self) -> Option<String> {
        self.source_codec().map(str::to_owned)
    }
}

#[cfg_attr(feature = "uniffi", uniffi::export)]
impl PlaybackPlan {
    /// The lines the server encodes the picture to, for a quality
    /// picker's "Auto (720p)"; `None` where the picture is the file's
    /// own (a direct play, a copied video) or there is none.
    pub fn encoded_height(&self) -> Option<u32> {
        match self {
            PlaybackPlan::Converted {
                video: Some(ConvertedVideo::Encode { height, .. }),
                ..
            } => Some(*height),
            _ => None,
        }
    }
}

impl PlaybackPlan {
    /// The encodes an HLS player switches between, highest first; empty
    /// for one encode, or a plan that isn't converted.
    pub fn ladder(&self) -> &[Rung] {
        match self {
            PlaybackPlan::Converted { ladder, .. } => ladder,
            _ => &[],
        }
    }

    /// The plan as an info panel shows it. A remux is a converted plan
    /// whose streams are all copied, as [`PlaybackPlan::Converted`]
    /// says, with no encoded rung in its ladder.
    pub fn summary(&self) -> PlanSummary {
        match self {
            PlaybackPlan::Direct {
                video,
                audio,
                subtitle,
                ..
            } => PlanSummary {
                kind: PlanKind::Direct,
                delivery: None,
                video: video.map(|codec| VideoSummary {
                    codec,
                    copied: true,
                    dolby_vision: false,
                    max_bitrate: None,
                }),
                audio: audio.map(|a| AudioSummary {
                    codec: a.codec,
                    index: a.index,
                    copied: true,
                    channels: None,
                    bitrate: None,
                }),
                subtitle: subtitle.map(|s| direct_subtitle(&s)),
                reasons: Vec::new(),
                target_height: None,
                ladder_heights: Vec::new(),
                tone_mapped: false,
            },
            PlaybackPlan::Converted {
                delivery,
                video,
                audio,
                subtitle,
                ladder,
                reasons,
                ..
            } => {
                let video_summary = video.and_then(|v| converted_video(&v));
                let audio = audio.and_then(|a| match a.action {
                    AudioAction::Copy { codec } => Some(AudioSummary {
                        codec,
                        index: a.index,
                        copied: true,
                        channels: None,
                        bitrate: None,
                    }),
                    AudioAction::Encode {
                        codec,
                        channels,
                        bitrate,
                    } => Some(AudioSummary {
                        codec,
                        index: a.index,
                        copied: false,
                        channels: Some(channels),
                        bitrate: Some(bitrate),
                    }),
                    AudioAction::Unknown => None,
                });
                let subtitle = subtitle.map(|s| {
                    let (how, format) = match s.action {
                        SubtitleAction::Copy { format } => (SubtitleHow::Copy, Some(format)),
                        SubtitleAction::Convert { to, .. } => (SubtitleHow::Convert, Some(to)),
                        SubtitleAction::BurnIn { format } => (SubtitleHow::BurnIn, Some(format)),
                        SubtitleAction::Ocr { to, .. } => (SubtitleHow::Ocr, Some(to)),
                        SubtitleAction::Unknown => (SubtitleHow::Unknown, None),
                    };
                    SubtitleSummary {
                        index: s.index,
                        how,
                        format,
                    }
                });
                let target_height = self.encoded_height();
                let tone_mapped =
                    matches!(video, Some(ConvertedVideo::Encode { tone_map: true, .. }));
                let copies_video = video.is_none_or(|v| matches!(v, ConvertedVideo::Copy { .. }))
                    && ladder
                        .iter()
                        .all(|r| !matches!(r.video, Some(ConvertedVideo::Encode { .. })));
                let copies_audio = audio.is_none_or(|a| a.copied);
                let burns_subtitle = subtitle.is_some_and(|s| s.how == SubtitleHow::BurnIn);
                let kind = if copies_video && copies_audio && !burns_subtitle {
                    PlanKind::Remux
                } else {
                    PlanKind::Convert
                };
                PlanSummary {
                    kind,
                    delivery: Some(*delivery),
                    video: video_summary,
                    audio,
                    subtitle,
                    reasons: reasons.clone(),
                    target_height,
                    ladder_heights: ladder.iter().map(|r| r.height).collect(),
                    tone_mapped,
                }
            }
            PlaybackPlan::Unsupported { reasons } => PlanSummary {
                reasons: reasons.clone(),
                ..empty(PlanKind::Unsupported)
            },
            PlaybackPlan::Unknown => empty(PlanKind::Unknown),
        }
    }
}

fn empty(kind: PlanKind) -> PlanSummary {
    PlanSummary {
        kind,
        delivery: None,
        video: None,
        audio: None,
        subtitle: None,
        reasons: Vec::new(),
        target_height: None,
        ladder_heights: Vec::new(),
        tone_mapped: false,
    }
}

fn direct_subtitle(s: &PlannedSubtitle) -> SubtitleSummary {
    let how = match s.delivery {
        SubtitleDelivery::Embedded => SubtitleHow::Embedded,
        SubtitleDelivery::External => SubtitleHow::External,
        SubtitleDelivery::Unknown => SubtitleHow::Unknown,
    };
    SubtitleSummary {
        index: s.index,
        how,
        format: Some(s.format),
    }
}

fn converted_video(v: &ConvertedVideo) -> Option<VideoSummary> {
    match *v {
        ConvertedVideo::Copy {
            codec,
            dolby_vision,
        } => Some(VideoSummary {
            codec,
            copied: true,
            dolby_vision,
            max_bitrate: None,
        }),
        ConvertedVideo::Encode {
            codec, max_bitrate, ..
        } => Some(VideoSummary {
            codec,
            copied: false,
            dolby_vision: false,
            max_bitrate,
        }),
        ConvertedVideo::Unknown => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playback::{ConvertedAudio, ConvertedSubtitle, PlannedAudio, Rung};
    use spoolframe_types::Container;

    fn copy_video() -> ConvertedVideo {
        ConvertedVideo::Copy {
            codec: VideoCodec::Hevc,
            dolby_vision: true,
        }
    }

    fn encode_video(height: u32, tone_map: bool) -> ConvertedVideo {
        ConvertedVideo::Encode {
            codec: VideoCodec::H264,
            height,
            max_bitrate: Some(8_000_000),
            tone_map,
        }
    }

    fn copy_audio() -> ConvertedAudio {
        ConvertedAudio {
            index: 1,
            action: AudioAction::Copy {
                codec: AudioCodec::Aac,
            },
        }
    }

    fn converted(
        video: Option<ConvertedVideo>,
        audio: Option<ConvertedAudio>,
        subtitle: Option<SubtitleAction>,
        ladder: Vec<Rung>,
    ) -> PlaybackPlan {
        PlaybackPlan::Converted {
            delivery: Delivery::Hls,
            container: Container::Mp4,
            video,
            audio,
            subtitle: subtitle.map(|action| ConvertedSubtitle { index: 2, action }),
            ladder,
            reasons: vec!["audio 1: dts not supported".into()],
            source_codec: None,
        }
    }

    fn rung(height: u32, video: Option<ConvertedVideo>) -> Rung {
        Rung {
            height,
            max_bitrate: 1_000_000,
            video,
        }
    }

    /// A player reads a plan's parts without matching on it: a burned-in
    /// subtitle over an encode in a ladder.
    #[test]
    fn a_plan_s_parts_read_by_value() {
        let burn = SubtitleAction::BurnIn {
            format: SubtitleFormat::Pgs,
        };
        let ladder = vec![rung(1080, None), rung(720, None)];
        let plan = converted(
            Some(encode_video(1080, false)),
            Some(copy_audio()),
            Some(burn),
            ladder.clone(),
        );
        assert_eq!(
            (
                plan.kind(),
                plan.converted_video(),
                plan.audio_action(),
                plan.subtitle_action(),
                plan.burns_in_subtitle(),
                plan.ladder(),
            ),
            (
                PlanKind::Convert,
                Some(encode_video(1080, false)),
                Some(copy_audio().action),
                Some(burn),
                true,
                ladder.as_slice(),
            )
        );
    }

    #[test]
    fn a_direct_play_has_no_converted_parts() {
        let plan = PlaybackPlan::Direct {
            container: Container::Matroska,
            video: Some(VideoCodec::Hevc),
            audio: None,
            subtitle: None,
            source_codec: Some("hvc1.2.4.L153.B0".into()),
        };
        assert_eq!(
            (
                plan.kind(),
                plan.converted_video(),
                plan.subtitle_action(),
                plan.burns_in_subtitle(),
                plan.ladder().is_empty(),
                plan.source_codec(),
            ),
            (
                PlanKind::Direct,
                None,
                None,
                false,
                true,
                Some("hvc1.2.4.L153.B0"),
            )
        );
    }

    #[test]
    fn a_direct_play_is_the_files_own_streams() {
        let plan = PlaybackPlan::Direct {
            container: Container::Matroska,
            video: Some(VideoCodec::Hevc),
            audio: Some(PlannedAudio {
                index: 0,
                codec: AudioCodec::Eac3,
            }),
            subtitle: Some(PlannedSubtitle {
                index: 3,
                format: SubtitleFormat::Srt,
                delivery: SubtitleDelivery::External,
            }),
            source_codec: None,
        };
        let s = plan.summary();
        assert_eq!(s.kind, PlanKind::Direct);
        assert_eq!(s.delivery, None);
        assert!(
            s.video
                .is_some_and(|v| v.copied && v.codec == VideoCodec::Hevc)
        );
        assert_eq!(
            s.audio,
            Some(AudioSummary {
                codec: AudioCodec::Eac3,
                index: 0,
                copied: true,
                channels: None,
                bitrate: None,
            })
        );
        assert_eq!(
            s.subtitle,
            Some(SubtitleSummary {
                index: 3,
                how: SubtitleHow::External,
                format: Some(SubtitleFormat::Srt),
            })
        );
    }

    #[test]
    fn every_stream_copied_is_a_remux() {
        let plan = converted(
            Some(copy_video()),
            Some(copy_audio()),
            Some(SubtitleAction::Copy {
                format: SubtitleFormat::Srt,
            }),
            Vec::new(),
        );
        let s = plan.summary();
        assert_eq!(s.kind, PlanKind::Remux);
        assert_eq!(s.delivery, Some(Delivery::Hls));
        assert!(s.video.is_some_and(|v| v.copied && v.dolby_vision));
        assert_eq!(s.target_height, None);
    }

    #[test]
    fn audio_encoded_makes_it_a_conversion_with_its_channels_and_bitrate() {
        let audio = ConvertedAudio {
            index: 1,
            action: AudioAction::Encode {
                codec: AudioCodec::Aac,
                channels: 6,
                bitrate: 384_000,
            },
        };
        let s = converted(Some(copy_video()), Some(audio), None, Vec::new()).summary();
        assert_eq!(s.kind, PlanKind::Convert);
        assert_eq!(
            s.audio.map(|a| (a.copied, a.channels, a.bitrate)),
            Some((false, Some(6), Some(384_000)))
        );
    }

    #[test]
    fn an_encode_says_its_height_and_whether_it_maps_hdr() {
        let s = converted(Some(encode_video(1080, true)), None, None, Vec::new()).summary();
        assert_eq!(s.kind, PlanKind::Convert);
        assert_eq!(s.target_height, Some(1080));
        assert!(s.tone_mapped);
        assert!(
            s.video
                .is_some_and(|v| !v.copied && v.max_bitrate == Some(8_000_000))
        );
    }

    #[test]
    fn only_an_encode_has_a_height_of_its_own() {
        assert_eq!(
            converted(Some(encode_video(540, false)), None, None, vec![]).encoded_height(),
            Some(540)
        );
        assert_eq!(
            converted(Some(copy_video()), None, None, vec![]).encoded_height(),
            None
        );
        let direct = PlaybackPlan::Direct {
            container: Container::Matroska,
            video: Some(VideoCodec::Hevc),
            audio: None,
            subtitle: None,
            source_codec: None,
        };
        assert_eq!(direct.encoded_height(), None);
    }

    #[test]
    fn a_ladder_lists_its_heights_highest_first() {
        let ladder = vec![rung(1080, None), rung(720, None), rung(480, None)];
        let s = converted(Some(encode_video(1080, false)), None, None, ladder).summary();
        assert_eq!(s.ladder_heights, [1080, 720, 480]);
        assert!(!s.tone_mapped);
    }

    #[test]
    fn a_copy_with_encoded_rungs_under_it_is_not_a_remux() {
        let ladder = vec![
            rung(2160, None),
            rung(1080, Some(encode_video(1080, false))),
        ];
        let s = converted(Some(copy_video()), Some(copy_audio()), None, ladder).summary();
        assert_eq!(s.kind, PlanKind::Convert);
        assert_eq!(s.target_height, None);
    }

    #[test]
    fn a_subtitle_says_how_it_shows_and_in_what_format() {
        let cases = [
            (
                SubtitleAction::Convert {
                    from: SubtitleFormat::Srt,
                    to: SubtitleFormat::WebVtt,
                },
                SubtitleHow::Convert,
                SubtitleFormat::WebVtt,
            ),
            (
                SubtitleAction::Ocr {
                    from: SubtitleFormat::Pgs,
                    to: SubtitleFormat::Srt,
                },
                SubtitleHow::Ocr,
                SubtitleFormat::Srt,
            ),
            (
                SubtitleAction::BurnIn {
                    format: SubtitleFormat::Pgs,
                },
                SubtitleHow::BurnIn,
                SubtitleFormat::Pgs,
            ),
        ];
        for (action, how, format) in cases {
            let s = converted(Some(copy_video()), None, Some(action), Vec::new()).summary();
            assert_eq!(
                s.subtitle,
                Some(SubtitleSummary {
                    index: 2,
                    how,
                    format: Some(format),
                })
            );
            // Only a burn-in touches the picture.
            let kind = match how {
                SubtitleHow::BurnIn => PlanKind::Convert,
                _ => PlanKind::Remux,
            };
            assert_eq!(s.kind, kind, "{how:?}");
        }
    }

    #[test]
    fn a_conversion_keeps_its_reasons() {
        let s = converted(None, Some(copy_audio()), None, Vec::new()).summary();
        assert_eq!(s.reasons, ["audio 1: dts not supported"]);
        assert_eq!(s.video, None);
    }

    #[test]
    fn an_unsupported_plan_says_why() {
        let plan = PlaybackPlan::Unsupported {
            reasons: vec!["video: av1 not supported".into()],
        };
        let s = plan.summary();
        assert_eq!(s.kind, PlanKind::Unsupported);
        assert_eq!(s.reasons, ["video: av1 not supported"]);
        assert_eq!(PlaybackPlan::Unknown.summary().kind, PlanKind::Unknown);
    }
}
