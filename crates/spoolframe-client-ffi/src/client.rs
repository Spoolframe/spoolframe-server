//! This device: its saved servers, the ones heard on the LAN, and signing
//! in.

use std::net::SocketAddr;
use std::sync::Arc;

use spoolframe_client_sdk::{
    self as sdk, ConnectionState, Experimental, FinishPasskeySignIn, InviteInfo, InviteLookup,
    LanServerInfo, PasskeyRequest, Reachability, ServerEntry, ServerEvent, TrustedProfile,
    types::Platform,
};
use tokio::sync::Mutex;

use crate::error::{SpoolError, invalid};
use crate::pairing::{SpoolPairing, SpoolPairingOffer};
use crate::portable::SpoolPortable;
use crate::runtime::run;
use crate::session::SpoolSession;
use crate::storage::{Bridge, CoreStorage};
use crate::values::{DeviceCapabilities, ScreenCapabilities};

/// What the app says about this device.
#[derive(Debug, Clone, uniffi::Record)]
pub struct DeviceSpec {
    /// What the device calls itself ("Andrey's iPhone"); shown in the
    /// server's session lists.
    pub name: String,
    pub client_name: String,
    pub client_version: String,
    /// What kind of client this is: `Ios`, `Tvos`, `Macos`, `Android`.
    pub platform: Platform,
    /// What this device's player can play.
    pub capabilities: DeviceCapabilities,
    /// Reach servers through the relays they name when they can't be
    /// dialled directly. Fixed until the app next starts.
    pub relays: bool,
    /// What experimental features to ask servers for: playbacks over
    /// their own HTTP port too, for AirPlay (`http_playback`), and paired
    /// servers' libraries (`federation`).
    pub experimental: Experimental,
    /// Where downloads are kept: a directory excluded from backup. `None`
    /// downloads nothing (tvOS).
    #[uniffi(default = None)]
    pub downloads_dir: Option<String>,
    /// Keep up to this many catalog reads (Home, the libraries, their
    /// pages, items' details, the playlists) in memory, answered at once
    /// and read again behind the screen once old; `SpoolSession`'s
    /// `watch_catalog` says which came back different. `None`: off, every
    /// read asks the server. `catalog_cache_capacity()` is the SDK's
    /// suggestion.
    #[uniffi(default = None)]
    pub catalog_cache: Option<u32>,
}

/// The protocol version this core speaks, for Settings › About beside
/// the server's.
#[uniffi::export]
pub fn protocol_version() -> u32 {
    spoolframe_protocol::PROTOCOL_VERSION
}

/// How many catalog reads the SDK suggests keeping
/// (`DeviceSpec.catalog_cache`).
#[uniffi::export]
pub fn catalog_cache_capacity() -> u32 {
    crate::values::count(sdk::CacheConfig::default().capacity)
}

/// The SDK's options for `device`.
fn options_of(device: &DeviceSpec) -> sdk::ClientOptions {
    sdk::ClientOptions {
        relays: device.relays,
        experimental: device.experimental,
        downloads_dir: device.downloads_dir.as_ref().map(std::path::PathBuf::from),
        catalog_cache: device.catalog_cache.map(|capacity| sdk::CacheConfig {
            capacity: capacity as usize,
        }),
        ..sdk::ClientOptions::default()
    }
}

/// A profile that signs in as a portable identity.
#[derive(Debug, Clone, uniffi::Record)]
pub struct PortableProfile {
    pub username: String,
    pub display_name: String,
    /// Hex.
    pub identity: String,
    pub identity_fingerprint: String,
}

/// A profile kept on this device for a server, however it signs in.
#[derive(Debug, Clone, uniffi::Record)]
pub struct SavedProfileName {
    pub username: String,
    pub display_name: String,
}

/// This device. One per app.
#[derive(uniffi::Object)]
pub struct SpoolClient {
    pub(crate) client: sdk::Client,
}

/// A client the SDK opened, as the apps hold it: for Rust that drives
/// the FFI's calls itself (the server's tests).
impl From<sdk::Client> for SpoolClient {
    fn from(client: sdk::Client) -> Self {
        Self { client }
    }
}

pub(crate) fn addrs(text: &[String]) -> Result<Vec<SocketAddr>, SpoolError> {
    text.iter()
        .map(|a| a.parse::<SocketAddr>().map_err(|e| invalid(a, e)))
        .collect()
}

