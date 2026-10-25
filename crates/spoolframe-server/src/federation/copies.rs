//! A title's copies across servers, as a client that names the
//! experimental `federation` feature is shown them (P8-05): a search's hit
//! and an item, each with where it is from, the title's other copies in
//! Play from order, and the copy that plays.
//!
//! A title's copies are matched by their external ids ([`matched`]): any
//! id they share, an episode's by its show's with its season and number.
//! The user's Play from choice is kept by the title's kind and the same
//! ids (`federation_play_from`), so it finds every copy of the title.

use std::collections::HashMap;

use async_trait::async_trait;
use spoolframe_catalog::{Catalog, ItemWithChildren};
use spoolframe_protocol::{Copies, SearchHit, SearchResults, TitleCopy};
use spoolframe_types::{Item, LibraryServer, MediaId};

use super::{Federation, items};
use crate::principal::Principal;
use crate::search::{Preferred, Searched, matched, same_year, year_of};
use crate::store::StoreError;
use crate::store::play_from::{Chosen, chosen};
use crate::watch::{Watch, WatchError};

/// Why a Play from choice was not kept.
#[derive(Debug, thiserror::Error)]
pub enum PlayFromError {
    /// The title has no id another server's copy could share.
    #[error("{0} has no external ids: it has one copy")]
    NoTitle(MediaId),
    /// No live pair has that server id.
    #[error("no paired server {0:?}")]
    NoSuchServer(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// One user's view of the titles' copies: the live pairs as they stand
/// now, and who is asking.
pub struct Viewer<'a> {
    federation: &'a Federation,
    principal: Principal,
    servers: HashMap<String, LibraryServer>,
    /// The user's Play from choices, read once for a search or an item.
    choices: Vec<Chosen>,
}

impl Federation {
    /// What `principal` is shown of the titles' copies.
    pub async fn viewer(&self, principal: Principal) -> Result<Viewer<'_>, StoreError> {
        Ok(Viewer {
            federation: self,
            principal,
            servers: self.servers().await?,
            choices: self
                .store()
                .play_from_choices(&principal.to_string())
                .await?,
        })
    }

    /// `principal` plays the title `title` (a copy's title key) from paired
    /// server `from`, or from this server with `None`.
    pub async fn set_play_from(
        &self,
        principal: Principal,
        id: &MediaId,
        title: &[String],
        from: Option<&str>,
    ) -> Result<(), PlayFromError> {
        let ids = matched(title);
        if ids.is_empty() {
            return Err(PlayFromError::NoTitle(id.clone()));
        }
        if let Some(server) = from
            && self.live_peer_by_server_id(server).await?.is_none()
        {
            return Err(PlayFromError::NoSuchServer(server.to_owned()));
        }
        self.store()
            .set_play_from(&principal.to_string(), id.kind(), &ids, from)
            .await?;
        Ok(())
    }
}

