//! The server's Iroh endpoint: one per process, routing connections by
//! ALPN (ARCHITECTURE §11–14, §20).
//!
//! A connection proves which `EndpointId` it comes from and nothing else.
//! Being on the LAN is never a reason to trust it: authority comes from a
//! session (P1-12), as it will for remote clients.

use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::{Arc, Mutex};

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

/// Which relays the endpoint uses to reach peers it cannot dial directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Relay {
    /// LAN only: no relay, no hole punching. Phase 1.
    Disabled,
    /// The public n0 relays.
    Default,
    /// A relay of our own.
    Custom(Url),
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
    pub relay: Relay,
    /// Announce the server on the LAN over mDNS (P1-13).
    pub mdns: bool,
    /// Who may hold connections (PX-10).
    pub admission: Admission,
    /// Which of its addresses a signed-in device is told (PX-12).
    pub announce: Announce,
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

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<usize, Arc<Peer>>> {
        self.peers
            .lock()
            .expect("the connections lock is never held across a panic")
    }

    pub(crate) fn admission(&self) -> Admission {
        self.admission
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
    /// limits.
    pub(crate) fn signed_in(&self, endpoint: &EndpointId) {
        for peer in self.lock().values().filter(|p| &p.endpoint == endpoint) {
            peer.raise();
        }
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

    /// Close the connections of every device that is no longer signed in:
    /// each time sessions end, and every [`admission::RECHECK`].
    async fn close_signed_out(self, store: crate::store::Store) {
        let mut ended = store.sessions_ended();
        loop {
            tokio::select! {
                changed = ended.changed() => if changed.is_err() { return },
                () = tokio::time::sleep(admission::RECHECK) => {}
            }
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
        let relay = match &options.relay {
            Relay::Disabled => RelayMode::Disabled,
            Relay::Default => RelayMode::Default,
            Relay::Custom(url) => RelayMode::Custom(RelayUrl::from(url.clone()).into()),
        };
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
                .close_signed_out(services.auth.store().clone()),
        );
        // Unknown ALPNs never get this far: the TLS handshake refuses them.
        let told = Told::new(endpoint.clone(), options.announce.clone());
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
        self.sign_outs.abort();
        if let Err(e) = self.router.shutdown().await {
            tracing::warn!(error = %e, "endpoint shutdown");
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.sign_outs.abort();
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
