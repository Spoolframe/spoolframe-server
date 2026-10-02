//! This device: its identity and its endpoint (ARCHITECTURE §13–17).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, SecretKey, TransportAddr};
use iroh_mdns_address_lookup::MdnsAddressLookup;
use spoolframe_protocol::{LoginPassword, Reply, Request, Response, SessionId, lan};
use spoolframe_types::ClientCapabilities;

use crate::connection::ServerConnection;
use crate::discovery::Discovery;
use crate::error::SdkError;
use crate::link::{Link, StateWatch};
use crate::servers::{self, SavedServer, ServerEntry};
use crate::session::{self, Session, StoredSession};
use crate::storage::{Storage, StorageError, keys};

/// What the app says about this device and itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// What the device calls itself (a hostname, "Living room TV"); shown
    /// in the server's session lists.
    pub name: String,
    /// The app, e.g. `spoolframe-linux`, and its version.
    pub client_name: String,
    pub client_version: String,
    /// What this device can play, from its decoders (P1-07).
    pub capabilities: ClientCapabilities,
}

/// This device. Cheap to clone: every clone is the same device.
#[derive(Clone)]
pub struct Client {
    pub(crate) inner: Arc<Inner>,
}

pub(crate) struct Inner {
    pub(crate) endpoint: Endpoint,
    pub(crate) storage: Arc<dyn Storage>,
    pub(crate) device: Device,
    discovery: Discovery,
    /// Addresses the app gave for servers (P6's invite tickets will too).
    manual: Mutex<HashMap<EndpointId, Vec<SocketAddr>>>,
    /// Each server's connection and its state.
    links: Mutex<HashMap<EndpointId, Arc<Link>>>,
    /// Serializes read-modify-write of the saved servers and sessions.
    saving: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("id", &self.id())
            .field("device", &self.inner.device.name)
            .finish_non_exhaustive()
    }
}

impl Client {
    /// Open this device: its key from `storage` (made on first start), and
    /// an endpoint with relays off that listens for servers on the LAN and
    /// never announces itself.
    pub async fn open(storage: Arc<dyn Storage>, device: Device) -> Result<Self, SdkError> {
        let key = load_or_create_key(storage.as_ref()).await?;
        // Minimal: no pkarr or DNS lookup, so a LAN client never tries the
        // Internet. mDNS only listens: a laptop or a TV must not announce
        // itself to the network (P1-13).
        let cannot_bind =
            |e: String| SdkError::Unreachable(format!("cannot bind the endpoint: {e}"));
        let mdns = MdnsAddressLookup::builder()
            .advertise(false)
            .service_name(lan::MDNS_SERVICE)
            .build(key.public())
            .map_err(|e| cannot_bind(e.to_string()))?;
        let endpoint = Endpoint::builder(presets::Minimal)
            .secret_key(key)
            .relay_mode(RelayMode::Disabled)
            .bind()
            .await
            .map_err(|e| cannot_bind(e.to_string()))?;
        // The same lookup finds the addresses of servers being dialled.
        endpoint
            .address_lookup()
            .map_err(|e| cannot_bind(e.to_string()))?
            .add(mdns.clone());
        let discovery = Discovery::start(&mdns).await;
        Ok(Self {
            inner: Arc::new(Inner {
                endpoint,
                storage,
                device,
                discovery,
                manual: Mutex::default(),
                links: Mutex::default(),
                saving: tokio::sync::Mutex::default(),
            }),
        })
    }

    /// The servers this device knows: saved ones and ones heard on the LAN,
    /// one entry per key. Saved first, then heard, each by name.
    pub async fn servers(&self) -> Result<Vec<ServerEntry>, SdkError> {
        let saved = servers::load(self.inner.storage.as_ref()).await?;
        let mut heard = self.inner.discovery.heard();
        let mut entries: Vec<ServerEntry> = saved
            .into_iter()
            .map(|s| {
                let id = EndpointId::from_str(&s.endpoint_id).ok();
                let on_lan = id.and_then(|id| heard.remove(&id));
                ServerEntry {
                    endpoint_id: s.endpoint_id,
                    name: s.name,
                    pinned: true,
                    on_network: on_lan.is_some(),
                    addrs: on_lan.map_or(s.last_known_addrs, |h| h.addrs),
                }
            })
            .collect();
        let mut unsaved: Vec<ServerEntry> = heard
            .into_iter()
            .map(|(id, h)| ServerEntry {
                endpoint_id: id.to_string(),
                name: h.name,
                addrs: h.addrs,
                pinned: false,
                on_network: true,
            })
            .collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        unsaved.sort_by(|a, b| a.name.cmp(&b.name));
        entries.extend(unsaved);
        Ok(entries)
    }

