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
use std::time::Duration;

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
    for target in targets {
        // One unreachable target (no broadcast route, say) leaves the rest.
        if let Err(e) = socket.send_to(SEARCH, target).await {
            tracing::debug!(%target, error = %e, "GDM search not sent");
        }
    }
    let mut found = BTreeMap::new();
    let mut buf = vec![0u8; 4096];
    let deadline = tokio::time::Instant::now() + wait;
    while let Ok(Ok((len, from))) =
        tokio::time::timeout_at(deadline, socket.recv_from(&mut buf)).await
    {
        if let Some(server) = parse(&String::from_utf8_lossy(&buf[..len]), from.ip()) {
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
