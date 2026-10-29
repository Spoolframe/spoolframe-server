//! An admin's invites from the app: mint one, list the live ones, revoke
//! one. Minting and revoking need a recent sign-in, as on the web.

use spoolframe_client_sdk::{InviteSpec, InviteSummary, MintedInvite};

use crate::error::SpoolError;
use crate::session::SpoolSession;

#[uniffi::export]
impl SpoolSession {
    /// Mint an invite (admins; `RecentSignInRequired`: confirm, then
    /// again). `role` only for an account invite; `uses` and `expires_in`
    /// `None` for the server's defaults.
    pub async fn create_invite(&self, spec: InviteSpec) -> Result<MintedInvite, SpoolError> {
        self.call(|s| async move { s.create_invite(spec).await })
            .await
    }

    /// The live invites, newest first as the server lists them.
    pub async fn invites(&self) -> Result<Vec<InviteSummary>, SpoolError> {
        self.call(|s| async move { s.invites().await }).await
    }

    /// Revoke an invite; `NotFound` when it's gone already.
    pub async fn revoke_invite(&self, id: String) -> Result<(), SpoolError> {
        self.call(|s| async move { s.revoke_invite(&id).await })
            .await
    }
}
