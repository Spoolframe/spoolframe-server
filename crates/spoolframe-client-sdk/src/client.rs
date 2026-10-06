//! This device: its identity and its endpoint (ARCHITECTURE §13–17).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use iroh::endpoint::{QuicTransportConfig, presets};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, SecretKey, TransportAddr};
use iroh_mdns_address_lookup::MdnsAddressLookup;
use std::time::Duration;

use spoolframe_protocol::{
    FinishPasskeySignIn, LoginPassword, PasskeyRequest, Reply, Request, Response, SessionId,
    TrustedProfile, device_sign_in, fingerprint, lan,
};
use spoolframe_types::{ClientCapabilities, Platform};

use crate::connection::{ServerConnection, Timeouts};
use crate::discovery::Discovery;
use crate::error::SdkError;
use crate::link::{ConnectionPath, Link, SignOutWatch, StateWatch};
use crate::servers::{self, AddrSource, SavedServer, ServerEntry};
use crate::session::{self, SavedProfile, Session, SignInMethod, StoredSession};
use crate::storage::{Storage, StorageError, keys};
use spoolframe_protocol::policy::retry_safe;
use spoolframe_protocol::rpc::{OPERATION_WINDOW_MS, Operation, RpcRequest};

/// How often the endpoint sends a QUIC keep-alive on an idle connection
/// (iroh's own default, made explicit: the idle timeout below needs it).
const KEEP_ALIVE: Duration = Duration::from_secs(5);
/// How long a connection may hear nothing from the server before QUIC
/// closes it: three keep-alives unanswered. iroh's default is 30 s; its
/// per-path limit is already 15 s, and a connection is no better than its
/// paths. The lower of this and the server's own limit applies.
const IDLE_TIMEOUT: Duration = Duration::from_secs(15);

type RevisionState = Arc<tokio::sync::Mutex<Option<u64>>>;
type PlaybackKey = (EndpointId, String, String);

/// What the app says about this device and itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// What the device calls itself (a hostname, "Living room TV"); shown
    /// in the server's session lists.
    pub name: String,
    /// The app, e.g. `spoolframe-linux`, and its version.
    pub client_name: String,
    pub client_version: String,
    /// What kind of client the app is: the server shows it in device and
    /// session lists ("Linux app", "Apple TV app").
    pub platform: Platform,
    /// What this device can play, from its decoders (P1-07).
    pub capabilities: ClientCapabilities,
}

/// A sign-in of this device waiting for approval on another (P3-07).
#[derive(Clone, PartialEq, Eq)]
pub struct DeviceSignIn {
    /// The server's key.
    pub server: String,
    /// The code as typed (`BCDFGHJK`).
    pub code: String,
    /// The code as shown, large: `BCDF-GHJK`.
    pub display_code: String,
    /// What to show as a QR code for a phone with the Spoolframe app:
    /// `spoolframe://approve?server=…&code=…`.
    pub link: String,
    /// What to show as a QR code for any phone: the page of the server's
    /// web UI that approves this code, which its camera opens in a browser
    /// (`https://media.example.org/auth/device?code=BCDFGHJK`). `None` if
    /// the server's web UI has no public URL: then only `link` and the
    /// code typed by hand can approve.
    pub web_url: Option<String>,
    /// How long it can be approved, from when it began.
    pub expires_in: Duration,
    /// It asked to be remembered (P4-01): the approver decides, and the
    /// session it brings says what they chose ([`Session::remembered`]).
    pub remember: bool,
    /// Only this device can collect the session with it.
    request: String,
}

impl std::fmt::Debug for DeviceSignIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceSignIn")
            .field("server", &self.server)
            .field("code", &self.display_code)
            .field("expires_in", &self.expires_in)
            .finish_non_exhaustive()
    }
}

/// This device. Cheap to clone: every clone is the same device.
#[derive(Clone)]
pub struct Client {
    pub(crate) inner: Arc<Inner>,
}

pub(crate) struct Inner {
    pub(crate) endpoint: Endpoint,
    pub(crate) storage: Arc<dyn Storage>,
    pub(crate) device: Device,
    pub(crate) timeouts: Timeouts,
    discovery: Discovery,
    /// Addresses the app gave for servers (P6's invite tickets will too).
    manual: Mutex<HashMap<EndpointId, Vec<SocketAddr>>>,
    /// Each server's connection and its state.
    links: Mutex<HashMap<EndpointId, Arc<Link>>>,
    /// Serializes read-modify-write of the saved servers and sessions.
    saving: tokio::sync::Mutex<()>,
    revisions: Mutex<HashMap<PlaybackKey, RevisionState>>,
    progress_sequences: Mutex<HashMap<PlaybackKey, RevisionState>>,
    activating: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("id", &self.id())
            .field("device", &self.inner.device.name)
            .finish_non_exhaustive()
    }
}

impl Client {
    /// Open this device: its key from `storage` (made on first start), and
    /// an endpoint with relays off that listens for servers on the LAN and
    /// never announces itself. Requests wait for answers as
    /// [`Timeouts::default`] says.
    pub async fn open(storage: Arc<dyn Storage>, device: Device) -> Result<Self, SdkError> {
        Self::open_with(storage, device, Timeouts::default()).await
    }

    /// [`Client::open`], with requests waiting for answers as `timeouts`
    /// says.
    pub async fn open_with(
        storage: Arc<dyn Storage>,
        device: Device,
        timeouts: Timeouts,
    ) -> Result<Self, SdkError> {
        let key = load_or_create_key(storage.as_ref()).await?;
        // Minimal: no pkarr or DNS lookup, so a LAN client never tries the
        // Internet. mDNS only listens: a laptop or a TV must not announce
        // itself to the network (P1-13).
        let cannot_bind =
            |e: String| SdkError::Unreachable(format!("cannot bind the endpoint: {e}"));
        let mdns = MdnsAddressLookup::builder()
            .advertise(false)
            .service_name(lan::MDNS_SERVICE)
            .build(key.public())
            .map_err(|e| cannot_bind(e.to_string()))?;
        let transport = QuicTransportConfig::builder()
            .keep_alive_interval(KEEP_ALIVE)
            .max_idle_timeout(Some(
                IDLE_TIMEOUT
                    .try_into()
                    .expect("15 s is a valid idle timeout"),
            ))
            .build();
        let endpoint = Endpoint::builder(presets::Minimal)
            .secret_key(key)
            .relay_mode(RelayMode::Disabled)
            .transport_config(transport)
            .bind()
            .await
            .map_err(|e| cannot_bind(e.to_string()))?;
        // The same lookup finds the addresses of servers being dialled.
        endpoint
            .address_lookup()
            .map_err(|e| cannot_bind(e.to_string()))?
            .add(mdns.clone());
        let discovery = Discovery::start(&mdns).await;
        Ok(Self {
            inner: Arc::new(Inner {
                endpoint,
                storage,
                device,
                timeouts,
                discovery,
                manual: Mutex::default(),
                links: Mutex::default(),
                saving: tokio::sync::Mutex::default(),
                revisions: Mutex::default(),
                progress_sequences: Mutex::default(),
                activating: tokio::sync::Mutex::default(),
            }),
        })
    }

