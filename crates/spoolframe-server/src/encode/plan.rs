//! What a conversion's runs are planned as (P2-05, PX-21, PX-26): where
//! they cut and seek, a rung's video, and audio that must be encoded
//! for its `init.mp4`.

use std::time::Duration;

use spoolframe_media::MediaDescriptor;
use spoolframe_protocol::{AudioAction, ConvertedAudio, ConvertedVideo, Rung};
use spoolframe_types::AudioCodec;

use super::args::{Cuts, forced_through};
use super::hls::Segments;

use super::*;

/// Where a run of `run` cuts, in a conversion of `plan` cut at
/// `segments`:
///
/// - A remux ladder's lower rung (PX-21) where the copied original does,
///   at its cuts, so every variant has the same segments and AVPlayer can
///   move between them at any one. The encode forces a keyframe at each
///   and cuts there (`args`).
/// - A copied video at the conversion's list, by the segment muxer, where
///   [`Segments::copied`] made it.
/// - Anything else where its muxer does by itself: an encode on its own
///   grid, a copy at every keyframe, anything not HLS.
pub(super) fn keyframe_cuts(
    plan: Option<ConvertedVideo>,
    run: Option<ConvertedVideo>,
    segments: Option<&Segments>,
) -> Cuts {
    match (plan, run, segments) {
        (Some(ConvertedVideo::Copy { .. }), Some(ConvertedVideo::Encode { .. }), Some(s)) => {
            Cuts::Forced(s.shared_starts())
        }
        (Some(ConvertedVideo::Copy { .. }), Some(ConvertedVideo::Copy { .. }), Some(s)) => {
            match s.copy_from_ms() {
                Some(from_ms) => Cuts::Listed {
                    starts: s.shared_starts(),
                    from_ms,
                },
                None => Cuts::Own,
            }
        }
        _ => Cuts::Own,
    }
}

/// Whether ffmpeg's segment muxer can write a copied video's header, once,
/// before its first packet, as it does (review 2026-10-11 M2): only from a
/// codec record that holds the parameter sets
/// ([`spoolframe_media::VideoStream::parameter_sets_in_record`]), and not
/// with AC-3 or E-AC-3, copied or encoded, whose MP4 sample entry is made
/// from their first packets (ffmpeg refuses the header: "Cannot write
/// moov atom before AC3 packets"). The rest, checked with ffmpeg 8.1: AAC,
/// FLAC, ALAC, Opus and MP3. Any other conversion of a copied video is cut
/// by the HLS muxer at every keyframe, which writes its header after the
/// first packets.
pub(super) fn header_first(descriptor: &MediaDescriptor, audio: Option<ConvertedAudio>) -> bool {
    let record = descriptor
        .video
        .as_ref()
        .is_some_and(|v| v.parameter_sets_in_record);
    let sound = match audio.map(|a| a.action) {
        None => true,
        Some(AudioAction::Copy { codec } | AudioAction::Encode { codec, .. }) => matches!(
            codec,
            AudioCodec::Aac
                | AudioCodec::Flac
                | AudioCodec::Alac
                | AudioCodec::Opus
                | AudioCodec::Mp3
        ),
        Some(AudioAction::Unknown) => false,
    };
    record && sound
}

/// How long before a run's first cut an AC-3 or E-AC-3 track's first
/// packet must come to reach the muxer first: a copied video's keyframe
/// is handed over by its decode time, up to a few frames before it shows.
const LATE_AUDIO_MARGIN_MS: u64 = 250;

/// Whether the MP4 muxer writes `action`'s sample entry from its first
/// packet: AC-3 and E-AC-3 (`dac3`, `dec3`), copied or encoded.
pub fn sample_entry_from_packets(action: AudioAction) -> bool {
    matches!(
        action,
        AudioAction::Copy {
            codec: AudioCodec::Ac3 | AudioCodec::Eac3
        } | AudioAction::Encode {
            codec: AudioCodec::Ac3 | AudioCodec::Eac3,
            ..
        }
    )
}

