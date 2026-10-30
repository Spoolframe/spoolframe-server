//! One user signed in on one server: who they are, the catalog, artwork,
//! the profile's PIN, passkeys and devices. Playback is `playback.rs`.

use std::future::Future;

use spoolframe_client_sdk::types::serde_time::unix_seconds;
use spoolframe_client_sdk::types::{
    ArtworkId, BitmapPreference, ForcedPreference, Item, ItemFilter, ItemSort, Library, LibraryId,
    MediaId, MediaTracks, PersonId, Role, SdhPreference, SearchKind, SearchScope, TrackPreferences,
};
use spoolframe_client_sdk::{
    self as sdk, ArtworkSize, ConnectionPath, DeviceInfo, DeviceSignInRequest, Experimental,
    FinishPasskeyRegistration, FinishPasskeySignIn, HomeScreen, ItemDetail, ItemsPage,
    PasskeyCreation, PasskeyInfo, PasskeyRequest, PinState, PlexLink, Previews,
    TrackPreferencesInfo,
};

use crate::error::SpoolError;
use crate::runtime::run;
use crate::values::{FilterValues, SearchResults};

/// Who a session is.
#[derive(Debug, Clone, uniffi::Record)]
pub struct User {
    pub user_id: String,
    pub username: String,
    /// What to show; the username when there's no display name.
    pub display_name: String,
    pub role: Role,
    /// Needs its PIN on this device before anything else.
    pub locked: bool,
    /// Signed in as a portable identity with no account here (a guest):
    /// account things (PIN, passkeys, password, devices, ties, approving
    /// sign-ins) don't apply.
    pub guest: bool,
    /// The user has a PIN, and its digits when the server kept them.
    pub pin: Option<PinState>,
}

#[uniffi::export]
impl User {
    /// Manages other users and the server.
    pub fn is_admin(&self) -> bool {
        self.role == Role::Admin
    }

    /// The user has a PIN.
    pub fn has_pin(&self) -> bool {
        self.pin.is_some()
    }

    /// How many boxes the PIN pad shows; `None` with no PIN, or one
    /// whose length the server didn't keep: a free-length pad then.
    pub fn pin_digits(&self) -> Option<u8> {
        self.pin.as_ref().and_then(|p| p.digits)
    }
}

impl From<&sdk::UserSummary> for User {
    fn from(u: &sdk::UserSummary) -> Self {
        Self {
            user_id: u.user_id.clone(),
            username: u.username.clone(),
            display_name: u.display_name.clone(),
            role: u.role,
            locked: u.locked,
            guest: u.user_id.starts_with("portable:"),
            pin: u.pin,
        }
    }
}

/// One portable identity tied to the account.
#[derive(Debug, Clone, uniffi::Record)]
pub struct AccountTie {
    /// Hex.
    pub id: String,
    pub fingerprint: String,
    pub tied_at: std::time::SystemTime,
    /// When it last signed in; `None` until it has.
    pub last_used_at: Option<std::time::SystemTime>,
}

/// The user's picture subtitle choice.
#[derive(Debug, Clone, uniffi::Record)]
pub struct PictureSubtitles {
    /// Read as text by the server (the default), else shown as pictures.
    pub as_text: bool,
    /// The server reads picture subtitles as text. When it doesn't, the
    /// choice has no effect: show it disabled.
    pub ocr_offered: bool,
}

/// A scanned or opened `spoolframe://approve` link.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ApprovalLink {
    /// The server's key, hex.
    pub server: String,
    /// The code, normalized.
    pub code: String,
}

/// Equal and hashed as the same user on the same server, whichever
/// session object holds them: a profile opened again is the same one.
#[derive(uniffi::Object)]
#[uniffi::export(Eq, Hash)]
pub struct SpoolSession {
    pub(crate) session: sdk::Session,
}

impl PartialEq for SpoolSession {
    fn eq(&self, other: &Self) -> bool {
        self.session.server_id() == other.session.server_id()
            && self.session.user().user_id == other.session.user().user_id
    }
}

impl Eq for SpoolSession {}

