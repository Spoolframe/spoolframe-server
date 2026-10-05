//! Libraries from several Plex servers as one catalog (P3-11).
//!
//! Each call goes to the Plex server its ID names ([`super::ids`]); the
//! library listing is the configured one, and search and recently added
//! ask every server and merge; a search in one library asks only its
//! server. A server that is down fails only the calls
//! that need it: its libraries stay listed, and opening one says it is
//! unavailable, while search and Home go on with the others.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use spoolframe_catalog::{
    Browse, Catalog, CatalogError, Found, FoundGroup, Image, Inspect, Inspected, ItemPage,
    ItemWithChildren, LibraryCheck, MissingFile, Page, PreviewFrames, Row, RowPage, SearchQuery,
};
use spoolframe_plex::{Dated, PlexCatalog};
use spoolframe_types::{ArtworkId, Item, Library, LibraryId, MediaId, MediaKind, PersonId};

use spoolframe_admin_api::SyncDirection;

use super::ids;
use crate::config::FIRST_PLEX_SERVER;

/// One Plex server the catalog reads from.
#[derive(Debug, Clone)]
pub struct PlexSource {
    /// [`FIRST_PLEX_SERVER`], or its id in `plex.servers`.
    pub id: String,
    /// What Spoolframe calls it.
    pub name: String,
    /// Serving only its sections that are libraries here.
    pub catalog: Arc<PlexCatalog>,
}

/// A library as configured: what clients see, and where it comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Served {
    /// Its id names its server and section; its title is Spoolframe's.
    pub library: Library,
    pub server: String,
    pub section: String,
    /// Which way its watch state syncs with linked Plex accounts (P3-12).
    pub sync: SyncDirection,
}

/// Which of a Plex server's sections sync with linked Plex accounts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Synced {
    /// These: each section's key, its direction and its library's name.
    Sections(Vec<(String, SyncDirection, String)>),
    /// Every movie and show section, this way: libraries not chosen yet.
    Every(SyncDirection),
}

/// Every configured Plex server and library, as one [`Catalog`].
#[derive(Debug)]
pub struct Composite {
    /// The first server first, then the others in order. Never empty.
    servers: Vec<PlexSource>,
    /// `None`: every library of the first server, under Plex's names, as a
    /// server configured before libraries were chosen serves.
    libraries: Option<Vec<Served>>,
    /// Which way those libraries sync while they are not chosen.
    default_sync: SyncDirection,
}

impl Composite {
    /// `servers` start with the first one; each catalog already serves
    /// only its sections in `libraries`.
    pub fn new(servers: Vec<PlexSource>, libraries: Option<Vec<Served>>) -> Self {
        assert!(
            servers.first().is_some_and(|s| s.id == FIRST_PLEX_SERVER),
            "the first Plex server comes first"
        );
        Self {
            servers,
            libraries,
            default_sync: SyncDirection::Off,
        }
    }

    /// Libraries not chosen yet sync `sync` (P3-12).
    pub fn with_default_sync(mut self, sync: SyncDirection) -> Self {
        self.default_sync = sync;
        self
    }

    /// Which of `server`'s sections sync, and how (P3-12): the chosen
    /// libraries' that are not off, or, while none are chosen, every one
    /// of the first server's.
    pub fn synced(&self, server: &str) -> Synced {
        match &self.libraries {
            Some(libraries) => Synced::Sections(
                libraries
                    .iter()
                    .filter(|l| l.server == server && l.sync != SyncDirection::Off)
                    .map(|l| (l.section.clone(), l.sync, l.library.title.clone()))
                    .collect(),
            ),
            None if server == FIRST_PLEX_SERVER && self.default_sync != SyncDirection::Off => {
                Synced::Every(self.default_sync)
            }
            None => Synced::Sections(Vec::new()),
        }
    }

    /// One Plex server and all its libraries: a server as it was before
    /// libraries were chosen.
    pub fn single(name: impl Into<String>, plex: PlexCatalog) -> Self {
        Self::new(
            vec![PlexSource {
                id: FIRST_PLEX_SERVER.to_owned(),
                name: name.into(),
                catalog: Arc::new(plex),
            }],
            None,
        )
    }

    pub fn servers(&self) -> &[PlexSource] {
        &self.servers
    }

    pub fn server(&self, id: &str) -> Option<&PlexSource> {
        self.servers.iter().find(|s| s.id == id)
    }

    /// The first Plex server: the one PX-04's watch sync is with.
    pub fn first(&self) -> &PlexSource {
        &self.servers[0]
    }

