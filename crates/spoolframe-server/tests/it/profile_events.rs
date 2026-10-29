//! Profile events (`ProfileRemoved`, `PinChanged`) over a real Iroh
//! connection: each change to a user's profile is told to the connections
//! acting as them that named `spoolframe:profile-events`, and to no
//! other. And an admin deleting a user (`admin.userDelete`): their
//! devices told, the viewers of their playlists told, and the refusals.

use crate::support;

use std::net::SocketAddr;
use std::time::Duration;

use iroh::endpoint::RecvStream;
use jsonrpsee::ws_client::WsClient;
use spoolframe_admin_api::{AdminApiClient, MeApiClient, error_code};
use spoolframe_protocol::codec::read_frame;
use spoolframe_protocol::playlists::{Kinds, PlaylistShare, Visibility};
use spoolframe_protocol::streams::uni;
use spoolframe_protocol::{Event, Reply, Request, Response, SessionId};
use spoolframe_protocol::{Feature, Features};
use spoolframe_server::admin::{self, Admin, Origins, TrustedProxies};
use spoolframe_server::passkeys::Passkeys;
use spoolframe_types::Role;
use support::{TestClient, TestServer};
use tokio::io::AsyncReadExt;

const PASSWORD: &str = "correct horse";
const WEB: &str = "https://media.example.org";
/// How long a connection that is to be told nothing is watched.
const A_WHILE: Duration = Duration::from_millis(1500);

/// A server with alice, bob and root, an admin.
async fn server(test: &str) -> TestServer {
    let server = TestServer::start(test).await;
    for name in ["alice", "bob", "root"] {
        server.add_user(name, PASSWORD, None).await;
    }
    server.store.set_role("root", Role::Admin).await.unwrap();
    server
}

/// The admin API beside `server`, deleting playlists and downloads as the
/// server's own do.
async fn web(server: &TestServer) -> SocketAddr {
    let origins = Origins::Exactly(WEB.to_owned());
    let (addr, handle) = admin::serve(
        "127.0.0.1:0".parse().unwrap(),
        Admin {
            passkeys: std::sync::Arc::new(Passkeys::new(&origins, &server.server_id)),
            library: Some(spoolframe_server::mcp::Library {
                catalog: server.catalog.clone(),
                watch: server.watch.clone(),
                playlists: server.playlists.clone(),
            }),
            downloads: Some(server.downloads.clone()),
            ..crate::admin::admin(server.auth.clone(), None)
        },
        origins,
        TrustedProxies::default(),
    )
    .await
    .unwrap();
    std::mem::forget(handle);
    addr
}

/// `username` signed in to the web UI just now.
async fn on_the_web(web: SocketAddr, username: &str) -> WsClient {
    let login = crate::admin::anonymous(web)
        .await
        .login(crate::admin::creds(username, PASSWORD))
        .await
        .unwrap();
    crate::admin::with_token(web, &login.token).await
}

/// A device signed in as `username`, and its events stream.
struct Device {
    client: TestClient,
    session: SessionId,
    events: RecvStream,
}

async fn device(server: &TestServer, username: &str) -> Device {
    device_naming(server, username, Features::only(Feature::ProfileEvents)).await
}

/// [`device`], its `Hello` naming the regular features `named`.
async fn device_naming(server: &TestServer, username: &str, named: Features) -> Device {
    let client = server.client().await;
    let info = client.hello_with_features(named).await;
    assert_eq!(info.features, named, "the server has them all");
    let mut events = tokio::time::timeout(Duration::from_secs(5), client.conn.accept_uni())
        .await
        .expect("no events stream within 5 s")
        .unwrap();
    assert_eq!(events.read_u8().await.unwrap(), uni::EVENTS);
    let session = client.login(username, PASSWORD).await;
    // Noted as acting as the user from its first request.
    ok(&client, &session, Request::WhoAmI).await;
    Device {
        client,
        session,
        events,
    }
}

async fn ok(client: &TestClient, session: &SessionId, request: Request) -> Reply {
    match client.call(Some(session), request).await {
        Response::Ok(reply) => reply,
        Response::Err(e) => panic!("refused: {e:?}"),
    }
}

