//! What Server › Libraries, its add-library flow and the wizard's Plex step
//! decide (P3-11), apart from the pages: host-compiled, so `cargo test`
//! covers it.

use spoolframe_admin_api::{
    DiscoveredPlex, LibrariesOverview, LibraryEntry, LibraryKind, LibraryPathCheck, LibraryStatus,
    PathMappingEntry, PlexAccountServer,
};

use std::net::IpAddr;

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

/// Where a library dragged from row `from` and held over row `over` would
/// go, as the line drawn between rows shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropLine {
    Above,
    Below,
}

/// The line to draw on row `over` while row `from` is dragged over it:
/// none on the row itself, since dropping it there moves nothing.
pub fn drop_line(from: usize, over: usize) -> Option<DropLine> {
    match over.cmp(&from) {
        std::cmp::Ordering::Less => Some(DropLine::Above),
        std::cmp::Ordering::Greater => Some(DropLine::Below),
        std::cmp::Ordering::Equal => None,
    }
}

/// The position `libraryMove` is given when row `from` is dropped on row
/// `onto`: it takes that row's place, which is the line [`drop_line`]
/// drew. `None` when it moves nothing.
pub fn dropped_at(from: usize, onto: usize) -> Option<u32> {
    // Taken out first, the rows below `from` move up one: `onto`'s index
    // is then the place after it (dragged down) or its own (dragged up).
    drop_line(from, onto).and_then(|_| u32::try_from(onto).ok())
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

/// Under the Plex server choices, in the add flow and the wizard: why a
/// server anywhere will not do.
pub const FILES_HERE: &str = "Spoolframe plays the files itself, so a Plex server's media must be mounted on this machine. A server whose files aren't here can't be added.";

/// On a remote server's choice.
pub const REMOTE_NOTE: &str = "Remote · its files must be mounted here";

/// Whether the Plex server at `url` is away from this machine's network,
/// so its files are here only if someone mounted them. `lan` is what
/// plex.tv says of an account's server; `None` for one found or typed,
/// told by its address: a public IP, or a plex.direct name that is not a
/// LAN address's. A plain host name is taken as local: nothing says
/// otherwise.
pub fn remote(url: &str, lan: Option<bool>) -> bool {
    if let Some(lan) = lan {
        return !lan;
    }
    let host_port = crate::logic::host_port(url);
    let host = match host_port.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(v6),
        None => host_port.split(':').next().unwrap_or(host_port),
    };
    if let Some(name) = host.strip_suffix(".plex.direct") {
        // "192-168-1-10.<hash>": the address it stands for, dashed.
        let first = name.split('.').next().unwrap_or_default();
        return first
            .replace('-', ".")
            .parse::<IpAddr>()
            .map_or(true, |ip| !on_lan(ip));
    }
    host.parse::<IpAddr>().is_ok_and(|ip| !on_lan(ip))
}

/// A Plex server the wizard's Plex step offers (P3-10): found on this
/// network, listed by the Plex account signed in with, or both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlexChoice {
    pub name: String,
    /// Where it is read from: the address discovery found it at, else the
    /// one plex.tv says (a LAN one where it knows one).
    pub url: String,
    pub version: Option<String>,
    /// Found on this network by Plex's discovery.
    pub found: bool,
    /// Listed by the Plex account, with whether plex.tv says `url` is on
    /// the LAN; `None` when the account does not list it.
    pub account: Option<bool>,
}

impl PlexChoice {
    /// Its files are here only if someone mounted them: never chosen for
    /// the admin.
    pub fn remote(&self) -> bool {
        let lan = if self.found { None } else { self.account };
        remote(&self.url, lan)
    }
}

