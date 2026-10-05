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
