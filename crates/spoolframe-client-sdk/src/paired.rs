//! Experimental (`federation`): the paired servers a user sees, as the
//! libraries and Home name them ([`Session::paired_servers`]).
//!
//! An empty federation (no paired server, or a portable account, which
//! sees nothing federated) is the app as it is without federation: no
//! groups, no badges, no error.

use spoolframe_protocol::HomeScreen;
use spoolframe_types::{Library, LibraryServer, MediaId};

use crate::{SdkError, Session};

/// A paired server the user sees ([`Session::paired_servers`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharingServer {
    pub server: LibraryServer,
    /// What it shares with the user, in the order listed. An offline
    /// server keeps its libraries: they browse from the copy the user's
    /// server keeps.
    pub shared_libraries: Vec<Library>,
    /// Its row shows on Home ([`Session::set_server_row`]).
    pub home_row_shown: bool,
}

/// Where an item is from ([`PairedServers::server_of`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerOf {
    /// The user's own server.
    Own,
    Paired(LibraryServer),
    /// A paired server's item no longer available: the server was
    /// unpaired, or its library is no longer shared. Shown as such, and
    /// not played.
    Unavailable,
}

/// The paired servers the user sees, with what Home says of them: online
/// ones by name, then offline ones by name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PairedServers {
    servers: Vec<SharingServer>,
    /// Home's items that are no longer available.
    unavailable: Vec<MediaId>,
}

/// The paired servers' order: online first, each half by name.
fn order(server: &LibraryServer) -> (bool, String) {
    (!server.online, server.name.to_lowercase())
}

impl PairedServers {
    /// From the libraries and Home: a server is listed by its libraries,
    /// or by its Home row when it shares nothing now. A server both name
    /// is as Home says it, read later, with whether it is online.
    pub fn of(libraries: &[Library], home: Option<&HomeScreen>) -> Self {
        let mut servers: Vec<SharingServer> = Vec::new();
        for library in libraries {
            let Some(server) = &library.server else {
                continue;
            };
            match servers
                .iter_mut()
                .find(|p| p.server.server_id == server.server_id)
            {
                Some(paired) => paired.shared_libraries.push(library.clone()),
                None => servers.push(SharingServer {
                    server: server.clone(),
                    shared_libraries: vec![library.clone()],
                    home_row_shown: false,
                }),
            }
        }
        for row in home.iter().flat_map(|h| &h.servers) {
            match servers
                .iter_mut()
                .find(|p| p.server.server_id == row.server.server_id)
            {
                Some(paired) => {
                    paired.server = row.server.clone();
                    paired.home_row_shown = row.shown;
                }
                None => servers.push(SharingServer {
                    server: row.server.clone(),
                    shared_libraries: Vec::new(),
                    home_row_shown: row.shown,
                }),
            }
        }
        servers.sort_by_key(|p| order(&p.server));
        Self {
            servers,
            unavailable: home.map(|h| h.unavailable.clone()).unwrap_or_default(),
        }
    }

    pub fn servers(&self) -> &[SharingServer] {
        &self.servers
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// Where item `id` is from. An item of a server not listed here, or
    /// one Home names as no longer available, is
    /// [`ServerOf::Unavailable`].
    pub fn server_of(&self, id: &MediaId) -> ServerOf {
        let Some(origin) = id.origin() else {
            return ServerOf::Own;
        };
        if self.unavailable.contains(id) {
            return ServerOf::Unavailable;
        }
        self.servers
            .iter()
            .find(|p| p.server.server_id == origin)
            .map_or(ServerOf::Unavailable, |p| {
                ServerOf::Paired(p.server.clone())
            })
    }

    /// The libraries by server: the user's own first, then the paired
    /// servers online by name, then those offline by name, each group's
    /// libraries in the order listed. A server sharing nothing has no
    /// group.
    pub fn library_groups(libraries: &[Library]) -> Vec<LibraryGroup> {
        let mut groups: Vec<LibraryGroup> = Vec::new();
        for library in libraries {
            let id = library.server.as_ref().map(|s| &s.server_id);
            match groups
                .iter_mut()
                .find(|g| g.server.as_ref().map(|s| &s.server_id) == id)
            {
                Some(group) => group.libraries.push(library.clone()),
                None => groups.push(LibraryGroup {
                    server: library.server.clone(),
                    libraries: vec![library.clone()],
                }),
            }
        }
        groups.sort_by_key(|g| g.server.as_ref().map(order));
        groups
    }
}

/// A server's libraries, for a sidebar grouped by server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryGroup {
    /// The paired server; `None` for the user's own.
    pub server: Option<LibraryServer>,
    pub libraries: Vec<Library>,
}

