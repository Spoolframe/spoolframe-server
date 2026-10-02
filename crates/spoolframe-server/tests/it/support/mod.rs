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
use spoolframe_media::encoders::{FfmpegCommand, TranscodeSettings};
use spoolframe_media::ocr::Ocr;
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
use spoolframe_server::encode::JobLimits;
use spoolframe_server::encode::job::Jobs;
use spoolframe_server::identity;
use spoolframe_server::network::{EndpointOptions, Relay, Server, Services};
use spoolframe_server::playback::Playbacks;
use spoolframe_server::store::Store;
use spoolframe_server::subtitles::{OcrSettings, Subtitles};
use spoolframe_server::transcode::Transcoding;
use spoolframe_server::watch::{Watch, WatchRules};
use spoolframe_types::{
    ArtworkId, ClientCapabilities, Episode, Item, Library, LibraryId, LibraryKind, MediaId,
    MediaKind, Movie, Namespace, PlaybackState, Season, Show,
};
use tokio::io::AsyncWriteExt;

/// Transcoding that has not detected anything: the software baseline.
/// No test runs a real ffmpeg.
pub fn transcoding() -> Arc<Transcoding> {
    Arc::new(Transcoding::new(
        Box::new(FfmpegCommand::new("/nonexistent/ffmpeg")),
        TranscodeSettings::default(),
        std::env::temp_dir(),
    ))
}

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
        show_title: Some("The Expanse".into()),
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
        show_title: Some("The Expanse".into()),
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
        let path = if id == movie().id() {
            self.media.clone()
        } else {
            episode_media(&self.media, id)
        };
        if path.exists() {
            Ok(path)
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
/// Where an episode's file goes, next to the movie's. The stand-in ffprobe
/// describes it as Brooklyn Nine-Nine S07E01 (P1-20's fixture).
fn episode_media(movie: &Path, id: &MediaId) -> PathBuf {
    movie.with_file_name(format!("episode-{}.b99.mkv", id.key()))
}

fn fake_ffprobe(dir: &Path) -> PathBuf {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../spoolframe-media/tests/fixtures");
    let script = dir.join("bin").join("ffprobe");
    std::fs::create_dir_all(script.parent().unwrap()).unwrap();
    write_script(
        &script,
        &format!(
            "#!/bin/sh\nfor path; do :; done\ncase \"$path\" in *.srt) cat '{}' ;; *.b99.mkv) cat '{}' ;; *) cat '{}' ;; esac\n",
            fixtures.join("external-srt.json").display(),
            fixtures.join("b99-s07e01.json").display(),
            fixtures.join("mkv-forced-subs.json").display(),
        ),
    );
    script
}

/// A Linux-client-like device that takes a progressive stream: H.264 and
/// AAC in Matroska, so an AC-3 file needs its audio converted.
pub fn progressive_capabilities() -> ClientCapabilities {
    use spoolframe_types::{AudioCodec, Container, Delivery, VideoCodec};
    ClientCapabilities {
        video: [VideoCodec::H264].into(),
        audio: [AudioCodec::Aac].into(),
        containers: [Container::Matroska].into(),
        delivery: vec![Delivery::Progressive],
        ..ClientCapabilities::default()
    }
}

/// An Apple-like device that takes HLS: H.264 and AAC in MP4, so an AC-3
/// file has its audio converted and its video copied.
pub fn hls_capabilities() -> ClientCapabilities {
    use spoolframe_types::{AudioCodec, Container, Delivery, VideoCodec};
    ClientCapabilities {
        video: [VideoCodec::H264].into(),
        audio: [AudioCodec::Aac].into(),
        containers: [Container::Mp4, Container::MpegTs].into(),
        delivery: vec![Delivery::Hls],
        ..ClientCapabilities::default()
    }
}

/// What the stand-in media needs: the formats of the forced-subtitle MKV.
pub fn mkv_capabilities() -> ClientCapabilities {
    use spoolframe_types::{AudioCodec, Container, SubtitleFormat, VideoCodec};
    ClientCapabilities {
        video: [VideoCodec::Hevc].into(),
        audio: [AudioCodec::Eac3].into(),
        containers: [Container::Matroska].into(),
        subtitles: [SubtitleFormat::Srt].into(),
        ..ClientCapabilities::default()
    }
}

/// How a [`TestServer`] runs. Off the LAN by default: only the tests about
/// discovery announce themselves.
#[derive(Debug, Clone)]
pub struct ServerOptions {
    pub name: String,
    pub port: Option<u16>,
    pub mdns: bool,
    /// Real ffprobe and ffmpeg, on a file made with [`TestServer::generate_media`],
    /// for converted playback (P2-03). Off: the stand-in ffprobe and its
    /// fixtures.
    pub real_media: bool,
    pub max_jobs: usize,
    pub read_ahead: Duration,
    /// The encoder jobs run: real ffmpeg, or a stand-in script.
    pub ffmpeg: PathBuf,
    /// Read bitmap subtitles as text with real tesseract (P2-07). Off: the
    /// server cannot.
    pub ocr: bool,
    /// How long failed logins and PINs count; tests of many failures
    /// shorten it.
    pub failure_window: Duration,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            name: "Home Media".into(),
            port: None,
            mdns: false,
            real_media: false,
            max_jobs: 2,
            read_ahead: Duration::from_secs(5),
            ffmpeg: PathBuf::from("ffmpeg"),
            ocr: false,
            failure_window: spoolframe_server::auth::FAILURE_WINDOW,
        }
    }
}