impl std::hash::Hash for SpoolSession {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.session.server_id().hash(state);
        self.session.user().user_id.hash(state);
    }
}

/// A session the SDK opened, as the apps hold it: for Rust that drives
/// the FFI's calls itself (the server's tests).
impl From<sdk::Session> for SpoolSession {
    fn from(session: sdk::Session) -> Self {
        Self::new(session)
    }
}

impl SpoolSession {
    pub(crate) fn new(session: sdk::Session) -> Self {
        Self { session }
    }

    /// Run one SDK call on this session, on the core's runtime.
    pub(crate) async fn call<T, F, Fut>(&self, f: F) -> Result<T, SpoolError>
    where
        F: FnOnce(sdk::Session) -> Fut,
        Fut: Future<Output = Result<T, sdk::SdkError>> + Send + 'static,
        T: Send + 'static,
    {
        Ok(run(f(self.session.clone())).await?)
    }
}

#[uniffi::export]
impl SpoolSession {
    /// The server's key.
    pub fn server(&self) -> String {
        self.session.server()
    }

    /// This session's server as `SpoolClient.servers()` lists it; `None`
    /// once forgotten and out of earshot.
    pub async fn server_entry(&self) -> Result<Option<sdk::ServerEntry>, SpoolError> {
        self.call(|s| async move { s.server_entry().await }).await
    }

    /// Where a browser reaches the server's web UI, when it has a public
    /// address. Read it when shown: a reconnect may bring a new one.
    pub fn web_url(&self) -> Option<String> {
        self.session.web_url()
    }

    /// The experimental features the server turned on for this session.
    pub fn experimental(&self) -> Experimental {
        self.session.experimental()
    }

    /// How this device reaches the server now: the LAN or the internet
    /// at an address, a relay, or not known (no connection). Asks nothing
    /// and never dials.
    pub fn connection_path(&self) -> ConnectionPath {
        self.session.connection_path()
    }

    /// How fast the link to the server carried media lately, bits per
    /// second; `None` until enough was read to tell.
    pub fn link_rate(&self) -> Option<u64> {
        self.session.link_rate()
    }

    /// Who this is, as of sign-in or the last `whoami`.
    pub fn user(&self) -> User {
        self.session.user().into()
    }

    pub async fn whoami(&self) -> Result<User, SpoolError> {
        let user = self.call(|s| async move { s.whoami().await }).await?;
        Ok((&user).into())
    }

    /// End this session on the server and forget it here.
    pub async fn sign_out(&self) -> Result<(), SpoolError> {
        self.call(|s| async move { s.sign_out().await }).await
    }

    /// Open a locked profile on this connection.
    pub async fn unlock(&self, pin: String) -> Result<(), SpoolError> {
        self.call(|s| async move { s.unlock(&pin).await }).await
    }

    /// Continue watching, next up, recently added, and paired servers'
    /// rows.
    pub async fn home(&self) -> Result<HomeScreen, SpoolError> {
        self.call(|s| async move { s.home().await }).await
    }

    pub async fn libraries(&self) -> Result<Vec<Library>, SpoolError> {
        self.call(|s| async move { s.libraries().await }).await
    }

    /// `limit` items of `library` from `offset`, in the library's order.
    pub async fn items(
        &self,
        library: LibraryId,
        offset: u32,
        limit: u32,
    ) -> Result<ItemsPage, SpoolError> {
        self.call(|s| async move { s.items(&library, offset, limit).await })
            .await
    }

    /// `items` sorted by `sort` and kept to what `filter` keeps, `total`
    /// counting only those. Either may be `None`: the library's order,
    /// everything. `InvalidInput` for a sort or filter the library
    /// doesn't offer (`item_filter_values` says which it does).
    pub async fn items_sorted(
        &self,
        library: LibraryId,
        offset: u32,
        limit: u32,
        sort: Option<ItemSort>,
        filter: Option<ItemFilter>,
    ) -> Result<ItemsPage, SpoolError> {
        self.call(|s| async move { s.items_sorted(&library, offset, limit, sort, filter).await })
            .await
    }

