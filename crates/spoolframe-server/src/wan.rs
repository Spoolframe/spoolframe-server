//! The server's public addresses (`network.wan`, P6-02): the ones a device
//! off the LAN dials, which the host does not see on its interfaces.
//!
//! Absent, nothing is advertised beyond what `network.announce` tells, and
//! the server is reached on the LAN (or, with P6-03, through relays). Each
//! part present adds to one set, each address with where it came from:
//!
//! - `static`: what the operator wrote, advertised as written;
//! - `mapped_port`: one address built from the port forwarded on the
//!   router, by the operator (`manual`) or by asking it (`auto`), with the
//!   public IP written down or looked up.
//!
//! The host's own IPv6 addresses are no part of it: iroh tells devices
//! every global IPv6 address on the host's interfaces itself, and
//! `network.ipv6 = false` is what keeps the server off IPv6.
//!
//! The set joins the told list ([`crate::announce::Told`]): signed-in
//! devices are pushed it in `ServerAddresses` and save it (PX-12), and the
//! status page lists it with each address's source.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::num::NonZeroU16;
use std::sync::Arc;

use parking_lot::Mutex;

/// `network.wan`, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wan {
    /// `static`: public addresses advertised as written.
    pub statics: Vec<SocketAddr>,
    /// `mapped_port`; `None` when set to `false`.
    pub mapped_port: Option<MappedPort>,
}

/// `network.wan.mapped_port`, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappedPort {
    /// The public port forwarded in `manual` mode; `network.port` when
    /// left out. In `auto` mode the router maps the endpoint's own port,
    /// and the port it reports is the one advertised.
    pub port: Option<u16>,
    pub mode: Mode,
    pub public_ip: PublicIp,
    /// Where `public_ip = "lookup"` asks; `None` forbids asking.
    pub lookup_via: Option<LookupVia>,
}

/// How the port gets forwarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The server asks the router (UPnP, NAT-PMP or PCP).
    Auto,
    /// The operator forwarded it on the router.
    Manual,
}

/// Where the mapped port's public IP comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicIp {
    /// Asked of [`MappedPort::lookup_via`].
    Lookup,
    /// Written down.
    Fixed(IpAddr),
}

/// A service that answers with the IP address a request came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookupVia {
    /// `https://ipv4.wtfismyip.com/text`: the address, as plain text.
    Wtfismyip,
}

impl LookupVia {
    /// The name `lookup_via` takes.
    pub fn name(self) -> &'static str {
        match self {
            Self::Wtfismyip => "wtfismyip",
        }
    }
}

/// Where an advertised address came from, as the status page says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Source {
    /// `network.wan.static`.
    Static,
    /// The mapped port with its public IP: the router's answer, or the
    /// address written in `public_ip`.
    MappedPort,
    /// The mapped port with the public IP looked up.
    Lookup,
}

/// An advertised address and its source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicAddr {
    pub addr: SocketAddr,
    pub source: Source,
}

/// Why `ip` cannot be reached from the Internet, if it cannot, the range
/// named: the admin API's rule, so the web UI says what the config check
/// says.
pub use spoolframe_admin_api::wan::not_public;

/// A `static` entry: a public address with its port, `"203.0.113.7:7420"`
/// or `"[2001:db8::1]:7420"`.
pub fn parse_static(text: &str) -> Result<SocketAddr, String> {
    spoolframe_admin_api::wan::check_static(text)
}

/// A part of the set found at run time, for its status note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Part {
    Mapping,
    Lookup,
}

#[derive(Debug, Default)]
struct State {
    statics: Vec<SocketAddr>,
    mapped: Option<PublicAddr>,
    notes: BTreeMap<Part, String>,
    /// The router's answer, with the port asked for; `None` when no
    /// router is asked.
    mapping: Option<(u16, Mapping)>,
    lookup: LookupState,
}

/// Where the mapped port's public IP lookup is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum LookupState {
    /// Nothing is looked up.
    #[default]
    NotUsed,
    /// The router reported a public address: no need to ask.
    NotNeeded,
    Asking,
    /// The answer, from where, and when.
    Found {
        ip: Ipv4Addr,
        via: LookupVia,
        at: std::time::SystemTime,
    },
    /// It failed; `retrying` while tries are left.
    Failed {
        error: String,
        retrying: bool,
    },
}

