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
use crate::store::federation_tokens::{DAY, PlayCount, PlaySide};

/// How many items a paired server's Home row holds.
const ROW: u32 = 20;

/// The days a paired server's own page counts plays over, today the last;
/// what it says a library was withdrawn within, too.
pub(crate) const WEEK_DAYS: i64 = 7;

/// What a paired server's own page on the Federation page shows beyond
/// its row (P8-01–P8-04).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerPage {
    /// When it was last seen connected, Unix seconds.
    pub last_seen: Option<i64>,
    /// When its key last said hello under another server id.
    pub identity_refused_at: Option<i64>,
    /// What it shares with this server, as cached here.
    pub received: Vec<Received>,
    /// Its artwork kept here, in bytes.
    pub cached_bytes: u64,
    /// The libraries it stopped sharing this week, as (library, title,
    /// when), the latest first.
    pub withdrawn: Vec<(String, String, i64)>,
    /// The week's plays, a day each, the oldest first: seven of them.
    pub week: Vec<PlayDay>,
}

/// A library a paired server shares with this one, as cached here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Received {
    pub library: String,
    pub title: String,
    pub kind: String,
    /// Its films, shows or artists.
    pub items: u64,
    pub revision: i64,
    pub synced_at: Option<i64>,
}

/// One day's plays across a pair, from its first second, Unix seconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlayDay {
    pub day: i64,
    /// Of this server's items by its users, and of them played through it.
    pub here: u64,
    pub proxied_here: u64,
    /// Of its items by this server's users, and of them played on by this
    /// server.
    pub there: u64,
    pub proxied_there: u64,
}

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
    pub(super) async fn servers(&self) -> Result<HashMap<String, LibraryServer>, StoreError> {
        Ok(self
            .live_peers()
            .await?
            .iter()
            .map(|p| (p.server_id.clone(), self.server(p)))
            .collect())
    }

    async fn live_peers(&self) -> Result<Vec<PeerRecord>, StoreError> {
        let mut peers = self.store().federation_peers().await?;
        peers.retain(PeerRecord::live);
        Ok(peers)
    }

    /// Pair `peer` as it is shown to a client: by the name this server's
    /// admin gave it, if any.
    fn server(&self, peer: &PeerRecord) -> LibraryServer {
        LibraryServer {
            online: self.links.online(&peer.server_id),
            server_id: peer.server_id.clone(),
            name: peer.name().to_owned(),
        }
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
    /// its row off. A server whose row this server's admin turned off has
    /// none, for every user.
    pub async fn home_rows(&self, principal: &str) -> Result<Vec<ServerRow>, StoreError> {
        let servers: HashMap<String, LibraryServer> = self
            .live_peers()
            .await?
            .iter()
            .filter(|p| p.home_row)
            .map(|p| (p.server_id.clone(), self.server(p)))
            .collect();
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

    /// What pair `peer`'s own page shows beyond its row.
    pub async fn peer_page(&self, peer: &PeerRecord) -> Result<PeerPage, StoreError> {
        let store = self.store();
        let now = store.now();
        let first = now.div_euclid(DAY) - (WEEK_DAYS - 1);
        // Seven reads that depend on nothing but the pair, at once: the
        // page waits for the slowest, not their sum.
        let (
            (last_seen, identity_refused_at),
            counts,
            libraries,
            served_days,
            asserted_days,
            cached_bytes,
            withdrawn,
        ) = tokio::try_join!(
            store.peer_presence(&peer.id),
            store.cached_counts(&peer.server_id),
            store.cached_libraries(Some(&peer.server_id)),
            store.federation_play_days(&peer.id, PlaySide::Served, first),
            store.federation_play_days(&peer.id, PlaySide::Asserted, first),
            store.cached_bytes(&peer.server_id),
            store.withdrawn(&peer.id, now - WEEK_DAYS * DAY),
        )?;
        let counts: HashMap<String, i64> = counts.into_iter().collect();
        let received = libraries
            .into_iter()
            .map(|l| Received {
                items: counts
                    .get(&l.library)
                    .and_then(|n| u64::try_from(*n).ok())
                    .unwrap_or_default(),
                library: l.library,
                title: l.title,
                kind: l.kind,
                revision: l.revision,
                synced_at: l.synced_at,
            })
            .collect();
        let mut week: Vec<PlayDay> = (first..first + WEEK_DAYS)
            .map(|day| PlayDay {
                day: day * DAY,
                ..PlayDay::default()
            })
            .collect();
        for (days, there) in [(served_days, false), (asserted_days, true)] {
            for (day, plays, proxied) in days {
                let Some(slot) = usize::try_from(day - first)
                    .ok()
                    .and_then(|i| week.get_mut(i))
                else {
                    continue;
                };
                if there {
                    slot.there = plays;
                    slot.proxied_there = proxied;
                } else {
                    slot.here = plays;
                    slot.proxied_here = proxied;
                }
            }
        }
        Ok(PeerPage {
            last_seen,
            identity_refused_at,
            received,
            cached_bytes,
            withdrawn,
            week,
        })
    }

    /// Pair `peer`'s plays and refusals (P8-03), one count per server and
    /// never per user: of this server's items by its users, and of its
    /// items by this server's users.
    pub async fn play_counts(&self, peer: &str) -> Result<(PlayCount, PlayCount), StoreError> {
        tokio::try_join!(
            self.store().federation_plays(peer, PlaySide::Served),
            self.store().federation_plays(peer, PlaySide::Asserted),
        )
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
