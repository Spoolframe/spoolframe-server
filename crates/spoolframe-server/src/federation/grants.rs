//! What this server shares with its pairs (P8-03): server-wide grants of
//! its own libraries, checked against the catalog it serves.
//!
//! Revoking a grant ends what hangs on it ([`Federation::grant_revoked`]):
//! the play tokens minted for its items go at once (P8-03), and the pair
//! learns at its next sync that the library is gone and drops its cache
//! of it (P8-02).

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
        Ok(grants)
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
        Ok(Some(grant))
    }

    /// `grant` was revoked: what hangs on it ends here. The pair's tokens
    /// for the library's items are deleted at once (P8-03), so a new play
    /// is refused; the sessions P8-04 opens with them end here too. The
    /// paired server drops its cache of the library at its next sync,
    /// when this one refuses it (P8-02). Unpairing deletes every token of
    /// the pair with its row.
    pub(crate) async fn grant_revoked(&self, grant: &GrantRecord) -> Result<(), StoreError> {
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
