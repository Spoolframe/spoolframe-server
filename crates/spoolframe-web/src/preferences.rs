//! My profile's Playback card (P3-03): track preferences (P1-20) as the
//! card's choices show them, and each series' or movie's pick in words.
//!
//! A preference is a list of languages; the card offers the lists people
//! pick (one language, or one then the original), and keeps any other
//! list a device set as a choice of its own, so opening the page never
//! changes what was set.

use spoolframe_admin_api::{
    AudioPick, AudioRole, BitmapPreference, ForcedPreference, SdhPreference, SubtitleMode,
    SubtitlePick, SubtitleRole, TrackOverrideEntry,
};

/// What stands for the item's original language in an audio list.
pub const ORIGINAL: &str = "original";

/// The languages the card offers, by ISO 639-2 code: the ones most
/// libraries have tracks in. Any other a device set is still shown.
pub const LANGUAGES: &[(&str, &str)] = &[
    ("eng", "English"),
    ("fra", "French"),
    ("deu", "German"),
    ("spa", "Spanish"),
    ("ita", "Italian"),
    ("por", "Portuguese"),
    ("nld", "Dutch"),
    ("swe", "Swedish"),
    ("nor", "Norwegian"),
    ("dan", "Danish"),
    ("fin", "Finnish"),
    ("pol", "Polish"),
    ("ces", "Czech"),
    ("ukr", "Ukrainian"),
    ("rus", "Russian"),
    ("tur", "Turkish"),
    ("ara", "Arabic"),
    ("heb", "Hebrew"),
    ("hin", "Hindi"),
    ("jpn", "Japanese"),
    ("kor", "Korean"),
    ("zho", "Chinese"),
];

/// A language's name: `eng` is English, `original` the original
/// language; a code the card does not know, as it is.
pub fn language_name(code: &str) -> String {
    if code == ORIGINAL {
        return "Original language".into();
    }
    LANGUAGES
        .iter()
        .find(|(c, _)| *c == code)
        .map_or_else(|| code.to_owned(), |(_, name)| (*name).to_owned())
}

/// A list of languages in words: "English, then original".
pub fn languages_line(codes: &[String]) -> String {
    codes
        .iter()
        .enumerate()
        .map(|(i, code)| match (i, code.as_str()) {
            (0, _) => language_name(code),
            (_, ORIGINAL) => "original".into(),
            _ => language_name(code),
        })
        .collect::<Vec<_>>()
        .join(", then ")
}

/// A list of codes as one select value, and back.
pub fn key(codes: &[String]) -> String {
    codes.join(",")
}

