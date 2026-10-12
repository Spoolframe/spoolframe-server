//! The server's public addresses (`network.wan`, P6-02): they join what a
//! signed-in device is told (PX-12), so the SDK saves them, and the admin
//! status lists each with its source. Nothing here reaches beyond this
//! machine: no router is asked and no lookup service is called.

use crate::support;

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use spoolframe_admin_api::{
    AddressSource, AdminApiClient, LookupStatus, MappingStatus, MeApiClient, PublicAddress,
    ServerStatus, error_code,
};
use spoolframe_client_sdk::MemoryStorage;
use spoolframe_protocol::Event;
use spoolframe_protocol::codec::read_frame;
use spoolframe_protocol::streams::uni;
use spoolframe_server::admin::{self, Origins, TrustedProxies};
use spoolframe_server::passkeys::Passkeys;
use spoolframe_server::wan::{
    Finders, Lookup, LookupVia, MappedPort, Mapping, Mode, PublicAddr, PublicIp, Source, Wan,
};
use support::{FakeRouter, ServerOptions, TestClient, TestServer, fresh_dir, mkv_capabilities};
use tokio::io::AsyncReadExt;

const PASSWORD: &str = "correct horse";

/// `wan.static` with these addresses, and no mapped port: nothing asks a
/// router or a lookup service.
fn statics(addrs: &[&str]) -> Wan {
    Wan {
        statics: addrs.iter().map(|a| a.parse().unwrap()).collect(),
        mapped_port: None,
    }
}

async fn server_with(test: &str, wan: Option<Wan>) -> TestServer {
    let options = ServerOptions {
        wan,
        ..ServerOptions::default()
    };
    let server = TestServer::start_with(fresh_dir(test).await, options).await;
    server.add_user("alice", PASSWORD, Some("Alice")).await;
    server
}

/// What `client` is pushed once it says Hello and signs in as alice.
async fn told(client: &TestClient) -> Vec<SocketAddr> {
    client.hello().await;
    let mut events = tokio::time::timeout(Duration::from_secs(5), client.conn.accept_uni())
        .await
        .expect("no events stream within 5 s")
        .unwrap();
    assert_eq!(events.read_u8().await.unwrap(), uni::EVENTS);
    client.login("alice", PASSWORD).await;
    let event = tokio::time::timeout(Duration::from_secs(5), read_frame::<_, Event>(&mut events))
        .await
        .expect("no event within 5 s")
        .unwrap();
    match event {
        Event::ServerAddresses { addrs, .. } => addrs,
        other => panic!("not the server's addresses: {other:?}"),
    }
}

/// The admin status of `server`, asked as an admin over the admin API.
async fn status(server: &TestServer) -> ServerStatus {
    admin_of(server, support::offline_finders())
        .await
        .server_status()
        .await
        .unwrap()
}

/// An admin's client of the admin API beside `server`, whose wizard tries
/// `network.wan` with `finders`.
async fn admin_of(server: &TestServer, finders: Finders) -> jsonrpsee::ws_client::WsClient {
    let phc = spoolframe_server::auth::hash_password(PASSWORD)
        .await
        .unwrap();
    // Made by the first call; a second one finds it there.
    let _ = server
        .store
        .add_user("root", None, &phc, spoolframe_types::Role::Admin)
        .await;
    let source = admin::StatusSource::new(
        server.name.clone(),
        server.server_id.clone(),
        support::transcoding(),
    );
    source.set_told(server.server.told());
    let origins = Origins::default();
    let (addr, handle) = admin::serve(
        "127.0.0.1:0".parse().unwrap(),
        admin::Admin {
            status: Arc::new(source),
            passkeys: Arc::new(Passkeys::new(&origins, &server.server_id)),
            wizard: admin::Wizard {
                wan: finders,
                ..admin::Wizard::default()
            },
            ..crate::admin::admin(server.auth.clone(), None)
        },
        origins,
        TrustedProxies::default(),
    )
    .await
    .unwrap();
    std::mem::forget(handle);
    let login = crate::admin::anonymous(addr)
        .await
        .login(crate::admin::creds("root", PASSWORD))
        .await
        .unwrap();
    crate::admin::with_token(addr, &login.token).await
}

