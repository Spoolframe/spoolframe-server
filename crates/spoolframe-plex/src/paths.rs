//! Plex path → local path (ARCHITECTURE §7).
//!
//! Plex and Spoolframe may see the same files under different mount
//! points. A mapping rewrites a Plex-side prefix to the local one. Matching
//! is by whole path components, so `/data/movies` never matches
//! `/data/movies2`, and Plex's stray double slashes (`/media//TV`) do not
//! matter.

use std::fmt;
use std::path::{Component, Path, PathBuf};

/// One `from → to` rule, optionally only for one Plex library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathMapping {
    /// The Plex library title it applies to; `None` for every library.
    pub library: Option<String>,
    pub from: PathBuf,
    pub to: PathBuf,
}

impl fmt::Display for PathMapping {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.library {
            Some(library) => write!(
                f,
                "{} -> {} (library {library:?})",
                self.from.display(),
                self.to.display()
            ),
            None => write!(f, "{} -> {}", self.from.display(), self.to.display()),
        }
    }
}

/// Where a Plex path ends up locally, and which rule put it there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapped<'m> {
    pub local: PathBuf,
    /// `None` when no rule matched and the Plex path is used as it is.
    pub mapping: Option<&'m PathMapping>,
}

/// Every configured mapping.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathMap {
    mappings: Vec<PathMapping>,
}

impl PathMap {
    pub fn new(mappings: Vec<PathMapping>) -> Self {
        Self { mappings }
    }

    /// Map `plex` from the library titled `library`. A rule scoped to that
    /// library wins over a global one; within each group the longest
    /// matching `from` wins. No match leaves the path as it is.
    pub fn map(&self, library: Option<&str>, plex: &Path) -> Mapped<'_> {
        let scoped = self
            .mappings
            .iter()
            .filter(|m| m.library.is_some() && m.library.as_deref() == library);
        let global = self.mappings.iter().filter(|m| m.library.is_none());
        let best = longest_match(scoped, plex).or_else(|| longest_match(global, plex));
        match best {
            Some((mapping, rest)) => Mapped {
                local: mapping.to.join(rest),
                mapping: Some(mapping),
            },
            None => Mapped {
                local: normalized(plex),
                mapping: None,
            },
        }
    }
}

/// The rule with the longest `from` that is a component prefix of `plex`,
/// and the remainder of `plex` after it.
fn longest_match<'m>(
    mappings: impl Iterator<Item = &'m PathMapping>,
    plex: &Path,
) -> Option<(&'m PathMapping, PathBuf)> {
    mappings
        .filter_map(|m| plex.strip_prefix(&m.from).ok().map(|rest| (m, rest)))
        .max_by_key(|(m, _)| m.from.components().count())
        .map(|(m, rest)| (m, rest.to_owned()))
}

/// Why `local` cannot be served, or `None` if it is a readable file.
pub fn unreadable(local: &Path) -> Option<String> {
    match std::fs::File::open(local).and_then(|f| f.metadata()) {
        Ok(meta) if meta.is_file() => None,
        Ok(_) => Some("not a regular file".into()),
        Err(e) => Some(e.to_string()),
    }
}

/// `plex` rebuilt from its components, which drops repeated and trailing
/// slashes.
fn normalized(plex: &Path) -> PathBuf {
    plex.components().map(Component::as_os_str).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(library: Option<&str>, from: &str, to: &str) -> PathMapping {
        PathMapping {
            library: library.map(str::to_owned),
            from: from.into(),
            to: to.into(),
        }
    }

    fn map(rules: Vec<PathMapping>, library: Option<&str>, plex: &str) -> PathBuf {
        PathMap::new(rules).map(library, Path::new(plex)).local
    }

    #[test]
    fn a_matching_prefix_is_replaced() {
        assert_eq!(
            map(
                vec![rule(None, "/data/movies", "/media/movies")],
                None,
                "/data/movies/Alien/Alien.mkv"
            ),
            PathBuf::from("/media/movies/Alien/Alien.mkv")
        );
    }

    #[test]
    fn the_longest_prefix_wins() {
        let rules = vec![
            rule(None, "/data", "/mnt/data"),
            rule(None, "/data/movies", "/media/movies"),
        ];
        assert_eq!(
            map(rules, None, "/data/movies/a.mkv"),
            PathBuf::from("/media/movies/a.mkv")
        );
    }

    #[test]
    fn a_prefix_matches_whole_components_only() {
        let rules = vec![rule(None, "/data/movies", "/media/movies")];
        let paths = PathMap::new(rules);
        let got = paths.map(None, Path::new("/data/movies2/a.mkv"));
        assert_eq!(got.mapping, None);
        assert_eq!(got.local, PathBuf::from("/data/movies2/a.mkv"));
    }

    #[test]
    fn no_match_keeps_the_plex_path() {
        let paths = PathMap::default();
        let got = paths.map(None, Path::new("/srv/a.mkv"));
        assert_eq!(got.local, PathBuf::from("/srv/a.mkv"));
        assert_eq!(got.mapping, None);
    }

    #[test]
    fn trailing_slashes_on_a_rule_do_not_matter() {
        assert_eq!(
            map(
                vec![rule(None, "/data/movies/", "/media/movies/")],
                None,
                "/data/movies/a.mkv"
            ),
            PathBuf::from("/media/movies/a.mkv")
        );
    }

    #[test]
    fn a_doubled_slash_in_the_plex_path_does_not_matter() {
        // PMS reports paths like this for some libraries.
        assert_eq!(
            map(
                vec![rule(None, "/slow/media/TVShows", "/tv")],
                None,
                "/slow/media//TVShows/Vigil/S01E04.mkv"
            ),
            PathBuf::from("/tv/Vigil/S01E04.mkv")
        );
    }

    #[test]
    fn a_rule_for_the_library_wins_over_a_longer_global_one() {
        let rules = vec![
            rule(None, "/data/music", "/global/music"),
            rule(Some("Music"), "/data", "/mnt/nas/music"),
        ];
        assert_eq!(
            map(rules, Some("Music"), "/data/music/a.flac"),
            PathBuf::from("/mnt/nas/music/music/a.flac")
        );
    }

    #[test]
    fn a_rule_for_another_library_is_ignored() {
        let rules = vec![rule(Some("Music"), "/data", "/mnt/nas/music")];
        let paths = PathMap::new(rules);
        let got = paths.map(Some("Movies"), Path::new("/data/a.mkv"));
        assert_eq!(got.mapping, None);
    }

    #[test]
    fn a_library_rule_needs_a_library() {
        let rules = vec![rule(Some("Music"), "/data", "/mnt/nas/music")];
        assert_eq!(
            PathMap::new(rules).map(None, Path::new("/data/a")).mapping,
            None
        );
    }
}
