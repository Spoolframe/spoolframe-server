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
use tokio::sync::watch;

use crate::admission::{self, Admission, Peer, PeerKind};
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

/// The ALPNs the endpoint takes: the client protocol, federation (served
/// while `experimental.federation` is on, else refused with a reason), and
/// its relays' question when it has relays of its own (P6-04).
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
    /// session. Also a server on `spoolframe/federation/1` that is not a
    /// live pair's (P8-01).
    Anonymous { endpoint: EndpointId },
    /// A paired server on `spoolframe/federation/1` (P8-01): its endpoint
    /// and the server id it is pinned with.
    FederationPeer {
        endpoint: EndpointId,
        peer: spoolframe_protocol::ServerId,
    },
}

/// The open client connections and their auth state, with who may open
/// more (PX-10).
#[derive(Debug, Clone, Default)]
pub struct Connections {
    peers: Arc<Mutex<HashMap<usize, Arc<Peer>>>>,
    admission: Admission,
    /// The requests in hand on connections let in by a paired server's
    /// sessions alone: by pair, and in all.
    remote_requests: Arc<Mutex<RemoteRequests>>,
}

/// How many requests of paired servers' sessions are in hand: every
/// pair's together (watched, so a test waits for one to end), and each
/// pair's by its row.
#[derive(Debug)]
struct RemoteRequests {
    all: watch::Sender<usize>,
    by_pair: HashMap<String, usize>,
}

impl Default for RemoteRequests {
    fn default() -> Self {
        Self {
            all: watch::Sender::new(0),
            by_pair: HashMap::new(),
        }
    }
}

/// One request's place among its pairs' [`RemoteRequests`], given back
/// when dropped.
#[derive(Debug)]
pub(crate) struct RemotePlace {
    requests: Arc<Mutex<RemoteRequests>>,
    pairs: Vec<String>,
}

impl Drop for RemotePlace {
    fn drop(&mut self) {
        let mut requests = self.requests.lock();
        requests.all.send_modify(|n| *n = n.saturating_sub(1));
        for pair in &self.pairs {
            if let Some(mine) = requests.by_pair.get_mut(pair) {
                *mine = mine.saturating_sub(1);
                if *mine == 0 {
                    requests.by_pair.remove(pair);
                }
            }
        }
    }
}

/// The devices' connections among `peers`: what signing in and out is
/// about. A server's federation connection has no session.
fn devices(peers: &HashMap<usize, Arc<Peer>>) -> impl Iterator<Item = (&usize, &Arc<Peer>)> {
    peers
        .iter()
        .filter(|(_, peer)| *peer.kind() == PeerKind::Device)
}

impl Connections {
    pub(crate) fn new(admission: Admission) -> Self {
        Self {
            peers: Arc::default(),
            admission,
            remote_requests: Arc::default(),
        }
    }

    /// A place for a request of pairs `pairs`' sessions, if there is one:
    /// each pair's together hold [`Admission::max_pair_remote_connections`]
    /// connections' worth of [`Admission::remote_requests_each`], every
    /// pair's [`Admission::max_all_remote_connections`] connections' worth.
    /// A connection held under several pairs' sessions takes a place of
    /// each of them (D8-02), so it never spends one pair's share as
    /// another's. Held while it is kept.
    pub(crate) fn take_remote_request(&self, pairs: &[String]) -> Option<RemotePlace> {
        let each = self.admission.remote_requests_each;
        let mut requests = self.remote_requests.lock();
        let full = |pair: &String| {
            requests.by_pair.get(pair).copied().unwrap_or(0)
                >= self.admission.max_pair_remote_connections * each
        };
        if *requests.all.borrow() >= self.admission.max_all_remote_connections * each
            || pairs.iter().any(full)
        {
            return None;
        }
        requests.all.send_modify(|n| *n += 1);
        for pair in pairs {
            *requests.by_pair.entry(pair.clone()).or_insert(0) += 1;
        }
        Some(RemotePlace {
            requests: self.remote_requests.clone(),
            pairs: pairs.to_vec(),
        })
    }

