//! Which relays the server asks what address it has (QUIC address
//! discovery): not one on the server's own side of the NAT.
//!
//! iroh asks each relay, every report, where it sees the endpoint from,
//! and learns its public address from the answers. A relay on the
//! server's LAN, or reached through the router's hairpin (its name
//! resolves to the server's own public address), sees the server from
//! inside the NAT: a private address, never the public one. Its answer is
//! wrong, and beside a true one (n0's) it makes iroh warn on every report
//! that the NAT maps by destination when it does not. Such a relay still
//! carries connections over its HTTPS side; it is only not asked.
//!
//! Whether a relay is on this side is told by where its name points: a
//! private, loopback, link-local or CGNAT address, one of this host's own,
//! or the server's public address. The last is known only once something
//! has told the server its public address ([`crate::wan`], or n0's
//! answers), so the relays are looked at again whenever the server's
//! addresses change, and every [`RECHECK`] for a name that moves.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use iroh::{Endpoint, RelayConfig, RelayMap, RelayMode, RelayUrl, Watcher as _};

use crate::network::Relay;
use crate::wan::not_public;

/// How long a relay's name is given to resolve.
const RESOLVE_WAIT: Duration = Duration::from_secs(5);

/// As [`RESOLVE_WAIT`], before the endpoint binds: the endpoint waits on
/// it, and [`watch`] places the relays again once it is up, so a boot
/// without DNS is held back a second, not five a relay (review
/// 2026-10-11 L47).
const INITIAL_RESOLVE_WAIT: Duration = Duration::from_secs(1);

/// How often the relays' names are looked up again with nothing else
/// changed: a name moved to another address.
const RECHECK: Duration = Duration::from_secs(10 * 60);

/// Where a relay is, as seen from the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Side {
    /// On the server's side of the NAT, and why.
    Ours(String),
    /// Outside it.
    Theirs,
    /// At a public address, while the server's own public address is not
    /// known yet: it may be the server's (a hairpin) or not.
    Unknown,
}

/// Where a relay whose name resolves to `relay` is, for a server whose own
/// addresses are `own` and whose known public addresses are `public`. A
/// name that did not resolve is taken as outside: asking it is what iroh
/// does anyway, and a wrong answer only costs a warning.
pub fn side(relay: &[IpAddr], own: &[IpAddr], public: &[IpAddr]) -> Side {
    if relay.is_empty() {
        return Side::Theirs;
    }
    for ip in relay {
        let ip = ip.to_canonical();
        if public.iter().any(|p| p.to_canonical() == ip) {
            return Side::Ours(format!("it resolves to {ip}, this server's public address"));
        }
        if own.iter().any(|o| o.to_canonical() == ip) {
            return Side::Ours(format!("it resolves to {ip}, an address of this host"));
        }
        if let Some(range) = not_public(ip) {
            return Side::Ours(format!("it resolves to {ip}, which is {range}"));
        }
    }
    if public.is_empty() {
        Side::Unknown
    } else {
        Side::Theirs
    }
}

/// Whether a relay at `side` is asked. One not placed yet is left
/// unasked while n0's relays are there to answer (`with_n0`), so a hairpin
/// does not warn before the server knows its address; alone, it is asked.
fn asked(side: &Side, with_n0: bool) -> bool {
    match side {
        Side::Ours(_) => false,
        Side::Theirs => true,
        Side::Unknown => !with_n0,
    }
}

/// The relays `relays` names that are not n0's, each with its URL: the
/// ones whose side is in question. n0's are always asked.
fn custom(relays: &[Relay]) -> Vec<RelayUrl> {
    let n0 = RelayMode::Default.relay_map();
    relays
        .iter()
        .filter_map(|relay| match relay {
            Relay::Custom(url) => Some(RelayUrl::from(url.clone())),
            Relay::Default => None,
        })
        .filter(|url| !n0.contains(url))
        .collect()
}

/// Where `url`'s host points: an IP written in it, or its name's
/// addresses. Empty when it does not resolve in [`RESOLVE_WAIT`].
async fn resolve(url: &RelayUrl) -> Vec<IpAddr> {
    resolve_within(url, RESOLVE_WAIT).await
}

