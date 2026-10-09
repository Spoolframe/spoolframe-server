//! The server's Iroh endpoint: one per process, routing connections by
//! ALPN (ARCHITECTURE §11–14, §20).
//!
//! A connection proves which `EndpointId` it comes from and nothing else.
//! Being on the LAN is never a reason to trust it: authority comes from a
//! session (P1-12), as it will for remote clients.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::Arc;

use iroh::endpoint::{Connection, VarInt, presets};
use iroh::endpoint_info::UserData;
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, RelayUrl, SecretKey};
use iroh_mdns_address_lookup::MdnsAddressLookup;
use spoolframe_protocol::{CLIENT_ALPN, FEDERATION_ALPN, PROTOCOL_VERSION, ServerInfo, close, lan};

use spoolframe_catalog::Catalog;
use spoolframe_media::Prober;

use crate::admission::{self, Admission, Peer};
use crate::announce::{Announce, Told};
use crate::artwork::ArtworkCache;
use crate::auth::Auth;
use crate::client::{self, ClientProtocol};
use crate::encode::job::Jobs;
use crate::playback::Playbacks;
use crate::subtitles::Subtitles;
use crate::transcode::Transcoding;
use crate::watch::Watch;
use url::Url;

/// One entry of `network.relays` (P6-03): relays the endpoint may take
/// its home relay from, which clients that cannot dial it directly reach
/// it through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Relay {
    /// The public n0 relays.
    Default,
    /// A relay of our own, or anyone's.
    Custom(Url),
}

/// `relays` as iroh's mode: none is off, `default` alone n0's own, and
/// any other list one map of every entry, `default` as n0's relays.
fn relay_mode(relays: &[Relay]) -> RelayMode {
    match relays {
        [] => RelayMode::Disabled,
        [Relay::Default] => RelayMode::Default,
        _ => RelayMode::Custom(relay_urls(relays).into_iter().collect()),
    }
}

/// Every relay URL `relays` names, in their order, `default` expanded to
/// n0's: what a device is told to reach the server through.
fn relay_urls(relays: &[Relay]) -> Vec<RelayUrl> {
    let mut urls: Vec<RelayUrl> = Vec::new();
    for relay in relays {
        let more = match relay {
            Relay::Default => RelayMode::Default.relay_map().urls(),
            Relay::Custom(url) => vec![RelayUrl::from(url.clone())],
        };
        for url in more {
            if !urls.contains(&url) {
                urls.push(url);
            }
        }
    }
    urls
}

/// What the server is called (`name`, P3-10): a live setting, so `Hello`,
/// the mDNS announcement and the admin status read it as it is now.
#[derive(Debug, Clone)]
pub struct ServerName(Arc<tokio::sync::watch::Sender<String>>);

impl ServerName {
    pub fn new(name: impl Into<String>) -> Self {
        Self(Arc::new(tokio::sync::watch::Sender::new(name.into())))
    }

    pub fn get(&self) -> String {
        self.0.borrow().clone()
    }

    /// Call the server `name` from now on.
    pub fn set(&self, name: impl Into<String>) {
        self.0.send_replace(name.into());
    }

    fn changes(&self) -> tokio::sync::watch::Receiver<String> {
        self.0.subscribe()
    }
}

/// How to build the endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointOptions {
    /// The UDP port to listen on (IPv4 and IPv6); `None` picks a free one.
    pub port: Option<u16>,
    /// `network.relays`; empty: relays off.
    pub relays: Vec<Relay>,
    /// Announce the server on the LAN over mDNS (P1-13).
    pub mdns: bool,
    /// Who may hold connections (PX-10).
    pub admission: Admission,
    /// Which of its addresses a signed-in device is told (PX-12).
    pub announce: Announce,
    /// Its public addresses (`network.wan`, P6-02); `None`: none.
    pub wan: Option<crate::wan::Wan>,
}

