//! The watch state of the account the PMS serves this catalog as (PX-04).
//!
//! Without a token, a LAN client of a PMS that allows it acts as the
//! server's owner; with one, as the token's account. Either way it is one
//! account. Plex only lists what that account has watched or started, so
//! reading it needs no scan of the whole library. Each user's own account
//! is a catalog with that account's token ([`PlexCatalog::with_token`],
//! P3-12).

use std::collections::HashSet;

use spoolframe_catalog::{Catalog, CatalogError};
use spoolframe_types::{LibraryKind, MediaId, MediaKind};

use crate::convert;
use crate::wire::Metadata;
use crate::{Call, PlexCatalog, plex_key};

/// How many items one listing call asks for.
const PAGE: u32 = 500;

/// What Plex's library plugin is called in its watch-state endpoints.
const IDENTIFIER: &str = "com.plexapp.plugins.library";

/// One movie or episode the account has watched or started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlexWatch {
    /// `plex:movie:…` or `plex:episode:…`.
    pub id: MediaId,
    /// An episode's show.
    pub show: Option<MediaId>,
    /// Watched to the end at least once.
    pub watched: bool,
    /// Where it stopped, when it is in progress.
    pub position_ms: Option<u64>,
    pub duration_ms: Option<u64>,
    /// When it was last viewed, Unix seconds.
    pub last_viewed_at: Option<i64>,
    /// Plex's guid and the agents' ids, as the catalog reports them.
    pub external_ids: Vec<String>,
    /// For people: `Banshee S3E8 All the Wisdom I Got Left`, `Mutiny`.
    pub title: String,
    /// The key of the library section it is in: what says which way it
    /// syncs (P3-12).
    pub section: String,
}

impl PlexCatalog {
    /// Every movie and episode the account has watched or has in
    /// progress, in the movie and show sections with these keys, or with
    /// `None` in every one, served or not: which ones sync is the caller's
    /// to say (P3-12).
    pub async fn watch_states(
        &self,
        sections: Option<&HashSet<String>>,
    ) -> Result<Vec<PlexWatch>, CatalogError> {
        let mut found: Vec<PlexWatch> = Vec::new();
        for library in self.unrestricted().libraries().await? {
            let (kind, plex_type) = match library.kind {
                LibraryKind::Movies => (MediaKind::Movie, "1"),
                LibraryKind::Shows => (MediaKind::Episode, "4"),
                _ => continue,
            };
            let section = plex_key(library.id.namespace(), library.id.key(), &library.id)?;
            if sections.is_some_and(|wanted| !wanted.contains(section)) {
                continue;
            }
            for filter in [("unwatched", "0"), ("inProgress", "1")] {
                for m in self.listing(section, plex_type, filter).await? {
                    let watch = self.watch(kind, section, &m)?;
                    // A rewatch in progress is in both lists.
                    match found.iter_mut().find(|w| w.id == watch.id) {
                        Some(seen) => {
                            seen.watched |= watch.watched;
                            seen.position_ms = seen.position_ms.or(watch.position_ms);
                        }
                        None => found.push(watch),
                    }
                }
            }
        }
        Ok(found)
    }

    /// One filtered listing of a section, page by page.
    async fn listing(
        &self,
        section: &str,
        plex_type: &str,
        filter: (&str, &str),
    ) -> Result<Vec<Metadata>, CatalogError> {
        self.listed(section, plex_type, &[filter]).await
    }

    /// A section's items of `plex_type` (`10`: tracks) that pass
    /// `filters`, page by page.
    pub(crate) async fn listed(
        &self,
        section: &str,
        plex_type: &str,
        filters: &[(&str, &str)],
    ) -> Result<Vec<Metadata>, CatalogError> {
        let mut all = Vec::new();
        loop {
            let mut call = Call::get(format!("/library/sections/{section}/all"));
            call.query = vec![("type", plex_type), ("includeGuids", "1")];
            call.query.extend_from_slice(filters);
            call.headers = vec![
                ("X-Plex-Container-Start", all.len().to_string()),
                ("X-Plex-Container-Size", PAGE.to_string()),
            ];
            let container = self.fetch(&call).await?;
            let got = container.metadata.len();
            all.extend(container.metadata);
            let total = container.total_size.map_or(usize::MAX, |t| t as usize);
            if got < PAGE as usize || all.len() >= total {
                return Ok(all);
            }
        }
    }

    fn watch(
        &self,
        kind: MediaKind,
        section: &str,
        m: &Metadata,
    ) -> Result<PlexWatch, CatalogError> {
        let id = |kind, key: &str| {
            convert::media_id(kind, key).map_err(|e| CatalogError::Backend {
                url: self.base.clone(),
                call: "list watch state".into(),
                item: Some(format!("ratingKey {key}")),
                message: e.message,
            })
        };
        Ok(PlexWatch {
            id: id(kind, &m.rating_key)?,
            show: match (kind, &m.grandparent_rating_key) {
                (MediaKind::Episode, Some(show)) => Some(id(MediaKind::Show, show)?),
                _ => None,
            },
            watched: m.view_count.unwrap_or(0) > 0,
            position_ms: m.view_offset.filter(|&o| o > 0),
            duration_ms: m.duration,
            last_viewed_at: m.last_viewed_at,
            external_ids: convert::external_ids(m),
            title: match (&m.grandparent_title, m.parent_index, m.index) {
                (Some(show), Some(season), Some(episode)) => {
                    format!("{show} S{season}E{episode} {}", m.title)
                }
                _ => m.title.clone(),
            },
            section: section.to_owned(),
        })
    }

    /// Mark `id` watched or unwatched for the account.
    pub async fn set_watched(&self, id: &MediaId, watched: bool) -> Result<(), CatalogError> {
        let key = plex_key(id.namespace(), id.key(), id)?;
        let path = if watched {
            "/:/scrobble"
        } else {
            "/:/unscrobble"
        };
        let mut call = Call::get(path).about(id);
        call.query = vec![("identifier", IDENTIFIER), ("key", key)];
        self.get_bytes(&call, "application/json", crate::MAX_JSON_BYTES)
            .await?;
        Ok(())
    }

    /// Set where the account stopped in `id`.
    pub async fn set_position(&self, id: &MediaId, position_ms: u64) -> Result<(), CatalogError> {
        let key = plex_key(id.namespace(), id.key(), id)?;
        let time = position_ms.to_string();
        let mut call = Call::get("/:/progress").about(id);
        call.query = vec![
            ("identifier", IDENTIFIER),
            ("key", key),
            ("time", &time),
            ("state", "stopped"),
        ];
        self.get_bytes(&call, "application/json", crate::MAX_JSON_BYTES)
            .await?;
        Ok(())
    }
}
