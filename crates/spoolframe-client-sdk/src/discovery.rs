//! Servers heard on the LAN (P1-13).
//!
//! The mDNS lookup only reports what it hears after a subscription, and
//! skips repeats of what it has already heard, so the SDK subscribes once,
//! as soon as the lookup is built, and keeps its own map for the life of
//! the client. Nor does it report a server that a dial is looking up when
//! it is heard: that record goes to the dial alone. So what the lookup
//! answers a dial is heard too ([`Discovery::overhearing`]). Records are
//! untrusted hints: where to dial and which key to expect. Iroh proves the
//! key on connect.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use iroh::EndpointId;
use iroh::address_lookup::{AddressLookup, EndpointData, EndpointInfo, Error, Item};
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
                        record(&mut map, &endpoint_info)
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

    /// `mdns`, as the endpoint's lookup: what it answers a dial is heard
    /// here as well, as if it had been reported.
    pub fn overhearing(&self, mdns: MdnsAddressLookup) -> Overheard {
        Overheard {
            inner: mdns,
            heard: self.heard.clone(),
            changes: self.changes.clone(),
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

/// `info` in `map`, if it is a Spoolframe server's record. Whether that
/// changed what is heard.
fn record(map: &mut HashMap<EndpointId, Heard>, info: &EndpointInfo) -> bool {
    let Some(advert) = info
        .user_data()
        .and_then(|data| lan::parse_advert(data.as_ref()))
    else {
        return false;
    };
    tracing::debug!(id = %info.endpoint_id, name = advert.name, "heard a server");
    let heard = Heard {
        name: advert.name,
        addrs: info.data.ip_addrs().copied().collect(),
    };
    map.insert(info.endpoint_id, heard.clone()) != Some(heard)
}

/// The mDNS lookup, with what it answers a dial recorded as heard
/// ([`Discovery::overhearing`]).
#[derive(Debug)]
pub(crate) struct Overheard {
    inner: MdnsAddressLookup,
    heard: HeardMap,
    changes: watch::Sender<u64>,
}

impl AddressLookup for Overheard {
    fn publish(&self, data: &EndpointData) {
        self.inner.publish(data);
    }

    fn resolve(&self, id: EndpointId) -> Option<BoxStream<'static, Result<Item, Error>>> {
        let (heard, changes) = (self.heard.clone(), self.changes.clone());
        let items = self.inner.resolve(id)?.inspect(move |item| {
            if let Ok(item) = item
                && record(&mut heard.lock(), item.endpoint_info())
            {
                changes.send_modify(|n| *n += 1);
            }
        });
        Some(Box::pin(items))
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}