async fn user_id(device: &Device) -> String {
    match ok(&device.client, &device.session, Request::WhoAmI).await {
        Reply::WhoAmI(who) => who.user.user_id,
        other => panic!("{other:?}"),
    }
}

/// The next profile event on `events` within `wait`, past the others.
async fn profile_event(events: &mut RecvStream, wait: Duration) -> Option<Event> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let read = tokio::time::timeout_at(deadline, read_frame::<_, Event>(events)).await;
        match read {
            Err(_) | Ok(Err(_)) => return None,
            Ok(Ok(event @ (Event::ProfileRemoved { .. } | Event::PinChanged { .. }))) => {
                return Some(event);
            }
            Ok(Ok(_)) => {}
        }
    }
}

/// The next profile event `device` is told. Waited for generously: a
/// full suite's load can hold one up for seconds.
async fn told(device: &mut Device) -> Event {
    profile_event(&mut device.events, Duration::from_secs(30))
        .await
        .expect("no profile event within 30 s")
}

async fn told_nothing(device: &mut Device) {
    let event = profile_event(&mut device.events, A_WHILE).await;
    assert_eq!(event, None);
}

#[tokio::test]
async fn disabling_a_user_tells_their_devices_alone() {
    let server = server("profile-events-disable").await;
    let web = web(&server).await;
    let mut tv = device(&server, "alice").await;
    let mut phone = device(&server, "alice").await;
    let mut bobs = device(&server, "bob").await;
    // An app that did not name the feature is sent none of it.
    let mut older = device_naming(&server, "alice", Features::empty()).await;
    let alice = user_id(&tv).await;

    on_the_web(web, "root")
        .await
        .user_disable("alice".into())
        .await
        .unwrap();

    let removed = Event::ProfileRemoved { user: alice };
    assert_eq!(told(&mut tv).await, removed);
    assert_eq!(told(&mut phone).await, removed);
    told_nothing(&mut bobs).await;
    told_nothing(&mut older).await;
    server.stop().await;
}

/// A PIN set in an app is told to the user's other devices; one an admin
/// clears, to all of them.
#[tokio::test]
async fn a_pin_changed_is_told_to_the_user_s_other_devices() {
    let server = server("profile-events-pin").await;
    let web = web(&server).await;
    let mut tv = device(&server, "alice").await;
    let mut phone = device(&server, "alice").await;
    let mut bobs = device(&server, "bob").await;
    let alice = user_id(&tv).await;

    let set = Request::SetPin {
        pin: "4321".into(),
        proof: None,
    };
    ok(&tv.client, &tv.session, set).await;
    assert_eq!(
        told(&mut phone).await,
        Event::PinChanged {
            user: alice.clone(),
            required: true
        }
    );
    told_nothing(&mut tv).await;

    on_the_web(web, "root")
        .await
        .user_clear_pin("alice".into())
        .await
        .unwrap();
    let cleared = Event::PinChanged {
        user: alice,
        required: false,
    };
    assert_eq!(told(&mut tv).await, cleared);
    assert_eq!(told(&mut phone).await, cleared);
    told_nothing(&mut bobs).await;
    server.stop().await;
}

/// Who turns the PIN on for one of the user's devices.
enum Asker {
    /// `admin.userSetPinRequired`.
    Admin,
    /// The user on the web: `me.setPinRequired`.
    User,
    /// The user in an app: `SetPinRequired` from another device.
    App,
}