/// `wan.static` alone: a signed-in device is told the address, after the
/// endpoint's own.
#[tokio::test]
async fn a_static_address_is_told_to_a_signed_in_device() {
    let server = server_with(
        "wan-static-told",
        Some(statics(&["203.0.113.7:7420", "[2001:db8::7]:7420"])),
    )
    .await;
    let client = server.client().await;
    let addrs = told(&client).await;
    assert!(
        addrs.ends_with(&[
            "203.0.113.7:7420".parse().unwrap(),
            "[2001:db8::7]:7420".parse().unwrap()
        ]),
        "{addrs:?}"
    );
    server.stop().await;
}

/// PX-12: the SDK keeps what it is told as the server's advertised
/// addresses, the static one with them.
#[tokio::test]
async fn the_sdk_saves_a_static_address_as_advertised() {
    let server = server_with("wan-static-sdk", Some(statics(&["203.0.113.7:7420"]))).await;
    let first = *server
        .server
        .addr()
        .ip_addrs()
        .next()
        .expect("an address to sign in over");
    let storage = Arc::new(MemoryStorage::default());
    let device = spoolframe_client_sdk::Device {
        name: "phone".into(),
        client_name: "sdk-test".into(),
        client_version: "0".into(),
        platform: spoolframe_client_sdk::types::Platform::Ios,
        capabilities: mkv_capabilities(),
    };
    let client = support::sdk_client(storage, device).await.unwrap();
    let id = server.server.id().to_string();
    client.add_address(&id, vec![first]).unwrap();
    client.connect(&id).await.unwrap();
    client.pin(&id, true).await.unwrap();
    client.sign_in(&id, "alice", PASSWORD, false).await.unwrap();

    let static_addr: SocketAddr = "203.0.113.7:7420".parse().unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let saved = client
            .servers()
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.endpoint_id == id)
            .expect("the server is saved");
        if saved.advertised_addrs.contains(&static_addr) {
            assert!(saved.addrs.contains(&static_addr), "{:?}", saved.addrs);
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "advertised {:?}, without {static_addr}",
            saved.advertised_addrs
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    client.close().await;
    server.stop().await;
}

/// The admin status lists each public address with its source, and they
/// are among the addresses told.
#[tokio::test]
async fn the_status_lists_a_static_address_with_its_source() {
    let server = server_with("wan-static-status", Some(statics(&["203.0.113.7:7420"]))).await;
    let status = status(&server).await;
    assert!(status.wan);
    assert_eq!(
        status.public_addresses,
        [PublicAddress {
            address: "203.0.113.7:7420".into(),
            source: AddressSource::Static,
        }]
    );
    assert_eq!(status.public_notes, Vec::<String>::new());
    assert!(
        status
            .told_addresses
            .contains(&"203.0.113.7:7420".to_owned()),
        "{:?}",
        status.told_addresses
    );
    server.stop().await;
}

/// Without `network.wan`, nothing changes: the status says so, and a
/// device is told just what `network.announce` tells.
#[tokio::test]
async fn without_wan_nothing_public_is_told_or_listed() {
    let server = server_with("wan-absent", None).await;
    let expected: BTreeSet<SocketAddr> = server.server.told().addrs().into_iter().collect();
    let client = server.client().await;
    assert_eq!(
        told(&client).await.into_iter().collect::<BTreeSet<_>>(),
        expected
    );
    let status = status(&server).await;
    assert!(!status.wan);
    assert_eq!(status.public_addresses, []);
    assert_eq!(status.public_notes, Vec::<String>::new());
    server.stop().await;
}

