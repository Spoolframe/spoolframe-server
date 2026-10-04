//! What Server › Libraries, its add-library flow and the wizard's Plex step
//! decide (P3-11), apart from the pages: host-compiled, so `cargo test`
//! covers it.

use spoolframe_admin_api::{
    LibrariesOverview, LibraryEntry, LibraryKind, LibraryPathCheck, LibraryStatus, PathMappingEntry,
};

use crate::logic::thousands;
use crate::setup::{Sample, mapped, sample};

/// What one top-level item of a library is called, by kind.
fn unit(kind: LibraryKind, n: u64) -> &'static str {
    match (kind, n == 1) {
        (LibraryKind::Movies, true) => "movie",
        (LibraryKind::Movies, false) => "movies",
        (LibraryKind::Shows, true) => "show",
        (LibraryKind::Shows, false) => "shows",
        (LibraryKind::Music, true) => "artist",
        (LibraryKind::Music, false) => "artists",
    }
}

/// A library's size as its row says it: "1,204 movies"; "—" when its
/// server does not say.
pub fn items_line(kind: LibraryKind, items: Option<u32>) -> String {
    match items {
        Some(n) => format!("{} {}", thousands(u64::from(n)), unit(kind, u64::from(n))),
        None => "—".to_owned(),
    }
}

/// What a library kind is called in a section's line.
pub fn kind_title(kind: LibraryKind) -> &'static str {
    match kind {
        LibraryKind::Movies => "Movies",
        LibraryKind::Shows => "Shows",
        LibraryKind::Music => "Music",
    }
}

/// A Plex section as the add flow lists it: "Movies · 318".
pub fn section_line(section: &LibraryStatus) -> String {
    format!(
        "{} · {}",
        kind_title(section.kind),
        thousands(u64::from(section.items))
    )
}

/// Where a library comes from: "Plex · Tower · Movies".
pub fn source_line(library: &LibraryEntry) -> String {
    let title = library
        .plex_title
        .clone()
        .unwrap_or_else(|| format!("section {}", library.section));
    format!("Plex · {} · {title}", library.server_name)
}

/// A library's paths, as its row says them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathCell {
    Ok,
    /// `n` of the sample files cannot be read.
    Missing(u64),
    /// Its server is down, or no longer has it: why, in a few words.
    Unavailable(String),
    /// Nothing to say: an empty library.
    Unknown,
}

/// `reachable`: whether the library's Plex server answers.
pub fn path_cell(library: &LibraryEntry, reachable: bool) -> PathCell {
    if !reachable {
        return PathCell::Unavailable(format!("{} can't be reached", library.server_name));
    }
    if library.error.is_some() {
        // The server answers, but has no such section any more.
        return PathCell::Unavailable(format!("{} no longer has it", library.server_name));
    }
    match library.paths.as_ref().map(sample) {
        Some(Sample::Ok { .. }) => PathCell::Ok,
        Some(Sample::Missing { missing, .. }) => PathCell::Missing(missing),
        Some(Sample::Empty) | None => PathCell::Unknown,
    }
}

/// Each Plex server that is down, with the names of its libraries.
pub fn down(overview: &LibrariesOverview) -> Vec<(String, Vec<String>)> {
    overview
        .servers
        .iter()
        .filter(|s| !s.status.reachable)
        .map(|s| {
            let names = overview
                .libraries
                .iter()
                .filter(|l| l.server == s.id)
                .map(|l| l.name.clone())
                .collect();
            (s.name.clone(), names)
        })
        .collect()
}

/// "A, B and C".
fn listed(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// What a down server's card says about its libraries.
pub fn down_text(libraries: &[String], all: usize) -> String {
    match libraries.len() {
        0 => "None of the libraries come from it.".to_owned(),
        n if n == all => "Its libraries are unavailable until it is back.".to_owned(),
        1 => format!(
            "Only {} is unavailable; the other libraries work.",
            libraries[0]
        ),
        _ => format!(
            "Only {} are unavailable; the other libraries work.",
            listed(libraries)
        ),
    }
}

/// "Add 2 libraries", "Add 1 library".
pub fn add_label(n: usize) -> String {
    match n {
        1 => "Add 1 library".to_owned(),
        n => format!("Add {n} libraries"),
    }
}

/// "2 libraries added from Cabin".
pub fn added_text(n: usize, server: &str) -> String {
    match n {
        1 => format!("1 library added from {server}"),
        n => format!("{n} libraries added from {server}"),
    }
}

/// What the wizard's library checkboxes change: the sections to add, and
/// the libraries (by id) to remove. `chosen` are the checked sections'
/// keys; a section is a library already when the server says so.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Choice {
    pub add: Vec<String>,
    pub remove: Vec<String>,
}

