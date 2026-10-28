//! "Find a server on my network": the servers heard on the LAN, and what
//! each says of itself before anyone signs in.
//!
//! The mDNS record carries the name alone, so the rest of a row comes from the server's `Hello` answer: one connect per row, with
//! only `Hello` sent, as [`Client::connect`] does. Nothing is saved.

use spoolframe_protocol::fingerprint;

use crate::client::{Client, parse_id};
use crate::{SdkError, ServerEntry};

/// A server as a "servers on my network" row shows it, from its `Hello`
/// answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanServerInfo {
    /// The server's key, hex: what [`Client::connect`] and
    /// [`Client::pin`] take.
    pub endpoint_id: String,
    /// The name it answers with, which may be newer than the one heard.
    pub name: String,
    /// The key's fingerprint, `K7QX-3MPA-9TRD-WF2H`: the code the
    /// apps show beside a server ([`ServerEntry::fingerprint`]).
    pub fingerprint: String,
    /// The server's release, as it says it.
    pub server_version: String,
    /// The protocol revision it speaks.
    pub protocol_version: u32,
    /// An account there is made only from an invite: "Invite only".
    pub invite_only: bool,
}

impl Client {
    /// The servers heard on this network just now, saved or not: the
    /// entries of [`Client::servers`] that are `on_network`, in its order.
    /// Empty when the client does not listen on the LAN
    /// ([`crate::ClientOptions::lan_discovery`]). [`Client::watch_servers`]
    /// says when to list again.
    pub async fn lan_servers(&self) -> Result<Vec<ServerEntry>, SdkError> {
        let mut servers = self.servers().await?;
        servers.retain(|s| s.on_network);
        Ok(servers)
    }

    /// Connect to server `endpoint_id`, or use the open connection, and
    /// say what its `Hello` answered: for a row of
    /// [`Client::lan_servers`]. Like [`Client::connect`], a server not
    /// saved must have been heard on the LAN or given an address, only
    /// `Hello` is sent to it, and nothing is saved.
    pub async fn lan_server_info(&self, endpoint_id: &str) -> Result<LanServerInfo, SdkError> {
        let id = parse_id(endpoint_id)?;
        let conn = self.link(id).connection(self).await?;
        let info = &conn.info;
        Ok(LanServerInfo {
            endpoint_id: id.to_string(),
            name: info.name.clone(),
            fingerprint: fingerprint::fingerprint(id.as_bytes()),
            server_version: info.server_version.clone(),
            protocol_version: info.protocol_version,
            invite_only: info.invite_only,
        })
    }
}
