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
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, RelayUrl, SecretKey};
use spoolframe_protocol::{CLIENT_ALPN, FEDERATION_ALPN, close};
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

/// How to build the endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointOptions {
    /// The UDP port to listen on (IPv4 and IPv6); `None` picks a free one.
    pub port: Option<u16>,
    pub relay: Relay,
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
}

#[derive(Debug, thiserror::Error)]
pub enum NetworkError {
    #[error("cannot bind the endpoint on port {port:?}: {message}")]
    Bind { port: Option<u16>, message: String },
}

/// The running endpoint.
#[derive(Debug)]
pub struct Server {
    router: Router,
    connections: Connections,
}

impl Server {
    /// Bind the endpoint with `key` and start accepting connections.
    pub async fn start(key: SecretKey, options: &EndpointOptions) -> Result<Self, NetworkError> {
        let relay = match &options.relay {
            Relay::Disabled => RelayMode::Disabled,
            Relay::Default => RelayMode::Default,
            Relay::Custom(url) => RelayMode::Custom(RelayUrl::from(url.clone()).into()),
        };
        let bind_failed = |message: String| NetworkError::Bind {
            port: options.port,
            message,
        };
        // Minimal: no address lookup services. LAN discovery is added
        // with mDNS (P1-13), and pkarr/DNS lookup with remote access (P3).
        let mut builder = Endpoint::builder(presets::Minimal)
            .secret_key(key)
            .relay_mode(relay)
            .alpns(vec![CLIENT_ALPN.to_vec(), FEDERATION_ALPN.to_vec()]);
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

        let connections = Connections::default();
        // Unknown ALPNs never get this far: the TLS handshake refuses them.
        let router = Router::builder(endpoint)
            .accept(
                CLIENT_ALPN,
                ClientProtocol {
                    connections: connections.clone(),
                },
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

/// `spoolframe/client/1`. Until the protocol exists (P1-10) a connection is
/// recorded and held open until the client closes it.
#[derive(Debug, Clone)]
struct ClientProtocol {
    connections: Connections,
}

impl ProtocolHandler for ClientProtocol {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let id = connection.stable_id();
        let auth = ConnectionAuth::Anonymous {
            endpoint: connection.remote_id(),
        };
        tracing::info!(?auth, "client connected");
        self.connections.lock().insert(id, auth.clone());
        let reason = connection.closed().await;
        self.connections.lock().remove(&id);
        tracing::info!(?auth, %reason, "client disconnected");
        Ok(())
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