#[derive(Debug)]
struct Inner {
    configured: bool,
    state: Mutex<State>,
    changed: tokio::sync::watch::Sender<()>,
}

/// The public addresses found so far, and what went wrong finding them:
/// one per server, shared by what finds them and what tells them.
#[derive(Debug, Clone)]
pub struct Public(Arc<Inner>);

impl Default for Public {
    /// `network.wan` absent: nothing, ever.
    fn default() -> Self {
        Self::new(None)
    }
}

impl Public {
    /// What `wan` advertises before anything is found at run time: its
    /// static addresses.
    pub fn new(wan: Option<&Wan>) -> Self {
        Self(Arc::new(Inner {
            configured: wan.is_some(),
            state: Mutex::new(State {
                statics: wan.map(|w| w.statics.clone()).unwrap_or_default(),
                ..State::default()
            }),
            changed: tokio::sync::watch::Sender::new(()),
        }))
    }

    /// Whether `network.wan` is set at all.
    pub fn configured(&self) -> bool {
        self.0.configured
    }

    /// Every address advertised now, each once: the static ones, then
    /// the mapped port's.
    pub fn addrs(&self) -> Vec<PublicAddr> {
        let state = self.0.state.lock();
        let mut out: Vec<PublicAddr> = Vec::new();
        let statics = state.statics.iter().map(|&addr| PublicAddr {
            addr,
            source: Source::Static,
        });
        for entry in statics.chain(state.mapped) {
            if !out.iter().any(|e| e.addr == entry.addr) {
                out.push(entry);
            }
        }
        out
    }

    /// What went wrong finding them, one line per part that failed.
    pub fn notes(&self) -> Vec<String> {
        self.0.state.lock().notes.values().cloned().collect()
    }

    /// A message each time [`Self::addrs`] changes.
    pub(crate) fn changes(&self) -> tokio::sync::watch::Receiver<()> {
        self.0.changed.subscribe()
    }

    /// Change the state with `f`; tell the watchers if the addresses
    /// changed.
    fn update(&self, f: impl FnOnce(&mut State)) {
        let before = self.addrs();
        f(&mut self.0.state.lock());
        if self.addrs() != before {
            self.0.changed.send_replace(());
        }
    }

    /// The mapped port's address, or none.
    pub(crate) fn set_mapped(&self, mapped: Option<PublicAddr>) {
        self.update(|state| state.mapped = mapped);
    }

    /// The router's answer about the mapped port and the port asked for;
    /// `None` when no router is asked.
    pub fn mapping(&self) -> Option<(u16, Mapping)> {
        self.0.state.lock().mapping.clone()
    }

    /// Where the public IP lookup is.
    pub fn lookup(&self) -> LookupState {
        self.0.state.lock().lookup.clone()
    }

    fn set_mapping(&self, mapping: Option<(u16, Mapping)>) {
        self.0.state.lock().mapping = mapping;
    }

    fn set_lookup(&self, lookup: LookupState) {
        self.0.state.lock().lookup = lookup;
    }

    /// Say what went wrong with `part`, or that nothing did.
    pub(crate) fn note(&self, part: Part, note: Option<String>) {
        let mut state = self.0.state.lock();
        match note {
            Some(note) => state.notes.insert(part, note),
            None => state.notes.remove(&part),
        };
    }
}

/// The IP lookup (`public_ip = "lookup"`): one GET to a service that
/// answers with the address the request came from, as plain text.
#[derive(Debug, Clone)]
pub struct Lookup {
    pub client: reqwest::Client,
    /// Where `lookup_via = "wtfismyip"` asks: [`WTFISMYIP`] in the server.
    pub wtfismyip: url::Url,
    /// How long one ask may take, its answer read.
    pub timeout: std::time::Duration,
    /// How long to wait before each ask again after a failed one; when
    /// they are spent, the lookup has failed.
    pub backoff: Vec<std::time::Duration>,
    /// How long until the address is looked up again, after it was found
    /// or the lookup failed: a server started before its network was up,
    /// or whose ISP gave it a new address, finds the right one.
    pub again: std::time::Duration,
}

