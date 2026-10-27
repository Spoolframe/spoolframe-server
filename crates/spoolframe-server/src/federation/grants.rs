//! What this server shares with its pairs (P8-03): server-wide grants of
//! its own libraries, checked against the catalog it serves.
//!
//! Revoking a grant ends what hangs on it ([`Federation::grant_revoked`]):
//! the play tokens minted for its items go at once (P8-03), and the pair
//! learns at its next sync that the library is gone and drops its cache
//! of it (P8-02). A grant added or revoked tells the pair at once, on the
//! connection held to it, so that next sync is soon
//! ([`Federation::tell_shares_changed`]).

use spoolframe_catalog::CatalogError;
use spoolframe_types::{Library, LibraryId, MediaId, Namespace};

use super::Federation;
use crate::store::StoreError;
use crate::store::federation_grants::GrantRecord;

/// Why libraries were not shared.
#[derive(Debug, thiserror::Error)]
pub enum ShareError {
    /// No live pair has that id.
    #[error("no paired server {0:?}")]
    NoSuchPeer(String),
    /// Not a library this server serves: unknown, or a paired server's.
    #[error("no library {0}")]
    NoSuchLibrary(String),
    /// The catalog cannot say which libraries it serves now.
    #[error("cannot list the libraries: {0}")]
    Catalog(#[from] CatalogError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl Federation {
    /// The libraries shared with live pair `peer` now, the oldest grant
    /// first.
    pub async fn grants(&self, peer: &str) -> Result<Vec<GrantRecord>, StoreError> {
        self.store().live_grants(peer).await
    }

    /// The libraries this server serves now that it could share: its
    /// own, never a paired server's, which is not shared on.
    pub async fn shareable(&self) -> Result<Vec<Library>, CatalogError> {
        Ok(self
            .catalog
            .libraries()
            .await?
            .into_iter()
            .filter(|l| l.id.namespace() != Namespace::Federated)
            .collect())
    }

    /// Share `libraries` with live pair `peer`, as admin `by`: each must
    /// be one this server serves ([`Federation::shareable`]), or none is
    /// shared. A library shared already keeps its grant. The grants, in
    /// the order asked.
    pub async fn share(
        &self,
        peer: &str,
        libraries: &[LibraryId],
        by: &str,
    ) -> Result<Vec<GrantRecord>, ShareError> {
        let served = self.shareable().await?;
        if let Some(unknown) = libraries
            .iter()
            .find(|asked| !served.iter().any(|l| l.id == **asked))
        {
            return Err(ShareError::NoSuchLibrary(unknown.to_string()));
        }
        let mut grants = Vec::with_capacity(libraries.len());
        for library in libraries {
            let grant = self
                .store()
                .grant_library(peer, &library.to_string(), by)
                .await?
                .ok_or_else(|| ShareError::NoSuchPeer(peer.to_owned()))?;
            tracing::info!(
                peer,
                library = %library,
                by,
                "a library is shared with a paired server"
            );
            grants.push(grant);
        }
        self.tell_shares_changed(peer).await;
        Ok(grants)
    }

    /// Tell live pair `peer` that what this server shares with it changed,
    /// on the connection this server holds to it, without waiting for its
    /// answer: it syncs this server soon. A pair not connected is not
    /// told: it syncs on connecting. Best effort: a pair that does not
    /// know the notice, or misses it, syncs at its own interval.
    pub(crate) async fn tell_shares_changed(&self, peer: &str) {
        let record = match self.store().federation_peer(peer).await {
            Ok(Some(record)) if record.live() => record,
            Ok(_) => return,
            Err(e) => {
                tracing::warn!(peer, error = %e, "cannot look a pair up to tell it its shares changed");
                return;
            }
        };
        let Some(dialed) = self.links.pinned(&record) else {
            return;
        };
        tokio::spawn(async move {
            if let Err(e) = dialed.shares_changed().await {
                tracing::debug!(
                    peer = %record.server_id,
                    error = %e,
                    "a paired server was not told its shares changed: it syncs at its interval"
                );
            }
        });
    }

    /// Stop sharing `library` with pair `peer`, as admin `by`: the grant
    /// revoked, if there was one.
    pub async fn unshare(
        &self,
        peer: &str,
        library: &str,
        by: &str,
    ) -> Result<Option<GrantRecord>, StoreError> {
        let Some(grant) = self.store().revoke_library_grant(peer, library).await? else {
            return Ok(None);
        };
        tracing::info!(
            peer,
            library,
            by,
            "a library is no longer shared with a paired server"
        );
        self.grant_revoked(&grant).await?;
        self.tell_shares_changed(peer).await;
        Ok(Some(grant))
    }

    /// `grant` was revoked: what hangs on it ends here. The pair's tokens
    /// for the library's items are deleted at once (P8-03), so a new play
    /// is refused; the sessions P8-04 opened with them end here too, with
    /// their playbacks. The
    /// paired server drops its cache of the library at its next sync,
    /// when this one refuses it (P8-02). Unpairing deletes every token of
    /// the pair with its row.
    pub(crate) async fn grant_revoked(&self, grant: &GrantRecord) -> Result<(), StoreError> {
        self.end_remote_sessions(&grant.peer, Some(&grant.library));
        // What was being made for the pair's users of the library stops
        // (P8-07).
        if let Some(peer) = self.store().federation_peer(&grant.peer).await? {
            self.cancel_served_downloads(&peer, Some(&grant.library))
                .await;
        }
        let deleted = self
            .store()
            .delete_federation_tokens(&grant.peer, Some(&grant.library))
            .await?;
        tracing::debug!(
            peer = %grant.peer,
            library = %grant.library,
            tokens = deleted,
            "a grant ended"
        );
        Ok(())
    }

    /// Library `library` if it is shared with live pair `peer` now and
    /// still served: what every request of a pair's is checked against
    /// ([`Federation::shared_library_of`], the sync's and the images'
    /// answers).
    pub(crate) async fn granted(
        &self,
        peer: &str,
        library: &str,
    ) -> Result<Option<Library>, ShareError> {
        if !self.store().is_granted(peer, library).await? {
            return Ok(None);
        }
        Ok(self
            .shareable()
            .await?
            .into_iter()
            .find(|l| l.id.to_string() == library))
    }

    /// The library shared with live pair `peer` that item `id` is in now:
    /// what a request about one item from that pair is checked against
    /// (P8-03's assertions, P8-04's tokens). An item the catalog cannot
    /// place is not shared, nor ever a paired server's item: it is not
    /// shared on.
    pub async fn shared_library_of(
        &self,
        peer: &str,
        id: &MediaId,
    ) -> Result<Option<LibraryId>, ShareError> {
        if id.namespace() == Namespace::Federated {
            return Ok(None);
        }
        let Some(library) = self.catalog.library_of(id).await? else {
            return Ok(None);
        };
        Ok(self
            .granted(peer, &library.to_string())
            .await?
            .map(|l| l.id))
    }

    /// Whether item `id` is in `library` now, shared with live pair
    /// `peer`: what a paired server's token and sessions are held to as
    /// the catalog changes (F8-04). `None` when the catalog cannot say now.
    pub(crate) async fn shared_in(&self, peer: &str, id: &MediaId, library: &str) -> Option<bool> {
        match self.placed_shared(peer, id).await {
            Ok(found) => Some(found.is_some_and(|l| l.to_string() == library)),
            Err(e) => {
                tracing::warn!(%id, error = %e, "cannot place a paired server's session's item");
                None
            }
        }
    }

    /// [`Federation::shared_library_of`], with an item the catalog does
    /// not have (not there, or in no library served) as not shared: only
    /// what keeps the catalog from saying is an error.
    pub(crate) async fn placed_shared(
        &self,
        peer: &str,
        id: &MediaId,
    ) -> Result<Option<LibraryId>, ShareError> {
        match self.shared_library_of(peer, id).await {
            Err(ShareError::Catalog(
                CatalogError::NotFound(_) | CatalogError::NotServed(_) | CatalogError::ForeignId(_),
            )) => Ok(None),
            placed => placed,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::auth::Auth;
    use crate::network::ServerName;
    use crate::store::federation::NewPeer;
    use crate::store::tests::test_store;
    use crate::test_support::{FakeCatalog, artist, episode, movie};

    /// An item is shared when the catalog places it in a library shared
    /// with the pair; one in another library, or in none, is not.
    #[tokio::test]
    async fn an_item_is_shared_by_the_library_it_is_in() {
        let store = test_store("fed-shares-item").await;
        let catalog = Arc::new(FakeCatalog::new("/nowhere/movie.mkv".into()));
        catalog.serve_music();
        let federation = Federation::new(
            Auth::new(store.store.clone()),
            catalog,
            "aaaaaaaaaaaaaaaa".parse().unwrap(),
            ServerName::new("Alice's server"),
            super::super::ReplicaSettings::in_dir("/nowhere/federation".into()),
        );
        let peer = store
            .add_peer(
                NewPeer {
                    server_id: "mzxw6ytbojtg633c",
                    endpoint_id: &"0b".repeat(32),
                    display_name: "Bob's server",
                    addrs: &[],
                    relays: &[],
                },
                "alice",
            )
            .await
            .unwrap();
        assert!(matches!(
            federation
                .share(&peer.id, &["plex:9".parse().unwrap()], "alice")
                .await,
            Err(ShareError::NoSuchLibrary(_))
        ));
        federation
            .share(&peer.id, &["plex:1".parse().unwrap()], "alice")
            .await
            .unwrap();
        let shared = |item: spoolframe_types::Item| {
            let federation = &federation;
            let peer = peer.id.clone();
            async move {
                federation
                    .shared_library_of(&peer, item.id())
                    .await
                    .unwrap()
                    .is_some()
            }
        };
        assert!(shared(movie()).await);
        assert!(!shared(artist()).await, "music is not shared");
        assert!(!shared(episode("12", "11", 1, 1)).await, "in no library");
        federation
            .unshare(&peer.id, "plex:1", "alice")
            .await
            .unwrap();
        assert!(!shared(movie()).await);
    }
}
