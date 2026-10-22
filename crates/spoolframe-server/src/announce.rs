//! Which of its addresses the server tells a signed-in device (PX-12),
//! as `network.announce` says.
//!
//! The endpoint's own addresses are every address on every interface,
//! Docker's and libvirt's bridges included, and no device reaches those.
//! So the told list is: the addresses on the allowed interfaces (every
//! interface but the virtual ones when none is named), less those on an
//! ignored interface, less those in an ignored range, plus the added ones.
//! An added address is told even when an ignore matches it: it was named
//! on purpose.
//!
//! A virtual interface ([`VIRTUAL_INTERFACES`]) is told only when an
//! allowed interface pattern names it. Its address is worse than useless
//! to a device: 172.17.0.1 is, on most devices, their own docker0.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use iroh::Endpoint;

pub use crate::admin_http::Network;

/// An address to tell besides the endpoint's own (`network.announce.add`):
/// one the host does not see, as a NAT's public address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Added {
    /// `203.0.113.7:7420`: told as written.
    Socket(SocketAddr),
    /// `203.0.113.7`: told with the endpoint's port.
    Ip(IpAddr),
}

impl std::str::FromStr for Added {
    type Err = String;

    /// `ip:port`, `[ipv6]:port`, or an address alone.
    fn from_str(text: &str) -> Result<Self, String> {
        let text = text.trim();
        if let Ok(addr) = text.parse::<SocketAddr>() {
            if addr.port() == 0 {
                return Err(format!("{text:?}: the port is 0"));
            }
            return Ok(Self::Socket(SocketAddr::new(
                addr.ip().to_canonical(),
                addr.port(),
            )));
        }
        text.parse::<IpAddr>()
            .map(|ip| Self::Ip(ip.to_canonical()))
            .map_err(|_| format!("{text:?} is not an address, or an address and a port"))
    }
}

/// An interface name, or a pattern of them: `*` stands for any run of
/// characters, `?` for one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Glob(String);

impl std::str::FromStr for Glob {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("an empty interface name".into());
        }
        Ok(Self(text.to_owned()))
    }
}

impl Glob {
    pub fn matches(&self, name: &str) -> bool {
        let pattern: Vec<char> = self.0.chars().collect();
        let name: Vec<char> = name.chars().collect();
        // The usual backtracking match: the last `*` seen takes one more
        // character each time what follows it fails.
        let (mut p, mut n) = (0, 0);
        let mut star: Option<(usize, usize)> = None;
        while n < name.len() {
            match pattern.get(p) {
                Some('*') => {
                    star = Some((p, n));
                    p += 1;
                }
                Some(&c) if c == '?' || c == name[n] => {
                    p += 1;
                    n += 1;
                }
                _ => match star {
                    Some((sp, sn)) => {
                        p = sp + 1;
                        n = sn + 1;
                        star = Some((sp, sn + 1));
                    }
                    None => return false,
                },
            }
        }
        pattern[p..].iter().all(|c| *c == '*')
    }
}

/// A network interface: its name and its addresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    pub name: String,
    pub addrs: Vec<IpAddr>,
}

/// This machine's interfaces that are up, now, each with its addresses.
pub fn interfaces() -> Vec<Interface> {
    let mut found: Vec<Interface> = Vec::new();
    for addr in crate::ifaddrs::up() {
        if addr.name.is_empty() {
            continue;
        }
        match found.iter_mut().find(|i| i.name == addr.name) {
            Some(interface) => interface.addrs.push(addr.ip),
            None => found.push(Interface {
                name: addr.name,
                addrs: vec![addr.ip],
            }),
        }
    }
    found
}

/// Docker's bridges, its networks', their veth pairs' and libvirt's
/// bridges: on the host only, never reached from another device. Not told
/// unless [`Announce::interfaces`] names them.
pub const VIRTUAL_INTERFACES: &[&str] = &["docker*", "br-*", "veth*", "virbr*"];

/// `network.announce`, checked. The default tells every address the
/// endpoint has that another device may reach, but those on virtual
/// interfaces.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Announce {
    pub add: Vec<Added>,
    pub ignore: Vec<Network>,
    /// Never told, even when `interfaces` names them.
    pub ignore_interfaces: Vec<Glob>,
    /// Only addresses on these; when empty, any interface's but the
    /// virtual ones ([`VIRTUAL_INTERFACES`]).
    pub interfaces: Vec<Glob>,
}

