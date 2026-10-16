//! Scoped search (P3-09): each kind is asked of Plex on its own, in the
//! sections that hold it, so the scope is Plex's to apply.
//!
//! Checked against PMS 1.43: `/hubs/search` cannot be scoped. It ignores
//! `sectionId` and `type`, gives each hub only as many as `limit` with
//! `size` the number returned and `more` always false, and matches beyond
//! titles (a show's name finds all its episodes; "expanse" finds The Great
//! Gatsby). `/library/sections/{id}/search?type=` matches titles in one
//! section and kind, takes `year`, and pages with an exact `totalSize`. So
//! items come from the sections, and only people from the hubs: Plex
//! lists actors and directors nowhere else.

use futures_util::future::try_join_all;
use spoolframe_catalog::{Catalog, CatalogError, Found, FoundGroup, SearchQuery};
use spoolframe_types::{
    Item, Library, LibraryId, LibraryKind, MediaId, MediaKind, Namespace, Person, PersonId,
    PersonRole, SearchKind,
};

use crate::wire::SectionKey;
use crate::{Call, PlexCatalog, convert, plex_key};

/// Where Plex keeps items of `kind`: the kind of section, the `type`
/// number its search takes, and the `type` its items carry. `None` for
/// people, who are not items.
fn plex_type(kind: SearchKind) -> Option<(LibraryKind, &'static str, &'static str)> {
    Some(match kind {
        SearchKind::Movies => (LibraryKind::Movies, "1", "movie"),
        SearchKind::Shows => (LibraryKind::Shows, "2", "show"),
        SearchKind::Episodes => (LibraryKind::Shows, "4", "episode"),
        SearchKind::Artists => (LibraryKind::Music, "8", "artist"),
        SearchKind::Albums => (LibraryKind::Music, "9", "album"),
        SearchKind::Tracks => (LibraryKind::Music, "10", "track"),
        SearchKind::People => return None,
    })
}

/// Plex's name for a role: the hub's `type`, and the filter a listing
/// takes.
fn role_name(role: PersonRole) -> &'static str {
    match role {
        PersonRole::Actor => "actor",
        PersonRole::Director => "director",
    }
}

/// A person's id: their role and tag id, which together name what their
/// items are listed by.
pub(crate) fn person_id(role: PersonRole, tag: u64) -> PersonId {
    PersonId::new(Namespace::Plex, format!("{}-{tag}", role_name(role))).expect("a key")
}

/// How many more people a search inside a library asks Plex's hubs for
/// than it shows, and the most it asks for.
const INSIDE_A_LIBRARY: u32 = 4;
const MAX_HUB: u32 = 100;

