//! Choosing tracks (P1-20): when a client asks to play without a
//! [`Selection`], the server picks the audio and subtitle tracks from the
//! viewer's preferences, or from what they picked for this series or movie.
//!
//! Choices are by language and role, never by position: Brooklyn Nine-Nine
//! season 7 has its English SDH subtitle at a different position from one
//! episode to the next.

use serde::{Deserialize, Serialize};
use spoolframe_protocol::{ChosenTracks, Selection, TrackReason};
use spoolframe_types::language::same_language;
use spoolframe_types::preferences::ORIGINAL_LANGUAGE;
use spoolframe_types::{
    AudioRole, AudioTrack, MediaTracks, SdhPreference, SubtitleMode, SubtitleRole, SubtitleTrack,
    TrackPreferences,
};

/// What a viewer picked for a series or a movie, by what the tracks are.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackOverride {
    /// `None`: audio follows the preferences.
    pub audio: Option<AudioKey>,
    /// `None`: subtitles follow the preferences.
    pub subtitle: Option<SubtitleChoice>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioKey {
    pub language: Option<String>,
    pub role: AudioRole,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SubtitleChoice {
    Track {
        language: Option<String>,
        role: SubtitleRole,
    },
    Off,
}

impl TrackOverride {
    /// The override for picking `selection` in `tracks`: each track by its
    /// language and role. No subtitle picked means "subtitles off".
    pub fn from_selection(tracks: &MediaTracks, selection: Selection) -> Self {
        let audio = selection
            .audio
            .and_then(|i| tracks.audio.get(i as usize))
            .map(|a| AudioKey {
                language: a.language.clone(),
                role: a.role,
            });
        let subtitle = match selection.subtitle {
            None => Some(SubtitleChoice::Off),
            Some(i) => tracks
                .subtitles
                .get(i as usize)
                .map(|s| SubtitleChoice::Track {
                    language: s.language.clone(),
                    role: s.role,
                }),
        };
        Self { audio, subtitle }
    }
}

/// The tracks to play from `tracks`, for a viewer with `preferences` and
/// maybe an `override_` for this series or movie.
pub fn choose(
    tracks: &MediaTracks,
    preferences: &TrackPreferences,
    override_: Option<&TrackOverride>,
) -> ChosenTracks {
    let (audio, audio_reason) = choose_audio(tracks, preferences, override_);
    let audio_language = audio
        .and_then(|i| tracks.audio[i].language.as_deref())
        .map(str::to_owned);
    let (subtitle, subtitle_reason) =
        choose_subtitle(tracks, preferences, override_, audio_language.as_deref());
    ChosenTracks {
        selection: Selection {
            audio: audio.map(|i| i as u32),
            subtitle: subtitle.map(|i| i as u32),
        },
        audio: audio_reason,
        subtitle: subtitle_reason,
    }
}

fn same(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => same_language(a, b),
        (None, None) => true,
        _ => false,
    }
}

fn choose_audio(
    tracks: &MediaTracks,
    preferences: &TrackPreferences,
    override_: Option<&TrackOverride>,
) -> (Option<usize>, TrackReason) {
    let all: Vec<usize> = (0..tracks.audio.len()).collect();
    if all.is_empty() {
        return (None, TrackReason::NoTracks);
    }
    if let Some(key) = override_.and_then(|o| o.audio.as_ref()) {
        let matching: Vec<usize> = all
            .iter()
            .copied()
            .filter(|&i| {
                let a = &tracks.audio[i];
                a.role == key.role && same(a.language.as_deref(), key.language.as_deref())
            })
            .collect();
        if let Some(best) = best_audio(&tracks.audio, &matching) {
            return (Some(best), TrackReason::Override);
        }
    }

    // 1. The first preferred language the file has.
    let original = original_language(&tracks.audio);
    let mut candidates = all.clone();
    let mut reason = TrackReason::FileDefault;
    for wanted in &preferences.audio.languages {
        let wanted = if wanted == ORIGINAL_LANGUAGE {
            match original {
                Some(language) => language,
                None => continue,
            }
        } else {
            wanted.as_str()
        };
        let in_language: Vec<usize> = all
            .iter()
            .copied()
            .filter(|&i| {
                tracks.audio[i]
                    .language
                    .as_deref()
                    .is_some_and(|l| same_language(l, wanted))
            })
            .collect();
        if !in_language.is_empty() {
            candidates = in_language;
            reason = TrackReason::Preferences;
            break;
        }
    }
    // 2. Without the avoided roles, unless that leaves none.
    let kept: Vec<usize> = candidates
        .iter()
        .copied()
        .filter(|&i| !preferences.audio.avoid.contains(&tracks.audio[i].role))
        .collect();
    if !kept.is_empty() {
        candidates = kept;
    }
    // 3. The file's default, then the most channels, then the first.
    (best_audio(&tracks.audio, &candidates), reason)
}

