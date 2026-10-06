//! Finding Plex Media Servers on the LAN with Plex's own discovery, GDM
//! (P3-10): a search sent to Plex's multicast group and the LAN's
//! broadcast address on UDP 32414, which each PMS answers with its name,
//! port and version. Nothing leaves the LAN.
//!
//! A PMS answers unicast, to the port the search came from. A host
//! firewall cannot match that answer to a search it saw go to a multicast
//! group or a broadcast address, so it drops it unless the port is open:
//! the search is sent from a fixed port ([`PORT`] by default) that the
//! firewall can be told about.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::fd::{AsRawFd as _, RawFd};
use std::time::Duration;

use tokio::io::Interest;
use tokio::net::UdpSocket;

/// Where a GDM search goes: Plex's multicast group, and the LAN's
/// broadcast address for servers that listen only there.
pub const TARGETS: [SocketAddr; 2] = [
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(239, 0, 0, 250)), 32414),
    SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), 32414),
];

/// The UDP port searches are sent from, and answered on, by default. Not
/// one of Plex's own GDM ports (32410, 32412, 32413, 32414), which a PMS
/// on the same host holds, nor 32411 between them; 32415 is the first
/// above them, and nothing of Plex's uses it.
pub const PORT: u16 = 32415;

/// One search at a time: two bound to the same port would each get only
/// some of the answers, so a second waits for the first (a second and a
/// half at most) rather than falling back to a port a firewall drops.
static SEARCHING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// How long to listen for answers by default.
pub const WAIT: Duration = Duration::from_millis(1500);

const SEARCH: &[u8] = b"M-SEARCH * HTTP/1.1\r\n\r\n";

/// A PMS that answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub name: String,
    /// `http://<its address>:<its port>`.
    pub url: String,
    pub version: Option<String>,
    /// Plex's id for it (`Resource-Identifier`, its machine identifier):
    /// what plex.tv lists the same server under.
    pub machine_id: Option<String>,
}

/// Ask `targets` from UDP `port` (0: any) and gather the servers that
/// answer within `wait`, each once, ordered by name. A port something else
/// holds is given up for any port, with a warning: answers to that one
/// are what a firewall drops.
pub async fn discover(
    targets: &[SocketAddr],
    port: u16,
    wait: Duration,
) -> std::io::Result<Vec<Found>> {
    search(targets, port, wait, host_links).await
}

/// One of this host's IPv4 networks: the index of the interface it is
/// on, and an address and mask there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Link {
    pub index: u32,
    pub addr: Ipv4Addr,
    pub mask: Ipv4Addr,
}

impl Link {
    fn holds(&self, ip: Ipv4Addr) -> bool {
        let mask = u32::from(self.mask);
        u32::from(self.addr) & mask == u32::from(ip) & mask
    }
}

/// Whether `from` is on a network of interface `index`'s: a PMS answering
/// a LAN search is on the link it answers over (PX-11 finding 36). An
/// answer routed in from anywhere else is not one. A PMS on this host
/// answers from one of its addresses over loopback, which is kept.
pub fn on_link(links: &[Link], index: u32, from: IpAddr) -> bool {
    let IpAddr::V4(from) = from.to_canonical() else {
        return false;
    };
    links
        .iter()
        .any(|l| (l.index == index && l.holds(from)) || l.addr == from)
}

/// This host's IPv4 networks, of the interfaces that are up.
pub fn host_links() -> Vec<Link> {
    let mut found = Vec::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `head` with a list it allocates, read only
    // until the freeifaddrs below. An AF_INET entry's address and mask
    // are sockaddr_in; its name is a C string.
    unsafe {
        if libc::getifaddrs(&mut head) != 0 {
            return found;
        }
        let mut entry = head;
        while let Some(ifa) = entry.as_ref() {
            entry = ifa.ifa_next;
            if ifa.ifa_addr.is_null()
                || ifa.ifa_netmask.is_null()
                || ifa.ifa_flags & libc::IFF_UP as u32 == 0
                || i32::from((*ifa.ifa_addr).sa_family) != libc::AF_INET
            {
                continue;
            }
            let index = libc::if_nametoindex(ifa.ifa_name);
            if index == 0 {
                continue;
            }
            let v4 = |sa: *const libc::sockaddr| {
                let sin = std::ptr::read_unaligned(sa.cast::<libc::sockaddr_in>());
                Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr))
            };
            found.push(Link {
                index,
                addr: v4(ifa.ifa_addr),
                mask: v4(ifa.ifa_netmask),
            });
        }
        libc::freeifaddrs(head);
    }
    found
}

