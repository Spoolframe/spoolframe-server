//! A relay of the operator's own (P6-04): `spoolframe-server relay`, the
//! same binary with no catalog, users or admin, only an iroh relay, for
//! devices that cannot reach the server directly when n0's public relays
//! are not wanted in the path. Its URL goes in the main server's
//! `network.relays`; its key in the main server's `network.own_relays`.
//!
//! Who it relays for is by tier: the main server says which tier an
//! endpoint is in ([`crate::relay_access`]), asked once a minute at most
//! for each, and the config says which tiers are let through. A device
//! redeeming an invite connects with the invite holder's token (P6-08),
//! passed on in the question. Relayed traffic stays encrypted end to end;
//! the relay learns only which keys talk.

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
use spoolframe_protocol::relay_access::{
    AccessAnswer, AccessQuery, RELAY_ACCESS_ALPN, Tier, parse_invite_token,
};

pub use config::{Access, RelayConfig, RelayConfigError, Serve};

/// How long the main server's answer for an endpoint is kept, with or
/// without an invite token: a device signed out, or an invite revoked, is
/// let through this much longer at most.
pub const CACHE: Duration = Duration::from_secs(60);
/// How long a failed answer is kept: the main server unreachable is asked
/// again soon, but not on every connection.
const FAILED_CACHE: Duration = Duration::from_secs(5);
/// How long a question may take.
const ASK_WAIT: Duration = Duration::from_secs(5);
/// The most answers kept (P6-07, glm E-2). Endpoint keys are free, so a
/// flood of fresh ones would otherwise grow the cache by its rate times
/// [`CACHE`]; past this, the answer that would expire first goes.
const MAX_ANSWERS: usize = 10_000;

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

    /// Whether `endpoint` may relay through it, connected with `invite`.
    async fn admits(&self, endpoint: EndpointId, invite: Option<String>) -> bool {
        // The main server homes on it; and with everyone let through
        // there is nothing to ask.
        if endpoint == self.server || self.access.everyone {
            return true;
        }
        match self.asker.tier(endpoint, invite).await {
            Tier::Owner => self.access.owner,
            Tier::Friend => self.access.friends,
            Tier::Stranger | Tier::Unknown => self.access.everyone,
        }
    }
}

