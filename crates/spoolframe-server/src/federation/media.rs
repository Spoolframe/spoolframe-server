//! The side a library is shared with (SB) on the media path (P8-04): where
//! a paired server is, for the device to dial, and a token for the device
//! that reached it.
//!
//! The device decides direct or proxy, not this server: what SB's route to
//! SA says nothing of the device's. So SB tells where SA is, from its own
//! record of the pair, and whether SB holds a connection to it now; the
//! device dials SA with a short budget, and asks for a token only once it
//! reached SA. With SA offline to SB, no assertion can reach SA either:
//! the grant is refused at once.

use spoolframe_protocol::federation::RemoteHints;
use spoolframe_types::MediaId;

use super::{AssertError, Federation};
use crate::store::federation::PeerRecord;

impl Federation {
    /// The live pair `item` (a `fed:` id) is from, and the item by that
    /// server's own id.
    pub async fn remote_peer(&self, item: &MediaId) -> Result<(PeerRecord, MediaId), AssertError> {
        let (origin, remote) = item
            .remote()
            .ok_or_else(|| AssertError::NotFederated(item.clone()))?;
        let peer = self
            .live_peer_by_server_id(origin)
            .await?
            .ok_or_else(|| AssertError::NotPaired(origin.to_owned()))?;
        Ok((peer, remote))
    }

    /// Where `peer`, a live pair, may be dialled.
    pub fn remote_hints(&self, peer: &PeerRecord) -> RemoteHints {
        RemoteHints {
            server_id: peer.server_id.clone(),
            endpoint_id: peer.endpoint_id.clone(),
            addrs: peer.addrs.clone(),
            // A device dials these: none on a host that is not public
            // (M8-07), a pair's kept before that check among them.
            relays: peer
                .relays
                .iter()
                .filter(|r| self.relay_hosts.allow(r))
                .cloned()
                .collect(),
            online: self.online(&peer.server_id),
        }
    }
}