pub fn choice(sections: &[LibraryStatus], chosen: &[String]) -> Choice {
    let mut out = Choice::default();
    for s in sections {
        let checked = chosen.contains(&s.section);
        match (&s.library, checked) {
            (None, true) => out.add.push(s.section.clone()),
            (Some(id), false) => out.remove.push(id.clone()),
            _ => {}
        }
    }
    out
}

/// The checkboxes as the server has them: the sections that are
/// libraries, or every one when none is (a server not added yet serves all
/// it can once it is).
pub fn checked(sections: &[LibraryStatus]) -> Vec<String> {
    let served: Vec<String> = sections
        .iter()
        .filter(|s| s.library.is_some())
        .map(|s| s.section.clone())
        .collect();
    if served.is_empty() {
        sections.iter().map(|s| s.section.clone()).collect()
    } else {
        served
    }
}

/// Where a library's first folder is on this server under `mappings`, or
/// the folder itself.
pub fn local_path(mappings: &[PathMappingEntry], library: &str, root: &str) -> String {
    mapped(mappings, library, root).unwrap_or_else(|| root.to_owned())
}

/// `mappings` with library `library`'s folder `root` at `local`: its own
/// rule for that folder replaced, and none when `local` is where Plex has
/// it already.
pub fn with_path(
    mappings: &[PathMappingEntry],
    library: &str,
    root: &str,
    local: &str,
) -> Vec<PathMappingEntry> {
    let same = |a: &str, b: &str| a.trim_end_matches('/') == b.trim_end_matches('/');
    let mut out: Vec<PathMappingEntry> = mappings
        .iter()
        .filter(|m| !(m.library.as_deref() == Some(library) && same(&m.from, root)))
        .cloned()
        .collect();
    let local = local.trim();
    if !local.is_empty() && !same(local, root) {
        out.push(PathMappingEntry {
            library: Some(library.to_owned()),
            from: root.to_owned(),
            to: local.trim_end_matches('/').to_owned(),
        });
    }
    out
}