    /// The configured libraries, or `None` when every library of the
    /// first server is served.
    pub fn configured(&self) -> Option<&[Served]> {
        self.libraries.as_deref()
    }

    /// The server `id` names. A server no longer configured serves
    /// nothing: its items are not served, not gone, so their watch state
    /// stays where it is.
    fn source(&self, server: &str, id: &impl ToString) -> Result<&PlexSource, CatalogError> {
        self.server(server)
            .ok_or_else(|| CatalogError::NotServed(id.to_string()))
    }

    /// Ask every server at once; each one's answer, in server order.
    async fn each<T, F, Fut>(&self, call: F) -> Vec<(&PlexSource, Result<T, CatalogError>)>
    where
        T: Send + 'static,
        F: Fn(Arc<PlexCatalog>) -> Fut,
        Fut: Future<Output = Result<T, CatalogError>> + Send + 'static,
    {
        let mut tasks = tokio::task::JoinSet::new();
        for (i, source) in self.servers.iter().enumerate() {
            let asked = call(source.catalog.clone());
            tasks.spawn(async move { (i, asked.await) });
        }
        let mut answers: Vec<Option<Result<T, CatalogError>>> =
            self.servers.iter().map(|_| None).collect();
        while let Some(done) = tasks.join_next().await {
            match done {
                Ok((i, answer)) => answers[i] = Some(answer),
                Err(e) => std::panic::resume_unwind(e.into_panic()),
            }
        }
        self.servers
            .iter()
            .zip(answers)
            .map(|(s, a)| (s, a.expect("every task answered")))
            .collect()
    }

    /// The answers that came, a down server's left out and logged; the
    /// first error if every server failed.
    fn reachable<'a, T>(
        what: &str,
        answers: Vec<(&'a PlexSource, Result<T, CatalogError>)>,
    ) -> Result<Vec<(&'a PlexSource, T)>, CatalogError> {
        let mut ok = Vec::new();
        let mut first_error = None;
        for (source, answer) in answers {
            match answer {
                Ok(value) => ok.push((source, value)),
                Err(e) => {
                    tracing::warn!(server = source.name, error = %e, "{what}: left out");
                    first_error.get_or_insert(e);
                }
            }
        }
        match first_error {
            Some(e) if ok.is_empty() => Err(e),
            _ => Ok(ok),
        }
    }
}

/// `e` about `local`, as about `ours`: errors name the ID the caller used.
fn named(e: CatalogError, local: &impl ToString, ours: &impl ToString) -> CatalogError {
    let rename = |what: String| {
        if what == local.to_string() {
            ours.to_string()
        } else {
            what
        }
    };
    match e {
        CatalogError::NotFound(what) => CatalogError::NotFound(rename(what)),
        CatalogError::NotServed(what) => CatalogError::NotServed(rename(what)),
        other => other,
    }
}

/// Each kind `query` asked for, from every server's answer: taken from
/// each server in turn, best first, and their totals summed.
fn merge(query: &SearchQuery, mut answers: Vec<Found>) -> Found {
    let groups = query
        .kinds
        .iter()
        .map(|&kind| {
            let of_kind = answers
                .iter_mut()
                .filter_map(|found| {
                    let at = found.groups.iter().position(|g| g.kind == kind)?;
                    Some(found.groups.swap_remove(at))
                })
                .collect();
            FoundGroup::merge(kind, of_kind, query.limit)
        })
        .collect();
    Found { groups }
}

/// Newest first across servers; an item without a date after the dated
/// ones, in its server's order.
fn newest_first(lists: Vec<Vec<Dated>>, limit: usize) -> Vec<Item> {
    let mut all: Vec<Dated> = lists.into_iter().flatten().collect();
    // Stable: equal dates keep the servers' order.
    all.sort_by_key(|(at, _)| std::cmp::Reverse(at.unwrap_or(i64::MIN)));
    all.into_iter().take(limit).map(|(_, item)| item).collect()
}

#[async_trait]
impl Catalog for Composite {
    async fn libraries(&self) -> Result<Vec<Library>, CatalogError> {
        match &self.libraries {
            // As configured: a library is listed whether or not its server
            // answers now.
            Some(served) => Ok(served.iter().map(|s| s.library.clone()).collect()),
            None => self.first().catalog.libraries().await,
        }
    }

    async fn items(&self, library: &LibraryId, page: Page) -> Result<ItemPage, CatalogError> {
        let (server, local) = ids::library_local(library);
        let source = self.source(server, library)?;
        let page = source
            .catalog
            .items(&local, page)
            .await
            .map_err(|e| named(e, &local, library))?;
        Ok(ItemPage {
            items: page
                .items
                .into_iter()
                .map(|i| ids::item(server, i))
                .collect(),
            total: page.total,
        })
    }

