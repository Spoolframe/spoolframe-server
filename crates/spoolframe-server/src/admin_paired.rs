//! The libraries paired servers share with this one, browsed by an admin
//! (experimental `federation`, P8-02): by server, from this server's
//! cache of them, online or not; a library's page sorted and filtered
//! through the query clients get ([`crate::watch::Watch::browse`]); an
//! item as far as the cache has it. Read-only: the paired server owns
//! them, and its files are there.

use std::sync::Arc;

use jsonrpsee::core::RpcResult;
use jsonrpsee::types::ErrorObjectOwned;
use spoolframe_admin_api::{
    CatalogCrumb, PairedItem, PairedLibrary, PairedPage, PairedQuery, PairedServer, error_code,
};
use spoolframe_catalog::{Catalog, Page};
use spoolframe_types::{Item, Library, LibraryId, LibraryServer, MediaId, Namespace};

use super::catalog::{MAX_ROWS, catalog_error, not_browsed, watch_error};
use super::{AdminApi, error};
use crate::principal::Principal;
use crate::store::users::LocalUserId;

fn off() -> ErrorObjectOwned {
    error(
        error_code::NOT_CONFIGURED,
        "federation is off: experimental.federation",
    )
}

fn paired_library(library: &Library) -> PairedLibrary {
    PairedLibrary {
        id: library.id.to_string(),
        title: library.title.clone(),
        kind: library.kind,
    }
}

impl AdminApi {
    fn federation_on(&self) -> RpcResult<&Arc<crate::federation::Federation>> {
        self.federation.as_ref().ok_or_else(off)
    }

    /// Every library the paired servers share, as cached, each with its
    /// server.
    async fn paired(&self) -> RpcResult<Vec<Library>> {
        self.federation_on()?
            .libraries()
            .await
            .map_err(catalog_error)
    }

    /// The paired library `id` names, with its server.
    pub(super) async fn paired_library(&self, id: &str) -> RpcResult<(Library, LibraryServer)> {
        let parsed: LibraryId = id
            .parse()
            .map_err(|_| error(error_code::INVALID_INPUT, format!("no library {id}")))?;
        self.paired()
            .await?
            .into_iter()
            .find(|l| l.id == parsed)
            .and_then(|l| {
                let server = l.server.clone()?;
                Some((l, server))
            })
            .ok_or_else(|| error(error_code::INVALID_INPUT, format!("no library {id}")))
    }

    pub(super) async fn catalog_paired_libraries_of(&self) -> RpcResult<Vec<PairedServer>> {
        let mut servers: Vec<PairedServer> = Vec::new();
        for library in self.paired().await? {
            let Some(server) = &library.server else {
                continue;
            };
            let entry = match servers.iter_mut().find(|s| s.server_id == server.server_id) {
                Some(entry) => entry,
                None => {
                    servers.push(PairedServer {
                        server_id: server.server_id.clone(),
                        name: server.name.clone(),
                        online: server.online,
                        libraries: Vec::new(),
                    });
                    servers.last_mut().expect("just pushed")
                }
            };
            entry.libraries.push(paired_library(&library));
        }
        // By name, as clients group them; each one's libraries by title.
        servers.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.server_id.cmp(&b.server_id))
        });
        for server in &mut servers {
            server
                .libraries
                .sort_by_key(|l| (l.title.to_lowercase(), l.id.clone()));
        }
        Ok(servers)
    }

    pub(super) async fn catalog_paired_items_of(
        &self,
        user: LocalUserId,
        query: PairedQuery,
    ) -> RpcResult<PairedPage> {
        if query.limit > MAX_ROWS {
            return Err(error(
                error_code::INVALID_INPUT,
                format!("at most {MAX_ROWS} rows a page"),
            ));
        }
        let (library, server) = self.paired_library(&query.library).await?;
        let watch = self.watch.as_ref().ok_or_else(not_browsed)?;
        let principal = Principal::LocalUser(user);
        let page = watch
            .browse(
                principal,
                &library,
                query.sort,
                query.item_filter,
                Page {
                    offset: query.offset,
                    limit: query.limit,
                },
            )
            .await
            .map_err(|e| watch_error(e, &query.library))?;
        let mut items = page.items;
        {
            let mut filled: Vec<&mut Item> = items.iter_mut().collect();
            watch
                .fill(principal, &mut filled)
                .await
                .map_err(super::internal)?;
        }
        Ok(PairedPage {
            library: paired_library(&library),
            server_id: server.server_id,
            server_name: server.name,
            online: server.online,
            items,
            total: page.total,
        })
    }

    pub(super) async fn catalog_paired_item_of(
        &self,
        user: LocalUserId,
        id: String,
    ) -> RpcResult<PairedItem> {
        self.federation_on()?;
        let parsed: MediaId = id
            .parse()
            .map_err(|_| error(error_code::INVALID_INPUT, format!("no item {id}")))?;
        let Some((origin, _)) = parsed
            .remote()
            .filter(|_| parsed.namespace() == Namespace::Federated)
        else {
            return Err(error(
                error_code::INVALID_INPUT,
                format!("{id} is not a paired server's"),
            ));
        };
        let origin = origin.to_owned();
        let mut found = self.plex.item(&parsed).await.map_err(catalog_error)?;
        if let Some(watch) = &self.watch {
            let mut items: Vec<&mut Item> = std::iter::once(&mut found.item)
                .chain(found.children.iter_mut())
                .collect();
            watch
                .fill(Principal::LocalUser(user), &mut items)
                .await
                .map_err(super::internal)?;
        }
        let libraries = self.paired().await?;
        let server = libraries
            .iter()
            .filter_map(|l| l.server.clone())
            .find(|s| s.server_id == origin);
        let library = self
            .plex
            .library_of(&parsed)
            .await
            .ok()
            .flatten()
            .and_then(|id| libraries.iter().find(|l| l.id == id))
            .map(|l| CatalogCrumb {
                id: l.id.to_string(),
                title: l.title.clone(),
            });
        Ok(PairedItem {
            library,
            server_id: origin.clone(),
            server_name: server
                .as_ref()
                .map_or_else(|| origin.clone(), |s| s.name.clone()),
            online: server.is_some_and(|s| s.online),
            item: found.item,
            children: found.children,
            external_ids: found.external_ids,
        })
    }
}