    /// The servers this device knows: saved ones and ones heard on the LAN,
    /// one entry per key. Saved first, then heard, each by name.
    ///
    /// A server heard under a saved server's name, with another key, while
    /// the saved key is not heard, is not listed apart: it is that server's
    /// entry, whose connect says [`SdkError::IdentityChanged`] (P1-13).
    pub async fn servers(&self) -> Result<Vec<ServerEntry>, SdkError> {
        let saved = servers::load(self.inner.storage.as_ref()).await?;
        let mut heard = self.inner.discovery.heard();
        let away: Vec<String> = saved
            .iter()
            .filter(|s| parse_id(&s.endpoint_id).map_or(true, |id| !heard.contains_key(&id)))
            .map(|s| s.name.clone())
            .collect();
        let mut entries: Vec<ServerEntry> = saved
            .into_iter()
            .map(|s| {
                let id = EndpointId::from_str(&s.endpoint_id).ok();
                let on_lan = id.and_then(|id| heard.remove(&id));
                let addrs = listed([
                    id.map(|id| self.in_use(id)).unwrap_or_default(),
                    on_lan.as_ref().map(|h| h.addrs.clone()).unwrap_or_default(),
                    s.addrs(),
                ]);
                ServerEntry {
                    fingerprint: fingerprint::of_hex(&s.endpoint_id).unwrap_or_default(),
                    pinned_at: servers::time(s.pinned_at),
                    verified_at: servers::time(s.verified_at),
                    last_connected_at: servers::time(s.last_connected_at),
                    endpoint_id: s.endpoint_id,
                    name: s.name,
                    pinned: true,
                    verified: s.verified,
                    on_network: on_lan.is_some(),
                    addrs,
                }
            })
            .collect();
        let mut unsaved: Vec<ServerEntry> = heard
            .into_iter()
            .filter(|(_, h)| !away.contains(&h.name))
            .map(|(id, h)| ServerEntry {
                endpoint_id: id.to_string(),
                fingerprint: fingerprint::fingerprint(id.as_bytes()),
                name: h.name,
                addrs: h.addrs,
                pinned: false,
                verified: false,
                on_network: true,
                pinned_at: None,
                verified_at: None,
                last_connected_at: None,
            })
            .collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        unsaved.sort_by(|a, b| a.name.cmp(&b.name));
        entries.extend(unsaved);
        Ok(entries)
    }

    /// Changes of [`Client::servers`]: a server heard or gone on the LAN, or
    /// one pinned or forgotten. The first answer comes before mDNS has had
    /// time to hear anything (about a second), so an app lists once, then
    /// again on every change.
    pub fn watch_servers(&self) -> ServersWatch {
        ServersWatch(self.inner.discovery.watch())
    }

    /// Where to find server `endpoint_id`, from outside the LAN's records:
    /// typed in, or (P6) from an invite. A hint like any other: the key is
    /// still proven on connect.
    pub fn add_address(&self, endpoint_id: &str, addrs: Vec<SocketAddr>) -> Result<(), SdkError> {
        let id = parse_id(endpoint_id)?;
        self.inner
            .manual
            .lock()
            .expect("never poisoned")
            .insert(id, addrs);
        Ok(())
    }

    /// Connect to server `endpoint_id`, or use the open connection, and say
    /// who answered: its name and its key's fingerprint. A server not saved
    /// yet must have been heard on the LAN or given an address, and is not
    /// saved by this: the app shows the fingerprint, then calls
    /// [`Client::pin`] (P1-21). Until then only `Hello` is sent to it.
    ///
    /// [`SdkError::IdentityChanged`] when a saved server seems to answer
    /// with another key (P1-13): `endpoint_id` is heard under a saved
    /// server's name while that server's own key is not, or a saved server
    /// cannot be reached and another key is heard under its name. Nothing
    /// is connected or saved; [`Client::trust_new_identity`] replaces the
    /// pin if the user says so.
    pub async fn connect(&self, endpoint_id: &str) -> Result<ServerEntry, SdkError> {
        let id = parse_id(endpoint_id)?;
        let saved = servers::load(self.inner.storage.as_ref()).await?;
        let pinned = saved.iter().find(|s| s.endpoint_id == endpoint_id).cloned();
        if pinned.is_none()
            && let Some(old) = self.replaced_by(&saved, id)
        {
            return Err(SdkError::IdentityChanged {
                saved: old.endpoint_id.clone(),
                now: endpoint_id.to_owned(),
            });
        }
        let conn = match self.link(id).connection(self).await {
            Ok(conn) => conn,
            Err(e) => {
                if let Some(old) = &pinned
                    && let Some(now) = self.replacement(old)
                {
                    return Err(SdkError::IdentityChanged {
                        saved: old.endpoint_id.clone(),
                        now: now.to_string(),
                    });
                }
                return Err(e);
            }
        };
        // Read again: dialling it has just dated the connect.
        let pinned = servers::load(self.inner.storage.as_ref())
            .await?
            .into_iter()
            .find(|s| s.endpoint_id == endpoint_id);
        Ok(self.entry(id, &conn, pinned.as_ref()))
    }

    /// Save server `endpoint_id`, which [`Client::connect`] reached, with
    /// `verified` saying whether the user confirmed its fingerprint (P1-21).
    /// A saved server keeps its pin and only has `verified` set: "Verify
    /// now", with no reconnect.
    pub async fn pin(&self, endpoint_id: &str, verified: bool) -> Result<ServerEntry, SdkError> {
        let id = parse_id(endpoint_id)?;
        {
            let storage = self.inner.storage.as_ref();
            let _saving = self.inner.saving.lock().await;
            let mut saved = servers::load(storage).await?;
            if let Some(server) = saved.iter_mut().find(|s| s.endpoint_id == endpoint_id) {
                server.set_verified(verified);
                let entry = ServerEntry {
                    endpoint_id: server.endpoint_id.clone(),
                    name: server.name.clone(),
                    addrs: listed([self.in_use(id), server.addrs()]),
                    fingerprint: fingerprint::fingerprint(id.as_bytes()),
                    pinned: true,
                    verified,
                    on_network: self.inner.discovery.get(&id).is_some(),
                    pinned_at: servers::time(server.pinned_at),
                    verified_at: servers::time(server.verified_at),
                    last_connected_at: servers::time(server.last_connected_at),
                };
                servers::save(storage, &saved).await?;
                self.inner.discovery.changed();
                return Ok(entry);
            }
            // A pin is never taken over this way: that is
            // `trust_new_identity`, which the user has to choose.
            if let Some(old) = self.replaced_by(&saved, id) {
                return Err(SdkError::IdentityChanged {
                    saved: old.endpoint_id.clone(),
                    now: endpoint_id.to_owned(),
                });
            }
        }
        let conn = self.link(id).connection(self).await?;
        let storage = self.inner.storage.as_ref();
        let _saving = self.inner.saving.lock().await;
        // Read under the lock: an address the connection reaches from now
        // on is saved by `save_reached`, which waits for it.
        let server = SavedServer::pinned_now(
            conn.info.name.clone(),
            endpoint_id.to_owned(),
            conn.reached(),
            verified,
        );
        let mut saved = servers::load(storage).await?;
        saved.retain(|s| s.endpoint_id != endpoint_id);
        tracing::info!(
            id = endpoint_id,
            name = server.name,
            verified,
            "pinned a new server"
        );
        saved.push(server.clone());
        servers::save(storage, &saved).await?;
        self.inner.discovery.changed();
        Ok(self.entry(id, &conn, Some(&server)))
    }