    /// What `library` sorts and filters by, and the values to offer
    /// (genres, years, ratings, resolutions, ranges).
    pub async fn item_filter_values(&self, library: LibraryId) -> Result<FilterValues, SpoolError> {
        let values = self
            .call(|s| async move { s.item_filter_values(&library).await })
            .await?;
        Ok(values.into())
    }

    /// An item with its children, intro and credits, the playlists
    /// holding it, its heart, and its copies on paired servers.
    pub async fn item(&self, id: MediaId) -> Result<ItemDetail, SpoolError> {
        self.call(|s| async move { s.item(&id).await }).await
    }

    /// Everything a search finds, the top match first, then each kind's
    /// group in the server's order (people left out).
    pub async fn search_items(&self, query: String, limit: u32) -> Result<Vec<Item>, SpoolError> {
        let request = sdk::SearchRequest {
            query,
            limit,
            scope: SearchScope::All,
            kind: None,
            library: None,
        };
        let results = self
            .call(|s| async move { s.search(request).await })
            .await?;
        let top = match results.top {
            Some(sdk::TopResult::Item(item)) => Some(item),
            _ => None,
        };
        Ok(top
            .into_iter()
            .chain(results.groups.into_iter().flat_map(|g| g.items))
            .collect())
    }

    /// A search's results as the search screen shows them: the top
    /// match, a group per kind, the counts per scope, and the year taken
    /// off the end of `query`. `kind` is one group's "See all".
    pub async fn search(
        &self,
        query: String,
        limit: u32,
        scope: SearchScope,
        kind: Option<SearchKind>,
        library: Option<LibraryId>,
    ) -> Result<SearchResults, SpoolError> {
        let request = sdk::SearchRequest {
            query,
            limit,
            scope,
            kind,
            library,
        };
        let results = self
            .call(|s| async move { s.search(request).await })
            .await?;
        Ok(sdk::search_results(results).into())
    }

    /// What a person a search found is in.
    pub async fn person_items(
        &self,
        person: PersonId,
        offset: u32,
        limit: u32,
    ) -> Result<ItemsPage, SpoolError> {
        self.call(|s| async move { s.person_items(&person, offset, limit).await })
            .await
    }

    pub async fn mark_watched(&self, id: MediaId) -> Result<(), SpoolError> {
        self.call(|s| async move { s.mark_watched(&id).await })
            .await
    }

    pub async fn mark_unwatched(&self, id: MediaId) -> Result<(), SpoolError> {
        self.call(|s| async move { s.mark_unwatched(&id).await })
            .await
    }

    /// Take an item off Continue watching without touching its progress.
    pub async fn hide_from_continue(&self, id: MediaId) -> Result<(), SpoolError> {
        self.call(|s| async move { s.hide_from_continue(&id).await })
            .await
    }

    /// A movie's or episode's frame previews for scrubbing: sheets of
    /// tiles read with `artwork` at `Original`. `PreviewsNotReady` while
    /// the server makes them, `NoPreviews` when it won't.
    pub async fn previews(&self, id: MediaId) -> Result<Previews, SpoolError> {
        self.call(|s| async move { s.previews(&id).await }).await
    }

    /// A movie's or episode's audio and subtitle tracks.
    pub async fn tracks(&self, id: MediaId) -> Result<MediaTracks, SpoolError> {
        self.call(|s| async move { s.tracks(&id).await }).await
    }

    /// An image's bytes (JPEG or PNG).
    pub async fn artwork(&self, id: ArtworkId, size: ArtworkSize) -> Result<Vec<u8>, SpoolError> {
        self.call(|s| async move { s.artwork(&id, size).await })
            .await
    }

    /// How this user wants picture subtitles (PGS, VobSub, DVB) shown, on
    /// every device, and whether the server reads them as text at all.
    pub async fn picture_subtitles(&self) -> Result<PictureSubtitles, SpoolError> {
        let info = self
            .call(|s| async move { s.track_preferences().await })
            .await?;
        Ok(PictureSubtitles {
            as_text: info.preferences.subtitles.bitmap == BitmapPreference::Text,
            ocr_offered: info.ocr_offered,
        })
    }

