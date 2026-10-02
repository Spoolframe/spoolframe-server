//! The playback planner (ARCHITECTURE §6, §8): from what a file is, what
//! the client can decode and which tracks it picked, how it will play.
//!
//! Phase 1 plans only `Direct` play, or says why not. Remux and transcode
//! (P2) add plans here without changing the protocol.

use spoolframe_media::{MediaDescriptor, SubtitleSource, codecs};
use spoolframe_protocol::{
    PlannedAudio, PlannedSubtitle, PlaybackPlan, Selection, SubtitleDelivery,
};
use spoolframe_types::{AudioCodec, ClientCapabilities, Container, SubtitleFormat, VideoCodec};

/// Plan `descriptor` for a client with `capabilities` (`None`: it sent
/// none) and `selection`. Only the container, the video and the selected
/// tracks count.
pub fn plan(
    descriptor: &MediaDescriptor,
    capabilities: Option<&ClientCapabilities>,
    selection: Selection,
) -> PlaybackPlan {
    let Some(caps) = capabilities else {
        return plan_without_capabilities();
    };
    let mut reasons = Vec::new();

    let container = codecs::container(&descriptor.container);
    if !supported(&caps.containers, container, Container::Unknown) {
        reasons.push(format!(
            "container: {} not supported",
            name(container.as_str(), &descriptor.container)
        ));
    }

    let video = descriptor.video.as_ref().map(|v| {
        let codec = codecs::video_codec(&v.codec);
        if !supported(&caps.video, codec, VideoCodec::Unknown) {
            reasons.push(format!(
                "video: {} not supported",
                name(codec.as_str(), &v.codec)
            ));
        }
        codec
    });

    let audio = match audio_index(descriptor, selection.audio) {
        Ok(None) => None,
        Ok(Some(index)) => {
            let stream = &descriptor.audio[index];
            let codec = codecs::audio_codec(&stream.codec);
            if !supported(&caps.audio, codec, AudioCodec::Unknown) {
                reasons.push(format!(
                    "audio {index}: {} not supported",
                    name(codec.as_str(), &stream.codec)
                ));
            }
            Some(PlannedAudio {
                index: index as u32,
                codec,
            })
        }
        Err(reason) => {
            reasons.push(reason);
            None
        }
    };

    let subtitle = selection.subtitle.and_then(|picked| {
        let Some(stream) = descriptor.subtitles.get(picked as usize) else {
            reasons.push(format!("subtitle {picked}: no such track"));
            return None;
        };
        let format = codecs::subtitle_format(&stream.codec);
        if !supported(&caps.subtitles, format, SubtitleFormat::Unknown) {
            reasons.push(format!(
                "subtitle {picked}: {} not supported",
                name(format.as_str(), &stream.codec)
            ));
        }
        let delivery = match stream.source {
            SubtitleSource::Embedded { .. } => SubtitleDelivery::Embedded,
            SubtitleSource::External { .. } => SubtitleDelivery::External,
        };
        Some(PlannedSubtitle {
            index: picked,
            format,
            delivery,
        })
    });

    if reasons.is_empty() {
        PlaybackPlan::Direct {
            container,
            video,
            audio,
            subtitle,
        }
    } else {
        PlaybackPlan::Unsupported { reasons }
    }
}

/// The plan for a client that sent no capabilities: nothing can be
/// promised to play.
pub fn plan_without_capabilities() -> PlaybackPlan {
    PlaybackPlan::Unsupported {
        reasons: vec!["the client sent no capabilities".into()],
    }
}

/// Unknown is never supported, whatever the client claims.
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

    use super::*;

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
                width: 1920,
                height: 1080,
                pixel_format: None,
                color_transfer: None,
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
}