    /// Replace saved server `saved_id`'s pin with `new_id`, which answered in
    /// its place (P1-13), after the user checked `new_id`'s fingerprint.
    /// The old key's sessions are forgotten: they belonged to the server
    /// that key was, so everyone signs in again.
    pub async fn trust_new_identity(
        &self,
        saved_id: &str,
        new_id: &str,
    ) -> Result<ServerEntry, SdkError> {
        let old = parse_id(saved_id)?;
        let new = parse_id(new_id)?;
        let conn = self.link(new).connection(self).await?;
        if let Some(link) = self
            .inner
            .links
            .lock()
            .expect("never poisoned")
            .remove(&old)
        {
            link.close();
        }
        let storage = self.inner.storage.as_ref();
        let _saving = self.inner.saving.lock().await;
        // The user checked the new key's fingerprint to get here.
        let server = SavedServer::pinned_now(
            conn.info.name.clone(),
            new_id.to_owned(),
            conn.reached(),
            true,
        );
        let mut saved = servers::load(storage).await?;
        let Some(at) = saved.iter().position(|s| s.endpoint_id == saved_id) else {
            return Err(SdkError::UnknownServer(saved_id.to_owned()));
        };
        tracing::warn!(
            old = saved_id,
            new = new_id,
            "replaced a server's pin, as the user asked"
        );
        saved[at] = server.clone();
        servers::save(storage, &saved).await?;
        storage.set_secret(&keys::sessions(saved_id), None).await?;
        self.inner.discovery.changed();
        Ok(self.entry(new, &conn, Some(&server)))
    }

