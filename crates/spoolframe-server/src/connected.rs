//! The devices connected now and what their `Hello` said they play
//! (P3-15): what playback debug's "Plan for…" offers beside the stored
//! profiles. In memory; a connection's entry goes when it closes.

use std::collections::HashMap;
use std::sync::Mutex;

use spoolframe_protocol::Hello;
use spoolframe_types::{ClientCapabilities, Platform};

/// One connected device, as its last `Hello` described it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectedDevice {
    /// Hex `EndpointId`.
    pub endpoint: String,
    pub device_name: Option<String>,
    pub client_name: String,
    pub client_version: String,
    pub platform: Platform,
    /// `None`: it sent none, and every plan for it is unsupported.
    pub capabilities: Option<ClientCapabilities>,
}

/// Every connection that said `Hello`, by its stable id.
#[derive(Debug, Default)]
pub struct Connected(Mutex<HashMap<usize, ConnectedDevice>>);

impl Connected {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<usize, ConnectedDevice>> {
        self.0.lock().expect("never held across a panic")
    }

    /// Connection `id` from `endpoint` said `hello`.
    pub fn hello(&self, id: usize, endpoint: String, hello: &Hello) {
        self.lock().insert(
            id,
            ConnectedDevice {
                endpoint,
                device_name: hello.device_name.clone(),
                client_name: hello.client_name.clone(),
                client_version: hello.client_version.clone(),
                platform: hello.platform,
                capabilities: hello.capabilities.clone(),
            },
        );
    }

    /// Connection `id` closed.
    pub fn closed(&self, id: usize) {
        self.lock().remove(&id);
    }

    /// Each device connected now, once (the newest connection of an
    /// endpoint with several), by name.
    pub fn devices(&self) -> Vec<ConnectedDevice> {
        let entries = self.lock();
        let mut ids: Vec<&usize> = entries.keys().collect();
        ids.sort();
        let mut by_endpoint: HashMap<&str, &ConnectedDevice> = HashMap::new();
        for id in ids {
            let device = &entries[id];
            by_endpoint.insert(&device.endpoint, device);
        }
        let mut devices: Vec<ConnectedDevice> = by_endpoint.into_values().cloned().collect();
        devices.sort_by(|a, b| (a.label(), &a.endpoint).cmp(&(b.label(), &b.endpoint)));
        devices
    }

    /// The device connected now from `endpoint`, if any.
    pub fn device(&self, endpoint: &str) -> Option<ConnectedDevice> {
        self.devices().into_iter().find(|d| d.endpoint == endpoint)
    }
}

impl ConnectedDevice {
    /// What to call it: its own name, else its app's.
    pub fn label(&self) -> String {
        self.device_name
            .clone()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| self.client_name.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(name: &str, caps: Option<ClientCapabilities>) -> Hello {
        Hello {
            protocol_version: spoolframe_protocol::PROTOCOL_VERSION,
            client_name: "spoolframe-linux".into(),
            client_version: "1".into(),
            platform: Platform::Linux,
            device_name: Some(name.into()),
            capabilities: caps,
        }
    }

    #[test]
    fn a_device_is_listed_once_with_its_newest_hello_until_it_goes() {
        let connected = Connected::default();
        connected.hello(1, "aa".into(), &hello("Den PC", None));
        let caps = ClientCapabilities {
            max_bitrate: Some(8_000_000),
            ..ClientCapabilities::default()
        };
        connected.hello(2, "aa".into(), &hello("Den PC", Some(caps.clone())));
        connected.hello(3, "bb".into(), &hello("Attic", None));
        let devices = connected.devices();
        assert_eq!(
            devices.iter().map(|d| d.label()).collect::<Vec<_>>(),
            ["Attic", "Den PC"]
        );
        assert_eq!(connected.device("aa").unwrap().capabilities, Some(caps));
        connected.closed(2);
        connected.closed(1);
        assert!(connected.device("aa").is_none());
    }
}
