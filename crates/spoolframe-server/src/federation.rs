//! Federation (P8-01): this server's pairs with others, over
//! `spoolframe-federation`'s handler and dialler; what it shares with them
//! (P8-03's grants), and how it answers their replication (P8-02).
//!
//! Experimental: there is a [`Federation`] only while
//! `experimental.federation` is on. Off, `spoolframe/federation/1` closes
//! at once and a `federation` invite is refused, as before P8.
//!
//! Both sides of a pair keep a `federation_peers` row. The server whose
//! `federation` invite is redeemed (SA) pins the redeemer's endpoint and
//! server id ([`Peers::redeem`]); the redeemer (SB) keeps SA's as its
//! admin pasted SA's link ([`Federation::redeem_link`]).
//!
//! A stranger on the ALPN is held, counted and limited as a device with no
//! session is on the client protocol: the same cap on held connections,
//! the same strangers' budget for a redemption, and the same count of
//! wrong guesses at a code (`invite@<endpoint>`), which a refused stream
//! adds to as a wrong code does. A new key that says it is a paired
//! server spends from the same budget each time it says hello, so turning
//! keys does not escape the count each key has.

mod answer;
pub mod cache;
mod exposure;
mod grants;
pub mod items;
mod limits;
mod media;
mod proxy;
mod redeem;
mod replica;
mod tokens;

pub use cache::{Links, PeerCache};
pub use exposure::{PeerPage, PlayDay, Received, ServerRowError};
pub use grants::ShareError;
pub use limits::Limits;
pub use proxy::{
    ORIGIN_STALL, ProxyError, ProxyStart, ProxyStarted, RelayedRead, RemotePlay, Stalling,
};
pub use redeem::PlayTokenError;
pub use replica::{DEFAULT_SYNC_EVERY, ReplicaError, SyncLimits};
pub use tokens::{AssertError, PLAY_TOKEN_LIFETIME};

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use iroh::{Endpoint, EndpointAddr, EndpointId, RelayUrl, TransportAddr};
use spoolframe_catalog::Catalog;
use spoolframe_federation::{
    DialError, Dialed, FederationProtocol, Live, Peers, PlayClient, Strangers,
};
use spoolframe_protocol::federation::{
    MAX_HINTS, PROTOCOL_VERSION, PairingRefused, PeerHello, PeerInfo, RedeemPairing, Standing,
    server_name,
};
use spoolframe_protocol::invite::{InviteLinkKind, is_relay_url, parse_link};
use spoolframe_protocol::invite_admin::InviteKind as LinkedKind;
use spoolframe_protocol::{
    Error as ProtocolError, Hello, InviteLookupOutcome, LookupInvite, Reply, Request, Response,
    ServerId, close, features, fingerprint,
};
use spoolframe_types::Platform;

use crate::announce::Told;
use crate::auth::Auth;
use crate::invites::RedeemError;
use crate::keyed_locks::KeyedLocks;
use crate::network::{Connections, ServerName};
use crate::store::StoreError;
use crate::store::federation::{HeldKey, KeyHold, NewPeer, PairRedeemed, PeerError, PeerRecord};
use crate::store::federation_cache::FileLimits;
use crate::store::invites::{InviteKind, InviteState};

/// This server's federation: its pairs, the handler that answers the
/// ALPN, and what dials other servers.
pub struct Federation {
    auth: Auth,
    /// The libraries this server serves: what it may share (P8-03).
    catalog: Arc<dyn Catalog>,
    server_id: ServerId,
    name: ServerName,
    live: Live,
    /// The endpoint, once it is up.
    attached: OnceLock<Attached>,
    /// What the pairs share with this server, cached (P8-02).
    cache: Arc<PeerCache>,
    /// The connections the replica holds to the pairs.
    links: Arc<Links>,
    /// Each origin's sync lock ([`replica`]): the replica's and an
    /// admin's never run at once.
    syncing: KeyedLocks,
    /// How the pairs' asks of this server are paced (H2).
    answering: answer::Answering,
    /// `federation.sync_interval_secs`.
    sync_every: parking_lot::Mutex<Duration>,
    /// What a pair may send and ask (P8-04).
    limits: parking_lot::Mutex<Limits>,
    /// [`Limits::max_streams`], as the handler reads it for each
    /// connection.
    max_streams: Arc<AtomicUsize>,
    /// Wakes the replica before its next tick: a pair made, a setting
    /// changed.
    wake: Arc<tokio::sync::Notify>,
    /// The connections this server plays its pairs' items through
    /// (proxy mode, P8-04).
    plays: Arc<proxy::PlayClients>,
    /// The tokens granted to devices, by user, item and device, and when:
    /// a device's play that falls back to proxy mode is not counted twice
    /// (P8-04).
    device_grants: tokens::DeviceGrants,
}