impl Viewer<'_> {
    /// The paired server `id` is a copy on, as a client is shown it;
    /// `None` for this server's own. One whose pair went meanwhile is
    /// shown by its id, offline.
    fn server(&self, id: &MediaId) -> Option<LibraryServer> {
        let origin = id.origin()?;
        Some(
            self.servers
                .get(origin)
                .cloned()
                .unwrap_or_else(|| LibraryServer {
                    server_id: origin.to_owned(),
                    name: origin.to_owned(),
                    online: false,
                }),
        )
    }

    /// `id` with where it is from, `others` (its title's other copies) in
    /// Play from order, and the copy that plays: on the server `choices`
    /// the user chose for `title` while it has one, else `id`.
    fn copies(&self, id: &MediaId, others: &[MediaId], title: &[String]) -> Copies {
        let mut also_on: Vec<TitleCopy> = others
            .iter()
            .map(|other| TitleCopy {
                id: other.clone(),
                server: self.server(other),
            })
            .collect();
        // This server's first, then those online by name, then the rest.
        fn key(c: &TitleCopy) -> Option<(bool, &str, &str)> {
            c.server
                .as_ref()
                .map(|s| (!s.online, s.name.as_str(), s.server_id.as_str()))
        }
        also_on.sort_by(|a, b| key(a).cmp(&key(b)));
        let play_from = chosen(&self.choices, id.kind(), title)
            .and_then(|choice| {
                std::iter::once(id)
                    .chain(others)
                    .find(|copy| copy.origin() == choice.server.as_deref())
            })
            .unwrap_or(id)
            .clone();
        Copies {
            origin: self.server(id),
            also_on,
            play_from,
        }
    }

    /// `searched`'s results as hits: each item with where it is from, the
    /// title's other copies and the copy that plays.
    pub fn hits(&self, searched: Searched) -> SearchResults<SearchHit> {
        let Searched {
            results,
            copies,
            titles,
        } = searched;
        let title = |id: &MediaId| titles.get(id).map(|t| matched(t)).unwrap_or_default();
        results.map(|item| {
            let others = copies.get(item.id()).map_or(&[][..], Vec::as_slice);
            let copies = self.copies(item.id(), others, &title(item.id()));
            SearchHit::new(item, copies)
        })
    }

    /// Each of `searched`'s titles found on several servers with its
    /// newest progress, whichever copy played (P8-03), in two reads.
    pub async fn fill_titles(
        &self,
        watch: &Watch,
        searched: &mut Searched,
    ) -> Result<(), WatchError> {
        let Searched {
            results,
            copies,
            titles,
        } = searched;
        let mut items: Vec<(&mut Item, &[String])> = crate::search::items_mut(results)
            .filter(|item| copies.contains_key(item.id()))
            .map(|item| {
                let title = titles.get(item.id()).map_or(&[][..], Vec::as_slice);
                (item, title)
            })
            .collect();
        watch.fill_titles(self.principal, &mut items).await
    }

    /// `found`'s copies: the copies of its title (`title`, its title key)
    /// here and on the paired servers, one per server and none on its
    /// own. This server's is looked up by its external ids; one the
    /// catalog cannot say now is left out.
    pub async fn item_copies(
        &self,
        catalog: &dyn Catalog,
        found: &ItemWithChildren,
        title: &[String],
    ) -> Result<Copies, StoreError> {
        let id = found.item.id();
        let kind = id.kind();
        let year = year_of(&found.item);
        let ids = matched(title);
        let mut candidates = Vec::new();
        if id.origin().is_some() && !ids.is_empty() {
            match catalog.find_external(kind, &ids).await {
                // Of the same year, as a search merges them: the paired
                // server's ids are its word only.
                Ok(Some(own)) => match catalog.item(&own).await {
                    Ok(here) if same_year(year_of(&here.item), year) => candidates.push(own),
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(%id, error = %e, "this server's copy of a title is not known now")
                    }
                },
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(%id, error = %e, "this server's copy of a title is not known now")
                }
            }
        }
        let store = self.federation.store();
        let mut cached = store.cached_copies(kind.as_str(), &ids, year).await?;
        if let Item::Episode(episode) = &found.item {
            let numbered = format!("#s{}e{}", episode.season, episode.episode);
            let shows: Vec<String> = ids
                .iter()
                .filter_map(|id| id.strip_suffix(&numbered))
                .map(str::to_owned)
                .collect();
            cached.extend(
                store
                    .cached_episode_copies(&shows, episode.season, episode.episode)
                    .await?,
            );
        }
        candidates.extend(
            cached
                .iter()
                .filter_map(|(origin, item)| items::media_id(origin, kind, item)),
        );
        let mut others: Vec<MediaId> = Vec::new();
        for copy in candidates {
            let server = copy.origin();
            if server != id.origin() && others.iter().all(|o| o.origin() != server) {
                others.push(copy);
            }
        }
        Ok(self.copies(id, &others, &ids))
    }
}

#[async_trait]
impl Preferred for Viewer<'_> {
    /// The copy on the server the user chose for the title, else the one
    /// the user's newest watch state is of: the server last played from.
    async fn preferred(&self, title: &[String], copies: &[MediaId]) -> Option<MediaId> {
        let kind = copies.first()?.kind();
        if let Some(choice) = chosen(&self.choices, kind, title)
            && let Some(copy) = copies
                .iter()
                .find(|c| c.origin() == choice.server.as_deref())
        {
            return Some(copy.clone());
        }
        let keys: Vec<String> = copies.iter().map(MediaId::to_string).collect();
        match self
            .federation
            .store()
            .watch_states(self.principal, &keys)
            .await
        {
            Ok(states) => states
                .values()
                .max_by_key(|row| row.updated_at)
                .and_then(|row| row.media_id.parse().ok()),
            Err(e) => {
                tracing::warn!(error = %e, "search: where a title was last played is not known now");
                None
            }
        }
    }

    fn online(&self, id: &MediaId) -> bool {
        id.origin()
            .is_none_or(|origin| self.federation.links.online(origin))
    }
}