/// Have `socket` say which interface each datagram came in on.
fn ask_for_interfaces(socket: &UdpSocket) -> std::io::Result<()> {
    let on: libc::c_int = 1;
    // SAFETY: setsockopt on a socket this owns, with an int it reads.
    let done = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_PKTINFO,
            (&raw const on).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if done != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// One datagram into `buf`, without waiting: its length, its sender, and
/// the index of the interface it came in on (`IP_PKTINFO`).
fn receive(fd: RawFd, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr, Option<u32>)> {
    // u64s, for the alignment cmsghdr needs.
    let mut control = [0u64; 16];
    // SAFETY: recvmsg writes at most the lengths given into `buf`, `from`
    // and `control`, which outlive the call; the control messages are read
    // only within the length it returned, through the CMSG macros.
    unsafe {
        let mut from: libc::sockaddr_in = std::mem::zeroed();
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr().cast(),
            iov_len: buf.len(),
        };
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_name = (&raw mut from).cast();
        msg.msg_namelen = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
        msg.msg_iov = &raw mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = std::mem::size_of_val(&control) as _;
        let len = libc::recvmsg(fd, &raw mut msg, libc::MSG_DONTWAIT);
        if len < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut index = None;
        let mut cmsg = libc::CMSG_FIRSTHDR(&raw const msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::IPPROTO_IP && (*cmsg).cmsg_type == libc::IP_PKTINFO {
                let info: libc::in_pktinfo = std::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast());
                index = u32::try_from(info.ipi_ifindex).ok();
            }
            cmsg = libc::CMSG_NXTHDR(&raw const msg, cmsg);
        }
        let ip = Ipv4Addr::from(u32::from_be(from.sin_addr.s_addr));
        let port = u16::from_be(from.sin_port);
        Ok((len as usize, SocketAddr::new(IpAddr::V4(ip), port), index))
    }
}

/// [`discover`], with the host's networks from `links`: read once a
/// search, after the answers are in.
async fn search(
    targets: &[SocketAddr],
    port: u16,
    wait: Duration,
    links: impl Fn() -> Vec<Link>,
) -> std::io::Result<Vec<Found>> {
    let _turn = SEARCHING.lock().await;
    let socket = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).await {
        Ok(socket) => socket,
        Err(e) if port != 0 => {
            tracing::warn!(
                port,
                error = %e,
                "cannot search for Plex from this UDP port; searching from any port, \
                 whose answers a firewall may drop"
            );
            UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?
        }
        Err(e) => return Err(e),
    };
    socket.set_broadcast(true)?;
    ask_for_interfaces(&socket)?;
    for target in targets {
        // One unreachable target (no broadcast route, say) leaves the rest.
        if let Err(e) = socket.send_to(SEARCH, target).await {
            tracing::debug!(%target, error = %e, "GDM search not sent");
        }
    }
    let mut answers = Vec::new();
    let mut buf = vec![0u8; 4096];
    let fd = socket.as_raw_fd();
    let deadline = tokio::time::Instant::now() + wait;
    while let Ok(Ok((len, from, index))) = tokio::time::timeout_at(
        deadline,
        socket.async_io(Interest::READABLE, || receive(fd, &mut buf)),
    )
    .await
    {
        answers.push((
            String::from_utf8_lossy(&buf[..len]).into_owned(),
            from,
            index,
        ));
    }
    let links = links();
    let mut found = BTreeMap::new();
    for (reply, from, index) in answers {
        // Only from the network of the interface it came in on.
        if !index.is_some_and(|index| on_link(&links, index, from.ip())) {
            tracing::debug!(%from, ?index, "GDM answer from off its link dropped");
            continue;
        }
        if let Some(server) = parse(&reply, from.ip()) {
            found.insert(server.url.clone(), server);
        }
    }
    let mut found: Vec<Found> = found.into_values().collect();
    found.sort_by(|a, b| a.name.cmp(&b.name).then(a.url.cmp(&b.url)));
    Ok(found)
}