/// wtfismyip's plain-text answer, over IPv4: the address IPv4 devices
/// reach.
pub const WTFISMYIP: &str = "https://ipv4.wtfismyip.com/text";

/// The longest answer a lookup takes: an IPv4 address and a newline fit
/// many times over.
pub const LOOKUP_MAX_BODY: usize = 64;

impl Lookup {
    /// The real services, with the server's TLS (aws-lc-rs, installed at
    /// start): asked a few times, a minute or two apart at most.
    pub fn system() -> Self {
        let client = reqwest::Client::builder()
            .user_agent(concat!("spoolframe-server/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default();
        Self {
            client,
            wtfismyip: WTFISMYIP.parse().expect("a fixed URL"),
            timeout: std::time::Duration::from_secs(10),
            backoff: [5, 30, 120].map(std::time::Duration::from_secs).to_vec(),
            again: std::time::Duration::from_secs(3600),
        }
    }

    fn url(&self, via: LookupVia) -> &url::Url {
        match via {
            LookupVia::Wtfismyip => &self.wtfismyip,
        }
    }

    /// Ask `via` once for this network's public IPv4 address. An answer
    /// that is not one (an IPv6 address, a private one, a page of HTML)
    /// is an error, never advertised.
    pub async fn once(&self, via: LookupVia) -> Result<Ipv4Addr, String> {
        let url = self.url(via);
        let ask = async {
            let mut response = self
                .client
                .get(url.clone())
                .send()
                .await
                .map_err(|e| format!("cannot ask {}: {e}", via.name()))?;
            if !response.status().is_success() {
                return Err(format!("{} answered {}", via.name(), response.status()));
            }
            let mut body: Vec<u8> = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|e| format!("cannot read {}'s answer: {e}", via.name()))?
            {
                body.extend_from_slice(&chunk);
                if body.len() > LOOKUP_MAX_BODY {
                    return Err(format!(
                        "{}'s answer is longer than {LOOKUP_MAX_BODY} bytes, so not an address",
                        via.name()
                    ));
                }
            }
            Ok(body)
        };
        let body = tokio::time::timeout(self.timeout, ask)
            .await
            .map_err(|_| {
                format!(
                    "{} did not answer within {} s",
                    via.name(),
                    self.timeout.as_secs_f32()
                )
            })??;
        let text = String::from_utf8_lossy(&body);
        let text = text.trim();
        let ip: Ipv4Addr = text.parse().map_err(|_| {
            format!(
                "{} answered {:?}, which is not an IPv4 address",
                via.name(),
                text.chars().take(LOOKUP_MAX_BODY).collect::<String>()
            )
        })?;
        if let Some(why) = not_public(IpAddr::V4(ip)) {
            return Err(format!(
                "{} answered {ip}, which is {why}: this network has no public IPv4 address of \
                 its own",
                via.name()
            ));
        }
        Ok(ip)
    }

    /// Ask `via` until it answers, waiting [`Self::backoff`] between
    /// tries; the last error once they are spent.
    pub async fn retried(&self, via: LookupVia) -> Result<Ipv4Addr, String> {
        self.retried_saying(via, |_, _| {}).await
    }

    /// As [`Self::retried`], telling `failed` each error and whether
    /// another try follows.
    async fn retried_saying(
        &self,
        via: LookupVia,
        mut failed: impl FnMut(&str, bool),
    ) -> Result<Ipv4Addr, String> {
        let mut waits = self.backoff.iter();
        loop {
            match self.once(via).await {
                Ok(ip) => return Ok(ip),
                Err(e) => match waits.next() {
                    Some(wait) => {
                        tracing::debug!(error = %e, ?wait, "the IP lookup failed; asking again");
                        failed(&e, true);
                        tokio::time::sleep(*wait).await;
                    }
                    None => {
                        failed(&e, false);
                        return Err(e);
                    }
                },
            }
        }
    }
}

/// The router's answer about the mapped port, as it stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mapping {
    /// Still asking.
    Asking,
    /// Mapped, and kept mapped: the external address the router gave it.
    /// A renewal that changes it says so again.
    Granted(SocketAddrV4),
    /// No mapping: no router answers, or it said no. Why, in words.
    Refused(String),
}