/// The public addresses once they are `wanted`, or after 10 s.
async fn public_once(
    server: &TestServer,
    wanted: impl Fn(&[PublicAddr]) -> bool,
) -> Vec<PublicAddr> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let now = server.server.told().public().addrs();
        if wanted(&now) || tokio::time::Instant::now() > deadline {
            return now;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A stand-in for wtfismyip answering `body` to every GET of /text.
async fn lookup_service(status: u16, body: &str) -> wiremock::MockServer {
    let service = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/text"))
        .respond_with(wiremock::ResponseTemplate::new(status).set_body_string(body))
        .mount(&service)
        .await;
    service
}

fn text_url(service: &wiremock::MockServer) -> String {
    format!("{}/text", service.uri())
}

#[tokio::test]
async fn a_lookup_reads_the_public_ipv4_address() {
    let service = lookup_service(200, "203.0.113.9\n").await;
    let lookup = support::lookup_at(&text_url(&service), &[]);
    assert_eq!(
        lookup.once(LookupVia::Wtfismyip).await,
        Ok("203.0.113.9".parse().unwrap())
    );
}

/// The answer must parse as a public IPv4 address, or it is ignored.
#[tokio::test]
async fn a_lookup_answer_that_is_not_a_public_ipv4_address_is_refused() {
    for (answer, says) in [
        ("10.0.0.1", "(10.0.0.0/8)"),
        ("100.64.3.4", "a carrier-grade NAT address (100.64.0.0/10)"),
        ("192.168.0.2\n", "(192.168.0.0/16)"),
        ("2001:db8::1", "not an IPv4 address"),
        ("<html>blocked</html>", "not an IPv4 address"),
        ("", "not an IPv4 address"),
    ] {
        let service = lookup_service(200, answer).await;
        let lookup = support::lookup_at(&text_url(&service), &[]);
        let err = lookup.once(LookupVia::Wtfismyip).await.unwrap_err();
        assert!(err.contains(says), "{answer:?}: {err}");
    }
    let service = lookup_service(503, "203.0.113.9").await;
    let lookup = support::lookup_at(&text_url(&service), &[]);
    let err = lookup.once(LookupVia::Wtfismyip).await.unwrap_err();
    assert!(err.contains("503"), "{err}");
}

#[tokio::test]
async fn a_lookup_reads_no_more_than_a_short_answer() {
    let service = lookup_service(200, &"1".repeat(10_000)).await;
    let lookup = support::lookup_at(&text_url(&service), &[]);
    let err = lookup.once(LookupVia::Wtfismyip).await.unwrap_err();
    assert!(err.contains("longer than 64 bytes"), "{err}");
}

#[tokio::test]
async fn a_lookup_that_does_not_answer_in_time_fails() {
    let service = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_string("203.0.113.9")
                .set_delay(Duration::from_secs(5)),
        )
        .mount(&service)
        .await;
    let lookup = Lookup {
        timeout: Duration::from_millis(200),
        ..support::lookup_at(&text_url(&service), &[])
    };
    let err = lookup.once(LookupVia::Wtfismyip).await.unwrap_err();
    assert!(err.contains("did not answer within"), "{err}");
}

/// A failed lookup is asked again after each wait of its backoff.
#[tokio::test]
async fn a_failed_lookup_is_asked_again_until_it_answers() {
    let service = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(wiremock::ResponseTemplate::new(502))
        .up_to_n_times(2)
        .with_priority(1)
        .expect(2)
        .mount(&service)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("203.0.113.9"))
        .with_priority(2)
        .expect(1)
        .mount(&service)
        .await;
    let wait = Duration::from_millis(10);
    let lookup = support::lookup_at(&text_url(&service), &[wait, wait, wait]);
    assert_eq!(
        lookup.retried(LookupVia::Wtfismyip).await,
        Ok("203.0.113.9".parse().unwrap())
    );
    service.verify().await;
}

/// A `manual` mapped port, as the operator forwarded it on the router.
fn forwarded(public_ip: PublicIp, lookup_via: Option<LookupVia>) -> Wan {
    Wan {
        mapped_port: Some(MappedPort {
            port: Some(7420),
            mode: Mode::Manual,
            public_ip,
            lookup_via,
        }),
        ..statics(&[])
    }
}

async fn server_finding(test: &str, wan: Wan, lookup: Lookup) -> TestServer {
    let options = ServerOptions {
        wan: Some(wan),
        finders: Finders {
            lookup,
            ..support::offline_finders()
        },
        ..ServerOptions::default()
    };
    TestServer::start_with(fresh_dir(test).await, options).await
}

#[tokio::test]
async fn a_forwarded_port_with_its_public_ip_written_is_advertised() {
    let ip = PublicIp::Fixed("198.51.100.4".parse().unwrap());
    let server = server_finding(
        "wan-manual-fixed",
        forwarded(ip, None),
        support::offline_finders().lookup,
    )
    .await;
    let found = public_once(&server, |found| !found.is_empty()).await;
    assert_eq!(
        found,
        [PublicAddr {
            addr: "198.51.100.4:7420".parse().unwrap(),
            source: Source::MappedPort,
        }]
    );
    server.stop().await;
}