fn session(session: sdk::Session) -> Arc<SpoolSession> {
    Arc::new(SpoolSession::new(session))
}

#[uniffi::export]
impl SpoolClient {
    /// Open this device: its key from `storage` (made on first start), and
    /// an endpoint that listens for servers on the LAN without announcing
    /// itself.
    #[uniffi::constructor]
    pub async fn open(
        storage: Arc<dyn CoreStorage>,
        device: DeviceSpec,
    ) -> Result<Arc<Self>, SpoolError> {
        // `SPOOLFRAME_RUST_LOG=<filter>` (e.g. `spoolframe_client_sdk=debug`):
        // the core's own log to stderr, for debugging. Nothing in a shipped
        // app sets it.
        if let Ok(filter) = std::env::var("SPOOLFRAME_RUST_LOG") {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
                .with_writer(std::io::stderr)
                .try_init();
        }
        let options = options_of(&device);
        let device = sdk::Device {
            name: device.name,
            client_name: device.client_name,
            client_version: device.client_version,
            platform: device.platform,
            capabilities: device.capabilities.into(),
        };
        let storage: Arc<dyn sdk::Storage> = Arc::new(Bridge(storage));
        let client = run(sdk::Client::open_with_options(storage, device, options)).await?;
        Ok(Arc::new(Self { client }))
    }

    /// Show a pairing code as this device, called `name`: only with
    /// guests allowed (`Pairing { NotAllowed }`), one at a time.
    pub async fn start_pairing(&self, name: String) -> Result<Arc<SpoolPairing>, SpoolError> {
        let client = self.client.clone();
        let pairing = run(async move { client.start_pairing(&name).await }).await?;
        Ok(Arc::new(SpoolPairing(Arc::new(pairing))))
    }

    /// Reach the device whose code `link` is, to authorize it for
    /// `identity` (hex), whose root this phone holds.
    pub async fn scan_pairing(
        &self,
        link: String,
        identity: String,
    ) -> Result<Arc<SpoolPairingOffer>, SpoolError> {
        let identity = crate::portable::identity(&identity)?;
        let client = self.client.clone();
        let offer = run(async move { client.scan_pairing(&link, identity).await }).await?;
        Ok(Arc::new(SpoolPairingOffer(Arc::new(offer))))
    }

    /// The portable identities on this device.
    pub fn portable(&self) -> Arc<SpoolPortable> {
        Arc::new(SpoolPortable(self.client.portable()))
    }

    /// The screen as it is now: its mode (height, frame rate), HDR and bit
    /// depth. Say it once after `open` and again whenever it changes; every
    /// plan, open and re-plan that names no `display` of its own sends it,
    /// so a mode switch counts from the next plan. `None` forgets it.
    /// `DeviceSpec.capabilities` stays the decoders' limits.
    pub fn set_display(&self, display: Option<ScreenCapabilities>) {
        self.client.set_display(display.map(Into::into));
    }

    /// This device's key, hex.
    pub fn id(&self) -> String {
        self.client.id()
    }

    /// Saved servers first, then ones heard on the LAN, each by name.
    pub async fn servers(&self) -> Result<Vec<ServerEntry>, SpoolError> {
        let client = self.client.clone();
        Ok(run(async move { client.servers().await }).await?)
    }

    /// Server `endpoint_id` as `servers()` lists it; `None` for one
    /// neither saved nor heard on the LAN.
    pub async fn server(&self, endpoint_id: String) -> Result<Option<ServerEntry>, SpoolError> {
        let client = self.client.clone();
        Ok(run(async move { client.server(&endpoint_id).await }).await?)
    }

    /// The servers heard on the LAN now, saved or not, by name.
    pub async fn servers_on_network(&self) -> Result<Vec<ServerEntry>, SpoolError> {
        let client = self.client.clone();
        Ok(run(async move { client.servers_on_network().await }).await?)
    }

    /// `servers()` again on each change to it.
    pub fn watch_servers(&self) -> Arc<ServerWatch> {
        Arc::new(ServerWatch {
            client: self.client.downgrade(),
            changes: Arc::new(Mutex::new(self.client.watch_servers())),
        })
    }