async fn resolve_within(url: &RelayUrl, wait: Duration) -> Vec<IpAddr> {
    let Some(host) = url.host_str() else {
        return Vec::new();
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = host.parse::<IpAddr>() {
        return vec![ip];
    }
    let port = url.port_or_known_default().unwrap_or(443);
    match tokio::time::timeout(wait, tokio::net::lookup_host((host, port))).await {
        Ok(Ok(addrs)) => addrs.map(|a| a.ip()).collect(),
        Ok(Err(e)) => {
            tracing::debug!(relay = %url, error = %e, "cannot resolve the relay's name");
            Vec::new()
        }
        Err(_) => {
            tracing::debug!(relay = %url, "the relay's name did not resolve in time");
            Vec::new()
        }
    }
}

/// `url` as iroh dials it: asked for the address, or not.
fn config(url: RelayUrl, asked: bool) -> RelayConfig {
    if asked {
        RelayConfig::from(url)
    } else {
        RelayConfig::new(url, None)
    }
}

/// Say where a relay was placed, once per change.
fn log(url: &RelayUrl, side: &Side) {
    match side {
        Side::Ours(why) => tracing::info!(
            "relay {url} is on this server's side of the NAT ({why}): not asked for the \
             server's address"
        ),
        Side::Theirs => {
            tracing::info!("relay {url} is outside this server's NAT: asked for its address")
        }
        Side::Unknown => tracing::debug!(
            relay = %url,
            "not yet known whether the relay is on this server's side of the NAT"
        ),
    }
}

/// The relays to start with: each of its own placed by its name alone,
/// beside the public addresses already known (`public`: `network.wan`'s
/// static ones), the rest as n0 serves them. The endpoint's addresses are
/// not known before it binds; [`watch`] places the relays again once they
/// are.
pub async fn initial(relays: &[Relay], public: &[IpAddr]) -> (RelayMap, HashMap<RelayUrl, Side>) {
    let n0 = RelayMode::Default.relay_map();
    let with_n0 = relays.contains(&Relay::Default);
    let own = custom(relays);
    let mut sides = HashMap::new();
    // All at once, each for a second at most.
    let resolved = futures_util::future::join_all(
        own.iter()
            .map(|url| resolve_within(url, INITIAL_RESOLVE_WAIT)),
    )
    .await;
    for (url, addrs) in own.iter().zip(resolved) {
        let side = side(&addrs, &[], public);
        log(url, &side);
        sides.insert(url.clone(), side);
    }
    let map = crate::network::relay_urls(relays)
        .into_iter()
        .map(|url| match n0.get(&url) {
            Some(config) => (*config).clone(),
            None => {
                let asked = sides.get(&url).is_none_or(|side| asked(side, with_n0));
                config(url, asked)
            }
        })
        .collect();
    (map, sides)
}

/// Place the relays of its own again whenever the endpoint's addresses or
/// the public ones change, and every [`RECHECK`], changing how iroh dials
/// one when its side changed. Ends with the endpoint.
pub fn watch(
    endpoint: Endpoint,
    relays: &[Relay],
    public: crate::wan::Public,
    mut sides: HashMap<RelayUrl, Side>,
) {
    let own = custom(relays);
    if own.is_empty() {
        return;
    }
    let with_n0 = relays.contains(&Relay::Default);
    tokio::spawn(async move {
        let mut addrs = endpoint.watch_addr();
        let mut changes = public.changes();
        let closed = endpoint.closed();
        tokio::pin!(closed);
        let mut recheck = tokio::time::interval(RECHECK);
        recheck.tick().await;
        loop {
            tokio::select! {
                () = &mut closed => return,
                updated = addrs.updated() => if updated.is_err() { return },
                Ok(()) = changes.changed() => {}
                _ = recheck.tick() => {}
            }
            let mine: Vec<IpAddr> = endpoint.addr().ip_addrs().map(|a| a.ip()).collect();
            let mut known: Vec<IpAddr> = public.addrs().iter().map(|p| p.addr.ip()).collect();
            known.extend(mine.iter().copied().filter(|ip| not_public(*ip).is_none()));
            for url in &own {
                let now = side(&resolve(url).await, &mine, &known);
                if sides.get(url) == Some(&now) {
                    continue;
                }
                let was = sides.get(url).is_none_or(|side| asked(side, with_n0));
                log(url, &now);
                let is = asked(&now, with_n0);
                sides.insert(url.clone(), now);
                if was != is {
                    endpoint
                        .insert_relay(url.clone(), Arc::new(config(url.clone(), is)))
                        .await;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ips(list: &[&str]) -> Vec<IpAddr> {
        list.iter().map(|s| s.parse().unwrap()).collect()
    }

    /// The hairpin: the relay's name is the server's public address.
    #[test]
    fn a_relay_at_the_server_s_public_address_is_ours() {
        let side = side(&ips(&["76.50.95.40"]), &[], &ips(&["76.50.95.40"]));
        assert!(matches!(side, Side::Ours(why) if why.contains("public address")));
    }

    /// Split DNS, or a relay named by its LAN address.
    #[test]
    fn a_relay_at_a_private_address_is_ours() {
        for ip in [
            "192.168.86.142",
            "10.10.0.2",
            "100.64.1.1",
            "127.0.0.1",
            "fe80::1",
            "fd00::1",
        ] {
            assert!(
                matches!(
                    side(&ips(&[ip]), &[], &ips(&["76.50.95.40"])),
                    Side::Ours(_)
                ),
                "{ip}"
            );
        }
    }

    /// A relay on this very host, at a public address of its own.
    #[test]
    fn a_relay_at_one_of_this_host_s_addresses_is_ours() {
        let side = side(&ips(&["203.0.113.9"]), &ips(&["203.0.113.9"]), &[]);
        assert!(matches!(side, Side::Ours(why) if why.contains("this host")));
    }

    /// A relay on a VPS: a public address that is not the server's.
    #[test]
    fn a_relay_at_another_public_address_is_theirs() {
        assert_eq!(
            side(&ips(&["198.51.100.4"]), &[], &ips(&["76.50.95.40"])),
            Side::Theirs
        );
    }

    /// Before the server knows its public address a public relay may be a
    /// hairpin or not.
    #[test]
    fn a_public_relay_is_unknown_until_the_server_knows_its_address() {
        assert_eq!(side(&ips(&["76.50.95.40"]), &[], &[]), Side::Unknown);
    }

    /// A name that does not resolve is asked, as iroh would.
    #[test]
    fn an_unresolved_relay_is_asked() {
        let side = side(&[], &[], &ips(&["76.50.95.40"]));
        assert_eq!(side, Side::Theirs);
        assert!(asked(&side, true));
    }

    /// Unknown: left unasked beside n0, which answers meanwhile; asked
    /// alone. Ours is never asked, theirs always.
    #[test]
    fn who_is_asked() {
        assert!(!asked(&Side::Unknown, true));
        assert!(asked(&Side::Unknown, false));
        assert!(!asked(&Side::Ours(String::new()), false));
        assert!(asked(&Side::Theirs, true));
    }

    /// The owner's relay beside n0, named by an IP on the LAN: not asked,
    /// and n0's relays still are.
    #[tokio::test]
    async fn an_own_relay_on_the_lan_beside_n0_is_not_asked() {
        let own = Relay::Custom("https://192.168.86.142".parse().unwrap());
        let (map, sides) = initial(&[Relay::Default, own], &[]).await;
        let url: RelayUrl = "https://192.168.86.142".parse().unwrap();
        assert!(map.get(&url).unwrap().quic.is_none());
        assert!(matches!(sides[&url], Side::Ours(_)));
        for n0 in RelayMode::Default.relay_map().urls::<Vec<_>>() {
            assert!(map.get(&n0).unwrap().quic.is_some(), "{n0}");
        }
    }

    /// A relay at a public address, alone, before the server knows its
    /// own: asked, since nothing else would answer.
    #[tokio::test]
    async fn a_public_relay_alone_is_asked_until_placed() {
        let relay = Relay::Custom("https://198.51.100.4".parse().unwrap());
        let (map, _) = initial(&[relay], &[]).await;
        let url: RelayUrl = "https://198.51.100.4".parse().unwrap();
        assert!(map.get(&url).unwrap().quic.is_some());
    }

    /// The hairpin with the public address known at start (a static
    /// `network.wan`): not asked from the first report.
    #[tokio::test]
    async fn a_hairpin_relay_with_a_static_public_address_is_not_asked() {
        let relay = Relay::Custom("https://76.50.95.40".parse().unwrap());
        let (map, _) = initial(&[Relay::Default, relay], &ips(&["76.50.95.40"])).await;
        let url: RelayUrl = "https://76.50.95.40".parse().unwrap();
        assert!(map.get(&url).unwrap().quic.is_none());
    }
}