impl AccessControl for Gate {
    async fn on_connect(&self, request: &ClientRequest) -> iroh_relay::server::Access {
        let endpoint = request.endpoint_id();
        // Only an invite holder's token is passed on: anything else in the
        // header is no token, and does not split the endpoint's answers.
        let invite = request
            .auth_token()
            .filter(|token| parse_invite_token(token).is_some());
        if self.admits(endpoint, invite).await {
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
    answers: parking_lot::Mutex<Answers>,
}

/// What an answer is for: an endpoint, and the invite token it connected
/// with, if any (P6-08). The same endpoint with and without one may be
/// answered differently.
type Asked = (EndpointId, Option<String>);

/// The answers kept, each until it expires, at most `max` of them.
#[derive(Debug)]
struct Answers {
    kept: HashMap<Asked, (Tier, Instant)>,
    max: usize,
}

impl Default for Answers {
    fn default() -> Self {
        Self {
            kept: HashMap::new(),
            max: MAX_ANSWERS,
        }
    }
}

impl Answers {
    /// `asked`'s answer, if it has not expired at `now`.
    fn get(&self, asked: &Asked, now: Instant) -> Option<Tier> {
        self.kept
            .get(asked)
            .filter(|(_, until)| now < *until)
            .map(|(tier, _)| *tier)
    }

    /// Keep `tier` for `asked` until `until`. Only a full cache is
    /// swept: the expired go, and if that frees nothing, the tenth that
    /// would expire soonest. A flood of fresh keys then scans the map once
    /// per tenth of `max` new keys, not once per key, under the lock every
    /// connection's lookup takes.
    fn keep(&mut self, asked: Asked, tier: Tier, until: Instant, now: Instant) {
        if self.kept.len() >= self.max && !self.kept.contains_key(&asked) {
            self.kept.retain(|_, (_, at)| now < *at);
            if self.kept.len() >= self.max {
                let mut by_expiry: Vec<(Instant, Asked)> = self
                    .kept
                    .iter()
                    .map(|(key, (_, at))| (*at, key.clone()))
                    .collect();
                let evict = (self.max / 10).max(1);
                by_expiry.select_nth_unstable(evict - 1);
                for (_, key) in &by_expiry[..evict] {
                    self.kept.remove(key);
                }
            }
        }
        self.kept.insert(asked, (tier, until));
    }
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

    async fn tier(&self, endpoint: EndpointId, invite: Option<String>) -> Tier {
        let now = Instant::now();
        let asked = (endpoint, invite);
        if let Some(tier) = self.answers.lock().get(&asked, now) {
            return tier;
        }
        let (tier, keep) = match tokio::time::timeout(ASK_WAIT, self.ask(&asked)).await {
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
        self.answers.lock().keep(asked, tier, now + keep, now);
        tier
    }

    async fn ask(&self, (endpoint, invite): &Asked) -> Result<Tier, String> {
        let connection = self.connection().await?;
        let asked = async {
            let (mut send, mut recv) = connection.open_bi().await.map_err(|e| e.to_string())?;
            write_frame(
                &mut send,
                &AccessQuery {
                    endpoint: endpoint.to_string(),
                    invite: invite.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> Asked {
        (SecretKey::from_bytes(&[n; 32]).public(), None)
    }

    #[test]
    fn answers_past_the_cap_drop_the_one_that_expires_soonest() {
        let now = Instant::now();
        let mut answers = Answers {
            max: 3,
            ..Answers::default()
        };
        for n in 1..=3 {
            let until = now + Duration::from_secs(10 * u64::from(n));
            answers.keep(key(n), Tier::Stranger, until, now);
        }
        answers.keep(key(4), Tier::Owner, now + CACHE, now);
        assert_eq!(answers.kept.len(), 3);
        assert_eq!(answers.get(&key(1), now), None);
        assert_eq!(answers.get(&key(2), now), Some(Tier::Stranger));
        assert_eq!(answers.get(&key(4), now), Some(Tier::Owner));
    }

    #[test]
    fn an_expired_answer_is_not_given_and_goes_before_a_live_one() {
        let now = Instant::now();
        let mut answers = Answers {
            max: 2,
            ..Answers::default()
        };
        answers.keep(key(1), Tier::Owner, now + FAILED_CACHE, now);
        answers.keep(key(2), Tier::Owner, now + CACHE, now);
        let later = now + FAILED_CACHE + Duration::from_secs(1);
        assert_eq!(answers.get(&key(1), later), None);
        answers.keep(key(3), Tier::Owner, later + CACHE, later);
        assert_eq!(answers.get(&key(2), later), Some(Tier::Owner));
        assert_eq!(answers.get(&key(3), later), Some(Tier::Owner));
        assert_eq!(answers.kept.len(), 2);
    }

    /// P6-08: an endpoint's answer with an invite token is not its answer
    /// without one, nor with another token.
    #[test]
    fn answers_are_kept_by_endpoint_and_token() {
        let now = Instant::now();
        let mut answers = Answers::default();
        let (endpoint, _) = key(1);
        let token = Some("sf-invite.1.a".to_owned());
        answers.keep((endpoint, token.clone()), Tier::Friend, now + CACHE, now);
        answers.keep((endpoint, None), Tier::Stranger, now + CACHE, now);
        assert_eq!(answers.get(&(endpoint, token), now), Some(Tier::Friend));
        assert_eq!(answers.get(&(endpoint, None), now), Some(Tier::Stranger));
        let other = Some("sf-invite.1.b".to_owned());
        assert_eq!(answers.get(&(endpoint, other), now), None);
    }
}
