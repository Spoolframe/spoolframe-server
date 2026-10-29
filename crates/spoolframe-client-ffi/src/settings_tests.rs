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

#[test]
fn the_probe_keeps_the_codec_strings_the_player_plays() {
    let probed = probe_capabilities(
        Arc::new(PlaysAvc),
        vec![VideoCodec::Hevc],
        ScreenCapabilities::default(),
        PlayerDecoders::default(),
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
        vec![VideoCodec::Hevc],
        ScreenCapabilities::default(),
        PlayerDecoders::default(),
    );
    assert_eq!(probed.max_video.and_then(|v| v.bit_depth), Some(10));
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