    /// Server `endpoint_id`'s connection state, sign-outs and a needed
    /// update, as one stream: what a connection banner shows.
    pub fn watch_server(&self, endpoint_id: String) -> Result<Arc<ServerEventWatch>, SpoolError> {
        let events = self.client.server_events(&endpoint_id)?;
        Ok(Arc::new(ServerEventWatch {
            current: events.current(),
            events: Arc::new(Mutex::new(events)),
        }))
    }

    /// Addresses to dial a server at, beside what the LAN says
    /// ("host:port").
    pub fn add_address(&self, endpoint_id: String, addrs: Vec<String>) -> Result<(), SpoolError> {
        let addrs = self::addrs(&addrs)?;
        Ok(self.client.add_address(&endpoint_id, addrs)?)
    }

    /// Reach a server and hear who it is (before it's pinned, only
    /// `Hello`).
    pub async fn connect(&self, endpoint_id: String) -> Result<ServerEntry, SpoolError> {
        let client = self.client.clone();
        Ok(run(async move { client.connect(&endpoint_id).await }).await?)
    }

    /// Save a server with its key pinned, `verified` if the user confirmed
    /// its fingerprint.
    pub async fn pin(
        &self,
        endpoint_id: String,
        verified: bool,
    ) -> Result<ServerEntry, SpoolError> {
        let client = self.client.clone();
        Ok(run(async move { client.pin(&endpoint_id, verified).await }).await?)
    }

    /// Replace saved server `saved_id`'s pin with `new_id`, after the user
    /// checked the new fingerprint. Its sessions are forgotten.
    pub async fn trust_new_identity(
        &self,
        saved_id: String,
        new_id: String,
    ) -> Result<ServerEntry, SpoolError> {
        let client = self.client.clone();
        Ok(run(async move { client.trust_new_identity(&saved_id, &new_id).await }).await?)
    }

    /// What this device knows of reaching `endpoint_id`: addresses,
    /// relays, lookups and their results, the last dial, the paths live
    /// now. Never dials. `UnknownServer` for one never seen.
    pub async fn reachability(&self, endpoint_id: String) -> Result<Reachability, SpoolError> {
        let client = self.client.clone();
        Ok(run(async move { client.reachability(&endpoint_id).await }).await?)
    }

    /// Saved server `endpoint_id`'s "Use relays": `None` follows the
    /// device's switch. Its open connection closes; the next request dials
    /// again.
    pub async fn set_server_relays(
        &self,
        endpoint_id: String,
        relays: Option<bool>,
    ) -> Result<(), SpoolError> {
        let client = self.client.clone();
        Ok(run(async move { client.set_server_relays(&endpoint_id, relays).await }).await?)
    }

    /// Saved server `endpoint_id`'s relay URLs from now on. Empty gives
    /// the list back to the server (what it pushes, and an invite's).
    pub async fn set_server_relay_urls(
        &self,
        endpoint_id: String,
        relays: Vec<String>,
    ) -> Result<(), SpoolError> {
        let client = self.client.clone();
        Ok(run(async move { client.set_server_relay_urls(&endpoint_id, relays).await }).await?)
    }

    /// Forget a saved server and its sessions.
    pub async fn forget(&self, endpoint_id: String) -> Result<(), SpoolError> {
        let client = self.client.clone();
        Ok(run(async move { client.forget(&endpoint_id).await }).await?)
    }

    /// Sign in with a password, and keep the session. With `remember`
    /// ("Remember this device") the server trusts this device for the
    /// user: `resume_profile` opens it again without a password once the
    /// session ends, until they sign out here.
    pub async fn sign_in(
        &self,
        endpoint_id: String,
        username: String,
        password: String,
        remember: bool,
    ) -> Result<Arc<SpoolSession>, SpoolError> {
        let client = self.client.clone();
        let signed_in = run(async move {
            client
                .sign_in(&endpoint_id, &username, &password, remember)
                .await
        })
        .await?;
        Ok(session(signed_in))
    }

    /// Begin a passkey sign-in; with a username, only that user's passkeys
    /// may answer.
    pub async fn begin_passkey_sign_in(
        &self,
        endpoint_id: String,
        username: Option<String>,
    ) -> Result<PasskeyRequest, SpoolError> {
        let client = self.client.clone();
        Ok(run(async move {
            client
                .begin_passkey_sign_in(&endpoint_id, username.as_deref())
                .await
        })
        .await?)
    }