/// A lookup that failed is tried again later, and what it then finds is
/// advertised (review 2026-10-11 M31): a server started before its
/// network was up, or given a new address, does not wait for a restart.
#[tokio::test]
async fn a_failed_public_ip_lookup_is_tried_again_later() {
    let service = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(wiremock::ResponseTemplate::new(503))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&service)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("203.0.113.7"))
        .with_priority(2)
        .mount(&service)
        .await;
    let lookup = Lookup {
        again: Duration::from_millis(200),
        ..support::lookup_at(&text_url(&service), &[])
    };
    let server = server_finding(
        "wan-lookup-again",
        forwarded(PublicIp::Lookup, Some(LookupVia::Wtfismyip)),
        lookup,
    )
    .await;
    let found = public_once(&server, |found| !found.is_empty()).await;
    assert_eq!(
        found,
        [PublicAddr {
            addr: "203.0.113.7:7420".parse().unwrap(),
            source: Source::Lookup,
        }]
    );
    server.stop().await;
}

#[tokio::test]
async fn a_forwarded_port_with_its_public_ip_looked_up_is_advertised() {
    let service = lookup_service(200, "203.0.113.9\n").await;
    let server = server_finding(
        "wan-manual-lookup",
        forwarded(PublicIp::Lookup, Some(LookupVia::Wtfismyip)),
        support::lookup_at(&text_url(&service), &[]),
    )
    .await;
    let found = public_once(&server, |found| !found.is_empty()).await;
    assert_eq!(
        found,
        [PublicAddr {
            addr: "203.0.113.9:7420".parse().unwrap(),
            source: Source::Lookup,
        }]
    );
    let status = status(&server).await;
    assert_eq!(
        status.public_addresses,
        [PublicAddress {
            address: "203.0.113.9:7420".into(),
            source: AddressSource::Lookup,
        }]
    );
    assert_eq!(status.wan_mapping, MappingStatus::NotUsed);
    assert!(
        matches!(
            &status.wan_lookup,
            LookupStatus::Found { ip, via, at } if ip == "203.0.113.9" && via == "wtfismyip" && *at > 0
        ),
        "{:?}",
        status.wan_lookup
    );
    server.stop().await;
}

/// A lookup that keeps failing costs a note on the status page, and the
/// server runs on with what it has.
#[tokio::test]
async fn a_failed_lookup_is_a_status_note_and_the_server_runs_on() {
    let service = lookup_service(500, "").await;
    let wan = Wan {
        statics: vec!["198.51.100.7:7420".parse().unwrap()],
        ..forwarded(PublicIp::Lookup, Some(LookupVia::Wtfismyip))
    };
    let wait = Duration::from_millis(10);
    let server = server_finding(
        "wan-manual-lookup-fails",
        wan,
        support::lookup_at(&text_url(&service), &[wait, wait]),
    )
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while server.server.told().public().notes().is_empty() {
        assert!(tokio::time::Instant::now() < deadline, "no note");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        service.received_requests().await.unwrap().len(),
        3,
        "asked once and again after each wait"
    );
    let status = status(&server).await;
    assert_eq!(
        status.public_addresses,
        [PublicAddress {
            address: "198.51.100.7:7420".into(),
            source: AddressSource::Static,
        }]
    );
    assert_eq!(
        status.public_notes,
        ["public IP lookup failed: wtfismyip answered 500 Internal Server Error"]
    );
    assert_eq!(
        status.wan_lookup,
        LookupStatus::Failed {
            error: "wtfismyip answered 500 Internal Server Error".into(),
            retrying: false,
        }
    );
    server.stop().await;
}

/// `mode = "auto"`, asking `router`, with the public IP for a mapping
/// whose external address is not one.
fn auto(public_ip: PublicIp) -> Wan {
    Wan {
        mapped_port: Some(MappedPort {
            port: None,
            mode: Mode::Auto,
            public_ip,
            lookup_via: Some(LookupVia::Wtfismyip),
        }),
        ..statics(&[])
    }
}

