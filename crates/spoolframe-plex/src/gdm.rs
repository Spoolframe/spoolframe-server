//! Finding Plex Media Servers on the LAN with Plex's own discovery, GDM
//! (P3-10): a search sent to Plex's multicast group and the LAN's
//! broadcast address on UDP 32414, which each PMS answers with its name,
//! port and version. Nothing leaves the LAN.

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
}

/// Ask `targets` and gather the servers that answer within `wait`, each
/// once, ordered by name.
pub async fn discover(targets: &[SocketAddr], wait: Duration) -> std::io::Result<Vec<Found>> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
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
            }
        );
        assert_eq!(parse("HTTP/1.0 404 Not Found\r\n\r\n", found_ip()), None);
        let player = REPLY.replace("plex/media-server", "plex/media-player");
        assert_eq!(parse(&player, found_ip()), None);
    }

    fn found_ip() -> IpAddr {
        "192.168.1.10".parse().unwrap()
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
        let found = discover(&[at, at], Duration::from_millis(500))
            .await
            .unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].name, "Tower");
        assert_eq!(found[0].url, "http://127.0.0.1:32400");
    }
}