/// Where and how this server caches what its pairs share (P8-02).
#[derive(Debug, Clone)]
pub struct ReplicaSettings {
    /// The directory the artwork files go in, one per origin.
    pub dir: PathBuf,
    /// The time between two syncs of a pair.
    pub sync_every: Duration,
    /// The most each origin's artwork files take, in bytes.
    pub cache_max_bytes: u64,
    /// The most every origin's artwork files take together, in bytes.
    pub cache_total_bytes: u64,
    /// What a pair may send and ask (P8-04).
    pub limits: Limits,
}

impl ReplicaSettings {
    /// The defaults, with the files under `dir`.
    pub fn in_dir(dir: PathBuf) -> Self {
        Self {
            dir,
            sync_every: DEFAULT_SYNC_EVERY,
            cache_max_bytes: cache::DEFAULT_MAX_BYTES,
            cache_total_bytes: cache::DEFAULT_TOTAL_BYTES,
            limits: Limits::default(),
        }
    }
}

/// What the endpoint gives, once it binds.
#[derive(Debug)]
struct Attached {
    endpoint: Endpoint,
    connections: Connections,
    told: Told,
}

/// Why redeeming another server's pairing link made no pair.
#[derive(Debug, thiserror::Error)]
pub enum PairError {
    #[error("that is not an invite link")]
    NotALink,
    /// A link with no code only finds a server.
    #[error("the link has no invite code")]
    NoCode,
    #[error("the link is an invite for an account, not to pair a server")]
    WrongKind,
    #[error("that link is this server's own, or names this server's id")]
    Itself,
    #[error("this server is starting; try again in a moment")]
    Starting,
    /// This server has a live pair with that server, by key or by id.
    #[error("the servers are paired already")]
    AlreadyPaired,
    /// The other server holds this server's key for its admin to confirm.
    #[error("the other server holds this server's new key until its admin confirms it")]
    KeyHeld,
    #[error(transparent)]
    Dial(#[from] DialError),
    /// The other server answered with what is not a server's.
    #[error("the other server answered with an id that is not one: {0}")]
    Unexpected(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl std::fmt::Debug for Federation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Federation")
            .field("server_id", &self.server_id)
            .field("live", &self.live)
            .finish_non_exhaustive()
    }
}

impl Federation {
    /// Federation for server `server_id`, called `name`, over `auth`'s
    /// store and limits, sharing from `catalog`, and caching what its pairs
    /// share as `replica` says.
    pub fn new(
        auth: Auth,
        catalog: Arc<dyn Catalog>,
        server_id: ServerId,
        name: ServerName,
        replica: ReplicaSettings,
    ) -> Self {
        let links = Arc::new(Links::default());
        let cache = Arc::new(PeerCache::new(
            auth.store().clone(),
            replica.dir,
            FileLimits {
                per_origin: replica.cache_max_bytes,
                total: replica.cache_total_bytes,
            },
            links.clone(),
        ));
        Self {
            auth,
            catalog,
            server_id,
            name,
            live: Live::default(),
            attached: OnceLock::new(),
            cache,
            links,
            syncing: KeyedLocks::default(),
            answering: answer::Answering::default(),
            sync_every: parking_lot::Mutex::new(replica.sync_every),
            limits: parking_lot::Mutex::new(replica.limits),
            max_streams: Arc::new(AtomicUsize::new(replica.limits.max_streams)),
            wake: Arc::default(),
            plays: Arc::default(),
            device_grants: parking_lot::Mutex::default(),
        }
    }

    /// The endpoint is up: what dials from it, holds its connections and
    /// tells where it is. The replica starts (P8-02).
    pub(crate) fn attach(
        self: &Arc<Self>,
        endpoint: Endpoint,
        connections: Connections,
        told: Told,
    ) {
        if self
            .attached
            .set(Attached {
                endpoint,
                connections,
                told,
            })
            .is_ok()
        {
            self.start_replica();
        }
    }

    /// What the pairs share with this server, cached: the backend of
    /// every `fed:` id ([`crate::catalog::LiveCatalog::set_peers`]).
    pub fn peer_catalog(&self) -> Arc<PeerCache> {
        self.cache.clone()
    }

