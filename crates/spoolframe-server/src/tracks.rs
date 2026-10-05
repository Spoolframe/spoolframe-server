//! Shared track policy; the server supplies persistence and probed tracks.
pub use spoolframe_policy::tracks::*;

#[cfg(test)]
use spoolframe_protocol::{Selection, TrackReason};
#[cfg(test)]
use spoolframe_types::preferences::ORIGINAL_LANGUAGE;
#[cfg(test)]
use spoolframe_types::{
    AudioRole, AudioTrack, MediaTracks, SdhPreference, SubtitleMode, SubtitleRole, SubtitleTrack,
    TrackPreferences,
};

#[cfg(test)]
mod tests {
    use spoolframe_types::{AudioPreferences, SubtitlePreferences};

    use super::*;

    /// Brooklyn Nine-Nine S07E01's tracks, from the recorded probe.
    fn b99() -> MediaTracks {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../spoolframe-media/tests/fixtures/b99-s07e01.json"
        );
        let probe = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        spoolframe_media::MediaDescriptor::from_probe(&probe).tracks()
    }

    fn english(mode: SubtitleMode, sdh: SdhPreference) -> TrackPreferences {
        TrackPreferences {
            audio: AudioPreferences {
                languages: vec!["eng".into()],
                ..AudioPreferences::default()
            },
            subtitles: SubtitlePreferences {
                mode,
                languages: vec!["eng".into()],
                sdh,
            },
        }
    }

    fn sdh_position(tracks: &MediaTracks) -> u32 {
        tracks
            .subtitles
            .iter()
            .position(|s| s.role == SubtitleRole::Sdh)
            .unwrap() as u32
    }

    #[test]
    fn b99_plays_the_main_english_track_not_the_description_nor_portuguese() {
        let tracks = b99();
        let chosen = choose(
            &tracks,
            &english(SubtitleMode::Off, SdhPreference::Accept),
            None,
        );
        // Stream 3: the third audio track, position 2.
        assert_eq!(chosen.selection.audio, Some(2));
        assert_eq!(chosen.audio, TrackReason::Preferences);
        assert_eq!(tracks.audio[2].channels, Some(6));
    }

    #[test]
    fn b99_with_default_preferences_still_avoids_the_description() {
        let chosen = choose(&b99(), &TrackPreferences::default(), None);
        assert_eq!(chosen.selection.audio, Some(2));
        assert_eq!(chosen.audio, TrackReason::FileDefault);
    }

    #[test]
    fn b99_always_english_shows_the_sdh_track_because_it_is_the_only_one() {
        let tracks = b99();
        let chosen = choose(
            &tracks,
            &english(SubtitleMode::Always, SdhPreference::Accept),
            None,
        );
        assert_eq!(chosen.selection.subtitle, Some(sdh_position(&tracks)));
        assert_eq!(chosen.subtitle, TrackReason::Preferences);
    }

    #[test]
    fn b99_avoiding_sdh_shows_none() {
        let chosen = choose(
            &b99(),
            &english(SubtitleMode::Always, SdhPreference::Avoid),
            None,
        );
        assert_eq!(chosen.selection.subtitle, None);
        assert_eq!(chosen.subtitle, TrackReason::NoneMatched);
    }

    #[test]
    fn foreign_only_shows_nothing_for_english_audio() {
        let chosen = choose(
            &b99(),
            &english(SubtitleMode::ForeignOnly, SdhPreference::Accept),
            None,
        );
        assert_eq!(chosen.selection.subtitle, None);
        assert_eq!(chosen.subtitle, TrackReason::NotWanted);
    }

    #[test]
    fn forced_only_follows_the_audio_language() {
        let tracks = b99();
        let mut german = TrackPreferences::default();
        german.audio.languages = vec!["deu".into()];
        let chosen = choose(&tracks, &german, None);
        let audio = &tracks.audio[chosen.selection.audio.unwrap() as usize];
        assert_eq!(audio.language.as_deref(), Some("ger"));
        let subtitle = &tracks.subtitles[chosen.selection.subtitle.unwrap() as usize];
        assert_eq!(
            (subtitle.language.as_deref(), subtitle.role),
            (Some("ger"), SubtitleRole::Forced)
        );
    }

    #[test]
    fn original_means_the_default_track_s_language() {
        let mut p = TrackPreferences::default();
        p.audio.languages = vec![ORIGINAL_LANGUAGE.into(), "por".into()];
        let chosen = choose(&b99(), &p, None);
        assert_eq!(chosen.selection.audio, Some(2));
        assert_eq!(chosen.audio, TrackReason::Preferences);
    }

    #[test]
    fn an_avoided_role_is_kept_when_it_is_all_there_is() {
        let tracks = MediaTracks {
            audio: vec![AudioTrack {
                language: Some("eng".into()),
                title: Some("Commentary".into()),
                channels: Some(2),
                default: false,
                role: AudioRole::Commentary,
            }],
            ..MediaTracks::default()
        };
        let chosen = choose(&tracks, &TrackPreferences::default(), None);
        assert_eq!(chosen.selection.audio, Some(0));
        assert_eq!(chosen.subtitle, TrackReason::NoTracks);
    }

    fn subtitle(language: &str, role: SubtitleRole) -> SubtitleTrack {
        SubtitleTrack {
            language: Some(language.into()),
            title: None,
            forced: role == SubtitleRole::Forced,
            default: false,
            external: false,
            role,
        }
    }

    #[test]
    fn a_series_override_follows_language_and_role_to_another_position() {
        let e01 = b99();
        let picked = Selection {
            audio: Some(2),
            subtitle: Some(sdh_position(&e01)),
        };
        let remembered = TrackOverride::from_selection(&e01, picked);

        // E05: the same tracks with one more subtitle in front.
        let mut e05 = e01.clone();
        e05.subtitles.insert(0, subtitle("ara", SubtitleRole::Full));
        let chosen = choose(&e05, &TrackPreferences::default(), Some(&remembered));
        assert_eq!(chosen.selection.subtitle, Some(sdh_position(&e05)));
        assert_ne!(chosen.selection.subtitle, picked.subtitle);
        assert_eq!(chosen.subtitle, TrackReason::Override);
        assert_eq!(chosen.selection.audio, Some(2));
        assert_eq!(chosen.audio, TrackReason::Override);
    }

    #[test]
    fn subtitles_turned_off_stay_off_for_the_series() {
        let e01 = b99();
        let remembered = TrackOverride::from_selection(
            &e01,
            Selection {
                audio: None,
                subtitle: None,
            },
        );
        let chosen = choose(
            &e01,
            &english(SubtitleMode::Always, SdhPreference::Accept),
            Some(&remembered),
        );
        assert_eq!(chosen.selection.subtitle, None);
        assert_eq!(chosen.subtitle, TrackReason::Override);
        // Audio was not picked, so it follows the preferences.
        assert_eq!(chosen.audio, TrackReason::Preferences);
    }

    #[test]
    fn an_override_with_no_match_falls_back_to_the_preferences() {
        let remembered = TrackOverride {
            audio: Some(AudioKey {
                language: Some("kor".into()),
                role: AudioRole::Main,
            }),
            subtitle: None,
        };
        let chosen = choose(&b99(), &TrackPreferences::default(), Some(&remembered));
        assert_eq!(chosen.selection.audio, Some(2));
        assert_eq!(chosen.audio, TrackReason::FileDefault);
    }

    #[test]
    fn subtitle_languages_match_across_spellings() {
        let tracks = MediaTracks {
            subtitles: vec![
                subtitle("fr", SubtitleRole::Full),
                subtitle("en", SubtitleRole::Full),
            ],
            ..MediaTracks::default()
        };
        let chosen = choose(
            &tracks,
            &english(SubtitleMode::Always, SdhPreference::Accept),
            None,
        );
        assert_eq!(chosen.selection.subtitle, Some(1));
    }

    #[test]
    fn sdh_preferred_comes_before_full() {
        let tracks = MediaTracks {
            subtitles: vec![
                subtitle("eng", SubtitleRole::Full),
                subtitle("eng", SubtitleRole::Sdh),
                subtitle("eng", SubtitleRole::Commentary),
            ],
            ..MediaTracks::default()
        };
        let pick = |sdh| {
            choose(&tracks, &english(SubtitleMode::Always, sdh), None)
                .selection
                .subtitle
        };
        assert_eq!(pick(SdhPreference::Prefer), Some(1));
        assert_eq!(pick(SdhPreference::Accept), Some(0));
        assert_eq!(pick(SdhPreference::Avoid), Some(0));
    }
}
