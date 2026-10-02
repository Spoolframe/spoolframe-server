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
            (LibraryKind::Movies, Item::Movie(movie)) => assert_playable(&catalog, &movie.id).await,
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
