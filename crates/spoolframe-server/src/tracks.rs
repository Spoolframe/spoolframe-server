//! Shared track policy; the server supplies persistence and probed tracks.
pub use spoolframe_policy::tracks::*;

use spoolframe_catalog::{Catalog, CatalogError};
use spoolframe_types::{Item, MediaId, MediaKind};

/// What a track override for `id` applies to: its show for an episode,
/// so a pick carries to the rest of the series; else `id` itself.
pub async fn override_scope(catalog: &dyn Catalog, id: &MediaId) -> Result<String, CatalogError> {
    if id.kind() != MediaKind::Episode {
        return Ok(id.to_string());
    }
    let found = catalog.item(id).await?;
    Ok(match found.item {
        Item::Episode(e) => e.show_id.to_string(),
        _ => id.to_string(),
    })
}

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
    use std::path::PathBuf;

    use spoolframe_types::{Container, VideoCodec, VideoTrack};

    use crate::principal::Principal;
    use crate::store::tests::test_store;
    use crate::store::users::LocalUserId;
    use crate::test_support::{FakeCatalog, episode};

    const ALICE: Principal = Principal::LocalUser(LocalUserId::from_uuid(uuid::Uuid::from_u128(1)));

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

    /// The B99 file's container and picture, as a client's track list
    /// says them.
    #[test]
    fn the_tracks_say_what_the_file_and_its_picture_are() {
        let tracks = b99();
        assert_eq!(tracks.container, Some(Container::Matroska));
        assert_eq!(
            tracks.video,
            Some(VideoTrack {
                codec: VideoCodec::H264,
                width: 1920,
                height: 1080,
                bit_depth: 8,
                hdr: None,
            })
        );
    }

    /// A user who never set preferences has the defaults: the main audio,
    /// not the description, and no subtitle (forced only, and B99 has no
    /// English forced one).
    #[tokio::test]
    async fn a_new_user_gets_the_main_audio_and_no_description() {
        let store = test_store("tracks-defaults").await;
        let preferences = store.track_preferences(ALICE).await.unwrap();
        let chosen = choose(&b99(), &preferences, None);
        assert_eq!(chosen.selection.audio, Some(2));
        assert_eq!(chosen.audio, TrackReason::FileDefault);
        assert_eq!(chosen.selection.subtitle, None);
        assert_eq!(chosen.subtitle, TrackReason::NoneMatched);
    }

    /// A pick on episode 12 is kept for its show, so episode 13 (the same
    /// tracks) is chosen by it, until it is cleared from either episode.
    #[tokio::test]
    async fn a_pick_on_one_episode_carries_to_the_next_until_cleared() {
        let store = test_store("tracks-override").await;
        let catalog = FakeCatalog::new(PathBuf::from("/nonexistent/movie.mkv"));
        let (e12, e13) = (episode("12", "11", 1, 1), episode("13", "11", 1, 2));
        let (e12, e13) = (e12.id(), e13.id());
        let always = english(SubtitleMode::Always, SdhPreference::Accept);
        store.set_track_preferences(ALICE, &always).await.unwrap();
        // On episode 12 the viewer turns subtitles off.
        let picked = Selection {
            audio: Some(2),
            subtitle: None,
        };
        let scope = override_scope(&catalog, e12).await.unwrap();
        let remembered = TrackOverride::from_selection(&b99(), picked);
        store
            .set_track_override(ALICE, &scope, &remembered)
            .await
            .unwrap();

        let chosen_for = async |id| {
            let scope = override_scope(&catalog, id).await.unwrap();
            let preferences = store.track_preferences(ALICE).await.unwrap();
            let remembered = store.track_override(ALICE, &scope).await.unwrap();
            choose(&b99(), &preferences, remembered.as_ref())
        };
        let next = chosen_for(e13).await;
        assert_eq!(next.selection.subtitle, None);
        assert_eq!(next.subtitle, TrackReason::Override);
        assert_eq!(next.audio, TrackReason::Override);

        let scope = override_scope(&catalog, e13).await.unwrap();
        store.clear_track_override(ALICE, &scope).await.unwrap();
        assert_eq!(chosen_for(e13).await.subtitle, TrackReason::Preferences);
    }

    #[tokio::test]
    async fn an_override_of_anything_but_an_episode_is_its_own() {
        let catalog = FakeCatalog::new(PathBuf::from("/nonexistent/movie.mkv"));
        let movie = crate::test_support::movie();
        assert_eq!(
            override_scope(&catalog, movie.id()).await.unwrap(),
            movie.id().to_string()
        );
        let e15 = episode("15", "14", 2, 1);
        assert_eq!(
            override_scope(&catalog, e15.id()).await.unwrap(),
            "plex:show:10"
        );
    }
}
