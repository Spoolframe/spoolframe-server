//! What a client that names the experimental `federation` feature is shown
//! of the paired servers (P8-02): their libraries, each with its server,
//! and each server's own Home row. A client that does not name it is shown
//! none of this; the client protocol decides which it is.

use std::collections::HashMap;

use spoolframe_catalog::{Catalog, CatalogError};
use spoolframe_protocol::ServerRow;
use spoolframe_types::{Library, LibraryServer};

use super::Federation;
use crate::store::StoreError;
use crate::store::federation::PeerRecord;
use crate::store::federation_tokens::{PlayCount, PlaySide};

/// How many items a paired server's Home row holds.
const ROW: u32 = 20;

/// Why a server's Home row was not set.
#[derive(Debug, thiserror::Error)]
pub enum ServerRowError {
    /// No live pair has that server id.
    #[error("no paired server {0:?}")]
    NoSuchServer(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl Federation {
    /// Told when what [`Federation::libraries`] lists may have changed: a
    /// paired server came online or went offline, or a sync found a
    /// library shared or no longer shared. Clients that named the feature
    /// are told `LibrariesChanged`.
    pub fn libraries_changed(&self) -> tokio::sync::broadcast::Receiver<()> {
        self.links.changes()
    }

    /// The live pairs, by server id, as each is shown to a client.
    async fn servers(&self) -> Result<HashMap<String, LibraryServer>, StoreError> {
        Ok(self
            .store()
            .federation_peers()
            .await?
            .into_iter()
            .filter(PeerRecord::live)
            .map(|p| {
                let server = LibraryServer {
                    online: self.links.online(&p.server_id),
                    server_id: p.server_id.clone(),
                    name: p.display_name,
                };
                (p.server_id, server)
            })
            .collect())
    }

    /// The libraries the paired servers share with this one, as cached,
    /// each with its server: browsed from the cache, online or not. Those
    /// of a pair that is gone are not listed.
    pub async fn libraries(&self) -> Result<Vec<Library>, CatalogError> {
        let servers = self.servers().await?;
        Ok(self
            .cache
            .libraries()
            .await?
            .into_iter()
            .filter_map(|mut library| {
                let (origin, _) = library.id.remote()?;
                library.server = Some(servers.get(origin)?.clone());
                Some(library)
            })
            .collect())
    }

    /// Each paired server's own Home row for `principal` (by its text):
    /// the servers that share something with this one, by name, each with
    /// what it shared most recently, or with nothing when the user turned
    /// its row off.
    pub async fn home_rows(&self, principal: &str) -> Result<Vec<ServerRow>, StoreError> {
        let servers = self.servers().await?;
        let hidden = self.store().hidden_server_rows(principal).await?;
        let mut sharing: Vec<&LibraryServer> = Vec::new();
        for library in self.store().cached_libraries(None).await? {
            if let Some(server) = servers.get(&library.origin)
                && !sharing.contains(&server)
            {
                sharing.push(server);
            }
        }
        sharing.sort_by(|a, b| a.name.cmp(&b.name).then(a.server_id.cmp(&b.server_id)));
        let mut rows = Vec::with_capacity(sharing.len());
        for server in sharing {
            let shown = !hidden.contains(&server.server_id);
            let items = if shown {
                self.cache
                    .recently_added_from(&server.server_id, ROW)
                    .await?
            } else {
                Vec::new()
            };
            rows.push(ServerRow {
                server: server.clone(),
                shown,
                items,
            });
        }
        Ok(rows)
    }

    /// When this server last synced what `origin` shares with it, whole
    /// (P8-02): the latest of its libraries' syncs; `None` before the
    /// first, or while it shares nothing.
    pub async fn last_sync(&self, origin: &str) -> Result<Option<i64>, StoreError> {
        Ok(self
            .store()
            .cached_libraries(Some(origin))
            .await?
            .iter()
            .filter_map(|l| l.synced_at)
            .max())
    }

    /// Pair `peer`'s plays and refusals (P8-03), one count per server and
    /// never per user: of this server's items by its users, and of its
    /// items by this server's users.
    pub async fn play_counts(&self, peer: &str) -> Result<(PlayCount, PlayCount), StoreError> {
        let here = self
            .store()
            .federation_plays(peer, PlaySide::Served)
            .await?;
        let there = self
            .store()
            .federation_plays(peer, PlaySide::Asserted)
            .await?;
        Ok((here, there))
    }

    /// Show paired server `server_id`'s own Home row to `principal` (by
    /// its text), or not.
    pub async fn set_server_row(
        &self,
        principal: &str,
        server_id: &str,
        shown: bool,
    ) -> Result<(), ServerRowError> {
        if self.live_peer_by_server_id(server_id).await?.is_none() {
            return Err(ServerRowError::NoSuchServer(server_id.to_owned()));
        }
        self.store()
            .set_server_row(principal, server_id, shown)
            .await?;
        Ok(())
    }
}
