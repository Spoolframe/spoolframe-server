//! An admin's look at a Plex library (P3-16): its rows as Plex lists
//! them, each item's match and files, and its files checked with the path
//! mapping. Read-only: a match is fixed in Plex.
//!
//! Rows come a page at a time from Plex's own listings and P3-09's
//! section search, which page with an exact `totalSize`; only the rows on
//! the page have their files looked for. The quick filters Plex cannot
//! apply (unmatched, no artwork) read the whole listing of the kinds
//! asked, which for a library's top level is one request.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use spoolframe_catalog::{
    Browse, CatalogError, Inspect, Inspected, ItemWithChildren, LibraryCheck, Row, RowFilter,
    RowPage, Version, VersionPart, inspect,
};
use spoolframe_types::{Artwork, Item, LibraryId, MediaId, MediaKind, Namespace};

use crate::wire::{Metadata, SectionKey};
use crate::{Call, Fetched, PlexCatalog, convert, has_children, has_markers, plex_key};

/// The `type` number Plex lists items of `kind` by.
fn type_number(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Movie => "1",
        MediaKind::Show => "2",
        MediaKind::Season => "3",
        MediaKind::Episode => "4",
        MediaKind::Artist => "8",
        MediaKind::Album => "9",
        MediaKind::Track => "10",
    }
}

/// The artwork clients get for `item`; `None` for a kind that has none of
/// its own (a track shows its album's).
fn artwork_of(item: &Item) -> Option<&[Artwork]> {
    match item {
        Item::Movie(v) => Some(&v.artwork),
        Item::Show(v) => Some(&v.artwork),
        Item::Season(v) => Some(&v.artwork),
        Item::Episode(v) => Some(&v.artwork),
        Item::Artist(v) => Some(&v.artwork),
        Item::Album(v) => Some(&v.artwork),
        Item::Track(_) => None,
    }
}

/// A listing's items, and the library they are in, for path mappings
/// scoped to it.
struct Listed {
    metadata: Vec<Metadata>,
    library: Option<String>,
    total: u32,
}

impl PlexCatalog {
    /// One kind of item in section `key`: the page `page` of it, or all of
    /// it; matching `text` as P3-09's search does, when there is one.
    async fn rows_of_kind(
        &self,
        key: &str,
        kind: MediaKind,
        text: Option<&str>,
        page: Option<(u32, u32)>,
    ) -> Result<Listed, CatalogError> {
        let mut call = match text {
            Some(text) => {
                let mut call = Call::get(format!("/library/sections/{key}/search"))
                    .about(format!("search {text:?}"));
                call.query = vec![("type", type_number(kind)), ("query", text)];
                call
            }
            None => {
                let mut call = Call::get(format!("/library/sections/{key}/all"))
                    .about(format!("library section {key}"));
                call.query = vec![("type", type_number(kind))];
                call
            }
        };
        call.query.push(("includeGuids", "1"));
        if let Some((start, size)) = page {
            call.headers = vec![
                ("X-Plex-Container-Start", start.to_string()),
                ("X-Plex-Container-Size", size.to_string()),
            ];
        }
        let container = self.fetch(&call).await?;
        let returned = u32::try_from(container.metadata.len()).unwrap_or(u32::MAX);
        let name = convert::plex_name(kind);
        // A search can answer with other kinds; only the one asked is
        // listed.
        let metadata: Vec<Metadata> = container
            .metadata
            .into_iter()
            .filter(|m| m.kind == name)
            .collect();
        let total = match page {
            Some(_) => container.total_size.unwrap_or(returned),
            None => u32::try_from(metadata.len()).unwrap_or(u32::MAX),
        };
        Ok(Listed {
            metadata,
            library: container.library_section_title,
            total,
        })
    }

    /// `m` as a row, its files (a movie's, an episode's, a track's) looked
    /// for now.
    fn row(
        &self,
        call: &Call<'_>,
        m: &Metadata,
        library: Option<&str>,
    ) -> Result<Row, CatalogError> {
        let item = convert::item(m).map_err(|e| self.converted(call, e))?;
        let files = convert::media_files(m)
            .into_iter()
            .map(|file| self.paths.check(library, Path::new(file)))
            .collect();
        Ok(Row {
            item,
            guid: m.guid.clone(),
            agents: m.guids.iter().map(|g| g.id.clone()).collect(),
            added_at: m.added_at,
            updated_at: m.updated_at,
            children: m.child_count,
            leaves: m.leaf_count,
            files,
        })
    }

    fn rows(
        &self,
        call: &Call<'_>,
        metadata: &[Metadata],
        library: Option<&str>,
    ) -> Result<Vec<Row>, CatalogError> {
        metadata
            .iter()
            .map(|m| self.row(call, m, library))
            .collect()
    }