/// A paired server's library gone from the list while its server is still
/// there: it stopped sharing it ([`LibraryDiff::unshared`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unshared {
    pub server: LibraryServer,
    pub library: Library,
}

/// What changed between two listings of the libraries.
pub struct LibraryDiff;

impl LibraryDiff {
    /// The libraries of `before` no longer shared in `after`: a paired
    /// server's, gone while its server is still listed. A server gone
    /// whole was unpaired, which is not a library's to say.
    pub fn unshared(before: &[Library], after: &[Library]) -> Vec<Unshared> {
        before
            .iter()
            .filter_map(|l| Some((l, l.server.as_ref()?)))
            .filter(|(l, server)| {
                let still_paired = after
                    .iter()
                    .any(|a| a.server.as_ref().map(|s| &s.server_id) == Some(&server.server_id));
                still_paired && !after.iter().any(|a| a.id == l.id)
            })
            .map(|(l, server)| Unshared {
                server: server.clone(),
                library: l.clone(),
            })
            .collect()
    }
}

impl Session {
    /// What [`Session::paired_servers`] reads from; nothing is asked
    /// without [`crate::Experimental::federation`].
    async fn paired(&self) -> Result<PairedServers, SdkError> {
        if !self.experimental().federation {
            return Ok(PairedServers::default());
        }
        let (libraries, home) = tokio::join!(self.libraries(), self.home());
        Ok(PairedServers::of(&libraries?, Some(&home?)))
    }

    /// The paired servers the user sees, with what each shares and
    /// whether its Home row shows; none without
    /// [`crate::Experimental::federation`].
    pub async fn paired_servers(&self) -> Result<Vec<SharingServer>, SdkError> {
        Ok(self.paired().await?.servers)
    }

    /// Where item `id` is from ([`PairedServers::server_of`]).
    pub async fn server_of(&self, id: &MediaId) -> Result<ServerOf, SdkError> {
        if id.origin().is_none() {
            return Ok(ServerOf::Own);
        }
        Ok(self.paired().await?.server_of(id))
    }

