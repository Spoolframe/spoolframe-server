//! Relays (P6-03): a server with `network.relays` takes its home relay
//! from them and tells it first, and a device with relays on reaches the
//! server through it alone: the path when nothing can dial the server
//! directly. The relay is a real iroh relay on loopback, over plain HTTP.

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use iroh_relay::server::{RelayConfig, Server as RelayServer, ServerConfig};
use spoolframe_client_sdk::{Client, ClientOptions, Device, MemoryStorage, SdkError, Storage};
use spoolframe_protocol::invite::Invite;
use spoolframe_server::network::Relay;

use crate::support::{self, ServerOptions, TestServer};

/// A relay on loopback, and its URL.
async fn relay() -> (RelayServer, url::Url) {
    let mut config = ServerConfig::default();
    config.relay = Some(RelayConfig::new((Ipv4Addr::LOCALHOST, 0)));
    let relay = RelayServer::spawn(config).await.unwrap();
    let addr = relay.http_addr().expect("served over HTTP");
    (relay, url::Url::parse(&format!("http://{addr}")).unwrap())
}

/// A server whose only relay is `url`, once it has taken it as its home
/// relay: until then it cannot be reached through it.
async fn server_on(test: &str, url: &url::Url) -> TestServer {
    let options = ServerOptions {
        relays: vec![Relay::Custom(url.clone())],
        ..ServerOptions::default()
    };
    let server = TestServer::start_with(support::fresh_dir(test).await, options).await;
    let told = server.server.told();
    let home = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if is_home(&server, url) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(home.is_ok(), "no home relay: {:?}", told.relays());
    server
}

/// Whether the server's endpoint has `url` as its home relay now.
fn is_home(server: &TestServer, url: &url::Url) -> bool {
    let url = iroh::RelayUrl::from(url.clone());
    server.server.addr().relay_urls().any(|u| *u == url)
}

/// A link to `server` with its relays and no address: only a relay can
/// reach it.
fn relay_only_link(server: &TestServer) -> String {
    Invite {
        server: server.server.id().to_string(),
        relays: server.server.told().relays(),
        ..Invite::default()
    }
    .link()
}

async fn device(relays: bool) -> Client {
    device_on(relays, Arc::new(MemoryStorage::default())).await
}

