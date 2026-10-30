//! Whether this device is on the server's home network, as the server
//! counts it (the iOS app's HomeNetwork): a LAN path, or a direct IPv6
//! path into one of this device's own global /64s, the home's prefix,
//! which is not a private address but is the same network. And the
//! quality a playback starts with from that.
//!
//! The app passes this device's addresses in: it reads them with its
//! platform's own API, where a sandbox (Android, tvOS) lets it.

use std::net::{IpAddr, Ipv6Addr};

use crate::types::Platform;
use crate::{ConnectionPath, PlaybackQuality, Session};

/// The first 64 bits of an IPv6 address: its network.
fn prefix64(ip: Ipv6Addr) -> [u8; 8] {
    let octets = ip.octets();
    let mut prefix = [0; 8];
    prefix.copy_from_slice(&octets[..8]);
    prefix
}

/// Global unicast (2000::/3): not link-local, unique-local or loopback.
/// The one rule for a public IPv6 address: a sign-in approval's address
/// class reads it too ([`crate::address_class`]).
pub(crate) fn global(ip: Ipv6Addr) -> bool {
    ip.octets()[0] & 0xE0 == 0x20
}

/// Whether `ip` is in the /64 of one of `local_addrs`' global IPv6
/// addresses.
fn in_local_64(ip: Ipv6Addr, local_addrs: &[IpAddr]) -> bool {
    local_addrs.iter().any(|local| match local {
        IpAddr::V6(local) => global(*local) && prefix64(*local) == prefix64(ip),
        IpAddr::V4(_) => false,
    })
}

/// Whether `path` is inside the home network: the LAN, or a direct IPv6
/// path into the /64 of one of `local_addrs` (this device's own addresses;
/// only global IPv6 ones count). No path yet is not home: see
/// [`ConnectionPath::at_home`] for a server heard on this network.
pub fn at_home(path: &ConnectionPath, local_addrs: &[IpAddr]) -> bool {
    match path {
        ConnectionPath::Lan { .. } => true,
        ConnectionPath::Direct { addr } => match addr.ip() {
            IpAddr::V6(v6) => in_local_64(v6, local_addrs),
            IpAddr::V4(_) => false,
        },
        ConnectionPath::Relay | ConnectionPath::Unknown => false,
    }
}

impl ConnectionPath {
    /// [`at_home`], and with no path yet, `heard`: the server was heard
    /// on this network.
    pub fn at_home(&self, local_addrs: &[IpAddr], heard: bool) -> bool {
        match self {
            ConnectionPath::Unknown => heard,
            path => at_home(path, local_addrs),
        }
    }
}

impl PlaybackQuality {
    /// The quality a playback starts with when the viewer picked none:
    /// the device's own `limit` for this network (bits per second) when
    /// it has one; else Original on a TV box (tvOS), which is always at
    /// home; else Original at home, Auto away.
    pub fn default_for(limit: Option<u64>, platform: Platform, at_home: bool) -> PlaybackQuality {
        match limit.filter(|l| *l > 0) {
            Some(limit) => PlaybackQuality::Bitrate(limit),
            None if platform == Platform::Tvos || at_home => PlaybackQuality::Original,
            None => PlaybackQuality::Auto,
        }
    }
}

impl Session {
    /// Whether this device is on the server's home network now
    /// ([`ConnectionPath::at_home`]): `local_addrs` are this device's own
    /// addresses, as the app reads them.
    pub fn at_home(&self, local_addrs: &[IpAddr]) -> bool {
        let heard = self.client.inner.discovery.get(&self.server).is_some();
        self.connection_path().at_home(local_addrs, heard)
    }

    /// The quality a playback starts with when the viewer picked none
    /// ([`PlaybackQuality::default_for`]): `limit` is the device's own
    /// limit for this network, if it has one; `local_addrs` are this
    /// device's addresses.
    pub fn default_quality(&self, limit: Option<u64>, local_addrs: &[IpAddr]) -> PlaybackQuality {
        PlaybackQuality::default_for(
            limit,
            self.client.inner.device.platform,
            self.at_home(local_addrs),
        )
    }
}

/// `at_home` and `default_quality` with this device's addresses read
/// here, on Apple and Linux; Android passes its own.
#[cfg(any(target_vendor = "apple", target_os = "linux"))]
impl Session {
    /// [`Session::at_home`] by the addresses this device's interfaces
    /// have now. They are read only for a direct IPv6 path, the one that
    /// needs them.
    pub fn at_home_here(&self) -> bool {
        let path = self.connection_path();
        let addrs = match &path {
            ConnectionPath::Direct { addr } if addr.is_ipv6() => global_v6_here(),
            _ => Vec::new(),
        };
        let heard = self.client.inner.discovery.get(&self.server).is_some();
        path.at_home(&addrs, heard)
    }

