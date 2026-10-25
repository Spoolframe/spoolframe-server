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
//! passed on in the question. An endpoint it carries is asked about again
//! every [`RECHECK`], and cut once it is no longer let through (P6-07,
//! glm L-1). The main server tells it when an answer no longer holds, a
//! paired server unpaired or moved to a new key: that endpoint's answers
//! are forgotten and those it carries asked about again, at once (C8-01).
//! Relayed traffic stays encrypted end to end; the relay learns only
//! which keys talk.

pub mod config;

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use iroh::endpoint::{Connection, presets};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, SecretKey};
use iroh_relay::server::{
    AccessControl, AcmeConfig, CertConfig, ClientRequest, ConnectionId, QuicConfig,
    RelayConfig as IrohRelay, Server, ServerConfig, TlsConfig, clients::Clients,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use spoolframe_protocol::codec::{read_frame_within, write_frame};
use spoolframe_protocol::relay_access::{
    AccessAnswer, AccessQuery, AccessRevoked, MAX_FRAME, RELAY_ACCESS_ALPN, Tier, is_relay_token,
};

pub use config::{Access, RelayConfig, RelayConfigError, Serve};

/// How long the main server's answer for an endpoint is kept, with or
/// without an invite token. It decides only whether a connection is
/// admitted: one that is carried is asked about again every [`RECHECK`].
pub const CACHE: Duration = Duration::from_secs(60);
/// How often the main server is asked again about each endpoint the relay
/// carries (P6-07, glm L-1). iroh keeps one relay connection per endpoint
/// for as long as it can, so a gate run only on connecting let a device
/// signed out, or an invite revoked, keep relaying until it happened to
/// reconnect. A carried endpoint no longer let through is cut within this
/// and one `ASK_WAIT`.
pub const RECHECK: Duration = Duration::from_secs(5);
/// The questions a recheck has open at once: the main server holds each
/// relay connection to 4 streams, and the questions of endpoints that are
/// connecting need room too.
const RECHECK_ASKS: usize = 2;
/// How long a failed answer is kept: the main server unreachable is asked
/// again soon, but not on every connection.
const FAILED_CACHE: Duration = Duration::from_secs(5);
/// How long a question may take.
const ASK_WAIT: Duration = Duration::from_secs(5);
/// The most answers kept (P6-07, glm E-2). Endpoint keys are free, so a
/// flood of fresh ones would otherwise grow the cache by its rate times
/// [`CACHE`]; past this, the answer that would expire first goes.
const MAX_ANSWERS: usize = 10_000;
/// How long an endpoint the main server said no longer holds is kept as
/// such with no fresh answer for it (X8-04), and the most kept so: one
/// that never comes back does not stay for ever. Only the main server
/// names them, so both are far above what it says in that time.
const REVOKED_FOR: Duration = Duration::from_secs(60 * 60);
const MAX_REVOKED: usize = 1_000;

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
    let gate = Arc::new(Gate::new(config, key).await?);
    let mut relay = IrohRelay::new(config.http);
    relay.tls = tls(&config.serve, state_dir).await?;
    relay.access = gate.clone();
    let mut server = ServerConfig::default();
    server.relay = Some(relay);
    server.quic = config.quic.map(QuicConfig::new);
    let server = Server::spawn(server)
        .await
        .map_err(|e| RelayError::Spawn(e.to_string()))?;
    // Always there, for a server spawned with a relay; if it were not,
    // carried endpoints would never be cut, so it is said, not skipped
    // (review 2026-10-11 L51).
    match server.relay_service() {
        Some(service) => {
            tokio::spawn(recheck(Arc::downgrade(&gate), service.clients().clone()));
        }
        None => tracing::error!(
            "the relay has no relay service: endpoints it carries are not cut when revoked"
        ),
    }
    Ok(server)
}

