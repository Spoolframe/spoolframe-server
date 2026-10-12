//! This machine's interface addresses, read once through `getifaddrs`
//! for everything that asks: the addresses told to devices
//! ([`crate::announce`]), the on-link networks a device sign-in is
//! judged by ([`crate::device_sign_in`]), and the subnets the uplink
//! budget calls the LAN ([`crate::uplink`]). One walker, so the unsafe
//! reading of the C list is in one place (Review 2026-10-11 N8.1).

use std::net::IpAddr;

/// One address of an interface that is up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IfAddr {
    /// The interface's name; empty if the kernel gave none.
    pub name: String,
    pub ip: IpAddr,
    /// Its netmask's prefix length, if the entry has a netmask.
    pub prefix: Option<u8>,
}

/// Every IPv4 and IPv6 address of the interfaces that are up now, in
/// the kernel's order. Empty if `getifaddrs` fails.
pub fn up() -> Vec<IfAddr> {
    let mut found = Vec::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `head` with a list it allocates, which is
    // read only until the freeifaddrs below. Each entry's pointers are
    // checked for null before they are read; its name is a C string, and
    // its address and netmask are read as the struct the address's family
    // names (sockaddr_in for AF_INET, sockaddr_in6 for AF_INET6), which is
    // what getifaddrs stores for that family.
    unsafe {
        if libc::getifaddrs(&mut head) != 0 {
            return found;
        }
        let mut entry = head;
        while let Some(ifa) = entry.as_ref() {
            entry = ifa.ifa_next;
            if ifa.ifa_addr.is_null() || ifa.ifa_flags & libc::IFF_UP as u32 == 0 {
                continue;
            }
            let mask = ifa.ifa_netmask;
            let (ip, prefix) = match i32::from((*ifa.ifa_addr).sa_family) {
                libc::AF_INET => {
                    let sa = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                    let prefix = (!mask.is_null()).then(|| {
                        let m = &*(mask as *const libc::sockaddr_in);
                        u32::from_be(m.sin_addr.s_addr).count_ones() as u8
                    });
                    (IpAddr::from(sa.sin_addr.s_addr.to_ne_bytes()), prefix)
                }
                libc::AF_INET6 => {
                    let sa = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                    let prefix = (!mask.is_null()).then(|| {
                        let m = &*(mask as *const libc::sockaddr_in6);
                        u128::from_be_bytes(m.sin6_addr.s6_addr).count_ones() as u8
                    });
                    (IpAddr::from(sa.sin6_addr.s6_addr), prefix)
                }
                _ => continue,
            };
            let name = if ifa.ifa_name.is_null() {
                String::new()
            } else {
                std::ffi::CStr::from_ptr(ifa.ifa_name)
                    .to_string_lossy()
                    .into_owned()
            };
            found.push(IfAddr { name, ip, prefix });
        }
        libc::freeifaddrs(head);
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The walker reads this machine's loopback, up, with its /8 or /128:
    /// a real call through the C list, so a misread struct shows.
    #[test]
    fn loopback_is_read_with_its_prefix() {
        let found = up();
        assert!(
            found
                .iter()
                .any(|a| a.ip.is_loopback() && matches!(a.prefix, Some(8 | 128))),
            "{found:?}"
        );
    }
}