/// A test's state directory: a tmpdir under a random name, so tests
/// running at once (in this run or another) never share one. It goes when
/// the last clone is dropped; a restarted server keeps its clone.
#[derive(Clone)]
pub struct StateDir {
    path: PathBuf,
    _tmp: Arc<spoolframe_test_dir::TestDir>,
}

impl std::ops::Deref for StateDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for StateDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

/// A new, empty state directory for `test`.
pub async fn fresh_dir(test: &str) -> StateDir {
    let tmp =
        spoolframe_test_dir::TestDir::new(&format!("spoolframe-{}-{test}", test_module())).await;
    StateDir {
        path: tmp.to_path_buf(),
        _tmp: Arc::new(tmp),
    }
}

/// The module of the running test, so two modules may use the same
/// `test` name. libtest names each test's thread by its path,
/// `sessions::two_users`, and a `#[tokio::test]` body runs on that thread.
fn test_module() -> String {
    std::thread::current()
        .name()
        .and_then(|name| name.split_once("::"))
        .map_or_else(|| "it".to_owned(), |(module, _)| module.to_owned())
}

/// A running server on loopback, with its database at hand.
pub struct TestServer {
    pub server: Server,
    pub store: Store,
    pub media: PathBuf,
    pub dir: StateDir,
}

impl TestServer {
    /// A fresh server whose state lives in a directory named for `test`.
    pub async fn start(test: &str) -> Self {
        Self::start_in(fresh_dir(test).await).await
    }

    /// A server on an existing state directory, as after a restart.
    pub async fn start_in(dir: StateDir) -> Self {
        Self::start_with(dir, ServerOptions::default()).await
    }

