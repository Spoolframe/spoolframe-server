//! Profiles and signing in, as the SDK decides them: "Who's watching" and
//! where the app opens, the PIN and password rules, links, invites by
//! code, confirming it's you, WebAuthn's JSON, each profile's saved
//! values, and an identity's overview.

use std::sync::Arc;

use spoolframe_client_sdk::{
    self as sdk, AddressClass, AppLink, DeviceAddress, ErrorKind, FinishPasskeyRegistration,
    FinishPasskeySignIn, InviteInfo, InviteLookup, InviteNext, InviteRefusal, LastProfile,
    LaunchTarget, PasskeyCreation, PasskeyRequest, PasswordProblem, PinFailure, PinRefusal,
    RootLocation, SignInMethod,
};

use crate::client::SpoolClient;
use crate::error::SpoolError;
use crate::portable::{IssuedDevice, PortableIdentity, SpoolPortable, identity};
use crate::runtime::run;
use crate::security_key::{IdentityRoot, root_of};
use crate::session::SpoolSession;

/// How a profile on "Who's watching" opens.
#[derive(uniffi::Enum)]
pub enum ProfileOpens {
    /// Signed in now: open this session.
    Session { session: Arc<SpoolSession> },
    /// `resume_profile` with `user_id`, no password.
    Trusted { user_id: String },
    /// `sign_in_portable` with `identity` (hex), nothing asked.
    Portable { identity: String },
    /// Sign in again, the way they last did.
    SignIn { method: Option<SignInMethod> },
}

/// One profile on "Who's watching".
#[derive(uniffi::Record)]
pub struct WatchingProfile {
    pub username: String,
    pub display_name: String,
    pub opens: ProfileOpens,
    pub locked: bool,
    /// How many digits its PIN has, when the server says.
    pub pin_digits: Option<u8>,
    pub auto_login: bool,
    pub last: bool,
}

/// "Who's watching" on one server, and where the app goes.
#[derive(uniffi::Record)]
pub struct WhosWatching {
    pub profiles: Vec<WatchingProfile>,
    /// For `open_last` as asked.
    pub launch: LaunchTarget,
}

/// A server a paired guest could not be signed in to.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ServerFailure {
    pub server: String,
    pub kind: ErrorKind,
    pub message: String,
}

/// What `sign_in_paired` did.
#[derive(uniffi::Record)]
pub struct PairedSignIn {
    pub sessions: Vec<Arc<SpoolSession>>,
    pub failed: Vec<ServerFailure>,
}

/// How "Confirm it's you" confirms the user.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum Reauth {
    /// Sign `request` with an app passkey, then `finish_confirm_passkey`.
    Passkey { request: PasskeyRequest },
    /// No app passkey: `confirm_password`.
    Password,
}

#[uniffi::export]
impl SpoolClient {
    /// "Who's watching" on `endpoint_id`, merged, with where to go: at
    /// start (`open_last`), or when the user went back to pick.
    pub async fn whos_watching(
        &self,
        endpoint_id: String,
        open_last: bool,
    ) -> Result<WhosWatching, SpoolError> {
        let client = self.client.clone();
        let watching = run(async move { client.whos_watching(&endpoint_id).await }).await?;
        let launch = watching.launch_target(open_last);
        Ok(WhosWatching {
            launch,
            profiles: watching
                .profiles
                .into_iter()
                .map(|p| WatchingProfile {
                    username: p.username,
                    display_name: p.display_name,
                    opens: match p.opens {
                        sdk::ProfileOpens::Session(s) => ProfileOpens::Session {
                            session: Arc::new(SpoolSession::new(s)),
                        },
                        sdk::ProfileOpens::Trusted { user_id } => ProfileOpens::Trusted { user_id },
                        sdk::ProfileOpens::Portable { identity } => ProfileOpens::Portable {
                            identity: identity.to_string(),
                        },
                        sdk::ProfileOpens::SignIn { method } => ProfileOpens::SignIn { method },
                    },
                    locked: p.locked,
                    pin_digits: p.pin_digits,
                    auto_login: p.auto_login,
                    last: p.last,
                })
                .collect(),
        })
    }

    /// Keep `username` on `endpoint_id` as the profile browsed last.
    pub async fn remember_last(
        &self,
        endpoint_id: String,
        username: String,
    ) -> Result<(), SpoolError> {
        let client = self.client.clone();
        Ok(run(async move { client.remember_last(&endpoint_id, &username).await }).await?)
    }

    /// The profile browsed last, on any server.
    pub async fn last_profile(&self) -> Result<Option<LastProfile>, SpoolError> {
        let client = self.client.clone();
        Ok(run(async move { client.last_profile().await }).await?)
    }

