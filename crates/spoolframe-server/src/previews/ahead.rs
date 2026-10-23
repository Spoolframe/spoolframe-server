//! Previews made ahead of any play (`previews.when = "ahead"`): what is
//! being watched first, then what is newest, then everything else, while
//! the cache has room ([`super::cache::Cache::has_room`]).
//!
//! There is no scan of the files: the catalog is asked. The newest items
//! and those in progress are looked at every [`NEWEST_EVERY`], so a film
//! added to Plex has its previews soon after; every library is walked at
//! start, when `ahead` is turned on, and every [`ALL_EVERY`].

use std::collections::HashSet;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use spoolframe_catalog::{Browse, Catalog, CatalogError, Inspect, Page};
use spoolframe_types::{Item, LibraryId, LibraryKind, MediaId, MediaKind};

use super::{Previews, When};
use crate::store::Store;

/// How often the newest and in-progress items are queued.
pub const NEWEST_EVERY: Duration = Duration::from_secs(10 * 60);
/// How often every library is walked.
pub const ALL_EVERY: Duration = Duration::from_secs(6 * 3600);
/// How often the sweep looks at whether one is due.
const TICK: Duration = Duration::from_secs(30);
/// How many newest and in-progress items are queued first.
const FIRST: u32 = 100;
/// Items asked for per page of a library.
const PAGE: u32 = 200;

/// A catalog that lists a library's episodes in pages, as one listing,
/// without asking for each show and each season: a TV library of 300
/// shows is a few pages, not some 2000 item fetches (review 2026-10-11
/// M12).
#[async_trait]
pub trait LibraryEpisodes: Send + Sync {
    /// One page of `library`'s episodes: their ids, and how many it has.
    async fn episodes(
        &self,
        library: &LibraryId,
        page: Page,
    ) -> Result<(Vec<MediaId>, u32), CatalogError>;
}

#[async_trait]
impl LibraryEpisodes for crate::catalog::LiveCatalog {
    async fn episodes(
        &self,
        library: &LibraryId,
        page: Page,
    ) -> Result<(Vec<MediaId>, u32), CatalogError> {
        let Some(composite) = self.composite() else {
            return Err(CatalogError::NotSetUp);
        };
        let rows = composite
            .browse(
                library,
                &Browse {
                    kinds: vec![MediaKind::Episode],
                    text: None,
                    filter: None,
                    page,
                },
            )
            .await?;
        let ids = rows
            .rows
            .into_iter()
            .filter(|r| matches!(r.item, Item::Episode(_)))
            .map(|r| r.item.id().clone())
            .collect();
        Ok((ids, rows.total))
    }
}

/// The items to make previews for, in order, each once: `in_progress`,
/// then the catalog's newest, then (with `everything`) every movie and
/// episode of every library. A TV library's episodes come from
/// `episodes` in pages where there is one, else show by show.
pub async fn candidates(
    catalog: &dyn Catalog,
    episodes: Option<&dyn LibraryEpisodes>,
    in_progress: Vec<MediaId>,
    everything: bool,
) -> Vec<MediaId> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut add = |id: MediaId| {
        if seen.insert(id.clone()) {
            out.push(id);
        }
    };
    for id in in_progress {
        add(id);
    }
    match catalog.recently_added(FIRST).await {
        Ok(items) => items.into_iter().for_each(|i| add(i.id().clone())),
        Err(e) => tracing::debug!(error = %e, "no newest items for previews"),
    }
    if !everything {
        return out;
    }
    let libraries = match catalog.libraries().await {
        Ok(libraries) => libraries,
        Err(e) => {
            tracing::debug!(error = %e, "no libraries for previews");
            return out;
        }
    };
    for library in libraries {
        if let (Some(episodes), LibraryKind::Shows) = (episodes, library.kind) {
            let mut offset = 0;
            loop {
                let page = Page {
                    offset,
                    limit: PAGE,
                };
                match episodes.episodes(&library.id, page).await {
                    Ok((ids, total)) => {
                        let got = ids.len() as u32;
                        ids.into_iter().for_each(&mut add);
                        offset += got;
                        if got == 0 || offset >= total {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::debug!(library = %library.id, error = %e, "a library's episodes are not listed for previews");
                        break;
                    }
                }
            }
            continue;
        }
        let mut offset = 0;
        loop {
            let page = match catalog
                .items(
                    &library.id,
                    Page {
                        offset,
                        limit: PAGE,
                    },
                )
                .await
            {
                Ok(page) => page,
                Err(e) => {
                    tracing::debug!(library = %library.id, error = %e, "a library is not walked for previews");
                    break;
                }
            };
            let got = page.items.len() as u32;
            for item in page.items {
                match item {
                    Item::Movie(_) | Item::Episode(_) => add(item.id().clone()),
                    Item::Show(show) => {
                        for episode in show_episodes(catalog, &show.id).await {
                            add(episode);
                        }
                    }
                    // Music has no pictures to preview.
                    _ => {}
                }
            }
            offset += got;
            if got == 0 || offset >= page.total {
                break;
            }
        }
    }
    out
}