/// A device with relays `relays`, over `storage`: the same device (its
/// key, saved servers) as every other opened over it, but a new endpoint,
/// with no path from an earlier run.
async fn device_on(relays: bool, storage: Arc<MemoryStorage>) -> Client {
    let options = ClientOptions {
        lan_discovery: false,
        relays,
        ..ClientOptions::default()
    };
    Client::open_with_options(
        storage,
        Device {
            name: "Den PC".into(),
            client_name: "sdk-test".into(),
            client_version: "0".into(),
            platform: spoolframe_client_sdk::types::Platform::Linux,
            capabilities: Default::default(),
        },
        options,
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn the_server_tells_its_home_relay() {
    let (relay, url) = relay().await;
    let server = server_on("relay-told", &url).await;
    assert_eq!(server.server.told().relays(), [url.to_string()]);
    server.stop().await;
    drop(relay);
}

#[tokio::test]
async fn a_device_with_relays_on_reaches_the_server_through_its_relay_alone() {
    let (relay, url) = relay().await;
    let server = server_on("relay-reached", &url).await;
    let client = device(true).await;
    let entry = client
        .connect_invite(&relay_only_link(&server))
        .await
        .unwrap();
    assert_eq!(entry.name, "Home Media");
    client.close().await;
    server.stop().await;
    drop(relay);
}

/// Acting on an invite whose link names relays is consent to them: a
/// device with relays off reaches the server through them, and saving it
/// keeps them chosen for it.
#[tokio::test]
async fn a_device_with_relays_off_reaches_an_invite_s_server_through_its_relays() {
    let (relay, url) = relay().await;
    let server = server_on("relay-off-invite", &url).await;
    let client = device(false).await;
    let link = relay_only_link(&server);
    assert_eq!(
        client.connect_invite(&link).await.unwrap().name,
        "Home Media"
    );
    let saved = client.save_invite_server(&link).await.unwrap();
    assert_eq!(saved.use_relays, Some(true));
    assert_eq!(saved.relays, [url.to_string()]);
    client.close().await;
    server.stop().await;
    drop(relay);
}

/// A server saved from a relay-only link, by a device with relays
/// `relays`, over the storage it returns.
async fn saved_through_relay(server: &TestServer, relays: bool) -> Arc<MemoryStorage> {
    let storage = Arc::new(MemoryStorage::default());
    let client = device_on(relays, storage.clone()).await;
    client
        .save_invite_server(&relay_only_link(server))
        .await
        .unwrap();
    client.close().await;
    storage
}

/// Whether a device with relays `relays` over `storage`, newly opened,
/// reaches saved `server`: through its relay only, every address saved
/// for it forgotten first (on loopback a relayed connection finds the
/// direct path, and the address it reached is saved).
async fn reaches(storage: Arc<MemoryStorage>, relays: bool, server: &TestServer) -> bool {
    forget_addresses(&storage).await;
    let client = device_on(relays, storage).await;
    let id = server.server.id().to_string();
    let reached = match client.connect(&id).await {
        Ok(_) => true,
        Err(SdkError::Unreachable(_)) => false,
        Err(e) => panic!("{e:?}"),
    };
    client.close().await;
    reached
}

/// Every address saved for every server in `storage`, forgotten.
async fn forget_addresses(storage: &MemoryStorage) {
    use spoolframe_client_sdk::storage::keys;
    let bytes = storage.data(keys::SERVERS).await.unwrap().unwrap();
    let mut servers: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
    for server in &mut servers {
        server["known_addrs"] = serde_json::json!([]);
        server["advertised_addrs"] = serde_json::json!([]);
    }
    let bytes = serde_json::to_vec(&servers).unwrap();
    storage.set_data(keys::SERVERS, Some(bytes)).await.unwrap();
}

/// A server's own choice wins over the client's both ways, and `None`
/// follows the client's.
#[tokio::test]
async fn a_server_s_relay_choice_wins_over_the_client_s_and_none_follows_it() {
    let (relay, url) = relay().await;
    let server = server_on("relay-choice", &url).await;
    let id = server.server.id().to_string();
    let storage = saved_through_relay(&server, false).await;
    // Chosen on by the invite: reached with the client's relays off.
    assert!(reaches(storage.clone(), false, &server).await);

    let client = device_on(true, storage.clone()).await;
    client.set_server_relays(&id, Some(false)).await.unwrap();
    client.close().await;
    // Off for it: not reached with the client's relays on.
    assert!(!reaches(storage.clone(), true, &server).await);

    let client = device_on(true, storage.clone()).await;
    client.set_server_relays(&id, None).await.unwrap();
    assert_eq!(client.servers().await.unwrap()[0].use_relays, None);
    client.close().await;
    // No choice: the client's.
    assert!(!reaches(storage.clone(), false, &server).await);
    assert!(reaches(storage, true, &server).await);
    server.stop().await;
    drop(relay);
}

/// A device with relays off never reaches, through a relay, a server
/// that has no choice of its own: one it only knows a relay of.
#[tokio::test]
async fn a_device_with_relays_off_never_reaches_a_server_without_a_choice_through_one() {
    let (relay, url) = relay().await;
    let server = server_on("relay-off", &url).await;
    let id = server.server.id().to_string();
    let storage = saved_through_relay(&server, true).await;
    let client = device_on(true, storage.clone()).await;
    client.set_server_relays(&id, None).await.unwrap();
    client.close().await;
    assert!(!reaches(storage, false, &server).await);
    server.stop().await;
    drop(relay);
}

/// The relays a server is reached through are the user's once edited:
/// a dead one alone does not reach it, and empty gives the list back.
#[tokio::test]
async fn an_edited_relay_list_is_the_one_dialled() {
    let (relay, url) = relay().await;
    let server = server_on("relay-edited", &url).await;
    let id = server.server.id().to_string();
    let storage = saved_through_relay(&server, true).await;
    let client = device_on(true, storage.clone()).await;
    client
        .set_server_relay_urls(&id, vec!["http://127.0.0.1:9/".into()])
        .await
        .unwrap();
    let entry = client.servers().await.unwrap().remove(0);
    assert!(entry.relays_edited);
    assert_eq!(entry.relays, ["http://127.0.0.1:9/"]);
    assert!(matches!(
        client
            .set_server_relay_urls(&id, vec!["ftp://relay".into()])
            .await,
        Err(SdkError::InvalidInput(_))
    ));
    client.close().await;
    assert!(!reaches(storage.clone(), true, &server).await);

    let client = device_on(true, storage.clone()).await;
    client
        .set_server_relay_urls(&id, vec![url.to_string()])
        .await
        .unwrap();
    client.close().await;
    assert!(reaches(storage, true, &server).await);
    server.stop().await;
    drop(relay);
}

/// The ticket's LAN check: relays that cannot be reached cost nothing to
/// a device that can dial the server directly.
#[tokio::test]
async fn a_relay_that_cannot_be_reached_does_not_stop_a_direct_connection() {
    // The discard port: nothing answers there.
    let dead = url::Url::parse("http://127.0.0.1:9/").unwrap();
    let options = ServerOptions {
        relays: vec![Relay::Custom(dead.clone())],
        ..ServerOptions::default()
    };
    let server = TestServer::start_with(support::fresh_dir("relay-dead").await, options).await;
    assert_eq!(server.server.told().relays(), [dead.to_string()]);
    let link = Invite {
        server: server.server.id().to_string(),
        addrs: server.server.addr().ip_addrs().copied().collect(),
        relays: server.server.told().relays(),
        ..Invite::default()
    }
    .link();
    let client = device(true).await;
    let entry = client.connect_invite(&link).await.unwrap();
    assert_eq!(entry.name, "Home Media");
    client.close().await;
    server.stop().await;
}

/// A stand-in pkarr relay (P6-05): it keeps the last record put for a
/// key and gives it back, as an iroh-dns-server's `/pkarr` does.
async fn pkarr_relay() -> (
    wiremock::MockServer,
    std::sync::Arc<parking_lot::Mutex<Option<Vec<u8>>>>,
) {
    use wiremock::matchers::method;
    use wiremock::{Mock, Request, ResponseTemplate};
    let relay = wiremock::MockServer::start().await;
    let kept: std::sync::Arc<parking_lot::Mutex<Option<Vec<u8>>>> = Default::default();
    let put = kept.clone();
    Mock::given(method("PUT"))
        .respond_with(move |request: &Request| {
            *put.lock() = Some(request.body.clone());
            ResponseTemplate::new(204)
        })
        .mount(&relay)
        .await;
    let get = kept.clone();
    Mock::given(method("GET"))
        .respond_with(move |_: &Request| match get.lock().clone() {
            Some(record) => ResponseTemplate::new(200).set_body_bytes(record),
            None => ResponseTemplate::new(404),
        })
        .mount(&relay)
        .await;
    (relay, kept)
}

/// The ticket's first check: a server that publishes to a pkarr relay
/// is found there by its key alone. The link names nothing but the key
/// and where it publishes; the record it put there names its relay.
#[tokio::test]
async fn a_device_with_only_the_key_finds_the_server_through_its_record() {
    let (relay, url) = relay().await;
    let (pkarr, record) = pkarr_relay().await;
    let lookup = url::Url::parse(&format!("{}/pkarr", pkarr.uri())).unwrap();
    let options = ServerOptions {
        relays: vec![Relay::Custom(url.clone())],
        pkarr: spoolframe_server::network::Pkarr::Custom(lookup.clone()),
        ..ServerOptions::default()
    };
    let server = TestServer::start_with(support::fresh_dir("pkarr-found").await, options).await;
    assert_eq!(server.server.told().lookup(), Some(lookup.to_string()));
    // Published once it has its home relay. iroh publishes what it knows
    // as soon as it binds, and the server publishes no private address:
    // on a host with none public (a CI container) that first record names
    // nothing at all, and a device that reads it finds no way to the
    // server. So the wait is for the record that names the relay, its TXT
    // `relay=http://127.0.0.1:port/` in the signed DNS packet.
    let relay_attr = format!("relay={}", iroh::RelayUrl::from(url.clone()));
    let names_relay = || {
        record.lock().as_ref().is_some_and(|r| {
            r.windows(relay_attr.len())
                .any(|w| w == relay_attr.as_bytes())
        })
    };
    let published = tokio::time::timeout(Duration::from_secs(10), async {
        while !names_relay() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(
        published.is_ok(),
        "no record naming {relay_attr} was published"
    );
    let link = Invite {
        server: server.server.id().to_string(),
        lookup: Some(lookup.to_string()),
        ..Invite::default()
    }
    .link();
    let client = device(true).await;
    let entry = client.connect_invite(&link).await.unwrap();
    assert_eq!(entry.name, "Home Media");
    client.close().await;
    server.stop().await;
    drop(relay);
}

/// With pkarr off, nothing is published and no device is told where to
/// look.
#[tokio::test]
async fn a_server_with_pkarr_off_publishes_nothing() {
    let (pkarr, record) = pkarr_relay().await;
    let server = TestServer::start(&format!("pkarr-off-{}", pkarr.address().port())).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(server.server.told().lookup(), None);
    assert!(record.lock().is_none());
    server.stop().await;
}

/// A relay of the server's own (P6-04), behind a proxy (plain HTTP on
/// loopback), for the main server whose key is `server` and that listens
/// on `port`, letting everyone else through or not; and its URL and key.
async fn own_relay(
    server: iroh::EndpointId,
    port: u16,
    everyone: &str,
) -> (iroh_relay::server::Server, url::Url, iroh::EndpointId) {
    let dir = support::fresh_dir(&format!("own-relay-{port}")).await;
    let config = spoolframe_server::own_relay::RelayConfig::parse(&format!(
        r#"schema = "spoolframe.relay.v1";
           server {{ key = "{server}"; addrs = ["127.0.0.1:{port}"]; }}
           access {{ everyone = "{everyone}"; }}
           tls {{ mode = "proxy"; }}
           http = "127.0.0.1:0";"#
    ))
    .unwrap();
    let key = iroh::SecretKey::generate();
    let id = key.public();
    let relay = spoolframe_server::own_relay::spawn(&config, key, &dir)
        .await
        .unwrap();
    let addr = relay.http_addr().unwrap();
    (
        relay,
        url::Url::parse(&format!("http://{addr}")).unwrap(),
        id,
    )
}

/// A free UDP port on loopback, for a server that must be known before it
/// starts.
fn free_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A main server homed on its own relay, its key made first so the relay
/// knows whom to ask.
async fn behind_own_relay(
    test: &str,
    everyone: &str,
) -> (TestServer, iroh_relay::server::Server, url::Url) {
    let dir = support::fresh_dir(test).await;
    let server_key = spoolframe_server::identity::load_or_create(&dir).unwrap();
    let port = free_port();
    let (relay, url, relay_key) = own_relay(server_key.public(), port, everyone).await;
    let options = ServerOptions {
        port: Some(port),
        relays: vec![Relay::Custom(url.clone())],
        own_relays: vec![relay_key],
        ..ServerOptions::default()
    };
    let server = TestServer::start_with(dir, options).await;
    let home = tokio::time::timeout(Duration::from_secs(10), async {
        while !is_home(&server, &url) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(home.is_ok(), "the server never homed on its own relay");
    (server, relay, url)
}

/// Whether an endpoint with `key` reaches `server` through `relay` alone.
async fn reaches_through(key: iroh::SecretKey, server: &TestServer, relay: &url::Url) -> bool {
    let endpoint = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
        .secret_key(key)
        .relay_mode(iroh::RelayMode::Custom(iroh::RelayMap::empty()))
        .bind()
        .await
        .unwrap();
    let addr = iroh::EndpointAddr::from_parts(
        server.server.id(),
        [iroh::TransportAddr::Relay(relay.clone().into())],
    );
    let connected = tokio::time::timeout(
        Duration::from_secs(5),
        endpoint.connect(addr, spoolframe_protocol::CLIENT_ALPN),
    )
    .await;
    let reached = matches!(connected, Ok(Ok(_)));
    endpoint.close().await;
    reached
}

/// The ticket's check: a device the server knows is relayed; a stranger
/// is refused at the relay, as the default access says.
#[tokio::test]
async fn the_own_relay_carries_the_owner_s_devices_and_refuses_strangers() {
    let (server, relay, url) = behind_own_relay("own-relay-tiers", "deny").await;
    server.add_user("alice", "correct horse", None).await;
    let phone = server.client().await;
    phone.hello().await;
    phone.login("alice", "correct horse").await;
    let phone_key = phone.endpoint.secret_key().clone();
    assert!(
        reaches_through(phone_key, &server, &url).await,
        "the owner's device was refused"
    );
    assert!(
        !reaches_through(iroh::SecretKey::generate(), &server, &url).await,
        "a stranger was relayed"
    );
    phone.close().await;
    server.stop().await;
    drop(relay);
}

#[tokio::test]
async fn an_own_relay_open_to_everyone_carries_a_stranger() {
    let (server, relay, url) = behind_own_relay("own-relay-everyone", "allow").await;
    assert!(reaches_through(iroh::SecretKey::generate(), &server, &url).await);
    server.stop().await;
    drop(relay);
}

/// TLS from the operator's files (P6-04): a certificate chain and key in
/// PEM serve HTTPS. A self-signed one here; no device would trust it.
#[tokio::test]
async fn the_own_relay_serves_tls_from_pem_files() {
    let dir = support::fresh_dir("own-relay-files").await;
    std::fs::create_dir_all(&dir).unwrap();
    let cert = rcgen::generate_simple_self_signed(vec!["relay.example.org".into()]).unwrap();
    let (cert_file, key_file) = (dir.join("cert.pem"), dir.join("key.pem"));
    std::fs::write(&cert_file, cert.cert.pem()).unwrap();
    std::fs::write(&key_file, cert.signing_key.serialize_pem()).unwrap();
    let server = iroh::SecretKey::generate().public();
    let config = spoolframe_server::own_relay::RelayConfig::parse(&format!(
        r#"schema = "spoolframe.relay.v1";
           server {{ key = "{server}"; addrs = ["127.0.0.1:9"]; }}
           tls {{ mode = "files"; https = "127.0.0.1:0";
                 cert_file = "{}"; key_file = "{}"; }}
           http = "127.0.0.1:0";"#,
        cert_file.display(),
        key_file.display()
    ))
    .unwrap();
    let relay = spoolframe_server::own_relay::spawn(&config, iroh::SecretKey::generate(), &dir)
        .await
        .unwrap();
    let https = relay.https_addr().unwrap();
    let der = cert.cert.der().clone();
    // TLS 1.3 only (P6-07, glm L-3): a client of 1.2 alone is refused on
    // the version, and one of 1.3 is served.
    let tls12 = handshake(https, der.clone(), &rustls::version::TLS12).await;
    assert!(
        matches!(
            tls12,
            Err(rustls::Error::AlertReceived(
                rustls::AlertDescription::ProtocolVersion
            ))
        ),
        "{tls12:?}"
    );
    let tls13 = handshake(https, der, &rustls::version::TLS13).await;
    assert_eq!(tls13, Ok(rustls::ProtocolVersion::TLSv1_3));
    relay.shutdown().await.unwrap();
}

/// A TLS handshake with `addr` as `relay.example.org`, trusting `cert`
/// and offering only `version`: the version agreed, or why there is none.
async fn handshake(
    addr: std::net::SocketAddr,
    cert: rustls::pki_types::CertificateDer<'static>,
    version: &'static rustls::SupportedProtocolVersion,
) -> Result<rustls::ProtocolVersion, rustls::Error> {
    tokio::task::spawn_blocking(move || {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert).unwrap();
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_protocol_versions(&[version])
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        let name = rustls::pki_types::ServerName::try_from("relay.example.org").unwrap();
        let mut tls = rustls::ClientConnection::new(Arc::new(config), name).unwrap();
        let mut tcp = std::net::TcpStream::connect(addr).unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        while tls.is_handshaking() {
            if let Err(e) = tls.complete_io(&mut tcp) {
                return Err(e
                    .into_inner()
                    .and_then(|e| e.downcast::<rustls::Error>().ok())
                    .map(|e| *e)
                    .unwrap_or_else(|| rustls::Error::General("the connection broke".into())));
            }
        }
        Ok(tls.protocol_version().unwrap())
    })
    .await
    .unwrap()
}

/// The value of gauge or counter `name` in `server`'s metrics, if it has
/// a sample.
fn metric(server: &TestServer, name: &str) -> Option<f64> {
    server
        .metrics
        .families()
        .into_iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("no {name}"))
        .value(&[])
}

/// The metrics say whether the server has a home relay: 1 once it has
/// one, 0 while it has relays and none answers, and nothing on a server
/// with no relays.
#[tokio::test]
async fn the_home_relay_gauge_says_whether_it_has_one() {
    let (relay, url) = relay().await;
    let server = server_on("relay-gauge", &url).await;
    let connected = |server: &TestServer| metric(server, "spoolframe_home_relay_connected");
    // The address names the relay a moment before its status says so.
    let up = tokio::time::timeout(Duration::from_secs(10), async {
        while connected(&server) != Some(1.0) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(up.is_ok(), "{:?}", connected(&server));
    server.stop().await;
    drop(relay);

    let options = ServerOptions {
        relays: vec![Relay::Custom("http://127.0.0.1:9".parse().unwrap())],
        ..ServerOptions::default()
    };
    let server =
        TestServer::start_with(support::fresh_dir("relay-gauge-down").await, options).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(connected(&server), Some(0.0));
    server.stop().await;

    let server = TestServer::start("relay-gauge-none").await;
    assert_eq!(connected(&server), None);
    server.stop().await;
}

/// A device that reaches the server through a relay alone starts on the
/// relay: an attempt to punch a hole, counted once.
#[tokio::test]
async fn a_connection_that_starts_on_a_relay_is_a_hole_punch_attempt() {
    let (relay, url) = relay().await;
    let server = server_on("relay-punch", &url).await;
    let attempts = |server: &TestServer| metric(server, "spoolframe_hole_punch_attempts_total");
    assert_eq!(attempts(&server), Some(0.0));
    let client = device(true).await;
    client
        .connect_invite(&relay_only_link(&server))
        .await
        .unwrap();
    let counted = tokio::time::timeout(Duration::from_secs(10), async {
        while attempts(&server) != Some(1.0) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(counted.is_ok(), "{:?}", attempts(&server));
    client.close().await;
    server.stop().await;
    drop(relay);
}