async fn server_asking(
    test: &str,
    wan: Wan,
    router: Arc<FakeRouter>,
    lookup: Lookup,
) -> TestServer {
    let options = ServerOptions {
        wan: Some(wan),
        finders: Finders {
            lookup,
            mapper: router,
        },
        ..ServerOptions::default()
    };
    TestServer::start_with(fresh_dir(test).await, options).await
}

fn mapped(addr: &str, source: Source) -> PublicAddr {
    PublicAddr {
        addr: addr.parse().unwrap(),
        source,
    }
}

/// The router maps the endpoint's own port, and its external address is
/// advertised.
#[tokio::test]
async fn a_mapping_the_router_grants_is_advertised() {
    let router = FakeRouter::new();
    let server = server_asking(
        "wan-auto-granted",
        auto(PublicIp::Lookup),
        router.clone(),
        support::offline_finders().lookup,
    )
    .await;
    let own_port = server
        .server
        .addr()
        .ip_addrs()
        .find(|a| a.is_ipv4())
        .expect("an IPv4 address")
        .port();
    router.answer(Mapping::Granted("198.51.100.20:40000".parse().unwrap()));
    let found = public_once(&server, |found| !found.is_empty()).await;
    assert_eq!(found, [mapped("198.51.100.20:40000", Source::MappedPort)]);
    assert_eq!(router.asked(), [own_port]);
    let status = status(&server).await;
    assert_eq!(
        status.public_addresses,
        [PublicAddress {
            address: "198.51.100.20:40000".into(),
            source: AddressSource::MappedPort,
        }]
    );
    assert_eq!(
        status.wan_mapping,
        MappingStatus::Granted {
            local_port: own_port,
            external: "198.51.100.20:40000".into(),
        }
    );
    assert_eq!(status.wan_lookup, LookupStatus::NotNeeded);
    server.stop().await;
}