    /// Whether `m` passes `filter`.
    fn passes(&self, m: &Metadata, filter: RowFilter) -> bool {
        match filter {
            RowFilter::Unmatched => !inspect::is_matched(m.guid.as_deref()),
            RowFilter::NoArtwork => convert::item(m)
                .ok()
                .and_then(|item| artwork_of(&item).map(<[Artwork]>::is_empty))
                .unwrap_or(false),
        }
    }

    fn section(&self, library: &LibraryId) -> Result<String, CatalogError> {
        let key = plex_key(library.namespace(), library.key(), library)?;
        if !self.serves(Some(&SectionKey(key.to_owned()))) {
            return Err(CatalogError::NotServed(library.to_string()));
        }
        Ok(key.to_owned())
    }

    /// What [`spoolframe_catalog::Catalog::item`] gives clients for the
    /// item `fetched`, whose children Plex lists as `children`.
    pub(crate) fn with_children(
        &self,
        fetched: &Fetched,
        children_call: Option<&Call<'_>>,
        children: &[Metadata],
    ) -> Result<ItemWithChildren, CatalogError> {
        let children = match children_call {
            Some(call) => self.items_of(call, children)?,
            None => Vec::new(),
        };
        Ok(ItemWithChildren {
            item: fetched.item.clone(),
            children,
            external_ids: convert::external_ids(&fetched.metadata),
            markers: convert::markers(&fetched.metadata),
        })
    }

    /// The children of the item at `path` (`/library/metadata/{key}`), as
    /// Plex lists them; with their agents' ids when `guids`.
    pub(crate) async fn children_of(
        &self,
        id: &MediaId,
        path: &str,
        guids: bool,
    ) -> Result<(Call<'static>, Vec<Metadata>, Option<String>), CatalogError> {
        let mut call = Call::get(format!("{path}/children")).about(id);
        if guids {
            call.query = vec![("includeGuids", "1")];
        }
        // Only `Metadata` entries are children; a show's "All episodes"
        // shortcut comes as a `Directory` and is not one.
        let container = self.fetch(&call).await?;
        Ok((call, container.metadata, container.library_section_title))
    }
}

/// The ids and titles of `m`'s show and season, or artist and album,
/// outermost first, as `item` names them.
fn ancestors(item: &Item, m: &Metadata) -> Vec<(MediaId, String)> {
    let named = |title: &Option<String>, fallback: String| title.clone().unwrap_or(fallback);
    match item {
        Item::Season(s) => vec![(
            s.show_id.clone(),
            named(&m.parent_title, s.show_id.to_string()),
        )],
        Item::Episode(e) => vec![
            (
                e.show_id.clone(),
                named(&m.grandparent_title, e.show_id.to_string()),
            ),
            (
                e.season_id.clone(),
                named(&m.parent_title, format!("Season {}", e.season)),
            ),
        ],
        Item::Album(a) => vec![(
            a.artist_id.clone(),
            named(&m.parent_title, a.artist_id.to_string()),
        )],
        Item::Track(t) => vec![
            (
                t.artist_id.clone(),
                named(&m.grandparent_title, t.artist_id.to_string()),
            ),
            (
                t.album_id.clone(),
                named(&m.parent_title, t.album_id.to_string()),
            ),
        ],
        _ => Vec::new(),
    }
}

/// The page of `total` rows split over listings: how many to skip of
/// each and how many to take, given each listing's size.
fn take_from(skip: &mut u32, want: &mut u32, size: u32, got: u32) {
    *skip = skip.saturating_sub(size);
    *want = want.saturating_sub(got);
}