/// `asker` makes alice's phone ask for her PIN: the phone is told, her
/// TV is not. Not by waiting for nothing to come to the TV: an admin then
/// clears the PIN, which every device of hers is told, and that is the
/// first the TV hears.
async fn pin_required_is_told_to_that_device_alone(test: &str, asker: Asker) {
    let server = server(test).await;
    let web = web(&server).await;
    let mut tv = device(&server, "alice").await;
    let mut phone = device(&server, "alice").await;
    let alice = user_id(&tv).await;
    let pin_changed = |required| Event::PinChanged {
        user: alice.clone(),
        required,
    };
    let set = Request::SetPin {
        pin: "4321".into(),
        proof: None,
    };
    // Told to the phone; the TV set it.
    ok(&tv.client, &tv.session, set).await;
    assert_eq!(told(&mut phone).await, pin_changed(true));

    let endpoint = phone.client.endpoint.id().to_string();
    match asker {
        Asker::Admin => on_the_web(web, "root")
            .await
            .user_set_pin_required("alice".into(), endpoint, true)
            .await
            .unwrap(),
        Asker::User => on_the_web(web, "alice")
            .await
            .set_pin_required(endpoint, true)
            .await
            .unwrap(),
        Asker::App => {
            let asked = Request::SetPinRequired {
                device: Some(endpoint),
                required: true,
            };
            ok(&tv.client, &tv.session, asked).await;
        }
    }
    assert_eq!(told(&mut phone).await, pin_changed(true));

    on_the_web(web, "root")
        .await
        .user_clear_pin("alice".into())
        .await
        .unwrap();
    assert_eq!(
        told(&mut tv).await,
        pin_changed(false),
        "the TV was told of the phone's PIN"
    );
    assert_eq!(told(&mut phone).await, pin_changed(false));
    server.stop().await;
}

#[tokio::test]
async fn a_device_an_admin_makes_ask_for_the_pin_is_told_alone() {
    pin_required_is_told_to_that_device_alone("profile-events-pin-required-admin", Asker::Admin)
        .await;
}

#[tokio::test]
async fn a_device_the_user_makes_ask_for_the_pin_on_the_web_is_told_alone() {
    pin_required_is_told_to_that_device_alone("profile-events-pin-required-web", Asker::User).await;
}

#[tokio::test]
async fn a_device_the_user_makes_ask_for_the_pin_in_an_app_is_told_alone() {
    pin_required_is_told_to_that_device_alone("profile-events-pin-required-app", Asker::App).await;
}

/// A device an admin forgets for a user is told; the user's others are
/// not.
#[tokio::test]
async fn a_device_forgotten_is_told_alone() {
    let server = server("profile-events-forget").await;
    let web = web(&server).await;
    let mut tv = device(&server, "alice").await;
    let mut phone = device(&server, "alice").await;
    let alice = user_id(&tv).await;

    on_the_web(web, "root")
        .await
        .user_forget_device("alice".into(), phone.client.endpoint.id().to_string())
        .await
        .unwrap();

    assert_eq!(
        told(&mut phone).await,
        Event::ProfileRemoved { user: alice }
    );
    told_nothing(&mut tv).await;
    server.stop().await;
}

const EPISODES: Kinds = Kinds {
    movies: false,
    episodes: true,
    tracks: false,
};