    /// A saved server that `id` answers in place of: `id` is heard under its
    /// name, and its own key is not heard.
    fn replaced_by<'a>(&self, saved: &'a [SavedServer], id: EndpointId) -> Option<&'a SavedServer> {
        let heard = self.inner.discovery.get(&id)?;
        saved.iter().find(|s| {
            s.name == heard.name
                && s.endpoint_id != id.to_string()
                && parse_id(&s.endpoint_id)
                    .map_or(true, |own| self.inner.discovery.get(&own).is_none())
        })
    }

    /// Another key heard under saved server `server`'s name while its own is
    /// not heard.
    fn replacement(&self, server: &SavedServer) -> Option<EndpointId> {
        let heard = self.inner.discovery.heard();
        if parse_id(&server.endpoint_id).is_ok_and(|own| heard.contains_key(&own)) {
            return None;
        }
        heard
            .into_iter()
            .find(|(id, h)| h.name == server.name && id.to_string() != server.endpoint_id)
            .map(|(id, _)| id)
    }

    /// `id` as `conn` reached it, saved as `saved` says.
    fn entry(
        &self,
        id: EndpointId,
        conn: &ServerConnection,
        saved: Option<&SavedServer>,
    ) -> ServerEntry {
        ServerEntry {
            endpoint_id: id.to_string(),
            name: conn.info.name.clone(),
            addrs: listed([
                conn.remote_addrs(),
                conn.reached(),
                saved.map(SavedServer::addrs).unwrap_or_default(),
            ]),
            fingerprint: fingerprint::fingerprint(id.as_bytes()),
            pinned: saved.is_some(),
            verified: saved.is_some_and(|s| s.verified),
            on_network: self.inner.discovery.get(&id).is_some(),
            pinned_at: saved.and_then(|s| servers::time(s.pinned_at)),
            verified_at: saved.and_then(|s| servers::time(s.verified_at)),
            last_connected_at: saved.and_then(|s| servers::time(s.last_connected_at)),
        }
    }

    /// Where the open connection to `id` reaches it, the path in use
    /// first; nothing without one. Never dials.
    fn in_use(&self, id: EndpointId) -> Vec<SocketAddr> {
        let link = self
            .inner
            .links
            .lock()
            .expect("never poisoned")
            .get(&id)
            .cloned();
        link.and_then(|l| l.open())
            .map(|conn| conn.remote_addrs())
            .unwrap_or_default()
    }

    /// Requests other than `Hello` go only to a pinned server (P1-21).
    async fn require_pinned(&self, id: EndpointId) -> Result<(), SdkError> {
        let saved = servers::load(self.inner.storage.as_ref()).await?;
        if saved.iter().any(|s| s.endpoint_id == id.to_string()) {
            Ok(())
        } else {
            Err(SdkError::NotPinned(id.to_string()))
        }
    }

    /// Forget server `endpoint_id`: its pin and every session on it.
    pub async fn forget(&self, endpoint_id: &str) -> Result<(), SdkError> {
        let id = parse_id(endpoint_id)?;
        if let Some(link) = self.inner.links.lock().expect("never poisoned").remove(&id) {
            link.close();
        }
        let storage = self.inner.storage.as_ref();
        let _saving = self.inner.saving.lock().await;
        let mut saved = servers::load(storage).await?;
        saved.retain(|s| s.endpoint_id != endpoint_id);
        servers::save(storage, &saved).await?;
        storage
            .set_secret(&keys::sessions(endpoint_id), None)
            .await?;
        self.inner.discovery.changed();
        Ok(())
    }

    /// Sign `username` in on server `endpoint_id` with a password (§22),
    /// and keep the session. With `remember` ("Remember this device",
    /// never ticked by default), the server trusts this device for them
    /// (P4-01): once the session ends, [`Client::resume_profile`] opens
    /// it again without a password, until they sign out here.
    pub async fn sign_in(
        &self,
        endpoint_id: &str,
        username: &str,
        password: &str,
        remember: bool,
    ) -> Result<Session, SdkError> {
        let server = parse_id(endpoint_id)?;
        self.require_pinned(server).await?;
        let login = Request::LoginPassword(LoginPassword {
            username: username.to_owned(),
            password: password.to_owned(),
            remember,
        });
        let (token, remembered) = match self.call(server, None, login).await? {
            Response::Ok(Reply::LoggedIn {
                session,
                remembered,
            }) => (session, remembered),
            Response::Ok(other) => {
                return Err(SdkError::Protocol(format!("login answered {other:?}")));
            }
            Response::Err(e) => return Err(SdkError::from_server(e)),
        };
        self.keep_session(server, token, Some(SignInMethod::Password), remembered)
            .await
    }

    /// The users this device is trusted for on server `endpoint_id`
    /// (P4-01), by username: for "Who's watching" once their sessions have
    /// ended. One with `auto_login` is the profile to open at start.
    /// Trust that lapsed or was removed is not listed.
    pub async fn trusted_profiles(
        &self,
        endpoint_id: &str,
    ) -> Result<Vec<TrustedProfile>, SdkError> {
        let server = parse_id(endpoint_id)?;
        self.require_pinned(server).await?;
        match self.call(server, None, Request::TrustedProfiles).await? {
            Response::Ok(Reply::TrustedProfiles { profiles }) => Ok(profiles),
            Response::Ok(other) => Err(SdkError::Protocol(format!(
                "TrustedProfiles answered {other:?}"
            ))),
            Response::Err(e) => Err(SdkError::from_server(e)),
        }
    }

    /// Open a session for `user_id` (a [`TrustedProfile::user_id`]) on
    /// server `endpoint_id` without a password, on a device trusted for
    /// them (P4-01), and keep it as a sign-in does. Its profile may start
    /// locked, as after any sign-in, when the device asks for their PIN.
    /// [`SdkError::SignInRequired`], whatever the reason, when the device
    /// is no longer trusted for them (removed, lapsed, or the user
    /// disabled): they sign in again in full.
    pub async fn resume_profile(
        &self,
        endpoint_id: &str,
        user_id: &str,
    ) -> Result<Session, SdkError> {
        let server = parse_id(endpoint_id)?;
        self.require_pinned(server).await?;
        let resume = Request::ResumeProfile {
            user_id: user_id.to_owned(),
        };
        let (token, remembered) = match self.call(server, None, resume).await? {
            Response::Ok(Reply::LoggedIn {
                session,
                remembered,
            }) => (session, remembered),
            Response::Ok(other) => {
                return Err(SdkError::Protocol(format!(
                    "ResumeProfile answered {other:?}"
                )));
            }
            Response::Err(e) => return Err(SdkError::from_server(e)),
        };
        // Not a sign-in: how they last signed in stays what it was.
        self.keep_session(server, token, None, remembered).await
    }

    /// Begin signing in on server `endpoint_id` with a passkey made in the
    /// app (P3-19): what the platform's passkey API signs, for the relying
    /// party [`crate::APP_RP_ID`]. No passkeys are listed: the platform
    /// offers whichever it has for this relying party (a keychain may hold
    /// several servers': their names say `<username> on <server name>`).
    /// With `username`, only that user's finish it. The challenge is this
    /// device's, for 5 minutes; beginning again replaces it.
    ///
    /// The server sends a nonce; the challenge is
    /// [`spoolframe_protocol::app_passkey_challenge`] of `endpoint_id`, the
    /// pinned server dialled, and that nonce (PX-10). A server that hands
    /// on another server's nonce gets a signature the other refuses.
    pub async fn begin_passkey_sign_in(
        &self,
        endpoint_id: &str,
        username: Option<&str>,
    ) -> Result<PasskeyRequest, SdkError> {
        let server = parse_id(endpoint_id)?;
        self.require_pinned(server).await?;
        let begin = Request::BeginPasskeySignIn {
            username: username.map(str::to_owned),
        };
        match self.call(server, None, begin).await? {
            Response::Ok(Reply::PasskeySignInBegun(begun)) => {
                Ok(begun.for_server(server.as_bytes()))
            }
            Response::Ok(other) => Err(SdkError::Protocol(format!(
                "BeginPasskeySignIn answered {other:?}"
            ))),
            Response::Err(e) => Err(SdkError::from_server(e)),
        }
    }

    /// Finish it with what the passkey signed, and keep the session, as
    /// [`Client::sign_in`] does. A passkey that does not sign in (another
    /// server's, a removed one, an expired or spent challenge) is
    /// [`SdkError::InvalidCredentials`], counted as a failed sign-in (then
    /// [`SdkError::RateLimited`]).
    /// `remember` is as for [`Client::sign_in`].
    pub async fn finish_passkey_sign_in(
        &self,
        endpoint_id: &str,
        signed: FinishPasskeySignIn,
        remember: bool,
    ) -> Result<Session, SdkError> {
        let server = parse_id(endpoint_id)?;
        self.require_pinned(server).await?;
        let (token, remembered) = match self
            .call(
                server,
                None,
                Request::FinishPasskeySignIn { signed, remember },
            )
            .await?
        {
            Response::Ok(Reply::LoggedIn {
                session,
                remembered,
            }) => (session, remembered),
            Response::Ok(other) => {
                return Err(SdkError::Protocol(format!(
                    "FinishPasskeySignIn answered {other:?}"
                )));
            }
            Response::Err(e) => return Err(SdkError::from_server(e)),
        };
        self.keep_session(server, token, Some(SignInMethod::Passkey), remembered)
            .await
    }

    /// Ask server `endpoint_id` to sign this device in by someone approving
    /// it on another device they are signed in on (P3-07): show the code
    /// and a QR code of the link, then [`Client::await_device_sign_in`].
    /// For a device without a keyboard (a TV).
    ///
    /// `remember` asks for this device to be trusted (P4-01), as
    /// [`Client::sign_in`]'s does; the approver sees the request and
    /// decides.
    pub async fn begin_device_sign_in(
        &self,
        endpoint_id: &str,
        remember: bool,
    ) -> Result<DeviceSignIn, SdkError> {
        let server = parse_id(endpoint_id)?;
        self.require_pinned(server).await?;
        match self
            .call(server, None, Request::BeginDeviceSignIn { remember })
            .await?
        {
            Response::Ok(Reply::DeviceSignInStarted {
                request,
                code,
                expires_in_ms,
                web_url,
            }) => Ok(DeviceSignIn {
                web_url,
                server: server.to_string(),
                link: device_sign_in::link(&server.to_string(), &code),
                display_code: device_sign_in::display(&code),
                code,
                request,
                expires_in: Duration::from_millis(expires_in_ms),
                remember,
            }),
            Response::Ok(other) => Err(SdkError::Protocol(format!(
                "BeginDeviceSignIn answered {other:?}"
            ))),
            Response::Err(e) => Err(SdkError::from_server(e)),
        }
    }

    /// Wait until `sign_in` is approved, and keep the session it brings:
    /// [`SdkError::SignInDenied`] if it was refused, [`SdkError::SignInExpired`]
    /// once it is over (begin another). Drop the future to stop waiting.
    ///
    /// [`SdkError::TimedOut`] or [`SdkError::Unreachable`] when the
    /// connection failed while waiting: call this again with the same
    /// `sign_in`, which lives on the server until it expires. It is not
    /// asked again by itself, since an approval the server sent on a
    /// connection that then died is spent; asking again says
    /// [`SdkError::SignInExpired`].
    pub async fn await_device_sign_in(&self, sign_in: &DeviceSignIn) -> Result<Session, SdkError> {
        let server = parse_id(&sign_in.server)?;
        loop {
            // Each round goes on the connection there is then: it may
            // have been replaced since, and the request lives on the server.
            let request = Request::AwaitDeviceSignIn {
                request: sign_in.request.clone(),
            };
            match self.call(server, None, request).await? {
                Response::Ok(Reply::LoggedIn {
                    session,
                    remembered,
                }) => {
                    return self
                        .keep_session(server, session, Some(SignInMethod::Device), remembered)
                        .await;
                }
                Response::Ok(Reply::DeviceSignInPending) => {}
                Response::Ok(other) => {
                    return Err(SdkError::Protocol(format!(
                        "AwaitDeviceSignIn answered {other:?}"
                    )));
                }
                Response::Err(e) => return Err(SdkError::from_server(e)),
            }
        }
    }

    /// Keep `token`, a new session on `server` made by `method` (`None`: a
    /// resume, which keeps the user's last method and place), under the
    /// user it is.
    async fn keep_session(
        &self,
        server: EndpointId,
        token: SessionId,
        method: Option<SignInMethod>,
        remembered: bool,
    ) -> Result<Session, SdkError> {
        let user = match self.call(server, Some(&token), Request::WhoAmI).await? {
            Response::Ok(Reply::WhoAmI(user)) => user,
            Response::Ok(other) => {
                return Err(SdkError::Protocol(format!("WhoAmI answered {other:?}")));
            }
            Response::Err(e) => return Err(SdkError::from_server(e)),
        };
        let storage = self.inner.storage.as_ref();
        let _saving = self.inner.saving.lock().await;
        let mut stored = session::load(storage, &server).await?;
        let kept = StoredSession {
            token: Some(token.0.clone()),
            username: user.username.clone(),
            display_name: user.display_name.clone(),
            method,
        };
        match (
            method,
            stored.iter_mut().find(|s| s.username == user.username),
        ) {
            // A resume: the same profile, in its place.
            (None, Some(old)) => {
                *old = StoredSession {
                    method: old.method,
                    ..kept
                };
            }
            // Signing in again as the same user replaces the old session
            // here; the server keeps it until it expires or is revoked.
            _ => {
                stored.retain(|s| s.username != user.username);
                stored.push(kept);
            }
        }
        session::save(storage, &server, &stored).await?;
        drop(_saving);
        // Signed in now, the device may be told where else the server is.
        if let Some(conn) = self.link(server).open() {
            tokio::spawn(learn_addresses(
                Arc::downgrade(&self.inner),
                server,
                conn.conn.clone(),
                Some(token.clone()),
            ));
        }
        Ok(Session {
            client: self.clone(),
            server,
            token,
            user,
            remembered: Some(remembered),
        })
    }

    /// "Who's watching": the users signed in on server `endpoint_id` from
    /// this device, as the server names them now. A session the server no
    /// longer knows is ended here: its user stays in
    /// [`Client::saved_profiles`], to sign in again. So is one the server
    /// signs the device out of while this runs ([`Client::signed_out`]):
    /// it is left out, not an error.
    pub async fn profiles(&self, endpoint_id: &str) -> Result<Vec<Session>, SdkError> {
        let server = parse_id(endpoint_id)?;
        self.require_pinned(server).await?;
        let stored = session::load(self.inner.storage.as_ref(), &server).await?;
        let mut profiles = Vec::new();
        for s in stored {
            let Some(token) = s.token else {
                continue;
            };
            let token = SessionId(token);
            // The server may sign the device out (close::SIGNED_OUT) after
            // the sessions were read: then the call is SignInRequired, with
            // or without asking. That session ended as surely as one the
            // server answers Unauthenticated for, and the list goes on.
            let answer = match self.call(server, Some(&token), Request::WhoAmI).await {
                Err(SdkError::SignInRequired) => {
                    Response::Err(spoolframe_protocol::Error::Unauthenticated)
                }
                answer => answer?,
            };
            match answer {
                Response::Ok(Reply::WhoAmI(user)) => profiles.push(Session {
                    client: self.clone(),
                    server,
                    token,
                    user,
                    remembered: None,
                }),
                Response::Err(spoolframe_protocol::Error::Unauthenticated) => {
                    tracing::info!(
                        user = s.username,
                        "a session has ended; keeping the profile"
                    );
                    self.end_session(server, &token).await?;
                }
                Response::Ok(other) => {
                    return Err(SdkError::Protocol(format!("WhoAmI answered {other:?}")));
                }
                Response::Err(e) => return Err(SdkError::from_server(e)),
            }
        }
        Ok(profiles)
    }

    /// Select the foreground profile. Listing profiles or making background
    /// requests never switches profiles or revokes another profile's PIN unlock.
    pub async fn activate_profile(
        &self,
        profile: &Session,
    ) -> Result<crate::ActiveProfile, SdkError> {
        let _activation = self.inner.activating.lock().await;
        if !Arc::ptr_eq(&self.inner, &profile.client.inner) {
            return Err(SdkError::InvalidInput(
                "profile belongs to another client".into(),
            ));
        }
        match self
            .call(
                profile.server,
                Some(&profile.token),
                Request::ActivateProfile,
            )
            .await?
        {
            Response::Ok(Reply::Done) => Ok(crate::ActiveProfile(profile.clone())),
            Response::Err(error) => Err(SdkError::from_server(error)),
            Response::Ok(other) => Err(SdkError::Protocol(format!(
                "ActivateProfile answered {other:?}"
            ))),
        }
    }

    /// Everyone signed in on server `endpoint_id` from this device, and
    /// whether choosing them signs in by itself or asks for a password: for
    /// "Who's watching" before, or without, a connection. In the order
    /// they first signed in. Nothing here asks the server; use
    /// [`Client::profiles`] for the sessions that still work.
    pub async fn saved_profiles(&self, endpoint_id: &str) -> Result<Vec<SavedProfile>, SdkError> {
        let server = parse_id(endpoint_id)?;
        let stored = session::load(self.inner.storage.as_ref(), &server).await?;
        Ok(stored.iter().map(SavedProfile::from).collect())
    }

    /// Take `username` off this device's "Who's watching" for server
    /// `endpoint_id`. A session kept for them is only forgotten here, not
    /// ended on the server: [`Session::sign_out`] does both.
    pub async fn forget_profile(&self, endpoint_id: &str, username: &str) -> Result<(), SdkError> {
        let server = parse_id(endpoint_id)?;
        self.update_sessions(server, |stored| stored.retain(|s| s.username != username))
            .await
    }

    /// Forget the session `token` on `server`, profile and all.
    pub(crate) async fn drop_session(
        &self,
        server: EndpointId,
        token: &SessionId,
    ) -> Result<(), SdkError> {
        self.update_sessions(server, |stored| {
            stored.retain(|s| s.token.as_deref() != Some(token.0.as_str()))
        })
        .await
    }

    /// The server ended the session `token` on `server`: forget the token,
    /// and keep its user, who now signs in with a password.
    pub(crate) async fn end_session(
        &self,
        server: EndpointId,
        token: &SessionId,
    ) -> Result<(), SdkError> {
        self.update_sessions(server, |stored| {
            for s in stored {
                if s.token.as_deref() == Some(token.0.as_str()) {
                    s.token = None;
                }
            }
        })
        .await
    }

    /// The server signed this device out (`close::SIGNED_OUT`): no session
    /// is left for it there. End every session kept for `server`, as
    /// [`Client::end_session`] does; the tokens that were kept.
    pub(crate) async fn end_every_session(
        &self,
        server: EndpointId,
    ) -> Result<Vec<String>, SdkError> {
        let mut ended = Vec::new();
        self.update_sessions(server, |stored| {
            ended.extend(stored.iter_mut().filter_map(|s| s.token.take()));
        })
        .await?;
        Ok(ended)
    }

    async fn update_sessions(
        &self,
        server: EndpointId,
        change: impl FnOnce(&mut Vec<StoredSession>),
    ) -> Result<(), SdkError> {
        let storage = self.inner.storage.as_ref();
        let _saving = self.inner.saving.lock().await;
        let mut stored = session::load(storage, &server).await?;
        change(&mut stored);
        session::save(storage, &server, &stored).await?;
        Ok(())
    }

    /// Where server `endpoint_id`'s connection stands, and its changes.
    pub fn state(&self, endpoint_id: &str) -> Result<StateWatch, SdkError> {
        Ok(self.link(parse_id(endpoint_id)?).watch())
    }

    /// Each time server `endpoint_id` signs this device out: it closed the
    /// connection because the device's last session there ended (signed
    /// out, revoked, its user disabled; [`spoolframe_protocol::close::SIGNED_OUT`]).
    /// The server sends that only once no session is left for this
    /// device, so by the time the watch fires every session kept for the
    /// server has been ended here, and their users stay on "Who's
    /// watching", to sign in again ([`Client::saved_profiles`]). A call in
    /// one of them is [`SdkError::SignInRequired`] without asking the
    /// server. The SDK does not dial again by itself: a device with no
    /// session has nothing to keep a connection for, and the server holds
    /// few of them. The next call dials.
    pub fn signed_out(&self, endpoint_id: &str) -> Result<SignOutWatch, SdkError> {
        Ok(self.link(parse_id(endpoint_id)?).sign_outs())
    }

    fn link(&self, id: EndpointId) -> Arc<Link> {
        self.inner
            .links
            .lock()
            .expect("never poisoned")
            .entry(id)
            .or_insert_with(|| Link::new(id))
            .clone()
    }

    /// How this device reaches server `endpoint_id` now, without dialling:
    /// for a "Who's watching" screen, before any profile is chosen.
    /// [`ConnectionPath::Unknown`] when there is no connection to it.
    pub fn connection_path(&self, endpoint_id: &str) -> Result<ConnectionPath, SdkError> {
        Ok(self.path_of(parse_id(endpoint_id)?))
    }

    /// The path of the connection to `id` now, without dialling.
    pub(crate) fn path_of(&self, id: EndpointId) -> ConnectionPath {
        self.link(id).path()
    }

    /// What `id` said in the latest connection's Hello.
    pub(crate) fn server_info(&self, id: EndpointId) -> Option<spoolframe_protocol::ServerInfo> {
        self.link(id).info()
    }

    /// One request to `server` in `session`, within its time limit
    /// ([`Timeouts`]); see [`Client::on_connection`] for a connection
    /// that fails under it.
    pub(crate) async fn call(
        &self,
        server: EndpointId,
        session: Option<&SessionId>,
        request: Request,
    ) -> Result<Response, SdkError> {
        let limit = self.inner.timeouts.of(&request);
        let handle = match &request {
            Request::ChangeSubtitle { handle, .. }
            | Request::SeekPlayback { handle, .. }
            | Request::StopPlayback { handle } => Some(handle.clone()),
            _ => None,
        };
        let state = handle
            .as_ref()
            .and_then(|handle| session.map(|session| self.revision_state(server, session, handle)));
        let mut guard = match &state {
            Some(state) => Some(state.lock().await),
            None => None,
        };
        if let (Some(guard), Some(handle), Some(session)) = (&mut guard, &handle, session)
            && guard.is_none()
        {
            **guard = Some(self.fetch_revision(server, session, handle).await?);
        }
        let link = self.link(server);
        self.not_signed_out(&link, session)?;
        let conn = link.connection(self).await?;
        let operation = (!retry_safe(&request)).then(|| Operation {
            id: uuid::Uuid::now_v7().to_string(),
            instance: conn.instance.id.clone(),
            expires_at_ms: conn
                .instance
                .time_ms
                .saturating_add(conn.greeted_at.elapsed().as_millis() as u64)
                .saturating_add(OPERATION_WINDOW_MS - 1000),
        });
        let wire = RpcRequest {
            envelope: spoolframe_protocol::RequestEnvelope {
                session: session.cloned(),
                request,
            },
            operation,
            expected_revision: guard.as_ref().and_then(|guard| **guard),
        };
        // The exact invocation, including its server incarnation and expiry,
        // is reused on a reconnect. A restarted server refuses it.
        let reply = self
            .on_connection(server, session, true, |conn| {
                let wire = &wire;
                async move { conn.call(wire, limit).await }
            })
            .await?;
        if let (Some(revision), Some(session)) = (&reply.playback, session) {
            if let Some(guard) = &mut guard {
                **guard = Some(revision.revision);
            } else {
                *self
                    .revision_state(server, session, &revision.handle)
                    .lock()
                    .await = Some(revision.revision);
            }
        }
        if let Response::Err(spoolframe_protocol::Error::RevisionConflict { revision }) =
            &reply.response
            && let Some(guard) = &mut guard
        {
            **guard = Some(*revision);
        }
        Ok(reply.response)
    }

    fn revision_state(
        &self,
        server: EndpointId,
        session: &SessionId,
        handle: &spoolframe_protocol::PlaybackHandle,
    ) -> RevisionState {
        Self::cached_state(
            &self.inner.revisions,
            (server, session.0.clone(), handle.0.clone()),
        )
    }

    fn cached_state(
        cache: &Mutex<HashMap<PlaybackKey, RevisionState>>,
        key: PlaybackKey,
    ) -> RevisionState {
        let mut revisions = cache.lock().expect("never poisoned");
        // Evict only idle entries: a waiter must keep sharing its control
        // lock. Evicted handles recover their revision from the server.
        if revisions.len() >= 1024 {
            revisions.retain(|_, state| Arc::strong_count(state) > 1);
        }
        revisions.entry(key).or_default().clone()
    }

    pub(crate) async fn progress_sequence(
        &self,
        server: EndpointId,
        session: &SessionId,
        handle: &spoolframe_protocol::PlaybackHandle,
    ) -> Result<(u64, RevisionState), SdkError> {
        let state = Self::cached_state(
            &self.inner.progress_sequences,
            (server, session.0.clone(), handle.0.clone()),
        );
        let mut sequence = state.lock().await;
        let previous = match *sequence {
            Some(sequence) => sequence,
            None => match self
                .call(
                    server,
                    Some(session),
                    Request::PlaybackProgressSequence {
                        handle: handle.clone(),
                    },
                )
                .await?
            {
                Response::Ok(Reply::PlaybackProgressSequence { sequence }) => sequence,
                Response::Err(error) => return Err(SdkError::from_server(error)),
                Response::Ok(other) => {
                    return Err(SdkError::Protocol(format!(
                        "PlaybackProgressSequence answered {other:?}"
                    )));
                }
            },
        };
        let next = previous
            .checked_add(1)
            .ok_or_else(|| SdkError::Protocol("progress sequence exhausted".into()))?;
        *sequence = Some(next);
        drop(sequence);
        Ok((next, state))
    }

    async fn fetch_revision(
        &self,
        server: EndpointId,
        session: &SessionId,
        handle: &spoolframe_protocol::PlaybackHandle,
    ) -> Result<u64, SdkError> {
        let wire = RpcRequest {
            envelope: spoolframe_protocol::RequestEnvelope {
                session: Some(session.clone()),
                request: Request::PlaybackRevision {
                    handle: handle.clone(),
                },
            },
            operation: None,
            expected_revision: None,
        };
        let reply = self
            .on_connection(server, Some(session), true, |conn| {
                let wire = &wire;
                async move { conn.call(wire, self.inner.timeouts.request).await }
            })
            .await?;
        match reply.response {
            Response::Ok(Reply::PlaybackRevision { revision }) => Ok(revision),
            Response::Err(error) => {
                if error == spoolframe_protocol::Error::Unauthenticated {
                    self.end_session(server, session).await?;
                }
                Err(SdkError::from_server(error))
            }
            Response::Ok(other) => Err(SdkError::Protocol(format!(
                "PlaybackRevision answered {other:?}"
            ))),
        }
    }

    pub(crate) async fn playback_revision(
        &self,
        server: EndpointId,
        session: &SessionId,
        handle: &spoolframe_protocol::PlaybackHandle,
    ) -> Result<u64, SdkError> {
        let state = self.revision_state(server, session, handle);
        let mut revision = state.lock().await;
        if let Some(revision) = *revision {
            return Ok(revision);
        }
        let found = self.fetch_revision(server, session, handle).await?;
        *revision = Some(found);
        Ok(found)
    }

    /// Run `exchange` on `server`'s connection. When the connection fails
    /// under it (no answer in time, a broken stream), it is asked for a
    /// `Ping`; unanswered, it is dropped, and the next call dials. Then an
    /// exchange that may be repeated (`retry`) runs once more, on a new
    /// connection. A connection that answers the `Ping` is kept: the
    /// server is alive and slow, and asking it again would not help.
    ///
    /// A connection the server closed because it signed the device out
    /// fails an exchange in `session` with [`SdkError::SignInRequired`],
    /// and the session is ended here. With no session, it is a lost
    /// connection like any other. A session a sign-out ended is not sent
    /// at all.
    pub(crate) async fn on_connection<T, F, Fut>(
        &self,
        server: EndpointId,
        session: Option<&SessionId>,
        retry: bool,
        exchange: F,
    ) -> Result<T, SdkError>
    where
        F: Fn(Arc<ServerConnection>) -> Fut,
        Fut: Future<Output = Result<T, SdkError>>,
    {
        let link = self.link(server);
        self.not_signed_out(&link, session)?;
        let conn = link.connection(self).await?;
        self.not_signed_out(&link, session)?;
        let done = exchange(conn.clone()).await;
        let error = match self.after_sign_out(&link, session, done).await {
            Err(e) if e.is_transport() => e,
            done => return done,
        };
        let dead = self.drop_if_dead(&link, &conn).await;
        if !(dead && retry) {
            return Err(error);
        }
        tracing::debug!(%server, %error, "asking again on a new connection");
        let conn = link.connection(self).await?;
        self.not_signed_out(&link, session)?;
        let again = exchange(conn.clone()).await;
        let again = self.after_sign_out(&link, session, again).await;
        if again.as_ref().is_err_and(SdkError::is_transport) {
            self.drop_if_dead(&link, &conn).await;
        }
        again
    }

    /// [`SdkError::SignInRequired`] if the server signed this device out
    /// of `session`.
    fn not_signed_out(&self, link: &Link, session: Option<&SessionId>) -> Result<(), SdkError> {
        match session {
            Some(session) if link.has_ended(session) => Err(SdkError::SignInRequired),
            _ => Ok(()),
        }
    }

    /// `done`, an exchange's outcome, once a sign-out that cut it off is
    /// handled. An exchange fails with [`SdkError::SignInRequired`] only
    /// when the server closed its connection with `close::SIGNED_OUT`: a
    /// server's `Unauthenticated` is in its answer.
    async fn after_sign_out<T>(
        &self,
        link: &Link,
        session: Option<&SessionId>,
        done: Result<T, SdkError>,
    ) -> Result<T, SdkError> {
        match (done, session) {
            (Err(SdkError::SignInRequired), Some(session)) => {
                link.end([session.0.clone()]);
                self.end_session(link.id(), session).await?;
                Err(SdkError::SignInRequired)
            }
            (Err(SdkError::SignInRequired), None) => Err(SdkError::Unreachable(format!(
                "{}: the connection closed as the device was signed out",
                link.id()
            ))),
            (done, _) => done,
        }
    }

    /// Drop `conn` if it does not answer a `Ping`; whether it was dropped.
    async fn drop_if_dead(&self, link: &Link, conn: &Arc<ServerConnection>) -> bool {
        if conn.answers(self.inner.timeouts.ping).await {
            return false;
        }
        link.drop_dead(conn);
        true
    }

    /// Check the connections after the app was away: call it when the app
    /// comes back to the foreground (iOS and tvOS: on
    /// `sceneWillEnterForeground` or `applicationWillEnterForeground`), and
    /// after the device wakes from sleep or changes network (Linux: logind's
    /// `PrepareForSleep(false)`, a NetworkManager change).
    ///
    /// While the app was suspended, the server may have dropped its side
    /// of a connection that still looks open here. Each open connection is
    /// asked for a `Ping`; one that does not answer within
    /// [`Timeouts::ping`] is dropped and dialled again in the background,
    /// so the next request goes on a working one instead of waiting out
    /// its time limit. Servers with no connection are not dialled. Returns
    /// within about [`Timeouts::ping`]; it is cheap, and safe to call at
    /// any time.
    pub async fn resume(&self) {
        let links: Vec<Arc<Link>> = self
            .inner
            .links
            .lock()
            .expect("never poisoned")
            .values()
            .cloned()
            .collect();
        let ping = self.inner.timeouts.ping;
        let mut checks = tokio::task::JoinSet::new();
        for link in links {
            let Some(conn) = link.open() else {
                continue;
            };
            checks.spawn(async move {
                if !conn.answers(ping).await {
                    link.drop_dead(&conn);
                }
            });
        }
        while checks.join_next().await.is_some() {}
    }

    /// Dial `id`. Once it answers as itself, a saved server's name and
    /// addresses are refreshed; an unsaved one stays unsaved.
    pub(crate) async fn dial(&self, id: EndpointId) -> Result<ServerConnection, SdkError> {
        let saved = servers::load(self.inner.storage.as_ref()).await?;
        let pinned = saved.iter().find(|s| s.endpoint_id == id.to_string());
        let heard = self.inner.discovery.get(&id);
        let manual = self
            .inner
            .manual
            .lock()
            .expect("never poisoned")
            .get(&id)
            .cloned();
        if pinned.is_none() && heard.is_none() && manual.is_none() {
            return Err(SdkError::UnknownServer(id.to_string()));
        }
        // Every address known: iroh tries them all at once.
        let addrs = listed([
            manual.unwrap_or_default(),
            heard.as_ref().map(|h| h.addrs.clone()).unwrap_or_default(),
            pinned.map(SavedServer::addrs).unwrap_or_default(),
        ]);
        let addr = EndpointAddr::from_parts(id, addrs.iter().copied().map(TransportAddr::Ip));
        let conn = ServerConnection::dial(
            &self.inner.endpoint,
            addr,
            &self.inner.device,
            &self.inner.timeouts,
        )
        .await?;
        // Where the server answered. Not what was heard before dialling:
        // when the endpoint resolves a server through mDNS while dialling,
        // that record is not passed on to the discovery subscription, so
        // the heard addresses may be the old ones.
        let mut fresh = conn.remote_addrs();
        if fresh.is_empty() {
            fresh = addrs;
        }
        self.inner.discovery.refresh(&id, &fresh);
        // Saved as seen: only where the connection reached the server, now
        // and later. An address dialled is not one reached, and kept as
        // seen it would never age out.
        let reached = conn.reached.clone();
        let saving = reached.borrow().clone();
        self.refresh(id, &conn.info.name, saving.clone()).await?;
        tokio::spawn(save_reached(
            Arc::downgrade(&self.inner),
            id,
            reached,
            saving,
        ));
        // A device signed in there (the app opened again, a reconnect)
        // asks on each new connection: the server may have gained an
        // address since.
        tokio::spawn(learn_addresses(
            Arc::downgrade(&self.inner),
            id,
            conn.conn.clone(),
            None,
        ));
        Ok(conn)
    }

    /// A saved server's name, as it answered now, that it answered now,
    /// and that it was reached at `addrs`, which join those known. The key
    /// is the match, so a pin is never moved to another key.
    async fn refresh(
        &self,
        id: EndpointId,
        name: &str,
        addrs: Vec<SocketAddr>,
    ) -> Result<(), SdkError> {
        let storage = self.inner.storage.as_ref();
        let _saving = self.inner.saving.lock().await;
        let mut saved = servers::load(storage).await?;
        let Some(server) = saved.iter_mut().find(|s| s.endpoint_id == id.to_string()) else {
            return Ok(());
        };
        server.name = name.to_owned();
        let now = servers::now();
        server.saw(addrs, now);
        server.last_connected_at = Some(now);
        servers::save(storage, &saved).await?;
        self.inner.discovery.changed();
        Ok(())
    }

    /// Saved server `id` was reached at `addrs` just now: they join those
    /// known. Nothing for a server not saved.
    async fn saw(&self, id: EndpointId, addrs: Vec<SocketAddr>) -> Result<(), SdkError> {
        self.sighted(id, addrs, AddrSource::Reached).await
    }

    /// Saved server `id` told this device just now that it has `addrs`
    /// (PX-12): they join those known as told. Nothing for a server not
    /// saved.
    async fn told(&self, id: EndpointId, addrs: Vec<SocketAddr>) -> Result<(), SdkError> {
        self.sighted(id, addrs, AddrSource::Told).await
    }

    /// A session kept for server `id` that a sign-out has not ended, to
    /// ask in.
    async fn kept_session(&self, id: EndpointId) -> Option<SessionId> {
        let link = self.link(id);
        session::load(self.inner.storage.as_ref(), &id)
            .await
            .ok()?
            .into_iter()
            .filter_map(|s| s.token.map(SessionId))
            .find(|token| !link.has_ended(token))
    }

    async fn sighted(
        &self,
        id: EndpointId,
        addrs: Vec<SocketAddr>,
        source: AddrSource,
    ) -> Result<(), SdkError> {
        let storage = self.inner.storage.as_ref();
        let _saving = self.inner.saving.lock().await;
        let mut saved = servers::load(storage).await?;
        let Some(server) = saved.iter_mut().find(|s| s.endpoint_id == id.to_string()) else {
            return Ok(());
        };
        let now = servers::now();
        let new = match source {
            AddrSource::Reached => server.saw(addrs, now),
            AddrSource::Told => server.told(addrs, now),
        };
        servers::save(storage, &saved).await?;
        if new {
            self.inner.discovery.changed();
        }
        Ok(())
    }

    /// This device's identity, as servers see it.
    pub fn id(&self) -> String {
        self.endpoint_id().to_string()
    }

    pub(crate) fn endpoint_id(&self) -> EndpointId {
        self.inner.endpoint.id()
    }

    /// Close the endpoint and every connection on it.
    pub async fn close(&self) {
        let links: Vec<_> = self
            .inner
            .links
            .lock()
            .expect("never poisoned")
            .drain()
            .map(|(_, link)| link)
            .collect();
        for link in links {
            link.close();
        }
        self.inner.endpoint.close().await;
    }
}