    /// Read them as text (`as_text`) or show them as pictures, the
    /// other track preferences as they are. A read and a whole write, as
    /// `set_subtitle_search`: a change made elsewhere in between is lost.
    pub async fn set_picture_subtitles(&self, as_text: bool) -> Result<(), SpoolError> {
        let bitmap = if as_text {
            BitmapPreference::Text
        } else {
            BitmapPreference::Pictures
        };
        self.call(move |s| async move { s.set_picture_subtitles(bitmap).await })
            .await
    }

    /// How this user wants tracks chosen, on every device.
    pub async fn track_preferences(&self) -> Result<TrackPreferencesInfo, SpoolError> {
        self.call(|s| async move { s.track_preferences().await })
            .await
    }

    /// Keep `preferences` as this user's.
    pub async fn set_track_preferences(
        &self,
        preferences: TrackPreferences,
    ) -> Result<(), SpoolError> {
        self.call(move |s| async move { s.set_track_preferences(&preferences).await })
            .await
    }

    /// Keep `sdh` and `forced` as the subtitle search's preferences,
    /// every other track preference as it is. It reads them all and
    /// writes them back, as `set_track_preferences` does: a change
    /// another device makes to another preference in between is lost,
    /// and a value this build does not know is written back as its
    /// fallback (an unknown subtitle mode as forced only).
    pub async fn set_subtitle_search(
        &self,
        sdh: SdhPreference,
        forced: ForcedPreference,
    ) -> Result<(), SpoolError> {
        self.call(move |s| async move { s.set_subtitle_search(sdh, forced).await })
            .await
    }

    /// The devices this user signs in from.
    pub async fn devices(&self) -> Result<Vec<DeviceInfo>, SpoolError> {
        self.call(|s| async move { s.devices().await }).await
    }

    /// Ask for the PIN on `device` (an endpoint id; this device for
    /// `None`), or stop asking.
    pub async fn set_pin_required(
        &self,
        device: Option<String>,
        required: bool,
    ) -> Result<(), SpoolError> {
        self.call(move |s| async move { s.set_pin_required(device.as_deref(), required).await })
            .await
    }

    /// Set the PIN (4 to 8 digits). Changing one proves it with the
    /// current PIN (`proof_pin`) or the password (`proof_password`).
    pub async fn set_pin(
        &self,
        pin: String,
        proof_pin: Option<String>,
        proof_password: Option<String>,
    ) -> Result<(), SpoolError> {
        let proof = pin_proof(proof_pin, proof_password);
        self.call(move |s| async move { s.set_pin(&pin, proof).await })
            .await
    }

    /// Remove the PIN, proved with the current PIN or the password.
    pub async fn clear_pin(
        &self,
        proof_pin: Option<String>,
        proof_password: Option<String>,
    ) -> Result<(), SpoolError> {
        let Some(proof) = pin_proof(proof_pin, proof_password) else {
            return Err(SpoolError::InvalidInput {
                message: "the current PIN or the password".into(),
            });
        };
        self.call(move |s| async move { s.clear_pin(proof).await })
            .await
    }

    /// This user's Plex link, or `None` when not linked. Linking is the
    /// web's (`<web_url>/profile`).
    pub async fn plex_link(&self) -> Result<Option<PlexLink>, SpoolError> {
        self.call(|s| async move { s.plex_link().await }).await
    }

    /// Leave this server: a guest's grant is revoked and their history
    /// deleted; an account is disabled ("left", an admin can restore it).
    /// On success this device forgets this profile; it keeps the server,
    /// and everyone else's profiles on it, until the last profile here
    /// leaves. `RecentSignInRequired`: confirm and call again. `LastAdmin`:
    /// make someone else an admin first.
    pub async fn leave_server(&self) -> Result<(), SpoolError> {
        self.call(|s| async move { s.leave_server().await }).await
    }

    /// False only for the last enabled admin, as of sign-in, resume or
    /// profiles (no request). An older server reads as true; `LastAdmin`
    /// from `leave_server` is the backstop.
    pub fn can_leave(&self) -> bool {
        self.session.can_leave()
    }