    async fn item(&self, id: &MediaId) -> Result<ItemWithChildren, CatalogError> {
        let (server, local) = ids::media_local(id);
        let found = self
            .source(server, id)?
            .catalog
            .item(&local)
            .await
            .map_err(|e| named(e, &local, id))?;
        Ok(ItemWithChildren {
            item: ids::item(server, found.item),
            children: found
                .children
                .into_iter()
                .map(|c| ids::item(server, c))
                .collect(),
            ..found
        })
    }

    async fn external_ids(&self, id: &MediaId) -> Result<Vec<String>, CatalogError> {
        let (server, local) = ids::media_local(id);
        self.source(server, id)?
            .catalog
            .external_ids(&local)
            .await
            .map_err(|e| named(e, &local, id))
    }

    async fn search(&self, query: &SearchQuery) -> Result<Found, CatalogError> {
        if let Some(library) = &query.library {
            let (server, local) = ids::library_local(library);
            let source = self.source(server, library)?;
            let asked = SearchQuery {
                library: Some(local.clone()),
                ..query.clone()
            };
            let found = source
                .catalog
                .find(&asked)
                .await
                .map_err(|e| named(e, &local, library))?;
            return Ok(ids::found(server, found));
        }
        let answers = self
            .each(|plex| {
                let query = query.clone();
                async move { plex.find(&query).await }
            })
            .await;
        let answers = Self::reachable("search", answers)?
            .into_iter()
            .map(|(source, found)| ids::found(&source.id, found))
            .collect();
        Ok(merge(query, answers))
    }

    async fn person_items(&self, id: &PersonId) -> Result<Vec<Item>, CatalogError> {
        let (server, local) = ids::person_local(id);
        let items = self
            .source(server, id)?
            .catalog
            .person_items(&local)
            .await
            .map_err(|e| named(e, &local, id))?;
        Ok(items.into_iter().map(|i| ids::item(server, i)).collect())
    }

    async fn media_path(&self, id: &MediaId) -> Result<PathBuf, CatalogError> {
        let (server, local) = ids::media_local(id);
        self.source(server, id)?
            .catalog
            .media_path(&local)
            .await
            .map_err(|e| named(e, &local, id))
    }

    async fn preview_frames(&self, id: &MediaId) -> Result<Option<PreviewFrames>, CatalogError> {
        let (server, local) = ids::media_local(id);
        self.source(server, id)?
            .catalog
            .preview_frames(&local)
            .await
            .map_err(|e| named(e, &local, id))
    }

    async fn recently_added(&self, limit: u32) -> Result<Vec<Item>, CatalogError> {
        let answers = self
            .each(|plex| async move { plex.recently_added_dated(limit).await })
            .await;
        let lists = Self::reachable("recently added", answers)?
            .into_iter()
            .map(|(source, items)| {
                items
                    .into_iter()
                    .map(|(at, i)| (at, ids::item(&source.id, i)))
                    .collect()
            })
            .collect();
        Ok(newest_first(
            lists,
            usize::try_from(limit).unwrap_or(usize::MAX),
        ))
    }

    async fn artwork(&self, id: &ArtworkId) -> Result<Image, CatalogError> {
        let (server, local) = ids::artwork_local(id);
        self.source(server, id)?
            .catalog
            .artwork(&local)
            .await
            .map_err(|e| named(e, &local, id))
    }
}

/// `row` from `server`, every ID in it naming that server.
fn row(server: &str, row: Row) -> Row {
    Row {
        item: ids::item(server, row.item),
        ..row
    }
}

fn rows(server: &str, rows: Vec<Row>) -> Vec<Row> {
    rows.into_iter().map(|r| row(server, r)).collect()
}

impl Composite {
    /// The library a Plex server's section is here: its configured one,
    /// or while none are chosen, the first server's under Plex's name.
    pub fn served_library(&self, server: &str, section: &str) -> Option<&Served> {
        self.libraries
            .as_deref()?
            .iter()
            .find(|l| l.server == server && l.section == section)
    }
}

/// An admin's look into each library, at the Plex server its ID names
/// (P3-16).
#[async_trait]
impl Inspect for Composite {
    async fn browse(&self, library: &LibraryId, query: &Browse) -> Result<RowPage, CatalogError> {
        let (server, local) = ids::library_local(library);
        let page = self
            .source(server, library)?
            .catalog
            .browse(&local, query)
            .await
            .map_err(|e| named(e, &local, library))?;
        Ok(RowPage {
            rows: rows(server, page.rows),
            total: page.total,
        })
    }

