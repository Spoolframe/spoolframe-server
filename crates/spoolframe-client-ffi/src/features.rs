//! The protocol's regular features, and the calls each gates. A
//! feature's calls cross as an object of their own, got only when the
//! feature is on: in a session, `next_episodes()` for `NextEpisodes`
//! (`spoolframe:next-episode`); before one, for an invite's server,
//! `SpoolClient.invite_passkeys(link)` for `InvitePasskeys`
//! (`spoolframe:invite-passkeys`). Off, the first is `None` and the
//! second `FeatureOff`, and nothing gated is sent.

use std::sync::Arc;

use spoolframe_client_sdk::gate::{InvitePasskeys, NextEpisodes};
use spoolframe_client_sdk::types::{Item, MediaId};
use spoolframe_client_sdk::{
    self as sdk, Feature, FinishPasskeyRegistration, NextMode, PasskeyCreation,
};

use crate::client::SpoolClient;
use crate::error::SpoolError;
use crate::runtime::run;
use crate::session::SpoolSession;

/// The calls of `spoolframe:next-episode`.
#[derive(uniffi::Object)]
pub struct NextEpisodesApi(sdk::On<NextEpisodes>);

#[uniffi::export]
impl NextEpisodesApi {
    /// The episode after `item`, in order or the first unwatched;
    /// specials only with `specials`. `None` at the show's end.
    pub async fn next_episode(
        &self,
        item: MediaId,
        mode: NextMode,
        specials: bool,
    ) -> Result<Option<Item>, SpoolError> {
        let on = self.0.clone();
        Ok(run(async move { on.next_episode(&item, mode, specials).await }).await?)
    }

    /// Where `show`'s Continue goes; `None` with every episode watched.
    pub async fn show_next(&self, show: MediaId) -> Result<Option<Item>, SpoolError> {
        let on = self.0.clone();
        Ok(run(async move { on.show_next(&show).await }).await?)
    }
}

/// The calls of `spoolframe:invite-passkeys`, for one invite's server:
/// an account made with the invite and an app passkey, no password.
#[derive(uniffi::Object)]
pub struct InvitePasskeysApi(sdk::Invited<InvitePasskeys>);

#[uniffi::export]
impl InvitePasskeysApi {
    /// The invite link it was made for.
    pub fn link(&self) -> String {
        self.0.link().to_owned()
    }

    /// What the platform makes the passkey with, for `username`.
    pub async fn begin_invite_passkey(
        &self,
        username: String,
        display_name: Option<String>,
    ) -> Result<PasskeyCreation, SpoolError> {
        let invited = self.0.clone();
        Ok(run(async move {
            invited
                .begin_invite_passkey(&username, display_name.as_deref())
                .await
        })
        .await?)
    }

    /// Make the account with the passkey made, and keep its session.
    pub async fn redeem_invite_passkey(
        &self,
        made: FinishPasskeyRegistration,
        remember: bool,
    ) -> Result<Arc<SpoolSession>, SpoolError> {
        let invited = self.0.clone();
        let session =
            run(async move { invited.redeem_invite_passkey(made, remember).await }).await?;
        Ok(Arc::new(SpoolSession::new(session)))
    }
}

#[uniffi::export]
impl SpoolSession {
    /// Whether `feature` is on for this connection: this app asked for it
    /// and the server has it.
    pub fn has_feature(&self, feature: Feature) -> bool {
        self.session.has_feature(feature)
    }

    /// Next episodes and Show next; `None` when the feature is off for
    /// this connection (an older server, or one without it).
    pub fn next_episodes(&self) -> Option<Arc<NextEpisodesApi>> {
        self.session
            .feature::<NextEpisodes>()
            .ok()
            .map(|on| Arc::new(NextEpisodesApi(on)))
    }
}

#[uniffi::export]
impl SpoolClient {
    /// Making an account with invite `link`'s code and an app passkey:
    /// its server is reached, and `FeatureOff` when it does not have the
    /// feature (offer a password instead).
    pub async fn invite_passkeys(
        &self,
        link: String,
    ) -> Result<Arc<InvitePasskeysApi>, SpoolError> {
        let client = self.client.clone();
        let invited = run(async move { client.invited::<InvitePasskeys>(&link).await }).await?;
        Ok(Arc::new(InvitePasskeysApi(invited)))
    }
}