/// A router that refuses, or has none of UPnP, NAT-PMP and PCP, costs a
/// note: the server runs on, and says no public address was found.
#[tokio::test]
async fn a_refused_mapping_is_a_note_and_the_server_runs_on() {
    let router = FakeRouter::new();
    router.answer(Mapping::Refused(
        "the router answers none of UPnP, NAT-PMP and PCP".into(),
    ));
    let server = server_asking(
        "wan-auto-refused",
        auto(PublicIp::Lookup),
        router,
        support::offline_finders().lookup,
    )
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while server.server.told().public().notes().is_empty() {
        assert!(tokio::time::Instant::now() < deadline, "no note");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let status = status(&server).await;
    assert!(status.wan);
    assert_eq!(status.public_addresses, []);
    assert_eq!(
        status.public_notes,
        ["router port mapping: the router answers none of UPnP, NAT-PMP and PCP"]
    );
    assert!(
        matches!(
            &status.wan_mapping,
            MappingStatus::Refused { reason }
                if reason == "the router answers none of UPnP, NAT-PMP and PCP"
        ),
        "{:?}",
        status.wan_mapping
    );
    server.add_user("alice", PASSWORD, None).await;
    let client = server.client().await;
    assert!(!told(&client).await.is_empty(), "the server serves on");
    server.stop().await;
}

/// A router behind carrier-grade NAT maps the port but reports its own,
/// private, side: the public IP is looked up and advertised with the
/// mapped port.
#[tokio::test]
async fn a_mapping_without_a_public_address_falls_back_to_the_public_ip() {
    let service = lookup_service(200, "203.0.113.9").await;
    let router = FakeRouter::new();
    router.answer(Mapping::Granted("100.64.7.7:40000".parse().unwrap()));
    let server = server_asking(
        "wan-auto-cgnat",
        auto(PublicIp::Lookup),
        router,
        support::lookup_at(&text_url(&service), &[]),
    )
    .await;
    let found = public_once(&server, |found| !found.is_empty()).await;
    assert_eq!(found, [mapped("203.0.113.9:40000", Source::Lookup)]);
    server.stop().await;

    let router = FakeRouter::new();
    router.answer(Mapping::Granted("192.168.0.1:40000".parse().unwrap()));
    let server = server_asking(
        "wan-auto-private-fixed",
        auto(PublicIp::Fixed("198.51.100.4".parse().unwrap())),
        router,
        support::offline_finders().lookup,
    )
    .await;
    let found = public_once(&server, |found| !found.is_empty()).await;
    assert_eq!(found, [mapped("198.51.100.4:40000", Source::MappedPort)]);
    server.stop().await;
}

/// The mapper renews the mapping before it expires; a renewal that moves
/// it is advertised anew, a lapse while it asks again keeps the last one,
/// and a refusal takes it away.
#[tokio::test]
async fn a_renewed_mapping_is_followed() {
    let router = FakeRouter::new();
    let server = server_asking(
        "wan-auto-renewed",
        auto(PublicIp::Lookup),
        router.clone(),
        support::offline_finders().lookup,
    )
    .await;
    router.answer(Mapping::Granted("198.51.100.20:40000".parse().unwrap()));
    let first = public_once(&server, |found| !found.is_empty()).await;
    assert_eq!(first, [mapped("198.51.100.20:40000", Source::MappedPort)]);

    router.answer(Mapping::Granted("198.51.100.21:40002".parse().unwrap()));
    let renewed = public_once(&server, |found| found != first.as_slice()).await;
    assert_eq!(renewed, [mapped("198.51.100.21:40002", Source::MappedPort)]);

    router.answer(Mapping::Asking);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(server.server.told().public().addrs(), renewed);

    router.answer(Mapping::Refused("the router did not map the port".into()));
    let gone = public_once(&server, |found| found.is_empty()).await;
    assert_eq!(gone, []);
    server.stop().await;
}

/// `client` said Hello and signed in as alice: its events stream, and the
/// addresses it was pushed first.
async fn signed_in(client: &TestClient) -> (iroh::endpoint::RecvStream, Vec<SocketAddr>) {
    client.hello().await;
    let mut events = tokio::time::timeout(Duration::from_secs(5), client.conn.accept_uni())
        .await
        .expect("no events stream within 5 s")
        .unwrap();
    assert_eq!(events.read_u8().await.unwrap(), uni::EVENTS);
    client.login("alice", PASSWORD).await;
    let first = next_addresses(&mut events)
        .await
        .expect("the addresses, pushed at sign-in");
    (events, first)
}

/// The next `ServerAddresses` on `events`, if one comes within 10 s.
async fn next_addresses(events: &mut iroh::endpoint::RecvStream) -> Option<Vec<SocketAddr>> {
    let event = tokio::time::timeout(Duration::from_secs(10), read_frame::<_, Event>(events))
        .await
        .ok()?
        .expect("an event that decodes");
    match event {
        Event::ServerAddresses { addrs, .. } => Some(addrs),
        other => panic!("not the server's addresses: {other:?}"),
    }
}

/// A mapping the router grants after a device signed in is pushed to it.
#[tokio::test]
async fn a_mapping_granted_later_is_pushed_to_signed_in_devices() {
    let router = FakeRouter::new();
    let server = server_asking(
        "wan-push-mapping",
        auto(PublicIp::Lookup),
        router.clone(),
        support::offline_finders().lookup,
    )
    .await;
    server.add_user("alice", PASSWORD, None).await;
    let client = server.client().await;
    let (mut events, first) = signed_in(&client).await;
    let external: SocketAddr = "198.51.100.20:40000".parse().unwrap();
    assert!(!first.contains(&external), "{first:?}");

    router.answer(Mapping::Granted("198.51.100.20:40000".parse().unwrap()));
    let pushed = next_addresses(&mut events).await.expect("a second push");
    assert_eq!(pushed.last(), Some(&external), "{pushed:?}");
    server.stop().await;
}

/// A lookup that answers only after a few tries is pushed once it does.
#[tokio::test]
async fn a_lookup_that_succeeds_later_is_pushed_to_signed_in_devices() {
    let service = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(wiremock::ResponseTemplate::new(503))
        .with_priority(5)
        .mount(&service)
        .await;
    let waits = [Duration::from_millis(100); 100];
    let server = server_finding(
        "wan-push-lookup",
        forwarded(PublicIp::Lookup, Some(LookupVia::Wtfismyip)),
        support::lookup_at(&text_url(&service), &waits),
    )
    .await;
    server.add_user("alice", PASSWORD, None).await;
    let client = server.client().await;
    let (mut events, first) = signed_in(&client).await;
    let looked_up: SocketAddr = "203.0.113.9:7420".parse().unwrap();
    assert!(!first.contains(&looked_up), "{first:?}");

    // The service answers from now on.
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("203.0.113.9"))
        .with_priority(1)
        .mount(&service)
        .await;
    let pushed = next_addresses(&mut events).await.expect("a second push");
    assert_eq!(pushed.last(), Some(&looked_up), "{pushed:?}");
    server.stop().await;
}