fn same_url(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

/// The servers `found` on the LAN, then the account's others, each once:
/// one both found and listed is the same server when Plex's id for it
/// (or its address) is the same, and is read at the address it was found
/// at. One only Plex's relay reaches is left out: Spoolframe never reads
/// through it.
pub fn plex_choices(found: &[DiscoveredPlex], account: &[PlexAccountServer]) -> Vec<PlexChoice> {
    let same = |f: &DiscoveredPlex, a: &PlexAccountServer| {
        f.machine_id.as_deref() == Some(a.machine_id.as_str())
            || a.url.as_deref().is_some_and(|u| same_url(u, &f.url))
    };
    let mut choices: Vec<PlexChoice> = found
        .iter()
        .map(|f| PlexChoice {
            name: f.name.clone(),
            url: f.url.clone(),
            version: f.version.clone(),
            found: true,
            account: account.iter().find(|a| same(f, a)).map(|a| a.local),
        })
        .collect();
    for a in account {
        if found.iter().any(|f| same(f, a)) {
            continue;
        }
        let Some(url) = &a.url else {
            continue;
        };
        choices.push(PlexChoice {
            name: a.name.clone(),
            url: url.clone(),
            version: None,
            found: false,
            account: Some(a.local),
        });
    }
    choices
}

/// What a choice says under its name: "Found on this network ·
/// 192.168.1.10:32400 · Plex Media Server 1.41", or "From your Plex
/// account · \<address\>", without an address too long to say for a remote
/// one (the remote note says the rest).
pub fn choice_line(choice: &PlexChoice) -> String {
    let host_port = crate::logic::host_port(&choice.url);
    if !choice.found {
        return if choice.remote() {
            "From your Plex account".to_owned()
        } else {
            format!("From your Plex account · {host_port}")
        };
    }
    let mut line = format!("Found on this network · {host_port}");
    if let Some(v) = &choice.version {
        let minor = v.split('.').take(2).collect::<Vec<_>>().join(".");
        line.push_str(&format!(" · Plex Media Server {minor}"));
    }
    line
}

/// What the Plex step chooses while the admin has not: the first server
/// that is not remote.
pub fn first_choice(choices: &[PlexChoice]) -> Option<String> {
    choices.iter().find(|c| !c.remote()).map(|c| c.url.clone())
}

/// A private, loopback or link-local address.
fn on_lan(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unique_local() || v6.is_unicast_link_local(),
    }
}

/// Why the add flow's chosen libraries cannot be added, from their
/// checks (`None` while one is still being checked): one sentence per
/// library none of whose files is here.
pub fn refusals<'a>(checks: impl IntoIterator<Item = Option<&'a LibraryPathCheck>>) -> Vec<String> {
    checks
        .into_iter()
        .flatten()
        .filter_map(LibraryPathCheck::refusal)
        .collect()
}