    /// Forget the last profile, if it is `username` on `endpoint_id`.
    pub async fn forget_last(
        &self,
        endpoint_id: String,
        username: String,
    ) -> Result<(), SpoolError> {
        let client = self.client.clone();
        Ok(run(async move { client.forget_last(&endpoint_id, &username).await }).await?)
    }

    /// Sign in as `identity` (hex) on each of `servers`; one failing never
    /// stops the rest.
    pub async fn sign_in_paired(
        &self,
        identity: String,
        servers: Vec<String>,
    ) -> Result<PairedSignIn, SpoolError> {
        let identity = crate::portable::identity(&identity)?;
        let client = self.client.clone();
        let signed = run(async move {
            Ok::<_, sdk::SdkError>(client.sign_in_paired(identity, &servers).await)
        })
        .await?;
        Ok(PairedSignIn {
            sessions: signed
                .sessions
                .into_iter()
                .map(|s| Arc::new(SpoolSession::new(s)))
                .collect(),
            failed: signed
                .failed
                .into_iter()
                .map(|(server, error)| {
                    let error = SpoolError::from(error);
                    ServerFailure {
                        server,
                        kind: error.kind(),
                        message: error.to_string(),
                    }
                })
                .collect(),
        })
    }

    /// Join `endpoint_id` with a code typed in: looked up, then redeemed.
    pub async fn redeem_invite_code(
        &self,
        endpoint_id: String,
        addrs: Vec<String>,
        typed: String,
        username: String,
        display_name: Option<String>,
        password: String,
    ) -> Result<Arc<SpoolSession>, SpoolError> {
        let addrs = crate::client::addrs(&addrs)?;
        let client = self.client.clone();
        let session = run(async move {
            client
                .redeem_invite_code(
                    &endpoint_id,
                    addrs,
                    &typed,
                    &username,
                    display_name.as_deref(),
                    &password,
                )
                .await
        })
        .await?;
        Ok(Arc::new(SpoolSession::new(session)))
    }

    /// What the app keeps for `username` on `endpoint_id`.
    pub fn profile_values(&self, endpoint_id: String, username: String) -> Arc<ProfileValues> {
        Arc::new(ProfileValues(
            self.client.profile_store(&endpoint_id, &username),
        ))
    }
}

#[uniffi::export]
impl SpoolSession {
    /// Unlock with the PIN that opened this profile's downloads offline;
    /// `false` with none held.
    pub async fn unlock_as_offline(&self) -> Result<bool, SpoolError> {
        self.call(|s| async move { s.unlock_as_offline().await })
            .await
    }

    /// That PIN is held: no PIN pad needed.
    pub fn unlocked_offline(&self) -> bool {
        self.session.unlocked_offline()
    }

    /// How "Confirm it's you" confirms this user.
    pub async fn reauth_options(&self) -> Result<Reauth, SpoolError> {
        let reauth = self
            .call(|s| async move { s.reauth_options().await })
            .await?;
        Ok(match reauth {
            sdk::Reauth::Passkey { request } => Reauth::Passkey { request },
            sdk::Reauth::Password => Reauth::Password,
        })
    }

    /// The portable identity this user is (hex); none for an account.
    pub fn portable_identity(&self) -> Option<String> {
        self.session.user().portable_id().map(|id| id.to_string())
    }
}

/// What the app keeps for one profile, by name: lowercase letters,
/// digits, `-` and `_`.
#[derive(uniffi::Object)]
pub struct ProfileValues(sdk::ProfileStore);

#[uniffi::export]
impl ProfileValues {
    pub async fn get(&self, name: String) -> Result<Option<Vec<u8>>, SpoolError> {
        let store = self.0.clone();
        Ok(run(async move { store.get(&name).await }).await?)
    }

    /// `None` removes it.
    pub async fn set(&self, name: String, value: Option<Vec<u8>>) -> Result<(), SpoolError> {
        let store = self.0.clone();
        Ok(run(async move { store.set(&name, value).await }).await?)
    }
}

#[uniffi::export]
impl SpoolError {
    /// What a PIN pad says after this error from a PIN's check; none for
    /// one that is not about the PIN.
    pub fn pin_failure(&self) -> Option<PinFailure> {
        PinFailure::of(&self.classified()?)
    }

    /// What this error from redeeming an invite says of its code.
    pub fn invite_refusal(&self) -> Option<InviteRefusal> {
        InviteRefusal::of_error(&self.classified()?)
    }
}

