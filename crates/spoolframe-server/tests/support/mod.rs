//! A server with a fake catalog, and in-process Iroh client endpoints that
//! talk `spoolframe/client/1` to it, relays off. Shared by the integration
//! tests (and P1-13's LAN test).

// Each test file uses part of this module.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use iroh::endpoint::{Connection, presets};
use iroh::{Endpoint, RelayMode};
use serde::Serialize;
use spoolframe_catalog::{Catalog, CatalogError, Image, ItemPage, ItemWithChildren, Page};
use spoolframe_media::Prober;
use spoolframe_protocol::codec::{read_frame, write_frame};
use spoolframe_protocol::streams::bi;
use spoolframe_protocol::{ArtworkRequest, ArtworkResponse, ArtworkSize};
use spoolframe_protocol::{
    CLIENT_ALPN, Hello, LoginPassword, PROTOCOL_VERSION, Reply, Request, RequestEnvelope, Response,
    SessionId,
};
use spoolframe_protocol::{MediaRead, MediaReadResponse, PlaybackHandle};
use spoolframe_server::artwork::ArtworkCache;
use spoolframe_server::auth::{Auth, hash_password};
use spoolframe_server::identity;
use spoolframe_server::network::{EndpointOptions, Relay, Server, Services};
use spoolframe_server::playback::Playbacks;
use spoolframe_server::store::Store;
use spoolframe_server::watch::{Watch, WatchRules};
use spoolframe_types::{
    ArtworkId, ClientCapabilities, Episode, Item, Library, LibraryId, LibraryKind, MediaId,
    MediaKind, Movie, Namespace, PlaybackState, Season, Show,
};
use tokio::io::AsyncWriteExt;

/// The fake catalog's one image.
pub const POSTER: &str = "/library/metadata/1/thumb/1";

pub fn poster() -> ArtworkId {
    ArtworkId::new(Namespace::Plex, POSTER).unwrap()
}

/// One library holding one movie, `plex:movie:1`.
#[derive(Debug)]
pub struct FakeCatalog {
    /// The movie's file, once a test writes it.
    media: PathBuf,
}

pub fn movie() -> Item {
    Item::Movie(Movie {
        id: MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap(),
        title: "Alien".into(),
        year: Some(1979),
        summary: None,
        duration: Some(Duration::from_secs(7020)),
        artwork: Vec::new(),
        playback: PlaybackState::default(),
    })
}

fn media(kind: MediaKind, key: &str) -> MediaId {
    MediaId::new(Namespace::Plex, kind, key).unwrap()
}

/// A show, `plex:show:10`: specials (season 0, episode 21), season 1
/// (episodes 12 and 13) and season 2 (episode 15).
pub fn show() -> Item {
    Item::Show(Show {
        id: media(MediaKind::Show, "10"),
        title: "The Expanse".into(),
        year: Some(2015),
        summary: None,
        artwork: Vec::new(),
    })
}

fn season(key: &str, number: u32) -> Item {
    Item::Season(Season {
        id: media(MediaKind::Season, key),
        show_id: media(MediaKind::Show, "10"),
        season: number,
        title: format!("Season {number}"),
        artwork: Vec::new(),
    })
}

/// Episode `key`: number `episode` of season `season` (`season_key`).
pub fn episode(key: &str, season_key: &str, season: u32, episode: u32) -> Item {
    Item::Episode(Episode {
        id: media(MediaKind::Episode, key),
        show_id: media(MediaKind::Show, "10"),
        season_id: media(MediaKind::Season, season_key),
        season,
        episode,
        title: format!("S{season}E{episode}"),
        summary: None,
        duration: Some(Duration::from_secs(2640)),
        artwork: Vec::new(),
        playback: PlaybackState::default(),
    })
}

/// Every item the fake catalog has, with its children.
fn tree() -> Vec<(Item, Vec<Item>)> {
    vec![
        (movie(), Vec::new()),
        (
            show(),
            vec![season("20", 0), season("11", 1), season("14", 2)],
        ),
        (season("20", 0), vec![episode("21", "20", 0, 1)]),
        (
            season("11", 1),
            vec![episode("12", "11", 1, 1), episode("13", "11", 1, 2)],
        ),
        (season("14", 2), vec![episode("15", "14", 2, 1)]),
        (episode("21", "20", 0, 1), Vec::new()),
        (episode("12", "11", 1, 1), Vec::new()),
        (episode("13", "11", 1, 2), Vec::new()),
        (episode("15", "14", 2, 1), Vec::new()),
    ]
}

pub fn library() -> Library {
    Library {
        id: LibraryId::new(Namespace::Plex, "1").unwrap(),
        title: "Movies".into(),
        kind: LibraryKind::Movies,
    }
}

#[async_trait]
impl Catalog for FakeCatalog {
    async fn libraries(&self) -> Result<Vec<Library>, CatalogError> {
        Ok(vec![library()])
    }

