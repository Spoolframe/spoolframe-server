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

/// The open client connections and their auth state.
#[derive(Debug, Clone, Default)]
pub struct Connections(Arc<Mutex<HashMap<usize, ConnectionAuth>>>);

impl Connections {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<usize, ConnectionAuth>> {
        self.0
            .lock()
            .expect("the connections lock is never held across a panic")
    }

    /// Every open client connection.
    pub fn snapshot(&self) -> Vec<ConnectionAuth> {
        self.lock().values().cloned().collect()
    }

    pub(crate) fn insert(&self, id: usize, auth: ConnectionAuth) {
        self.lock().insert(id, auth);
    }

    pub(crate) fn remove(&self, id: usize) {
        self.lock().remove(&id);
    }

    /// Whether `endpoint` has a connection open.
    pub(crate) fn has_endpoint(&self, endpoint: &EndpointId) -> bool {
        self.lock()
            .values()
            .any(|auth| matches!(auth, ConnectionAuth::Anonymous { endpoint: e } if e == endpoint))
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
    connections: Connections,
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
        let connections = Connections::default();
        services.metrics.observe_services(&services, &connections);
        // Unknown ALPNs never get this far: the TLS handshake refuses them.
        let router = Router::builder(endpoint)
            .accept(
                CLIENT_ALPN,
                ClientProtocol::new(connections.clone(), info, name.clone(), services),
            )
            .accept(FEDERATION_ALPN, Unsupported("spoolframe/federation/1"))
            .spawn();
        Ok(Self {
            router,
            connections,
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

    pub fn connections(&self) -> &Connections {
        &self.connections
    }

    /// Close every connection and the endpoint.
    pub async fn shutdown(self) {
        if let Err(e) = self.router.shutdown().await {
            tracing::warn!(error = %e, "endpoint shutdown");
        }
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
