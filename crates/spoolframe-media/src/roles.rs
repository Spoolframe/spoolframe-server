//! What a track is for (P1-20): from the file's disposition flags first,
//! then from its title.
//!
//! Files often set no flags. The household's Brooklyn Nine-Nine season 7
//! marks neither its audio description ("English - Audio Description") nor
//! its SDH subtitles ("SDH"), so titles are read too: matched as whole
//! words, case-insensitive, by the tables below. A title that matches
//! nothing is the main track.

use spoolframe_types::{AudioRole, SubtitleRole};

/// The disposition flags that say what a track is for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Flags {
    pub forced: bool,
    pub hearing_impaired: bool,
    pub visual_impaired: bool,
    /// Audio that describes the picture (ffmpeg's `descriptions`).
    pub descriptions: bool,
    pub comment: bool,
    /// Karaoke, or music and effects only.
    pub other: bool,
}

/// Title words for each audio role, checked in this order.
const AUDIO_TITLES: &[(AudioRole, &[&str])] = &[
    (
        AudioRole::AudioDescription,
        &["audio description", "described", "descriptive", "ad"],
    ),
    (AudioRole::Commentary, &["commentary", "director", "cast"]),
];

/// Title words for each subtitle role, checked in this order.
const SUBTITLE_TITLES: &[(SubtitleRole, &[&str])] = &[
    (SubtitleRole::Commentary, &["commentary"]),
    (SubtitleRole::Forced, &["forced", "signs", "foreign parts"]),
    (SubtitleRole::Sdh, &["sdh", "cc", "hearing impaired"]),
];

pub fn audio_role(flags: Flags, title: Option<&str>) -> AudioRole {
    if flags.visual_impaired || flags.descriptions {
        return AudioRole::AudioDescription;
    }
    if flags.comment {
        return AudioRole::Commentary;
    }
    if flags.other {
        return AudioRole::Other;
    }
    by_title(AUDIO_TITLES, title).unwrap_or_default()
}

pub fn subtitle_role(flags: Flags, title: Option<&str>) -> SubtitleRole {
    if flags.forced {
        return SubtitleRole::Forced;
    }
    if flags.hearing_impaired {
        return SubtitleRole::Sdh;
    }
    if flags.comment {
        return SubtitleRole::Commentary;
    }
    by_title(SUBTITLE_TITLES, title).unwrap_or_default()
}

/// The first role one of whose phrases is in `title` as whole words.
fn by_title<R: Copy>(table: &[(R, &[&str])], title: Option<&str>) -> Option<R> {
    let words = words(title?);
    table.iter().find_map(|(role, phrases)| {
        phrases
            .iter()
            .any(|phrase| contains_phrase(&words, phrase))
            .then_some(*role)
    })
}

/// The title's words, lowercased; anything not a letter or digit splits.
fn words(title: &str) -> Vec<String> {
    title
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn contains_phrase(words: &[String], phrase: &str) -> bool {
    let phrase: Vec<&str> = phrase.split(' ').collect();
    words
        .windows(phrase.len())
        .any(|window| window.iter().zip(&phrase).all(|(w, p)| w == p))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Flags = Flags {
        forced: false,
        hearing_impaired: false,
        visual_impaired: false,
        descriptions: false,
        comment: false,
        other: false,
    };

    #[test]
    fn audio_titles() {
        for (title, role) in [
            ("English - Audio Description", AudioRole::AudioDescription),
            ("Described Video", AudioRole::AudioDescription),
            ("English AD", AudioRole::AudioDescription),
            ("Commentary by the director", AudioRole::Commentary),
            ("Director's Commentary", AudioRole::Commentary),
            ("Cast commentary", AudioRole::Commentary),
            ("English", AudioRole::Main),
            ("Brazilian", AudioRole::Main),
            // Whole words only.
            ("Broadcast Mix", AudioRole::Main),
            ("Adventure", AudioRole::Main),
        ] {
            assert_eq!(audio_role(NONE, Some(title)), role, "{title:?}");
        }
        assert_eq!(audio_role(NONE, None), AudioRole::Main);
    }

    #[test]
    fn subtitle_titles() {
        for (title, role) in [
            ("English (SDH)", SubtitleRole::Sdh),
            ("SDH", SubtitleRole::Sdh),
            ("English CC", SubtitleRole::Sdh),
            ("For the hearing impaired", SubtitleRole::Sdh),
            ("Forced", SubtitleRole::Forced),
            ("Forced European", SubtitleRole::Forced),
            ("Signs & Songs", SubtitleRole::Forced),
            ("Foreign parts only", SubtitleRole::Forced),
            ("Commentary", SubtitleRole::Commentary),
            ("English", SubtitleRole::Full),
            ("Simplified", SubtitleRole::Full),
        ] {
            assert_eq!(subtitle_role(NONE, Some(title)), role, "{title:?}");
        }
    }

    #[test]
    fn flags_give_the_same_roles_as_titles() {
        let flagged = |f: fn(&mut Flags)| {
            let mut flags = NONE;
            f(&mut flags);
            flags
        };
        assert_eq!(
            audio_role(flagged(|f| f.visual_impaired = true), Some("English")),
            AudioRole::AudioDescription
        );
        assert_eq!(
            audio_role(flagged(|f| f.descriptions = true), None),
            AudioRole::AudioDescription
        );
        assert_eq!(
            audio_role(flagged(|f| f.comment = true), Some("English")),
            AudioRole::Commentary
        );
        assert_eq!(
            subtitle_role(flagged(|f| f.hearing_impaired = true), Some("English")),
            SubtitleRole::Sdh
        );
        assert_eq!(
            subtitle_role(flagged(|f| f.forced = true), None),
            SubtitleRole::Forced
        );
    }

    #[test]
    fn a_flag_wins_over_the_title() {
        let forced = Flags {
            forced: true,
            ..NONE
        };
        assert_eq!(subtitle_role(forced, Some("SDH")), SubtitleRole::Forced);
    }
}