    /// The passkey's answer: a session, kept like a password sign-in's;
    /// `remember` as for `sign_in`.
    pub async fn finish_passkey_sign_in(
        &self,
        endpoint_id: String,
        assertion: FinishPasskeySignIn,
        remember: bool,
    ) -> Result<Arc<SpoolSession>, SpoolError> {
        let client = self.client.clone();
        let signed_in = run(async move {
            client
                .finish_passkey_sign_in(&endpoint_id, assertion, remember)
                .await
        })
        .await?;
        Ok(session(signed_in))
    }

    /// Make `profile` the one in front: choosing it relocks the others'
    /// PINs on this connection. Call when a profile is picked, before its
    /// PIN.
    pub async fn activate_profile(&self, profile: Arc<SpoolSession>) -> Result<(), SpoolError> {
        let client = self.client.clone();
        let session = profile.session.clone();
        run(async move { client.activate_profile(&session).await.map(|_| ()) }).await?;
        Ok(())
    }

    /// "Who's watching": the users signed in on a server from this device.
    pub async fn profiles(
        &self,
        endpoint_id: String,
    ) -> Result<Vec<Arc<SpoolSession>>, SpoolError> {
        let client = self.client.clone();
        let profiles = run(async move { client.profiles(&endpoint_id).await }).await?;
        Ok(profiles.into_iter().map(session).collect())
    }

    /// The users this device is trusted for on a server: Who's watching
    /// lists them beside `profiles`, matched by username, once their
    /// sessions have ended. The one with `auto_login` opens at start.
    pub async fn trusted_profiles(
        &self,
        endpoint_id: String,
    ) -> Result<Vec<TrustedProfile>, SpoolError> {
        let client = self.client.clone();
        Ok(run(async move { client.trusted_profiles(&endpoint_id).await }).await?)
    }

    /// Open a trusted profile again without a password, kept like a
    /// sign-in. `SignInRequired` once the device isn't trusted for them
    /// any more: they sign in in full.
    pub async fn resume_profile(
        &self,
        endpoint_id: String,
        user_id: String,
    ) -> Result<Arc<SpoolSession>, SpoolError> {
        let client = self.client.clone();
        let resumed =
            run(async move { client.resume_profile(&endpoint_id, &user_id).await }).await?;
        Ok(session(resumed))
    }

    /// Redeem a guest invite as portable identity `identity` (hex): the
    /// server grants it access, makes no account; this device's
    /// credential for it signs in. Saves and pins the server.
    pub async fn redeem_invite_portable(
        &self,
        link: String,
        identity: String,
        display_name: Option<String>,
    ) -> Result<Arc<SpoolSession>, SpoolError> {
        let identity = crate::portable::identity(&identity)?;
        let client = self.client.clone();
        let redeemed = run(async move {
            client
                .redeem_invite_portable(&link, identity, display_name.as_deref())
                .await
        })
        .await?;
        Ok(session(redeemed))
    }

    /// Sign in to `endpoint_id` as portable identity `identity` (hex) with
    /// this device's credential for it: no password. The account it's
    /// tied to there, else the identity as a guest.
    pub async fn sign_in_portable(
        &self,
        endpoint_id: String,
        identity: String,
    ) -> Result<Arc<SpoolSession>, SpoolError> {
        let identity = crate::portable::identity(&identity)?;
        let client = self.client.clone();
        let signed_in =
            run(async move { client.sign_in_portable(&endpoint_id, identity).await }).await?;
        Ok(session(signed_in))
    }

    /// The profiles on `endpoint_id` that last signed in as a portable
    /// identity: "Who's watching" signs them in again with
    /// `sign_in_portable`, asking nothing.
    pub async fn portable_profiles(
        &self,
        endpoint_id: String,
    ) -> Result<Vec<PortableProfile>, SpoolError> {
        let client = self.client.clone();
        let saved = run(async move { client.saved_profiles(&endpoint_id).await }).await?;
        Ok(saved
            .into_iter()
            .filter(|p| p.method == Some(sdk::SignInMethod::Portable))
            .filter_map(|p| {
                p.portable.map(|id| PortableProfile {
                    username: p.username,
                    display_name: p.display_name,
                    identity: id.to_string(),
                    identity_fingerprint: id.fingerprint(),
                })
            })
            .collect())
    }