/// Save each address a connection to server `id` reaches from now on
/// (`reached`), past those in `saved`, until the connection closes or the
/// client is gone. An address reached before the server is saved is saved
/// with its pin, which reads `reached` under the same lock.
async fn save_reached(
    client: std::sync::Weak<Inner>,
    id: EndpointId,
    mut reached: tokio::sync::watch::Receiver<Vec<SocketAddr>>,
    mut saved: Vec<SocketAddr>,
) {
    while reached.changed().await.is_ok() {
        let new: Vec<SocketAddr> = reached
            .borrow_and_update()
            .iter()
            .filter(|a| !saved.contains(a))
            .copied()
            .collect();
        if new.is_empty() {
            continue;
        }
        let Some(inner) = client.upgrade() else {
            return;
        };
        match (Client { inner }).saw(id, new.clone()).await {
            Ok(()) => saved.extend(new),
            Err(e) => tracing::warn!(server = %id, error = %e, "cannot save where it was reached"),
        }
    }
}

/// Ask server `id` on `conn` where it can be dialled directly (PX-12), in
/// `session`, or in a session kept for it when `None`; the addresses it
/// tells join those saved, as told. A device with no session does not ask:
/// a stranger is not told. Asked on that one connection only, never
/// retried: an older server that does not know the request, a session
/// that ended, a connection lost, each costs only the addresses, which the
/// next connection asks for again.
async fn learn_addresses(
    client: std::sync::Weak<Inner>,
    id: EndpointId,
    conn: iroh::endpoint::Connection,
    session: Option<SessionId>,
) {
    let (session, limit) = {
        let Some(inner) = client.upgrade() else {
            return;
        };
        let client = Client { inner };
        let session = match session {
            Some(session) => session,
            None => match client.kept_session(id).await {
                Some(session) => session,
                None => return,
            },
        };
        (session, client.inner.timeouts.request)
    };
    let addrs = match crate::connection::server_addresses(&conn, &session, limit).await {
        Ok(addrs) => addrs,
        Err(e) => {
            tracing::debug!(server = %id, error = %e, "not told the server's addresses");
            return;
        }
    };
    let Some(inner) = client.upgrade() else {
        return;
    };
    if let Err(e) = (Client { inner }).told(id, addrs).await {
        tracing::warn!(server = %id, error = %e, "cannot save the addresses the server told");
    }
}