/// Ask again, every [`RECHECK`], about each endpoint `gate` carries, and
/// cut the connections of those no longer let through. It ends when the
/// relay does: the gate is the relay's, and goes with it.
/// The main server saying an answer no longer holds asks again at once.
async fn recheck(gate: Weak<Gate>, clients: Clients) {
    let Some(revoked) = gate.upgrade().map(|gate| gate.asker.revoked.clone()) else {
        return;
    };
    let mut every = tokio::time::interval(RECHECK);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = every.tick() => {}
            () = revoked.notified() => {}
        }
        let Some(gate) = gate.upgrade() else {
            return;
        };
        for (endpoint, connection) in gate.revoked().await {
            tracing::info!(
                endpoint = %endpoint.fmt_short(),
                "cutting an endpoint the relay no longer lets through"
            );
            clients.disconnect(endpoint, Some(connection));
        }
    }
}

/// The relay's TLS, as `serve` says; none behind a proxy. TLS 1.3 only
/// (P6-07, glm L-3): every iroh client speaks it, and rustls' defaults
/// would offer 1.2 as well.
async fn tls(serve: &Serve, state_dir: &Path) -> Result<Option<TlsConfig>, RelayError> {
    let builder = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
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
    /// The connections it let through on the main server's answer, and
    /// what that answer was for: each is asked about again (L-1).
    carried: parking_lot::Mutex<HashMap<ConnectionId, Asked>>,
}

impl Gate {
    async fn new(config: &RelayConfig, key: SecretKey) -> Result<Self, RelayError> {
        Ok(Self {
            access: config.access,
            server: config.server.id,
            asker: Asker::new(key, config.server.clone()).await?,
            carried: parking_lot::Mutex::default(),
        })
    }

    /// Whether `endpoint` is let through without asking: the main server
    /// homes on it; and with everyone let through there is nothing to ask.
    fn needs_no_answer(&self, endpoint: EndpointId) -> bool {
        endpoint == self.server || self.access.everyone
    }

    /// Whether an endpoint in `tier` may relay through it.
    fn lets_through(&self, tier: Tier) -> bool {
        match tier {
            Tier::Owner => self.access.owner,
            Tier::Friend => self.access.friends,
            Tier::Stranger | Tier::Unknown => self.access.everyone,
        }
    }

    /// The carried connections whose endpoints the main server now puts
    /// in a tier not let through, each asked about afresh
    /// ([`Gate::no_longer_let_through`]).
    async fn revoked(&self) -> Vec<(EndpointId, ConnectionId)> {
        let carried: Vec<(ConnectionId, Asked)> = self
            .carried
            .lock()
            .iter()
            .map(|(id, asked)| (*id, asked.clone()))
            .collect();
        let asked: HashSet<Asked> = carried.iter().map(|(_, asked)| asked.clone()).collect();
        let refused = self.no_longer_let_through(asked).await;
        carried
            .into_iter()
            .filter(|(_, asked)| refused.contains(asked))
            .map(|(id, (endpoint, _))| (endpoint, id))
            .collect()
    }

    /// Those of `carried` that are not to be carried on, each asked about
    /// afresh: an endpoint the main server now puts in a tier not let
    /// through; and one it said no longer holds and cannot answer for now
    /// (X8-04), which is carried on nothing until it answers afresh. Any
    /// other endpoint it cannot answer for is kept: a main server out of
    /// reach does not cut every device, and is no worse than before a
    /// recheck.
    async fn no_longer_let_through(&self, carried: HashSet<Asked>) -> HashSet<Asked> {
        let revoked = self.asker.revoked_now();
        futures_util::stream::iter(carried)
            .map(|asked| {
                let revoked = &revoked;
                async move {
                    match self.asker.fresh_tier(&asked).await {
                        Some(tier) => (!self.lets_through(tier)).then_some(asked),
                        None if revoked.contains(&asked.0) => Some(asked),
                        None => None,
                    }
                }
            })
            .buffer_unordered(RECHECK_ASKS)
            .filter_map(std::future::ready)
            .collect()
            .await
    }
}