    /// [`Session::default_quality`] by this device's addresses now: read
    /// only when the answer turns on them, with no `limit` and not on a
    /// TV box.
    pub fn default_quality_here(&self, limit: Option<u64>) -> PlaybackQuality {
        let platform = self.client.inner.device.platform;
        let decided = limit.is_some_and(|l| l > 0) || platform == Platform::Tvos;
        let at_home = !decided && self.at_home_here();
        PlaybackQuality::default_for(limit, platform, at_home)
    }
}

/// This device's global IPv6 addresses, as its interfaces have them now
/// (getifaddrs): the only ones [`at_home`] reads.
#[cfg(any(target_vendor = "apple", target_os = "linux"))]
fn global_v6_here() -> Vec<IpAddr> {
    netdev::get_interfaces()
        .into_iter()
        .flat_map(|i| i.ipv6)
        .map(|n| n.addr())
        .filter(|ip| global(*ip))
        .map(IpAddr::V6)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn home() -> Vec<IpAddr> {
        vec![
            "192.168.1.20".parse().unwrap(),
            "fe80::1".parse().unwrap(),
            "2001:db8:1:2::1".parse().unwrap(),
        ]
    }

    fn direct(addr: &str) -> ConnectionPath {
        ConnectionPath::Direct {
            addr: addr.parse::<SocketAddr>().unwrap(),
        }
    }

    /// Only this device's global IPv6 addresses are read: no IPv4, no
    /// link-local or loopback one, though every machine has some.
    #[cfg(any(target_vendor = "apple", target_os = "linux"))]
    #[test]
    fn only_global_ipv6_addresses_are_read_here() {
        assert!(
            global_v6_here()
                .iter()
                .all(|ip| matches!(ip, IpAddr::V6(v6) if global(*v6)))
        );
    }

    #[test]
    fn a_direct_path_into_this_devices_own_64_is_at_home() {
        assert!(direct("[2001:db8:1:2::99]:7430").at_home(&home(), false));
        assert!(!direct("[2001:db8:1:3::99]:7430").at_home(&home(), false));
        assert!(!direct("203.0.113.5:7430").at_home(&home(), true));
    }

    #[test]
    fn a_link_local_or_private_address_makes_no_home_64() {
        let local: Vec<IpAddr> = vec!["fe80::1".parse().unwrap(), "fd00::1".parse().unwrap()];
        assert!(!direct("[fe80::99]:7430").at_home(&local, false));
        assert!(!direct("[fd00::99]:7430").at_home(&local, false));
    }

    #[test]
    fn the_pure_check_needs_a_path() {
        assert!(at_home(&direct("[2001:db8:1:2::99]:7430"), &home()));
        assert!(!at_home(&direct("[2001:db8:1:3::99]:7430"), &home()));
        assert!(!at_home(&ConnectionPath::Unknown, &home()));
        assert!(!at_home(&ConnectionPath::Relay, &home()));
    }

    #[test]
    fn the_lan_is_home_a_relay_is_not_and_no_path_goes_by_what_was_heard() {
        let lan = ConnectionPath::Lan {
            addr: "192.168.1.5:7430".parse().unwrap(),
        };
        assert!(lan.at_home(&[], false));
        assert!(!ConnectionPath::Relay.at_home(&home(), true));
        assert!(ConnectionPath::Unknown.at_home(&home(), true));
        assert!(!ConnectionPath::Unknown.at_home(&home(), false));
    }

    #[test]
    fn the_default_quality_is_the_limit_else_original_at_home_else_auto() {
        assert_eq!(
            PlaybackQuality::default_for(Some(4_000_000), Platform::Ios, true),
            PlaybackQuality::Bitrate(4_000_000)
        );
        assert_eq!(
            PlaybackQuality::default_for(Some(0), Platform::Ios, true),
            PlaybackQuality::Original
        );
        assert_eq!(
            PlaybackQuality::default_for(None, Platform::Ios, false),
            PlaybackQuality::Auto
        );
    }

    #[test]
    fn a_tv_box_starts_at_original_away_from_home_too() {
        assert_eq!(
            PlaybackQuality::default_for(None, Platform::Tvos, false),
            PlaybackQuality::Original
        );
        assert_eq!(
            PlaybackQuality::default_for(Some(8_000_000), Platform::Tvos, false),
            PlaybackQuality::Bitrate(8_000_000)
        );
    }
}
