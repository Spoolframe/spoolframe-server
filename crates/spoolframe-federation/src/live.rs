//! The connections open on the ALPN now, by the endpoint they come from:
//! what unpairing and a rejected key close.

use std::collections::HashMap;
use std::sync::Arc;

use iroh::EndpointId;
use iroh::endpoint::{Connection, VarInt};

/// The open `spoolframe/federation/1` connections, shared by the handler
/// and the server that closes them.
#[derive(Debug, Clone, Default)]
pub struct Live {
    open: Arc<parking_lot::Mutex<HashMap<EndpointId, Vec<Connection>>>>,
}

impl Live {
    pub(crate) fn opened(&self, connection: &Connection) {
        self.open
            .lock()
            .entry(connection.remote_id())
            .or_default()
            .push(connection.clone());
    }

    pub(crate) fn closed(&self, connection: &Connection) {
        let mut open = self.open.lock();
        let endpoint = connection.remote_id();
        if let Some(connections) = open.get_mut(&endpoint) {
            connections.retain(|c| c.stable_id() != connection.stable_id());
            if connections.is_empty() {
                open.remove(&endpoint);
            }
        }
    }

    /// Whether `endpoint` has a connection open.
    pub fn has(&self, endpoint: &EndpointId) -> bool {
        self.open.lock().contains_key(endpoint)
    }

    /// Close every connection from `endpoint` with `code` and `reason`: how
    /// many there were.
    pub fn close(&self, endpoint: &EndpointId, code: u32, reason: &str) -> usize {
        let connections = self.open.lock().remove(endpoint).unwrap_or_default();
        for connection in &connections {
            connection.close(VarInt::from_u32(code), reason.as_bytes());
        }
        connections.len()
    }
}