/// A show's episodes, season by season.
async fn show_episodes(catalog: &dyn Catalog, show: &MediaId) -> Vec<MediaId> {
    let Ok(show) = catalog.item(show).await else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for season in show.children {
        if let Ok(season) = catalog.item(season.id()).await {
            out.extend(
                season
                    .children
                    .into_iter()
                    .filter(|e| matches!(e, Item::Episode(_)))
                    .map(|e| e.id().clone()),
            );
        }
    }
    out
}

/// Queue previews ahead, as the sweeps fall due, for as long as
/// `previews` exist and are not stopped.
pub fn spawn(
    previews: &Arc<Previews>,
    catalog: Arc<dyn Catalog>,
    episodes: Option<Arc<dyn LibraryEpisodes>>,
    store: Store,
) {
    let previews = Arc::downgrade(previews);
    tokio::spawn(async move { sweep_loop(previews, catalog, episodes, store).await });
}

async fn sweep_loop(
    previews: Weak<Previews>,
    catalog: Arc<dyn Catalog>,
    episodes: Option<Arc<dyn LibraryEpisodes>>,
    store: Store,
) {
    let mut last_all: Option<Instant> = None;
    let mut last_newest: Option<Instant> = None;
    let mut was_ahead = false;
    loop {
        let Some(now) = previews.upgrade() else {
            return;
        };
        if now.stopped() {
            return;
        }
        let ahead = now.settings().when == When::Ahead;
        // Turned on: everything, at once.
        if ahead && !was_ahead {
            last_all = None;
        }
        was_ahead = ahead;
        let everything = last_all.is_none_or(|t| t.elapsed() >= ALL_EVERY);
        let newest = last_newest.is_none_or(|t| t.elapsed() >= NEWEST_EVERY);
        // Nothing made ahead fits a full cache: the walk of every item
        // would only ask Plex where each file is to make none.
        let everything = everything && now.cache.has_room();
        if ahead && (everything || newest) {
            let in_progress = match store.in_progress_anyone(FIRST).await {
                Ok(ids) => ids.iter().filter_map(|id| id.parse().ok()).collect(),
                Err(e) => {
                    tracing::warn!(error = %e, "cannot read what is in progress");
                    Vec::new()
                }
            };
            let ids = candidates(
                catalog.as_ref(),
                episodes.as_deref(),
                in_progress,
                everything,
            )
            .await;
            tracing::debug!(items = ids.len(), everything, "previews queued ahead");
            now.ahead(ids);
            last_newest = Some(Instant::now());
            if everything {
                last_all = Some(Instant::now());
            }
        }
        drop(now);
        tokio::time::sleep(TICK).await;
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use async_trait::async_trait;
    use spoolframe_catalog::{CatalogError, Found, Image, ItemPage, ItemWithChildren, SearchQuery};
    use spoolframe_types::{
        Artist, ArtworkId, Episode, Library, LibraryId, LibraryKind, MediaKind, Movie, Namespace,
        PersonId, PlaybackState, Season, Show,
    };

    use super::*;

    fn id(kind: MediaKind, key: &str) -> MediaId {
        MediaId::new(Namespace::Plex, kind, key).unwrap()
    }

    fn movie(key: &str) -> Item {
        Item::Movie(Movie {
            id: id(MediaKind::Movie, key),
            title: key.into(),
            year: None,
            release_date: None,
            summary: None,
            content_rating: None,
            duration: None,
            cast: Vec::new(),
            artwork: Vec::new(),
            playback: PlaybackState::default(),
        })
    }

    fn episode(key: &str) -> Item {
        Item::Episode(Episode {
            id: id(MediaKind::Episode, key),
            show_id: id(MediaKind::Show, "s"),
            show_title: None,
            season_id: id(MediaKind::Season, "s1"),
            season: 1,
            episode: 1,
            title: key.into(),
            summary: None,
            content_rating: None,
            year: None,
            air_date: None,
            duration: None,
            cast: Vec::new(),
            artwork: Vec::new(),
            playback: PlaybackState::default(),
        })
    }

    fn library(key: &str, kind: LibraryKind) -> Library {
        Library {
            id: LibraryId::new(Namespace::Plex, key).unwrap(),
            title: key.into(),
            kind,
            server: None,
        }
    }

    /// Films: 450 movies, m0 … m449. Shows: one show of two seasons,
    /// e1 e2 and e3. Music: one artist. Newest: m7 and e2.
    struct Libraries;

    #[async_trait]
    impl Catalog for Libraries {
        async fn libraries(&self) -> Result<Vec<Library>, CatalogError> {
            Ok(vec![
                library("films", LibraryKind::Movies),
                library("shows", LibraryKind::Shows),
                library("music", LibraryKind::Music),
            ])
        }
        async fn items(&self, library: &LibraryId, page: Page) -> Result<ItemPage, CatalogError> {
            let all: Vec<Item> = match library.key() {
                "films" => (0..450).map(|n| movie(&format!("m{n}"))).collect(),
                "shows" => vec![Item::Show(Show {
                    id: id(MediaKind::Show, "s"),
                    title: "s".into(),
                    year: None,
                    summary: None,
                    content_rating: None,
                    season_count: None,
                    genres: Vec::new(),
                    last_aired: None,
                    cast: Vec::new(),
                    artwork: Vec::new(),
                })],
                _ => vec![Item::Artist(Artist {
                    id: id(MediaKind::Artist, "a"),
                    name: "a".into(),
                    summary: None,
                    artwork: Vec::new(),
                })],
            };
            let total = all.len() as u32;
            Ok(ItemPage {
                items: all
                    .into_iter()
                    .skip(page.offset as usize)
                    .take(page.limit as usize)
                    .collect(),
                total,
            })
        }
        async fn item(&self, of: &MediaId) -> Result<ItemWithChildren, CatalogError> {
            let season = |key: &str| {
                Item::Season(Season {
                    id: id(MediaKind::Season, key),
                    show_id: id(MediaKind::Show, "s"),
                    show_title: None,
                    season: 1,
                    title: key.into(),
                    artwork: Vec::new(),
                })
            };
            let children = match of.key() {
                "s" => vec![season("s1"), season("s2")],
                "s1" => vec![episode("e1"), episode("e2")],
                "s2" => vec![episode("e3")],
                _ => Vec::new(),
            };
            Ok(ItemWithChildren {
                item: movie("x"),
                children,
                external_ids: Vec::new(),
                markers: Vec::new(),
            })
        }
        async fn external_ids(&self, _: &MediaId) -> Result<Vec<String>, CatalogError> {
            unreachable!()
        }
        async fn search(&self, _: &SearchQuery) -> Result<Found, CatalogError> {
            unreachable!()
        }
        async fn person_items(&self, _: &PersonId) -> Result<Vec<Item>, CatalogError> {
            unreachable!()
        }
        async fn media_path(&self, _: &MediaId) -> Result<PathBuf, CatalogError> {
            unreachable!()
        }
        async fn recently_added(&self, _: u32) -> Result<Vec<Item>, CatalogError> {
            Ok(vec![movie("m7"), episode("e2")])
        }
        async fn authorize_artwork(
            &self,
            _id: &spoolframe_types::ArtworkId,
        ) -> Result<(), CatalogError> {
            Ok(())
        }

        async fn artwork(&self, _: &ArtworkId) -> Result<Image, CatalogError> {
            unreachable!()
        }
    }

    fn keys(ids: &[MediaId]) -> Vec<&str> {
        ids.iter().map(MediaId::key).collect()
    }

    #[tokio::test]
    async fn in_progress_first_then_newest_then_every_film_and_episode_once() {
        let in_progress = vec![id(MediaKind::Episode, "e3")];
        let all = candidates(&Libraries, None, in_progress.clone(), true).await;
        let keys = keys(&all);
        assert_eq!(keys[..3], ["e3", "m7", "e2"]);
        // Every page of films, m7 not again.
        assert_eq!(keys[3..6], ["m0", "m1", "m2"]);
        assert!(keys.contains(&"m449"));
        assert_eq!(keys.iter().filter(|k| **k == "m7").count(), 1);
        // The show's episodes, season by season; nothing of the music.
        assert_eq!(keys[keys.len() - 1], "e1");
        assert_eq!(keys.len(), 3 + 449 + 1);
        assert!(!keys.contains(&"a"));

        let first = candidates(&Libraries, None, in_progress, false).await;
        assert_eq!(super::tests::keys(&first), ["e3", "m7", "e2"]);
    }

    /// A TV library's episodes listed in pages, as one listing: no show
    /// or season is fetched (review 2026-10-11 M12).
    #[tokio::test]
    async fn a_tv_library_s_episodes_are_listed_not_walked_show_by_show() {
        struct Listed(std::sync::atomic::AtomicUsize);
        #[async_trait]
        impl LibraryEpisodes for Listed {
            async fn episodes(
                &self,
                library: &LibraryId,
                page: Page,
            ) -> Result<(Vec<MediaId>, u32), CatalogError> {
                assert_eq!(library.key(), "shows");
                self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let all: Vec<MediaId> = (0..250)
                    .map(|n| id(MediaKind::Episode, &format!("x{n}")))
                    .collect();
                let page: Vec<MediaId> = all
                    .into_iter()
                    .skip(page.offset as usize)
                    .take(page.limit as usize)
                    .collect();
                Ok((page, 250))
            }
        }
        /// Libraries, whose shows must not be fetched one by one.
        struct NoWalk;
        #[async_trait]
        impl Catalog for NoWalk {
            async fn libraries(&self) -> Result<Vec<Library>, CatalogError> {
                Libraries.libraries().await
            }
            async fn items(&self, l: &LibraryId, p: Page) -> Result<ItemPage, CatalogError> {
                Libraries.items(l, p).await
            }
            async fn item(&self, of: &MediaId) -> Result<ItemWithChildren, CatalogError> {
                panic!("{of} fetched: a show walked")
            }
            async fn external_ids(&self, _: &MediaId) -> Result<Vec<String>, CatalogError> {
                unreachable!()
            }
            async fn search(&self, _: &SearchQuery) -> Result<Found, CatalogError> {
                unreachable!()
            }
            async fn person_items(&self, _: &PersonId) -> Result<Vec<Item>, CatalogError> {
                unreachable!()
            }
            async fn media_path(&self, _: &MediaId) -> Result<PathBuf, CatalogError> {
                unreachable!()
            }
            async fn recently_added(&self, n: u32) -> Result<Vec<Item>, CatalogError> {
                Libraries.recently_added(n).await
            }
            async fn authorize_artwork(&self, _: &ArtworkId) -> Result<(), CatalogError> {
                Ok(())
            }
            async fn artwork(&self, _: &ArtworkId) -> Result<Image, CatalogError> {
                unreachable!()
            }
        }
        let listed = Listed(Default::default());
        let all = candidates(&NoWalk, Some(&listed), Vec::new(), true).await;
        let keys = keys(&all);
        assert!(keys.contains(&"x0") && keys.contains(&"x249"));
        assert!(keys.contains(&"m449"));
        assert_eq!(
            listed.0.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "two pages"
        );
    }
}