/// What asks the router to forward a port (`mode = "auto"`).
pub trait PortMapper: Send + Sync + std::fmt::Debug {
    /// Ask for UDP `local_port` to be mapped, renewed before each lease
    /// runs out, for as long as the receiver is held.
    fn map(&self, local_port: NonZeroU16) -> tokio::sync::watch::Receiver<Mapping>;
}

/// The router, over UPnP, NAT-PMP or PCP, through iroh's port mapper
/// (the `portmapper` crate, which renews the mapping before it expires).
#[derive(Debug, Clone, Copy, Default)]
pub struct RouterMapper;

/// How long the router has to say which of the three it speaks.
const PROBE_WAIT: std::time::Duration = std::time::Duration::from_secs(10);
/// How long a mapping may take before it is asked for again, then again
/// after each of [`MAP_RETRIES`]; then it is refused.
const MAP_WAIT: std::time::Duration = std::time::Duration::from_secs(20);
const MAP_RETRIES: [std::time::Duration; 2] = [
    std::time::Duration::from_secs(30),
    std::time::Duration::from_secs(120),
];

impl PortMapper for RouterMapper {
    fn map(&self, local_port: NonZeroU16) -> tokio::sync::watch::Receiver<Mapping> {
        let (tx, rx) = tokio::sync::watch::channel(Mapping::Asking);
        tokio::spawn(async move {
            let client = portmapper::Client::new(portmapper::Config::default());
            let probe = tokio::time::timeout(PROBE_WAIT, client.probe()).await;
            let spoken = match probe {
                Ok(Ok(Ok(found))) => found.upnp || found.pcp || found.nat_pmp,
                Ok(Ok(Err(e))) => {
                    tx.send_replace(Mapping::Refused(format!("no router to ask: {e}")));
                    return;
                }
                _ => false,
            };
            if !spoken {
                tx.send_replace(Mapping::Refused(
                    "the router answers none of UPnP, NAT-PMP and PCP".into(),
                ));
                return;
            }
            client.update_local_port(local_port);
            let mut external = client.watch_external_address();
            let mut retries = MAP_RETRIES.iter();
            let mut gave_up = false;
            let wait = tokio::time::sleep(MAP_WAIT);
            tokio::pin!(wait);
            loop {
                tokio::select! {
                    () = tx.closed() => {
                        client.deactivate();
                        return;
                    }
                    changed = external.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        let now = *external.borrow_and_update();
                        tx.send_replace(match now {
                            Some(addr) => Mapping::Granted(addr),
                            // Lost; the mapper asks again on its own.
                            None => Mapping::Asking,
                        });
                    }
                    // Until it is granted once: then the mapper keeps it.
                    () = &mut wait, if !gave_up && external.borrow().is_none() => {
                        match retries.next() {
                            Some(again) => {
                                client.procure_mapping();
                                wait.as_mut().reset(tokio::time::Instant::now() + *again);
                            }
                            None => {
                                gave_up = true;
                                tx.send_replace(Mapping::Refused(
                                    "the router did not map the port".into(),
                                ));
                            }
                        }
                    }
                }
            }
        });
        rx
    }
}

/// How long [`try_mapping`] waits for the router's answer.
pub const TRY_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Ask `mapper` for `local_port` now and give the first answer, or
/// `Asking` after [`TRY_WAIT`]; the mapping is given up when this returns.
/// What the wizard shows before `mode = "auto"` is chosen.
pub async fn try_mapping(mapper: &dyn PortMapper, local_port: NonZeroU16) -> Mapping {
    let mut mapping = mapper.map(local_port);
    let answered =
        match tokio::time::timeout(TRY_WAIT, mapping.wait_for(|m| *m != Mapping::Asking)).await {
            Ok(Ok(answer)) => Some(answer.clone()),
            // The mapper is gone: its last word.
            Ok(Err(_)) => None,
            Err(_) => return Mapping::Asking,
        };
    answered.unwrap_or_else(|| mapping.borrow().clone())
}

