//! Plex path → local path (ARCHITECTURE §7).
//!
//! Plex and Spoolframe may see the same files under different mount
//! points. A mapping rewrites a Plex-side prefix to the local one. Matching
//! is by whole path components, so `/data/movies` never matches
//! `/data/movies2`, and Plex's stray double slashes (`/media//TV`) do not
//! matter.
//!
//! A Plex server names the files Spoolframe reads, so its paths are
//! confined (PX-09 B-5): none with `..`, and once any rule exists, none
//! that no rule maps. With no rules at all, Plex's paths are used as they
//! are, as on a host that mounts the media where Plex does.

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
    /// Why `local` is not served whether or not it can be read: a path
    /// that climbs with `..`, or that no rule maps when there are rules.
    pub refused: Option<&'static str>,
}

impl Mapped<'_> {
    /// Why the file cannot be served, or `None` if it is a readable file
    /// the rules allow.
    pub fn problem(&self) -> Option<String> {
        match self.refused {
            Some(reason) => Some(reason.to_owned()),
            None => unreadable(&self.local),
        }
    }
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
    /// matching `from` wins. No match leaves the path as it is, refused
    /// when there are rules.
    pub fn map(&self, library: Option<&str>, plex: &Path) -> Mapped<'_> {
        let scoped = self
            .mappings
            .iter()
            .filter(|m| m.library.is_some() && m.library.as_deref() == library);
        let global = self.mappings.iter().filter(|m| m.library.is_none());
        let best = longest_match(scoped, plex).or_else(|| longest_match(global, plex));
        // Before mapping, so that no rule's `to` is climbed out of: a
        // parent component is never in a path Plex has for a file.
        let climbs = plex.components().any(|c| c == Component::ParentDir);
        let refused = climbs.then_some("the Plex path has a `..` component");
        match best {
            Some((mapping, rest)) => Mapped {
                local: mapping.to.join(rest),
                mapping: Some(mapping),
                refused,
            },
            None => Mapped {
                local: normalized(plex),
                mapping: None,
                refused: refused.or_else(|| {
                    (!self.mappings.is_empty()).then_some("no path mapping matches it")
                }),
            },
        }
    }
}