/// A library's sample as the add flow's last column says it.
pub fn sample_line(check: &LibraryPathCheck) -> (bool, String) {
    match sample(check) {
        Sample::Ok { of } => (true, format!("OK · {of} of {of}")),
        Sample::Missing { missing, of } if missing == of => {
            (false, format!("Not on this machine · 0 of {of}"))
        }
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

    fn found(name: &str, url: &str, machine: Option<&str>) -> DiscoveredPlex {
        DiscoveredPlex {
            name: name.into(),
            url: url.into(),
            version: Some("1.41.0".into()),
            machine_id: machine.map(str::to_owned),
        }
    }

    fn listed(name: &str, url: Option<&str>, local: bool) -> PlexAccountServer {
        PlexAccountServer {
            name: name.into(),
            machine_id: format!("{}-machine", name.to_lowercase()),
            url: url.map(str::to_owned),
            local,
            owned: true,
            added: None,
        }
    }

    #[test]
    fn the_wizard_lists_a_server_found_and_in_the_account_once_by_its_lan_address() {
        let choices = plex_choices(
            &[
                found("Tower", "http://192.168.1.10:32400", Some("tower-machine")),
                found("nas", "http://192.168.1.30:32400", None),
            ],
            &[
                listed(
                    "Tower",
                    Some("https://192-168-1-10.abc.plex.direct:32400"),
                    true,
                ),
                listed(
                    "fulcrum-ng",
                    Some("https://192-168-1-40.abc.plex.direct:32400"),
                    true,
                ),
                listed(
                    "Cabin",
                    Some("https://1-2-3-4.def.plex.direct:32400"),
                    false,
                ),
                listed("Far", None, false),
            ],
        );
        let names: Vec<_> = choices.iter().map(|c| c.name.as_str()).collect();
        // Found first, then the account's others; one only Plex's relay
        // reaches is not offered.
        assert_eq!(names, ["Tower", "nas", "fulcrum-ng", "Cabin"]);
        assert_eq!(choices[0].url, "http://192.168.1.10:32400");
        assert!(choices[0].found && choices[0].account.is_some());
        assert!(choices[1].account.is_none());
        assert!(!choices[2].found && !choices[2].remote());
        assert!(choices[3].remote(), "plex.tv says Cabin is not on the LAN");
        assert_eq!(
            choice_line(&choices[0]),
            "Found on this network · 192.168.1.10:32400 · Plex Media Server 1.41"
        );
        assert_eq!(
            choice_line(&choices[2]),
            "From your Plex account · 192-168-1-40.abc.plex.direct:32400"
        );
        // Its plex.direct name is too long to say; the remote note says
        // the rest.
        assert_eq!(choice_line(&choices[3]), "From your Plex account");
        // Nothing chosen yet: the first that is not remote.
        assert_eq!(first_choice(&choices[3..]), None);
        assert_eq!(
            first_choice(&choices[2..]).as_deref(),
            Some(choices[2].url.as_str())
        );
    }

    #[test]
    fn a_server_found_without_an_id_is_still_matched_to_the_account_by_its_address() {
        let choices = plex_choices(
            &[found("Tower", "http://192.168.1.10:32400/", None)],
            &[listed("Tower", Some("http://192.168.1.10:32400"), true)],
        );
        assert_eq!(choices.len(), 1);
        assert!(choices[0].account.is_some());
    }

    #[test]
    fn plex_tv_s_word_on_an_account_server_decides_whether_it_is_remote() {
        assert!(remote("https://1-2-3-4.abc.plex.direct:32400", Some(false)));
        assert!(!remote("http://8.8.8.8:32400", Some(true)));
    }

    #[test]
    fn a_found_or_typed_server_is_remote_by_its_address() {
        assert!(remote("http://8.8.8.8:32400", None));
        assert!(remote("https://8-8-8-8.abc123.plex.direct:32400/", None));
        assert!(remote("https://abc123.plex.direct:32400", None));
        assert!(remote("http://[2001:db8::1]:32400", None));
        assert!(!remote("http://192.168.1.10:32400", None));
        assert!(!remote(
            "https://192-168-1-10.abc123.plex.direct:32400",
            None
        ));
        assert!(!remote("http://10.0.0.5:32400", None));
        assert!(!remote("http://127.0.0.1:32400", None));
        assert!(!remote("http://[fd00::5]:32400", None));
        assert!(!remote("http://tower.lan:32400", None));
        assert!(!remote("tower:32400", None));
    }

    fn sampled(checked: u64, ok: u64) -> LibraryPathCheck {
        LibraryPathCheck {
            library: "Movies".into(),
            roots: vec!["/data/movies".into()],
            checked,
            ok,
            failures: Vec::new(),
            suggestion: None,
        }
    }

    #[test]
    fn a_library_with_no_sample_file_here_is_said_to_be_not_here() {
        assert_eq!(
            sample_line(&sampled(5, 0)),
            (false, "Not on this machine · 0 of 5".to_owned())
        );
        assert_eq!(
            sample_line(&sampled(5, 3)),
            (false, "2 of 5 missing".to_owned())
        );
    }

    #[test]
    fn only_the_checked_libraries_not_on_this_machine_hold_the_add_up() {
        let here = sampled(5, 5);
        let gone = sampled(5, 0);
        assert_eq!(refusals([Some(&here), None]), Vec::<String>::new());
        assert_eq!(
            refusals([Some(&here), Some(&gone)]),
            [gone.refusal().unwrap()]
        );
    }

    #[test]
    fn the_add_button_counts_what_it_adds() {
        assert_eq!(add_label(2), "Add 2 libraries");
        assert_eq!(add_label(1), "Add 1 library");
        assert_eq!(added_text(2, "Cabin"), "2 libraries added from Cabin");
    }

    /// `libraryMove` takes the library out, then puts it at the position:
    /// what the server does with the list, to check a drop against.
    fn moved(list: &[&'static str], from: usize, position: u32) -> Vec<&'static str> {
        let mut list = list.to_vec();
        let library = list.remove(from);
        list.insert((position as usize).min(list.len()), library);
        list
    }

    #[test]
    fn a_library_dropped_on_a_row_below_goes_after_it() {
        let list = ["A", "B", "C", "D"];
        assert_eq!(drop_line(0, 2), Some(DropLine::Below));
        let position = dropped_at(0, 2).unwrap();
        assert_eq!(moved(&list, 0, position), ["B", "C", "A", "D"]);
        assert_eq!(
            moved(&list, 0, dropped_at(0, 3).unwrap()),
            ["B", "C", "D", "A"]
        );
    }

    #[test]
    fn a_library_dropped_on_a_row_above_goes_before_it() {
        let list = ["A", "B", "C", "D"];
        assert_eq!(drop_line(3, 1), Some(DropLine::Above));
        let position = dropped_at(3, 1).unwrap();
        assert_eq!(moved(&list, 3, position), ["A", "D", "B", "C"]);
        assert_eq!(
            moved(&list, 3, dropped_at(3, 0).unwrap()),
            ["D", "A", "B", "C"]
        );
    }

    #[test]
    fn a_library_dropped_on_itself_does_not_move() {
        assert_eq!(drop_line(2, 2), None);
        assert_eq!(dropped_at(2, 2), None);
    }
}