    async fn items(&self, library: &LibraryId, page: Page) -> Result<ItemPage, CatalogError> {
        if library.key() != "1" {
            return Err(CatalogError::NotFound(library.to_string()));
        }
        let items = if page.offset == 0 && page.limit > 0 {
            vec![movie()]
        } else {
            Vec::new()
        };
        Ok(ItemPage { items, total: 1 })
    }

    async fn item(&self, id: &MediaId) -> Result<ItemWithChildren, CatalogError> {
        tree()
            .into_iter()
            .find(|(item, _)| item.id() == id)
            .map(|(item, children)| ItemWithChildren {
                external_ids: vec![format!("test://{}", item.id().key())],
                item,
                children,
            })
            .ok_or_else(|| CatalogError::NotFound(id.to_string()))
    }

    async fn search(&self, query: &str, _limit: u32) -> Result<Vec<Item>, CatalogError> {
        Ok(
            if "alien".contains(&query.to_lowercase()) && !query.is_empty() {
                vec![movie()]
            } else {
                Vec::new()
            },
        )
    }

    async fn media_path(&self, id: &MediaId) -> Result<PathBuf, CatalogError> {
        if id == movie().id() && self.media.exists() {
            Ok(self.media.clone())
        } else {
            Err(CatalogError::NotFound(id.to_string()))
        }
    }

    async fn recently_added(&self, _limit: u32) -> Result<Vec<Item>, CatalogError> {
        Ok(vec![movie()])
    }