    /// The handler for `spoolframe/federation/1`.
    pub(crate) fn protocol(self: &Arc<Self>) -> FederationProtocol<Self, Self, Self> {
        let name = self.name.clone();
        FederationProtocol::new(
            self.clone(),
            self.clone(),
            self.clone(),
            self.server_id.clone(),
            Arc::new(move || name.get()),
            self.live.clone(),
        )
        .with_max_streams(self.max_streams.clone())
    }

    fn store(&self) -> &crate::store::Store {
        self.auth.store()
    }

    /// What this server says of itself to another.
    fn hello(&self) -> PeerHello {
        PeerHello {
            protocol_version: PROTOCOL_VERSION,
            server_id: self.server_id.to_string(),
            name: self.name.get(),
        }
    }

    /// Pair with the server whose `federation` invite `link` is, as admin
    /// `by` (SB's side, P8-01): dial it, check it is not paired here
    /// already, redeem the code, and keep its row.
    pub async fn redeem_link(&self, link: &str, by: &str) -> Result<PeerRecord, PairError> {
        let Aim {
            attached,
            code,
            server,
            hex,
            addr,
            addrs,
            relays,
        } = self.aim(link).await?;
        let (dialed, info) = Dialed::connect(&attached.endpoint, addr, self.hello()).await?;
        let paired = async {
            let theirs = self.pairable(&info).await?;
            let mine = crate::invites::server_link(
                attached.endpoint.id().to_string(),
                self.name.get(),
                Some(attached.told.clone()),
            )
            .await;
            let paired = dialed
                .redeem(RedeemPairing {
                    code,
                    addrs: mine.addrs.iter().map(ToString::to_string).collect(),
                    relays: mine.relays,
                })
                .await?;
            // The other server has paired from here on: if this side keeps
            // nothing, it is told so, best effort, rather than left with a
            // pair this one does not know.
            let kept = if paired.server_id != theirs.as_str() {
                Err(PairError::Unexpected(paired.server_id))
            } else {
                self.store()
                    .add_peer(
                        NewPeer {
                            server_id: theirs.as_str(),
                            endpoint_id: &hex,
                            display_name: &server_name(info.name.clone()),
                            addrs: &addrs,
                            relays: &relays,
                        },
                        by,
                    )
                    .await
                    .map_err(|e| match e {
                        PeerError::Store(e) => PairError::Store(e),
                        _ => PairError::AlreadyPaired,
                    })
            };
            if let Err(e) = &kept {
                tracing::warn!(
                    peer = %server.fmt_short(),
                    error = %e,
                    "paired, but the pair cannot be kept here: telling the other server"
                );
                if let Err(e) = dialed.unpaired().await {
                    tracing::warn!(
                        peer = %server.fmt_short(),
                        error = %e,
                        "the other server could not be told: its admin unpairs it"
                    );
                }
            }
            kept
        }
        .await;
        dialed.close();
        let peer = paired?;
        tracing::info!(
            peer = %server.fmt_short(),
            server_id = %peer.server_id,
            name = %peer.display_name,
            by,
            "paired with another server"
        );
        // The replica connects to it at once (P8-02).
        self.wake.notify_one();
        Ok(peer)
    }
}

/// Where a `federation` invite link points (SB's side), checked as far
/// as it can be before dialing.
struct Aim<'a> {
    attached: &'a Attached,
    code: String,
    server: EndpointId,
    /// `server`, hex, as a pair keeps it.
    hex: String,
    addr: EndpointAddr,
    /// The addresses and relays kept of the link's.
    addrs: Vec<String>,
    relays: Vec<String>,
}

/// What looking a `federation` invite up before redeeming it found
/// (P8-01): the server that made it, as it says, and its key's
/// fingerprint, for the admin to compare with what its own admin reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairLookup {
    pub name: String,
    pub server_id: String,
    pub fingerprint: String,
    /// The admin who made it, when they have a display name.
    pub inviter: Option<String>,
    /// Unix seconds.
    pub expires_at: i64,
}

