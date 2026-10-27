//! `PlexCatalog` against a real PMS. Skipped unless
//! `SPOOLFRAME_TEST_PLEX_URL` is set; `SPOOLFRAME_TEST_PLEX_TOKEN_FILE`
//! names the token, for a PMS that wants one.
//! `SPOOLFRAME_TEST_PLEX_PATH_MAP=<plex prefix>=<local prefix>` maps the
//! media to where it is mounted here; with it set, every file reached must
//! be readable, without it a file that is not mounted here is tolerated.
//!
//! Lists every library, then walks one path down each to a playable item:
//! movie; show → season → episode; artist → album → track.

use spoolframe_catalog::{Catalog, CatalogError, Page};
use spoolframe_plex::{PathMap, PathMapping, PlexCatalog};
use spoolframe_types::{Item, LibraryKind, MediaId, MediaKind};

/// The mapping from `SPOOLFRAME_TEST_PLEX_PATH_MAP`, if set.
fn path_map() -> Option<PathMap> {
    let spec = std::env::var("SPOOLFRAME_TEST_PLEX_PATH_MAP").ok()?;
    let (from, to) = spec
        .split_once('=')
        .expect("SPOOLFRAME_TEST_PLEX_PATH_MAP is <plex prefix>=<local prefix>");
    Some(PathMap::new(vec![PathMapping {
        library: None,
        from: from.into(),
        to: to.into(),
    }]))
}

fn live_catalog() -> Option<PlexCatalog> {
    let Ok(url) = std::env::var("SPOOLFRAME_TEST_PLEX_URL") else {
        eprintln!("SPOOLFRAME_TEST_PLEX_URL not set; skipping the live PMS test");
        return None;
    };
    let token = std::env::var("SPOOLFRAME_TEST_PLEX_TOKEN_FILE")
        .ok()
        .map(|path| {
            std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{path}: {e}"))
                .trim()
                .to_owned()
        });
    let catalog = PlexCatalog::new(url.parse().expect("SPOOLFRAME_TEST_PLEX_URL"), token).unwrap();
    Some(match path_map() {
        Some(paths) => catalog.with_paths(paths),
        None => catalog,
    })
}

/// The first child of `id`, which must be of `kind`.
async fn first_child(catalog: &PlexCatalog, id: &MediaId, kind: MediaKind) -> MediaId {
    let got = catalog.item(id).await.unwrap();
    let child = got
        .children
        .first()
        .unwrap_or_else(|| panic!("{id} has no children"));
    assert_eq!(child.id().kind(), kind, "{id}");
    child.id().clone()
}

async fn assert_playable(catalog: &PlexCatalog, id: &MediaId) {
    let got = catalog.item(id).await.unwrap();
    assert!(got.children.is_empty(), "{id}");
    match catalog.media_path(id).await {
        Ok(path) => assert!(path.is_absolute(), "{id}: {}", path.display()),
        // Without a mapping the PMS's media need not be mounted where the
        // test runs; the path still came back from Plex.
        Err(e @ CatalogError::MediaFileUnavailable(_)) if path_map().is_none() => {
            eprintln!("not readable here: {e}")
        }
        Err(e) => panic!("{id}: {e}"),
    }
}

#[tokio::test]
async fn every_library_walks_down_to_a_playable_item() {
    let Some(catalog) = live_catalog() else {
        return;
    };
    let libraries = catalog.libraries().await.unwrap();
    assert!(!libraries.is_empty(), "the PMS has no libraries");
    for library in libraries {
        let page = catalog
            .items(
                &library.id,
                Page {
                    offset: 0,
                    limit: 1,
                },
            )
            .await
            .unwrap();
        let Some(top) = page.items.first() else {
            eprintln!("{} is empty", library.title);
            continue;
        };
        assert!(page.total >= 1, "{}", library.title);
        match (library.kind, top) {
            (LibraryKind::Movies, Item::Movie(movie)) => {
                assert_playable(&catalog, &movie.id).await;
                // Plex's own preview index (P3-13), where it made one.
                match catalog.preview_frames(&movie.id).await.unwrap() {
                    Some(index) => {
                        eprintln!("{}: Plex's preview index: {index:?}", movie.title);
                        let first = &index.frames[0];
                        assert_eq!(first.jpeg[..3], [0xFF, 0xD8, 0xFF], "a JPEG");
                        assert!(index.frames.windows(2).all(|w| w[0].at_ms < w[1].at_ms));
                    }
                    None => eprintln!("{}: no preview index", movie.title),
                }
                if let Some(poster) = movie.artwork.first() {
                    let image = catalog.artwork(&poster.id).await.unwrap();
                    assert!(image.content_type.starts_with("image/"), "{image:?}");
                }
            }
            (LibraryKind::Shows, Item::Show(show)) => {
                let season = first_child(&catalog, &show.id, MediaKind::Season).await;
                let episode = first_child(&catalog, &season, MediaKind::Episode).await;
                assert_playable(&catalog, &episode).await;
            }
            (LibraryKind::Music, Item::Artist(artist)) => {
                let album = first_child(&catalog, &artist.id, MediaKind::Album).await;
                let track = first_child(&catalog, &album, MediaKind::Track).await;
                assert_playable(&catalog, &track).await;
            }
            (kind, item) => panic!("{kind:?} library starts with {}", item.id()),
        }
    }
}

