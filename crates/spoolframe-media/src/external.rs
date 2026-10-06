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
    pub name: NameTags,
}

/// What a subtitle file's name says about it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NameTags {
    /// The language code in the name (`en`, `eng`), if there is one.
    pub language: Option<String>,
    pub forced: bool,
    pub default: bool,
    /// `sdh`, `cc` or `hi`: for deaf and hard-of-hearing viewers.
    pub sdh: bool,
}

/// What the name `file_name` says about a subtitle for the media file
/// whose stem is `stem`, or `None` if it is not one.
fn parse(stem: &str, file_name: &str) -> Option<NameTags> {
    let rest = file_name.strip_prefix(stem)?.strip_prefix('.')?;
    let (middle, ext) = match rest.rsplit_once('.') {
        Some((middle, ext)) => (middle, ext),
        None => ("", rest),
    };
    if !EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()) {
        return None;
    }
    let mut tags = NameTags::default();
    for token in middle.split('.').filter(|t| !t.is_empty()) {
        match token.to_ascii_lowercase().as_str() {
            "forced" => tags.forced = true,
            "default" => tags.default = true,
            // Before the language rule: "cc" and "hi" would pass for codes.
            "sdh" | "cc" | "hi" => tags.sdh = true,
            t if tags.language.is_none()
                && (2..=3).contains(&t.len())
                && t.bytes().all(|b| b.is_ascii_alphabetic()) =>
            {
                tags.language = Some(t.to_owned());
            }
            // Release-group tags and the like: not something to act on.
            _ => {}
        }
    }
    Some(tags)
}

/// The subtitle files beside `media`, in name order. A directory that
/// cannot be listed yields none: the media itself is still playable.
///
/// Only regular files are subtitles (PX-09 B-1): a FIFO would block the
/// read, and a symlink could name any file the server can read. A symlink
/// to a regular file in the same directory is kept, under its target's
/// path, so that reading it never follows a link.
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
            let tags = parse(stem, name.to_str()?)?;
            Some(ExternalFile {
                path: regular_file_in(dir, &entry)?,
                name: tags,
            })
        })
        .collect();
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found
}

/// `entry`'s path if it is a regular file, or a symlink to a regular file
/// in `dir` itself; that file's path then.
fn regular_file_in(dir: &Path, entry: &std::fs::DirEntry) -> Option<PathBuf> {
    // Of the entry itself: a symlink is not followed here.
    let kind = entry.file_type().ok()?;
    if kind.is_file() {
        return Some(entry.path());
    }
    if !kind.is_symlink() {
        return None;
    }
    let target = entry.path().canonicalize().ok()?;
    let inside = target.parent() == Some(dir.canonicalize().ok()?.as_path());
    let regular = std::fs::symlink_metadata(&target).ok()?.is_file();
    (inside && regular).then_some(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEM: &str = "Slow.Horses.S03E01.Strange.Games.WEBDL-2160p";

    fn name(rest: &str) -> String {
        format!("{STEM}{rest}")
    }

    fn tags(language: Option<&str>, forced: bool, default: bool, sdh: bool) -> NameTags {
        NameTags {
            language: language.map(str::to_owned),
            forced,
            default,
            sdh,
        }
    }

    #[test]
    fn a_bare_srt_has_no_language() {
        assert_eq!(parse(STEM, &name(".srt")), Some(NameTags::default()));
    }

    #[test]
    fn a_language_code_is_read() {
        assert_eq!(
            parse(STEM, &name(".en.vtt")),
            Some(tags(Some("en"), false, false, false))
        );
    }

    #[test]
    fn forced_and_default_are_read() {
        assert_eq!(
            parse(STEM, &name(".eng.forced.default.srt")),
            Some(tags(Some("eng"), true, true, false))
        );
    }

    #[test]
    fn sdh_cc_and_hi_are_read_and_are_not_languages() {
        for tag in ["sdh", "cc", "hi", "SDH"] {
            assert_eq!(
                parse(STEM, &name(&format!(".en.{tag}.srt"))),
                Some(tags(Some("en"), false, false, true)),
                "{tag}"
            );
        }
        assert_eq!(
            parse(STEM, &name(".cc.srt")),
            Some(tags(None, false, false, true))
        );
    }

    #[test]
    fn unknown_tokens_are_ignored() {
        assert_eq!(
            parse(STEM, &name(".en.rarbg.srt")),
            Some(tags(Some("en"), false, false, false))
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

    #[tokio::test]
    async fn find_lists_matching_files_in_name_order() {
        let tmp = spoolframe_test_dir::TestDir::new("spoolframe-external-find").await;
        let dir = tmp.as_ref();
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
        assert!(found[0].name.forced);
    }

    /// PX-09 B-1: a sidecar name that is a symlink out of the media's
    /// directory is not a subtitle, whatever it points at.
    #[tokio::test]
    async fn find_skips_a_symlink_out_of_the_media_directory() {
        let tmp = spoolframe_test_dir::TestDir::new("spoolframe-external-outside").await;
        let library = tmp.as_ref().join("library");
        std::fs::create_dir_all(&library).unwrap();
        std::fs::write(tmp.as_ref().join("secret.key"), b"key").unwrap();
        std::fs::write(library.join("Movie.mkv"), b"").unwrap();
        std::os::unix::fs::symlink(tmp.as_ref().join("secret.key"), library.join("Movie.srt"))
            .unwrap();
        assert_eq!(find(&library.join("Movie.mkv")), Vec::new());
    }

    /// A FIFO or a directory with a sidecar's name is not a subtitle:
    /// opening a FIFO would wait for a writer.
    #[tokio::test]
    async fn find_skips_what_is_not_a_regular_file() {
        let tmp = spoolframe_test_dir::TestDir::new("spoolframe-external-special").await;
        let dir = tmp.as_ref();
        std::fs::write(dir.join("Movie.mkv"), b"").unwrap();
        std::fs::create_dir(dir.join("Movie.en.srt")).unwrap();
        let fifo =
            std::ffi::CString::new(dir.join("Movie.srt").into_os_string().into_encoded_bytes())
                .unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert_eq!(find(&dir.join("Movie.mkv")), Vec::new());
    }

    /// A symlink to another file in the same directory is kept, as the
    /// file it names, so that a read need not follow a link.
    #[tokio::test]
    async fn find_keeps_a_symlink_within_the_media_directory_as_its_target() {
        let tmp = spoolframe_test_dir::TestDir::new("spoolframe-external-inside").await;
        let dir = tmp.as_ref();
        std::fs::write(dir.join("Movie.mkv"), b"").unwrap();
        std::fs::write(dir.join("subs.txt"), b"1\n").unwrap();
        std::os::unix::fs::symlink("subs.txt", dir.join("Movie.en.srt")).unwrap();
        let found = find(&dir.join("Movie.mkv"));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name.language.as_deref(), Some("en"));
        assert_eq!(
            found[0].path,
            dir.canonicalize().unwrap().join("subs.txt"),
            "the path names the file, not the link"
        );
    }
}