impl Federation {
    /// The invite `link` names, if it is one this server can redeem: one
    /// with a code, not an account's, not this server's own, and not of a
    /// server paired here already.
    async fn aim(&self, link: &str) -> Result<Aim<'_>, PairError> {
        let invite = parse_link(link).ok_or(PairError::NotALink)?;
        let code = invite.code.clone().ok_or(PairError::NoCode)?;
        if invite.kind == InviteLinkKind::PortableAccount {
            return Err(PairError::WrongKind);
        }
        let attached = self.attached.get().ok_or(PairError::Starting)?;
        let server: EndpointId = invite.server.parse().map_err(|_| PairError::NotALink)?;
        if server == attached.endpoint.id() {
            return Err(PairError::Itself);
        }
        if self
            .store()
            .live_peer_by_endpoint(&invite.server)
            .await?
            .is_some()
        {
            return Err(PairError::AlreadyPaired);
        }
        let dial_at = kept_addrs(invite.addrs.iter().copied());
        let relays = kept_relays(invite.relays.iter().cloned());
        Ok(Aim {
            attached,
            code,
            server,
            hex: invite.server,
            addr: endpoint_addr(server, dial_at.iter().copied(), &relays),
            addrs: dial_at.iter().map(ToString::to_string).collect(),
            relays,
        })
    }

    /// The server id of the server dialed, which said `info`, if this
    /// server may pair with it: a stranger to it, and not paired here.
    async fn pairable(&self, info: &PeerInfo) -> Result<ServerId, PairError> {
        let theirs = ServerId::parse(&info.server_id)
            .map_err(|_| PairError::Unexpected(info.server_id.clone()))?;
        if theirs == self.server_id {
            return Err(PairError::Itself);
        }
        match info.standing {
            Standing::Stranger => {}
            Standing::KeyHeld => return Err(PairError::KeyHeld),
            // It has this server's row and this server has none of its:
            // its admin unpairs first.
            _ => return Err(PairError::AlreadyPaired),
        }
        if self
            .store()
            .live_peer_by_server_id(theirs.as_str())
            .await?
            .is_some()
        {
            return Err(PairError::AlreadyPaired);
        }
        Ok(theirs)
    }

    /// Look the `federation` invite `link` up without redeeming it (SB's
    /// side, P8-01): dial the server that made it as pairing would, hear
    /// its name, and ask its client protocol whether the code still works,
    /// as an app looks an invite up, on the strangers' budget. A code that
    /// does not work is refused as redeeming it would be.
    pub async fn look_up_link(&self, link: &str) -> Result<PairLookup, PairError> {
        let Aim {
            attached,
            code,
            hex,
            addr,
            ..
        } = self.aim(link).await?;
        let (dialed, info) =
            Dialed::connect(&attached.endpoint, addr.clone(), self.hello()).await?;
        dialed.close();
        let theirs = self.pairable(&info).await?;
        let hello = Hello {
            protocol_version: spoolframe_protocol::PROTOCOL_VERSION,
            client_name: "spoolframe-server".into(),
            client_version: env!("CARGO_PKG_VERSION").into(),
            platform: Platform::Unknown,
            device_name: Some(self.name.get()),
            capabilities: None,
            experimental: vec![spoolframe_protocol::experimental::FEDERATION.into()],
            features: vec![features::INVITE_LOOKUP.into()],
        };
        let (client, _) = PlayClient::connect(&attached.endpoint, addr, hello).await?;
        let answered = client
            .call(None, Request::LookupInvite(LookupInvite { code }), None)
            .await;
        client.close();
        let refused = |reason| PairError::Dial(DialError::Refused(reason));
        match answered?.response {
            Response::Ok(Reply::InviteLookup { outcome }) => match outcome {
                InviteLookupOutcome::Valid {
                    kind: LinkedKind::Federation,
                    inviter,
                    expires_at,
                    ..
                } => Ok(PairLookup {
                    name: server_name(info.name),
                    server_id: theirs.to_string(),
                    // The key this server dials, not what the other says
                    // of itself: the admins compare it.
                    fingerprint: fingerprint::of_hex(&hex).unwrap_or_default(),
                    inviter,
                    expires_at,
                }),
                InviteLookupOutcome::Valid { .. } => Err(PairError::WrongKind),
                InviteLookupOutcome::Expired { .. } => Err(refused(PairingRefused::InviteExpired)),
                InviteLookupOutcome::Used => Err(refused(PairingRefused::InviteExhausted)),
                InviteLookupOutcome::Revoked
                | InviteLookupOutcome::NoMatch
                | InviteLookupOutcome::Unknown => Err(refused(PairingRefused::InviteInvalid)),
            },
            Response::Err(ProtocolError::RateLimited { retry_after_ms }) => {
                Err(refused(PairingRefused::RateLimited {
                    retry_after_secs: retry_after_ms.div_ceil(1000),
                }))
            }
            other => Err(PairError::Dial(DialError::Unreachable(format!(
                "the other server did not look the invite up: {other:?}"
            )))),
        }
    }

    /// Every pair, live or not, as the store keeps them.
    pub async fn peers(&self) -> Result<Vec<PeerRecord>, StoreError> {
        self.store().federation_peers().await
    }

    /// Pair `id`, live or not.
    pub async fn peer(&self, id: &str) -> Result<Option<PeerRecord>, StoreError> {
        self.store().federation_peer(id).await
    }

    /// The new keys kept for pair `id`, held and rejected.
    pub async fn held_keys(&self, id: &str) -> Result<Vec<HeldKey>, StoreError> {
        self.store().held_peer_keys(id).await
    }

    /// The live pair whose server id is `server_id`.
    pub async fn live_peer_by_server_id(
        &self,
        server_id: &str,
    ) -> Result<Option<PeerRecord>, StoreError> {
        self.store().live_peer_by_server_id(server_id).await
    }

    /// Whether `endpoint_id` (hex) is a live pair's key.
    pub async fn is_paired(&self, endpoint_id: &str) -> Result<bool, StoreError> {
        Ok(self
            .store()
            .live_peer_by_endpoint(endpoint_id)
            .await?
            .is_some())
    }

    /// Whether `endpoint_id` (hex) has a federation connection open now.
    pub fn connected(&self, endpoint_id: &str) -> bool {
        endpoint_id
            .parse::<EndpointId>()
            .is_ok_and(|endpoint| self.live.has(&endpoint))
    }

    /// Unpair `id` (P8-01): revoke this side's row and its grants
    /// (P8-03), drop the cache of what it shared but for what a user here
    /// holds (P8-02), close the other's connections, and tell it, best
    /// effort, so it revokes its own. The row as it was; `None` for no live pair.
    /// The relay's tier falls back once its cache of the answer expires.
    pub async fn unpair(&self, id: &str, by: &str) -> Result<Option<PeerRecord>, StoreError> {
        let Some(peer) = self.store().revoke_peer(id).await? else {
            return Ok(None);
        };
        tracing::info!(
            peer = %peer.endpoint_id,
            server_id = %peer.server_id,
            name = %peer.display_name,
            by,
            "unpaired from another server"
        );
        self.end_remote_sessions(&peer.id, None);
        self.close_plays(&peer.id);
        self.forget_origin(&peer.server_id).await;
        if let Ok(endpoint) = peer.endpoint_id.parse::<EndpointId>() {
            self.live.close(&endpoint, close::UNPAIRED, "unpaired");
            if let Some(attached) = self.attached.get() {
                let addr = endpoint_addr(endpoint, parsed(&peer.addrs), &peer.relays);
                tokio::spawn(tell_unpaired(attached.endpoint.clone(), addr, self.hello()));
            }
        }
        Ok(Some(peer))
    }

    /// Confirm `endpoint_id`, a key held for pair `id`: the pin moves to
    /// it, and the old key's connections are closed, with those of the
    /// pair's other held keys.
    pub async fn confirm_key(
        &self,
        id: &str,
        endpoint_id: &str,
        by: &str,
    ) -> Result<PeerRecord, PeerError> {
        let old = self
            .store()
            .federation_peer(id)
            .await?
            .ok_or_else(|| PeerError::NoSuchPeer(id.to_owned()))?;
        let others = self.store().held_peer_keys(id).await?;
        let peer = self.store().confirm_peer_key(id, endpoint_id).await?;
        tracing::info!(
            server_id = %peer.server_id,
            name = %peer.display_name,
            old = %old.endpoint_id,
            new = %peer.endpoint_id,
            by,
            "a paired server's new key confirmed"
        );
        let gone = std::iter::once(old.endpoint_id.as_str()).chain(
            others
                .iter()
                .map(|k| k.endpoint_id.as_str())
                .filter(|k| *k != endpoint_id),
        );
        for key in gone {
            if let Ok(endpoint) = key.parse::<EndpointId>() {
                self.live
                    .close(&endpoint, close::UNPAIRED, "the pair moved to a new key");
            }
        }
        Ok(peer)
    }

    /// Reject `endpoint_id`, a key held for pair `id`: its connections
    /// are closed, and it is refused, not held again, for a day.
    pub async fn reject_key(
        &self,
        id: &str,
        endpoint_id: &str,
        by: &str,
    ) -> Result<PeerRecord, PeerError> {
        let peer = self.store().reject_peer_key(id, endpoint_id).await?;
        tracing::info!(
            server_id = %peer.server_id,
            name = %peer.display_name,
            rejected = %endpoint_id,
            by,
            "a key that said it is a paired server was rejected"
        );
        if let Ok(endpoint) = endpoint_id.parse::<EndpointId>() {
            self.live.close(&endpoint, close::UNPAIRED, "key rejected");
        }
        Ok(peer)
    }
}