    /// Changes of [`Client::servers`]: a server heard or gone on the LAN, or
    /// one pinned or forgotten. The first answer comes before mDNS has had
    /// time to hear anything (about a second), so an app lists once, then
    /// again on every change.
    pub fn watch_servers(&self) -> ServersWatch {
        ServersWatch(self.inner.discovery.watch())
    }

    /// Where to find server `endpoint_id`, from outside the LAN's records:
    /// typed in, or (P6) from an invite. A hint like any other: the key is
    /// still proven on connect.
    pub fn add_address(&self, endpoint_id: &str, addrs: Vec<SocketAddr>) -> Result<(), SdkError> {
        let id = parse_id(endpoint_id)?;
        self.inner
            .manual
            .lock()
            .expect("never poisoned")
            .insert(id, addrs);
        Ok(())
    }

    /// Connect to server `endpoint_id`, or use the open connection. A server
    /// not saved yet must have been heard on the LAN or given an address;
    /// the first connect saves it, pinning its key (TOFU, §19).
    pub async fn connect(&self, endpoint_id: &str) -> Result<ServerEntry, SdkError> {
        let id = parse_id(endpoint_id)?;
        self.connection(id).await?;
        let saved = servers::load(self.inner.storage.as_ref()).await?;
        let server = saved
            .into_iter()
            .find(|s| s.endpoint_id == endpoint_id)
            .expect("a connected server is saved");
        let heard = self.inner.discovery.get(&id);
        Ok(ServerEntry {
            endpoint_id: server.endpoint_id,
            name: server.name,
            addrs: server.last_known_addrs,
            pinned: true,
            on_network: heard.is_some(),
        })
    }

    /// Forget server `endpoint_id`: its pin and every session on it.
    pub async fn forget(&self, endpoint_id: &str) -> Result<(), SdkError> {
        let id = parse_id(endpoint_id)?;
        if let Some(link) = self.inner.links.lock().expect("never poisoned").remove(&id) {
            link.close();
        }
        let storage = self.inner.storage.as_ref();
        let _saving = self.inner.saving.lock().await;
        let mut saved = servers::load(storage).await?;
        saved.retain(|s| s.endpoint_id != endpoint_id);
        servers::save(storage, &saved).await?;
        storage
            .set_secret(&keys::sessions(endpoint_id), None)
            .await?;
        self.inner.discovery.changed();
        Ok(())
    }

    /// Sign `username` in on server `endpoint_id` with a password (§22),
    /// and keep the session.
    pub async fn sign_in(
        &self,
        endpoint_id: &str,
        username: &str,
        password: &str,
    ) -> Result<Session, SdkError> {
        let server = parse_id(endpoint_id)?;
        let conn = self.connection(server).await?;
        let login = Request::LoginPassword(LoginPassword {
            username: username.to_owned(),
            password: password.to_owned(),
        });
        let token = match conn.call(None, login).await? {
            Response::Ok(Reply::LoggedIn { session }) => session,
            Response::Ok(other) => {
                return Err(SdkError::Protocol(format!("login answered {other:?}")));
            }
            Response::Err(e) => return Err(SdkError::from_server(e)),
        };
        let user = match conn.call(Some(&token), Request::WhoAmI).await? {
            Response::Ok(Reply::WhoAmI(user)) => user,
            Response::Ok(other) => {
                return Err(SdkError::Protocol(format!("WhoAmI answered {other:?}")));
            }
            Response::Err(e) => return Err(SdkError::from_server(e)),
        };
        let storage = self.inner.storage.as_ref();
        let _saving = self.inner.saving.lock().await;
        let mut stored = session::load(storage, &server).await?;
        // Signing in again as the same user replaces the old session here;
        // the server keeps it until it expires or is revoked.
        stored.retain(|s| s.username != user.username);
        stored.push(StoredSession {
            token: token.0.clone(),
            username: user.username.clone(),
            display_name: user.display_name.clone(),
        });
        session::save(storage, &server, &stored).await?;
        Ok(Session {
            client: self.clone(),
            server,
            token,
            user,
        })
    }

