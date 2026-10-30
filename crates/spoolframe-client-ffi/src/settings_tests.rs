//! The settings calls as an app makes them: what crosses is what the SDK
//! decides, in the FFI's records.

use super::*;
use spoolframe_client_sdk::types::{HdrFormat, SubtitleSync};

#[test]
fn a_kept_quality_reads_back_in_each_form_apps_kept() {
    let kept: Vec<_> = [
        "auto",
        "Original",
        "8000000",
        r#"{"Bitrate": 4000000}"#,
        "fast",
    ]
    .into_iter()
    .map(|k| quality_from_setting(k.into()))
    .collect();
    assert_eq!(
        kept,
        [
            Some(PlaybackQuality::Auto),
            Some(PlaybackQuality::Original),
            Some(PlaybackQuality::Bitrate(8_000_000)),
            Some(PlaybackQuality::Bitrate(4_000_000)),
            None,
        ]
    );
}

#[test]
fn a_quality_is_kept_in_the_form_it_reads_back_from() {
    let back: Vec<_> = quality_choices()
        .into_iter()
        .map(|q| quality_from_setting(quality_setting(q)))
        .collect();
    assert_eq!(
        back,
        quality_choices().into_iter().map(Some).collect::<Vec<_>>()
    );
}

#[test]
fn a_bitrate_the_picker_does_not_offer_is_shown_as_auto() {
    assert_eq!(quality_choice_index(PlaybackQuality::Bitrate(3_000_000)), 0);
}

#[test]
fn a_tv_box_away_from_home_starts_at_original() {
    assert_eq!(
        default_quality_for(None, Platform::Tvos, false),
        PlaybackQuality::Original
    );
}

#[test]
fn an_address_that_is_none_is_refused() {
    assert!(matches!(
        ips(&["fe80::1".into(), "home".into()]),
        Err(SpoolError::InvalidInput { .. })
    ));
}

#[test]
fn hdr_on_without_dolby_vision_is_hdr10_and_hlg() {
    assert_eq!(
        screen_for_hdr(true, false).hdr,
        Some(vec![HdrFormat::Hdr10, HdrFormat::Hlg])
    );
}

struct PlaysAvc;

impl CodecPlayer for PlaysAvc {
    fn plays(&self, codec: String) -> bool {
        codec.starts_with("avc1.")
    }
}

/// A player that names HEVC as decoded in hardware.
fn hevc_in_hardware() -> PlayerDecoders {
    PlayerDecoders {
        hardware_video: Some(vec![VideoCodec::Hevc]),
        ..PlayerDecoders::default()
    }
}

#[test]
fn the_probe_keeps_the_codec_strings_the_player_plays() {
    let probed = probe_capabilities(
        Arc::new(PlaysAvc),
        ScreenCapabilities::default(),
        hevc_in_hardware(),
    );
    let codecs = probed.video_codecs.unwrap();
    assert!(
        !codecs.is_empty() && codecs.iter().all(|c| c.starts_with("avc1.")),
        "{codecs:?}"
    );
}

#[test]
fn hevc_in_hardware_probes_as_ten_bit() {
    let probed = probe_capabilities(
        Arc::new(PlaysAvc),
        ScreenCapabilities::default(),
        hevc_in_hardware(),
    );
    assert_eq!(
        (
            probed.max_video.and_then(|v| v.bit_depth),
            probed.video.contains(&VideoCodec::Hevc)
        ),
        (Some(10), true)
    );
}

/// The probe is the decoders' limits: the screen's mode goes with each
/// plan (`SpoolClient.set_display`), not with the sign-in.
#[test]
fn the_screen_s_mode_is_not_the_probe_s_limit() {
    let probed = probe_capabilities(
        Arc::new(PlaysAvc),
        ScreenCapabilities {
            max_height: Some(1080),
            max_frame_rate: Some(60),
            ..ScreenCapabilities::default()
        },
        PlayerDecoders::default(),
    );
    let limits = probed.max_video.unwrap();
    assert_eq!((limits.height, limits.frame_rate), (None, None));
}

/// `None` is every HDR format and an empty list is none: a screen
/// without HDR must say the empty list, or the server sends it HDR.
#[test]
fn a_screen_without_hdr_says_none_rather_than_all() {
    let hdr = screen_for_hdr(false, false).hdr;
    assert_eq!(hdr, Some(vec![]));
}

#[test]
fn android_s_decoder_types_name_their_codecs() {
    assert_eq!(
        (
            video_codec_for_mime("video/av01".into()),
            audio_codec_for_mime("audio/true-hd".into()),
            audio_codec_for_mime("audio/x-unknown".into()),
        ),
        (Some(VideoCodec::Av1), Some(AudioCodec::TrueHd), None)
    );
}

#[test]
fn the_offset_control_stops_at_ten_minutes() {
    let timing = SubtitleTiming {
        sync: SubtitleSync::Original,
        offset_ms: 599_950,
        offset_by: None,
        offset_at: None,
    };
    assert_eq!(subtitle_offset_stepped(timing, 3), 600_000);
}

#[test]
fn a_long_name_is_cut_to_the_most_a_name_has() {
    let fitted = fit_name(format!("  {}  ", "n".repeat(100))).unwrap();
    assert_eq!(fitted.chars().count() as u32, max_name_chars());
}
