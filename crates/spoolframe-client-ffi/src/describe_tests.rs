//! The describe calls as an app makes them: the SDK's parts, in the
//! FFI's records where they differ.

use super::*;
use spoolframe_client_sdk::types::{AudioRole, Counted, ItemFilterValues};

use crate::values::FilterValues;

fn english(title: Option<&str>) -> AudioTrack {
    AudioTrack {
        language: Some("eng".into()),
        title: title.map(str::to_owned),
        channels: Some(2),
        default: false,
        role: AudioRole::Main,
    }
}

#[test]
fn a_language_with_a_region_names_both() {
    let pt = language("pt-BR".into());
    assert_eq!(
        (
            pt.iso639_1.as_deref(),
            pt.region.as_deref(),
            pt.english.as_deref()
        ),
        (Some("pt"), Some("BR"), Some("Portuguese"))
    );
}

#[test]
fn lossless_audio_says_its_depth_and_rate_apart() {
    let flac = audio_format(Some(AudioCodec::Flac), Some(24), Some(96_000)).unwrap();
    assert_eq!((flac.bit_depth, flac.sample_rate), (Some(24), Some(96_000)));
}

#[test]
fn a_repeated_label_in_the_menu_is_numbered_from_two() {
    let tracks = MediaTracks {
        audio: vec![english(None), english(None)],
        ..MediaTracks::default()
    };
    let ordinals: Vec<_> = track_menu(tracks, None)
        .audio
        .into_iter()
        .map(|r| r.ordinal)
        .collect();
    assert_eq!(ordinals, [None, Some(2)]);
}

#[test]
fn a_newer_server_asks_the_app_to_update() {
    let error = SpoolError::UpdateRequired {
        server_protocol: Some(spoolframe_protocol::PROTOCOL_VERSION + 1),
    };
    assert_eq!(error.who_updates(), Some(UpdateNeeded::App));
}

#[test]
fn only_an_update_error_names_who_updates() {
    assert_eq!(SpoolError::SignInRequired.who_updates(), None);
}

#[test]
fn a_running_show_reads_open_ended() {
    assert_eq!(years_aired_text(YearsAired::Running(2019)), "2019\u{2013}");
}

#[test]
fn a_librarys_filter_values_carry_its_decades_newest_first() {
    let values = ItemFilterValues {
        years: [1994, 2021, 1999, 2003]
            .into_iter()
            .map(|value| Counted { value, count: None })
            .collect(),
        ..ItemFilterValues::default()
    };
    assert_eq!(FilterValues::from(values).decades, [2020, 2000, 1990]);
}

/// An item read through Swift's and Kotlin's accessors, whatever kind it
/// is: what a list keys its rows by and shows.
#[test]
fn an_item_s_accessors_read_it_whatever_its_kind() {
    let film = crate::test_fixtures::arrival();
    assert_eq!(
        (
            film.id_owned().to_string(),
            film.title_owned(),
            film.artwork_owned().len(),
            film.duration(),
            film.playback_owned().and_then(|p| p.position),
        ),
        (
            "plex:movie:1".to_owned(),
            "Arrival".to_owned(),
            1,
            Some(std::time::Duration::from_secs(6_000)),
            Some(std::time::Duration::from_secs(60)),
        )
    );
}

#[test]
fn a_quota_says_whether_it_is_the_user_s_own() {
    use spoolframe_client_sdk::types::QuotaOf;
    assert_eq!(
        [QuotaOf::User, QuotaOf::Account, QuotaOf::Unknown].map(|q| q.is_user()),
        [true, false, false]
    );
}