impl AccessControl for Gate {
    async fn on_connect(&self, request: &ClientRequest) -> iroh_relay::server::Access {
        let endpoint = request.endpoint_id();
        // Only an invite holder's or a play token holder's (P8-04) token is
        // passed on: anything else in the header is no token, and does not
        // split the endpoint's answers.
        let invite = request.auth_token().filter(|token| is_relay_token(token));
        let admitted = self.needs_no_answer(endpoint) || {
            let tier = self.asker.tier(endpoint, invite.clone()).await;
            let admitted = self.lets_through(tier);
            if admitted {
                // Kept until `on_disconnect`, which iroh-relay calls for
                // every connection let through.
                self.carried
                    .lock()
                    .insert(request.connection_id(), (endpoint, invite));
            }
            admitted
        };
        if admitted {
            tracing::debug!(endpoint = %endpoint.fmt_short(), "relaying for an endpoint");
            iroh_relay::server::Access::Allow
        } else {
            tracing::debug!(endpoint = %endpoint.fmt_short(), "refusing an endpoint");
            iroh_relay::server::Access::Deny {
                reason: Some("this relay is for its server's own devices".into()),
            }
        }
    }

    fn on_disconnect(&self, _endpoint: EndpointId, connection: ConnectionId) {
        self.carried.lock().remove(&connection);
    }
}

/// Asks the main server which tier an endpoint is in, over one kept
/// connection, keeping each answer [`CACHE`], and hears on it which no
/// longer hold.
#[derive(Debug)]
struct Asker {
    endpoint: Endpoint,
    server: EndpointAddr,
    connection: tokio::sync::Mutex<Option<Connection>>,
    answers: Arc<parking_lot::Mutex<Answers>>,
    /// Told each time the main server says an answer no longer holds.
    revoked: Arc<tokio::sync::Notify>,
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
    /// Moved on each time answers are forgotten: an answer asked for
    /// before is not kept, as it may be one that no longer holds.
    epoch: u64,
    /// The endpoints the main server said no longer hold, and when, each
    /// until it answers for it afresh or [`REVOKED_FOR`] (X8-04): a
    /// connection of one is not carried on through a question that fails,
    /// as other connections are. At most [`MAX_REVOKED`]; past them the
    /// one said longest ago goes.
    revoked: HashMap<EndpointId, Instant>,
}

impl Default for Answers {
    fn default() -> Self {
        Self {
            kept: HashMap::new(),
            max: MAX_ANSWERS,
            epoch: 0,
            revoked: HashMap::new(),
        }
    }
}

impl Answers {
    /// Forget what was kept for `endpoint`, with any token: it no longer
    /// holds, until the main server answers for it again.
    fn forget(&mut self, endpoint: EndpointId, now: Instant) {
        self.kept.retain(|(kept, _), _| *kept != endpoint);
        self.epoch += 1;
        if self.revoked.len() >= MAX_REVOKED && !self.revoked.contains_key(&endpoint) {
            self.revoked.retain(|_, at| now < *at + REVOKED_FOR);
            if let Some(oldest) = self
                .revoked
                .iter()
                .min_by_key(|(_, at)| **at)
                .map(|(e, _)| *e)
                && self.revoked.len() >= MAX_REVOKED
            {
                self.revoked.remove(&oldest);
            }
        }
        self.revoked.insert(endpoint, now);
    }

    /// The main server answered `asked` afresh, on `epoch`: the answer
    /// that no longer held for its endpoint is replaced, if the question
    /// was asked after it was forgotten (one in flight across the
    /// forgetting may be the old answer) and is the endpoint's own, with
    /// no token, as a pair's or a server's connection is.
    fn answered(&mut self, asked: &Asked, epoch: u64) {
        if epoch == self.epoch && asked.1.is_none() {
            self.revoked.remove(&asked.0);
        }
    }

    /// Whether `endpoint`'s answer no longer holds, as of `now`.
    fn is_revoked(&self, endpoint: &EndpointId, now: Instant) -> bool {
        self.revoked
            .get(endpoint)
            .is_some_and(|at| now < *at + REVOKED_FOR)
    }

    /// Forget every answer: what the main server said while this relay
    /// was not connected to it was not heard.
    fn forget_all(&mut self) {
        self.kept.clear();
        self.epoch += 1;
    }