    /// The libraries grouped by server ([`PairedServers::library_groups`]).
    pub async fn library_groups(&self) -> Result<Vec<LibraryGroup>, SdkError> {
        Ok(PairedServers::library_groups(&self.libraries().await?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spoolframe_protocol::ServerRow;
    use spoolframe_types::{LibraryId, LibraryKind, MediaKind, Namespace};

    fn server(id: &str, name: &str, online: bool) -> LibraryServer {
        LibraryServer {
            server_id: id.into(),
            name: name.into(),
            online,
        }
    }

    fn library(key: &str, server: Option<LibraryServer>) -> Library {
        let own = LibraryId::new(Namespace::Plex, key).unwrap();
        Library {
            id: match &server {
                Some(s) => LibraryId::federated(&s.server_id, &own).unwrap(),
                None => own,
            },
            title: key.into(),
            kind: LibraryKind::Movies,
            server,
        }
    }

    fn home(rows: Vec<(LibraryServer, bool)>, unavailable: Vec<MediaId>) -> HomeScreen {
        HomeScreen {
            continue_watching: Vec::new(),
            next_up: Vec::new(),
            recently_added: Vec::new(),
            unavailable,
            servers: rows
                .into_iter()
                .map(|(server, shown)| ServerRow {
                    server,
                    shown,
                    items: Vec::new(),
                })
                .collect(),
        }
    }

    fn movie(origin: Option<&str>, key: &str) -> MediaId {
        let own = MediaId::new(Namespace::Plex, MediaKind::Movie, key).unwrap();
        match origin {
            Some(origin) => MediaId::federated(origin, &own).unwrap(),
            None => own,
        }
    }

    #[test]
    fn paired_servers_come_online_by_name_then_offline_with_what_they_share() {
        let libraries = [
            library("1", None),
            library("2", Some(server("dave", "Dave", false))),
            library("3", Some(server("zed", "Zed", true))),
            library("4", Some(server("bob", "Bob", true))),
            library("5", Some(server("bob", "Bob", true))),
        ];
        let rows = vec![
            (server("bob", "Bob", true), true),
            (server("cy", "Cy", true), false),
        ];
        let paired = PairedServers::of(&libraries, Some(&home(rows, Vec::new())));
        let listed: Vec<_> = paired
            .servers()
            .iter()
            .map(|p| {
                (
                    p.server.name.as_str(),
                    p.shared_libraries.len(),
                    p.home_row_shown,
                )
            })
            .collect();
        assert_eq!(
            listed,
            [
                ("Bob", 2, true),
                ("Cy", 0, false),
                ("Zed", 1, false),
                ("Dave", 1, false)
            ]
        );
    }

    #[test]
    fn home_says_a_server_went_offline() {
        let libraries = [library("2", Some(server("bob", "Bob", true)))];
        let rows = vec![(server("bob", "Bob", false), true)];
        let paired = PairedServers::of(&libraries, Some(&home(rows, Vec::new())));
        assert!(!paired.servers()[0].server.online);
    }

    #[test]
    fn an_item_is_its_servers_or_unavailable() {
        let gone = movie(Some("bob"), "9");
        let paired = PairedServers::of(
            &[library("2", Some(server("bob", "Bob", true)))],
            Some(&home(Vec::new(), vec![gone.clone()])),
        );
        assert_eq!(paired.server_of(&movie(None, "1")), ServerOf::Own);
        assert_eq!(
            paired.server_of(&movie(Some("bob"), "1")),
            ServerOf::Paired(server("bob", "Bob", true))
        );
        assert_eq!(paired.server_of(&gone), ServerOf::Unavailable);
        assert_eq!(
            paired.server_of(&movie(Some("unpaired"), "1")),
            ServerOf::Unavailable
        );
    }

    #[test]
    fn this_servers_libraries_come_first_then_paired_ones_online_then_offline() {
        let libraries = [
            library("1", None),
            library("2", Some(server("dave", "Dave’s Den", false))),
            library("3", Some(server("zed", "Zed", true))),
            library("4", Some(server("bob", "Bob’s Media", true))),
            library("5", None),
        ];
        let groups = PairedServers::library_groups(&libraries);
        let order: Vec<Vec<&str>> = groups
            .iter()
            .map(|g| g.libraries.iter().map(|l| l.title.as_str()).collect())
            .collect();
        assert_eq!(order, [vec!["1", "5"], vec!["4"], vec!["3"], vec!["2"]]);
    }

    #[test]
    fn a_library_gone_while_its_server_stays_was_unshared() {
        let bob = server("bob", "Bob’s Media", true);
        let before = [
            library("1", Some(bob.clone())),
            library("2", Some(bob.clone())),
        ];
        let after = [library("1", Some(bob.clone()))];
        let gone = LibraryDiff::unshared(&before, &after);
        assert_eq!(gone.len(), 1);
        assert_eq!(
            (gone[0].library.title.as_str(), &gone[0].server),
            ("2", &bob)
        );
    }

    #[test]
    fn a_server_gone_whole_is_not_a_library_unshared() {
        let before = [library("1", Some(server("bob", "Bob", true)))];
        assert!(LibraryDiff::unshared(&before, &[]).is_empty());
    }
}
