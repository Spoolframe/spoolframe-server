//! Which copy plays (PX-37), planned by the planner itself, for a device
//! that plays H.264 and AC-3 in Matroska by range, or progressive.

use std::path::PathBuf;
use std::time::Duration;

use spoolframe_media::encoders::Encoders;
use spoolframe_media::ocr::Ocr;
use spoolframe_media::{AudioStream, SubtitleKind, SubtitleStream};
use spoolframe_protocol::{PlaybackPlan, Quality};
use spoolframe_types::{
    AudioCodec, AudioRole, BitmapPreference, ClientCapabilities, Container, Delivery, HdrFormat,
    SubtitleFormat, SubtitleRole, VideoCodec, VideoLimits,
};

use super::*;
use crate::planner::{Bitmap, plan_for};

fn video(index: u32, codec: &str, height: u32) -> VideoStream {
    VideoStream {
        index,
        codec: codec.into(),
        profile: None,
        level: None,
        width: height * 16 / 9,
        height,
        pixel_format: None,
        color_transfer: None,
        frame_rate_millis: Some(23_976),
        bit_depth: 8,
        hdr: None,
        dolby_vision: None,
        bit_rate: None,
        parameter_sets_in_record: false,
    }
}

fn audio(index: u32, codec: &str) -> AudioStream {
    AudioStream {
        index,
        codec: codec.into(),
        profile: None,
        channels: Some(2),
        channel_layout: None,
        language: Some("eng".into()),
        title: None,
        default: true,
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

fn beside() -> SubtitleStream {
    subtitle(
        SubtitleSource::External {
            path: PathBuf::from("/media/Alien.en.srt"),
        },
        "subrip",
    )
}

/// A 1080p H.264 film at 20 Mbit/s with AC-3, an embedded SubRip and
/// one beside it.
fn film() -> MediaDescriptor {
    MediaDescriptor {
        container: "matroska,webm".into(),
        duration: Some(Duration::from_secs(7000)),
        bit_rate: Some(20_000_000),
        video: Some(video(0, "h264", 1080)),
        audio: vec![audio(1, "ac3")],
        subtitles: vec![
            subtitle(SubtitleSource::Embedded { index: 2 }, "subrip"),
            beside(),
        ],
    }
}

/// A copy of [`film`] `height` lines tall at `bit_rate`, as the copy's
/// probe describes it: no file beside it.
fn copy(height: u32, bit_rate: u64) -> MediaDescriptor {
    MediaDescriptor {
        container: "matroska,webm".into(),
        duration: Some(Duration::from_secs(7000)),
        bit_rate: Some(bit_rate),
        video: Some(video(0, "h264", height)),
        audio: vec![audio(1, "ac3")],
        subtitles: vec![subtitle(SubtitleSource::Embedded { index: 2 }, "subrip")],
    }
}

fn caps() -> ClientCapabilities {
    ClientCapabilities {
        video: [VideoCodec::H264].into(),
        audio: [AudioCodec::Ac3, AudioCodec::Aac].into(),
        containers: [Container::Matroska].into(),
        subtitles: [SubtitleFormat::Srt].into(),
        delivery: vec![Delivery::Range, Delivery::Progressive],
        ..ClientCapabilities::default()
    }
}

/// AVPlayer: HLS alone.
fn hls_caps() -> ClientCapabilities {
    ClientCapabilities {
        video: [VideoCodec::H264].into(),
        audio: [AudioCodec::Aac].into(),
        containers: [Container::Mp4, Container::MpegTs].into(),
        subtitles: [SubtitleFormat::WebVtt].into(),
        delivery: vec![Delivery::Hls],
        ..ClientCapabilities::default()
    }
}

const CAPPED: Quality = Quality::Bitrate {
    max_bitrate: 6_000_000,
};

fn planner(
    caps: ClientCapabilities,
    quality: Quality,
) -> impl Fn(&MediaDescriptor, Selection) -> PlaybackPlan {
    move |d, selection| {
        plan_for(
            d,
            Some(&caps),
            quality,
            None,
            false,
            selection,
            Bitmap::Preferred(BitmapPreference::Text),
            &Encoders::software_baseline(),
            &Ocr::disabled(),
        )
    }
}

/// What plays `film` with `selection` among `copies`, for `caps` at
/// `quality`.
fn chosen_with(
    film: &MediaDescriptor,
    copies: &[MediaDescriptor],
    selection: Selection,
    caps: ClientCapabilities,
    quality: Quality,
) -> Choice {
    let replan = planner(caps, quality);
    let plan = replan(film, selection);
    let copies: Vec<&MediaDescriptor> = copies.iter().collect();
    choose(film, &plan, selection, &copies, replan)
}

fn chosen(copies: &[MediaDescriptor]) -> Choice {
    chosen_with(&film(), copies, Selection::default(), caps(), CAPPED)
}

/// The copy played in the film's place, and how: `None` for none.
fn played(choice: &Choice) -> Option<(usize, Kind)> {
    match choice {
        Choice::Copy { copy, plan, .. } => Some((*copy, Kind::of(plan))),
        _ => None,
    }
}

#[test]
fn the_height_next_under_1080_is_720() {
    assert_eq!(height_below(1080), 720);
}

#[test]
fn a_copy_under_the_cap_plays_directly_in_place_of_an_encode() {
    assert_eq!(
        played(&chosen(&[copy(1080, 4_500_000)])),
        Some((0, Kind::Direct))
    );
}

#[test]
fn a_copy_remuxes_for_a_device_that_reads_no_range() {
    let caps = ClientCapabilities {
        delivery: vec![Delivery::Progressive],
        ..caps()
    };
    let choice = chosen_with(
        &film(),
        &[copy(1080, 4_500_000)],
        Selection::default(),
        caps,
        CAPPED,
    );
    assert_eq!(played(&choice), Some((0, Kind::Remux)));
}

#[test]
fn with_no_copy_the_film_plays_as_planned() {
    assert_eq!(chosen(&[]), Choice::Source);
}

#[test]
fn a_copy_over_the_quality_s_cap_is_not_played() {
    assert_eq!(chosen(&[copy(1080, 8_000_000)]), Choice::Source);
}

#[test]
fn a_copy_taller_than_the_device_takes_is_not_played() {
    let caps = ClientCapabilities {
        max_video: Some(VideoLimits {
            height: Some(720),
            ..VideoLimits::default()
        }),
        ..caps()
    };
    let choice = chosen_with(
        &film(),
        &[copy(1080, 4_500_000)],
        Selection::default(),
        caps,
        CAPPED,
    );
    assert_eq!(choice, Choice::Source);
}

#[test]
fn a_copy_well_under_the_encode_s_height_is_not_played() {
    assert_eq!(chosen(&[copy(480, 1_000_000)]), Choice::Source);
}

#[test]
fn of_the_copies_that_fit_the_tallest_plays() {
    let copies = [copy(720, 2_500_000), copy(1080, 4_500_000)];
    assert_eq!(played(&chosen(&copies)), Some((1, Kind::Direct)));
}

/// A 2160p HDR10 HEVC film at 40 Mbit/s: an H.264 device gets it tone
/// mapped.
fn hdr_film() -> MediaDescriptor {
    let mut film = film();
    let mut picture = video(0, "hevc", 2160);
    picture.hdr = Some(HdrFormat::Hdr10);
    picture.bit_depth = 10;
    film.video = Some(picture);
    film.bit_rate = Some(40_000_000);
    film
}

#[test]
fn an_hdr_copy_is_not_played_to_a_device_that_shows_no_hdr() {
    let mut hdr = copy(1080, 4_500_000);
    if let Some(v) = hdr.video.as_mut() {
        v.hdr = Some(HdrFormat::Hdr10);
    }
    let choice = chosen_with(&hdr_film(), &[hdr], Selection::default(), caps(), CAPPED);
    assert_eq!(choice, Choice::Source);
}

#[test]
fn a_tone_mapped_copy_plays_to_a_device_that_shows_no_hdr() {
    let choice = chosen_with(
        &hdr_film(),
        &[copy(1080, 4_500_000)],
        Selection::default(),
        caps(),
        CAPPED,
    );
    assert_eq!(played(&choice), Some((0, Kind::Direct)));
}

#[test]
fn auto_over_hls_stays_a_ladder() {
    let auto = Quality::Auto {
        link_bitrate: Some(6_000_000),
    };
    let mut aac = copy(1080, 4_500_000);
    aac.audio = vec![audio(1, "aac")];
    let mut film = film();
    film.audio = vec![audio(1, "aac")];
    let choice = chosen_with(&film, &[aac], Selection::default(), hls_caps(), auto);
    assert_eq!(choice, Choice::Source);
}

#[test]
fn a_copy_whose_audio_is_in_another_language_does_not_line_up() {
    let mut french = copy(1080, 4_500_000);
    french.audio[0].language = Some("fre".into());
    assert!(aligned(&film(), &french).is_none());
}

/// [`film`] with a teletext track first, which copies leave out.
fn film_with_teletext() -> MediaDescriptor {
    let mut film = film();
    film.subtitles.insert(
        0,
        subtitle(SubtitleSource::Embedded { index: 3 }, "dvb_teletext"),
    );
    film
}

#[test]
fn a_picked_track_the_copy_left_out_plays_from_the_film() {
    let picked = Selection {
        audio: None,
        subtitle: Some(0),
    };
    let choice = chosen_with(
        &film_with_teletext(),
        &[copy(1080, 4_500_000)],
        picked,
        caps(),
        CAPPED,
    );
    assert_eq!(choice, Choice::Source);
}

#[test]
fn a_direct_copy_whose_picked_subtitle_moved_in_the_file_is_not_played() {
    let picked = Selection {
        audio: None,
        subtitle: Some(1),
    };
    let choice = chosen_with(
        &film_with_teletext(),
        &[copy(1080, 4_500_000)],
        picked,
        caps(),
        CAPPED,
    );
    assert_eq!(choice, Choice::Source);
}

#[test]
fn a_subtitle_file_beside_the_film_plays_beside_its_copy() {
    let picked = Selection {
        audio: None,
        subtitle: Some(1),
    };
    let choice = chosen_with(&film(), &[copy(1080, 4_500_000)], picked, caps(), CAPPED);
    assert_eq!(played(&choice), Some((0, Kind::Direct)));
}

/// [`film`] with DTS, which the device does not play.
fn dts_film() -> MediaDescriptor {
    let mut film = film();
    film.audio = vec![audio(1, "dts")];
    film
}

/// A copy too short to stand in for the video, its DTS made AAC.
fn short_aac_copy() -> MediaDescriptor {
    let mut short = copy(240, 400_000);
    short.audio = vec![audio(1, "aac")];
    short
}

/// The audio a choice takes from a copy, and the plan's action for it.
fn copy_audio(choice: &Choice) -> Option<AudioAction> {
    match choice {
        Choice::Audio {
            plan: PlaybackPlan::Converted { audio, .. },
            ..
        } => audio.map(|a| a.action),
        _ => None,
    }
}

#[test]
fn a_copy_s_converted_audio_takes_the_place_of_an_audio_encode() {
    let choice = chosen_with(
        &dts_film(),
        &[short_aac_copy()],
        Selection::default(),
        caps(),
        CAPPED,
    );
    assert_eq!(
        copy_audio(&choice),
        Some(AudioAction::Copy {
            codec: AudioCodec::Aac
        })
    );
}

#[test]
fn with_a_copy_s_audio_the_film_s_video_is_encoded() {
    let choice = chosen_with(
        &dts_film(),
        &[short_aac_copy()],
        Selection::default(),
        caps(),
        CAPPED,
    );
    let Choice::Audio {
        plan: PlaybackPlan::Converted { video, .. },
        ..
    } = choice
    else {
        panic!("not the copy's audio: {choice:?}");
    };
    assert!(matches!(
        video,
        Some(ConvertedVideo::Encode { height: 1080, .. })
    ));
}

#[test]
fn a_ladder_takes_its_audio_from_a_copy() {
    let auto = Quality::Auto {
        link_bitrate: Some(6_000_000),
    };
    let choice = chosen_with(
        &dts_film(),
        &[short_aac_copy()],
        Selection::default(),
        hls_caps(),
        auto,
    );
    let Choice::Audio {
        plan: PlaybackPlan::Converted { ladder, .. },
        ..
    } = choice
    else {
        panic!("not the copy's audio: {choice:?}");
    };
    assert!(ladder.len() > 1, "{ladder:?}");
}