    /// A server on `dir` (its key and database), run as `options` say.
    pub async fn start_with(dir: StateDir, options: ServerOptions) -> Self {
        let key = identity::load_or_create(&dir).unwrap();
        let store = Store::open(&dir).await.unwrap();
        let media = dir.join("media").join("movie.mkv");
        let catalog: Arc<dyn Catalog> = Arc::new(FakeCatalog {
            media: media.clone(),
        });
        let artwork =
            ArtworkCache::open(&dir.join("cache/artwork"), 64 << 20, catalog.clone()).unwrap();
        let ffprobe = if options.real_media {
            PathBuf::from("ffprobe")
        } else {
            fake_ffprobe(&dir)
        };
        let prober = Arc::new(Prober::new(ffprobe, Prober::DEFAULT_TIMEOUT));
        let jobs = Jobs::new(
            JobLimits {
                max_jobs: options.max_jobs,
                scratch_dir: dir.join("cache").join("transcode"),
                scratch_limit_bytes: 1 << 30,
                read_ahead: options.read_ahead,
            },
            options.ffmpeg.clone(),
        )
        .unwrap();
        let ocr = if options.ocr {
            Ocr::detect(Path::new("tesseract")).await
        } else {
            Ocr::disabled()
        };
        let subtitles = Subtitles::new(
            ocr,
            PathBuf::from("ffmpeg"),
            &OcrSettings {
                enabled: true,
                threads: 2,
                cache_dir: dir.join("cache").join("subtitles"),
            },
        )
        .unwrap();
        let watch = Arc::new(Watch::new(
            store.clone(),
            catalog.clone(),
            prober.clone(),
            WatchRules::default(),
        ));
        let services = Services {
            auth: Auth::with_failure_window(store.clone(), options.failure_window),
            watch,
            catalog,
            prober,
            artwork: Arc::new(artwork),
            playbacks: Arc::new(Playbacks::new(PLAYBACK_IDLE, PLAYBACK_RECHECK)),
            transcoding: transcoding(),
            jobs: Arc::new(jobs),
            subtitles: Arc::new(subtitles),
        };
        let endpoint = EndpointOptions {
            port: options.port,
            relay: Relay::Disabled,
            mdns: options.mdns,
        };
        let server = Server::start(key, &endpoint, &options.name, services)
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

    /// Write episode `key`'s file; ffprobe describes it as B99 S07E01.
    pub fn write_episode(&self, key: &str) {
        let id = MediaId::new(Namespace::Plex, MediaKind::Episode, key).unwrap();
        let path = episode_media(&self.media, &id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"stand-in").unwrap();
    }

    /// Make the movie's file with real ffmpeg: `secs` of a 640×360 test
    /// pattern in `video` and a tone in `audio` (encoder names), with a
    /// keyframe every 2 s, in Matroska.
    pub fn generate_media(&self, secs: u32, video: &str, audio: &str) {
        std::fs::create_dir_all(self.media.parent().unwrap()).unwrap();
        let status = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .args(["-f", "lavfi", "-i", "testsrc2=size=640x360:rate=25"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"])
            .args(["-t", &secs.to_string(), "-pix_fmt", "yuv420p", "-g", "50"])
            .args(["-c:v", video, "-c:a", audio, "-ac", "2"])
            .arg(&self.media)
            .status()
            .expect("ffmpeg runs");
        assert!(status.success(), "ffmpeg could not make the test media");
    }

    /// Give the movie made with [`TestServer::generate_media`] an English
    /// PGS subtitle track (stream 2) showing each `(text, start_ms,
    /// end_ms)`: the text drawn with ffmpeg's drawtext, as a Blu-ray
    /// subtitle would show it.
    pub fn add_pgs(&self, cues: &[(&str, u64, u64)]) {
        use spoolframe_media::pgs::fixture;
        const WIDTH: u16 = 640;
        const HEIGHT: u16 = 64;
        let font =
            std::env::var("SPOOLFRAME_TEST_FONT").expect("the dev shell sets SPOOLFRAME_TEST_FONT");
        let shown: Vec<fixture::Cue> = cues
            .iter()
            .map(|&(text, start_ms, end_ms)| {
                let out = std::process::Command::new("ffmpeg")
                    .args(["-v", "error", "-f", "lavfi", "-i"])
                    .arg(format!("color=black:s={WIDTH}x{HEIGHT}:d=1"))
                    .arg("-vf")
                    .arg(format!(
                        "drawtext=fontfile={font}:text='{text}':fontcolor=white:fontsize=40:x=12:y=10"
                    ))
                    .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "gray", "-"])
                    .output()
                    .expect("ffmpeg runs");
                assert!(out.status.success(), "ffmpeg could not draw {text:?}");
                fixture::Cue {
                    start_ms,
                    end_ms,
                    x: 640,
                    y: 960,
                    width: WIDTH,
                    height: HEIGHT,
                    mask: out.stdout.into_iter().map(|p| u8::from(p > 128)).collect(),
                }
            })
            .collect();
        let sup = self.dir.join("subs.sup");
        std::fs::write(&sup, fixture::sup(&shown, 65_000)).unwrap();
        let with = self.media.with_extension("pgs.mkv");
        // `-copyts`: without it ffmpeg moves the track back by its first
        // cue's time.
        let status = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(&self.media)
            .arg("-i")
            .arg(&sup)
            .args(["-map", "0", "-map", "1", "-c", "copy", "-copyts"])
            .args(["-metadata:s:s:0", "language=eng"])
            .arg(&with)
            .status()
            .expect("ffmpeg runs");
        assert!(status.success(), "ffmpeg could not add the PGS track");
        std::fs::rename(&with, &self.media).unwrap();
    }

    /// Give the movie made with [`TestServer::generate_media`] a text
    /// subtitle track (the next stream) of `codec` (`ass`, `mov_text`, …),
    /// English, with the cues of SubRip `srt`.
    pub fn add_text_track(&self, srt: &str, codec: &str) {
        let source = self.dir.join("subs.srt");
        std::fs::write(&source, srt).unwrap();
        let with = self.media.with_extension("text.mkv");
        let status = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(&self.media)
            .arg("-i")
            .arg(&source)
            .args(["-map", "0", "-map", "1", "-c", "copy", "-c:s", codec])
            .args(["-metadata:s:s:0", "language=eng"])
            .arg(&with)
            .status()
            .expect("ffmpeg runs");
        assert!(status.success(), "ffmpeg could not add the {codec} track");
        std::fs::rename(&with, &self.media).unwrap();
    }

    /// Where a converted playback's job writes, for epoch `epoch`.
    pub fn job_dir(&self, handle: &PlaybackHandle, epoch: u32) -> PathBuf {
        self.dir
            .join("cache")
            .join("transcode")
            .join(&handle.0)
            .join(epoch.to_string())
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
            .add_user(
                username,
                display_name,
                &phc,
                spoolframe_types::Role::Consumer,
            )
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

    /// Stop and start again on the same directory (key, database) and
    /// UDP port, as a restarted server is.
    pub async fn restart(self) -> Self {
        let port = self
            .server
            .addr()
            .ip_addrs()
            .find(|a| a.is_ipv4())
            .expect("an IPv4 address")
            .port();
        let dir = self.dir.clone();
        self.stop().await;
        let options = ServerOptions {
            port: Some(port),
            ..ServerOptions::default()
        };
        Self::start_with(dir, options).await
    }
}

#[derive(Clone)]
pub struct TestClient {
    pub endpoint: Endpoint,
    pub conn: Connection,
}

impl TestClient {
    /// A new connection from the same endpoint (the same device), as after
    /// a restart of the app. It says Hello again.
    pub async fn reconnect(&self, server: &TestServer) -> TestClient {
        let conn = self
            .endpoint
            .connect(server.server.addr(), CLIENT_ALPN)
            .await
            .unwrap();
        let client = TestClient {
            endpoint: self.endpoint.clone(),
            conn,
        };
        client.hello().await;
        client
    }

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

    /// A read of a converted playback's output in `epoch` (P2-04).
    pub async fn read_at(
        &self,
        session: &SessionId,
        handle: &PlaybackHandle,
        epoch: u32,
        offset: u64,
        length: u64,
    ) -> (MediaReadResponse, Vec<u8>) {
        self.media(MediaRead {
            session: session.clone(),
            handle: handle.clone(),
            offset,
            length,
            epoch,
            segment: None,
        })
        .await
    }

    /// One MEDIA stream: `read` out, the response frame and the bytes back.
    pub async fn media(&self, read: MediaRead) -> (MediaReadResponse, Vec<u8>) {
        let (mut send, mut recv) = self.conn.open_bi().await.unwrap();
        send.write_u8(bi::MEDIA).await.unwrap();
        write_frame(&mut send, &read).await.unwrap();
        send.finish().unwrap();
        let response: MediaReadResponse = read_frame(&mut recv).await.unwrap();
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
        self.read_at(session, handle, 0, offset, length).await
    }

    pub async fn close(self) {
        self.conn.close(0u32.into(), b"done");
        self.endpoint.close().await;
    }
}

/// Writes an executable script. The text goes to a plain file first and
/// `install` makes the script from it, so this process never holds the
/// script open for writing: a child another test forks at that moment
/// would inherit the handle, and running the script would then fail with
/// "Text file busy" (ETXTBSY).
pub fn write_script(path: &Path, text: &str) {
    let source = path.with_extension("txt");
    std::fs::write(&source, text).unwrap();
    let status = std::process::Command::new("install")
        .args(["-m", "755"])
        .arg(&source)
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success(), "install {} failed", path.display());
}
