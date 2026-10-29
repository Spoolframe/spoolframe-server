//! Federation (experimental): a server paired with others, their
//! libraries browsed beside its own. Browsing needs nothing here: a
//! paired server's libraries come with `libraries` (each with its
//! `server`), its items under `fed:` ids, and Home's rows per server.
//! Playing one is `play`, as any item.

use spoolframe_client_sdk::types::{LibraryId, MediaId, SearchKind, SearchScope};
use spoolframe_client_sdk::{self as sdk, PeerServer};

use crate::error::SpoolError;
use crate::session::SpoolSession;
use crate::values::HitSearchResults;

#[uniffi::export]
impl SpoolSession {
    /// Pair this server with the one whose `federation` invite `link` is
    /// (an admin's, recently confirmed: `RecentSignInRequired`, confirm
    /// and ask again). Look the link up first (`look_up_invite`) for the
    /// admin to check who it's from.
    pub async fn pair_server(&self, link: String) -> Result<PeerServer, SpoolError> {
        self.call(|s| async move { s.pair_server(&link).await })
            .await
    }

    /// Share `libraries` of this server with the paired `server_id`: the
    /// libraries shared.
    pub async fn share_libraries(
        &self,
        server_id: String,
        libraries: Vec<LibraryId>,
    ) -> Result<Vec<LibraryId>, SpoolError> {
        self.call(|s| async move { s.share_libraries(&server_id, &libraries).await })
            .await
    }

    /// Show or hide the paired `server_id`'s row on Home.
    pub async fn set_server_row(&self, server_id: String, shown: bool) -> Result<(), SpoolError> {
        self.call(|s| async move { s.set_server_row(&server_id, shown).await })
            .await
    }

    /// `search` over this server and what its paired servers share, each
    /// item with where it's from, its other copies and the copy that
    /// plays.
    pub async fn search_hits(
        &self,
        query: String,
        limit: u32,
        scope: SearchScope,
        kind: Option<SearchKind>,
        library: Option<LibraryId>,
    ) -> Result<HitSearchResults, SpoolError> {
        let request = sdk::SearchRequest {
            query,
            limit,
            scope,
            kind,
            library,
        };
        let results = self
            .call(|s| async move { s.search_hits(request).await })
            .await?;
        Ok(results.into())
    }

    /// Play the title `title` (any copy's id) from the paired server
    /// `from`, or from this server with `None`, from now on.
    pub async fn set_play_from(
        &self,
        title: MediaId,
        from: Option<String>,
    ) -> Result<(), SpoolError> {
        self.call(|s| async move { s.set_play_from(&title, from.as_deref()).await })
            .await
    }
}