/// Tell the server at `addr` that this one, saying `hello`, unpaired:
/// once, and only logged however it goes.
async fn tell_unpaired(endpoint: Endpoint, addr: EndpointAddr, hello: PeerHello) {
    let peer = addr.id;
    let told = async {
        let (dialed, info) = Dialed::connect(&endpoint, addr, hello).await?;
        let told = if info.standing == Standing::Paired {
            dialed.unpaired().await
        } else {
            // It holds no pair with this server already.
            Ok(())
        };
        dialed.close();
        told
    }
    .await;
    match told {
        Ok(()) => tracing::info!(peer = %peer.fmt_short(), "the unpaired server was told"),
        Err(e) => tracing::warn!(
            peer = %peer.fmt_short(),
            error = %e,
            "the unpaired server could not be told: it keeps its side until its admin unpairs"
        ),
    }
}

/// Where server `id` may be dialled: its `addrs` and `relays` hints.
fn endpoint_addr(
    id: EndpointId,
    addrs: impl IntoIterator<Item = SocketAddr>,
    relays: &[String],
) -> EndpointAddr {
    EndpointAddr::from_parts(
        id,
        addrs.into_iter().map(TransportAddr::Ip).chain(
            relays
                .iter()
                .filter_map(|r| r.parse::<RelayUrl>().ok())
                .map(TransportAddr::Relay),
        ),
    )
}