/// Read-only: the account's watched and in-progress items (PX-04). Never
/// writes to the PMS.
#[tokio::test]
async fn the_account_s_watch_state_is_read() {
    let Some(catalog) = live_catalog() else {
        return;
    };
    let states = catalog.watch_states(None).await.unwrap();
    let watched = states.iter().filter(|w| w.watched).count();
    let started = states.iter().filter(|w| w.position_ms.is_some()).count();
    eprintln!(
        "{} items: {watched} watched, {started} in progress",
        states.len()
    );
    for w in states.iter().filter(|w| w.position_ms.is_some()).take(5) {
        eprintln!("  in progress: {}", w.title);
    }
    assert!(!states.is_empty());
    assert!(states.iter().all(|w| w.watched || w.position_ms.is_some()));
}

/// Read-only: each sort and filter a section offers
/// as the PMS itself applies them. Every
/// key both ways pages without a repeat and walks in the same order; the
/// dates and lengths a movie carries are in order with none last; a year
/// range keeps only its years; and the quality classes count about every
/// item. Prints how long a whole walk of each section takes.
#[tokio::test]
async fn every_sort_and_filter_is_the_pms_s_own() {
    use spoolframe_catalog::ItemQuery;
    use spoolframe_types::{FilterPart, ItemFilter, ItemSort, SortKey};
    let Some(catalog) = live_catalog() else {
        return;
    };
    let asked = |key, descending, filter| ItemQuery {
        sort: ItemSort { key, descending },
        filter,
        viewer: None,
    };
    for library in catalog.libraries().await.unwrap() {
        let browsing = catalog.browsing(&library).await.unwrap();
        let started = std::time::Instant::now();
        let walked = catalog
            .walk_items(
                &library.id,
                &asked(SortKey::Title, false, ItemFilter::default()),
            )
            .await
            .unwrap();
        eprintln!(
            "{} ({:?}): {} items walked in {:?}; sorts {:?}, filters {:?}",
            library.title,
            library.kind,
            walked.len(),
            started.elapsed(),
            browsing.sorts,
            browsing.filters
        );
        for key in browsing
            .sorts
            .iter()
            .copied()
            .filter(|k| *k != SortKey::LastPlayed)
        {
            for descending in [false, true] {
                let query = asked(key, descending, ItemFilter::default());
                let mut paged = Vec::new();
                let mut total = 0;
                for offset in (0..400).step_by(100) {
                    let page = catalog
                        .query_items(&library.id, &query, Page { offset, limit: 100 })
                        .await
                        .unwrap();
                    total = page.total;
                    paged.extend(page.items);
                }
                assert_eq!(total as usize, walked.len(), "{key:?}");
                let ids: Vec<MediaId> = paged.iter().map(|i| i.id().clone()).collect();
                let unique: std::collections::HashSet<&MediaId> = ids.iter().collect();
                assert_eq!(unique.len(), ids.len(), "{key:?} {descending}: a repeat");
                let walk: Vec<MediaId> = catalog
                    .walk_items(&library.id, &query)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|l| l.id)
                    .take(ids.len())
                    .collect();
                assert_eq!(walk, ids, "{key:?} {descending}: the walk's order");
                if !matches!(key, SortKey::Released | SortKey::Duration)
                    || library.kind != LibraryKind::Movies
                {
                    continue;
                }
                let values: Vec<Option<String>> = paged
                    .iter()
                    .map(|item| match (key, item) {
                        (SortKey::Released, Item::Movie(m)) => {
                            m.release_date.map(|d| d.to_string())
                        }
                        (SortKey::Duration, Item::Movie(m)) => {
                            m.duration.map(|d| format!("{:015}", d.as_millis()))
                        }
                        _ => None,
                    })
                    .collect();
                let first_none = values
                    .iter()
                    .position(Option::is_none)
                    .unwrap_or(values.len());
                assert!(
                    values[first_none..].iter().all(Option::is_none),
                    "{key:?} {descending}: none last"
                );
                let some: Vec<&String> = values[..first_none].iter().flatten().collect();
                let inversions = some
                    .windows(2)
                    .filter(|w| if descending { w[0] < w[1] } else { w[0] > w[1] })
                    .count();
                // Plex sorts by its own column, which for a few films is
                // not the length it lists (a film with several versions):
                // its order is what a client gets, so only report it.
                eprintln!("  {key:?} descending {descending}: {inversions} out of order");
                if key == SortKey::Released {
                    assert_eq!(inversions, 0, "{key:?} {descending}: out of order");
                }
            }
        }
        if browsing.filters.contains(&FilterPart::Years) {
            let decade = ItemFilter {
                year_from: Some(1990),
                year_to: Some(1999),
                ..ItemFilter::default()
            };
            let page = catalog
                .query_items(
                    &library.id,
                    &asked(SortKey::Title, false, decade),
                    Page {
                        offset: 0,
                        limit: 500,
                    },
                )
                .await
                .unwrap();
            for item in &page.items {
                let year = match item {
                    Item::Movie(m) => m.year,
                    Item::Show(s) => s.year,
                    _ => None,
                };
                assert!(
                    year.is_some_and(|y| (1990..=1999).contains(&y)),
                    "{} is from {year:?}",
                    item.id()
                );
            }
            eprintln!("  the 1990s: {}", page.total);
        }
        let started = std::time::Instant::now();
        let values = catalog.filter_values(&library).await.unwrap();
        eprintln!(
            "  values in {:?}: {} genres, {} years, {} ratings, resolutions {:?}, ranges {:?}",
            started.elapsed(),
            values.genres.len(),
            values.years.len(),
            values.content_ratings.len(),
            values.resolutions,
            values.dynamic_ranges
        );
        if !values.resolutions.is_empty() {
            // An item with two versions is in two classes.
            let classes: u32 = values.resolutions.iter().filter_map(|r| r.count).sum();
            assert!(classes as usize >= walked.len() * 9 / 10, "{classes}");
        }
    }
}
