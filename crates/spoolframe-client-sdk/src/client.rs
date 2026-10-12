//! This device: its identity and its endpoint (ARCHITECTURE §13–17).

use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;

use iroh::endpoint::{QuicTransportConfig, presets};
use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayMap, RelayMode, RelayUrl, SecretKey, TransportAddr,
};
use iroh_mdns_address_lookup::MdnsAddressLookup;
use std::time::Duration;

use spoolframe_protocol::{
    FinishPasskeySignIn, LoginPassword, PasskeyRequest, Reply, Request, Response, ServerId,
    SessionId, TrustedProfile, device_sign_in, fingerprint, lan,
};
use spoolframe_types::{ClientCapabilities, Platform};

use crate::connection::{ServerConnection, Timeouts};
use crate::discovery::Discovery;
use crate::error::SdkError;
use crate::link::{ConnectionPath, Link, SignOutWatch, StateWatch};
use crate::portable::{Portable, PortableUserId};
use crate::reachability::{
    self, DialReport, KnownPath, LivePath, PathKind, ReachLog, Reachability,
};
use crate::relay_policy::{RelayFiltered, RelayPolicy};
use crate::servers::{self, SavedServer, ServerEntry};
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
/// How much the server may send on one stream before this device reads
/// it (P6-06): a direct read's whole block (`reader::BLOCK`). QUIC's
/// default, 1.25 MB, lets a stream carry only that per round trip, about
/// 33 Mbit/s at 300 ms, a relay's or another continent's: less than a
/// remux needs, however few reads are in flight.
const STREAM_WINDOW: u32 = 8 << 20;

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