/// Whether `addr` may be dialled as another server's: not loopback,
/// unspecified, multicast or broadcast, nor port 0. Private, LAN, CGNAT
/// and link-local addresses are kept: two home servers on one LAN or
/// tailnet pair over them.
fn dialable(addr: SocketAddr) -> bool {
    use std::net::IpAddr;
    let refused = match addr.ip().to_canonical() {
        IpAddr::V4(v4) => {
            v4.is_loopback() || v4.is_unspecified() || v4.is_multicast() || v4.is_broadcast()
        }
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified() || v6.is_multicast(),
    };
    !refused && addr.port() != 0
}

/// The `ip:port` hints among `addrs` that read as one.
fn parsed(addrs: &[String]) -> impl Iterator<Item = SocketAddr> + '_ {
    addrs.iter().filter_map(|a| a.parse().ok())
}

/// The `addrs` that may be dialled, at most [`MAX_HINTS`]: what another
/// server's addresses are kept as.
fn kept_addrs(addrs: impl IntoIterator<Item = SocketAddr>) -> Vec<SocketAddr> {
    addrs
        .into_iter()
        .filter(|a| dialable(*a))
        .take(MAX_HINTS)
        .collect()
}

/// The `relays` that are HTTPS relay URLs, at most [`MAX_HINTS`]: what
/// another server's relays are kept as.
fn kept_relays(relays: impl IntoIterator<Item = String>) -> Vec<String> {
    relays
        .into_iter()
        .filter(|r| r.starts_with("https://") && is_relay_url(r))
        .take(MAX_HINTS)
        .collect()
}

/// A redemption's hints, as [`kept_addrs`] and [`kept_relays`] keep
/// them, written for the store.
fn hints(redeem: &RedeemPairing) -> (Vec<String>, Vec<String>) {
    let addrs = kept_addrs(parsed(&redeem.addrs))
        .iter()
        .map(ToString::to_string)
        .collect();
    (addrs, kept_relays(redeem.relays.iter().cloned()))
}

/// A redemption's refusal, for the server that asked.
fn refusal(e: RedeemError) -> PairingRefused {
    match e {
        RedeemError::Expired => PairingRefused::InviteExpired,
        RedeemError::Exhausted => PairingRefused::InviteExhausted,
        RedeemError::WrongKind => PairingRefused::WrongInviteKind,
        RedeemError::RateLimited { retry_after } => rate_limited(retry_after),
        RedeemError::Store(e) => {
            tracing::error!(error = %e, "cannot check a pairing invite");
            PairingRefused::InviteInvalid
        }
        _ => PairingRefused::InviteInvalid,
    }
}

fn rate_limited(retry_after: std::time::Duration) -> PairingRefused {
    PairingRefused::RateLimited {
        retry_after_secs: retry_after.as_secs().max(1),
    }
}