fn usize_of(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

impl PlexCatalog {
    /// Search the served sections, or only `query.library`, for each kind
    /// `query` asks for.
    pub async fn find(&self, query: &SearchQuery) -> Result<Found, CatalogError> {
        let sections = self.searched(query.library.as_ref()).await?;
        let groups = query
            .kinds
            .iter()
            .map(|&kind| self.find_kind(kind, &sections, query));
        Ok(Found {
            groups: try_join_all(groups).await?,
        })
    }

    /// The items a person found by [`Self::find`] is in, in the served
    /// sections, in Plex's order.
    pub async fn person_items(&self, id: &PersonId) -> Result<Vec<Item>, CatalogError> {
        if id.namespace() != Namespace::Plex {
            return Err(CatalogError::ForeignId(id.to_string()));
        }
        let (role, tag) = id
            .key()
            .split_once('-')
            .filter(|(role, tag)| {
                matches!(*role, "actor" | "director")
                    && !tag.is_empty()
                    && tag.bytes().all(|b| b.is_ascii_digit())
            })
            .ok_or_else(|| CatalogError::NotFound(id.to_string()))?;
        let mut call = Call::get("/library/all").about(id);
        call.query = vec![(role, tag)];
        let container = self.fetch(&call).await?;
        container
            .metadata
            .iter()
            .filter(|m| convert::is_modelled(&m.kind))
            .filter(|m| {
                self.serves(
                    m.library_section_id
                        .as_ref()
                        .or(container.library_section_id.as_ref()),
                )
            })
            .map(convert::item)
            .collect::<Result<_, _>>()
            .map_err(|e| self.converted(&call, e))
    }

    /// The served libraries a search looks in: all of them, or `library`.
    async fn searched(&self, library: Option<&LibraryId>) -> Result<Vec<Library>, CatalogError> {
        let Some(id) = library else {
            return self.libraries().await;
        };
        let key = plex_key(id.namespace(), id.key(), id)?;
        if !self.serves(Some(&SectionKey(key.to_owned()))) {
            return Err(CatalogError::NotServed(id.to_string()));
        }
        let library = self
            .libraries()
            .await?
            .into_iter()
            .find(|l| l.id == *id)
            .ok_or_else(|| CatalogError::NotFound(id.to_string()))?;
        Ok(vec![library])
    }

    /// The item of `kind` with Plex's guid (`plex://…`) among
    /// `external_ids`, in the served sections that hold that kind: what a
    /// playlist's entry finds its item by after a library is rebuilt
    /// (PX-40). Agents' ids (`imdb://…`) are not asked: Plex filters a
    /// listing by its own guid only. `None` when none has it.
    pub async fn find_guid(
        &self,
        kind: MediaKind,
        external_ids: &[String],
    ) -> Result<Option<MediaId>, CatalogError> {
        let Some(guid) = external_ids.iter().find(|id| id.starts_with("plex://")) else {
            return Ok(None);
        };
        let (holds, number) = match kind {
            MediaKind::Movie => (LibraryKind::Movies, "1"),
            MediaKind::Show => (LibraryKind::Shows, "2"),
            MediaKind::Season => (LibraryKind::Shows, "3"),
            MediaKind::Episode => (LibraryKind::Shows, "4"),
            MediaKind::Artist => (LibraryKind::Music, "8"),
            MediaKind::Album => (LibraryKind::Music, "9"),
            MediaKind::Track => (LibraryKind::Music, "10"),
        };
        for section in self.libraries().await?.iter().filter(|l| l.kind == holds) {
            let mut call = Call::get(format!("/library/sections/{}/all", section.id.key()))
                .about(format!("guid {guid}"));
            call.query = vec![("type", number), ("guid", guid)];
            let container = self.fetch(&call).await?;
            let found = container.metadata.iter().find(|m| {
                convert::media_kind(&m.kind) == Some(kind) && m.guid.as_deref() == Some(guid)
            });
            if let Some(m) = found {
                let item = convert::item(m).map_err(|e| self.converted(&call, e))?;
                return Ok(Some(item.id().clone()));
            }
        }
        Ok(None)
    }

    async fn find_kind(
        &self,
        kind: SearchKind,
        sections: &[Library],
        query: &SearchQuery,
    ) -> Result<FoundGroup, CatalogError> {
        let Some((holds, number, name)) = plex_type(kind) else {
            // Plex cannot narrow people to a section, but it says which
            // section it found each in: inside a library, those (P3-18).
            let inside = query.library.as_ref().and_then(|_| sections.first());
            return self.find_people(query, inside.map(|l| l.id.key())).await;
        };
        let asked = sections
            .iter()
            .filter(|l| l.kind == holds)
            .map(|l| self.search_section(l.id.key(), (kind, number, name), query));
        Ok(FoundGroup::merge(
            kind,
            try_join_all(asked).await?,
            query.limit,
        ))
    }

    /// One kind in one section, at most `query.limit` of it, and how many
    /// the section has.
    async fn search_section(
        &self,
        section: &str,
        (kind, number, name): (SearchKind, &str, &str),
        query: &SearchQuery,
    ) -> Result<FoundGroup, CatalogError> {
        let year = query.year.map(|y| y.to_string());
        let mut call = Call::get(format!("/library/sections/{section}/search"))
            .about(format!("search {:?}", query.text));
        call.query = vec![("type", number), ("query", &query.text)];
        if let Some(year) = &year {
            call.query.push(("year", year));
        }
        call.headers = vec![
            ("X-Plex-Container-Start", "0".to_owned()),
            ("X-Plex-Container-Size", query.limit.to_string()),
        ];
        let container = self.fetch(&call).await?;
        let returned = u32::try_from(container.metadata.len()).unwrap_or(u32::MAX);
        let mut items = container
            .metadata
            .iter()
            .filter(|m| {
                let asked = m.kind == name;
                if !asked {
                    tracing::debug!(rating_key = %m.rating_key, kind = %m.kind, asked = name, "skipping a search result of another kind");
                }
                asked
            })
            .map(convert::item)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| self.converted(&call, e))?;
        items.truncate(usize_of(query.limit));
        Ok(FoundGroup {
            kind,
            items,
            people: Vec::new(),
            total: container.total_size.unwrap_or(returned).max(returned),
        })
    }

    /// The actors and directors whose names match, each once: Plex lists
    /// a person once per section, with their count there. With `section`,
    /// only the ones it found in that section, with their count in it.
    async fn find_people(
        &self,
        query: &SearchQuery,
        section: Option<&str>,
    ) -> Result<FoundGroup, CatalogError> {
        if query.limit == 0 {
            // Counting people would cost a call for a number no scope has.
            return Ok(FoundGroup::empty(SearchKind::People));
        }
        // Plex caps each hub at `limit` over every section: inside one,
        // ask for more, so that people found elsewhere first leave some
        // for it. Still one call.
        let asked = match section {
            Some(_) => query.limit.saturating_mul(INSIDE_A_LIBRARY).min(MAX_HUB),
            None => query.limit,
        };
        let limit = asked.to_string();
        let mut call = Call::get("/hubs/search").about(format!("search {:?}", query.text));
        call.query = vec![("query", &query.text), ("limit", &limit)];
        let container = self.fetch(&call).await?;
        let mut people: Vec<Person> = Vec::new();
        for hub in &container.hubs {
            let role = match hub.kind.as_deref() {
                Some("actor") => PersonRole::Actor,
                Some("director") => PersonRole::Director,
                _ => continue,
            };
            for tag in &hub.tags {
                let found_in = tag.library_section_id.as_ref();
                if !self.serves(found_in) {
                    continue;
                }
                if section.is_some_and(|s| found_in.is_none_or(|f| f.0 != s)) {
                    continue;
                }
                let id = person_id(role, tag.id);
                match people.iter_mut().find(|p| p.id == id) {
                    Some(person) => {
                        person.count = match (person.count, tag.count) {
                            (Some(a), Some(b)) => Some(a.saturating_add(b)),
                            (a, b) => a.or(b),
                        };
                    }
                    None => people.push(Person {
                        id,
                        name: tag.tag.clone(),
                        role,
                        count: tag.count,
                    }),
                }
            }
        }
        let total = u32::try_from(people.len()).unwrap_or(u32::MAX);
        people.truncate(usize_of(query.limit));
        Ok(FoundGroup {
            kind: SearchKind::People,
            items: Vec::new(),
            people,
            total,
        })
    }
}
