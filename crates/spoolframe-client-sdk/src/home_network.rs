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

impl ConnectionPath {
    /// Whether this path is inside the home network: the LAN, or a direct
    /// IPv6 path into the /64 of one of `local_addrs` (this device's own
    /// addresses; only global IPv6 ones count). With no path yet,
    /// `heard`: the server was heard on this network.
    pub fn at_home(&self, local_addrs: &[IpAddr], heard: bool) -> bool {
        match self {
            ConnectionPath::Lan { .. } => true,
            ConnectionPath::Direct { addr } => match addr.ip() {
                IpAddr::V6(v6) => in_local_64(v6, local_addrs),
                IpAddr::V4(_) => false,
            },
            ConnectionPath::Relay => false,
            ConnectionPath::Unknown => heard,
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