/// How [`Client::open_with_options`] opens this device. The default is
/// what [`Client::open`] does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientOptions {
    /// How long requests wait for answers.
    pub timeouts: Timeouts,
    /// Listen for servers on the LAN (mDNS). On by default. Off, nothing
    /// is heard: [`Client::servers`] lists the saved servers only, and a
    /// server is dialled only at the addresses saved for it or given with
    /// [`Client::add_address`]. For tests, and for any app that must not
    /// depend on what else is on the network.
    pub lan_discovery: bool,
    /// Reach a server through the relays it told this device of, in an
    /// invite or its addresses (P6-03), when it cannot be dialled
    /// directly. On by default. Off, a server is reached only at its
    /// addresses and no relay is contacted for it, unless the server has a
    /// choice of its own ([`Client::set_server_relays`]; redeeming or
    /// saving an invite whose link names relays chooses them: accepting
    /// the invite is consent to that server's relays). It is fixed for the
    /// life of the [`Client`]: an app whose user changes it opens a new
    /// one.
    pub relays: bool,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            timeouts: Timeouts::default(),
            lan_discovery: true,
            relays: true,
        }
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
    pub(crate) discovery: Discovery,
    /// Addresses the app gave for servers, and invites' hints (P6-01).
    pub(crate) manual: Mutex<HashMap<EndpointId, Vec<SocketAddr>>>,
    /// Invites' relays, for a server not saved yet (P6-03).
    pub(crate) invite_relays: Mutex<HashMap<EndpointId, Vec<String>>>,
    /// The pkarr relays servers are looked up at (P6-05).
    lookups: Mutex<std::collections::HashSet<String>>,
    /// [`ClientOptions::relays`].
    pub(crate) relays: bool,
    /// Which servers are reached through relays: their choices, and
    /// [`ClientOptions::relays`] for the rest.
    pub(crate) relay_policy: Arc<RelayPolicy>,
    /// What lookups answered and how dials went, for
    /// [`Client::reachability`].
    pub(crate) reach: Arc<ReachLog>,
    /// [`ClientOptions::lan_discovery`]: mDNS is one of the lookups.
    lan_discovery: bool,
    /// Each server's connection and its state.
    links: Mutex<HashMap<EndpointId, Arc<Link>>>,
    /// Serializes read-modify-write of the saved servers and sessions.
    pub(crate) saving: tokio::sync::Mutex<()>,
    revisions: Mutex<HashMap<PlaybackKey, RevisionState>>,
    progress_sequences: Mutex<HashMap<PlaybackKey, RevisionState>>,
    activating: tokio::sync::Mutex<()>,
    /// The portable identities on this device, over the same storage.
    portable: Portable,
    /// A pairing code is shown (P7-03): one at a time.
    pub(crate) pairing: Arc<std::sync::atomic::AtomicBool>,
    /// Refusals on this device, as logged (finding 46).
    pub(crate) refusals: Arc<crate::refusals::Refusals>,
    /// Playbacks whose share of a server's uplink changed (PX-46), as the
    /// servers told: each managed playback in Auto follows its own.
    pub(crate) shares: tokio::sync::broadcast::Sender<crate::PlaybackShare>,
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
    /// an endpoint that listens for servers on the LAN, never announces
    /// itself, and reaches a server through the relays it told (P6-03).
    /// Requests wait for answers as [`Timeouts::default`] says.
    /// [`Client::open_with_options`] can turn the listening or the relays
    /// off.
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
        let options = ClientOptions {
            timeouts,
            ..ClientOptions::default()
        };
        Self::open_with_options(storage, device, options).await
    }

    /// [`Client::open`], as `options` say: its timeouts, and whether it
    /// listens for servers on the LAN.
    pub async fn open_with_options(
        storage: Arc<dyn Storage>,
        device: Device,
        options: ClientOptions,
    ) -> Result<Self, SdkError> {
        let ClientOptions {
            timeouts,
            lan_discovery,
            relays,
        } = options;
        let relay_policy = Arc::new(RelayPolicy::new(relays));
        let reach = Arc::new(ReachLog::default());
        let key = load_or_create_key(storage.as_ref()).await?;
        // Minimal: no pkarr or DNS lookup, so a LAN client never tries the
        // Internet. mDNS only listens: a laptop or a TV must not announce
        // itself to the network (P1-13).
        let cannot_bind =
            |e: String| SdkError::Unreachable(format!("cannot bind the endpoint: {e}"));
        let mdns = if lan_discovery {
            let mdns = MdnsAddressLookup::builder()
                .advertise(false)
                .service_name(lan::MDNS_SERVICE)
                .build(key.public())
                .map_err(|e| cannot_bind(e.to_string()))?;
            Some(mdns)
        } else {
            None
        };
        let transport = QuicTransportConfig::builder()
            .keep_alive_interval(KEEP_ALIVE)
            .stream_receive_window(STREAM_WINDOW.into())
            .max_idle_timeout(Some(
                IDLE_TIMEOUT
                    .try_into()
                    .expect("15 s is a valid idle timeout"),
            ))
            .build();
        // An empty map, relays on or off: no home relay of its own, and so
        // no relay contacted at start or ever probed. Iroh reaches any
        // relay a server is dialled through; the server's own list is
        // where it can be reached. Whether a server is dialled through
        // its relays is its own choice or the client's (`RelayPolicy`):
        // a disabled relay transport would make that choice for every
        // server, and an invite could not be redeemed through its relays
        // on a device with them off.
        let endpoint = Endpoint::builder(presets::Minimal)
            .secret_key(key)
            .relay_mode(RelayMode::Custom(RelayMap::empty()))
            .transport_config(transport)
            .bind()
            .await
            .map_err(|e| cannot_bind(e.to_string()))?;
        let discovery = match mdns {
            Some(mdns) => {
                // The same lookup finds the addresses of servers being
                // dialled.
                endpoint
                    .address_lookup()
                    .map_err(|e| cannot_bind(e.to_string()))?
                    .add(RelayFiltered::new(
                        mdns.clone(),
                        relay_policy.clone(),
                        MDNS_LOOKUP,
                        reach.clone(),
                    ));
                Discovery::start(&mdns).await
            }
            None => Discovery::off(),
        };
        let client = Self {
            inner: Arc::new(Inner {
                endpoint,
                storage: storage.clone(),
                device,
                timeouts,
                discovery,
                manual: Mutex::default(),
                invite_relays: Mutex::default(),
                lookups: Mutex::default(),
                relays,
                relay_policy,
                reach,
                lan_discovery,
                links: Mutex::default(),
                saving: tokio::sync::Mutex::default(),
                revisions: Mutex::default(),
                progress_sequences: Mutex::default(),
                activating: tokio::sync::Mutex::default(),
                portable: Portable::new(storage.clone()),
                pairing: Arc::default(),
                refusals: Arc::default(),
                shares: tokio::sync::broadcast::channel(16).0,
            }),
        };
        // Where saved servers publish (P6-05): looked up there from the
        // start; and each one's relay choice.
        for server in servers::load(client.inner.storage.as_ref()).await? {
            if let Some(url) = &server.lookup {
                client.look_up_at(url);
            }
            client.look_up_fallback(&server);
            if let Ok(id) = parse_id(&server.endpoint_id) {
                client.inner.relay_policy.choose(id, server.use_relays);
            }
        }
        Ok(client)
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
        // Only a pin with no server id yet is matched by name: one with an
        // id is matched by that (PX-14), and a name may change freely.
        let away: Vec<String> = saved
            .iter()
            .filter(|s| s.server_id.is_none())
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
                    server_id: s.server_id,
                    endpoint_id: s.endpoint_id,
                    name: s.name,
                    pinned: true,
                    verified: s.verified,
                    on_network: on_lan.is_some(),
                    addrs,
                    advertised_at: servers::time(s.advertised_at),
                    advertised_addrs: s.advertised_addrs,
                    relays: s.relays,
                    relays_edited: s.relays_edited,
                    use_relays: s.use_relays,
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
                advertised_addrs: Vec::new(),
                relays: Vec::new(),
                advertised_at: None,
                pinned: false,
                verified: false,
                on_network: true,
                pinned_at: None,
                verified_at: None,
                last_connected_at: None,
                server_id: None,
                use_relays: None,
                relays_edited: false,
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

    /// Every change of a playback's share of its server's uplink, from
    /// every server this device is connected to (PX-46), as the server
    /// tells it: the handle and the most it may take now, bits/s. A
    /// managed playback in Auto plans itself again on its own; this is for
    /// showing it ([`crate::ManagedPlayback::share`] holds a playback's
    /// last). Changes before the call are not seen.
    pub fn playback_shares(&self) -> tokio::sync::broadcast::Receiver<crate::PlaybackShare> {
        self.inner.shares.subscribe()
    }

    /// Where to find server `endpoint_id`, from outside the LAN's records:
    /// typed in, or from an invite (P6-01). A hint like any other: the key is
    /// still proven on connect.
    pub fn add_address(&self, endpoint_id: &str, addrs: Vec<SocketAddr>) -> Result<(), SdkError> {
        let id = parse_id(endpoint_id)?;
        self.inner.manual.lock().insert(id, addrs);
        Ok(())
    }

    /// Connect to server `endpoint_id`, or use the open connection, and say
    /// who answered: its name and its key's fingerprint. A server not saved
    /// yet must have been heard on the LAN or given an address, and is not
    /// saved by this: the app shows the fingerprint, then calls
    /// [`Client::pin`] (P1-21). Until then only `Hello` is sent to it.
    ///
    /// The pin is the pair of the key and the server id (PX-14): a saved
    /// server connects only when both answer as pinned, and the name it
    /// answers with is saved, whatever it is.
    ///
    /// [`SdkError::IdentityChanged`] when a saved server seems to answer
    /// with another key (P1-13): `endpoint_id` answers with a saved
    /// server's server id. For a server saved before the SDK kept ids,
    /// also when `endpoint_id` is heard under its name while its own key
    /// is not, or when it cannot be reached and another key is heard
    /// under its name. Nothing is saved; [`Client::trust_new_identity`]
    /// replaces the pin if the user says so.
    ///
    /// [`SdkError::ServerIdChanged`] when a saved server answers under its
    /// key with another server id: refused, and nothing is saved. A key
    /// and a server id that are both new are another server: nothing
    /// pinned applies to it.
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
        if pinned.is_none()
            && let Some(old) = same_server(&saved, id, &conn.info.server_id)
        {
            return Err(SdkError::IdentityChanged {
                saved: old.endpoint_id.clone(),
                now: endpoint_id.to_owned(),
            });
        }
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
                    server_id: server.server_id.clone(),
                    advertised_addrs: server.advertised_addrs.clone(),
                    relays: server.relays.clone(),
                    advertised_at: servers::time(server.advertised_at),
                    use_relays: server.use_relays,
                    relays_edited: server.relays_edited,
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
            conn.info.server_id.clone(),
            conn.reached(),
            verified,
        );
        let mut saved = servers::load(storage).await?;
        // Nor this way, by a new key that says it is a saved server.
        if let Some(old) = same_server(&saved, id, &conn.info.server_id) {
            return Err(SdkError::IdentityChanged {
                saved: old.endpoint_id.clone(),
                now: endpoint_id.to_owned(),
            });
        }
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
    ///
    /// The new pin is `new_id` with the server id it answers with. When
    /// the server only replaced its key, that is the id pinned before
    /// (PX-14), so its app passkeys, made for that id, still sign in: the
    /// SDK makes their challenges with the new key from now on.
    pub async fn trust_new_identity(
        &self,
        saved_id: &str,
        new_id: &str,
    ) -> Result<ServerEntry, SdkError> {
        let old = parse_id(saved_id)?;
        let new = parse_id(new_id)?;
        let conn = self.link(new).connection(self).await?;
        if let Some(link) = self.inner.links.lock().remove(&old) {
            link.close();
        }
        let storage = self.inner.storage.as_ref();
        let _saving = self.inner.saving.lock().await;
        // The user checked the new key's fingerprint to get here.
        let server = SavedServer::pinned_now(
            conn.info.name.clone(),
            new_id.to_owned(),
            conn.info.server_id.clone(),
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

    /// A saved server with no server id pinned yet that `id` answers in
    /// place of: `id` is heard under its name, and its own key is not
    /// heard. One with an id is matched by it ([`same_server`]).
    fn replaced_by<'a>(&self, saved: &'a [SavedServer], id: EndpointId) -> Option<&'a SavedServer> {
        let heard = self.inner.discovery.get(&id)?;
        saved.iter().find(|s| {
            s.server_id.is_none()
                && s.name == heard.name
                && s.endpoint_id != id.to_string()
                && parse_id(&s.endpoint_id)
                    .map_or(true, |own| self.inner.discovery.get(&own).is_none())
        })
    }

    /// Another key heard under saved server `server`'s name while its own is
    /// not heard, if `server` has no server id pinned yet: a name says
    /// nothing about one that has (PX-14).
    fn replacement(&self, server: &SavedServer) -> Option<EndpointId> {
        if server.server_id.is_some() {
            return None;
        }
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
    pub(crate) fn entry(
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
            server_id: Some(conn.info.server_id.clone()),
            advertised_addrs: saved
                .map(|s| s.advertised_addrs.clone())
                .unwrap_or_default(),
            relays: saved.map(|s| s.relays.clone()).unwrap_or_default(),
            advertised_at: saved.and_then(|s| servers::time(s.advertised_at)),
            use_relays: saved.and_then(|s| s.use_relays),
            relays_edited: saved.is_some_and(|s| s.relays_edited),
        }
    }

    /// Where the open connection to `id` reaches it, the path in use
    /// first; nothing without one. Never dials.
    fn in_use(&self, id: EndpointId) -> Vec<SocketAddr> {
        let link = self.inner.links.lock().get(&id).cloned();
        link.and_then(|l| l.open())
            .map(|conn| conn.remote_addrs())
            .unwrap_or_default()
    }

    /// The server id pinned with saved server `id`'s key (PX-14), which
    /// names its app passkeys' relying party. Every connection to a saved
    /// server pins it, or refuses one that differs, so after a call to it
    /// there is one.
    pub(crate) async fn pinned_server_id(&self, id: EndpointId) -> Result<ServerId, SdkError> {
        let saved = servers::load(self.inner.storage.as_ref()).await?;
        let server_id = saved
            .into_iter()
            .find(|s| s.endpoint_id == id.to_string())
            .and_then(|s| s.server_id)
            .ok_or_else(|| SdkError::NotPinned(id.to_string()))?;
        ServerId::parse(&server_id).map_err(|e| SdkError::Protocol(e.to_string()))
    }

    /// Requests other than `Hello` go only to a pinned server (P1-21).
    pub(crate) async fn require_pinned(&self, id: EndpointId) -> Result<(), SdkError> {
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
        if let Some(link) = self.inner.links.lock().remove(&id) {
            link.close();
        }
        let storage = self.inner.storage.as_ref();
        let _saving = self.inner.saving.lock().await;
        let mut saved = servers::load(storage).await?;
        saved.retain(|s| s.endpoint_id != endpoint_id);
        servers::save(storage, &saved).await?;
        self.inner.relay_policy.choose(id, None);
        storage
            .set_secret(&keys::sessions(endpoint_id), None)
            .await?;
        self.inner.discovery.changed();
        Ok(())
    }

    /// Whether saved server `endpoint_id` is reached through relays: `Some`
    /// whatever [`ClientOptions::relays`] says, `None` as it says. Saved
    /// with the server, and used from the next connection: an open one is
    /// closed, and the next request dials again. [`SdkError::NotPinned`]
    /// for a server not saved.
    ///
    /// Off, the server is dialled at its addresses only, and an address
    /// lookup's record of it loses its relays. A path iroh opened through
    /// a relay earlier in this run may still be known to it; nothing
    /// dials a new one.
    pub async fn set_server_relays(
        &self,
        endpoint_id: &str,
        relays: Option<bool>,
    ) -> Result<(), SdkError> {
        let id = parse_id(endpoint_id)?;
        {
            let storage = self.inner.storage.as_ref();
            let _saving = self.inner.saving.lock().await;
            let mut saved = servers::load(storage).await?;
            let Some(server) = saved.iter_mut().find(|s| s.endpoint_id == endpoint_id) else {
                return Err(SdkError::NotPinned(endpoint_id.to_owned()));
            };
            if server.use_relays == relays {
                return Ok(());
            }
            server.use_relays = relays;
            self.look_up_fallback(server);
            servers::save(storage, &saved).await?;
        }
        self.inner.relay_policy.choose(id, relays);
        if let Some(link) = self.inner.links.lock().remove(&id) {
            link.close();
        }
        self.inner.discovery.changed();
        Ok(())
    }

    /// Saved server `endpoint_id`'s relays from now on: those it is
    /// dialled through while its relays are on. Empty gives the list back
    /// to the server (its next `ServerAddresses` event, and an invite's
    /// link, fill it); otherwise it is the user's, and neither changes it.
    /// Used from the next connection: an open one is closed.
    /// [`SdkError::InvalidInput`] for one that is not an http(s) URL;
    /// [`SdkError::NotPinned`] for a server not saved. At most
    /// [`spoolframe_protocol::invite::MAX_HINTS`] are kept.
    pub async fn set_server_relay_urls(
        &self,
        endpoint_id: &str,
        relays: Vec<String>,
    ) -> Result<(), SdkError> {
        let id = parse_id(endpoint_id)?;
        {
            let storage = self.inner.storage.as_ref();
            let _saving = self.inner.saving.lock().await;
            let mut saved = servers::load(storage).await?;
            let Some(server) = saved.iter_mut().find(|s| s.endpoint_id == endpoint_id) else {
                return Err(SdkError::NotPinned(endpoint_id.to_owned()));
            };
            if !server.edit_relays(relays) {
                return Err(SdkError::InvalidInput(
                    "a relay is an http:// or https:// URL".into(),
                ));
            }
            self.look_up_fallback(server);
            servers::save(storage, &saved).await?;
        }
        if let Some(link) = self.inner.links.lock().remove(&id) {
            link.close();
        }
        self.inner.discovery.changed();
        Ok(())
    }

    /// Sign `username` in on server `endpoint_id` with a password (§22),
    /// and keep the session. A one-time password an admin reset to gives a
    /// session whose [`Session::password_change_required`] is true: show
    /// "Choose a new password" and call [`Session::change_password`] with
    /// the one-time password as the current one; until then every other
    /// request of the session's is [`SdkError::PasswordChangeRequired`]. With `remember` ("Remember this device",
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
        let (token, remembered, password_change_required) =
            match self.call(server, None, login).await? {
                Response::Ok(Reply::LoggedIn {
                    session,
                    remembered,
                    password_change_required,
                }) => (session, remembered, password_change_required),
                Response::Ok(other) => {
                    return Err(SdkError::Protocol(format!("login answered {other:?}")));
                }
                Response::Err(e) => return Err(SdkError::from_server(e)),
            };
        let mut session = self
            .keep_session(server, token, Some(SignInMethod::Password), remembered)
            .await?;
        session.password_change_required = password_change_required;
        Ok(session)
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
                ..
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
    /// party in [`PasskeyRequest::rp_id`], from [`PasskeyRequest::origin`]
    /// where the platform takes one. That is the server's own,
    /// `<server id>.spoolframe.app`, from the server id pinned with its key
    /// (PX-14): pass it to the platform as it is, never a fixed one. No
    /// passkeys are listed: the platform offers whichever it has for this
    /// relying party, which are only this server's. With `username`, only
    /// that user's finish it. The challenge is this device's, for 5
    /// minutes; beginning again replaces it.
    ///
    /// The server sends a nonce; the challenge is
    /// [`spoolframe_protocol::app_passkey_challenge`] of `endpoint_id`, the
    /// pinned server dialled, and that nonce (PX-10). A server that claims
    /// another's server id and hands on its nonce gets a signature the
    /// other refuses.
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
                let server_id = self.pinned_server_id(server).await?;
                Ok(begun.for_server(server.as_bytes(), &server_id))
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
    ///
    /// `signed.user_handle` is `None` when the platform gave no user
    /// handle; an empty one is sent as `None` too, which the server takes
    /// (it refuses empty bytes).
    pub async fn finish_passkey_sign_in(
        &self,
        endpoint_id: &str,
        signed: FinishPasskeySignIn,
        remember: bool,
    ) -> Result<Session, SdkError> {
        let server = parse_id(endpoint_id)?;
        self.require_pinned(server).await?;
        let signed = without_empty_user_handle(signed);
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
                ..
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
                web_url: web_page(web_url),
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
                    ..
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
    pub(crate) async fn keep_session(
        &self,
        server: EndpointId,
        token: SessionId,
        method: Option<SignInMethod>,
        remembered: bool,
    ) -> Result<Session, SdkError> {
        self.keep_session_as(server, token, method, remembered, None)
            .await
    }

    /// [`Client::keep_session`], signed in with portable identity
    /// `portable` if `Some` (P7-06): kept with it, to sign in again.
    pub(crate) async fn keep_session_as(
        &self,
        server: EndpointId,
        token: SessionId,
        method: Option<SignInMethod>,
        remembered: bool,
        portable: Option<PortableUserId>,
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
            portable,
        };
        match (
            method,
            stored.iter_mut().find(|s| s.username == user.username),
        ) {
            // A resume: the same profile, in its place.
            (None, Some(old)) => {
                *old = StoredSession {
                    method: old.method,
                    portable: old.portable,
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
        Ok(Session {
            client: self.clone(),
            server,
            token,
            user,
            remembered: Some(remembered),
            password_change_required: false,
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
                    password_change_required: false,
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

    pub(crate) fn link(&self, id: EndpointId) -> Arc<Link> {
        self.inner
            .links
            .lock()
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
            | Request::ChangeQuality { handle, .. }
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
        let mut revisions = cache.lock();
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
        let links: Vec<Arc<Link>> = self.inner.links.lock().values().cloned().collect();
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

    /// The relays to reach `id` through: none with its relays off
    /// ([`crate::relay_policy::RelayPolicy`]), else those it told (saved
    /// with `pinned`) and an invite's. One that is not a URL is left out:
    /// what a server tells is not trusted to be one.
    fn relays_of(&self, id: EndpointId, pinned: Option<&SavedServer>) -> Vec<RelayUrl> {
        if !self.inner.relay_policy.allows(&id) {
            return Vec::new();
        }
        let invited = self.inner.invite_relays.lock().get(&id).cloned();
        let mut urls: Vec<RelayUrl> = Vec::new();
        let told = pinned.map(|s| s.relays.clone()).unwrap_or_default();
        for relay in told.iter().chain(invited.iter().flatten()) {
            match relay.parse::<RelayUrl>() {
                Ok(url) if !urls.contains(&url) => urls.push(url),
                Ok(_) => {}
                Err(e) => tracing::debug!(relay, error = %e, "not a relay URL; left out"),
            }
        }
        urls
    }

    /// Dial `id`. Once it answers as itself, a saved server's name and
    /// addresses are refreshed; an unsaved one stays unsaved.
    pub(crate) async fn dial(&self, id: EndpointId) -> Result<ServerConnection, SdkError> {
        let saved = servers::load(self.inner.storage.as_ref()).await?;
        let pinned = saved.iter().find(|s| s.endpoint_id == id.to_string());
        let heard = self.inner.discovery.get(&id);
        let manual = self.inner.manual.lock().get(&id).cloned();
        // An invite with only relays names a server too (P6-03), dialled
        // through them whatever the client's relays (see `reach`).
        let invited = self.inner.invite_relays.lock().contains_key(&id);
        if pinned.is_none() && heard.is_none() && manual.is_none() && !invited {
            return Err(SdkError::UnknownServer(id.to_string()));
        }
        // Every address known: iroh tries them all at once.
        let addrs = listed([
            manual.unwrap_or_default(),
            heard.as_ref().map(|h| h.addrs.clone()).unwrap_or_default(),
            pinned.map(SavedServer::addrs).unwrap_or_default(),
        ]);
        let relays = self.relays_of(id, pinned);
        let addr = EndpointAddr::from_parts(
            id,
            addrs
                .iter()
                .copied()
                .map(TransportAddr::Ip)
                .chain(relays.into_iter().map(TransportAddr::Relay)),
        );
        let dialled = addr
            .addrs
            .iter()
            .filter_map(PathKind::of)
            .map(|(_, addr)| addr)
            .take(reachability::MAX_LISTED)
            .collect();
        let (at, started) = (servers::now(), std::time::Instant::now());
        let dial = ServerConnection::dial(
            &self.inner.endpoint,
            addr,
            &self.inner.device,
            &self.inner.timeouts,
        )
        .await;
        let failed = dial.as_ref().err();
        let report = DialReport {
            at,
            took_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
            connected: failed.is_none(),
            stage: failed.map(|f| f.stage),
            error: failed.map(|f| f.error.to_string()),
            dialled,
            known: self.known_paths(id).await,
        };
        if let Some(failed) = failed {
            tracing::debug!(server = %id, stage = ?failed.stage, error = %failed.error, "dial failed");
        }
        self.inner.reach.dialled(id, report);
        let conn = dial.map_err(|f| f.error)?;
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
        if let Err(e) = self
            .refresh(id, &conn.info.name, &conn.info.server_id, saving.clone())
            .await
        {
            conn.conn.close(0u32.into(), b"not the server pinned");
            return Err(e);
        }
        tokio::spawn(save_reached(
            Arc::downgrade(&self.inner),
            id,
            reached,
            saving,
        ));
        Ok(conn)
    }

    /// A saved server's name, as it answered now, that it answered now,
    /// and that it was reached at `addrs`, which join those known. The key
    /// is the match, so a pin is never moved to another key.
    ///
    /// `server_id` is the server id it answered with: one pinned before
    /// must be it, or nothing is kept and the answer is
    /// [`SdkError::ServerIdChanged`]. A pin from before the SDK kept ids
    /// takes it now, as a first connect does.
    async fn refresh(
        &self,
        id: EndpointId,
        name: &str,
        server_id: &str,
        addrs: Vec<SocketAddr>,
    ) -> Result<(), SdkError> {
        let storage = self.inner.storage.as_ref();
        let _saving = self.inner.saving.lock().await;
        let mut saved = servers::load(storage).await?;
        let Some(server) = saved.iter_mut().find(|s| s.endpoint_id == id.to_string()) else {
            return Ok(());
        };
        match &server.server_id {
            Some(pinned) if pinned != server_id => {
                tracing::warn!(
                    server = %id,
                    pinned,
                    now = server_id,
                    "a saved server answered with another server id; refused"
                );
                return Err(SdkError::ServerIdChanged {
                    endpoint_id: id.to_string(),
                    saved: pinned.clone(),
                    now: server_id.to_owned(),
                });
            }
            Some(_) => {}
            None => server.server_id = Some(server_id.to_owned()),
        }
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
        self.sighted(id, |server, now| server.saw(addrs, now)).await
    }

    /// Saved server `id` told this device just now where it is (PX-12,
    /// the `ServerAddresses` event): `addrs` replace the addresses it told
    /// before, `relays` the relays, and `lookup` where it publishes
    /// (P6-05), which this device looks servers up at from now on. Nothing
    /// for a server not saved.
    pub(crate) async fn told(
        &self,
        id: EndpointId,
        addrs: Vec<SocketAddr>,
        relays: Vec<String>,
        lookup: Option<String>,
    ) -> Result<(), SdkError> {
        self.sighted(id, |server, now| {
            server.told_relays(relays, lookup);
            if let Some(url) = &server.lookup {
                self.look_up_at(url);
            }
            self.look_up_fallback(server);
            server.told(addrs, now)
        })
        .await
    }

    /// Look servers up at the pkarr relay `url` (P6-05) from now on, once:
    /// iroh asks it for a server's record by its key when what this device
    /// knows does not reach the server. A record is a hint only: the
    /// connection still proves the key.
    pub(crate) fn look_up_at(&self, url: &str) {
        let Ok(parsed) = url.parse::<url::Url>() else {
            tracing::debug!(url, "not a pkarr relay's URL; not looked up at");
            return;
        };
        if !admit_lookup(&mut self.inner.lookups.lock(), url) {
            return;
        }
        let endpoint = &self.inner.endpoint;
        let Ok(services) = endpoint.address_lookup() else {
            return;
        };
        services.add(RelayFiltered::new(
            iroh::address_lookup::PkarrResolver::builder(parsed)
                .build(endpoint.tls_config().clone()),
            self.inner.relay_policy.clone(),
            url,
            self.inner.reach.clone(),
        ));
        tracing::debug!(url, "servers are looked up here too");
    }

    /// Every path iroh knows of for `id` now, from what it was given and
    /// what lookups found, and whether a connection uses it.
    async fn known_paths(&self, id: EndpointId) -> Vec<KnownPath> {
        let Some(info) = self.inner.endpoint.remote_info(id).await else {
            return Vec::new();
        };
        info.addrs()
            .filter_map(|a| {
                let (kind, addr) = PathKind::of(a.addr())?;
                Some(KnownPath {
                    kind,
                    addr,
                    active: matches!(a.usage(), iroh::endpoint::TransportAddrUsage::Active),
                })
            })
            .take(reachability::MAX_LISTED)
            .collect()
    }

    /// How this device stands with server `endpoint_id` now, for an app's
    /// "can't be reached" details: what it knows of the server, what each
    /// lookup last answered, how its last dial in this run went, and the
    /// paths of the connection open now. Never dials.
    ///
    /// Iroh does not say how each path of a failed dial fared: a dial's
    /// [`DialReport::known`] lists the paths it knew of, and its stage is
    /// where the dial as a whole stopped.
    ///
    /// [`SdkError::UnknownServer`] for a server this device knows nothing
    /// of: not saved, not heard on the LAN, named by no address or invite,
    /// and never dialled.
    pub async fn reachability(&self, endpoint_id: &str) -> Result<Reachability, SdkError> {
        let id = parse_id(endpoint_id)?;
        let saved = servers::load(self.inner.storage.as_ref()).await?;
        let pinned = saved.iter().find(|s| s.endpoint_id == endpoint_id);
        let heard = self.inner.discovery.get(&id);
        let hinted: Vec<String> = self
            .inner
            .manual
            .lock()
            .get(&id)
            .map(|a| a.iter().map(ToString::to_string).collect())
            .unwrap_or_default();
        let invite_relays = self
            .inner
            .invite_relays
            .lock()
            .get(&id)
            .cloned()
            .unwrap_or_default();
        let last_dial = self.inner.reach.last_dial(&id);
        let link = self.inner.links.lock().get(&id).cloned();
        if pinned.is_none()
            && heard.is_none()
            && hinted.is_empty()
            && invite_relays.is_empty()
            && last_dial.is_none()
            && link.is_none()
        {
            return Err(SdkError::UnknownServer(endpoint_id.to_owned()));
        }
        let mut lookups: Vec<String> = Vec::new();
        if self.inner.lan_discovery {
            lookups.push(MDNS_LOOKUP.to_owned());
        }
        let mut pkarr: Vec<String> = self.inner.lookups.lock().iter().cloned().collect();
        pkarr.sort();
        lookups.extend(pkarr);
        let live = link
            .as_ref()
            .and_then(|l| l.open())
            .map(|conn| live_paths(&conn.conn))
            .unwrap_or_default();
        let name = pinned
            .map(|s| s.name.clone())
            .or_else(|| link.as_ref().and_then(|l| l.info()).map(|i| i.name))
            .or_else(|| heard.as_ref().map(|h| h.name.clone()));
        Ok(Reachability {
            endpoint_id: endpoint_id.to_owned(),
            name,
            saved: pinned.is_some(),
            addrs: pinned.map(reachability::saved_addrs).unwrap_or_default(),
            advertised_addrs: pinned
                .map(|s| s.advertised_addrs.iter().map(ToString::to_string).collect())
                .unwrap_or_default(),
            advertised_at: pinned.and_then(|s| s.advertised_at),
            relays: pinned.map(|s| s.relays.clone()).unwrap_or_default(),
            relays_edited: pinned.is_some_and(|s| s.relays_edited),
            lookup: pinned.and_then(|s| s.lookup.clone()),
            use_relays: pinned.and_then(|s| s.use_relays),
            relays_effective: self.inner.relay_policy.allows(&id),
            heard_on_lan: heard
                .map(|h| h.addrs.iter().map(ToString::to_string).collect())
                .unwrap_or_default(),
            hinted_addrs: hinted,
            invite_relays,
            lookups,
            lookup_results: self.inner.reach.lookups_of(&id),
            last_dial,
            last_connected_at: pinned.and_then(|s| s.last_connected_at),
            live,
        })
    }

    /// Look servers up at n0 too when saved `server` has no other way to
    /// it off the LAN ([`SavedServer::needs_n0_lookup`]): one saved while
    /// its relays and record were off, which turned them on since, is
    /// found again if it publishes there, without first reaching it on
    /// the LAN to hear its new relays.
    pub(crate) fn look_up_fallback(&self, server: &SavedServer) {
        if server.needs_n0_lookup(self.inner.relays) {
            self.look_up_at(servers::N0_LOOKUP);
        }
    }

    /// Change saved server `id` with `change`, given the time now, which
    /// says whether its addresses changed.
    async fn sighted(
        &self,
        id: EndpointId,
        change: impl FnOnce(&mut SavedServer, u64) -> bool,
    ) -> Result<(), SdkError> {
        let storage = self.inner.storage.as_ref();
        let _saving = self.inner.saving.lock().await;
        let mut saved = servers::load(storage).await?;
        let Some(server) = saved.iter_mut().find(|s| s.endpoint_id == id.to_string()) else {
            return Ok(());
        };
        let new = change(server, servers::now());
        servers::save(storage, &saved).await?;
        if new {
            self.inner.discovery.changed();
        }
        Ok(())
    }

    /// The portable identities this device holds the roots of (P7-01).
    /// Every call shares one lock, so take them from here rather than
    /// making another [`Portable`] over the same storage.
    pub fn portable(&self) -> Portable {
        self.inner.portable.clone()
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
            .drain()
            .map(|(_, link)| link)
            .collect();
        for link in links {
            link.close();
        }
        self.inner.endpoint.close().await;
    }
}

/// The saved server, under another key than `id`, whose pinned server id is
/// `server_id`: the server `id` says it is, with its key replaced (PX-14).
pub(crate) fn same_server<'a>(
    saved: &'a [SavedServer],
    id: EndpointId,
    server_id: &str,
) -> Option<&'a SavedServer> {
    let id = id.to_string();
    saved
        .iter()
        .find(|s| s.endpoint_id != id && s.server_id.as_deref() == Some(server_id))
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

/// The name mDNS goes by among [`Client::reachability`]'s lookups.
const MDNS_LOOKUP: &str = "mdns";

/// The paths of `conn` open now, the selected one first.
fn live_paths(conn: &iroh::endpoint::Connection) -> Vec<LivePath> {
    let paths = conn.paths();
    let mut live: Vec<LivePath> = paths
        .iter()
        .filter_map(|p| {
            let (kind, addr) = PathKind::of(p.remote_addr())?;
            Some(LivePath {
                kind,
                addr,
                rtt_ms: p.rtt().as_millis().try_into().unwrap_or(u64::MAX),
                selected: p.is_selected(),
            })
        })
        .collect();
    live.sort_by_key(|p| !p.selected);
    live
}

/// The most pkarr relays a client looks servers up at (P6-07, glm E-3).
/// iroh cannot take a lookup service back once added, so each distinct URL
/// a server or invite names would otherwise add one for the client's life.
/// A household names one or two.
pub(crate) const MAX_LOOKUPS: usize = 8;

/// Whether `url` is a lookup to add: not added yet, and room for it.
fn admit_lookup(lookups: &mut std::collections::HashSet<String>, url: &str) -> bool {
    if lookups.contains(url) {
        return false;
    }
    if lookups.len() >= MAX_LOOKUPS {
        tracing::debug!(
            url,
            max = MAX_LOOKUPS,
            "too many pkarr relays; not looked up at"
        );
        return false;
    }
    lookups.insert(url.to_owned())
}

/// `new` then `known`, each once, at most `max`: the newest hints win
/// (P6-07, glm E-3), so pasting link after link cannot grow them.
pub(crate) fn newest_first<T: PartialEq + Clone>(new: &[T], known: &[T], max: usize) -> Vec<T> {
    let mut kept: Vec<T> = Vec::new();
    for item in new.iter().chain(known) {
        if kept.len() == max {
            break;
        }
        if !kept.contains(item) {
            kept.push(item.clone());
        }
    }
    kept
}

/// The addresses of `groups`, in order, each once.
pub(crate) fn listed(groups: impl IntoIterator<Item = Vec<SocketAddr>>) -> Vec<SocketAddr> {
    let mut addrs: Vec<SocketAddr> = Vec::new();
    for addr in groups.into_iter().flatten() {
        if !addrs.contains(&addr) {
            addrs.push(addr);
        }
    }
    addrs
}

pub(crate) fn parse_id(endpoint_id: &str) -> Result<EndpointId, SdkError> {
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

/// `url`, if it is a web page an app may open or show as a QR code: an
/// `http` or `https` URL with a host. A server sends it (`ServerInfo`,
/// `DeviceSignInStarted`), and a malicious one could send a `file:` or an
/// app's own scheme instead (PX-11, PX-09 finding 18); that is dropped,
/// as if the server had no web UI.
pub(crate) fn web_page(url: Option<String>) -> Option<String> {
    let url = url?;
    if !is_web_page(&url) {
        tracing::warn!(url, "the server's web_url is not an http(s) page; not used");
        return None;
    }
    Some(url)
}

/// `scheme://host...` with an `http` or `https` scheme (any case) and a
/// host, and nothing an opener could read another way: no whitespace or
/// control characters. Parsed by hand, so the SDK (and the apps' lock
/// files) take no URL parser for it.
fn is_web_page(url: &str) -> bool {
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority.rsplit('@').next().unwrap_or_default();
    let host = match host_port.strip_prefix('[') {
        // An IPv6 literal, `[::1]:7421`.
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => host_port.split(':').next().unwrap_or_default(),
    };
    !host.is_empty()
}

/// `signed` with an empty user handle as `None`: what an app gives when
/// its platform gave no handle is no handle on the wire, whichever way
/// the app says it.
pub(crate) fn without_empty_user_handle(mut signed: FinishPasskeySignIn) -> FinishPasskeySignIn {
    if signed.user_handle.as_ref().is_some_and(Vec::is_empty) {
        signed.user_handle = None;
    }
    signed
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use super::*;

    /// P6-07, glm E-3: hints from link after link do not pile up.
    #[test]
    fn newest_hints_win_and_are_held_to_the_limit() {
        let known: Vec<u32> = (0..40).collect();
        let kept = newest_first(&[100, 3, 101], &known, 32);
        assert_eq!(kept.len(), 32);
        assert_eq!(&kept[..4], &[100, 3, 101, 0]);
        assert!(!kept.contains(&39));
    }

    /// P6-07, glm E-3: each pkarr relay is added once, and at most
    /// MAX_LOOKUPS of them, however many servers or links name one.
    #[test]
    fn lookups_are_added_once_and_at_most_the_limit() {
        let mut lookups = std::collections::HashSet::new();
        assert!(admit_lookup(&mut lookups, "https://dns.iroh.link/pkarr"));
        assert!(!admit_lookup(&mut lookups, "https://dns.iroh.link/pkarr"));
        let admitted = (0..20)
            .filter(|n| admit_lookup(&mut lookups, &format!("https://pkarr{n}.example.net/")))
            .count();
        assert_eq!(admitted, MAX_LOOKUPS - 1);
        assert_eq!(lookups.len(), MAX_LOOKUPS);
    }
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