#[async_trait::async_trait]
impl Peers for Federation {
    async fn known(&self, endpoint: EndpointId) -> Option<ServerId> {
        match self
            .store()
            .live_peer_by_endpoint(&endpoint.to_string())
            .await
        {
            Ok(peer) => peer.and_then(|p| ServerId::parse(&p.server_id).ok()),
            Err(e) => {
                tracing::warn!(error = %e, "cannot look a server's pair up");
                None
            }
        }
    }

    async fn hello(
        &self,
        endpoint: EndpointId,
        known: Option<&ServerId>,
        server_id: &ServerId,
    ) -> Result<Standing, PairingRefused> {
        if let Some(pinned) = known {
            if pinned == server_id {
                return Ok(Standing::Paired);
            }
            // Kept for the Federation page, which says so with Unpair.
            if let Err(e) = self
                .store()
                .peer_identity_refused(&endpoint.to_string())
                .await
            {
                tracing::warn!(error = %e, "cannot keep that a paired server's id changed");
            }
            return Err(PairingRefused::ServerIdChanged);
        }
        let store = self.store();
        let hex = endpoint.to_string();
        // A store error makes it a stranger, which fails safe: a stranger
        // reaches only a redemption, which asks the store again.
        let standing = async {
            if let Some(paired) = store.live_peer_by_server_id(server_id.as_str()).await? {
                // Each key has its own count of guesses: this budget is
                // what keys made by the thousand share.
                if let Err(wait) = self.auth.stranger_attempt() {
                    return Ok(Err(rate_limited(wait)));
                }
                return Ok(match store.hold_peer_key(&paired.id, &hex).await? {
                    KeyHold::New => {
                        tracing::warn!(
                            peer = %endpoint.fmt_short(),
                            server_id = %server_id,
                            name = %paired.display_name,
                            "a paired server presents a new key: held until an admin confirms it"
                        );
                        Ok(Standing::KeyHeld)
                    }
                    KeyHold::Held => Ok(Standing::KeyHeld),
                    KeyHold::Rejected => Err(PairingRefused::KeyRefused),
                    KeyHold::Full => {
                        tracing::warn!(
                            peer = %endpoint.fmt_short(),
                            server_id = %server_id,
                            name = %paired.display_name,
                            "a new key for a paired server is dropped: as many as are held \
                             wait for an admin already"
                        );
                        Err(PairingRefused::KeyRefused)
                    }
                    // Unpaired since it was looked up.
                    KeyHold::NoPeer => Ok(Standing::Stranger),
                });
            }
            Ok::<_, StoreError>(Ok(Standing::Stranger))
        }
        .await;
        standing.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "cannot look a server's pair up");
            Ok(Standing::Stranger)
        })
    }

    async fn redeem(
        &self,
        endpoint: EndpointId,
        server_id: &ServerId,
        hello: &PeerHello,
        redeem: &RedeemPairing,
    ) -> Result<(), PairingRefused> {
        // On the strangers' budget first, as an account invite is (PX-10).
        self.auth.stranger_attempt().map_err(rate_limited)?;
        let hex = endpoint.to_string();
        let hash =
            crate::invites::usable_code(&self.auth, &hex, &redeem.code, InviteKind::Federation)
                .await
                .map_err(refusal)?;
        let (addrs, relays) = hints(redeem);
        let paired = self
            .store()
            .pair_by_invite(
                &hash,
                NewPeer {
                    server_id: server_id.as_str(),
                    endpoint_id: &hex,
                    display_name: &hello.name,
                    addrs: &addrs,
                    relays: &relays,
                },
            )
            .await
            .map_err(|e| refusal(RedeemError::Store(e)))?;
        match paired {
            PairRedeemed::Paired(peer) => {
                tracing::info!(
                    peer = %endpoint.fmt_short(),
                    server_id = %peer.server_id,
                    name = %peer.display_name,
                    invite = ?peer.invite_id,
                    "a server redeemed a pairing invite"
                );
                Ok(())
            }
            PairRedeemed::AlreadyPaired => Err(PairingRefused::AlreadyPaired),
            // Revoked, expired or used up since it was looked at.
            PairRedeemed::Refused(InviteState::Expired) => Err(PairingRefused::InviteExpired),
            PairRedeemed::Refused(InviteState::Exhausted) => Err(PairingRefused::InviteExhausted),
            PairRedeemed::Refused(_) => Err(PairingRefused::InviteInvalid),
        }
    }

    async fn unpaired(&self, endpoint: EndpointId) {
        match self
            .store()
            .revoke_peer_by_endpoint(&endpoint.to_string())
            .await
        {
            Ok(Some(peer)) => {
                tracing::info!(
                    peer = %endpoint.fmt_short(),
                    server_id = %peer.server_id,
                    name = %peer.display_name,
                    "a paired server unpaired: its pair is revoked here too"
                );
                self.end_remote_sessions(&peer.id, None);
                self.close_plays(&peer.id);
                self.forget_origin(&peer.server_id).await;
            }
            Ok(None) => {}
            Err(e) => tracing::error!(error = %e, "cannot revoke a pair the other server ended"),
        }
    }
}

