//! The protocol's regular features, and the calls each gates. A
//! feature's calls cross as an object of their own, got only when the
//! feature is on: in a session, `october_2026()`; before one, for an
//! invite's server, `SpoolClient.invited_october_2026(link)`, both of
//! `spoolframe:october-2026`. Off, the first is `None` and the second
//! `FeatureOff`, and nothing gated is sent. `download_addons()` is
//! `spoolframe:download-addons`'s, as `october_2026()` is.

use std::sync::Arc;

use spoolframe_client_sdk::gate::{DownloadAddons, October2026};
use spoolframe_client_sdk::types::{Item, MediaId};
use spoolframe_client_sdk::{
    self as sdk, DownloadAddonOptions, DownloadTrack, Feature, FinishPasskeyRegistration,
    LocalDownload, NextMode, PasskeyCreation,
};

use crate::client::SpoolClient;
use crate::error::SpoolError;
use crate::runtime::run;
use crate::session::SpoolSession;

/// The calls of `spoolframe:october-2026` in a session.
#[derive(uniffi::Object)]
pub struct October2026Api(sdk::On<October2026>);

#[uniffi::export]
impl October2026Api {
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

/// The calls of `spoolframe:download-addons` in a session: subtitles and
/// audio tracks added to a download without fetching the film again.
#[derive(uniffi::Object)]
pub struct DownloadAddonsApi(sdk::On<DownloadAddons>);

#[uniffi::export]
impl DownloadAddonsApi {
    /// What may be added to a download of film or episode `id`, and why
    /// a track may not.
    pub async fn options(&self, id: MediaId) -> Result<DownloadAddonOptions, SpoolError> {
        let on = self.0.clone();
        Ok(run(async move { on.download_addon_options(&id).await }).await?)
    }

    /// Subtitle `subtitle` (its place in the item's tracks) of `id`, whole,
    /// as WebVTT text, waiting while the server reads pictures as text.
    pub async fn read_track_subtitle(
        &self,
        id: MediaId,
        subtitle: u32,
    ) -> Result<String, SpoolError> {
        let on = self.0.clone();
        Ok(run(async move { on.read_track_subtitle(&id, subtitle).await }).await?)
    }

    /// Add `subtitles` and `audio` (places in the item's tracks) to
    /// download `key`, this session's user's: each subtitle is fetched in
    /// the call; each audio track is queued for the download manager,
    /// which goes on from what arrived after a cut or a restart, and this
    /// returns once it is. Calling it again for a failed or paused audio
    /// track retries it. The download's `addons` say where each is.
    pub async fn add_tracks(
        &self,
        key: String,
        audio: Vec<u32>,
        subtitles: Vec<u32>,
    ) -> Result<LocalDownload, SpoolError> {
        let on = self.0.clone();
        Ok(run(async move { on.add_tracks(&key, audio, subtitles).await }).await?)
    }
}

/// Every track a player of `download` may offer, the file's and the
/// add-ons here, as one list: the file's audio, the audio added, the
/// file's subtitles, the subtitles added.
#[uniffi::export]
pub fn download_tracks_with_addons(download: LocalDownload) -> Vec<DownloadTrack> {
    download.tracks_with_addons()
}

/// The calls of `spoolframe:october-2026`, for one invite's server:
/// an account made with the invite and an app passkey, no password.
#[derive(uniffi::Object)]
pub struct October2026InvitedApi(sdk::Invited<October2026>);

#[uniffi::export]
impl October2026InvitedApi {
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

    /// `spoolframe:october-2026`: next episodes and Show next; `None` when the feature is off for
    /// this connection (an older server, or one without it).
    pub fn october_2026(&self) -> Option<Arc<October2026Api>> {
        self.session
            .feature::<October2026>()
            .ok()
            .map(|on| Arc::new(October2026Api(on)))
    }

    /// `spoolframe:download-addons`: subtitles and audio tracks added to a
    /// download; `None` when the feature is off for this connection.
    pub fn download_addons(&self) -> Option<Arc<DownloadAddonsApi>> {
        self.session
            .feature::<DownloadAddons>()
            .ok()
            .map(|on| Arc::new(DownloadAddonsApi(on)))
    }
}

#[uniffi::export]
impl SpoolClient {
    /// Making an account with invite `link`'s code and an app passkey:
    /// its server is reached, and `FeatureOff` when it does not have the
    /// feature (offer a password instead).
    pub async fn invited_october_2026(
        &self,
        link: String,
    ) -> Result<Arc<October2026InvitedApi>, SpoolError> {
        let client = self.client.clone();
        let invited = run(async move { client.invited::<October2026>(&link).await }).await?;
        Ok(Arc::new(October2026InvitedApi(invited)))
    }
}