#[async_trait]
impl Inspect for PlexCatalog {
    async fn browse(&self, library: &LibraryId, query: &Browse) -> Result<RowPage, CatalogError> {
        let key = self.section(library)?;
        let text = query
            .text
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty());
        let call = Call::get(format!("/library/sections/{key}")).about(library);
        if let Some(filter) = query.filter {
            // Plex cannot filter by these: every item of the kinds, then
            // the page of the ones that pass.
            let mut passing = Vec::new();
            let mut library_title = None;
            for &kind in &query.kinds {
                let listed = self.rows_of_kind(&key, kind, text, None).await?;
                library_title = library_title.or(listed.library);
                passing.extend(
                    listed
                        .metadata
                        .into_iter()
                        .filter(|m| self.passes(m, filter)),
                );
            }
            let total = u32::try_from(passing.len()).unwrap_or(u32::MAX);
            let start = usize::try_from(query.page.offset).unwrap_or(usize::MAX);
            let size = usize::try_from(query.page.limit).unwrap_or(usize::MAX);
            let page: Vec<Metadata> = passing.into_iter().skip(start).take(size).collect();
            let rows = self.rows(&call, &page, library_title.as_deref())?;
            return Ok(RowPage { rows, total });
        }
        // Each kind in turn, as if one listing: the page may start in one
        // and end in the next.
        let mut skip = query.page.offset;
        let mut want = query.page.limit;
        let mut total = 0u32;
        let mut rows = Vec::new();
        for &kind in &query.kinds {
            let listed = self
                .rows_of_kind(&key, kind, text, Some((skip, want)))
                .await?;
            let got = u32::try_from(listed.metadata.len()).unwrap_or(u32::MAX);
            total = total.saturating_add(listed.total);
            rows.extend(self.rows(&call, &listed.metadata, listed.library.as_deref())?);
            take_from(&mut skip, &mut want, listed.total, got);
        }
        Ok(RowPage { rows, total })
    }

    async fn count(&self, library: &LibraryId, kind: MediaKind) -> Result<u32, CatalogError> {
        let key = self.section(library)?;
        Ok(self
            .rows_of_kind(&key, kind, None, Some((0, 0)))
            .await?
            .total)
    }

    async fn child_rows(&self, id: &MediaId) -> Result<Vec<Row>, CatalogError> {
        if !has_children(id.kind()) {
            return Ok(Vec::new());
        }
        // Checked to be the item it says, and served.
        let fetched = self.metadata(id, false).await?;
        let (call, children, library) = self.children_of(id, &fetched.call.path, true).await?;
        self.rows(
            &call,
            &children,
            library.as_deref().or(fetched.library.as_deref()),
        )
    }

    async fn inspect(&self, id: &MediaId) -> Result<Inspected, CatalogError> {
        let fetched = self.metadata(id, has_markers(id.kind())).await?;
        let (children_call, children) = if has_children(id.kind()) {
            let (call, children, _) = self.children_of(id, &fetched.call.path, true).await?;
            (Some(call), children)
        } else {
            (None, Vec::new())
        };
        let found = self.with_children(&fetched, children_call.as_ref(), &children)?;
        let library = fetched.library.as_deref();
        let child_rows = match &children_call {
            Some(call) => self.rows(call, &children, library)?,
            None => Vec::new(),
        };
        let m = &fetched.metadata;
        let versions = m
            .media
            .iter()
            .map(|media| Version {
                id: media.id,
                container: media.container.clone(),
                video_codec: media.video_codec.clone(),
                video_resolution: media.video_resolution.clone(),
                video_profile: media.video_profile.clone(),
                audio_codec: media.audio_codec.clone(),
                audio_channels: media.audio_channels,
                bitrate: media.bitrate,
                duration_ms: media.duration,
                parts: media
                    .parts
                    .iter()
                    .filter_map(|part| {
                        let file = part.file.as_deref()?;
                        Some(VersionPart {
                            id: part.id,
                            size: part.size,
                            duration_ms: part.duration,
                            file: self.paths.check(library, Path::new(file)),
                        })
                    })
                    .collect(),
            })
            .collect();
        let section = fetched
            .section
            .as_deref()
            .and_then(|s| LibraryId::new(Namespace::Plex, s).ok());
        Ok(Inspected {
            ancestors: ancestors(&fetched.item, m),
            library: section,
            guid: m.guid.clone(),
            added_at: m.added_at,
            updated_at: m.updated_at,
            folders: m.locations.iter().map(|l| PathBuf::from(&l.path)).collect(),
            versions,
            children: child_rows,
            found,
        })
    }

    async fn check_library(&self, library: &LibraryId) -> Result<LibraryCheck, CatalogError> {
        PlexCatalog::check_library(self, library).await
    }

    async fn backend_link(&self, id: &MediaId) -> Result<Option<String>, CatalogError> {
        let key = plex_key(id.namespace(), id.key(), id)?;
        Ok(self
            .machine_id()
            .await?
            .map(|machine| plex_web_link(crate::account::PLEX_APP, &machine, key)))
    }
}

/// The Plex web app's page for the item with `key` on the server
/// `machine`: what Plex's own "share" links name.
pub fn plex_web_link(app: &str, machine: &str, key: &str) -> String {
    format!(
        "{}/desktop/#!/server/{machine}/details?key=%2Flibrary%2Fmetadata%2F{key}",
        app.trim_end_matches('/')
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_spans_listings_in_turn() {
        // 30 shows then 100 episodes; rows 25..35 are 5 shows and 5
        // episodes.
        let (mut skip, mut want) = (25, 10);
        take_from(&mut skip, &mut want, 30, 5);
        assert_eq!((skip, want), (0, 5));
        // Past the first listing entirely.
        let (mut skip, mut want) = (40, 10);
        take_from(&mut skip, &mut want, 30, 0);
        assert_eq!((skip, want), (10, 10));
    }

    #[test]
    fn the_web_app_link_names_the_server_and_the_item() {
        assert_eq!(
            plex_web_link("https://app.plex.tv/", "94ad03ca", "208618"),
            "https://app.plex.tv/desktop/#!/server/94ad03ca/details?key=%2Flibrary%2Fmetadata%2F208618"
        );
    }
}