/// A GDM answer from `from`: HTTP-like lines, `Name: Tower`,
/// `Port: 32400`, `Version: 1.41.0`. `None` for anything else.
pub fn parse(reply: &str, from: IpAddr) -> Option<Found> {
    let mut lines = reply.lines();
    if !lines.next()?.contains("200 OK") {
        return None;
    }
    let fields: BTreeMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    if fields
        .get("content-type")
        .is_some_and(|t| t != "plex/media-server")
    {
        return None;
    }
    let port: u16 = fields.get("port")?.parse().ok()?;
    Some(Found {
        name: fields
            .get("name")
            .cloned()
            .unwrap_or_else(|| from.to_string()),
        url: format!("http://{}", SocketAddr::new(from, port)),
        version: fields.get("version").cloned(),
        machine_id: fields.get("resource-identifier").cloned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPLY: &str = "HTTP/1.0 200 OK\r\nContent-Type: plex/media-server\r\n\
        Resource-Identifier: 0123abcd\r\nName: Tower\r\nPort: 32400\r\n\
        Updated-At: 1696000000\r\nVersion: 1.41.0.8992\r\n\r\n";

    #[test]
    fn an_answer_names_the_server_its_address_and_version() {
        let found = parse(REPLY, "192.168.1.10".parse().unwrap()).unwrap();
        assert_eq!(
            found,
            Found {
                name: "Tower".into(),
                url: "http://192.168.1.10:32400".into(),
                version: Some("1.41.0.8992".into()),
                machine_id: Some("0123abcd".into()),
            }
        );
        assert_eq!(parse("HTTP/1.0 404 Not Found\r\n\r\n", found_ip()), None);
        let player = REPLY.replace("plex/media-server", "plex/media-player");
        assert_eq!(parse(&player, found_ip()), None);
    }

    fn found_ip() -> IpAddr {
        "192.168.1.10".parse().unwrap()
    }

    /// A stand-in PMS that answers every search, and says from which
    /// port each one came.
    async fn stand_in() -> (SocketAddr, tokio::sync::mpsc::UnboundedReceiver<u16>) {
        let pms = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let at = pms.local_addr().unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut buf = [0u8; 256];
            loop {
                let (len, from) = pms.recv_from(&mut buf).await.unwrap();
                assert_eq!(&buf[..len], SEARCH);
                let _ = tx.send(from.port());
                pms.send_to(REPLY.as_bytes(), from).await.unwrap();
            }
        });
        (at, rx)
    }

    /// A UDP port nothing is bound to just now.
    async fn free_port() -> u16 {
        let probe = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await.unwrap();
        probe.local_addr().unwrap().port()
    }

    #[tokio::test]
    async fn the_search_is_sent_from_the_port_asked_for_and_answered_there() {
        let (at, mut from) = stand_in().await;
        let port = free_port().await;
        let found = discover(&[at], port, Duration::from_millis(500))
            .await
            .unwrap();
        // The answer came back to that port: a firewall that lets it in
        // lets the server be found.
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(from.recv().await, Some(port));
    }

    #[tokio::test]
    async fn a_port_in_use_falls_back_to_any_port_and_still_searches() {
        let (at, mut from) = stand_in().await;
        let taken = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await.unwrap();
        let port = taken.local_addr().unwrap().port();
        let found = discover(&[at], port, Duration::from_millis(500))
            .await
            .unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        let sent_from = from.recv().await.unwrap();
        assert_ne!(sent_from, port);
        drop(taken);
    }

    #[tokio::test]
    async fn searches_at_once_take_turns_on_the_one_port() {
        let (at, mut from) = stand_in().await;
        let port = free_port().await;
        let wait = Duration::from_millis(300);
        let targets = [at];
        let (a, b) = tokio::join!(
            discover(&targets, port, wait),
            discover(&targets, port, wait)
        );
        // Each got its own answer, and both asked from the port: the
        // second waited for the first rather than falling back.
        assert_eq!(a.unwrap().len(), 1);
        assert_eq!(b.unwrap().len(), 1);
        assert_eq!(from.recv().await, Some(port));
        assert_eq!(from.recv().await, Some(port));
    }

    #[test]
    fn a_sender_is_on_link_only_on_the_interface_whose_network_holds_it() {
        let links = [
            Link {
                index: 2,
                addr: Ipv4Addr::new(192, 168, 1, 5),
                mask: Ipv4Addr::new(255, 255, 255, 0),
            },
            Link {
                index: 3,
                addr: Ipv4Addr::new(10, 8, 0, 1),
                mask: Ipv4Addr::new(255, 255, 0, 0),
            },
        ];
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        assert!(on_link(&links, 2, ip("192.168.1.10")));
        assert!(on_link(&links, 3, ip("10.8.200.7")));
        // On a network of this host's, but not the one it came in on.
        assert!(!on_link(&links, 2, ip("10.8.200.7")));
        assert!(!on_link(&links, 3, ip("192.168.1.10")));
        assert!(!on_link(&links, 2, ip("192.168.2.10")));
        assert!(!on_link(&links, 4, ip("192.168.1.10")));
        assert!(!on_link(&links, 2, ip("::1")));
        // A PMS on this host, from its LAN address over loopback.
        assert!(on_link(&links, 1, ip("192.168.1.5")));
    }

    /// PX-11 finding 36: an answer from a sender that is not on the
    /// network of the interface it came in on is dropped.
    #[tokio::test]
    async fn an_answer_from_off_the_link_it_came_in_on_is_dropped() {
        let (at, _from) = stand_in().await;
        // Loopback, as if its network were 10.0.0.0/8: 127.0.0.1 is off it.
        let elsewhere = || {
            host_links()
                .into_iter()
                .map(|l| Link {
                    addr: Ipv4Addr::new(10, 0, 0, 1),
                    mask: Ipv4Addr::new(255, 0, 0, 0),
                    ..l
                })
                .collect()
        };
        let found = search(&[at], 0, Duration::from_millis(500), elsewhere)
            .await
            .unwrap();
        assert_eq!(found, []);
        // Its own networks, as they are: found.
        let found = search(&[at], 0, Duration::from_millis(500), host_links)
            .await
            .unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
    }

    #[tokio::test]
    async fn a_server_that_answers_the_search_is_found_once() {
        let pms = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let at = pms.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 256];
            loop {
                let (len, from) = pms.recv_from(&mut buf).await.unwrap();
                assert_eq!(&buf[..len], SEARCH);
                pms.send_to(REPLY.as_bytes(), from).await.unwrap();
            }
        });
        // Asked twice, as over multicast and broadcast: listed once.
        let found = discover(&[at, at], 0, Duration::from_millis(500))
            .await
            .unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].name, "Tower");
        assert_eq!(found[0].url, "http://127.0.0.1:32400");
    }
}
