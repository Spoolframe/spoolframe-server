//! The server's Iroh endpoint: one per process, routing connections by
//! ALPN (ARCHITECTURE §11–14, §20).
//!
//! A connection proves which `EndpointId` it comes from and nothing else.
//! Being on the LAN is never a reason to trust it: authority comes from a
//! session (P1-12), as it will for remote clients.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use iroh::endpoint::{Connection, VarInt, presets};
use iroh::endpoint_info::UserData;
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, RelayUrl, SecretKey};
use iroh_mdns_address_lookup::MdnsAddressLookup;
use spoolframe_protocol::relay_access::RELAY_ACCESS_ALPN;
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

/// The ALPNs the endpoint takes: the client protocol, federation (to
/// refuse it with a reason), and its relays' question when it has relays
/// of its own (P6-04).
fn alpns(options: &EndpointOptions) -> Vec<Vec<u8>> {
    let mut alpns = vec![CLIENT_ALPN.to_vec(), FEDERATION_ALPN.to_vec()];
    if !options.own_relays.is_empty() {
        alpns.push(RELAY_ACCESS_ALPN.to_vec());
    }
    alpns
}

/// Where the server publishes where it is (`network.pkarr`, P6-05).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Pkarr {
    /// Nothing is published.
    #[default]
    Off,
    /// n0's public pkarr relay, which its DNS (dns.iroh.link) serves.
    N0,
    /// A pkarr relay of the operator's own.
    Custom(Url),
}

/// n0's pkarr relay, as iroh names it: where `Pkarr::N0` publishes, and
/// what a device is told to look the server up at.
pub const N0_PKARR: &str = "https://dns.iroh.link/pkarr";

impl Pkarr {
    /// The pkarr relay's URL; `None` while off.
    pub fn url(&self) -> Option<Url> {
        match self {
            Pkarr::Off => None,
            Pkarr::N0 => Some(Url::parse(N0_PKARR).expect("a valid URL")),
            Pkarr::Custom(url) => Some(url.clone()),
        }
    }
}

/// How the server's QUIC paths share the link (`network.congestion_control`).
/// One choice for every path: noq builds each path's controller knowing
/// neither its address nor whether it crosses the WAN.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CongestionControl {
    /// noq's default, and iroh's: backs off on every loss, which on a
    /// lossy cellular link holds a window of a few dozen KB.
    #[default]
    Cubic,
    /// BBR (v3): paces by the rate and round trip it measures, not by
    /// loss, so a lossy link keeps its throughput.
    Bbr,
}

impl CongestionControl {
    /// As the setting writes it.
    pub fn as_str(self) -> &'static str {
        match self {
            CongestionControl::Cubic => "cubic",
            CongestionControl::Bbr => "bbr",
        }
    }

    /// What builds each path's controller.
    fn factory(
        self,
    ) -> std::sync::Arc<dyn iroh::endpoint::ControllerFactory + Send + Sync + 'static> {
        match self {
            CongestionControl::Cubic => {
                std::sync::Arc::new(noq_proto::congestion::CubicConfig::default())
            }
            CongestionControl::Bbr => {
                std::sync::Arc::new(noq_proto::congestion::Bbr3Config::default())
            }
        }
    }
}

/// What the server publishes (P6-05): its relays and the addresses that
/// are public. A LAN, link-local or loopback address is never put in a
/// record anyone can read.
fn published() -> iroh::address_lookup::AddrFilter {
    iroh::address_lookup::AddrFilter::new(|addrs| {
        let kept = addrs
            .iter()
            .filter(|addr| match addr {
                iroh::TransportAddr::Ip(ip) => crate::wan::not_public(ip.ip()).is_none(),
                _ => true,
            })
            .cloned()
            .collect::<Vec<_>>();
        std::borrow::Cow::Owned(kept)
    })
}