    /// "Who's watching": the users signed in on server `endpoint_id` from
    /// this device, as the server names them now. Sessions the server no
    /// longer knows are forgotten; their users have to sign in again.
    pub async fn profiles(&self, endpoint_id: &str) -> Result<Vec<Session>, SdkError> {
        let server = parse_id(endpoint_id)?;
        let stored = session::load(self.inner.storage.as_ref(), &server).await?;
        let conn = self.connection(server).await?;
        let mut profiles = Vec::new();
        for s in stored {
            let token = SessionId(s.token);
            match conn.call(Some(&token), Request::WhoAmI).await? {
                Response::Ok(Reply::WhoAmI(user)) => profiles.push(Session {
                    client: self.clone(),
                    server,
                    token,
                    user,
                }),
                Response::Err(spoolframe_protocol::Error::Unauthenticated) => {
                    tracing::info!(user = s.username, "a session has ended; forgetting it");
                    self.drop_session(server, &token).await?;
                }
                Response::Ok(other) => {
                    return Err(SdkError::Protocol(format!("WhoAmI answered {other:?}")));
                }
                Response::Err(e) => return Err(SdkError::from_server(e)),
            }
        }
        Ok(profiles)
    }

    /// Forget the session `token` on `server`.
    pub(crate) async fn drop_session(
        &self,
        server: EndpointId,
        token: &SessionId,
    ) -> Result<(), SdkError> {
        let storage = self.inner.storage.as_ref();
        let _saving = self.inner.saving.lock().await;
        let mut stored = session::load(storage, &server).await?;
        stored.retain(|s| s.token != token.0);
        session::save(storage, &server, &stored).await?;
        Ok(())
    }

    /// Where server `endpoint_id`'s connection stands, and its changes.
    pub fn state(&self, endpoint_id: &str) -> Result<StateWatch, SdkError> {
        Ok(self.link(parse_id(endpoint_id)?).watch())
    }

    fn link(&self, id: EndpointId) -> Arc<Link> {
        self.inner
            .links
            .lock()
            .expect("never poisoned")
            .entry(id)
            .or_insert_with(|| Link::new(id))
            .clone()
    }

    /// The open connection to `id`: the one there is, or a new one.
    pub(crate) async fn connection(
        &self,
        id: EndpointId,
    ) -> Result<Arc<ServerConnection>, SdkError> {
        self.link(id).connection(self).await
    }

    /// Dial `id` and, once it answers as itself, pin it or refresh its pin.
    pub(crate) async fn dial(&self, id: EndpointId) -> Result<ServerConnection, SdkError> {
        let saved = servers::load(self.inner.storage.as_ref()).await?;
        let pinned = saved.iter().find(|s| s.endpoint_id == id.to_string());
        let heard = self.inner.discovery.get(&id);
        let manual = self
            .inner
            .manual
            .lock()
            .expect("never poisoned")
            .get(&id)
            .cloned();
        if pinned.is_none() && heard.is_none() && manual.is_none() {
            return Err(SdkError::UnknownServer(id.to_string()));
        }
        let mut addrs: Vec<SocketAddr> = Vec::new();
        let hints = [
            manual.as_ref(),
            heard.as_ref().map(|h| &h.addrs),
            pinned.map(|s| &s.last_known_addrs),
        ];
        for addr in hints.into_iter().flatten().flatten() {
            if !addrs.contains(addr) {
                addrs.push(*addr);
            }
        }
        let addr = EndpointAddr::from_parts(id, addrs.iter().copied().map(TransportAddr::Ip));
        let conn = ServerConnection::dial(&self.inner.endpoint, addr, &self.inner.device).await?;
        // Where the server answered. Not what was heard before dialling:
        // when the endpoint resolves a server through mDNS while dialling,
        // that record is not passed on to the discovery subscription, so
        // the heard addresses may be the old ones.
        let mut fresh = conn.remote_addrs();
        if fresh.is_empty() {
            fresh = addrs;
        }
        self.inner.discovery.refresh(&id, &fresh);
        self.pin(SavedServer {
            name: conn.info.name.clone(),
            endpoint_id: id.to_string(),
            last_known_addrs: fresh,
        })
        .await?;
        Ok(conn)
    }

    /// Save `server`, or refresh its name and addresses. The key is the
    /// match, so a pin is never moved to another key.
    async fn pin(&self, server: SavedServer) -> Result<(), SdkError> {
        let storage = self.inner.storage.as_ref();
        let _saving = self.inner.saving.lock().await;
        let mut saved = servers::load(storage).await?;
        match saved
            .iter_mut()
            .find(|s| s.endpoint_id == server.endpoint_id)
        {
            Some(existing) => *existing = server,
            None => {
                tracing::info!(
                    id = server.endpoint_id,
                    name = server.name,
                    "pinned a new server"
                );
                saved.push(server);
            }
        }
        servers::save(storage, &saved).await?;
        self.inner.discovery.changed();
        Ok(())
    }