    /// `can_leave`, asking the server again.
    pub async fn can_leave_now(&self) -> Result<bool, SpoolError> {
        self.call(|s| async move { s.can_leave_now().await }).await
    }

    /// Signed in with a one-time password an admin set: a new one must be
    /// chosen (`change_password`) before anything else.
    pub fn password_change_required(&self) -> bool {
        self.session.password_change_required()
    }

    /// Change this user's password: `current` (a one-time password too)
    /// for `new`. `InvalidCredentials` for a wrong current one;
    /// `InvalidInput` for one too short or the same.
    pub async fn change_password(&self, current: String, new: String) -> Result<(), SpoolError> {
        self.call(|s| async move { s.change_password(&current, &new).await })
            .await
    }

    /// Confirm it's this session's user with their password: a change
    /// that needs a recent sign-in then goes through for ten minutes.
    /// `NoPassword` when the account has none.
    pub async fn confirm_password(&self, password: String) -> Result<(), SpoolError> {
        self.call(|s| async move { s.confirm_password(&password).await })
            .await
    }

    /// Begin confirming with one of the user's app passkeys; `NotFound`
    /// when they have none, so ask for the password.
    pub async fn begin_confirm_passkey(&self) -> Result<PasskeyRequest, SpoolError> {
        self.call(|s| async move { s.begin_confirm_passkey().await })
            .await
    }

    /// The passkey's answer to `begin_confirm_passkey`.
    pub async fn finish_confirm_passkey(
        &self,
        assertion: FinishPasskeySignIn,
    ) -> Result<(), SpoolError> {
        self.call(|s| async move { s.finish_confirm_passkey(assertion).await })
            .await
    }

    /// Begin making an app passkey for this user. `RecentSignInRequired`
    /// when the sign-in is older than ten minutes.
    pub async fn begin_passkey_registration(&self) -> Result<PasskeyCreation, SpoolError> {
        self.call(|s| async move { s.begin_passkey_registration().await })
            .await
    }

    /// Keep the passkey the platform made.
    pub async fn finish_passkey_registration(
        &self,
        made: FinishPasskeyRegistration,
    ) -> Result<(), SpoolError> {
        self.call(|s| async move { s.finish_passkey_registration(made).await })
            .await
    }

    /// This user's passkeys, the web UI's too, oldest first.
    pub async fn passkeys(&self) -> Result<Vec<PasskeyInfo>, SpoolError> {
        self.call(|s| async move { s.passkeys().await }).await
    }

    /// Rename a passkey: 1 to 64 characters.
    pub async fn rename_passkey(&self, id: String, name: String) -> Result<(), SpoolError> {
        self.call(|s| async move { s.rename_passkey(&id, &name).await })
            .await
    }

    /// Remove a passkey. `RecentSignInRequired` after ten minutes;
    /// `LastSignInMethod` when it's the account's last way in.
    pub async fn remove_passkey(&self, id: String) -> Result<(), SpoolError> {
        self.call(|s| async move { s.remove_passkey(&id).await })
            .await
    }

    /// Tie this account to portable identity `identity` (hex), proved
    /// with its root here; an account ties several, and the same one
    /// again is fine. `RecentSignInRequired`: confirm, then again;
    /// `PortableIdTaken`, `TieChallengeExpired`.
    pub async fn tie(&self, identity: String) -> Result<(), SpoolError> {
        let id = crate::portable::identity(&identity)?;
        self.call(|s| async move { s.tie(id).await }).await
    }

    /// Sever this account's tie to `identity`; the others, the account
    /// and its sessions stay as they are. Needs a recent sign-in.
    pub async fn untie(&self, identity: String) -> Result<(), SpoolError> {
        let id = crate::portable::identity(&identity)?;
        self.call(move |s| async move { s.untie(id).await }).await
    }