    /// Every profile kept on this device for `endpoint_id`: who else a
    /// Leave leaves the server here for.
    pub async fn saved_profiles(
        &self,
        endpoint_id: String,
    ) -> Result<Vec<SavedProfileName>, SpoolError> {
        let client = self.client.clone();
        let saved = run(async move { client.saved_profiles(&endpoint_id).await }).await?;
        Ok(saved
            .into_iter()
            .map(|p| SavedProfileName {
                username: p.username,
                display_name: p.display_name,
            })
            .collect())
    }

    /// What an invite link says, sending nothing: the server, its
    /// fingerprint, its kind (account, guest, or a server's federation
    /// invite), the hints. `NotAnInvite` for a paste that isn't one.
    pub async fn read_invite(&self, link: String) -> Result<InviteInfo, SpoolError> {
        let client = self.client.clone();
        Ok(run(async move { client.read_invite(&link).await }).await?)
    }

    /// Reach the invite's server (Hello only, nothing saved), for "Is this
    /// the server you were invited to?": its name, fingerprint, server id.
    pub async fn connect_invite(&self, link: String) -> Result<ServerEntry, SpoolError> {
        let client = self.client.clone();
        Ok(run(async move { client.connect_invite(&link).await }).await?)
    }

    /// "Got an invite?": reach the server `link` names and ask what its
    /// code is (valid, expired, used, revoked, no match), using nothing up
    /// and saving nothing. Each lookup counts toward the server's five
    /// wrong guesses; after them, `RateLimited`. `NotOnServer` when the
    /// server can't look invites up: show the join as before.
    pub async fn look_up_invite(&self, link: String) -> Result<InviteLookup, SpoolError> {
        let client = self.client.clone();
        Ok(run(async move { client.look_up_invite(&link).await }).await?)
    }

    /// A code typed in (`QW7K-2MFP-XR4T`, any case or dashes), looked up
    /// at server `endpoint_id`: a saved one (`addrs` empty), or one heard
    /// on the network or reached at `addrs` ("host:port").
    /// `NotAnInviteCode` before anything is sent; otherwise as
    /// `look_up_invite`.
    pub async fn look_up_invite_code(
        &self,
        endpoint_id: String,
        addrs: Vec<String>,
        typed: String,
    ) -> Result<InviteLookup, SpoolError> {
        let addrs = self::addrs(&addrs)?;
        let client = self.client.clone();
        Ok(run(async move {
            client
                .look_up_invite_code(&endpoint_id, addrs, &typed)
                .await
        })
        .await?)
    }

    /// The servers heard on this network just now, saved or not, in
    /// `servers()`'s order. `watch_servers` says when to ask again.
    pub async fn lan_servers(&self) -> Result<Vec<ServerEntry>, SpoolError> {
        let client = self.client.clone();
        Ok(run(async move { client.lan_servers().await }).await?)
    }

    /// What server `endpoint_id`, from `lan_servers`, says of itself
    /// before anyone signs in: its version, and whether it's invite only.
    /// Only `Hello` is sent; nothing is saved.
    pub async fn lan_server_info(&self, endpoint_id: String) -> Result<LanServerInfo, SpoolError> {
        let client = self.client.clone();
        Ok(run(async move { client.lan_server_info(&endpoint_id).await }).await?)
    }

    /// Save the invite's server (pinned, verified by the link) without
    /// making an account: "Save server only", or a link with no code.
    pub async fn save_invite_server(&self, link: String) -> Result<ServerEntry, SpoolError> {
        let client = self.client.clone();
        Ok(run(async move { client.save_invite_server(&link).await }).await?)
    }

    /// Make the invite's account and sign in; the server is saved only if
    /// that works. `UsernameTaken`, `InviteInvalid`, `InviteExpired`,
    /// `InviteExhausted` as the server answers.
    pub async fn redeem_invite(
        &self,
        link: String,
        username: String,
        display_name: Option<String>,
        password: String,
    ) -> Result<Arc<SpoolSession>, SpoolError> {
        let client = self.client.clone();
        let redeemed = run(async move {
            client
                .redeem_invite(&link, &username, display_name.as_deref(), &password)
                .await
        })
        .await?;
        Ok(session(redeemed))
    }