impl Announce {
    /// What to tell, of the endpoint's reachable addresses `direct` (see
    /// `direct_addrs`), with `interfaces` as the machine has them. An
    /// added address without a port takes the port of the first of
    /// `bound` (the endpoint's sockets) of its family, else of any.
    pub fn told(
        &self,
        direct: &[SocketAddr],
        bound: &[SocketAddr],
        interfaces: &[Interface],
    ) -> Vec<SocketAddr> {
        let any = |globs: &[Glob], on: &[&str]| {
            on.iter().any(|name| globs.iter().any(|g| g.matches(name)))
        };
        let mut told: Vec<SocketAddr> = Vec::new();
        for addr in direct {
            let ip = addr.ip().to_canonical();
            let on: Vec<&str> = interfaces
                .iter()
                .filter(|i| i.addrs.iter().any(|a| a.to_canonical() == ip))
                .map(|i| i.name.as_str())
                .collect();
            let allowed = if self.interfaces.is_empty() {
                !on.iter().any(|name| is_virtual(name))
            } else {
                any(&self.interfaces, &on)
            };
            let ignored = any(&self.ignore_interfaces, &on)
                || self.ignore.iter().any(|range| range.contains(ip));
            if allowed && !ignored && !told.contains(addr) {
                told.push(*addr);
            }
        }
        for added in &self.add {
            let addr = match *added {
                Added::Socket(addr) => addr,
                Added::Ip(ip) => {
                    let port = port_of(bound, ip.is_ipv4());
                    match port {
                        Some(port) => SocketAddr::new(ip, port),
                        None => continue,
                    }
                }
            };
            if !told.contains(&addr) {
                told.push(addr);
            }
        }
        told
    }
}

/// Whether interface `name` is a virtual one ([`VIRTUAL_INTERFACES`]).
pub(crate) fn is_virtual(name: &str) -> bool {
    VIRTUAL_INTERFACES
        .iter()
        .any(|pattern| Glob(String::from(*pattern)).matches(name))
}

/// The port of the first of `bound` (the endpoint's sockets) of the
/// family `ipv4` names, else of the first: the port told with an added
/// address, and the one a router is asked to map (`network.wan`).
pub(crate) fn port_of(bound: &[SocketAddr], ipv4: bool) -> Option<u16> {
    bound
        .iter()
        .find(|b| b.is_ipv4() == ipv4)
        .or_else(|| bound.first())
        .map(SocketAddr::port)
}

/// Of `addrs`, the endpoint's own, those another device may reach it at
/// (PX-12): every one but loopback, link-local and unspecified addresses,
/// each once, in the order given. LAN, public and VPN addresses all stay:
/// which of them a device can reach is for it to find out.
pub(crate) fn direct_addrs(addrs: impl IntoIterator<Item = SocketAddr>) -> Vec<SocketAddr> {
    let mut told: Vec<SocketAddr> = Vec::new();
    for addr in addrs {
        let addr = SocketAddr::new(addr.ip().to_canonical(), addr.port());
        let unreachable = match addr.ip() {
            IpAddr::V4(v4) => v4.is_loopback() || v4.is_link_local() || v4.is_unspecified(),
            IpAddr::V6(v6) => v6.is_loopback() || v6.is_unicast_link_local() || v6.is_unspecified(),
        };
        if !unreachable && !told.contains(&addr) {
            told.push(addr);
        }
    }
    told
}

/// The endpoint and `network.announce`: what a signed-in device is told,
/// worked out at each ask, so an interface that comes or goes counts.
#[derive(Clone)]
pub struct Told {
    endpoint: Endpoint,
    announce: Arc<Announce>,
    relays: Arc<Vec<String>>,
    /// The pkarr relay it publishes to (`network.pkarr`, P6-05).
    lookup: Option<String>,
    /// The public addresses (`network.wan`, P6-02), told after the rest.
    public: crate::wan::Public,
}

impl std::fmt::Debug for Told {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Told")
            .field("announce", &self.announce)
            .field("relays", &self.relays)
            .field("public", &self.public)
            .finish_non_exhaustive()
    }
}

