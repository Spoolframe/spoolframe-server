//! A relay of the operator's own (P6-04): `spoolframe-server relay`, the
//! same binary with no catalog, users or admin, only an iroh relay, for
//! devices that cannot reach the server directly when n0's public relays
//! are not wanted in the path. Its URL goes in the main server's
//! `network.relays`; its key in the main server's `network.own_relays`.
//!
//! Who it relays for is by tier: the main server says which tier an
//! endpoint is in ([`crate::relay_access`]), asked once a minute at most
//! for each, and the config says which tiers are let through. Relayed
//! traffic stays encrypted end to end; the relay learns only which keys
//! talk.

pub mod config;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use iroh::endpoint::{Connection, presets};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, SecretKey};
use iroh_relay::server::{
    AccessControl, AcmeConfig, CertConfig, ClientRequest, QuicConfig, RelayConfig as IrohRelay,
    Server, ServerConfig, TlsConfig,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use spoolframe_protocol::codec::{read_frame_within, write_frame};
use spoolframe_protocol::relay_access::{AccessAnswer, AccessQuery, RELAY_ACCESS_ALPN, Tier};

pub use config::{Access, RelayConfig, RelayConfigError, Serve};

/// How long the main server's answer for an endpoint is kept: a device
/// signed out is let through this much longer at most.
pub const CACHE: Duration = Duration::from_secs(60);
/// How long a failed answer is kept: the main server unreachable is asked
/// again soon, but not on every connection.
const FAILED_CACHE: Duration = Duration::from_secs(5);
/// How long a question may take.
const ASK_WAIT: Duration = Duration::from_secs(5);

/// Why the relay cannot start.
#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    #[error("cannot read {path}: {message}")]
    Tls { path: String, message: String },
    #[error("cannot make the TLS config: {0}")]
    Rustls(String),
    #[error("cannot bind the endpoint that asks the main server: {0}")]
    Endpoint(String),
    #[error("cannot start the relay: {0}")]
    Spawn(String),
}

/// Start the relay `config` describes, as `key`, keeping what the ACME
/// server gives it under `state_dir`. Dropping the server stops it.
pub async fn spawn(
    config: &RelayConfig,
    key: SecretKey,
    state_dir: &Path,
) -> Result<Server, RelayError> {
    let gate = Gate::new(config, key).await?;
    let mut relay = IrohRelay::new(config.http);
    relay.tls = tls(&config.serve, state_dir).await?;
    relay.access = Arc::new(gate);
    let mut server = ServerConfig::default();
    server.relay = Some(relay);
    server.quic = config.quic.map(QuicConfig::new);
    Server::spawn(server)
        .await
        .map_err(|e| RelayError::Spawn(e.to_string()))
}

/// The relay's TLS, as `serve` says; none behind a proxy.
async fn tls(serve: &Serve, state_dir: &Path) -> Result<Option<TlsConfig>, RelayError> {
    let builder = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| RelayError::Rustls(e.to_string()))?
    .with_no_client_auth();
    Ok(match serve {
        Serve::Proxy => None,
        Serve::Files {
            https,
            cert_file,
            key_file,
        } => {
            let read = |path: &Path, e: &dyn std::fmt::Display| RelayError::Tls {
                path: path.display().to_string(),
                message: e.to_string(),
            };
            let certs = CertificateDer::pem_file_iter(cert_file)
                .map_err(|e| read(cert_file, &e))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| read(cert_file, &e))?;
            let key = PrivateKeyDer::from_pem_file(key_file).map_err(|e| read(key_file, &e))?;
            let server_config = builder
                .with_single_cert(certs, key)
                .map_err(|e| RelayError::Rustls(e.to_string()))?;
            Some(TlsConfig::new(*https, CertConfig::Manual { server_config }))
        }
        Serve::Acme {
            https,
            hostname,
            contact,
            directory,
        } => {
            let acme_config = AcmeConfig::new(directory.clone())
                .domains(vec![hostname.clone()])
                .contact(vec![format!("mailto:{contact}")])
                .cache_path(state_dir.join("relay-acme"));
            Some(TlsConfig::new(
                *https,
                CertConfig::LetsEncrypt {
                    acme_config,
                    server_config_builder: builder,
                },
            ))
        }
    })
}

/// The relay's access hook: who it lets through.
#[derive(Debug)]
struct Gate {
    access: Access,
    server: EndpointId,
    asker: Asker,
}