/// Whether an HLS conversion's audio starts too late for its `init.mp4`:
/// an AC-3 or E-AC-3 track whose first packet, at `first_ms`, comes after
/// where a run from the start makes its first cut. The HLS muxer writes
/// `init.mp4` at that cut, its `moov` before any packet of the track
/// came, and the MP4 muxer cannot make the track's sample entry: the
/// `moov` says size 0, and no player opens it (Saw, 2004: an Italian dub
/// 6.2 s in, copied as AC-3, with keyframes every 0.9 s).
pub(super) fn late_audio(
    segments: Option<&Segments>,
    audio: Option<ConvertedAudio>,
    first_ms: Option<u64>,
) -> bool {
    let (Some(segments), Some(first_ms)) = (segments, first_ms) else {
        return false;
    };
    let cut = segments.end(1).unwrap_or(segments.total_ms);
    audio.is_some_and(|a| sample_entry_from_packets(a.action))
        && first_ms + LATE_AUDIO_MARGIN_MS >= cut
}

/// `audio` encoded: a copy as its own codec where the server encodes it,
/// else as AAC, which every HLS player plays and whose sample entry needs
/// no packet; at the bitrate the planner gives that encode. An encode as
/// it is. `None` where neither encodes here.
pub(super) fn encoded(
    descriptor: &MediaDescriptor,
    audio: ConvertedAudio,
    encoders: &spoolframe_media::encoders::Encoders,
) -> Option<ConvertedAudio> {
    let AudioAction::Copy { codec } = audio.action else {
        return Some(audio);
    };
    let codec = [codec, AudioCodec::Aac]
        .into_iter()
        .find(|c| encoders.audio_encoder(*c).is_some())?;
    let channels = descriptor
        .audio
        .get(audio.index as usize)
        .and_then(|a| a.channels)
        .unwrap_or(2)
        .clamp(1, 6);
    let bitrate = match (codec, channels > 2) {
        (AudioCodec::Aac, true) => 384_000,
        (_, true) => 640_000,
        _ => 192_000,
    };
    Some(ConvertedAudio {
        action: AudioAction::Encode {
            codec,
            channels,
            bitrate,
        },
        ..audio
    })
}

/// The video of `rung`: a ladder's rung at its own height and cap, the
/// rest of the encode as `video`'s; for one encode, `video`.
pub(super) fn rung_video(
    video: Option<ConvertedVideo>,
    ladder: &[Rung],
    rung: u32,
) -> Option<ConvertedVideo> {
    // A rung with a video of its own: an encode under a copied original.
    if let Some(own @ Some(_)) = ladder.get(rung as usize).map(|r| r.video) {
        return own;
    }
    match (video, ladder.get(rung as usize)) {
        (
            Some(ConvertedVideo::Encode {
                codec, tone_map, ..
            }),
            Some(r),
        ) => Some(ConvertedVideo::Encode {
            codec,
            height: r.height,
            max_bitrate: Some(r.max_bitrate),
            tone_map,
        }),
        _ => video,
    }
}

pub(super) fn playback_seek(
    segments: Option<&Segments>,
    start: Option<Duration>,
    video: Option<ConvertedVideo>,
) -> (Option<Duration>, u32) {
    match (segments, start) {
        (Some(segments), Some(at)) => {
            let n = segments
                .starts_ms()
                .partition_point(|&s| u128::from(s) <= at.as_millis())
                .max(1) as u32;
            run_at(segments, n, video)
        }
        (Some(_), None) => (None, 1),
        (None, start) => (start, 0),
    }
}

/// Where a run that is to write HLS segment `n` seeks, and the segment it
/// starts at: `n`, or 1 for a segment of an encoded video's head (PX-26),
/// which only a run from 0 cuts as the playlist says.
pub(super) fn run_at(
    segments: &Segments,
    n: u32,
    video: Option<ConvertedVideo>,
) -> (Option<Duration>, u32) {
    let from = segments.run_from(n);
    (segment_seek(segments, from, video), from)
}