impl Told {
    /// What `endpoint` tells, as `announce` says, with the URLs of the
    /// relays it may use (`network.relays`, in order) and the public
    /// addresses `public` has found.
    pub(crate) fn new(
        endpoint: Endpoint,
        announce: Announce,
        relays: Vec<String>,
        lookup: Option<String>,
        public: crate::wan::Public,
    ) -> Self {
        Self {
            endpoint,
            announce: Arc::new(announce),
            relays: Arc::new(relays),
            lookup,
            public,
        }
    }

    /// The addresses told now: the endpoint's, as `network.announce`
    /// filters them, then the public ones not among them. It reads the
    /// interfaces from the system, so call it off the async threads.
    pub fn addrs(&self) -> Vec<SocketAddr> {
        let direct = direct_addrs(self.endpoint.addr().ip_addrs().copied());
        // Always read: a virtual interface is known by its name.
        let interfaces = interfaces();
        let mut told = self
            .announce
            .told(&direct, &self.endpoint.bound_sockets(), &interfaces);
        for public in self.public.addrs() {
            if !told.contains(&public.addr) {
                told.push(public.addr);
            }
        }
        told
    }

    /// The port of the endpoint's first IPv4 socket, else of its first:
    /// the one a router is asked to map (`network.wan`).
    pub fn own_port(&self) -> Option<u16> {
        port_of(&self.endpoint.bound_sockets(), true)
    }

    /// The public addresses (`network.wan`), each with its source.
    pub fn public(&self) -> &crate::wan::Public {
        &self.public
    }

    /// The URLs of the relays the endpoint may use (`network.relays`),
    /// its home relay now first: the one it is reached through, while the
    /// rest are where it fails over to (P6-03). None while relays are off.
    pub fn relays(&self) -> Vec<String> {
        let mut told: Vec<String> = self
            .endpoint
            .addr()
            .relay_urls()
            .map(ToString::to_string)
            .collect();
        for relay in self.relays.iter() {
            if !told.contains(relay) {
                told.push(relay.clone());
            }
        }
        told
    }

    /// The pkarr relay the server publishes where it is to
    /// (`network.pkarr`, P6-05), for a device to look it up there.
    pub fn lookup(&self) -> Option<String> {
        self.lookup.clone()
    }

    /// What a signed-in connection is pushed now: [`Self::addrs`],
    /// [`Self::relays`] and [`Self::lookup`]. It reads the interfaces, as
    /// `addrs` does.
    pub fn event(&self) -> spoolframe_protocol::Event {
        spoolframe_protocol::Event::ServerAddresses {
            addrs: self.addrs(),
            relays: self.relays(),
            lookup: self.lookup(),
        }
    }