/// A connection's place among the held ones, given back when it closes.
#[derive(Debug)]
pub struct Held {
    connections: Connections,
    id: usize,
}

impl Drop for Held {
    fn drop(&mut self) {
        self.connections.remove(self.id);
    }
}

impl Strangers for Federation {
    type Held = Held;

    fn admit(
        &self,
        connection: &iroh::endpoint::Connection,
        peer: Option<&ServerId>,
    ) -> Option<Held> {
        let attached = self.attached.get()?;
        attached
            .connections
            .admit_federation(connection, peer.cloned())?;
        Some(Held {
            connections: attached.connections.clone(),
            id: connection.stable_id(),
        })
    }

    fn strayed(&self, endpoint: EndpointId) {
        // Counted only within the strangers' budget: the limiter holds at
        // most so many keys and makes room by forgetting the oldest, so
        // keys made by the thousand must not fill it faster than sign-in
        // attempts can. Past the budget a stray is not counted, and its
        // endpoint reaches no redemption, which spends the same budget.
        if self.auth.stranger_attempt().is_err() {
            return;
        }
        let source = endpoint.to_string();
        // Never taken back: a wrong code stays counted the same way.
        let _ = self.auth.guess(format!("invite@{source}"), Some(source));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_hints_that_read_are_kept_and_no_more_than_the_limit() {
        let redeem = RedeemPairing {
            code: String::new(),
            addrs: ["nowhere", "203.0.113.7:7420"]
                .into_iter()
                .map(str::to_owned)
                .chain((0..MAX_HINTS + 3).map(|i| format!("10.0.0.{}:7420", i % 250)))
                .collect(),
            relays: vec![
                "ftp://relay.example.org".into(),
                "https://relay.example.org".into(),
            ],
        };
        let (addrs, relays) = hints(&redeem);
        assert_eq!(addrs.len(), MAX_HINTS);
        assert_eq!(addrs[0], "203.0.113.7:7420");
        assert_eq!(relays, ["https://relay.example.org"]);
    }

    /// Where no other server is, nothing is dialled: loopback,
    /// unspecified, multicast, broadcast, and relays not over HTTPS. A
    /// LAN's and a tailnet's addresses are kept.
    #[test]
    fn hints_that_lead_nowhere_else_are_dropped_and_a_lan_s_kept() {
        let refused = [
            "127.0.0.1:7420",
            "[::1]:7420",
            "0.0.0.0:7420",
            "[::]:7420",
            "224.0.0.251:7420",
            "[ff02::1]:7420",
            "255.255.255.255:7420",
            "[::ffff:127.0.0.1]:7420",
            "203.0.113.7:0",
        ];
        let kept = [
            "10.1.2.3:7420",
            "172.16.0.9:7420",
            "192.168.1.20:7420",
            "100.64.0.7:7420",
            "[fd00::7]:7420",
            "[fe80::1]:7420",
            "203.0.113.7:7420",
        ];
        let addrs = kept_addrs(refused.iter().chain(&kept).map(|a| a.parse().unwrap()));
        let relays = kept_relays(
            [
                "http://relay.example.org",
                "https://relay.example.org",
                "https:///x",
            ]
            .map(str::to_owned),
        );
        assert_eq!(
            addrs,
            kept.map(|a| a.parse::<SocketAddr>().unwrap()).to_vec()
        );
        assert_eq!(relays, ["https://relay.example.org"]);
    }

    #[test]
    fn a_wrong_code_and_a_store_error_are_told_alike() {
        assert_eq!(refusal(RedeemError::Invalid), PairingRefused::InviteInvalid);
        assert_eq!(
            refusal(RedeemError::WrongKind),
            PairingRefused::WrongInviteKind
        );
        assert_eq!(
            refusal(RedeemError::RateLimited {
                retry_after: std::time::Duration::from_millis(10)
            }),
            PairingRefused::RateLimited {
                retry_after_secs: 1
            }
        );
    }
}