/// What the server knows about who is on the other end of a connection.
/// Only the endpoint is proven by the connection itself; which user is
/// acting is a property of each request's session (P1-12), so one
/// connection can carry several users.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConnectionAuth {
    /// A client endpoint. Its requests are anonymous unless they carry a
    /// session.
    Anonymous { endpoint: EndpointId },
}

/// The open client connections and their auth state, with who may open
/// more (PX-10).
#[derive(Debug, Clone, Default)]
pub struct Connections {
    peers: Arc<Mutex<HashMap<usize, Arc<Peer>>>>,
    admission: Admission,
}

impl Connections {
    pub(crate) fn new(admission: Admission) -> Self {
        Self {
            peers: Arc::default(),
            admission,
        }
    }

    fn lock(&self) -> parking_lot::MutexGuard<'_, HashMap<usize, Arc<Peer>>> {
        self.peers.lock()
    }

    pub(crate) fn admission(&self) -> Admission {
        self.admission
    }

    /// How many open client connections go straight to their device now,
    /// and how many through a relay (P6-03): what remote access costs.
    /// One with no path selected yet is neither.
    pub fn paths(&self) -> (usize, usize) {
        let (mut direct, mut relayed) = (0, 0);
        for peer in self.lock().values() {
            match peer.relayed() {
                Some(false) => direct += 1,
                Some(true) => relayed += 1,
                None => {}
            }
        }
        (direct, relayed)
    }

    /// Every open client connection.
    pub fn snapshot(&self) -> Vec<ConnectionAuth> {
        self.lock()
            .values()
            .map(|peer| ConnectionAuth::Anonymous {
                endpoint: peer.endpoint,
            })
            .collect()
    }

    /// Hold `connection`, if there is room: a signed-in device (`known`)
    /// always, with the normal limits; a stranger only while fewer than
    /// [`Admission::max_strangers`] are held.
    pub(crate) fn admit(&self, connection: &Connection, known: bool) -> Option<Arc<Peer>> {
        let mut peers = self.lock();
        if !known
            && peers.values().filter(|peer| !peer.known()).count() >= self.admission.max_strangers
        {
            return None;
        }
        let peer = Arc::new(Peer::new(connection));
        if known {
            peer.raise();
        }
        peers.insert(connection.stable_id(), peer.clone());
        Some(peer)
    }

    pub(crate) fn remove(&self, id: usize) {
        self.lock().remove(&id);
    }

    /// Whether `endpoint` has a connection open.
    pub(crate) fn has_endpoint(&self, endpoint: &EndpointId) -> bool {
        self.lock().values().any(|peer| &peer.endpoint == endpoint)
    }

    /// `endpoint` signed in: each of its connections gets the normal
    /// limits. Their ids.
    pub(crate) fn signed_in(&self, endpoint: &EndpointId) -> Vec<usize> {
        let mut raised = Vec::new();
        for (id, peer) in self.lock().iter().filter(|(_, p)| &p.endpoint == endpoint) {
            peer.raise();
            peer.note_signed_in();
            raised.push(*id);
        }
        raised
    }

    /// Whether connection `id` is signed in, and not signed out since.
    pub(crate) fn is_signed_in(&self, id: usize) -> bool {
        self.lock().get(&id).is_some_and(|peer| peer.signed_in())
    }

    /// The endpoints of the connections that have the normal limits.
    fn known_endpoints(&self) -> Vec<EndpointId> {
        let mut endpoints: Vec<_> = self
            .lock()
            .values()
            .filter(|peer| peer.known())
            .map(|peer| peer.endpoint)
            .collect();
        endpoints.sort();
        endpoints.dedup();
        endpoints
    }

    /// `endpoint` has no live session left: close its connections.
    fn sign_out(&self, endpoint: &EndpointId) {
        for peer in self.lock().values().filter(|p| &p.endpoint == endpoint) {
            peer.sign_out();
        }
    }

    /// End the playbacks of every session that has ended (B-10), and
    /// close the connections of every device that is no longer signed in:
    /// each time sessions end, and every [`admission::RECHECK`].
    async fn close_signed_out(self, auth: Auth, playbacks: Arc<Playbacks>) {
        let store = auth.store().clone();
        let mut ended = store.sessions_ended();
        loop {
            tokio::select! {
                changed = ended.changed() => if changed.is_err() { return },
                () = tokio::time::sleep(admission::RECHECK) => {}
            }
            end_signed_out_playbacks(&auth, &playbacks).await;
            for endpoint in self.known_endpoints() {
                match store.endpoint_signed_in(&endpoint.to_string()).await {
                    Ok(true) => {}
                    Ok(false) => self.sign_out(&endpoint),
                    Err(e) => tracing::warn!(error = %e, "cannot check a device's sessions"),
                }
            }
        }
    }
}