/// A library's sample as the add flow's last column says it.
pub fn sample_line(check: &LibraryPathCheck) -> (bool, String) {
    match sample(check) {
        Sample::Ok { of } => (true, format!("OK · {of} of {of}")),
        Sample::Missing { missing, of } => (false, format!("{missing} of {of} missing")),
        Sample::Empty => (true, "No files yet".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spoolframe_admin_api::{PlexServerEntry, PlexStatus, SyncDirection};

    fn section(key: &str, library: Option<&str>) -> LibraryStatus {
        LibraryStatus {
            section: key.into(),
            title: format!("Section {key}"),
            kind: LibraryKind::Movies,
            items: 3,
            library: library.map(str::to_owned),
        }
    }

    fn library(name: &str, server: &str, error: Option<&str>) -> LibraryEntry {
        LibraryEntry {
            id: format!("plex:{server}:1"),
            name: name.into(),
            kind: LibraryKind::Movies,
            server: server.into(),
            server_name: server.to_uppercase(),
            section: "1".into(),
            plex_title: error.is_none().then(|| "Movies".into()),
            items: error.is_none().then_some(1204),
            paths: None,
            error: error.map(str::to_owned),
            sync: SyncDirection::Off,
        }
    }

    fn server(id: &str, reachable: bool) -> PlexServerEntry {
        PlexServerEntry {
            id: id.into(),
            name: id.to_uppercase(),
            status: PlexStatus {
                reachable,
                error: None,
                server_name: None,
                version: None,
                url: None,
                libraries: Vec::new(),
                unsupported: Vec::new(),
            },
            token_set: false,
            token_locked: false,
            locked: false,
            path_mapping: Vec::new(),
        }
    }

    #[test]
    fn a_library_s_size_is_counted_in_what_it_holds() {
        assert_eq!(items_line(LibraryKind::Movies, Some(1204)), "1,204 movies");
        assert_eq!(items_line(LibraryKind::Shows, Some(1)), "1 show");
        assert_eq!(items_line(LibraryKind::Music, Some(12)), "12 artists");
        assert_eq!(items_line(LibraryKind::Movies, None), "—");
    }

    #[test]
    fn a_library_says_where_it_comes_from() {
        assert_eq!(
            source_line(&library("Films", "tower", None)),
            "Plex · TOWER · Movies"
        );
    }

    #[test]
    fn a_library_on_a_server_that_is_down_is_unavailable_whatever_its_paths() {
        let down = library("Cabin movies", "cabin", Some("cannot reach http://cabin"));
        assert_eq!(
            path_cell(&down, false),
            PathCell::Unavailable("CABIN can't be reached".into())
        );
        let gone = library("Old", "tower", Some("no section 9"));
        assert_eq!(
            path_cell(&gone, true),
            PathCell::Unavailable("TOWER no longer has it".into())
        );
        let mut fine = library("Films", "tower", None);
        assert_eq!(path_cell(&fine, true), PathCell::Unknown);
        fine.paths = Some(LibraryPathCheck {
            library: "Movies".into(),
            roots: Vec::new(),
            checked: 5,
            ok: 3,
            failures: Vec::new(),
            suggestion: None,
        });
        assert_eq!(path_cell(&fine, true), PathCell::Missing(2));
    }

    #[test]
    fn a_down_server_s_card_names_only_its_libraries() {
        let overview = LibrariesOverview {
            libraries: vec![
                library("Films", "tower", None),
                library("Cabin movies", "cabin", Some("down")),
                library("Cabin TV", "cabin", Some("down")),
            ],
            servers: vec![server("tower", true), server("cabin", false)],
            chosen: true,
            locked: false,
        };
        let down = down(&overview);
        assert_eq!(
            down,
            [(
                "CABIN".to_owned(),
                vec!["Cabin movies".to_owned(), "Cabin TV".to_owned()]
            )]
        );
        assert_eq!(
            down_text(&down[0].1, 3),
            "Only Cabin movies and Cabin TV are unavailable; the other libraries work."
        );
        assert_eq!(
            down_text(&["Cabin movies".into()], 3),
            "Only Cabin movies is unavailable; the other libraries work."
        );
        assert_eq!(
            down_text(&["Cabin movies".into()], 1),
            "Its libraries are unavailable until it is back."
        );
    }

    #[test]
    fn the_wizard_adds_what_is_checked_and_removes_what_is_not() {
        let sections = [
            section("1", Some("plex:1")),
            section("2", Some("plex:2")),
            section("3", None),
        ];
        assert_eq!(
            choice(&sections, &["1".into(), "3".into()]),
            Choice {
                add: vec!["3".into()],
                remove: vec!["plex:2".into()],
            }
        );
        // As the server has them: nothing to do.
        assert_eq!(choice(&sections, &checked(&sections)), Choice::default());
    }

    #[test]
    fn a_server_with_no_library_yet_starts_with_every_box_checked() {
        let sections = [section("1", None), section("2", None)];
        assert_eq!(checked(&sections), ["1", "2"]);
    }

    fn rule(library: Option<&str>, from: &str, to: &str) -> PathMappingEntry {
        PathMappingEntry {
            library: library.map(str::to_owned),
            from: from.into(),
            to: to.into(),
        }
    }

    #[test]
    fn a_library_s_path_replaces_only_its_own_rule() {
        let saved = vec![
            rule(None, "/data", "/mnt/nas"),
            rule(Some("Movies"), "/data/movies", "/old"),
        ];
        assert_eq!(
            with_path(&saved, "Movies", "/data/movies/", "/mnt/cabin/movies/"),
            [
                rule(None, "/data", "/mnt/nas"),
                rule(Some("Movies"), "/data/movies/", "/mnt/cabin/movies"),
            ]
        );
        // Where Plex has it: no rule of its own.
        assert_eq!(
            with_path(&saved, "Movies", "/data/movies", "/data/movies"),
            [rule(None, "/data", "/mnt/nas")]
        );
        assert_eq!(local_path(&saved, "Movies", "/data/movies"), "/old");
        assert_eq!(local_path(&saved, "Kids", "/data/kids"), "/mnt/nas/kids");
        assert_eq!(local_path(&[], "Kids", "/data/kids"), "/data/kids");
    }

    #[test]
    fn the_add_button_counts_what_it_adds() {
        assert_eq!(add_label(2), "Add 2 libraries");
        assert_eq!(add_label(1), "Add 1 library");
        assert_eq!(added_text(2, "Cabin"), "2 libraries added from Cabin");
    }
}