    /// The portable identities this account is tied to, as the server
    /// says now, oldest first.
    pub async fn tie_info(&self) -> Result<Vec<AccountTie>, SpoolError> {
        Ok(self
            .call(|s| async move { s.tie_info().await })
            .await?
            .into_iter()
            .map(|t| AccountTie {
                fingerprint: t.id.fingerprint(),
                id: t.id.to_string(),
                tied_at: unix_seconds::from_wire(t.tied_at),
                last_used_at: t.last_used_at.map(unix_seconds::from_wire),
            })
            .collect())
    }

    /// Who's asking with `code` (as typed or scanned), to ask "Sign in
    /// 'Living Room' as Alice?" before approving. `platform_label` names
    /// its platform; its address's network says whether it's nearby.
    pub async fn lookup_device_sign_in(
        &self,
        code: String,
    ) -> Result<DeviceSignInRequest, SpoolError> {
        self.call(|s| async move { s.lookup_device_sign_in(&code).await })
            .await
    }

    /// Sign that device in as this session's user; `remember` trusts it
    /// if it asked to be.
    pub async fn approve_device_sign_in(
        &self,
        code: String,
        remember: bool,
    ) -> Result<(), SpoolError> {
        self.call(|s| async move { s.approve_device_sign_in(&code, remember).await })
            .await
    }

    pub async fn deny_device_sign_in(&self, code: String) -> Result<(), SpoolError> {
        self.call(|s| async move { s.deny_device_sign_in(&code).await })
            .await
    }

    /// Trust this device for the user, or stop, and whether it opens
    /// their profile at start (needs trust; turns the PIN off here and
    /// another profile's auto-login off). Turning either on needs a recent
    /// sign-in. Turning trust off signs this session out.
    pub async fn set_device_trust(
        &self,
        trusted: bool,
        auto_login: bool,
    ) -> Result<(), SpoolError> {
        self.call(|s| async move { s.set_device_trust(None, trusted, auto_login).await })
            .await
    }

    /// Delete another of the user's devices (an endpoint id): its
    /// sessions, trust and row go, and it's signed out. Needs a recent
    /// sign-in; this device is `InvalidInput` (sign out instead), an
    /// unknown one `NotFound`.
    pub async fn forget_device(&self, device: String) -> Result<(), SpoolError> {
        self.call(|s| async move { s.forget_device(&device).await })
            .await
    }

    /// Whether the sign-in that made this session left the device
    /// trusted: `None` for a session found again later.
    pub fn remembered(&self) -> Option<bool> {
        self.session.remembered()
    }
}

/// The server and code of a `spoolframe://approve?…` link; `None` if it
/// isn't one.
#[uniffi::export]
pub fn parse_approval_link(text: String) -> Option<ApprovalLink> {
    sdk::device_sign_in::parse_link(&text).map(|l| ApprovalLink {
        server: l.server,
        code: l.code,
    })
}

/// A typed code, whatever its case, dashes or spaces (`BCDFGHJK`); `None`
/// if it can't be one.
#[uniffi::export]
pub fn normalize_sign_in_code(input: String) -> Option<String> {
    sdk::device_sign_in::normalize(&input)
}

/// What proves a PIN change: the current PIN, else the password.
fn pin_proof(pin: Option<String>, password: Option<String>) -> Option<sdk::PinProof> {
    match (pin, password) {
        (Some(pin), _) => Some(sdk::PinProof::Pin { pin }),
        (None, Some(password)) => Some(sdk::PinProof::Password { password }),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spoolframe_client_sdk::PinState;

    fn user(role: Role, pin: Option<PinState>) -> User {
        User {
            user_id: "u1".into(),
            username: "ana".into(),
            display_name: "Ana".into(),
            role,
            locked: false,
            guest: false,
            pin,
        }
    }

    #[test]
    fn a_user_says_their_role_and_pin() {
        let read = |u: User| (u.is_admin(), u.has_pin(), u.pin_digits());
        assert_eq!(
            [
                read(user(Role::Admin, Some(PinState { digits: Some(6) }))),
                read(user(Role::Consumer, Some(PinState { digits: None }))),
                read(user(Role::Unknown, None)),
            ],
            [
                (true, true, Some(6)),
                (false, true, None),
                (false, false, None)
            ]
        );
    }
}