    /// How many requests of paired servers' sessions are in hand, as it
    /// changes: for tests, which wait for a stream to end.
    #[cfg(any(test, feature = "test-support"))]
    pub fn remote_requests_in_hand(&self) -> watch::Receiver<usize> {
        self.remote_requests.lock().all.subscribe()
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

    /// Every open connection: the client protocol's, and federation's.
    pub fn snapshot(&self) -> Vec<ConnectionAuth> {
        self.lock()
            .values()
            .map(|peer| match peer.kind() {
                PeerKind::Federation(Some(server)) => ConnectionAuth::FederationPeer {
                    endpoint: peer.endpoint,
                    peer: server.clone(),
                },
                PeerKind::Federation(None) | PeerKind::Device => ConnectionAuth::Anonymous {
                    endpoint: peer.endpoint,
                },
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
        self.hold(Peer::new(connection), connection, known, local)
    }

    /// Hold the connection of a device known here by pairs `pairs`'
    /// sessions alone (C8-04), with the normal limits, while its endpoint
    /// holds fewer than [`Admission::max_remote_connections`] such, each
    /// of its pairs' endpoints fewer than
    /// [`Admission::max_pair_remote_connections`] and every endpoint's
    /// fewer than [`Admission::max_all_remote_connections`]. A device in
    /// several pairs' sessions counts in each pair's share (D8-02). "Known"
    /// is not a local user here: a paired server holding one session must
    /// not hold connections without end.
    pub(crate) fn admit_remote(
        &self,
        connection: &Connection,
        pairs: Vec<String>,
    ) -> Option<Arc<Peer>> {
        self.hold(Peer::remote(connection, pairs), connection, true, false)
    }

    /// Hold a server's `spoolframe/federation/1` connection (P8-01) as
    /// [`Connections::admit`] holds a device's: a live pair's (`peer`)
    /// with the normal limits, while its key holds fewer than
    /// [`Admission::max_pair_connections`] and every pair's fewer than
    /// [`Admission::max_paired_connections`] (F8-07); a stranger's on the
    /// same cap as devices that are not signed in.
    pub(crate) fn admit_federation(
        &self,
        connection: &Connection,
        peer: Option<spoolframe_protocol::ServerId>,
    ) -> Option<Arc<Peer>> {
        let known = peer.is_some();
        self.hold(Peer::federation(connection, peer), connection, known, false)
    }

    fn hold(
        &self,
        peer: Peer,
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
        // Counted under the same lock as the insert, so connections at
        // once never pass a cap.
        if matches!(peer.kind(), PeerKind::Federation(Some(_))) {
            let (all, this_key) = peers
                .values()
                .filter(|held| matches!(held.kind(), PeerKind::Federation(Some(_))))
                .fold((0, 0), |(all, mine), held| {
                    (all + 1, mine + usize::from(held.endpoint == peer.endpoint))
                });
            if all >= self.admission.max_paired_connections
                || this_key >= self.admission.max_pair_connections
            {
                return None;
            }
        }
        let pairs = peer.remote_pairs();
        if !pairs.is_empty() && !self.remote_room(&peers, &peer.endpoint, &pairs) {
            return None;
        }
        let peer = Arc::new(peer);
        if known {
            peer.raise();
        }
        if local {
            peer.note_local();
        }
        peers.insert(connection.stable_id(), peer.clone());
        Some(peer)
    }

    /// Whether `peers` leave room for one more connection of `endpoint`'s
    /// held by pairs `pairs`' sessions alone: under the endpoint's cap,
    /// each of those pairs' shares, and every pair's together.
    fn remote_room(
        &self,
        peers: &HashMap<usize, Arc<Peer>>,
        endpoint: &EndpointId,
        pairs: &[String],
    ) -> bool {
        let held: Vec<(EndpointId, Vec<String>)> = peers
            .values()
            .map(|held| (held.endpoint, held.remote_pairs()))
            .filter(|(_, of)| !of.is_empty())
            .collect();
        let this_key = held.iter().filter(|(e, _)| e == endpoint).count();
        held.len() < self.admission.max_all_remote_connections
            && self.pairs_have_room(peers, pairs)
            && this_key < self.admission.max_remote_connections
    }

    /// Whether each of `pairs` holds fewer than
    /// [`Admission::max_pair_remote_connections`] of `peers`.
    fn pairs_have_room(&self, peers: &HashMap<usize, Arc<Peer>>, pairs: &[String]) -> bool {
        pairs.iter().all(|pair| {
            peers
                .values()
                .filter(|held| held.remote_pairs().contains(pair))
                .count()
                < self.admission.max_pair_remote_connections
        })
    }

    /// `endpoint` redeemed a paired server's token (Z8-01): its sessions
    /// here are pairs `pairs`' alone. Each of its connections that holds
    /// no session of this server's own counts under every one of those
    /// pairs from now on, as one let in by them would: one held so
    /// already takes the pairs as they are now, if each it joins has room
    /// for it (W4); a stranger's gets the normal limits only while the
    /// remote caps have room for it. Either is closed `BUSY` once its
    /// streams are answered otherwise ([`Peer::refuse`]), as one more past
    /// the caps is. A device signed
    /// in here is left as it is. The ids raised.
    pub(crate) fn remote_signed_in(&self, endpoint: &EndpointId, pairs: &[String]) -> Vec<usize> {
        let mut raised = Vec::new();
        if pairs.is_empty() {
            return raised;
        }
        let peers = self.lock();
        let mine: Vec<(usize, Arc<Peer>)> = devices(&peers)
            .filter(|(_, p)| &p.endpoint == endpoint)
            .map(|(id, p)| (*id, p.clone()))
            .collect();
        for (id, peer) in mine {
            let held_by = peer.remote_pairs();
            if !held_by.is_empty() {
                // Already counted, so the caps it fills are not asked
                // again; only the pairs it joins now, whose shares it
                // would spend without a place (W4).
                let joined: Vec<String> = pairs
                    .iter()
                    .filter(|pair| !held_by.contains(pair))
                    .cloned()
                    .collect();
                if self.pairs_have_room(&peers, &joined) {
                    peer.set_remote_pairs(pairs.to_vec());
                } else {
                    peer.refuse(self.admission.sign_out_drain);
                }
            } else if !peer.known() {
                if self.remote_room(&peers, endpoint, pairs) {
                    peer.set_remote_pairs(pairs.to_vec());
                    peer.raise();
                    raised.push(id);
                } else {
                    peer.refuse(self.admission.sign_out_drain);
                }
            }
        }
        raised
    }

    pub(crate) fn remove(&self, id: usize) {
        self.lock().remove(&id);
    }

    /// Whether `endpoint` has a client connection open.
    pub(crate) fn has_endpoint(&self, endpoint: &EndpointId) -> bool {
        devices(&self.lock()).any(|(_, peer)| &peer.endpoint == endpoint)
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
        for (id, peer) in devices(&self.lock()).filter(|(_, p)| &p.endpoint == endpoint) {
            peer.raise();
            peer.note_own_session();
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

    /// The endpoints of the client connections that have the normal
    /// limits. A paired server's has them too, with no session behind it.
    fn known_endpoints(&self) -> Vec<EndpointId> {
        let mut endpoints: Vec<_> = devices(&self.lock())
            .filter(|(_, peer)| peer.known())
            .map(|(_, peer)| peer.endpoint)
            .collect();
        endpoints.sort();
        endpoints.dedup();
        endpoints
    }

    /// `endpoint` has no live session left: close its signed-in
    /// connections. Only those: the check that found no session was made
    /// a moment ago, and a connection the device opened since, not signed
    /// in yet (as an app does once signed out, to sign in again), is not
    /// one it ended.
    fn sign_out(&self, endpoint: &EndpointId) {
        for (_, peer) in devices(&self.lock()).filter(|(_, p)| &p.endpoint == endpoint && p.known())
        {
            peer.sign_out(self.admission.sign_out_drain);
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
            end_unplayed_remote_sessions(&auth, &playbacks);
            end_signed_out_playbacks(&auth, &playbacks).await;
            for endpoint in self.known_endpoints() {
                match auth.endpoint_has_any_session(&endpoint.to_string()).await {
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

/// End the paired servers' play token sessions past their most with no
/// playback open (F8-06): a client's cheap requests keep none for ever.
fn end_unplayed_remote_sessions(auth: &Auth, playbacks: &Playbacks) {
    let playing: std::collections::HashSet<String> = playbacks
        .sessions()
        .into_iter()
        .map(|session| crate::auth::SessionToken(session.0).hash())
        .collect();
    let ended = auth
        .remote_sessions()
        .end_unplayed(|hash| playing.contains(hash));
    auth.remote_sessions_ended(ended, "past their most with no playback");
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
/// serves (B-10), and the paired servers' sessions of items no longer in
/// the library shared with them (F8-04).
async fn end_unserved_playbacks_on_change(
    mut changes: tokio::sync::watch::Receiver<u64>,
    catalog: Arc<dyn Catalog>,
    playbacks: Arc<Playbacks>,
    federation: Option<Arc<crate::federation::Federation>>,
) {
    while changes.changed().await.is_ok() {
        end_unserved_playbacks(catalog.as_ref(), &playbacks).await;
        if let Some(federation) = &federation {
            federation.end_moved_sessions().await;
        }
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
    /// The optimized copies (PX-37), which a playback plays in place of
    /// an encode whenever one fits; `None` plays none. The server always
    /// gives it.
    pub optimized: Option<Arc<crate::optimized::Pool>>,
    /// `optimized.binge_ahead`: an episode's playback has the pool make
    /// the next one ahead.
    pub binge_ahead: bool,
    /// Downloads (PX-38), with the pool they use.
    pub downloads: Arc<crate::downloads::Downloads>,
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
    /// Federation (P8-01), while `experimental.federation` is on; `None`
    /// closes `spoolframe/federation/1` at once.
    pub federation: Option<Arc<crate::federation::Federation>>,
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
            // An account is made only by `RedeemInvite`: there is no open
            // sign-up, and no setting for one.
            invite_only: true,
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
                services.federation.clone(),
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
                services.federation.clone(),
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
        let federation = services.federation.clone();
        if let Some(federation) = &federation {
            federation.attach(endpoint.clone(), connections.clone(), told.clone());
        }
        let client = ClientProtocol::new(
            endpoint.clone(),
            told.clone(),
            connections.clone(),
            info,
            name.clone(),
            services,
            http_playback.clone(),
        );
        let mut router = Router::builder(endpoint).accept(CLIENT_ALPN, client);
        // Experimental (P8-01): off, the ALPN is refused with a reason, as
        // it was before federation.
        router = match federation {
            Some(federation) => router.accept(FEDERATION_ALPN, federation.protocol()),
            None => router.accept(FEDERATION_ALPN, Unsupported("spoolframe/federation/1")),
        };
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

    /// The endpoint: what this server dials others from, as one of their
    /// devices when it plays their items through itself (P8-04).
    pub fn endpoint(&self) -> &iroh::Endpoint {
        self.router.endpoint()
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
            ip("203.0.114.7:7420"),
        ];
        assert_eq!(
            published().apply(&addrs).into_owned(),
            [iroh::TransportAddr::Relay(relay), ip("203.0.114.7:7420")]
        );
    }

    /// D8-02: a request of a connection held under two pairs' sessions
    /// takes a place of each pair's, so it spends neither's share as the
    /// other's; each place comes back when it goes.
    #[test]
    fn a_request_under_two_pairs_takes_a_place_of_each() {
        let connections = Connections::new(Admission {
            max_pair_remote_connections: 1,
            remote_requests_each: 1,
            ..Admission::default()
        });
        let pairs = |p: &[&str]| p.iter().map(|p| (*p).to_owned()).collect::<Vec<_>>();
        let both = connections
            .take_remote_request(&pairs(&["a", "b"]))
            .expect("room for one");
        assert!(connections.take_remote_request(&pairs(&["a"])).is_none());
        assert!(connections.take_remote_request(&pairs(&["b"])).is_none());
        let other = connections.take_remote_request(&pairs(&["c"]));
        assert!(other.is_some());
        drop(both);
        assert!(connections.take_remote_request(&pairs(&["b"])).is_some());
    }

    /// A device of Bob's with a play token's session open on a server
    /// sharing its movies, from `catalog`: the federation, and the
    /// device's endpoint.
    async fn bob_playing(
        test: &str,
        catalog: Arc<crate::test_support::FakeCatalog>,
    ) -> (
        crate::store::tests::TestStore,
        Arc<crate::federation::Federation>,
        crate::auth::SessionToken,
        String,
    ) {
        use crate::federation::test_pairs::{minted, sharing_from};
        let (store, federation, bob_s) = sharing_from(test, catalog).await;
        let device = iroh::SecretKey::from_bytes(&[9; 32]).public().to_string();
        let token = minted(&federation, &bob_s, device.clone()).await;
        let (session, _) = federation.redeem_play_token(&token, &device).await.unwrap();
        (store, Arc::new(federation), session, device)
    }

    /// A catalog replaced runs the check of the paired servers' sessions
    /// (F8-04): the session of an item moved out of its library ends.
    #[tokio::test]
    async fn a_catalog_change_ends_a_moved_item_s_sessions() {
        use crate::test_support::{FakeCatalog, movie, music_library};
        let catalog = Arc::new(FakeCatalog::new("/nowhere/movie.mkv".into()));
        let (store, federation, session, _) = bob_playing("net-moved", catalog.clone()).await;
        let (changed, changes) = tokio::sync::watch::channel(0);
        let task = tokio::spawn(end_unserved_playbacks_on_change(
            changes,
            catalog.clone(),
            Arc::default(),
            Some(federation.clone()),
        ));
        catalog.move_to(movie().id(), music_library().id);
        let mut ended = store.sessions_ended();
        changed.send(1).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), ended.changed())
            .await
            .expect("the sessions' check never ran")
            .unwrap();
        assert!(
            federation
                .auth()
                .remote_sessions()
                .owner(&session.0)
                .is_none()
        );
        task.abort();
    }

    /// The sweep of ended sessions runs on its own and ends a play
    /// token's session kept past its most with no playback (F8-06).
    #[tokio::test]
    async fn the_sweep_ends_a_play_session_with_no_playback_past_its_most() {
        let catalog = Arc::new(crate::test_support::FakeCatalog::new(
            "/nowhere/movie.mkv".into(),
        ));
        let (store, federation, session, device) = bob_playing("net-unplayed", catalog).await;
        let auth = federation.auth().clone();
        let step = crate::remote_sessions::IDLE - std::time::Duration::from_secs(1);
        let mut used = std::time::Duration::ZERO;
        while used < crate::remote_sessions::PLAY_MAX_AGE {
            store.clock().advance(step);
            used += step;
            assert!(auth.remote_sessions().who(&session.0, &device).is_some());
        }
        let sweep = tokio::spawn(
            Connections::new(Admission::default()).close_signed_out(auth.clone(), Arc::default()),
        );
        // Until the sweep has subscribed, a nudge may go unseen: nudged
        // again until the session is gone.
        let ended = async {
            while auth.remote_sessions().owner(&session.0).is_some() {
                store.note_sessions_ended();
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), ended)
            .await
            .expect("the sweep never ended the session");
        sweep.abort();
    }
}