/// Where to seek for segment `n`.
///
/// An encoded video seeks exactly: decoding lands on any frame. A copied
/// one can only start at a keyframe, and ffmpeg's seek lands on the
/// keyframe at or before where it is aimed, 3/23 s before that where the
/// frames are reordered (B-frames).
///
/// - Cut at the conversion's list by the segment muxer
///   ([`Segments::copy_from_ms`]): a millisecond before the cut (the
///   keyframe index rounds to the millisecond), and the run drops what is
///   before it (`-copypriorss 0`, `args`), so its first packet is the
///   cut's keyframe on any source, all-intra too (review 2026-10-11 M2).
/// - Cut by the HLS muxer at every keyframe: halfway to the next, which
///   lands on this one for any GOP over 0.4 s; aimed at the keyframe
///   itself it started a GOP early, and every number after it was off.
fn segment_seek(segments: &Segments, n: u32, video: Option<ConvertedVideo>) -> Option<Duration> {
    let start = segments.start(n)?;
    let ms = match video {
        Some(ConvertedVideo::Copy { .. }) if start > 0 && segments.copy_from_ms().is_some() => {
            start - 1
        }
        Some(ConvertedVideo::Copy { .. }) if start > 0 => {
            let next = segments.start(n + 1).unwrap_or(segments.total_ms);
            start + next.saturating_sub(start) / 2
        }
        _ => start,
    };
    (ms > 0).then(|| Duration::from_millis(ms))
}