    /// A server's connection state and its changes.
    pub fn state(&self, endpoint_id: String) -> Result<Arc<StateWatch>, SpoolError> {
        let watch = self.client.state(&endpoint_id)?;
        Ok(Arc::new(StateWatch {
            current: watch.clone(),
            changes: Arc::new(Mutex::new(watch)),
        }))
    }

    /// Each time the server signs this device out of `endpoint_id` (its
    /// last session ended: a sign-out, a revoke, a password change), with
    /// every stored session for it already ended.
    pub fn signed_out(&self, endpoint_id: String) -> Result<Arc<SignOutWatch>, SpoolError> {
        let watch = self.client.signed_out(&endpoint_id)?;
        Ok(Arc::new(SignOutWatch(Arc::new(Mutex::new(watch)))))
    }

    /// Ask to be signed in from another device: a code to show, as text
    /// and as a QR code, while `wait` collects the session. `remember`
    /// asks to be trusted; the approver decides.
    pub async fn begin_device_sign_in(
        &self,
        endpoint_id: String,
        remember: bool,
    ) -> Result<Arc<PendingSignIn>, SpoolError> {
        let client = self.client.clone();
        let sign_in =
            run(async move { client.begin_device_sign_in(&endpoint_id, remember).await }).await?;
        Ok(Arc::new(PendingSignIn {
            client: self.client.clone(),
            sign_in,
            waiting: Default::default(),
        }))
    }

    /// The app is back in the foreground (or the device woke, or changed
    /// network): each open connection is pinged, and one that died while
    /// suspended is redialled, so the next request doesn't wait it out.
    /// Cheap; returns within a couple of seconds.
    pub async fn resume(&self) {
        let client = self.client.clone();
        run(async move { client.resume().await }).await
    }

    /// Close every connection. The client is done after this.
    pub async fn close(&self) {
        let client = self.client.clone();
        run(async move { client.close().await }).await
    }
}

/// The server list, as it changes.
#[derive(uniffi::Object)]
pub struct ServerWatch {
    /// Weak: the watch must not keep the client, and so its list's
    /// sender, alive, or `next` never ends.
    client: sdk::WeakClient,
    changes: Arc<Mutex<sdk::ServersWatch>>,
}

#[uniffi::export]
impl ServerWatch {
    /// `servers()` after the next change; `None` once the client is
    /// gone.
    pub async fn next(&self) -> Result<Option<Vec<ServerEntry>>, SpoolError> {
        let client = self.client.clone();
        let changes = self.changes.clone();
        run(async move {
            if !changes.lock().await.changed().await {
                return Ok(None);
            }
            match client.upgrade() {
                Some(client) => client.servers().await.map(Some),
                None => Ok(None),
            }
        })
        .await
        .map_err(Into::into)
    }
}

/// One server's connection banner: its state, sign-outs and a needed
/// update.
#[derive(uniffi::Object)]
pub struct ServerEventWatch {
    current: ConnectionState,
    events: Arc<Mutex<sdk::ServerEvents>>,
}

#[uniffi::export]
impl ServerEventWatch {
    /// The connection state when the watch began.
    pub fn current(&self) -> ConnectionState {
        self.current
    }

    /// The next event; `None` once the client is gone.
    pub async fn next(&self) -> Option<ServerEvent> {
        let events = self.events.clone();
        run(async move { events.lock().await.next().await }).await
    }
}

/// Changes of one server's connection state.
#[derive(uniffi::Object)]
pub struct StateWatch {
    current: sdk::StateWatch,
    changes: Arc<Mutex<sdk::StateWatch>>,
}

#[uniffi::export]
impl StateWatch {
    pub fn current(&self) -> ConnectionState {
        self.current.current()
    }

    /// Why the link stopped dialling, while Disconnected: something
    /// dialling again won't fix (the app is too old, the server isn't the
    /// pinned one, this device was signed out). `None` while it's still
    /// trying.
    pub fn stopped(&self) -> Option<SpoolError> {
        self.current.stopped().map(Into::into)
    }

    /// The next state; `None` once the client is gone.
    pub async fn changed(&self) -> Option<ConnectionState> {
        let watch = self.changes.clone();
        run(async move { watch.lock().await.changed().await }).await
    }
}