    async fn count(&self, library: &LibraryId, kind: MediaKind) -> Result<u32, CatalogError> {
        let (server, local) = ids::library_local(library);
        self.source(server, library)?
            .catalog
            .count(&local, kind)
            .await
            .map_err(|e| named(e, &local, library))
    }

    async fn child_rows(&self, id: &MediaId) -> Result<Vec<Row>, CatalogError> {
        let (server, local) = ids::media_local(id);
        let found = self
            .source(server, id)?
            .catalog
            .child_rows(&local)
            .await
            .map_err(|e| named(e, &local, id))?;
        Ok(rows(server, found))
    }

    async fn inspect(&self, id: &MediaId) -> Result<Inspected, CatalogError> {
        let (server, local) = ids::media_local(id);
        let seen = self
            .source(server, id)?
            .catalog
            .inspect(&local)
            .await
            .map_err(|e| named(e, &local, id))?;
        Ok(Inspected {
            found: ItemWithChildren {
                item: ids::item(server, seen.found.item),
                children: seen
                    .found
                    .children
                    .into_iter()
                    .map(|c| ids::item(server, c))
                    .collect(),
                ..seen.found
            },
            library: seen.library.map(|l| ids::library(server, l)),
            ancestors: seen
                .ancestors
                .into_iter()
                .map(|(id, title)| (ids::media(server, id), title))
                .collect(),
            children: rows(server, seen.children),
            ..seen
        })
    }

    async fn check_library(&self, library: &LibraryId) -> Result<LibraryCheck, CatalogError> {
        let (server, local) = ids::library_local(library);
        let check = self
            .source(server, library)?
            .catalog
            .check_library(&local)
            .await
            .map_err(|e| named(e, &local, library))?;
        Ok(LibraryCheck {
            ok: check.ok,
            missing: check
                .missing
                .into_iter()
                .map(|m| MissingFile {
                    row: row(server, m.row),
                    file: m.file,
                })
                .collect(),
        })
    }

    async fn backend_link(&self, id: &MediaId) -> Result<Option<String>, CatalogError> {
        let (server, local) = ids::media_local(id);
        self.source(server, id)?
            .catalog
            .backend_link(&local)
            .await
            .map_err(|e| named(e, &local, id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spoolframe_types::{Movie, PlaybackState, SearchKind};

    fn movie(key: &str) -> Item {
        Item::Movie(Movie {
            id: format!("plex:movie:{key}").parse().unwrap(),
            title: key.into(),
            year: None,
            summary: None,
            content_rating: None,
            duration: None,
            artwork: Vec::new(),
            playback: PlaybackState::default(),
        })
    }

    fn keys(items: &[Item]) -> Vec<String> {
        items.iter().map(|i| i.id().key().to_owned()).collect()
    }

    #[test]
    fn search_takes_each_kind_from_each_server_in_turn_and_sums_totals() {
        let found = |items: Vec<Item>, total| Found {
            groups: vec![
                FoundGroup::empty(SearchKind::Shows),
                FoundGroup {
                    items,
                    total,
                    ..FoundGroup::empty(SearchKind::Movies)
                },
            ],
        };
        let query = SearchQuery {
            text: "a".into(),
            year: None,
            kinds: vec![SearchKind::Movies, SearchKind::Shows],
            library: None,
            limit: 3,
        };
        let a = found(vec![movie("1"), movie("2"), movie("3")], 30);
        let b = found(vec![movie("b:1")], 1);
        let merged = merge(&query, vec![a, b]);
        let kinds: Vec<_> = merged.groups.iter().map(|g| g.kind).collect();
        assert_eq!(kinds, [SearchKind::Movies, SearchKind::Shows]);
        assert_eq!(keys(&merged.groups[0].items), ["1", "b:1", "2"]);
        assert_eq!(merged.groups[0].total, 31);
    }

    #[test]
    fn recently_added_is_newest_first_across_servers() {
        let a = vec![(Some(30), movie("1")), (Some(10), movie("2"))];
        let b = vec![(Some(20), movie("b:1")), (None, movie("b:2"))];
        assert_eq!(
            keys(&newest_first(vec![a, b], 10)),
            ["1", "b:1", "2", "b:2"]
        );
        assert_eq!(
            keys(&newest_first(
                vec![vec![(Some(1), movie("1"))], vec![(Some(2), movie("b:1"))]],
                1
            )),
            ["b:1"]
        );
    }
}