    /// A message each time the endpoint's own addresses change (iroh
    /// watches the interfaces), or the public ones do (`network.wan`: a
    /// mapping granted, a lookup answered, the host's IPv6 addresses
    /// changed), until the endpoint closes.
    pub(crate) fn changes(&self) -> tokio::sync::mpsc::Receiver<()> {
        use iroh::Watcher as _;
        let (changed, changes) = tokio::sync::mpsc::channel(1);
        let mut watcher = self.endpoint.watch_addr();
        let mut public = self.public.changes();
        let closed = self.endpoint.closed();
        tokio::spawn(async move {
            tokio::pin!(closed);
            loop {
                tokio::select! {
                    () = &mut closed => return,
                    updated = watcher.updated() => {
                        if updated.is_err() {
                            return;
                        }
                    }
                    // Its sender lives as long as this `Told`: never closed.
                    Ok(()) = public.changed() => {}
                }
                // Full: a change is waiting to be read already, and
                // reading it reads this one too.
                if let Err(tokio::sync::mpsc::error::TrySendError::Closed(())) =
                    changed.try_send(())
                {
                    return;
                }
            }
        });
        changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn interface(name: &str, addrs: &[&str]) -> Interface {
        Interface {
            name: name.into(),
            addrs: addrs.iter().map(|a| a.parse().unwrap()).collect(),
        }
    }

    fn globs(list: &[&str]) -> Vec<Glob> {
        list.iter().map(|g| g.parse().unwrap()).collect()
    }

    /// The user's server: a LAN address, a VPN one, and Docker's bridges.
    fn machine() -> (Vec<SocketAddr>, Vec<Interface>) {
        let direct = vec![
            addr("192.168.86.176:7420"),
            addr("10.10.0.2:7420"),
            addr("172.17.0.1:7420"),
            addr("172.18.0.1:7420"),
            addr("[2001:db8::5]:7420"),
        ];
        let interfaces = vec![
            interface("lo", &["127.0.0.1", "::1"]),
            interface("enp5s0", &["192.168.86.176", "2001:db8::5"]),
            interface("wg0", &["10.10.0.2"]),
            interface("docker0", &["172.17.0.1"]),
            interface("br-0123abcd", &["172.18.0.1"]),
        ];
        (direct, interfaces)
    }

    const BOUND: [&str; 2] = ["0.0.0.0:7420", "[::]:7421"];

    fn bound() -> Vec<SocketAddr> {
        BOUND.iter().map(|a| addr(a)).collect()
    }

    /// 172.17.0.1 told to a device is worse than useless: on most devices
    /// it is their own docker0.
    #[test]
    fn without_settings_every_address_but_virtual_interfaces_is_told() {
        let (direct, interfaces) = machine();
        assert_eq!(
            Announce::default().told(&direct, &bound(), &interfaces),
            [
                addr("192.168.86.176:7420"),
                addr("10.10.0.2:7420"),
                addr("[2001:db8::5]:7420"),
            ]
        );
    }

    #[test]
    fn a_virtual_interface_named_in_interfaces_is_told() {
        let (direct, interfaces) = machine();
        let announce = Announce {
            interfaces: globs(&["enp*", "docker0"]),
            ..Announce::default()
        };
        assert_eq!(
            announce.told(&direct, &bound(), &interfaces),
            [
                addr("192.168.86.176:7420"),
                addr("172.17.0.1:7420"),
                addr("[2001:db8::5]:7420"),
            ]
        );
    }

    #[test]
    fn addresses_on_an_ignored_interface_are_not_told() {
        let (direct, interfaces) = machine();
        let announce = Announce {
            ignore_interfaces: globs(&["docker*", "br-*", "veth*", "virbr*"]),
            ..Announce::default()
        };
        assert_eq!(
            announce.told(&direct, &bound(), &interfaces),
            [
                addr("192.168.86.176:7420"),
                addr("10.10.0.2:7420"),
                addr("[2001:db8::5]:7420"),
            ]
        );
    }

    #[test]
    fn with_allowed_interfaces_only_their_addresses_are_told() {
        let (direct, interfaces) = machine();
        let announce = Announce {
            interfaces: globs(&["wg?"]),
            ..Announce::default()
        };
        assert_eq!(
            announce.told(&direct, &bound(), &interfaces),
            [addr("10.10.0.2:7420")]
        );
    }

    #[test]
    fn an_ignored_interface_wins_over_an_allowed_one() {
        let (direct, interfaces) = machine();
        let announce = Announce {
            interfaces: globs(&["*"]),
            ignore_interfaces: globs(&["enp5s0"]),
            ..Announce::default()
        };
        assert_eq!(
            announce.told(&direct, &bound(), &interfaces),
            [
                addr("10.10.0.2:7420"),
                addr("172.17.0.1:7420"),
                addr("172.18.0.1:7420"),
            ]
        );
    }

    #[test]
    fn an_address_on_no_known_interface_is_told_unless_interfaces_are_named() {
        let direct = [addr("198.51.100.9:7420")];
        let ignoring = Announce {
            ignore_interfaces: globs(&["docker*"]),
            ..Announce::default()
        };
        assert_eq!(ignoring.told(&direct, &bound(), &[]), direct);
        let allowing = Announce {
            interfaces: globs(&["enp*"]),
            ..Announce::default()
        };
        assert_eq!(allowing.told(&direct, &bound(), &[]), []);
    }

    #[test]
    fn addresses_in_an_ignored_range_are_not_told() {
        let (direct, interfaces) = machine();
        let announce = Announce {
            ignore: vec![
                "172.16.0.0/12".parse().unwrap(),
                "2001:db8::5".parse().unwrap(),
            ],
            ..Announce::default()
        };
        assert_eq!(
            announce.told(&direct, &bound(), &interfaces),
            [addr("192.168.86.176:7420"), addr("10.10.0.2:7420")]
        );
    }

    #[test]
    fn added_addresses_are_told_last_with_the_endpoint_port_when_they_have_none() {
        let (direct, interfaces) = machine();
        let announce = Announce {
            interfaces: globs(&["wg0"]),
            add: vec![
                "203.0.113.7".parse().unwrap(),
                "203.0.113.8:443".parse().unwrap(),
                "2001:db8::77".parse().unwrap(),
                "10.10.0.2:7420".parse().unwrap(),
            ],
            ..Announce::default()
        };
        assert_eq!(
            announce.told(&direct, &bound(), &interfaces),
            [
                addr("10.10.0.2:7420"),
                addr("203.0.113.7:7420"),
                addr("203.0.113.8:443"),
                addr("[2001:db8::77]:7421"),
            ]
        );
    }

    #[test]
    fn an_added_address_is_told_though_an_ignore_matches_it() {
        let (direct, interfaces) = machine();
        let announce = Announce {
            ignore: vec!["0.0.0.0/0".parse().unwrap(), "::/0".parse().unwrap()],
            ignore_interfaces: globs(&["docker*"]),
            add: vec!["172.17.0.1".parse().unwrap()],
            ..Announce::default()
        };
        assert_eq!(
            announce.told(&direct, &bound(), &interfaces),
            [addr("172.17.0.1:7420")]
        );
    }

    #[test]
    fn an_added_address_without_a_port_and_no_socket_is_left_out() {
        let announce = Announce {
            add: vec!["203.0.113.7".parse().unwrap()],
            ..Announce::default()
        };
        assert_eq!(announce.told(&[], &[], &[]), []);
    }

    #[test]
    fn an_added_address_is_an_address_with_or_without_a_port() {
        let parse = |s: &str| s.parse::<Added>();
        assert_eq!(
            parse("203.0.113.7"),
            Ok(Added::Ip("203.0.113.7".parse().unwrap()))
        );
        assert_eq!(
            parse("203.0.113.7:7420"),
            Ok(Added::Socket(addr("203.0.113.7:7420")))
        );
        assert_eq!(
            parse("[2001:db8::1]:7420"),
            Ok(Added::Socket(addr("[2001:db8::1]:7420")))
        );
        assert_eq!(
            parse("2001:db8::1"),
            Ok(Added::Ip("2001:db8::1".parse().unwrap()))
        );
        assert!(parse("203.0.113.7:0").is_err());
        assert!(parse("nas.local").is_err());
        assert!(parse("203.0.113.7/24").is_err());
    }

    #[test]
    fn this_machines_interfaces_are_read_with_their_names() {
        let found = interfaces();
        let loopback = found
            .iter()
            .find(|i| i.addrs.iter().any(IpAddr::is_loopback))
            .expect("an interface with a loopback address");
        assert!(!loopback.name.is_empty());
    }

    #[test]
    fn a_glob_matches_names_with_stars_and_question_marks() {
        let glob = |g: &str| g.parse::<Glob>().unwrap();
        assert!(glob("docker*").matches("docker0"));
        assert!(glob("docker*").matches("docker"));
        assert!(!glob("docker*").matches("xdocker0"));
        assert!(glob("br-*").matches("br-0123abcd"));
        assert!(!glob("br-*").matches("bridge0"));
        assert!(glob("eth0").matches("eth0"));
        assert!(!glob("eth0").matches("eth01"));
        assert!(glob("wg?").matches("wg0"));
        assert!(!glob("wg?").matches("wg10"));
        assert!(glob("*a*b").matches("xxaxxab"));
        assert!(!glob("*a*b").matches("xxaxxa"));
        assert!(glob("*").matches(""));
        assert!("".parse::<Glob>().is_err());
        assert!("  ".parse::<Glob>().is_err());
    }

    #[test]
    fn a_device_is_told_lan_public_and_vpn_addresses_with_their_port_only() {
        let told = direct_addrs([
            addr("192.168.86.176:4433"),
            addr("127.0.0.1:4433"),
            addr("10.10.0.2:4433"),
            addr("169.254.3.4:4433"),
            addr("[2001:db8::1]:4433"),
            addr("[::1]:4433"),
            addr("[fe80::1]:4433"),
            addr("0.0.0.0:4433"),
            addr("100.64.1.2:4433"),
            addr("[fd12:3456::1]:4433"),
            addr("[::ffff:192.168.86.176]:4433"),
        ]);
        assert_eq!(
            told,
            [
                addr("192.168.86.176:4433"),
                addr("10.10.0.2:4433"),
                addr("[2001:db8::1]:4433"),
                addr("100.64.1.2:4433"),
                addr("[fd12:3456::1]:4433"),
            ]
        );
    }
}