    /// A 400×200 PNG at the movie's poster path; nothing else.
    async fn artwork(&self, id: &ArtworkId) -> Result<Image, CatalogError> {
        if id.key() != POSTER {
            return Err(CatalogError::NotFound(id.to_string()));
        }
        let img = image::RgbImage::from_pixel(400, 200, image::Rgb([10, 120, 200]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        Ok(Image {
            bytes: out.into_inner(),
            content_type: "image/png".into(),
        })
    }
}

/// Short playback timings, so tests can see a handle expire or a session
/// re-checked without waiting minutes.
pub const PLAYBACK_IDLE: Duration = Duration::from_secs(2);
pub const PLAYBACK_RECHECK: Duration = Duration::from_millis(300);

/// A stand-in ffprobe answering with recorded output: the forced-subtitle
/// MKV (matroska, hevc, eac3, subrip) for media, the SRT for `.srt` files.
fn fake_ffprobe(dir: &Path) -> PathBuf {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../spoolframe-media/tests/fixtures");
    let script = dir.join("bin").join("ffprobe");
    std::fs::create_dir_all(script.parent().unwrap()).unwrap();
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nfor path; do :; done\ncase \"$path\" in *.srt) cat '{}' ;; *) cat '{}' ;; esac\n",
            fixtures.join("external-srt.json").display(),
            fixtures.join("mkv-forced-subs.json").display(),
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// What the stand-in media needs: the formats of the forced-subtitle MKV.
pub fn mkv_capabilities() -> ClientCapabilities {
    use spoolframe_types::{AudioCodec, Container, SubtitleFormat, VideoCodec};
    ClientCapabilities {
        video: [VideoCodec::Hevc].into(),
        audio: [AudioCodec::Eac3].into(),
        containers: [Container::Matroska].into(),
        subtitles: [SubtitleFormat::Srt].into(),
    }
}

/// A running server on loopback, with its database at hand.
pub struct TestServer {
    pub server: Server,
    pub store: Store,
    pub media: PathBuf,
    pub dir: PathBuf,
}

impl TestServer {
    /// A fresh server whose state lives in a directory named for `test`.
    pub async fn start(test: &str) -> Self {
        // Per test file too: two files may use the same test name.
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("servers")
            .join(env!("CARGO_CRATE_NAME"))
            .join(test);
        let _ = std::fs::remove_dir_all(&dir);
        Self::start_in(dir).await
    }

    /// A server on an existing state directory, as after a restart.
    pub async fn start_in(dir: PathBuf) -> Self {
        let key = identity::load_or_create(&dir).unwrap();
        let store = Store::open(&dir).await.unwrap();
        let media = dir.join("media").join("movie.mkv");
        let catalog: Arc<dyn Catalog> = Arc::new(FakeCatalog {
            media: media.clone(),
        });
        let artwork =
            ArtworkCache::open(&dir.join("cache/artwork"), 64 << 20, catalog.clone()).unwrap();
        let prober = Arc::new(Prober::new(fake_ffprobe(&dir), Prober::DEFAULT_TIMEOUT));
        let watch = Arc::new(Watch::new(
            store.clone(),
            catalog.clone(),
            prober.clone(),
            WatchRules::default(),
        ));
        let services = Services {
            auth: Auth::new(store.clone()),
            watch,
            catalog,
            prober,
            artwork: Arc::new(artwork),
            playbacks: Arc::new(Playbacks::new(PLAYBACK_IDLE, PLAYBACK_RECHECK)),
        };
        let options = EndpointOptions {
            port: None,
            relay: Relay::Disabled,
        };
        let server = Server::start(key, &options, "Home Media", services)
            .await
            .unwrap();
        Self {
            server,
            store,
            dir,
            media,
        }
    }

    /// Write the movie's file: `size` bytes of a fixed pattern, returned
    /// so reads can be compared.
    pub fn write_media(&self, size: usize) -> Vec<u8> {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let bytes: Vec<u8> = (0..size)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        std::fs::create_dir_all(self.media.parent().unwrap()).unwrap();
        std::fs::write(&self.media, &bytes).unwrap();
        bytes
    }

    /// Write a subtitle file next to the movie; returns its bytes.
    pub fn write_sidecar(&self) -> Vec<u8> {
        let bytes = b"1\n00:00:01,000 --> 00:00:03,000\nHello there.\n".to_vec();
        std::fs::write(self.media.with_extension("en.srt"), &bytes).unwrap();
        bytes
    }

    pub async fn add_user(&self, username: &str, password: &str, display_name: Option<&str>) {
        let phc = hash_password(password).await.unwrap();
        self.store
            .add_user(username, display_name, &phc, false)
            .await
            .unwrap();
    }

    /// A new client endpoint (its own `EndpointId`) connected to the
    /// server.
    pub async fn client(&self) -> TestClient {
        let endpoint = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .bind()
            .await
            .unwrap();
        let conn = endpoint
            .connect(self.server.addr(), CLIENT_ALPN)
            .await
            .unwrap();
        TestClient { endpoint, conn }
    }

    pub async fn stop(self) {
        self.server.shutdown().await;
    }
}

#[derive(Clone)]
pub struct TestClient {
    pub endpoint: Endpoint,
    pub conn: Connection,
}

impl TestClient {
    /// Send any serializable envelope as one RPC.
    pub async fn send<T: Serialize>(&self, envelope: &T) -> Response {
        let (mut send, mut recv) = self.conn.open_bi().await.unwrap();
        send.write_u8(bi::RPC).await.unwrap();
        write_frame(&mut send, envelope).await.unwrap();
        send.finish().unwrap();
        read_frame(&mut recv).await.unwrap()
    }

    pub async fn call(&self, session: Option<&SessionId>, request: Request) -> Response {
        self.send(&RequestEnvelope {
            session: session.cloned(),
            request,
        })
        .await
    }

    /// `Hello`, as a device named `device_name`.
    pub async fn hello_as(&self, device_name: Option<&str>) -> Response {
        self.hello_with(device_name, None).await
    }

    /// `Hello` with everything a client can say.
    pub async fn hello_with(
        &self,
        device_name: Option<&str>,
        capabilities: Option<ClientCapabilities>,
    ) -> Response {
        self.call(
            None,
            Request::Hello(Hello {
                protocol_version: PROTOCOL_VERSION,
                client_name: "test".into(),
                client_version: "0".into(),
                device_name: device_name.map(str::to_owned),
                capabilities,
            }),
        )
        .await
    }

    pub async fn hello(&self) {
        assert!(matches!(
            self.hello_as(None).await,
            Response::Ok(Reply::ServerInfo(_))
        ));
    }

    /// Log in and return the session.
    pub async fn login(&self, username: &str, password: &str) -> SessionId {
        let request = Request::LoginPassword(LoginPassword {
            username: username.into(),
            password: password.into(),
        });
        match self.call(None, request).await {
            Response::Ok(Reply::LoggedIn { session }) => session,
            other => panic!("login as {username} failed: {other:?}"),
        }
    }

    /// One image over the artwork stream: the response frame and the
    /// bytes after it.
    pub async fn artwork(
        &self,
        session: &SessionId,
        id: ArtworkId,
        size: ArtworkSize,
    ) -> (ArtworkResponse, Vec<u8>) {
        let (mut send, mut recv) = self.conn.open_bi().await.unwrap();
        send.write_u8(bi::ARTWORK).await.unwrap();
        let request = ArtworkRequest {
            session: session.clone(),
            id,
            size,
        };
        write_frame(&mut send, &request).await.unwrap();
        send.finish().unwrap();
        let response: ArtworkResponse = read_frame(&mut recv).await.unwrap();
        let bytes = recv.read_to_end(64 << 20).await.unwrap();
        (response, bytes)
    }

    /// One range read on a MEDIA stream: the response frame and the bytes
    /// after it.
    pub async fn read(
        &self,
        session: &SessionId,
        handle: &PlaybackHandle,
        offset: u64,
        length: u64,
    ) -> (MediaReadResponse, Vec<u8>) {
        let (mut send, mut recv) = self.conn.open_bi().await.unwrap();
        send.write_u8(bi::MEDIA).await.unwrap();
        let read = MediaRead {
            session: session.clone(),
            handle: handle.clone(),
            offset,
            length,
        };
        write_frame(&mut send, &read).await.unwrap();
        send.finish().unwrap();
        let response: MediaReadResponse = read_frame(&mut recv).await.unwrap();
        let bytes = recv.read_to_end(64 << 20).await.unwrap();
        (response, bytes)
    }

    pub async fn close(self) {
        self.conn.close(0u32.into(), b"done");
        self.endpoint.close().await;
    }
}