impl PathMap {
    /// Map `plex` from the library titled `library`, and look for the
    /// file there: what the path check (P1-04) and an admin's look at an
    /// item (P3-16) both report.
    pub fn check(&self, library: Option<&str>, plex: &Path) -> spoolframe_catalog::FileCheck {
        let mapped = self.map(library, plex);
        spoolframe_catalog::FileCheck {
            reported: plex.to_owned(),
            mapping: mapped.mapping.map(|m| spoolframe_catalog::MappingUsed {
                library: m.library.clone(),
                from: m.from.clone(),
                to: m.to.clone(),
            }),
            problem: mapped.problem(),
            local: mapped.local,
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

/// A mapping that would make `plex`, a media file this host cannot read
/// at its Plex path, readable (P3-10): the longest tail of `plex` that is a
/// file under one of `bases`. `/data/movies/A/A.mkv` found as
/// `/mnt/nas/movies/A/A.mkv` suggests `/data -> /mnt/nas`. The longest
/// tail wins, so a file name alone that happens to exist somewhere is the
/// last resort, and the first base wins a tie. `None` when no tail is a
/// file anywhere.
pub fn suggest(plex: &Path, bases: &[PathBuf]) -> Option<PathMapping> {
    let parts: Vec<&std::ffi::OsStr> = plex
        .components()
        .filter_map(|c| match c {
            Component::Normal(part) => Some(part),
            _ => None,
        })
        .collect();
    // Longest tail first; the whole path is the Plex path itself, which
    // is not readable, so at least one component is left behind.
    for keep in (1..parts.len()).rev() {
        let from: PathBuf = std::iter::once(std::ffi::OsStr::new("/"))
            .chain(parts[..parts.len() - keep].iter().copied())
            .collect();
        let tail: PathBuf = parts[parts.len() - keep..].iter().collect();
        for base in bases {
            if base != &from && unreadable(&base.join(&tail)).is_none() {
                return Some(PathMapping {
                    library: None,
                    from,
                    to: base.clone(),
                });
            }
        }
    }
    None
}

/// Where to look for media files that are not at their Plex paths: the
/// usual mount points and the directories in them, one level down
/// (`/mnt/nas`, `/media/usb`), in that order.
pub fn usual_bases() -> Vec<PathBuf> {
    let tops = [
        "/mnt", "/media", "/srv", "/data", "/storage", "/nas", "/volume1",
    ];
    let mut bases = Vec::new();
    for top in tops.iter().map(PathBuf::from) {
        if !top.is_dir() {
            continue;
        }
        bases.push(top.clone());
        if let Ok(entries) = std::fs::read_dir(&top) {
            let mut children: Vec<PathBuf> = entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect();
            children.sort();
            bases.extend(children);
        }
    }
    bases
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

    #[tokio::test]
    async fn a_suggestion_maps_the_plex_prefix_to_where_the_file_is() {
        let dir = spoolframe_test_dir::TestDir::new("spoolframe-suggest").await;
        let nas = dir.as_ref().join("nas");
        let movie = nas.join("movies/Alien (1979)/Alien.mkv");
        std::fs::create_dir_all(movie.parent().unwrap()).unwrap();
        std::fs::write(&movie, b"x").unwrap();
        let elsewhere = dir.as_ref().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let bases = vec![elsewhere, nas.clone()];
        let found = suggest(Path::new("/data/movies/Alien (1979)/Alien.mkv"), &bases).unwrap();
        assert_eq!(found.from, PathBuf::from("/data"));
        assert_eq!(found.to, nas);
        // The suggestion reads the file.
        let map = PathMap::new(vec![found]);
        let mapped = map.map(None, Path::new("/data/movies/Alien (1979)/Alien.mkv"));
        assert_eq!(unreadable(&mapped.local), None);
        assert_eq!(suggest(Path::new("/data/movies/Missing.mkv"), &bases), None);
    }

    /// A file that can be read, under a fresh directory, for the checks
    /// below: a refused path must be refused although it can be read.
    async fn readable(test: &str) -> (spoolframe_test_dir::TestDir, PathBuf) {
        let dir = spoolframe_test_dir::TestDir::new(test).await;
        let library = dir.as_ref().join("library/movies");
        std::fs::create_dir_all(&library).unwrap();
        std::fs::write(dir.as_ref().join("spoolframe.db"), b"secret").unwrap();
        (dir, library)
    }

    /// PX-09 B-5: a Plex path that climbs out of its rule's `to` is not
    /// served, though the file it names is readable.
    #[tokio::test]
    async fn a_parent_component_after_mapping_is_refused() {
        let (_dir, library) = readable("spoolframe-paths-dotdot").await;
        let map = PathMap::new(vec![PathMapping {
            library: None,
            from: "/data/movies".into(),
            to: library,
        }]);
        let checked = map.check(None, Path::new("/data/movies/../../spoolframe.db"));
        assert!(checked.problem.is_some(), "{checked:?}");
    }

    /// With no rules the Plex path is used as it is, but never with `..`.
    #[tokio::test]
    async fn a_parent_component_without_mappings_is_refused() {
        let (dir, library) = readable("spoolframe-paths-dotdot-unmapped").await;
        let plex = library.join("../../spoolframe.db");
        assert_eq!(unreadable(&plex), None, "the file is readable");
        let checked = PathMap::default().check(None, &plex);
        assert!(checked.problem.is_some(), "{checked:?}");
        drop(dir);
    }

    /// Once there are rules, a path that none of them maps is not used as
    /// it is: a Plex server cannot name a file outside its libraries.
    #[tokio::test]
    async fn a_path_no_rule_matches_is_refused_when_rules_exist() {
        let (dir, library) = readable("spoolframe-paths-unmatched").await;
        let map = PathMap::new(vec![PathMapping {
            library: None,
            from: "/data/movies".into(),
            to: library,
        }]);
        let checked = map.check(None, &dir.as_ref().join("spoolframe.db"));
        assert!(checked.problem.is_some(), "{checked:?}");
        assert_eq!(checked.mapping, None);
    }

    /// A rule for another library is no rule for this one.
    #[tokio::test]
    async fn a_path_only_another_librarys_rule_matches_is_refused() {
        let (dir, library) = readable("spoolframe-paths-other-library").await;
        let map = PathMap::new(vec![PathMapping {
            library: Some("Music".into()),
            from: "/".into(),
            to: library,
        }]);
        let checked = map.check(Some("Movies"), &dir.as_ref().join("spoolframe.db"));
        assert!(checked.problem.is_some(), "{checked:?}");
    }

    /// No rules at all: the readable Plex path is served as before.
    #[tokio::test]
    async fn without_rules_a_readable_plex_path_is_served() {
        let (dir, _) = readable("spoolframe-paths-no-rules").await;
        let checked = PathMap::default().check(None, &dir.as_ref().join("spoolframe.db"));
        assert_eq!(checked.problem, None);
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
