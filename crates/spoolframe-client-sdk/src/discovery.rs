//! Servers heard on the LAN (P1-13).
//!
//! The mDNS lookup only reports what it hears after a subscription, and
//! skips repeats of what it has already reported, so the SDK subscribes
//! once, when the client opens, and keeps its own map for the life of the
//! client. Records are untrusted hints: where to dial and which key to
//! expect. Iroh proves the key on connect.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use futures_util::StreamExt;
use iroh::EndpointId;
use iroh_mdns_address_lookup::{DiscoveryEvent, MdnsAddressLookup};
use spoolframe_protocol::lan;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// A server's latest record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Heard {
    pub name: String,
    pub addrs: Vec<SocketAddr>,
}

type HeardMap = Arc<Mutex<HashMap<EndpointId, Heard>>>;

pub(crate) struct Discovery {
    heard: HeardMap,
    /// Bumped whenever what `Client::servers` returns may have changed:
    /// a server heard, changed or gone here, or a pin saved or forgotten.
    changes: watch::Sender<u64>,
    /// `None` when the client does not listen on the LAN.
    task: Option<JoinHandle<()>>,
}

impl Discovery {
    /// Start listening on `mdns`, which the endpoint also uses to look up
    /// the addresses of the servers it dials.
    pub async fn start(mdns: &MdnsAddressLookup) -> Self {
        let mut events = mdns.subscribe().await;
        let heard = HeardMap::default();
        let map = heard.clone();
        let changes = watch::Sender::new(0u64);
        let notify = changes.clone();
        let task = tokio::spawn(async move {
            while let Some(event) = events.next().await {
                let mut map = map.lock();
                let changed = match event {
                    DiscoveryEvent::Discovered { endpoint_info, .. } => {
                        let Some(advert) = endpoint_info
                            .user_data()
                            .and_then(|data| lan::parse_advert(data.as_ref()))
                        else {
                            continue;
                        };
                        tracing::debug!(id = %endpoint_info.endpoint_id, name = advert.name, "heard a server");
                        let heard = Heard {
                            name: advert.name,
                            addrs: endpoint_info.data.ip_addrs().copied().collect(),
                        };
                        map.insert(endpoint_info.endpoint_id, heard.clone()) != Some(heard)
                    }
                    DiscoveryEvent::Expired { endpoint_id } => map.remove(&endpoint_id).is_some(),
                    _ => false,
                };
                drop(map);
                if changed {
                    notify.send_modify(|n| *n += 1);
                }
            }
        });
        Self {
            heard,
            changes,
            task: Some(task),
        }
    }

    /// Nothing is ever heard: the client does not listen on the LAN.
    /// Pins and forgets still notify watchers.
    pub fn off() -> Self {
        Self {
            heard: HeardMap::default(),
            changes: watch::Sender::new(0u64),
            task: None,
        }
    }

    /// Tell watchers the server list may have changed.
    pub fn changed(&self) {
        self.changes.send_modify(|n| *n += 1);
    }

    pub fn watch(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    pub fn heard(&self) -> HashMap<EndpointId, Heard> {
        self.heard.lock().clone()
    }

    /// `id` answered at `addrs`: newer than its last record, if it has one.
    pub fn refresh(&self, id: &EndpointId, addrs: &[SocketAddr]) {
        if let Some(heard) = self.heard.lock().get_mut(id) {
            heard.addrs = addrs.to_vec();
        }
    }

    pub fn get(&self, id: &EndpointId) -> Option<Heard> {
        self.heard.lock().get(id).cloned()
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}