/// Log the home relay iroh picks from `network.relays` by latency, and
/// each time it fails over or loses it, until the endpoint closes: the
/// admin's proof that relays are up (P6-03).
async fn log_home_relay(endpoint: Endpoint) {
    use iroh::Watcher as _;
    let mut status = endpoint.home_relay_status();
    let closed = endpoint.closed();
    tokio::pin!(closed);
    let mut logged: Option<Vec<String>> = None;
    loop {
        let connected: Vec<String> = status
            .get()
            .iter()
            .filter(|s| s.is_connected())
            .map(|s| s.url().to_string())
            .collect();
        if logged.as_ref() != Some(&connected) {
            if connected.is_empty() {
                if logged.is_some() {
                    tracing::warn!("no home relay: devices that cannot dial in cannot connect");
                }
            } else {
                tracing::info!(relay = %connected.join(", "), "home relay");
            }
            logged = Some(connected);
        }
        tokio::select! {
            () = &mut closed => return,
            updated = status.updated() => {
                if updated.is_err() {
                    return;
                }
            }
        }
    }
}

/// End the playbacks of each session that is no longer live, at once
/// (B-10): their encodes stop, rather than after the disconnect grace that
/// closing the device's connection starts. A session that cannot be
/// looked up is left to the next check, and to each read's recheck.
async fn end_signed_out_playbacks(auth: &Auth, playbacks: &Playbacks) {
    for session in playbacks.sessions() {
        match auth.session_owner(&session.0).await {
            Ok(Some(owner)) if owner.live => {}
            Ok(_) => {
                playbacks.end_session(&session);
            }
            Err(e) => tracing::warn!(error = %e, "cannot check a playing session"),
        }
    }
}

/// Each time the catalog changes, end the playbacks of items it no longer
/// serves (B-10).
async fn end_unserved_playbacks_on_change(
    mut changes: tokio::sync::watch::Receiver<u64>,
    catalog: Arc<dyn Catalog>,
    playbacks: Arc<Playbacks>,
) {
    while changes.changed().await.is_ok() {
        end_unserved_playbacks(catalog.as_ref(), &playbacks).await;
    }
}