    /// Keep `tier` for `asked` as [`Answers::keep`] does, if nothing was
    /// forgotten since `epoch`, when it was asked for.
    fn keep_since(&mut self, epoch: u64, asked: Asked, tier: Tier, until: Instant, now: Instant) {
        if self.epoch == epoch {
            self.keep(asked, tier, until, now);
        }
    }

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
            answers: Arc::default(),
            revoked: Arc::default(),
        })
    }

    async fn tier(&self, endpoint: EndpointId, invite: Option<String>) -> Tier {
        let now = Instant::now();
        let asked = (endpoint, invite);
        if let Some(tier) = self.answers.lock().get(&asked, now) {
            return tier;
        }
        let epoch = self.answers.lock().epoch;
        let (tier, keep, epoch, answered) = match self.ask_within(&asked).await {
            Some((tier, epoch)) => (tier, CACHE, epoch, true),
            None => (Tier::Stranger, FAILED_CACHE, epoch, false),
        };
        let mut answers = self.answers.lock();
        if answered {
            answers.answered(&asked, epoch);
        }
        answers.keep_since(epoch, asked, tier, now + keep, now);
        tier
    }

    /// `asked`'s tier, asked now whatever is kept, and kept for the
    /// connections still to come; `None` if the main server did not say.
    /// A failure replaces nothing: the answer kept stands.
    async fn fresh_tier(&self, asked: &Asked) -> Option<Tier> {
        let now = Instant::now();
        let (tier, epoch) = self.ask_within(asked).await?;
        let mut answers = self.answers.lock();
        answers.answered(asked, epoch);
        answers.keep_since(epoch, asked.clone(), tier, now + CACHE, now);
        Some(tier)
    }

    /// The endpoints the main server said no longer hold and has not
    /// answered for since (X8-04), as of now.
    fn revoked_now(&self) -> HashSet<EndpointId> {
        let now = Instant::now();
        let answers = self.answers.lock();
        answers
            .revoked
            .keys()
            .filter(|endpoint| answers.is_revoked(endpoint, now))
            .copied()
            .collect()
    }

    /// `asked`'s tier from the main server, waiting [`ASK_WAIT`] at most,
    /// and the answers' epoch as it was asked on its connection.
    async fn ask_within(&self, asked: &Asked) -> Option<(Tier, u64)> {
        match tokio::time::timeout(ASK_WAIT, self.ask(asked)).await {
            Ok(Ok(answered)) => Some(answered),
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "cannot ask the main server who an endpoint is");
                None
            }
            Err(_) => {
                tracing::warn!("the main server did not say who an endpoint is in time");
                None
            }
        }
    }

    async fn ask(&self, (endpoint, invite): &Asked) -> Result<(Tier, u64), String> {
        let connection = self.connection().await?;
        // Read once the connection is had: a new one forgets what was
        // kept before, and an answer on it is not one from before.
        let epoch = self.answers.lock().epoch;
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
            let answer: AccessAnswer = read_frame_within(&mut recv, MAX_FRAME)
                .await
                .map_err(|e| e.to_string())?;
            Ok((answer.tier, epoch))
        };
        let answer = asked.await;
        if answer.is_err() {
            // Dialled again next time.
            self.connection.lock().await.take();
        }
        answer
    }

    /// The kept connection, dialled if there is none or it closed. A new
    /// one starts with no answer kept: what the main server said of them
    /// with none open was not heard.
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
        self.answers.lock().forget_all();
        tokio::spawn(hear_revoked(
            connection.clone(),
            self.answers.clone(),
            self.revoked.clone(),
        ));
        *kept = Some(connection.clone());
        Ok(connection)
    }
}