    /// This device's identity, as servers see it.
    pub fn id(&self) -> String {
        self.endpoint_id().to_string()
    }

    pub(crate) fn endpoint_id(&self) -> EndpointId {
        self.inner.endpoint.id()
    }

    /// Close the endpoint and every connection on it.
    pub async fn close(&self) {
        let links: Vec<_> = self
            .inner
            .links
            .lock()
            .expect("never poisoned")
            .drain()
            .map(|(_, link)| link)
            .collect();
        for link in links {
            link.close();
        }
        self.inner.endpoint.close().await;
    }
}

fn parse_id(endpoint_id: &str) -> Result<EndpointId, SdkError> {
    EndpointId::from_str(endpoint_id)
        .map_err(|e| SdkError::UnknownServer(format!("{endpoint_id:?} is not a server key: {e}")))
}

/// Changes of the server list; see [`Client::watch_servers`].
#[derive(Debug, Clone)]
pub struct ServersWatch(tokio::sync::watch::Receiver<u64>);

impl ServersWatch {
    /// Waits for the next change; `false` once the client is gone.
    pub async fn changed(&mut self) -> bool {
        self.0.changed().await.is_ok()
    }
}

/// The device's key, made and stored only if storage says there is none.
async fn load_or_create_key(storage: &dyn Storage) -> Result<SecretKey, SdkError> {
    if let Some(bytes) = storage.secret(keys::ENDPOINT_KEY).await? {
        let bytes: [u8; 32] = bytes.try_into().map_err(|b: Vec<u8>| {
            StorageError(format!(
                "the device key is {} bytes, not 32; refusing to replace it",
                b.len()
            ))
        })?;
        return Ok(SecretKey::from_bytes(&bytes));
    }
    let key = SecretKey::generate();
    storage
        .set_secret(keys::ENDPOINT_KEY, Some(key.to_bytes().to_vec()))
        .await?;
    tracing::info!(id = %key.public(), "made this device's key");
    Ok(key)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use super::*;
    use crate::storage::MemoryStorage;

    fn device() -> Device {
        Device {
            name: "test".into(),
            client_name: "spoolframe-sdk-test".into(),
            client_version: "0".into(),
            capabilities: ClientCapabilities::default(),
        }
    }

    #[tokio::test]
    async fn the_key_is_the_same_after_reopening() {
        let storage = Arc::new(MemoryStorage::default());
        let first = Client::open(storage.clone(), device()).await.unwrap();
        let id = first.id();
        first.close().await;
        let again = Client::open(storage, device()).await.unwrap();
        assert_eq!(again.id(), id);
        again.close().await;
    }

    /// Secrets cannot be read; counts writes.
    #[derive(Default)]
    struct Locked {
        writes: AtomicUsize,
    }

    #[async_trait]
    impl Storage for Locked {
        async fn secret(&self, _: &str) -> Result<Option<Vec<u8>>, StorageError> {
            Err(StorageError("the keyring is locked".into()))
        }
        async fn set_secret(&self, _: &str, _: Option<Vec<u8>>) -> Result<(), StorageError> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn data(&self, _: &str) -> Result<Option<Vec<u8>>, StorageError> {
            Ok(None)
        }
        async fn set_data(&self, _: &str, _: Option<Vec<u8>>) -> Result<(), StorageError> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn unreadable_storage_is_an_error_and_no_key_is_made() {
        let storage = Arc::new(Locked::default());
        let err = Client::open(storage.clone(), device()).await.unwrap_err();
        assert_eq!(
            err,
            SdkError::Storage(StorageError("the keyring is locked".into()))
        );
        assert_eq!(storage.writes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_damaged_key_is_an_error_not_a_new_key() {
        let storage = Arc::new(MemoryStorage::default());
        storage
            .set_secret(keys::ENDPOINT_KEY, Some(vec![1, 2, 3]))
            .await
            .unwrap();
        assert!(matches!(
            Client::open(storage.clone(), device()).await,
            Err(SdkError::Storage(_))
        ));
        assert_eq!(
            storage.secret(keys::ENDPOINT_KEY).await.unwrap(),
            Some(vec![1, 2, 3])
        );
    }
}
