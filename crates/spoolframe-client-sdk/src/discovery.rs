//! Servers heard on the LAN (P1-13).
//!
//! The mDNS lookup only reports what it hears after a subscription, and
//! skips repeats of what it has already reported, so the SDK subscribes
//! once, when the client opens, and keeps its own map for the life of the
//! client. Records are untrusted hints: where to dial and which key to
//! expect. Iroh proves the key on connect.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use iroh::EndpointId;
use iroh_mdns_address_lookup::{DiscoveryEvent, MdnsAddressLookup};
use spoolframe_protocol::lan;
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
    task: JoinHandle<()>,
}

impl Discovery {
    /// Start listening on `mdns`, which the endpoint also uses to look up
    /// the addresses of the servers it dials.
    pub async fn start(mdns: &MdnsAddressLookup) -> Self {
        let mut events = mdns.subscribe().await;
        let heard = HeardMap::default();
        let map = heard.clone();
        let task = tokio::spawn(async move {
            while let Some(event) = events.next().await {
                let mut map = map.lock().expect("never poisoned");
                match event {
                    DiscoveryEvent::Discovered { endpoint_info, .. } => {
                        let Some(advert) = endpoint_info
                            .user_data()
                            .and_then(|data| lan::parse_advert(data.as_ref()))
                        else {
                            continue;
                        };
                        tracing::debug!(id = %endpoint_info.endpoint_id, name = advert.name, "heard a server");
                        map.insert(
                            endpoint_info.endpoint_id,
                            Heard {
                                name: advert.name,
                                addrs: endpoint_info.data.ip_addrs().copied().collect(),
                            },
                        );
                    }
                    DiscoveryEvent::Expired { endpoint_id } => {
                        map.remove(&endpoint_id);
                    }
                    _ => {}
                }
            }
        });
        Self { heard, task }
    }

    pub fn heard(&self) -> HashMap<EndpointId, Heard> {
        self.heard.lock().expect("never poisoned").clone()
    }

    /// `id` answered at `addrs`: newer than its last record, if it has one.
    pub fn refresh(&self, id: &EndpointId, addrs: &[SocketAddr]) {
        if let Some(heard) = self.heard.lock().expect("never poisoned").get_mut(id) {
            heard.addrs = addrs.to_vec();
        }
    }

    pub fn get(&self, id: &EndpointId) -> Option<Heard> {
        self.heard.lock().expect("never poisoned").get(id).cloned()
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self.task.abort();
    }
}