/// The wizard tries the router before `mode = "auto"` is chosen: the
/// router's answer comes back, for the server's own port.
#[tokio::test]
async fn the_wizard_tries_the_router_and_hears_its_answer() {
    let server = server_with("wan-try-mapping", None).await;
    let own_port = server
        .server
        .addr()
        .ip_addrs()
        .find(|a| a.is_ipv4())
        .expect("an IPv4 address")
        .port();
    let router = FakeRouter::new();
    router.answer(Mapping::Granted("203.0.113.24:7420".parse().unwrap()));
    let finders = Finders {
        mapper: router.clone(),
        ..support::offline_finders()
    };
    let admin = admin_of(&server, finders).await;
    assert_eq!(
        admin.wan_try_mapping().await.unwrap(),
        MappingStatus::Granted {
            local_port: own_port,
            external: "203.0.113.24:7420".into(),
        }
    );
    assert_eq!(router.asked(), [own_port]);

    router.answer(Mapping::Refused("the router did not map the port".into()));
    assert_eq!(
        admin.wan_try_mapping().await.unwrap(),
        MappingStatus::Refused {
            reason: "the router did not map the port".into()
        }
    );
    server.stop().await;
}

/// The wizard looks the public IP up before it is chosen.
#[tokio::test]
async fn the_wizard_looks_the_ip_up() {
    let server = server_with("wan-try-lookup", None).await;
    let service = lookup_service(200, "203.0.113.24\n").await;
    let finders = Finders {
        lookup: support::lookup_at(&text_url(&service), &[]),
        ..support::offline_finders()
    };
    let admin = admin_of(&server, finders).await;
    assert_eq!(admin.wan_lookup().await.unwrap(), "203.0.113.24");

    let failing = lookup_service(200, "10.0.0.1").await;
    let finders = Finders {
        lookup: support::lookup_at(&text_url(&failing), &[]),
        ..support::offline_finders()
    };
    let err = admin_of(&server, finders)
        .await
        .wan_lookup()
        .await
        .unwrap_err();
    let jsonrpsee::core::ClientError::Call(call) = err else {
        panic!("{err:?}");
    };
    assert_eq!(call.code(), error_code::UNAVAILABLE);
    assert!(
        call.message().contains("(10.0.0.0/8)"),
        "{}",
        call.message()
    );
    server.stop().await;
}

/// `network.wan` is one setting, set and taken out whole, checked as the
/// config file is: the web UI's refusals are the server's words.
#[tokio::test]
async fn network_wan_is_one_setting_set_and_unset_whole() {
    let (_addr, root, _dir) = crate::settings::served("wan-setting", "").await;
    let entry = root
        .setting_set(
            "network.wan".into(),
            serde_json::json!({ "static": ["203.0.113.7:7420"], "mapped_port": false }),
        )
        .await
        .unwrap();
    assert_eq!(entry.apply, spoolframe_admin_api::SettingApply::Restart);
    assert_eq!(
        entry.value,
        Some(serde_json::json!({ "static": ["203.0.113.7:7420"], "mapped_port": false }))
    );

    for (wan, says) in [
        (
            serde_json::json!({ "static": ["192.168.1.20:7420"] }),
            "(192.168.0.0/16)",
        ),
        (
            serde_json::json!({ "static": ["100.64.0.9:7420"] }),
            "(100.64.0.0/10)",
        ),
        (
            serde_json::json!({ "mapped_port": { "public_ip": "lookup", "lookup_via": false } }),
            "lookup_via = false",
        ),
    ] {
        let err = root
            .setting_set("network.wan".into(), wan.clone())
            .await
            .unwrap_err();
        let jsonrpsee::core::ClientError::Call(call) = err else {
            panic!("{err:?}");
        };
        assert_eq!(call.code(), error_code::INVALID_INPUT, "{wan}");
        assert!(call.message().contains(says), "{wan}: {}", call.message());
    }

    let unset = root.setting_unset("network.wan".into()).await.unwrap();
    assert_eq!(unset.value, None);
}