impl Gate {
    async fn new(config: &RelayConfig, key: SecretKey) -> Result<Self, RelayError> {
        Ok(Self {
            access: config.access,
            server: config.server.id,
            asker: Asker::new(key, config.server.clone()).await?,
        })
    }

    /// Whether `endpoint` may relay through it.
    async fn admits(&self, endpoint: EndpointId) -> bool {
        // The main server homes on it; and with everyone let through
        // there is nothing to ask.
        if endpoint == self.server || self.access.everyone {
            return true;
        }
        match self.asker.tier(endpoint).await {
            Tier::Owner => self.access.owner,
            Tier::Friend => self.access.friends,
            Tier::Stranger | Tier::Unknown => self.access.everyone,
        }
    }
}

impl AccessControl for Gate {
    async fn on_connect(&self, request: &ClientRequest) -> iroh_relay::server::Access {
        let endpoint = request.endpoint_id();
        if self.admits(endpoint).await {
            tracing::debug!(endpoint = %endpoint.fmt_short(), "relaying for an endpoint");
            iroh_relay::server::Access::Allow
        } else {
            tracing::debug!(endpoint = %endpoint.fmt_short(), "refusing an endpoint");
            iroh_relay::server::Access::Deny {
                reason: Some("this relay is for its server's own devices".into()),
            }
        }
    }

    fn on_disconnect(&self, _endpoint: EndpointId, _connection: iroh_relay::server::ConnectionId) {}
}

/// Asks the main server which tier an endpoint is in, over one kept
/// connection, keeping each answer [`CACHE`].
#[derive(Debug)]
struct Asker {
    endpoint: Endpoint,
    server: EndpointAddr,
    connection: tokio::sync::Mutex<Option<Connection>>,
    answers: parking_lot::Mutex<HashMap<EndpointId, (Tier, Instant)>>,
}

impl Asker {
    async fn new(key: SecretKey, server: EndpointAddr) -> Result<Self, RelayError> {
        // Direct to the main server: it cannot ask through itself.
        let endpoint = Endpoint::builder(presets::Minimal)
            .secret_key(key)
            .relay_mode(RelayMode::Disabled)
            .bind()
            .await
            .map_err(|e| RelayError::Endpoint(e.to_string()))?;
        Ok(Self {
            endpoint,
            server,
            connection: tokio::sync::Mutex::default(),
            answers: parking_lot::Mutex::default(),
        })
    }

    async fn tier(&self, endpoint: EndpointId) -> Tier {
        let now = Instant::now();
        if let Some((tier, until)) = self.answers.lock().get(&endpoint).copied()
            && now < until
        {
            return tier;
        }
        let (tier, keep) = match tokio::time::timeout(ASK_WAIT, self.ask(endpoint)).await {
            Ok(Ok(tier)) => (tier, CACHE),
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "cannot ask the main server who an endpoint is");
                (Tier::Stranger, FAILED_CACHE)
            }
            Err(_) => {
                tracing::warn!("the main server did not say who an endpoint is in time");
                (Tier::Stranger, FAILED_CACHE)
            }
        };
        let mut answers = self.answers.lock();
        answers.retain(|_, (_, until)| now < *until);
        answers.insert(endpoint, (tier, now + keep));
        tier
    }

    async fn ask(&self, endpoint: EndpointId) -> Result<Tier, String> {
        let connection = self.connection().await?;
        let asked = async {
            let (mut send, mut recv) = connection.open_bi().await.map_err(|e| e.to_string())?;
            write_frame(
                &mut send,
                &AccessQuery {
                    endpoint: endpoint.to_string(),
                },
            )
            .await
            .map_err(|e| e.to_string())?;
            send.finish().map_err(|e| e.to_string())?;
            let answer: AccessAnswer = read_frame_within(&mut recv, 1024)
                .await
                .map_err(|e| e.to_string())?;
            Ok(answer.tier)
        };
        let answer = asked.await;
        if answer.is_err() {
            // Dialled again next time.
            self.connection.lock().await.take();
        }
        answer
    }

    /// The kept connection, dialled if there is none or it closed.
    async fn connection(&self) -> Result<Connection, String> {
        let mut kept = self.connection.lock().await;
        if let Some(connection) = kept.as_ref()
            && connection.close_reason().is_none()
        {
            return Ok(connection.clone());
        }
        let connection = self
            .endpoint
            .connect(self.server.clone(), RELAY_ACCESS_ALPN)
            .await
            .map_err(|e| e.to_string())?;
        *kept = Some(connection.clone());
        Ok(connection)
    }
}