/// The addresses of `groups`, in order, each once.
fn listed(groups: impl IntoIterator<Item = Vec<SocketAddr>>) -> Vec<SocketAddr> {
    let mut addrs: Vec<SocketAddr> = Vec::new();
    for addr in groups.into_iter().flatten() {
        if !addrs.contains(&addr) {
            addrs.push(addr);
        }
    }
    addrs
}

fn parse_id(endpoint_id: &str) -> Result<EndpointId, SdkError> {
    EndpointId::from_str(endpoint_id)
        .map_err(|e| SdkError::UnknownServer(format!("{endpoint_id:?} is not a server key: {e}")))
}

/// Changes of the server list; see [`Client::watch_servers`].
#[derive(Debug, Clone)]
pub struct ServersWatch(tokio::sync::watch::Receiver<u64>);

impl ServersWatch {
    /// Waits for the next change; `false` once the client is gone.
    pub async fn changed(&mut self) -> bool {
        self.0.changed().await.is_ok()
    }
}

/// The device's key, made and stored only if storage says there is none.
async fn load_or_create_key(storage: &dyn Storage) -> Result<SecretKey, SdkError> {
    if let Some(bytes) = storage.secret(keys::ENDPOINT_KEY).await? {
        let bytes: [u8; 32] = bytes.try_into().map_err(|b: Vec<u8>| {
            StorageError(format!(
                "the device key is {} bytes, not 32; refusing to replace it",
                b.len()
            ))
        })?;
        return Ok(SecretKey::from_bytes(&bytes));
    }
    let key = SecretKey::generate();
    storage
        .set_secret(keys::ENDPOINT_KEY, Some(key.to_bytes().to_vec()))
        .await?;
    tracing::info!(id = %key.public(), "made this device's key");
    Ok(key)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use super::*;
    use crate::storage::MemoryStorage;

    fn device() -> Device {
        Device {
            name: "test".into(),
            client_name: "spoolframe-sdk-test".into(),
            client_version: "0".into(),
            platform: crate::types::Platform::Cli,
            capabilities: ClientCapabilities::default(),
        }
    }

    /// Heard servers are fingerprinted from their key, saved ones from the
    /// key as iroh prints it: the same code either way.
    #[test]
    fn a_key_and_its_printed_form_have_one_fingerprint() {
        let id = SecretKey::generate().public();
        assert_eq!(
            Some(fingerprint::fingerprint(id.as_bytes())),
            fingerprint::of_hex(&id.to_string())
        );
    }

    #[tokio::test]
    async fn the_key_is_the_same_after_reopening() {
        let storage = Arc::new(MemoryStorage::default());
        let first = Client::open(storage.clone(), device()).await.unwrap();
        let id = first.id();
        first.close().await;
        let again = Client::open(storage, device()).await.unwrap();
        assert_eq!(again.id(), id);
        again.close().await;
    }

    /// Secrets cannot be read; counts writes.
    #[derive(Default)]
    struct Locked {
        writes: AtomicUsize,
    }

    #[async_trait]
    impl Storage for Locked {
        async fn secret(&self, _: &str) -> Result<Option<Vec<u8>>, StorageError> {
            Err(StorageError("the keyring is locked".into()))
        }
        async fn set_secret(&self, _: &str, _: Option<Vec<u8>>) -> Result<(), StorageError> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn data(&self, _: &str) -> Result<Option<Vec<u8>>, StorageError> {
            Ok(None)
        }
        async fn set_data(&self, _: &str, _: Option<Vec<u8>>) -> Result<(), StorageError> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn unreadable_storage_is_an_error_and_no_key_is_made() {
        let storage = Arc::new(Locked::default());
        let err = Client::open(storage.clone(), device()).await.unwrap_err();
        assert_eq!(
            err,
            SdkError::Storage(StorageError("the keyring is locked".into()))
        );
        assert_eq!(storage.writes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_damaged_key_is_an_error_not_a_new_key() {
        let storage = Arc::new(MemoryStorage::default());
        storage
            .set_secret(keys::ENDPOINT_KEY, Some(vec![1, 2, 3]))
            .await
            .unwrap();
        assert!(matches!(
            Client::open(storage.clone(), device()).await,
            Err(SdkError::Storage(_))
        ));
        assert_eq!(
            storage.secret(keys::ENDPOINT_KEY).await.unwrap(),
            Some(vec![1, 2, 3])
        );
    }
}