/// The server signing this device out, as it happens.
#[derive(uniffi::Object)]
pub struct SignOutWatch(Arc<Mutex<sdk::SignOutWatch>>);

#[uniffi::export]
impl SignOutWatch {
    /// Waits for the next sign-out; `false` once the client is gone. One
    /// between two calls returns at once.
    pub async fn next(&self) -> bool {
        let watch = self.0.clone();
        run(async move { watch.lock().await.next().await })
            .await
            .is_some()
    }
}

/// What a waiting sign-in shows ([`PendingSignIn::details`]).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SignInCode {
    /// As shown, large: `BCDF-GHJK`.
    pub code: String,
    /// What the QR code carries.
    pub link: String,
    /// The server's web page that approves it; `None` without a public
    /// web UI.
    pub web_url: Option<String>,
    /// How long it can be approved from when it began.
    pub expires_in: std::time::Duration,
}

/// This device's sign-in, waiting for another device to approve it.
#[derive(uniffi::Object)]
pub struct PendingSignIn {
    client: sdk::Client,
    sign_in: sdk::DeviceSignIn,
    /// The `wait` running, to stop on `cancel`: a foreign task's
    /// cancellation doesn't reach a Rust future.
    waiting: Arc<std::sync::Mutex<Option<tokio::task::AbortHandle>>>,
}

#[uniffi::export]
impl PendingSignIn {
    /// The code as shown, large: `BCDF-GHJK`.
    pub fn code(&self) -> String {
        self.sign_in.display_code.clone()
    }

    /// What the QR code carries: `spoolframe://approve?server=…&code=…`.
    pub fn link(&self) -> String {
        self.sign_in.link.clone()
    }

    /// The server's web page that approves it, for any phone's camera;
    /// `None` when the server's web UI has no public URL.
    pub fn web_url(&self) -> Option<String> {
        self.sign_in.web_url.clone()
    }

    /// How long it can be approved from when it began (5 minutes).
    pub fn expires_in(&self) -> std::time::Duration {
        self.sign_in.expires_in
    }

    /// The code, link, web page and expiry in one record, as the screen
    /// shows them.
    pub fn details(&self) -> SignInCode {
        SignInCode {
            code: self.code(),
            link: self.link(),
            web_url: self.web_url(),
            expires_in: self.expires_in(),
        }
    }

    /// Wait for the outcome: the session once approved, `SignInDenied`, or
    /// `SignInExpired` (start again). `Cancelled` once `cancel` stops it.
    pub async fn wait(&self) -> Result<Arc<SpoolSession>, SpoolError> {
        let client = self.client.clone();
        let sign_in = self.sign_in.clone();
        let waiting = self.waiting.clone();
        let approved = run(async move {
            let task = tokio::spawn(async move { client.await_device_sign_in(&sign_in).await });
            *waiting.lock().expect("never poisoned") = Some(task.abort_handle());
            match task.await {
                Ok(answer) => answer.map_err(SpoolError::from),
                Err(_) => Err(SpoolError::Cancelled),
            }
        })
        .await?;
        Ok(session(approved))
    }

    /// Stop waiting: `wait` ends with `Cancelled`, and nothing it would
    /// have signed in is kept. The code itself lapses on the server.
    pub fn cancel(&self) {
        if let Some(task) = self.waiting.lock().expect("never poisoned").take() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::values::DeviceCapabilities;

    fn device(catalog_cache: Option<u32>) -> DeviceSpec {
        DeviceSpec {
            name: "iPhone".into(),
            client_name: "spoolframe-ios".into(),
            client_version: "1".into(),
            platform: Platform::Ios,
            capabilities: DeviceCapabilities::default(),
            relays: true,
            experimental: Experimental::default(),
            downloads_dir: None,
            catalog_cache,
        }
    }

    /// The app turns the catalog cache on with a capacity; off by default.
    #[test]
    fn a_device_asks_for_the_catalog_cache_by_its_capacity() {
        assert_eq!(
            [
                options_of(&device(Some(500))).catalog_cache,
                options_of(&device(None)).catalog_cache,
            ],
            [Some(sdk::CacheConfig { capacity: 500 }), None]
        );
    }
}