/// End, at once, the playbacks of every item in a library no longer
/// served (B-10): removed, or on a Plex server no longer configured. Their
/// encodes stop, and a client still using one is told `PlaybackExpired`;
/// starting it again is refused as not found. An item that cannot be
/// looked up now (Plex down) plays on, left to its read's recheck.
pub(crate) async fn end_unserved_playbacks(catalog: &dyn Catalog, playbacks: &Playbacks) {
    for item in playbacks.items() {
        match catalog.item(&item).await {
            Err(e) if client::unserved(&e) => {
                playbacks.end_item(&item, crate::playback::EndReason::AccessRemoved);
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(%item, error = %e, "cannot check a playing item"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum NetworkError {
    #[error("cannot bind the endpoint on port {port:?}: {message}")]
    Bind { port: Option<u16>, message: String },
}

/// What the client protocol serves from.
#[derive(Clone)]
pub struct Services {
    pub auth: Auth,
    pub catalog: Arc<dyn Catalog>,
    /// Changes each time the catalog's libraries may have changed
    /// ([`crate::catalog::LiveCatalog::changes`]); `None` for a catalog
    /// that never changes.
    pub catalog_changes: Option<tokio::sync::watch::Receiver<u64>>,
    pub prober: Arc<Prober>,
    pub artwork: Arc<ArtworkCache>,
    pub playbacks: Arc<Playbacks>,
    pub watch: Arc<Watch>,
    /// The encoders the planner may use (P2-02).
    pub transcoding: Arc<Transcoding>,
    /// Running encodes (P2-03).
    pub jobs: Arc<Jobs>,
    /// Bitmap subtitles read as text (P2-07).
    pub subtitles: Arc<Subtitles>,
    /// Frame previews for scrubbing (P3-13).
    pub previews: Arc<crate::previews::Previews>,
    /// The apps' passkeys (P3-19).
    pub app_passkeys: Arc<crate::app_passkeys::AppPasskeys>,
    /// The web UI's origin (`admin.public_url`), where a browser approves
    /// a device's sign-in (P3-05); `None` without one.
    pub public_url: Option<String>,
    /// Users' Plex links and their sync (P3-12), for `GetPlexLink`; `None`
    /// on a server that links no Plex accounts.
    pub plex_accounts: Option<Arc<crate::plex_accounts::PlexAccounts>>,
    pub plex_sync: Option<Arc<crate::plex_sync::WatchSync>>,
    /// What the client protocol counts, and where its parts are read
    /// from for the metrics (P3-14).
    pub metrics: Arc<crate::metrics::Metrics>,
    /// The devices connected now and what they play (P3-15).
    pub connected: Arc<crate::connected::Connected>,
    /// What finds the public addresses `network.wan` asks for (P6-02).
    pub wan: crate::wan::Finders,
}

impl std::fmt::Debug for Services {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Services").finish_non_exhaustive()
    }
}

/// The running endpoint.
#[derive(Debug)]
pub struct Server {
    router: Router,
    told: Told,
    connections: Connections,
    /// Closes signed-out devices' connections; ends with the server.
    sign_outs: tokio::task::JoinHandle<()>,
    /// Ends the playbacks of libraries no longer served; ends with the
    /// server.
    unserved: Option<tokio::task::JoinHandle<()>>,
}

impl Server {
    /// Bind the endpoint with `key` and start accepting connections, as
    /// the server called `name`.
    pub async fn start(
        key: SecretKey,
        options: &EndpointOptions,
        name: &ServerName,
        services: Services,
    ) -> Result<Self, NetworkError> {
        let relay = relay_mode(&options.relays);
        // What a signed-in device and an invite are told of them (PX-12,
        // P6-01), the home relay put first at each ask.
        let relays: Vec<String> = relay_urls(&options.relays)
            .iter()
            .map(ToString::to_string)
            .collect();
        let bind_failed = |message: String| NetworkError::Bind {
            port: options.port,
            message,
        };
        // Minimal: no address lookup services but the mDNS added here;
        // pkarr/DNS lookup comes with remote access (P6).
        let mut builder = Endpoint::builder(presets::Minimal)
            .secret_key(key)
            .relay_mode(relay)
            .transport_config(admission::transport_config())
            .alpns(vec![CLIENT_ALPN.to_vec(), FEDERATION_ALPN.to_vec()]);
        if options.mdns {
            let advert: UserData = lan::advert(&name.get())
                .parse()
                .expect("lan::advert fits the record");
            builder = builder
                .address_lookup(MdnsAddressLookup::builder().service_name(lan::MDNS_SERVICE))
                .user_data_for_address_lookup(advert);
        }
        if let Some(port) = options.port {
            builder = builder
                .clear_ip_transports()
                .bind_addr((Ipv4Addr::UNSPECIFIED, port))
                .map_err(|e| bind_failed(e.to_string()))?
                .bind_addr((Ipv6Addr::UNSPECIFIED, port))
                .map_err(|e| bind_failed(e.to_string()))?;
        }
        let endpoint = builder
            .bind()
            .await
            .map_err(|e| bind_failed(e.to_string()))?;
        if !options.relays.is_empty() {
            tokio::spawn(log_home_relay(endpoint.clone()));
        }

        // Idle playbacks end on their own, conversions with them.
        let playbacks = Arc::downgrade(&services.playbacks);
        let every = services.playbacks.sweep_every();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                let Some(playbacks) = playbacks.upgrade() else {
                    return;
                };
                playbacks.sweep();
            }
        });
        if options.mdns {
            // A new name is announced at once, as Hello answers with it.
            let mut changes = name.changes();
            let announcing = endpoint.clone();
            tokio::spawn(async move {
                while changes.changed().await.is_ok() {
                    let advert = lan::advert(&changes.borrow_and_update());
                    match advert.parse::<UserData>() {
                        Ok(data) => announcing.set_user_data_for_address_lookup(Some(data)),
                        Err(e) => tracing::warn!(error = %e, "cannot announce the new name"),
                    }
                }
            });
        }
        let info = ServerInfo {
            name: name.get(),
            endpoint_id: endpoint.id().to_string(),
            server_id: services.app_passkeys.server_id().to_string(),
            protocol_version: PROTOCOL_VERSION,
            server_version: env!("CARGO_PKG_VERSION").to_owned(),
            auth_methods: client::auth_methods(),
            web_url: services.public_url.clone(),
        };
        let connections = Connections::new(options.admission);
        services.metrics.observe_services(&services, &connections);
        let sign_outs = tokio::spawn(
            connections
                .clone()
                .close_signed_out(services.auth.clone(), services.playbacks.clone()),
        );
        let unserved = services.catalog_changes.clone().map(|changes| {
            tokio::spawn(end_unserved_playbacks_on_change(
                changes,
                services.catalog.clone(),
                services.playbacks.clone(),
            ))
        });
        // Unknown ALPNs never get this far: the TLS handshake refuses them.
        let public = crate::wan::Public::new(options.wan.as_ref());
        if let Some(wan) = &options.wan {
            crate::wan::start(&public, wan, &endpoint, &services.wan);
        }
        let told = Told::new(endpoint.clone(), options.announce.clone(), relays, public);
        let client = ClientProtocol::new(
            endpoint.clone(),
            told.clone(),
            connections.clone(),
            info,
            name.clone(),
            services,
        );
        let router = Router::builder(endpoint)
            .accept(CLIENT_ALPN, client)
            .accept(FEDERATION_ALPN, Unsupported("spoolframe/federation/1"))
            .spawn();
        Ok(Self {
            router,
            told,
            connections,
            sign_outs,
            unserved,
        })
    }

    /// The server's identity.
    pub fn id(&self) -> EndpointId {
        self.router.endpoint().id()
    }

    /// Where the endpoint can be dialled right now.
    pub fn addr(&self) -> EndpointAddr {
        self.router.endpoint().addr()
    }

    /// What a signed-in device is told about where to dial (PX-12):
    /// read at each call, as for each device.
    pub fn told(&self) -> Told {
        self.told.clone()
    }

    pub fn connections(&self) -> &Connections {
        &self.connections
    }

    /// Close every connection and the endpoint.
    pub async fn shutdown(self) {
        self.stop_tasks();
        if let Err(e) = self.router.shutdown().await {
            tracing::warn!(error = %e, "endpoint shutdown");
        }
    }
}

impl Server {
    fn stop_tasks(&self) {
        self.sign_outs.abort();
        if let Some(unserved) = &self.unserved {
            unserved.abort();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop_tasks();
    }
}

/// A protocol this server knows of but does not serve: closed at once,
/// with a reason the peer can show.
#[derive(Debug, Clone)]
struct Unsupported(&'static str);

impl ProtocolHandler for Unsupported {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        tracing::info!(
            peer = %connection.remote_id(),
            protocol = self.0,
            "refusing a connection for an unsupported protocol"
        );
        let reason = format!("{} is not supported by this server", self.0);
        connection.close(
            VarInt::from_u32(close::UNSUPPORTED_PROTOCOL),
            reason.as_bytes(),
        );
        // Stay until the close is delivered, or the peer never sees why.
        connection.closed().await;
        Ok(())
    }
}