/// What finds the public addresses at run time: real ones in the server,
/// stand-ins in tests, which must never reach a router or the Internet.
#[derive(Debug, Clone)]
pub struct Finders {
    pub lookup: Lookup,
    pub mapper: Arc<dyn PortMapper>,
}

impl Finders {
    /// The system's.
    pub fn system() -> Self {
        Self {
            lookup: Lookup::system(),
            mapper: Arc::new(RouterMapper),
        }
    }
}

/// Find what `wan` says to find, into `public`, until `endpoint` closes.
/// Nothing here waits: startup never blocks on a router or a lookup.
pub(crate) fn start(public: &Public, wan: &Wan, endpoint: &iroh::Endpoint, finders: &Finders) {
    let Some(mapped) = wan.mapped_port.clone() else {
        return;
    };
    let bound = endpoint.bound_sockets();
    let own_port = bound
        .iter()
        .find(|b| b.is_ipv4())
        .or_else(|| bound.first())
        .map(SocketAddr::port)
        .and_then(NonZeroU16::new);
    let public = public.clone();
    let lookup = finders.lookup.clone();
    let mapper = finders.mapper.clone();
    let closed = endpoint.clone();
    tokio::spawn(async move {
        let finding = async {
            match mapped.mode {
                Mode::Manual => {
                    if let Some(port) = mapped.port.or(own_port.map(NonZeroU16::get)) {
                        forwarded(&public, &lookup, &mapped, port).await;
                    }
                }
                // The router maps the endpoint's own port; the port it
                // reports is the one advertised.
                Mode::Auto => {
                    if let Some(port) = own_port {
                        router_mapped(&public, &lookup, &mapped, &*mapper, port).await;
                    }
                }
            }
        };
        tokio::select! {
            () = closed.closed() => {}
            () = finding => {}
        }
    });
}

/// The public IP `mapped` names: written down, or looked up. `None`, with
/// a note and a log line, when the lookup fails.
async fn public_ip(public: &Public, lookup: &Lookup, mapped: &MappedPort) -> Option<PublicAddr> {
    let port = 0;
    match (mapped.public_ip, mapped.lookup_via) {
        (PublicIp::Fixed(ip), _) => Some(PublicAddr {
            addr: SocketAddr::new(ip, port),
            source: Source::MappedPort,
        }),
        (PublicIp::Lookup, Some(via)) => {
            public.set_lookup(LookupState::Asking);
            let found = lookup
                .retried_saying(via, |error, retrying| {
                    public.set_lookup(LookupState::Failed {
                        error: error.to_owned(),
                        retrying,
                    });
                })
                .await;
            looked_up(public, via, found)
        }
        // Refused at load: there is nothing to advertise it with.
        (PublicIp::Lookup, None) => None,
    }
}

/// What a lookup through `via` found, said on the status page and
/// advertised as the mapped port's IP.
fn looked_up(
    public: &Public,
    via: LookupVia,
    found: Result<Ipv4Addr, String>,
) -> Option<PublicAddr> {
    let port = 0;
    match found {
        Ok(ip) => {
            tracing::info!(%ip, "the IP lookup found this network's public address");
            public.set_lookup(LookupState::Found {
                ip,
                via,
                at: std::time::SystemTime::now(),
            });
            public.note(Part::Lookup, None);
            Some(PublicAddr {
                addr: SocketAddr::new(IpAddr::V4(ip), port),
                source: Source::Lookup,
            })
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "the IP lookup failed, so the mapped port is not advertised; set \
                 network.wan.mapped_port.public_ip to this network's public address"
            );
            public.note(Part::Lookup, Some(format!("public IP lookup failed: {e}")));
            None
        }
    }
}