/// The language of the file's default audio track, or of its first.
fn original_language(audio: &[AudioTrack]) -> Option<&str> {
    audio
        .iter()
        .find(|a| a.default)
        .or(audio.first())
        .and_then(|a| a.language.as_deref())
}

fn best_audio(audio: &[AudioTrack], candidates: &[usize]) -> Option<usize> {
    candidates.iter().copied().min_by_key(|&i| {
        let a = &audio[i];
        (!a.default, std::cmp::Reverse(a.channels.unwrap_or(0)), i)
    })
}

fn choose_subtitle(
    tracks: &MediaTracks,
    preferences: &TrackPreferences,
    override_: Option<&TrackOverride>,
    audio_language: Option<&str>,
) -> (Option<usize>, TrackReason) {
    let subtitles = &tracks.subtitles;
    if subtitles.is_empty() {
        return (None, TrackReason::NoTracks);
    }
    match override_.and_then(|o| o.subtitle.as_ref()) {
        Some(SubtitleChoice::Off) => return (None, TrackReason::Override),
        Some(SubtitleChoice::Track { language, role }) => {
            let found = first_subtitle(subtitles, |s| {
                s.role == *role && same(s.language.as_deref(), language.as_deref())
            });
            if found.is_some() {
                return (found, TrackReason::Override);
            }
        }
        None => {}
    }

    let wanted = &preferences.subtitles;
    let languages: Vec<&str> = if wanted.languages.is_empty() {
        audio_language.into_iter().collect()
    } else {
        wanted.languages.iter().map(String::as_str).collect()
    };
    match wanted.mode {
        SubtitleMode::Off => (None, TrackReason::NotWanted),
        SubtitleMode::ForcedOnly => {
            let Some(audio_language) = audio_language else {
                return (None, TrackReason::NoneMatched);
            };
            let found = first_subtitle(subtitles, |s| {
                s.role == SubtitleRole::Forced
                    && s.language
                        .as_deref()
                        .is_some_and(|l| same_language(l, audio_language))
            });
            match found {
                Some(i) => (Some(i), TrackReason::Preferences),
                None => (None, TrackReason::NoneMatched),
            }
        }
        SubtitleMode::ForeignOnly
            if audio_language
                .is_some_and(|audio| languages.iter().any(|l| same_language(l, audio))) =>
        {
            (None, TrackReason::NotWanted)
        }
        SubtitleMode::Always | SubtitleMode::ForeignOnly => {
            // Full dialogue only: never commentary, and forced tracks are
            // not the whole dialogue.
            let order: &[SubtitleRole] = match wanted.sdh {
                SdhPreference::Prefer => &[SubtitleRole::Sdh, SubtitleRole::Full],
                SdhPreference::Accept => &[SubtitleRole::Full, SubtitleRole::Sdh],
                SdhPreference::Avoid => &[SubtitleRole::Full],
            };
            for language in &languages {
                for role in order {
                    let found = first_subtitle(subtitles, |s| {
                        s.role == *role
                            && s.language
                                .as_deref()
                                .is_some_and(|l| same_language(l, language))
                    });
                    if found.is_some() {
                        return (found, TrackReason::Preferences);
                    }
                }
            }
            (None, TrackReason::NoneMatched)
        }
    }
}

/// The file's default among the tracks `fits` accepts, else the first.
fn first_subtitle(
    subtitles: &[SubtitleTrack],
    fits: impl Fn(&SubtitleTrack) -> bool,
) -> Option<usize> {
    let fitting: Vec<usize> = (0..subtitles.len())
        .filter(|&i| fits(&subtitles[i]))
        .collect();
    fitting
        .iter()
        .copied()
        .find(|&i| subtitles[i].default)
        .or(fitting.first().copied())
}

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
