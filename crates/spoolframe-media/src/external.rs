//! Subtitle files that sit next to a media file:
//! `Movie.srt`, `Movie.en.srt`, `Movie.en.forced.srt`, `Movie.idx`.

use std::path::{Path, PathBuf};

/// Extensions read as subtitle files. VobSub is found by its `.idx`; the
/// `.sub` beside it is its picture data. A lone `.sub` may be MicroDVD text
/// or VobSub data, so it is not guessed at.
const EXTENSIONS: &[&str] = &["srt", "ass", "ssa", "vtt", "sup", "idx"];

/// A subtitle file found next to a media file, with what its name says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalFile {
    pub path: PathBuf,
    /// The language code in the name (`en`, `eng`), if there is one.
    pub language: Option<String>,
    pub forced: bool,
    pub default: bool,
}

/// What the name `file_name` says about a subtitle for the media file
/// whose stem is `stem`, or `None` if it is not one.
fn parse(stem: &str, file_name: &str) -> Option<(Option<String>, bool, bool)> {
    let rest = file_name.strip_prefix(stem)?.strip_prefix('.')?;
    let (middle, ext) = match rest.rsplit_once('.') {
        Some((middle, ext)) => (middle, ext),
        None => ("", rest),
    };
    if !EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()) {
        return None;
    }
    let mut language = None;
    let (mut forced, mut default) = (false, false);
    for token in middle.split('.').filter(|t| !t.is_empty()) {
        match token.to_ascii_lowercase().as_str() {
            "forced" => forced = true,
            "default" => default = true,
            t if language.is_none()
                && (2..=3).contains(&t.len())
                && t.bytes().all(|b| b.is_ascii_alphabetic()) =>
            {
                language = Some(t.to_owned());
            }
            // "sdh", "cc", release-group tags…: not something to act on.
            _ => {}
        }
    }
    Some((language, forced, default))
}

/// The subtitle files beside `media`, in name order. A directory that
/// cannot be listed yields none: the media itself is still playable.
pub fn find(media: &Path) -> Vec<ExternalFile> {
    let (Some(dir), Some(stem)) = (media.parent(), media.file_stem().and_then(|s| s.to_str()))
    else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<_> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let (language, forced, default) = parse(stem, name.to_str()?)?;
            Some(ExternalFile {
                path: entry.path(),
                language,
                forced,
                default,
            })
        })
        .collect();
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEM: &str = "Slow.Horses.S03E01.Strange.Games.WEBDL-2160p";

    fn name(rest: &str) -> String {
        format!("{STEM}{rest}")
    }

    #[test]
    fn a_bare_srt_has_no_language() {
        assert_eq!(parse(STEM, &name(".srt")), Some((None, false, false)));
    }

    #[test]
    fn a_language_code_is_read() {
        assert_eq!(
            parse(STEM, &name(".en.vtt")),
            Some((Some("en".into()), false, false))
        );
    }

    #[test]
    fn forced_and_default_are_read() {
        assert_eq!(
            parse(STEM, &name(".eng.forced.default.srt")),
            Some((Some("eng".into()), true, true))
        );
    }

    #[test]
    fn unknown_tokens_are_ignored() {
        assert_eq!(
            parse(STEM, &name(".en.sdh.srt")),
            Some((Some("en".into()), false, false))
        );
    }

    #[test]
    fn the_media_file_itself_is_not_a_subtitle() {
        assert_eq!(parse(STEM, &name(".mkv")), None);
    }

    #[test]
    fn another_episode_is_not_a_subtitle() {
        assert_eq!(
            parse(STEM, "Slow.Horses.S03E02.Hello.Goodbye.WEBDL-2160p.srt"),
            None
        );
    }

    #[test]
    fn a_name_sharing_only_a_prefix_is_not_a_subtitle() {
        // "Movie2.srt" is not a subtitle of "Movie.mkv".
        assert_eq!(parse("Movie", "Movie2.srt"), None);
    }

    #[test]
    fn a_lone_sub_file_is_not_guessed_at() {
        assert_eq!(parse(STEM, &name(".sub")), None);
    }

    #[test]
    fn find_lists_matching_files_in_name_order() {
        let dir =
            std::env::temp_dir().join(format!("spoolframe-external-find-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in [
            "Movie.mkv",
            "Movie.srt",
            "Movie.de.forced.srt",
            "Movie.nfo",
            "Other.srt",
        ] {
            std::fs::write(dir.join(f), b"").unwrap();
        }
        let found = find(&dir.join("Movie.mkv"));
        let names: Vec<_> = found
            .iter()
            .map(|f| f.path.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(names, ["Movie.de.forced.srt", "Movie.srt"]);
        assert!(found[0].forced);
    }
}