/// The operator forwarded `port` (`mode = "manual"`): advertise it with
/// the public IP written down, or once it is looked up, and with what each
/// later lookup ([`Lookup::again`]) finds. A lookup that fails keeps what
/// an earlier one found.
async fn forwarded(public: &Public, lookup: &Lookup, mapped: &MappedPort, port: u16) {
    loop {
        if let Some(mut found) = public_ip(public, lookup, mapped).await {
            found.addr.set_port(port);
            public.set_mapped(Some(found));
        }
        if mapped.public_ip != PublicIp::Lookup {
            return;
        }
        tokio::time::sleep(lookup.again).await;
    }
}

/// Ask the router to map `local_port` (`mode = "auto"`) and advertise
/// what it gives: its external address when that is a public one; when it
/// is not (a router behind carrier-grade NAT reports its own private
/// side), `public_ip` with the external port. A refusal is one log line
/// and a status note; a mapping granted later, or renewed to another
/// address, is advertised then.
async fn router_mapped(
    public: &Public,
    lookup: &Lookup,
    mapped: &MappedPort,
    mapper: &dyn PortMapper,
    local_port: NonZeroU16,
) {
    let mut mapping = mapper.map(local_port);
    // The public IP for a mapping whose external address is not one:
    // found at the first such mapping, and looked up again every
    // `lookup.again` while one is advertised with it.
    let mut fallback: Option<Option<PublicAddr>> = None;
    let mut refused = false;
    let mut look_again = false;
    loop {
        if look_again {
            look_again = false;
            if let Some(found) = public_ip(public, lookup, mapped).await {
                fallback = Some(Some(found));
            }
        }
        let now = mapping.borrow_and_update().clone();
        public.set_mapping(Some((local_port.get(), now.clone())));
        match now {
            // Still asking, or asking again after a lapse: what is
            // advertised stays until the router answers.
            Mapping::Asking => {}
            Mapping::Granted(external) => {
                public.note(Part::Mapping, None);
                if refused {
                    tracing::info!(%external, "the router mapped the port after all");
                }
                refused = false;
                let found = if not_public(IpAddr::V4(*external.ip())).is_none() {
                    if fallback.is_none() && mapped.public_ip == PublicIp::Lookup {
                        public.set_lookup(LookupState::NotNeeded);
                    }
                    Some(PublicAddr {
                        addr: SocketAddr::V4(external),
                        source: Source::MappedPort,
                    })
                } else {
                    if fallback.is_none() {
                        tracing::info!(
                            %external,
                            "the router's own address is not a public one; advertising the \
                             mapped port with network.wan.mapped_port.public_ip"
                        );
                        fallback = Some(public_ip(public, lookup, mapped).await);
                    }
                    fallback.flatten().map(|mut found| {
                        found.addr.set_port(external.port());
                        found
                    })
                };
                public.set_mapped(found);
            }
            Mapping::Refused(why) => {
                if !refused {
                    tracing::warn!(
                        reason = %why,
                        "the router did not map the server's port, so no mapped address is \
                         advertised; forward it by hand and set \
                         network.wan.mapped_port.mode = \"manual\""
                    );
                }
                refused = true;
                public.note(Part::Mapping, Some(format!("router port mapping: {why}")));
                public.set_mapped(None);
            }
        }
        let looking = fallback.is_some() && mapped.public_ip == PublicIp::Lookup;
        tokio::select! {
            changed = mapping.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            () = tokio::time::sleep(lookup.again), if looking => look_again = true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn only_addresses_the_internet_routes_are_public() {
        for public in [
            "1.2.3.4",
            "100.63.255.255",
            "100.128.0.1",
            "172.32.0.1",
            "203.0.113.7",
            "8.8.8.8",
            "2001:db8::1",
            "2a01:4f8::1",
            "::ffff:1.2.3.4",
        ] {
            assert_eq!(not_public(ip(public)), None, "{public}");
        }
        for (address, why) in [
            ("192.168.1.5", "a private address (192.168.0.0/16)"),
            ("10.0.0.1", "a private address (10.0.0.0/8)"),
            ("172.16.0.1", "a private address (172.16.0.0/12)"),
            ("172.31.255.1", "a private address (172.16.0.0/12)"),
            ("127.0.0.1", "a loopback address (127.0.0.0/8)"),
            ("169.254.1.1", "a link-local address (169.254.0.0/16)"),
            ("100.64.0.1", "a carrier-grade NAT address (100.64.0.0/10)"),
            (
                "100.127.255.254",
                "a carrier-grade NAT address (100.64.0.0/10)",
            ),
            ("0.0.0.0", "an unspecified address (0.0.0.0/8)"),
            ("224.0.0.1", "a multicast address (224.0.0.0/4)"),
            ("255.255.255.255", "a reserved address (240.0.0.0/4)"),
            ("::1", "a loopback address (::1)"),
            ("::", "an unspecified address (::)"),
            ("fe80::1", "a link-local address (fe80::/10)"),
            ("fd12:3456::1", "a unique local address (fc00::/7)"),
            ("ff02::1", "a multicast address (ff00::/8)"),
            ("::ffff:192.168.1.5", "a private address (192.168.0.0/16)"),
            (
                "::ffff:100.64.0.1",
                "a carrier-grade NAT address (100.64.0.0/10)",
            ),
        ] {
            assert_eq!(not_public(ip(address)), Some(why), "{address}");
        }
    }

    #[test]
    fn a_static_entry_is_a_public_address_and_a_port() {
        assert_eq!(
            parse_static("1.2.3.4:7420"),
            Ok("1.2.3.4:7420".parse().unwrap())
        );
        assert_eq!(
            parse_static(" [2001:db8::1]:7420 "),
            Ok("[2001:db8::1]:7420".parse().unwrap())
        );
        assert!(
            parse_static("1.2.3.4")
                .unwrap_err()
                .contains("not an address and a port")
        );
        assert!(
            parse_static("1.2.3.4:0")
                .unwrap_err()
                .contains("the port is 0")
        );
        let private = parse_static("192.168.1.5:7420").unwrap_err();
        assert!(
            private.contains("a private address (192.168.0.0/16)"),
            "{private}"
        );
        let cgnat = parse_static("100.64.1.2:7420").unwrap_err();
        assert!(cgnat.contains("(100.64.0.0/10)"), "{cgnat}");
        assert!(
            parse_static("[fe80::1]:7420")
                .unwrap_err()
                .contains("link-local")
        );
        assert!(
            parse_static("127.0.0.1:7420")
                .unwrap_err()
                .contains("loopback")
        );
    }

    fn static_wan(addrs: &[&str]) -> Wan {
        Wan {
            statics: addrs.iter().map(|a| a.parse().unwrap()).collect(),
            mapped_port: None,
        }
    }

    #[test]
    fn without_wan_nothing_is_advertised() {
        let public = Public::default();
        assert!(!public.configured());
        assert_eq!(public.addrs(), []);
    }

    #[test]
    fn static_addresses_are_advertised_first_and_each_once() {
        let public = Public::new(Some(&static_wan(&["1.2.3.4:7420", "[2001:db8::1]:7420"])));
        public.set_mapped(Some(PublicAddr {
            addr: "1.2.3.4:7420".parse().unwrap(),
            source: Source::Lookup,
        }));
        assert_eq!(
            public.addrs(),
            [
                PublicAddr {
                    addr: "1.2.3.4:7420".parse().unwrap(),
                    source: Source::Static,
                },
                PublicAddr {
                    addr: "[2001:db8::1]:7420".parse().unwrap(),
                    source: Source::Static,
                },
            ]
        );
    }

    #[test]
    fn a_change_to_the_addresses_is_signalled_and_a_note_alone_is_not() {
        let public = Public::new(Some(&static_wan(&[])));
        let mut changes = public.changes();
        public.note(Part::Lookup, Some("the lookup failed".into()));
        assert!(!changes.has_changed().unwrap());
        assert_eq!(public.notes(), ["the lookup failed"]);
        let mapped = PublicAddr {
            addr: "203.0.113.9:7420".parse().unwrap(),
            source: Source::MappedPort,
        };
        public.set_mapped(Some(mapped));
        assert!(changes.has_changed().unwrap());
        changes.mark_unchanged();
        public.set_mapped(Some(mapped));
        assert!(!changes.has_changed().unwrap());
    }
}