/// Why `typed` is no PIN; none when it is one.
#[uniffi::export]
pub fn pin_refusal(typed: String) -> Option<PinRefusal> {
    sdk::Pin::new(&typed).err()
}

/// The fewest and most digits a PIN has: a PIN pad's length.
#[uniffi::export]
pub fn pin_digits_range() -> Vec<u8> {
    vec![sdk::PIN_MIN_DIGITS, sdk::PIN_MAX_DIGITS]
}

/// The fewest characters a new password has.
#[uniffi::export]
pub fn password_min_chars() -> u32 {
    sdk::PASSWORD_MIN
}

/// Why `typed` cannot be a new password; none when it can.
#[uniffi::export]
pub fn password_problem(typed: String) -> Option<PasswordProblem> {
    sdk::password_problem(&typed)
}

/// How an approval shows where a device asked from.
#[uniffi::export]
pub fn address_class(from: DeviceAddress) -> AddressClass {
    sdk::address_class(&from)
}

/// What a link opens; none for no link the app takes.
#[uniffi::export]
pub fn parse_app_link(text: String) -> Option<AppLink> {
    sdk::parse_app_link(&text)
}

#[uniffi::export]
pub fn is_invite_link(text: String) -> bool {
    sdk::is_invite_link(&text)
}

/// The https form of invite `link`, to send as text.
#[uniffi::export]
pub fn invite_share_link(link: String) -> Option<String> {
    sdk::invite_share_link(&link)
}

/// What an invite's check leads to.
#[uniffi::export]
pub fn invite_next(
    info: InviteInfo,
    lookup: Option<InviteLookup>,
    identities_here: bool,
) -> InviteNext {
    sdk::invite_next(&info, lookup.as_ref(), identities_here)
}

/// The hash a profile's values are kept under: the Linux app's.
#[uniffi::export]
pub fn profile_hash(server: String, username: String) -> String {
    sdk::profile_hash(&server, &username)
}

/// `request` as WebAuthn's request options JSON.
#[uniffi::export]
pub fn passkey_request_json(request: PasskeyRequest) -> String {
    sdk::webauthn::request_options(&request)
}

/// `creation` as WebAuthn's creation options JSON.
#[uniffi::export]
pub fn passkey_creation_json(creation: PasskeyCreation) -> String {
    sdk::webauthn::creation_options(&creation)
}

/// A platform's JSON answer to a passkey request, to finish with.
#[uniffi::export]
pub fn passkey_assertion_from_json(json: String) -> Result<FinishPasskeySignIn, SpoolError> {
    Ok(sdk::webauthn::assertion(&json)?)
}

/// A platform's JSON answer to a passkey creation, to keep as `name`.
#[uniffi::export]
pub fn passkey_registration_from_json(
    json: String,
    name: String,
) -> Result<FinishPasskeyRegistration, SpoolError> {
    Ok(sdk::webauthn::registration(&json, &name)?)
}

/// A device an identity gave a credential to.
#[derive(Debug, Clone, uniffi::Record)]
pub struct IdentityDevice {
    pub device: IssuedDevice,
    pub this_device: bool,
}

/// What can be done with an identity here.
#[derive(Debug, Clone, Copy, uniffi::Record)]
pub struct IdentityAbilities {
    pub sign: bool,
    pub sync: bool,
    pub join: bool,
}

/// An identity's settings page.
#[derive(Debug, Clone, uniffi::Record)]
pub struct IdentityOverview {
    pub identity: PortableIdentity,
    pub roots: Vec<IdentityRoot>,
    pub devices: Vec<IdentityDevice>,
    pub location: RootLocation,
    /// Unix seconds.
    pub renewal_due: Option<i64>,
    pub abilities: IdentityAbilities,
}

#[uniffi::export]
impl SpoolPortable {
    /// Identity `id`'s settings page, from this device alone.
    pub async fn overview(&self, id: String) -> Result<IdentityOverview, SpoolError> {
        let id = identity(&id)?;
        let portable = self.0.clone();
        let o = run(async move { portable.overview(id).await }).await?;
        Ok(IdentityOverview {
            identity: o.identity.into(),
            roots: o.roots.iter().map(root_of).collect(),
            devices: o
                .devices
                .into_iter()
                .map(|d| IdentityDevice {
                    this_device: d.this_device,
                    device: d.credential.into(),
                })
                .collect(),
            location: o.location,
            renewal_due: o.renewal_due,
            abilities: IdentityAbilities {
                sign: o.abilities.sign,
                sync: o.abilities.sync,
                join: o.abilities.join,
            },
        })
    }
}