/// The first segment a run from `start` (segment `from`) cut at `cuts`
/// does not cut where the playlist says: its list of keyframes to force,
/// or of the segment muxer's cuts, stops at what one argument holds
/// ([`forced_through`], [`args::listed_through`]), and segment `s` starts
/// at the last listed one, its end not given. `None` for a run given
/// every cut from `start` on, and for one cut by its muxer alone.
pub(super) fn aligned_until(cuts: &Cuts, start: Option<Duration>, from: u32) -> Option<u32> {
    let (cuts, last) = match cuts {
        Cuts::Own => return None,
        Cuts::Forced(cuts) => (cuts, forced_through(cuts, start)?),
        Cuts::Listed { starts, from_ms } => (starts, args::listed_through(starts, from, *from_ms)?),
    };
    Some(cuts.partition_point(|&ms| ms < last) as u32 + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spoolframe_types::VideoCodec;

    const COPY: ConvertedVideo = ConvertedVideo::Copy {
        codec: VideoCodec::H264,
        dolby_vision: false,
    };

    fn under(height: u32, cap: u64) -> Rung {
        Rung {
            height,
            max_bitrate: cap,
            video: Some(ConvertedVideo::Encode {
                codec: VideoCodec::H264,
                height,
                max_bitrate: Some(cap),
                tone_map: false,
            }),
        }
    }

    /// A remux ladder's (PX-21) top rung is the plan's copy; each rung
    /// under it is its own encode, cut at the copy's keyframes; the copy
    /// cuts by itself.
    #[test]
    fn a_remux_ladder_s_rungs_are_the_copy_and_encodes_cut_where_it_is() {
        let ladder = [
            Rung {
                height: 1080,
                max_bitrate: 8_000_000,
                video: None,
            },
            under(720, 4_000_000),
            under(480, 2_000_000),
        ];
        assert_eq!(rung_video(Some(COPY), &ladder, 0), Some(COPY));
        assert_eq!(rung_video(Some(COPY), &ladder, 2), ladder[2].video);
        let segments = Segments::at_keyframes(&[0, 1_001, 2_002], 3_000);
        let encoded = rung_video(Some(COPY), &ladder, 1);
        assert_eq!(
            keyframe_cuts(Some(COPY), encoded, Some(&segments)),
            Cuts::Forced(segments.shared_starts())
        );
        assert_eq!(
            keyframe_cuts(Some(COPY), Some(COPY), Some(&segments)),
            Cuts::Own
        );
        // A ladder of encodes is on its own grid.
        assert_eq!(keyframe_cuts(encoded, encoded, Some(&segments)), Cuts::Own);
    }

    /// Review 2026-10-11 M2: a copy cut at the conversion's own list is
    /// handed it, with where a run from the start begins; a rung under it
    /// forces its keyframes at the same cuts.
    #[test]
    fn a_listed_copy_s_runs_cut_at_the_conversion_s_list() {
        let keyframes = spoolframe_media::KeyframeTimes {
            times_ms: vec![0, 500, 3500, 3708, 4125, 6083, 9000],
            first_ms: -40,
        };
        let segments = Segments::copied(&keyframes, 12_000, 2);
        assert_eq!(
            keyframe_cuts(Some(COPY), Some(COPY), Some(&segments)),
            Cuts::Listed {
                starts: segments.shared_starts(),
                from_ms: -40
            }
        );
        let encoded = rung_video(Some(COPY), &[under(720, 4_000_000)], 0);
        assert_eq!(
            keyframe_cuts(Some(COPY), encoded, Some(&segments)),
            Cuts::Forced(segments.shared_starts())
        );
    }

    /// A two-hour H.264 film `height` lines high, its parameter sets only
    /// in the stream (as a probe that read no record says).
    fn film_of(height: u32) -> MediaDescriptor {
        MediaDescriptor {
            container: "matroska,webm".into(),
            duration: Some(Duration::from_secs(7200)),
            bit_rate: None,
            video: Some(spoolframe_media::VideoStream {
                index: 0,
                codec: "h264".into(),
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
                parameter_sets_in_record: false,
                codec_string: None,
            }),
            audio: Vec::new(),
            subtitles: Vec::new(),
        }
    }

    /// A film whose record holds its parameter sets.
    fn whole_record() -> MediaDescriptor {
        let mut film = film_of(1080);
        film.video.as_mut().unwrap().parameter_sets_in_record = true;
        film
    }

    fn sound(action: AudioAction) -> Option<ConvertedAudio> {
        Some(ConvertedAudio { index: 0, action })
    }

    /// The segment muxer writes the header before any packet: from the
    /// record, and not for AC-3 or E-AC-3, whose sample entry needs one.
    #[test]
    fn a_copy_is_cut_at_its_list_only_where_the_header_can_come_first() {
        let aac = AudioAction::Encode {
            codec: AudioCodec::Aac,
            channels: 2,
            bitrate: 192_000,
        };
        let eac3 = AudioAction::Copy {
            codec: AudioCodec::Eac3,
        };
        let ac3 = AudioAction::Encode {
            codec: AudioCodec::Ac3,
            channels: 6,
            bitrate: 640_000,
        };
        let flac = AudioAction::Copy {
            codec: AudioCodec::Flac,
        };
        assert!(header_first(&whole_record(), sound(aac)));
        assert!(header_first(&whole_record(), sound(flac)));
        assert!(header_first(&whole_record(), None));
        assert!(!header_first(&whole_record(), sound(eac3)));
        assert!(!header_first(&whole_record(), sound(ac3)));
        // The parameter sets only in the stream.
        assert!(!header_first(&film_of(1080), sound(aac)));
    }

    /// Only AC-3 and E-AC-3 that start at or past a run's first cut are
    /// encoded: one a few frames late is copied still.
    #[test]
    fn audio_is_late_only_past_the_first_cut_and_only_ac3() {
        let segments = Segments::at_keyframes(&[0, 2000, 4000], 6000);
        let ac3 = sound(AudioAction::Copy {
            codec: AudioCodec::Ac3,
        });
        let aac = sound(AudioAction::Copy {
            codec: AudioCodec::Aac,
        });
        assert!(late_audio(Some(&segments), ac3, Some(3000)));
        assert!(late_audio(Some(&segments), ac3, Some(1800)));
        assert!(!late_audio(Some(&segments), ac3, Some(40)));
        assert!(!late_audio(Some(&segments), aac, Some(3000)));
        assert!(!late_audio(Some(&segments), ac3, None));
        assert!(!late_audio(None, ac3, Some(3000)));
    }

    /// A late copy is encoded as its own codec, or as AAC where the server
    /// does not encode that; an encode is left as it is.
    #[test]
    fn a_late_copy_is_encoded_as_its_own_codec() {
        use spoolframe_media::encoders::{AudioEncoder, Encoders};
        let aac_only = Encoders::software_baseline();
        let with_eac3 = Encoders {
            audio: vec![AudioEncoder {
                codec: AudioCodec::Eac3,
                name: "eac3".into(),
            }],
            ..Encoders::software_baseline()
        };
        let none = Encoders {
            audio: Vec::new(),
            ..Encoders::software_baseline()
        };
        let mut film = whole_record();
        film.audio = vec![spoolframe_media::AudioStream {
            index: 1,
            codec: "eac3".into(),
            profile: None,
            channels: Some(8),
            channel_layout: None,
            language: None,
            title: None,
            default: true,
            role: spoolframe_types::AudioRole::Main,
        }];
        let copy = ConvertedAudio {
            index: 0,
            action: AudioAction::Copy {
                codec: AudioCodec::Eac3,
            },
        };
        let action = |encoders| encoded(&film, copy, encoders).map(|a| a.action);
        assert_eq!(
            action(&with_eac3),
            Some(AudioAction::Encode {
                codec: AudioCodec::Eac3,
                channels: 6,
                bitrate: 640_000,
            })
        );
        assert_eq!(
            action(&aac_only),
            Some(AudioAction::Encode {
                codec: AudioCodec::Aac,
                channels: 6,
                bitrate: 384_000,
            })
        );
        assert_eq!(action(&none), None);
        let stereo = ConvertedAudio {
            action: AudioAction::Encode {
                codec: AudioCodec::Ac3,
                channels: 2,
                bitrate: 192_000,
            },
            ..copy
        };
        assert_eq!(encoded(&film, stereo, &none), Some(stereo));
    }

    /// Review 2026-10-11 M2: a lower rung's run is handed the copy's cuts,
    /// one per source keyframe (172k for two hours of an all-intra film);
    /// it shares the conversion's list rather than copying it per run.
    #[test]
    fn a_rung_s_cuts_are_the_conversion_s_own_list() {
        let under = Rung {
            height: 720,
            max_bitrate: 4_000_000,
            video: Some(ConvertedVideo::Encode {
                codec: VideoCodec::H264,
                height: 720,
                max_bitrate: Some(4_000_000),
                tone_map: false,
            }),
        };
        let segments = Segments::at_keyframes(&[0, 1_001, 2_002], 3_000);
        let encoded = rung_video(Some(COPY), &[under], 0);
        let Cuts::Forced(cuts) = keyframe_cuts(Some(COPY), encoded, Some(&segments)) else {
            panic!("no cuts for a rung");
        };
        assert_eq!(cuts.as_ptr(), segments.starts_ms().as_ptr());
    }

    /// A run's first segment off the playlist's grid: past its last
    /// forced keyframe when its list stops short; none when the list holds
    /// every cut, or the run is not cut at given keyframes.
    #[test]
    fn a_run_knows_where_its_forced_keyframes_end() {
        // Keyed every 500 ms for two hours: more than one argument holds.
        let long: Vec<u64> = (0..14_400).map(|n| n * 500).collect();
        let last = args::forced_through(&long, None).expect("cut short");
        let forced = Cuts::Forced(long.clone().into());
        let s = aligned_until(&forced, None, 1).expect("cut short");
        // Segment `s` (from 1) starts at the last forced keyframe.
        assert_eq!(long[s as usize - 1], last);
        // From near the end the rest fits.
        assert_eq!(
            aligned_until(&forced, Some(Duration::from_secs(7_000)), 14_001),
            None
        );
        let short = Cuts::Forced(vec![0, 6_000, 12_000].into());
        assert_eq!(aligned_until(&short, None, 1), None);
        assert_eq!(aligned_until(&Cuts::Own, None, 1), None);
    }

    /// The segment muxer's list stops where one argument does too: 48
    /// hours cut every second.
    #[test]
    fn a_run_knows_where_its_listed_cuts_end() {
        let long: Vec<u64> = (0..172_800).map(|n| n * 1000).collect();
        let listed = Cuts::Listed {
            starts: long.clone().into(),
            from_ms: 0,
        };
        let s = aligned_until(&listed, None, 1).expect("cut short");
        let last = args::listed_through(&long, 1, 0).unwrap();
        assert_eq!(long[s as usize - 1], last);
        // A run from segment `s` is given the rest.
        let near_end = Some(Duration::from_millis(long[170_000]));
        assert_eq!(aligned_until(&listed, near_end, 170_001), None);
    }

    #[test]
    fn a_copied_restart_aims_halfway_into_the_segment() {
        let segments = Segments::at_keyframes(&[0, 2000, 4000], 6000);
        let copy = Some(ConvertedVideo::Copy {
            codec: spoolframe_types::VideoCodec::H264,
            dolby_vision: false,
        });
        assert_eq!(
            segment_seek(&segments, 2, copy),
            Some(Duration::from_millis(3000))
        );
        assert_eq!(
            segment_seek(&segments, 3, copy),
            Some(Duration::from_millis(5000))
        );
        assert_eq!(segment_seek(&segments, 1, copy), None);
        // Encoded: exactly at the start.
        let grid = Segments::grid(30_000);
        assert_eq!(segment_seek(&grid, 2, None), Some(Duration::from_secs(6)));
    }

    /// Review 2026-10-11 M2: a copy cut at its list starts exactly at its
    /// segment's keyframe. Aimed halfway to the next cut, the seek landed
    /// on whatever keyframe was before that: a later one wherever the
    /// source keys more often than it is cut (every frame, all-intra).
    /// Aimed a millisecond before the cut, it lands on the cut or before
    /// it (ffmpeg seeks 3/23 s early where frames are reordered), and what
    /// is before the cut is dropped (`-copypriorss 0`, `args`).
    #[test]
    fn a_listed_copy_restarts_just_before_its_cut() {
        let keyframes = spoolframe_media::KeyframeTimes {
            times_ms: (0..240).map(|n| n * 1001 / 24).collect(),
            first_ms: 0,
        };
        let segments = Segments::copied(&keyframes, 10_000, 2);
        let copy = Some(ConvertedVideo::Copy {
            codec: spoolframe_types::VideoCodec::H264,
            dolby_vision: false,
        });
        let cut = segments.start(3).unwrap();
        assert_eq!(
            segment_seek(&segments, 3, copy),
            Some(Duration::from_millis(cut - 1))
        );
        assert_eq!(segment_seek(&segments, 1, copy), None);
    }

    /// PX-26: a run for a segment of an encoded video's head starts from
    /// 0, the only run that cuts the head as listed; past it, a run starts
    /// at its segment's start.
    #[test]
    fn an_encoded_restart_in_the_head_starts_from_zero() {
        let encoded = Segments::encoded(60_000);
        let video = Some(ConvertedVideo::Encode {
            codec: spoolframe_types::VideoCodec::H264,
            height: 720,
            max_bitrate: None,
            tone_map: false,
        });
        assert_eq!(run_at(&encoded, 2, video), (None, 1));
        assert_eq!(run_at(&encoded, 3, video), (None, 1));
        assert_eq!(
            run_at(&encoded, 4, video),
            (Some(Duration::from_secs(8)), 4)
        );
        assert_eq!(
            run_at(&encoded, 5, video),
            (Some(Duration::from_secs(14)), 5)
        );
        // A position 5 s in is in segment 3, 4 to 8 s: written from 0.
        let at = Some(Duration::from_secs(5));
        assert_eq!(playback_seek(Some(&encoded), at, video), (None, 1));
        let at = Some(Duration::from_secs(13));
        assert_eq!(
            playback_seek(Some(&encoded), at, video),
            (Some(Duration::from_secs(8)), 4)
        );
    }
}