/// What the main server says no longer holds on `connection`, until it
/// closes: each endpoint named is forgotten from `answers`, and `revoked`
/// told, so those carried are asked about again.
async fn hear_revoked(
    connection: Connection,
    answers: Arc<parking_lot::Mutex<Answers>>,
    revoked: Arc<tokio::sync::Notify>,
) {
    while let Ok(mut recv) = connection.accept_uni().await {
        let heard = tokio::time::timeout(ASK_WAIT, read_frame_within(&mut recv, MAX_FRAME)).await;
        let Ok(Ok(AccessRevoked { endpoint })) = heard else {
            continue;
        };
        let Ok(endpoint) = endpoint.parse::<EndpointId>() else {
            continue;
        };
        tracing::info!(
            endpoint = %endpoint.fmt_short(),
            "the main server says an endpoint's answer no longer holds"
        );
        answers.lock().forget(endpoint, Instant::now());
        revoked.notify_one();
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

    /// What the main server revokes goes, with any token, and an answer
    /// asked for before that is not kept: it may be the one revoked.
    #[test]
    fn a_revoked_endpoint_is_forgotten_and_an_answer_asked_before_not_kept() {
        let now = Instant::now();
        let mut answers = Answers::default();
        let (endpoint, _) = key(1);
        let token = Some("sf-play.1.a".to_owned());
        answers.keep((endpoint, None), Tier::Friend, now + CACHE, now);
        answers.keep((endpoint, token.clone()), Tier::Friend, now + CACHE, now);
        answers.keep(key(2), Tier::Friend, now + CACHE, now);
        let asked = answers.epoch;
        answers.forget(endpoint, now);
        assert_eq!(answers.get(&(endpoint, None), now), None);
        assert_eq!(answers.get(&(endpoint, token), now), None);
        assert_eq!(answers.get(&key(2), now), Some(Tier::Friend));
        answers.keep_since(asked, (endpoint, None), Tier::Friend, now + CACHE, now);
        assert_eq!(answers.get(&(endpoint, None), now), None, "asked before");
        let now_asked = answers.epoch;
        answers.keep_since(
            now_asked,
            (endpoint, None),
            Tier::Stranger,
            now + CACHE,
            now,
        );
        assert_eq!(answers.get(&(endpoint, None), now), Some(Tier::Stranger));
    }

    /// The audit's C8-01: a paired server's old key, its answer kept as a
    /// friend's, is asked about again as soon as the main server says the
    /// answer no longer holds, not once it expires.
    #[tokio::test]
    async fn an_answer_the_main_server_revokes_is_asked_for_again_at_once() {
        // The main server, as far as the questions: its answer is `tier`.
        let main = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .alpns(vec![RELAY_ACCESS_ALPN.to_vec()])
            .bind()
            .await
            .unwrap();
        let tier = Arc::new(parking_lot::Mutex::new(Tier::Friend));
        let (connected, relay) = tokio::sync::oneshot::channel();
        let (accepting, answering) = (main.clone(), tier.clone());
        tokio::spawn(async move {
            let connection = accepting.accept().await.unwrap().await.unwrap();
            let _ = connected.send(connection.clone());
            while let Ok((mut send, mut recv)) = connection.accept_bi().await {
                let _: AccessQuery = read_frame_within(&mut recv, MAX_FRAME).await.unwrap();
                let tier = *answering.lock();
                write_frame(&mut send, &AccessAnswer { tier })
                    .await
                    .unwrap();
                send.finish().unwrap();
            }
        });
        let asker = Asker::new(SecretKey::from_bytes(&[3; 32]), main.addr())
            .await
            .unwrap();
        let (old_key, _) = key(1);
        assert_eq!(asker.tier(old_key, None).await, Tier::Friend);
        *tier.lock() = Tier::Stranger;
        assert_eq!(asker.tier(old_key, None).await, Tier::Friend, "kept");
        let relay = relay.await.unwrap();
        let mut send = relay.open_uni().await.unwrap();
        let revoked = AccessRevoked {
            endpoint: old_key.to_string(),
        };
        write_frame(&mut send, &revoked).await.unwrap();
        send.finish().unwrap();
        tokio::time::timeout(Duration::from_secs(10), asker.revoked.notified())
            .await
            .expect("the relay hears it");
        assert_eq!(asker.tier(old_key, None).await, Tier::Stranger);
    }

    /// The review of X8-04: an endpoint said to no longer hold stays so
    /// through an answer asked before it was said (the old answer, maybe)
    /// and through one for the endpoint with a token; an answer asked
    /// after, for the endpoint itself, ends it. One never answered for
    /// again ends with its time, and past the cap the oldest goes.
    #[test]
    fn a_revoked_endpoint_stays_so_through_a_stale_or_a_token_s_answer() {
        let now = Instant::now();
        let mut answers = Answers::default();
        let (endpoint, _) = key(1);
        let before = answers.epoch;
        answers.forget(endpoint, now);
        answers.answered(&(endpoint, None), before);
        assert!(
            answers.is_revoked(&endpoint, now),
            "asked before it was said"
        );
        let token = Some("sf-play.1.a".to_owned());
        answers.answered(&(endpoint, token), answers.epoch);
        assert!(answers.is_revoked(&endpoint, now), "a token's answer");
        answers.answered(&(endpoint, None), answers.epoch);
        assert!(!answers.is_revoked(&endpoint, now));
        answers.forget(endpoint, now);
        assert!(
            !answers.is_revoked(&endpoint, now + REVOKED_FOR),
            "timed out"
        );
        for n in 1..=MAX_REVOKED {
            let mut bytes = [7; 32];
            bytes[..2].copy_from_slice(&u16::try_from(n).unwrap().to_le_bytes());
            let later = SecretKey::from_bytes(&bytes).public();
            answers.forget(later, now + Duration::from_secs(1));
        }
        assert_eq!(answers.revoked.len(), MAX_REVOKED);
        assert!(!answers.revoked.contains_key(&endpoint), "the oldest went");
    }

    /// The audit's X8-04: a carried endpoint whose answer the main server
    /// revoked is cut at the next recheck though the main server cannot
    /// be asked then, and is let through again only once it answers
    /// afresh; an endpoint not revoked is carried on through such a
    /// failure, as before. The main server that cannot be asked resets
    /// the question's stream, which is no answer, as a question that
    /// times out is.
    #[tokio::test]
    async fn a_revoked_endpoint_is_cut_though_the_main_server_cannot_answer() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let main = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .alpns(vec![RELAY_ACCESS_ALPN.to_vec()])
            .bind()
            .await
            .unwrap();
        let answering = Arc::new(AtomicBool::new(true));
        let (connected, relay) = tokio::sync::oneshot::channel();
        let (accepting, answers) = (main.clone(), answering.clone());
        // Every connection: the relay dials again after a failed question.
        tokio::spawn(async move {
            let mut connected = Some(connected);
            while let Some(incoming) = accepting.accept().await {
                let Ok(connection) = incoming.await else {
                    continue;
                };
                if let Some(connected) = connected.take() {
                    let _ = connected.send(connection.clone());
                }
                let answers = answers.clone();
                tokio::spawn(async move {
                    while let Ok((mut send, mut recv)) = connection.accept_bi().await {
                        let _: AccessQuery = read_frame_within(&mut recv, MAX_FRAME).await.unwrap();
                        if answers.load(Ordering::Acquire) {
                            write_frame(&mut send, &AccessAnswer { tier: Tier::Friend })
                                .await
                                .unwrap();
                            send.finish().unwrap();
                        } else {
                            let _ = send.reset(0u32.into());
                        }
                    }
                });
            }
        });
        let gate = Gate {
            access: Access::default(),
            server: main.id(),
            asker: Asker::new(SecretKey::from_bytes(&[4; 32]), main.addr())
                .await
                .unwrap(),
            carried: parking_lot::Mutex::default(),
        };
        let (old_key, other) = (key(1), key(2));
        for (endpoint, _) in [&old_key, &other] {
            assert_eq!(gate.asker.tier(*endpoint, None).await, Tier::Friend);
        }
        let carried: HashSet<Asked> = [old_key.clone(), other.clone()].into();
        assert!(gate.no_longer_let_through(carried.clone()).await.is_empty());
        let relay = relay.await.unwrap();
        let mut send = relay.open_uni().await.unwrap();
        let revoked = AccessRevoked {
            endpoint: old_key.0.to_string(),
        };
        write_frame(&mut send, &revoked).await.unwrap();
        send.finish().unwrap();
        tokio::time::timeout(Duration::from_secs(10), gate.asker.revoked.notified())
            .await
            .expect("the relay hears it");
        answering.store(false, Ordering::Release);
        let cut: HashSet<Asked> = [old_key.clone()].into();
        assert_eq!(
            gate.no_longer_let_through(carried.clone()).await,
            cut,
            "the revoked key is cut, unanswered; the other is carried on"
        );
        // Blocked still: a new connection of its is no friend, and it is
        // cut again at the next recheck.
        assert_eq!(gate.asker.tier(old_key.0, None).await, Tier::Stranger);
        assert_eq!(gate.no_longer_let_through(carried.clone()).await, cut);
        answering.store(true, Ordering::Release);
        assert!(
            gate.no_longer_let_through(carried).await.is_empty(),
            "answered afresh: let through again"
        );
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