/// A deleted user's devices are told, and so is whoever saw a playlist
/// of theirs, a server-wide one too; their account is gone.
#[tokio::test]
async fn deleting_a_user_tells_their_devices_and_their_playlists_viewers() {
    let server = server("profile-events-delete").await;
    // An admin, to make a server-wide playlist.
    server.store.set_role("alice", Role::Admin).await.unwrap();
    let web = web(&server).await;
    let mut tv = device(&server, "alice").await;
    let mut bobs = device(&server, "bob").await;
    let alice = user_id(&tv).await;
    let bob = user_id(&bobs).await;
    let made = ok(
        &tv.client,
        &tv.session,
        Request::CreatePlaylist {
            name: "Heists".into(),
            kinds: EPISODES,
            visibility: Visibility::Shared {
                with: vec![PlaylistShare::new(bob, false)],
            },
        },
    )
    .await;
    let Reply::PlaylistInfo(made) = made else {
        panic!("{made:?}")
    };
    let everyones = ok(
        &tv.client,
        &tv.session,
        Request::CreatePlaylist {
            name: "Classics".into(),
            kinds: EPISODES,
            visibility: Visibility::ServerWide,
        },
    )
    .await;
    let Reply::PlaylistInfo(everyones) = everyones else {
        panic!("{everyones:?}")
    };
    let mut unseen = vec![made.id.clone(), everyones.id.clone()];

    on_the_web(web, "root")
        .await
        .user_delete("alice".into())
        .await
        .unwrap();

    assert_eq!(told(&mut tv).await, Event::ProfileRemoved { user: alice });
    loop {
        let event = tokio::time::timeout(
            Duration::from_secs(5),
            read_frame::<_, Event>(&mut bobs.events),
        )
        .await
        .expect("bob is told within 5 s")
        .unwrap();
        assert!(
            !matches!(event, Event::ProfileRemoved { .. }),
            "bob is told of alice: {event:?}"
        );
        if let Event::PlaylistGone { id } = event {
            unseen.retain(|unseen| *unseen != id);
        }
        if unseen.is_empty() {
            break;
        }
    }
    assert!(server.store.user_by_name("alice").await.unwrap().is_none());
    let gone = Request::GetPlaylist {
        id: made.id,
        offset: 0,
        limit: 10,
    };
    assert!(
        matches!(
            bobs.client.call(Some(&bobs.session), gone).await,
            Response::Err(spoolframe_protocol::Error::NotFound { .. })
        ),
        "bob sees it no more"
    );
    assert!(
        server
            .store
            .playlist(&everyones.id)
            .await
            .unwrap()
            .is_none()
    );
    server.stop().await;
}

/// A delete that fails leaves the user's playlists as they were, and
/// tells their viewers nothing: the playlists go in the delete's own
/// transaction.
#[tokio::test]
async fn a_failed_delete_keeps_the_user_s_playlists() {
    let server = server("profile-events-delete-fails").await;
    let web = web(&server).await;
    let tv = device(&server, "alice").await;
    let mut bobs = device(&server, "bob").await;
    let bob = user_id(&bobs).await;
    let made = ok(
        &tv.client,
        &tv.session,
        Request::CreatePlaylist {
            name: "Heists".into(),
            kinds: EPISODES,
            visibility: Visibility::Shared {
                with: vec![PlaylistShare::new(bob, false)],
            },
        },
    )
    .await;
    let Reply::PlaylistInfo(made) = made else {
        panic!("{made:?}")
    };
    // The user's own row is the last the delete takes: it fails there, as
    // on a full disk.
    let pool = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::new().filename(server.dir.join("spoolframe.db")),
    )
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER users_undeletable BEFORE DELETE ON users \
         BEGIN SELECT RAISE(ABORT, 'database or disk is full'); END",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    assert!(
        on_the_web(web, "root")
            .await
            .user_delete("alice".into())
            .await
            .is_err()
    );

    assert!(server.store.user_by_name("alice").await.unwrap().is_some());
    assert!(
        server.store.playlist(&made.id).await.unwrap().is_some(),
        "the playlist outlived a delete that failed"
    );
    let deadline = tokio::time::Instant::now() + A_WHILE;
    while let Ok(Ok(event)) =
        tokio::time::timeout_at(deadline, read_frame::<_, Event>(&mut bobs.events)).await
    {
        assert!(
            !matches!(event, Event::PlaylistGone { .. }),
            "bob is told of a playlist still there: {event:?}"
        );
    }
    server.stop().await;
}

/// An admin cannot delete themself; a consumer cannot delete anyone.
#[tokio::test]
async fn deleting_a_user_is_refused_for_oneself_and_for_a_consumer() {
    let server = server("profile-events-delete-refused").await;
    let web = web(&server).await;
    let root = on_the_web(web, "root").await;
    assert_eq!(
        crate::admin::code(root.user_delete("root".into()).await.unwrap_err()),
        error_code::INVALID_INPUT
    );
    let bob = on_the_web(web, "bob").await;
    assert_eq!(
        crate::admin::code(bob.user_delete("alice".into()).await.unwrap_err()),
        error_code::FORBIDDEN
    );
    assert!(server.store.user_by_name("root").await.unwrap().is_some());
    assert!(server.store.user_by_name("alice").await.unwrap().is_some());
    server.stop().await;
}