pub fn codes(key: &str) -> Vec<String> {
    key.split(',')
        .filter(|c| !c.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The audio choices: `(value, label)`. The file's default, the original
/// language, each language alone and then the original, and `current`
/// if it is none of those.
pub fn audio_choices(current: &[String]) -> Vec<(String, String)> {
    let mut choices = vec![
        (String::new(), "The file's default".to_owned()),
        (ORIGINAL.to_owned(), "Original language".to_owned()),
    ];
    for (code, name) in LANGUAGES {
        choices.push(((*code).to_owned(), (*name).to_owned()));
        choices.push((
            format!("{code},{ORIGINAL}"),
            format!("{name}, then original"),
        ));
    }
    keep(choices, current, languages_line)
}

/// The subtitle language choices: the audio's own, each language, and
/// `current` if it is none of those.
pub fn subtitle_choices(current: &[String]) -> Vec<(String, String)> {
    let mut choices = vec![(String::new(), "Same as the audio".to_owned())];
    for (code, name) in LANGUAGES {
        choices.push(((*code).to_owned(), (*name).to_owned()));
    }
    keep(choices, current, languages_line)
}

fn keep(
    mut choices: Vec<(String, String)>,
    current: &[String],
    label: fn(&[String]) -> String,
) -> Vec<(String, String)> {
    let value = key(current);
    if !choices.iter().any(|(v, _)| *v == value) {
        choices.push((value, label(current)));
    }
    choices
}

/// When subtitles show, as the card names each mode.
pub const SUBTITLE_MODES: &[(SubtitleMode, &str)] = &[
    (SubtitleMode::ForcedOnly, "Forced only"),
    (SubtitleMode::ForeignOnly, "When the audio is foreign"),
    (SubtitleMode::Always, "Always"),
    (SubtitleMode::Off, "Off"),
];

/// Subtitles for deaf and hard-of-hearing viewers.
pub const SDH: &[(SdhPreference, &str)] = &[
    (SdhPreference::Accept, "When there are no others"),
    (SdhPreference::Prefer, "Before others"),
    (SdhPreference::Avoid, "Never"),
    (SdhPreference::Only, "Only SDH"),
];

/// Forced subtitles (signs and foreign lines only) in a subtitle search,
/// as the card names each choice: the same four as [`SDH`].
pub const FORCED: &[(ForcedPreference, &str)] = &[
    (ForcedPreference::Accept, "After others"),
    (ForcedPreference::Prefer, "Before others"),
    (ForcedPreference::Avoid, "Never"),
    (ForcedPreference::Only, "Only forced"),
];

/// How picture subtitles (PGS, VobSub, DVB) show, as the card names each
/// choice.
pub const PICTURE_SUBTITLES: &[(BitmapPreference, &str)] = &[
    (BitmapPreference::Text, "Read as text (OCR)"),
    (BitmapPreference::Pictures, "Show as pictures"),
];

/// The picture subtitle row as the card draws it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PictureRow {
    /// The choice shown, in [`PICTURE_SUBTITLES`].
    pub shown: usize,
    /// Why the choice is disabled, when the server does not read picture
    /// subtitles as text.
    pub off: Option<&'static str>,
}

/// The row for `preference` on a server that reads picture subtitles as
/// text if `ocr_offered`. Without it every user gets pictures, so that is
/// what the row shows; the saved choice is kept for when it is offered.
pub fn picture_row(preference: BitmapPreference, ocr_offered: bool) -> PictureRow {
    if !ocr_offered {
        return PictureRow {
            shown: index_of(PICTURE_SUBTITLES, &BitmapPreference::Pictures),
            off: Some(
                "This server doesn't read picture subtitles as text, so they show as pictures.",
            ),
        };
    }
    PictureRow {
        shown: index_of(PICTURE_SUBTITLES, &preference),
        off: None,
    }
}

/// A choice's value in a select: its place in the list.
pub fn index_of<T: PartialEq>(list: &[(T, &str)], value: &T) -> usize {
    list.iter().position(|(v, _)| v == value).unwrap_or(0)
}

/// The roles "avoid audio description and commentary" stands for.
pub const AVOIDED: [AudioRole; 2] = [AudioRole::AudioDescription, AudioRole::Commentary];

/// Whether the preferences avoid both: what the card's switch shows.
pub fn avoids_extras(avoid: &[AudioRole]) -> bool {
    AVOIDED.iter().all(|role| avoid.contains(role))
}

/// What one series' or movie's pick is, in words: "English audio ·
/// subtitles off".
pub fn override_line(entry: &TrackOverrideEntry) -> String {
    let audio = entry.audio.as_ref().map(audio_words);
    let subtitle = entry.subtitle.as_ref().map(subtitle_words);
    match (audio, subtitle) {
        (Some(a), Some(s)) => format!("{a} · {s}"),
        (Some(a), None) => a,
        (None, Some(s)) => capitalised(&s),
        (None, None) => "As your preferences".into(),
    }
}

fn in_language(language: &Option<String>) -> String {
    language
        .as_deref()
        .map_or_else(|| "an unnamed language".into(), language_name)
}

fn audio_words(pick: &AudioPick) -> String {
    let language = in_language(&pick.language);
    match pick.role {
        AudioRole::Main => format!("{language} audio"),
        AudioRole::AudioDescription => format!("Audio description in {language}"),
        AudioRole::Commentary => format!("Commentary in {language}"),
        _ => format!("Other audio in {language}"),
    }
}

fn subtitle_words(pick: &SubtitlePick) -> String {
    match pick {
        SubtitlePick::Off => "subtitles off".into(),
        SubtitlePick::Track { language, role } => {
            let language = in_language(language);
            match role {
                SubtitleRole::Sdh => format!("{language} subtitles (SDH)"),
                SubtitleRole::Forced => format!("{language} forced subtitles"),
                SubtitleRole::Commentary => format!("{language} commentary subtitles"),
                _ => format!("{language} subtitles"),
            }
        }
    }
}

fn capitalised(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(codes: &[&str]) -> Vec<String> {
        codes.iter().map(|c| (*c).to_owned()).collect()
    }

    #[test]
    fn picture_subtitles_are_read_as_text_by_default() {
        let row = picture_row(BitmapPreference::default(), true);
        assert_eq!(PICTURE_SUBTITLES[row.shown].1, "Read as text (OCR)");
        assert_eq!(row.off, None);
    }

    #[test]
    fn a_user_who_chose_pictures_sees_pictures() {
        let row = picture_row(BitmapPreference::Pictures, true);
        assert_eq!(PICTURE_SUBTITLES[row.shown].1, "Show as pictures");
        assert_eq!(row.off, None);
    }

    #[test]
    fn without_the_servers_ocr_the_choice_is_off_and_shows_pictures() {
        let row = picture_row(BitmapPreference::Text, false);
        assert_eq!(PICTURE_SUBTITLES[row.shown].1, "Show as pictures");
        assert_eq!(
            row.off,
            Some("This server doesn't read picture subtitles as text, so they show as pictures.")
        );
    }

    #[test]
    fn a_list_reads_as_the_board_writes_it() {
        assert_eq!(
            languages_line(&list(&["eng", "original"])),
            "English, then original"
        );
        assert_eq!(languages_line(&list(&["original"])), "Original language");
        assert_eq!(
            languages_line(&list(&["eng", "jpn"])),
            "English, then Japanese"
        );
        assert_eq!(languages_line(&list(&["tlh"])), "tlh");
    }

    #[test]
    fn a_value_goes_to_its_codes_and_back() {
        for codes in [list(&[]), list(&["eng"]), list(&["eng", "original"])] {
            assert_eq!(super::codes(&key(&codes)), codes);
        }
    }

    #[test]
    fn a_list_a_device_set_is_kept_as_a_choice_of_its_own() {
        let offered = audio_choices(&list(&["eng", "original"]));
        let custom = audio_choices(&list(&["eng", "jpn"]));
        assert_eq!(custom.len(), offered.len() + 1);
        assert_eq!(
            custom.last().unwrap(),
            &("eng,jpn".to_owned(), "English, then Japanese".to_owned())
        );
        // One the card offers is not added twice.
        assert!(
            offered
                .iter()
                .any(|(v, l)| v == "eng,original" && l == "English, then original")
        );
        let subtitles = subtitle_choices(&list(&["fra", "eng"]));
        assert_eq!(subtitles.last().unwrap().0, "fra,eng");
        assert_eq!(subtitle_choices(&[]).len(), LANGUAGES.len() + 1);
    }

    #[test]
    fn the_switch_shows_on_only_when_both_extras_are_avoided() {
        assert!(avoids_extras(&AVOIDED));
        assert!(!avoids_extras(&[AudioRole::Commentary]));
        assert!(!avoids_extras(&[]));
    }

    #[test]
    fn a_pick_reads_as_words() {
        let entry = |audio, subtitle| TrackOverrideEntry {
            id: "plex:show:10".into(),
            title: None,
            audio,
            subtitle,
        };
        let english = Some(AudioPick {
            language: Some("eng".into()),
            role: AudioRole::Main,
        });
        assert_eq!(
            override_line(&entry(english.clone(), Some(SubtitlePick::Off))),
            "English audio · subtitles off"
        );
        let sdh = SubtitlePick::Track {
            language: Some("eng".into()),
            role: SubtitleRole::Sdh,
        };
        assert_eq!(
            override_line(&entry(None, Some(sdh))),
            "English subtitles (SDH)"
        );
        let described = AudioPick {
            language: None,
            role: AudioRole::AudioDescription,
        };
        assert_eq!(
            override_line(&entry(Some(described), None)),
            "Audio description in an unnamed language"
        );
    }

    #[test]
    fn modes_and_sdh_are_found_by_value() {
        assert_eq!(index_of(SUBTITLE_MODES, &SubtitleMode::Off), 3);
        assert_eq!(index_of(SDH, &SdhPreference::Prefer), 1);
        assert_eq!(index_of(SDH, &SdhPreference::Only), 3);
        assert_eq!(index_of(FORCED, &ForcedPreference::Only), 3);
        assert_eq!(index_of(FORCED, &ForcedPreference::default()), 0);
    }
}