/// `relays` as iroh's mode: none is off, `default` alone n0's own, and
/// any other list one map of every entry, `default` as n0's relays, each
/// relay of its own asked for the server's address or not by which side
/// of the NAT it is on ([`crate::relay_nat`]); `public` is the public
/// addresses already known. Also where each of its own relays was placed,
/// for [`crate::relay_nat::watch`] to go on from.
async fn relay_mode(
    relays: &[Relay],
    public: &[std::net::IpAddr],
) -> (RelayMode, HashMap<RelayUrl, crate::relay_nat::Side>) {
    match relays {
        [] => (RelayMode::Disabled, HashMap::new()),
        [Relay::Default] => (RelayMode::Default, HashMap::new()),
        _ => {
            let (map, sides) = crate::relay_nat::initial(relays, public).await;
            (RelayMode::Custom(map), sides)
        }
    }
}

/// Every relay URL `relays` names, in their order, `default` expanded to
/// n0's: what a device is told to reach the server through.
pub(crate) fn relay_urls(relays: &[Relay]) -> Vec<RelayUrl> {
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
    /// Listen on IPv6 too (`network.ipv6`). Off, the endpoint has only an
    /// IPv4 socket, so iroh has no IPv6 address to tell or punch through.
    pub ipv6: bool,
    /// `network.relays`; empty: relays off.
    pub relays: Vec<Relay>,
    /// `network.pkarr` (P6-05).
    pub pkarr: Pkarr,
    /// `network.own_relays` (P6-04): the relay processes of its own it
    /// tells who an endpoint is.
    pub own_relays: Vec<EndpointId>,
    /// Announce the server on the LAN over mDNS (P1-13).
    pub mdns: bool,
    /// Who may hold connections (PX-10).
    pub admission: Admission,
    /// Which of its addresses a signed-in device is told (PX-12).
    pub announce: Announce,
    /// Its public addresses (`network.wan`, P6-02); `None`: none.
    pub wan: Option<crate::wan::Wan>,
    /// `network.congestion_control`.
    pub congestion: CongestionControl,
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
    pub(crate) fn admit(
        &self,
        connection: &Connection,
        known: bool,
        local: bool,
    ) -> Option<Arc<Peer>> {
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
        if local {
            peer.note_local();
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

    /// `endpoint` signed in, as `user` when known, a local user if
    /// `local` (else a portable guest, P7-06): each of its connections
    /// gets the normal limits. Their ids.
    pub(crate) fn signed_in(
        &self,
        endpoint: &EndpointId,
        user: Option<&str>,
        local: bool,
    ) -> Vec<usize> {
        let mut raised = Vec::new();
        for (id, peer) in self.lock().iter().filter(|(_, p)| &p.endpoint == endpoint) {
            peer.raise();
            if local {
                peer.note_local();
            }
            peer.note_signed_in(user);
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
                match store.endpoint_has_any_session(&endpoint.to_string()).await {
                    Ok(true) => {}
                    Ok(false) => self.sign_out(&endpoint),
                    Err(e) => tracing::warn!(error = %e, "cannot check a device's sessions"),
                }
            }
        }
    }
}

/// Tell `metrics` whether the NAT maps this server's port per
/// destination, from each net report iroh makes, until the endpoint
/// closes. iroh's public API has it only behind `unstable-net-report`.
async fn watch_nat_mapping(endpoint: Endpoint, metrics: Arc<crate::metrics::Metrics>) {
    use iroh::Watcher as _;
    let mut reports = endpoint.net_report();
    let closed = endpoint.closed();
    tokio::pin!(closed);
    loop {
        metrics.nat_mapping_varies(reports.get().and_then(|r| r.mapping_varies_by_dest()));
        tokio::select! {
            () = &mut closed => return,
            updated = reports.updated() => {
                if updated.is_err() {
                    return;
                }
            }
        }
    }
}

/// Log the home relay iroh picks from `network.relays` by latency, and
/// each time it fails over or loses it, until the endpoint closes: the
/// admin's proof that relays are up (P6-03); and say whether it has one
/// in the metrics.
async fn log_home_relay(endpoint: Endpoint, metrics: Arc<crate::metrics::Metrics>) {
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
        metrics.home_relay(!connected.is_empty());
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
    /// `http_playback.listen` could not be bound (PX-49).
    #[error("cannot listen for HTTP playback on {addr}: {message}")]
    HttpPlayback {
        addr: std::net::SocketAddr,
        message: String,
    },
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
    /// Playlists (PX-40).
    pub playlists: Arc<crate::playlists::Playlists>,
    /// The encoders the planner may use (P2-02).
    pub transcoding: Arc<Transcoding>,
    /// Running encodes (P2-03).
    pub jobs: Arc<Jobs>,
    /// Bitmap subtitles read as text (P2-07).
    pub subtitles: Arc<Subtitles>,
    /// External text subtitles synced and offset (PX-03).
    pub subtitle_timings: Arc<crate::subtitle_sync::SubtitleTimings>,
    /// Subtitles downloaded from OpenSubtitles on demand (PX-02).
    pub subtitle_downloads: Arc<crate::subtitle_downloads::SubtitleDownloads>,
    /// The optimized copies (PX-37), which a playback may play in place of
    /// an encode; `None` plays none.
    pub optimized: Option<Arc<crate::optimized::Pool>>,
    /// Frame previews for scrubbing (P3-13).
    pub previews: Arc<crate::previews::Previews>,
    /// Keyframes read ahead of any play (`experimental.index_ahead`).
    pub index_ahead: Arc<crate::index_ahead::IndexAhead>,
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
    /// The WAN uplink budget, the LAN, and the playbacks off it (PX-46).
    pub uplink: Arc<crate::uplink::Uplink>,
    /// The HTTP playback port (PX-49), bound at start; off by default.
    pub http_playback: crate::http_playback::Settings,
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
    /// The HTTP playback port (PX-49), when it is on.
    http_playback: Option<Arc<crate::http_playback::HttpPlayback>>,
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
        // The public addresses found so far: `network.wan`'s static ones
        // now, the rest as they are found (P6-02).
        let public = crate::wan::Public::new(options.wan.as_ref());
        let known: Vec<std::net::IpAddr> = public.addrs().iter().map(|p| p.addr.ip()).collect();
        let (relay, sides) = relay_mode(&options.relays, &known).await;
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
        tracing::info!(
            congestion_control = options.congestion.as_str(),
            "QUIC congestion control"
        );
        // Minimal: no address lookup services but the mDNS added here;
        // pkarr/DNS lookup comes with remote access (P6).
        let mut builder = Endpoint::builder(presets::Minimal)
            .secret_key(key)
            .relay_mode(relay)
            .transport_config(admission::transport_config(options.congestion.factory()))
            .alpns(alpns(options));
        if options.mdns {
            let advert: UserData = lan::advert(&name.get())
                .parse()
                .expect("lan::advert fits the record");
            builder = builder
                .address_lookup(MdnsAddressLookup::builder().service_name(lan::MDNS_SERVICE))
                .user_data_for_address_lookup(advert);
        }
        if let Some(url) = options.pkarr.url() {
            // Signed with the server's key; iroh republishes it as what it
            // knows of itself changes, and every few minutes.
            builder = builder.address_lookup(
                iroh::address_lookup::PkarrPublisher::builder(url).addr_filter(published()),
            );
        }
        // The builder binds 0.0.0.0 and [::] on a free port unless told
        // otherwise: a fixed port, or IPv4 only, replaces both.
        if options.port.is_some() || !options.ipv6 {
            let port = options.port.unwrap_or(0);
            builder = builder
                .clear_ip_transports()
                .bind_addr((Ipv4Addr::UNSPECIFIED, port))
                .map_err(|e| bind_failed(e.to_string()))?;
            if options.ipv6 {
                builder = builder
                    .bind_addr((Ipv6Addr::UNSPECIFIED, port))
                    .map_err(|e| bind_failed(e.to_string()))?;
            }
        }
        let endpoint = builder
            .bind()
            .await
            .map_err(|e| bind_failed(e.to_string()))?;
        if !options.relays.is_empty() {
            tokio::spawn(log_home_relay(endpoint.clone(), services.metrics.clone()));
        }
        tokio::spawn(watch_nat_mapping(
            endpoint.clone(),
            services.metrics.clone(),
        ));
        crate::relay_nat::watch(endpoint.clone(), &options.relays, public.clone(), sides);

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
            // Per connection, from its Hello (PX-49).
            experimental: Vec::new(),
            // PX-16: only the pre-auth ones before a session.
            features: spoolframe_protocol::features::names(spoolframe_protocol::features::PRE_AUTH),
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
        if let Some(wan) = &options.wan {
            crate::wan::start(&public, wan, &endpoint, &services.wan);
        }
        let lookup = options.pkarr.url().map(|url| url.to_string());
        let told = Told::new(
            endpoint.clone(),
            options.announce.clone(),
            relays,
            lookup,
            public,
        );
        // The relays of its own it answers (P6-04); none, and the ALPN is
        // refused as any other unknown one.
        let relay_access = (!options.own_relays.is_empty()).then(|| {
            crate::relay_access::RelayAccess::new(
                options.own_relays.clone(),
                endpoint.id(),
                services.auth.clone(),
            )
        });
        // Bound before the client protocol answers anyone, so a Hello
        // never names a feature that is not served (PX-49).
        let http_playback = match services.http_playback.listen {
            None => None,
            Some(addr) => {
                crate::http_playback::HttpPlayback::start(&services.http_playback, &services)
                    .await
                    .map_err(|e| NetworkError::HttpPlayback {
                        addr,
                        message: e.to_string(),
                    })?
            }
        };
        let client = ClientProtocol::new(
            endpoint.clone(),
            told.clone(),
            connections.clone(),
            info,
            name.clone(),
            services,
            http_playback.clone(),
        );
        let mut router = Router::builder(endpoint)
            .accept(CLIENT_ALPN, client)
            .accept(FEDERATION_ALPN, Unsupported("spoolframe/federation/1"));
        if let Some(relay_access) = relay_access {
            router = router.accept(RELAY_ACCESS_ALPN, relay_access);
        }
        let router = router.spawn();
        Ok(Self {
            router,
            told,
            connections,
            sign_outs,
            unserved,
            http_playback,
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

    /// The endpoint's sockets: IPv4 and, unless `network.ipv6 = false`,
    /// IPv6.
    pub fn bound_sockets(&self) -> Vec<SocketAddr> {
        self.router.endpoint().bound_sockets()
    }

    /// What a signed-in device is told about where to dial (PX-12):
    /// read at each call, as for each device.
    pub fn told(&self) -> Told {
        self.told.clone()
    }

    pub fn connections(&self) -> &Connections {
        &self.connections
    }

    /// The HTTP playback port (PX-49), when it is on.
    pub fn http_playback(&self) -> Option<&Arc<crate::http_playback::HttpPlayback>> {
        self.http_playback.as_ref()
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
        if let Some(http_playback) = &self.http_playback {
            http_playback.stop();
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Each setting builds its own controller for a path.
    #[test]
    fn congestion_control_builds_the_controller_it_names() {
        let built = |c: CongestionControl| {
            c.factory()
                .build(std::time::Instant::now(), 1200)
                .into_any()
        };
        assert!(
            built(CongestionControl::Cubic)
                .downcast::<noq_proto::congestion::Cubic>()
                .is_ok()
        );
        assert!(
            built(CongestionControl::Bbr)
                .downcast::<noq_proto::congestion::Bbr3>()
                .is_ok()
        );
    }

    /// The record anyone can read holds the relays and the public
    /// addresses, never a LAN one (P6-05).
    #[test]
    fn only_relays_and_public_addresses_are_published() {
        let relay: RelayUrl = "https://relay.example.org".parse().unwrap();
        let ip = |s: &str| iroh::TransportAddr::Ip(s.parse().unwrap());
        let addrs = vec![
            iroh::TransportAddr::Relay(relay.clone()),
            ip("192.168.1.10:7420"),
            ip("127.0.0.1:7420"),
            ip("[fe80::1]:7420"),
            ip("203.0.113.7:7420"),
        ];
        assert_eq!(
            published().apply(&addrs).into_owned(),
            [iroh::TransportAddr::Relay(relay), ip("203.0.113.7:7420")]
        );
    }
}
