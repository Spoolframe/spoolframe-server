//! `--check-wan <addr>` (P6-02): whether the server can be reached through
//! one public address, as a device off its network would reach it.
//!
//! A server cannot check this itself: its own packets to its public
//! address turn around in the router (NAT hairpin), or never leave it, so
//! the answer would say nothing of the forward. Run from another network.
//!
//! The probe dials only the address given: no relay, no address lookup,
//! no LAN discovery. Iroh still proves the server's key, so an answer is
//! from the server named, through that address.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use iroh::{EndpointAddr, EndpointId};
use serde::Serialize;

use crate::ServerSummary;
use crate::wire::{Probe, Problem};

/// What the check found.
#[derive(Debug, Clone, Serialize)]
pub struct WanCheck {
    pub endpoint: String,
    pub addr: String,
    /// The server answered a good `Hello` through `addr`.
    pub reachable: bool,
    pub server: Option<ServerSummary>,
    /// Why it was not reached.
    pub error: Option<String>,
    pub elapsed_ms: u64,
}

/// Dial the server `endpoint` at `addr` only, and say `Hello`: within
/// `wait` for the handshake and again for the answer.
pub async fn check_wan(
    endpoint: EndpointId,
    addr: SocketAddr,
    wait: Duration,
) -> anyhow::Result<WanCheck> {
    let target = EndpointAddr::new(endpoint).with_ip_addr(addr);
    // Two: the connection and its Hello stream.
    let probe = Probe::new(target, 2, wait).await?;
    let began = Instant::now();
    let outcome = probe.connect_hello().await;
    let elapsed_ms = began.elapsed().as_millis() as u64;
    let check = match outcome {
        Ok((_conn, info)) => WanCheck {
            endpoint: endpoint.to_string(),
            addr: addr.to_string(),
            reachable: true,
            server: Some(ServerSummary {
                name: info.name,
                server_version: info.server_version,
                protocol_version: info.protocol_version,
            }),
            error: None,
            elapsed_ms,
        },
        Err(problem) => WanCheck {
            endpoint: endpoint.to_string(),
            addr: addr.to_string(),
            reachable: false,
            server: None,
            error: Some(match problem {
                Problem::Failed(why) => why,
                Problem::Stop(stop) => stop.to_string(),
            }),
            elapsed_ms,
        },
    };
    probe.close().await;
    Ok(check)
}

/// The check as a person reads it.
pub fn wan_summary(check: &WanCheck) -> String {
    match (&check.server, &check.error) {
        (Some(server), _) => format!(
            "check-wan: PASS  reached {:?} (version {}, protocol {}) at {} in {} ms\n",
            server.name,
            server.server_version,
            server.protocol_version,
            check.addr,
            check.elapsed_ms
        ),
        (None, error) => format!(
            "check-wan: FAIL  {} at {}: {}\n  Is the port forwarded to the server (UDP), and open in \
             its firewall? Run this from another network: from the server's own, the router may not \
             turn its packets around.\n",
            check.endpoint,
            check.addr,
            error.as_deref().unwrap_or("no answer")
        ),
    }
}
