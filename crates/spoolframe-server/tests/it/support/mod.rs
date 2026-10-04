//! A server with a fake catalog, and in-process Iroh client endpoints that
//! talk `spoolframe/client/1` to it, relays off. Shared by the integration
//! tests (and P1-13's LAN test).

// Each test file uses part of this module.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use iroh::endpoint::{Connection, presets};
use iroh::{Endpoint, RelayMode};
use serde::Serialize;
use spoolframe_catalog::{Catalog, CatalogError, Image, ItemPage, ItemWithChildren, Page};
use spoolframe_media::Prober;
use spoolframe_media::encoders::{FfmpegCommand, TranscodeSettings};
use spoolframe_media::ocr::Ocr;
use spoolframe_protocol::codec::{MAX_FRAME, read_frame, write_frame};
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
    ArtworkId, ClientCapabilities, Episode, Item, Library, LibraryId, LibraryKind, Marker,
    MarkerKind, MediaId, MediaKind, Movie, Namespace, PlaybackState, Season, Show,
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

/// A search for this finds a movie whose title alone is over a frame: a
/// reply that cannot be sent.
pub const A_TITLE_OVER_A_FRAME: &str = "a title over a frame";

/// One library holding one movie, `plex:movie:1`.
#[derive(Debug)]
pub struct FakeCatalog {
    /// The movie's file, once a test writes it.
    media: PathBuf,
    /// What the catalog answers for keys items had before it was rebuilt.
    old_keys: Mutex<HashMap<MediaId, OldKey>>,
    /// Each `item` and `external_ids` call: the method and the id.
    calls: Mutex<Vec<(&'static str, MediaId)>>,
}

/// What a key holds after the catalog gave its item a new one. A key the
/// fake knows nothing of is not found.
#[derive(Debug, Clone)]
pub enum OldKey {
    /// Another item of the same kind, with these external ids.
    AnotherItem(Vec<String>),
    /// An item of another kind: `PlexCatalog` cannot read it as `item`,
    /// and has no `external_ids` for it.
    AnotherKind,
    /// The key is another catalog's.
    Foreign,
    /// The catalog cannot be asked.
    Unreachable,
}

impl FakeCatalog {
    pub fn set_old_key(&self, id: &MediaId, holds: OldKey) {
        self.old_keys.lock().unwrap().insert(id.clone(), holds);
    }

    pub fn forget_old_key(&self, id: &MediaId) {
        self.old_keys.lock().unwrap().remove(id);
    }

    /// The `item` and `external_ids` calls since the last time, by method.
    pub fn take_calls(&self, id: &MediaId) -> Vec<&'static str> {
        let mut calls = self.calls.lock().unwrap();
        let about = calls.iter().filter(|(_, i)| i == id).map(|(m, _)| *m);
        let about = about.collect();
        calls.clear();
        about
    }

    fn called(&self, method: &'static str, id: &MediaId) {
        self.calls.lock().unwrap().push((method, id.clone()));
    }

    fn old_key(&self, id: &MediaId) -> Option<OldKey> {
        self.old_keys.lock().unwrap().get(id).cloned()
    }

    fn unreachable() -> CatalogError {
        CatalogError::Unreachable {
            url: "http://fake".into(),
            call: "GET /library/metadata".into(),
            source: "connection refused".into(),
        }
    }
}

/// The external ids of an item of the fake's tree.
fn external_ids(item: &Item) -> Vec<String> {
    let first = movie();
    let of = if item.id() == second_copy().id() {
        &first
    } else {
        item
    };
    vec![format!("test://{}", of.id().key())]
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

/// The same film in a second library, `plex:movie:2`: its own key, and
/// the first copy's external ids.
pub fn second_copy() -> Item {
    Item::Movie(Movie {
        id: MediaId::new(Namespace::Plex, MediaKind::Movie, "2").unwrap(),
        title: "Alien (4K)".into(),
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

/// The markers of episode 15, the only item that has any: an intro, and
/// credits that run to the end, starting after the point P1-14's rule
/// calls the episode watched at (39:00 of its 44:00).
pub fn markers() -> Vec<Marker> {
    vec![
        Marker {
            kind: MarkerKind::Intro,
            start_ms: 30_000,
            end_ms: 90_000,
            to_end: false,
        },
        Marker {
            kind: MarkerKind::Credits,
            start_ms: 2_400_000,
            end_ms: 2_640_001,
            to_end: true,
        },
    ]
}

/// Every item the fake catalog has, with its children.
fn tree() -> Vec<(Item, Vec<Item>)> {
    vec![
        (movie(), Vec::new()),
        (second_copy(), Vec::new()),
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
        self.called("item", id);
        match self.old_key(id) {
            Some(OldKey::AnotherItem(external_ids)) => {
                let Item::Movie(movie) = movie() else {
                    unreachable!()
                };
                return Ok(ItemWithChildren {
                    item: Item::Movie(Movie {
                        id: id.clone(),
                        title: "Aliens".into(),
                        ..movie
                    }),
                    children: Vec::new(),
                    external_ids,
                    markers: Vec::new(),
                });
            }
            Some(OldKey::AnotherKind) => {
                return Err(CatalogError::Backend {
                    url: "http://fake".into(),
                    call: "GET /library/metadata".into(),
                    item: Some(id.to_string()),
                    message: format!("the PMS returned another kind, not {id}"),
                });
            }
            Some(OldKey::Foreign) => return Err(CatalogError::ForeignId(id.to_string())),
            Some(OldKey::Unreachable) => return Err(Self::unreachable()),
            None => {}
        }
        tree()
            .into_iter()
            .find(|(item, _)| item.id() == id)
            .map(|(item, children)| {
                let markers = if id.key() == "15" {
                    markers()
                } else {
                    Vec::new()
                };
                ItemWithChildren {
                    external_ids: external_ids(&item),
                    item,
                    children,
                    markers,
                }
            })
            .ok_or_else(|| CatalogError::NotFound(id.to_string()))
    }

    async fn external_ids(&self, id: &MediaId) -> Result<Vec<String>, CatalogError> {
        self.called("external_ids", id);
        match self.old_key(id) {
            Some(OldKey::AnotherItem(external_ids)) => return Ok(external_ids),
            Some(OldKey::AnotherKind) => return Err(CatalogError::NotFound(id.to_string())),
            Some(OldKey::Foreign) => return Err(CatalogError::ForeignId(id.to_string())),
            Some(OldKey::Unreachable) => return Err(Self::unreachable()),
            None => {}
        }
        tree()
            .iter()
            .find(|(item, _)| item.id() == id)
            .map(|(item, _)| external_ids(item))
            .ok_or_else(|| CatalogError::NotFound(id.to_string()))
    }

    async fn search(&self, query: &str, _limit: u32) -> Result<Vec<Item>, CatalogError> {
        if query == A_TITLE_OVER_A_FRAME {
            let Item::Movie(movie) = movie() else {
                unreachable!()
            };
            return Ok(vec![Item::Movie(Movie {
                title: "a".repeat(MAX_FRAME as usize),
                ..movie
            })]);
        }
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
    /// The most all conversions may write together, in bytes.
    pub scratch_limit: u64,
    pub read_ahead: Duration,
    /// How long an encode may make no progress before it is ended; tests
    /// of a stalled encode shorten it.
    pub stalled_after: Duration,
    /// The encoder jobs run: real ffmpeg, or a stand-in script.
    pub ffmpeg: PathBuf,
    /// Read bitmap subtitles as text with real tesseract (P2-07). Off: the
    /// server cannot.
    pub ocr: bool,
    /// The tesseract that reads them: the real one, or a stand-in script.
    pub tesseract: PathBuf,
    /// How long failed logins and PINs count; tests of many failures
    /// shorten it.
    pub failure_window: Duration,
    /// How long device sign-in codes live (P3-07).
    pub sign_in_lifetime: Duration,
    /// How long a device's playbacks outlive its last connection.
    pub disconnect_grace: Duration,
    /// Where browsers reach the web UI (`admin.public_url`), if anywhere.
    pub public_url: Option<String>,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            name: "Home Media".into(),
            port: None,
            mdns: false,
            real_media: false,
            max_jobs: 2,
            scratch_limit: 1 << 30,
            read_ahead: Duration::from_secs(5),
            stalled_after: spoolframe_server::encode::job::STALLED_AFTER,
            ffmpeg: PathBuf::from("ffmpeg"),
            ocr: false,
            tesseract: PathBuf::from("tesseract"),
            failure_window: spoolframe_server::auth::FAILURE_WINDOW,
            sign_in_lifetime: spoolframe_server::device_sign_in::LIFETIME,
            disconnect_grace: spoolframe_server::playback::DISCONNECT_GRACE,
            public_url: None,
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
    /// Its logins, sessions and pending device sign-ins: what an admin
    /// API beside it shares.
    pub auth: Auth,
    pub catalog: Arc<FakeCatalog>,
    pub media: PathBuf,
    pub dir: StateDir,
    /// Where browsers reach its web UI, if anywhere.
    pub public_url: Option<String>,
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
        let fake = Arc::new(FakeCatalog {
            media: media.clone(),
            old_keys: Mutex::default(),
            calls: Mutex::default(),
        });
        let catalog: Arc<dyn Catalog> = fake.clone();
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
                scratch_limit_bytes: options.scratch_limit,
                read_ahead: options.read_ahead,
                stalled_after: options.stalled_after,
            },
            options.ffmpeg.clone(),
        )
        .unwrap();
        let ocr = if options.ocr {
            Ocr::detect(&options.tesseract).await
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
        let auth = Auth::with_failure_window(store.clone(), options.failure_window)
            .with_sign_in_lifetime(options.sign_in_lifetime);
        let services = Services {
            auth: auth.clone(),
            public_url: options.public_url.clone(),
            watch,
            catalog,
            prober,
            artwork: Arc::new(artwork),
            playbacks: Arc::new(
                Playbacks::new(PLAYBACK_IDLE, PLAYBACK_RECHECK)
                    .with_disconnect_grace(options.disconnect_grace),
            ),
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
            auth,
            public_url: options.public_url,
            catalog: fake,
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
        self.generate(secs, video, audio, "0");
    }

    /// As [`TestServer::generate_media`], with the picture starting 0.5 s
    /// after the sound, as in many real files: the keyframes are at about
    /// 0.5 s, 2.5 s and so on, none at 0.
    pub fn generate_media_with_late_video(&self, secs: u32, video: &str, audio: &str) {
        self.generate(secs, video, audio, "0.5");
    }

    /// As [`TestServer::generate_media`], then cut with stream copy 40 ms
    /// after its second keyframe, into MP4: the file starts at that
    /// keyframe, at -0.04 s, and the next are at 1.96 s, 3.96 s and so on.
    pub fn generate_media_cut_after_a_keyframe(&self, secs: u32, video: &str, audio: &str) {
        self.generate(secs + 2, video, audio, "0");
        let whole = self.media.with_extension("whole.mkv");
        std::fs::rename(&self.media, &whole).unwrap();
        let status = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .args(["-ss", "2.04", "-i"])
            .arg(&whole)
            .args(["-c", "copy", "-f", "mp4"])
            .arg(&self.media)
            .status()
            .expect("ffmpeg runs");
        assert!(status.success(), "ffmpeg could not cut the test media");
        std::fs::remove_file(&whole).unwrap();
    }

    fn generate(&self, secs: u32, video: &str, audio: &str, video_offset: &str) {
        std::fs::create_dir_all(self.media.parent().unwrap()).unwrap();
        let status = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .args(["-f", "lavfi", "-itsoffset", video_offset])
            .args(["-i", "testsrc2=size=640x360:rate=25"])
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
        let sup = self.dir.join("subs.sup");
        std::fs::write(&sup, Self::pgs(cues)).unwrap();
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

    /// Write the same PGS subtitle as [`TestServer::add_pgs`] makes, as an
    /// English `.sup` file next to the movie.
    pub fn write_pgs_sidecar(&self, cues: &[(&str, u64, u64)]) {
        std::fs::write(self.media.with_extension("en.sup"), Self::pgs(cues)).unwrap();
    }

    /// Write a VobSub subtitle showing each `(text, start_ms, end_ms)` as
    /// [`TestServer::add_pgs`] shows it, as English `.idx` and `.sub`
    /// files next to the movie. ffmpeg has no VobSub muxer: it encodes
    /// the pictures into an MPEG program stream, which is what a `.sub`
    /// is, and the `.idx` (the palette, and where each cue is in the
    /// `.sub`) is written here.
    ///
    /// Give it two cues or more. ffmpeg 9.0 can wait for ever when it lays
    /// a picture subtitle file with a single packet over the video: the
    /// file's end reaches the filters before they are set up, and is lost.
    pub fn write_vobsub_sidecar(&self, cues: &[(&str, u64, u64)]) {
        // ffmpeg's own default, said here so the `.idx` can repeat it.
        const PALETTE: [&str; 16] = [
            "000000", "0000ff", "00ff00", "ff0000", "ffff00", "ff00ff", "00ffff", "ffffff",
            "808000", "8080ff", "800080", "80ff80", "008080", "ff8080", "555555", "aaaaaa",
        ];
        /// An MPEG program stream pack. A cue this small is one pack.
        const PACK: usize = 2048;
        let sup = self.dir.join("vobsub.sup");
        std::fs::write(&sup, Self::pgs(cues)).unwrap();
        let sub = self.media.with_extension("en.sub");
        // `-fix_sub_duration`: a PGS cue ends with a packet of its own,
        // a VobSub one carries its length.
        let status = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .args(["-copyts", "-fix_sub_duration", "-i"])
            .arg(&sup)
            .args(["-c:s", "dvdsub", "-palette", &PALETTE.join(",")])
            .args(["-f", "vob"])
            .arg(&sub)
            .status()
            .expect("ffmpeg runs");
        assert!(status.success(), "ffmpeg could not write the VobSub file");
        assert_eq!(
            std::fs::metadata(&sub).unwrap().len() as usize,
            cues.len() * PACK,
            "one pack per cue"
        );
        let mut idx = format!(
            "# VobSub index file, v7 (do not modify this line!)\n\
             size: 1920x1080\n\
             palette: {}\n\
             langidx: 0\n\
             id: en, index: 0\n",
            PALETTE.join(", ")
        );
        for (i, &(_, start_ms, _)) in cues.iter().enumerate() {
            idx.push_str(&format!(
                "timestamp: {:02}:{:02}:{:02}:{:03}, filepos: {:09x}\n",
                start_ms / 3_600_000,
                start_ms / 60_000 % 60,
                start_ms / 1000 % 60,
                start_ms % 1000,
                i * PACK
            ));
        }
        std::fs::write(self.media.with_extension("en.idx"), idx).unwrap();
    }

    /// A PGS (`.sup`) stream showing each `(text, start_ms, end_ms)`.
    fn pgs(cues: &[(&str, u64, u64)]) -> Vec<u8> {
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
        fixture::sup(&shown, 65_000)
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

    /// Write an English ASS subtitle file next to the movie, with the cues
    /// of SubRip `srt`.
    pub fn write_ass_sidecar(&self, srt: &str) {
        let source = self.dir.join("sidecar.srt");
        std::fs::write(&source, srt).unwrap();
        let status = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(&source)
            .arg(self.media.with_extension("en.ass"))
            .status()
            .expect("ffmpeg runs");
        assert!(status.success(), "ffmpeg could not write the ASS file");
    }

    /// Where a converted playback's job writes, for epoch `epoch`.
    pub fn job_dir(&self, handle: &PlaybackHandle, epoch: u32) -> PathBuf {
        self.dir
            .join("cache")
            .join("transcode")
            .join(spoolframe_server::encode::job::JOBS_DIR)
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
        self.reconnect_with(server, None).await
    }

    /// As [`TestClient::reconnect`], saying what the device plays.
    pub async fn reconnect_with(
        &self,
        server: &TestServer,
        capabilities: Option<ClientCapabilities>,
    ) -> TestClient {
        let conn = self
            .endpoint
            .connect(server.server.addr(), CLIENT_ALPN)
            .await
            .unwrap();
        let client = TestClient {
            endpoint: self.endpoint.clone(),
            conn,
        };
        assert!(matches!(
            client.hello_with(None, capabilities).await,
            Response::Ok(Reply::ServerInfo(_))
        ));
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

/// The warnings the servers in this test binary log, kept so a test can
/// check that something was said. All tests share them: look for a line
/// by what only that test's server would say.
pub mod logs {
    use std::io::Write;
    use std::sync::{Mutex, OnceLock};

    static LINES: OnceLock<&'static Mutex<Vec<u8>>> = OnceLock::new();

    struct Kept(&'static Mutex<Vec<u8>>);

    impl Write for Kept {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Keep warnings from now on. Call before what should log.
    pub fn keep() {
        LINES.get_or_init(|| {
            let lines: &'static Mutex<Vec<u8>> = Box::leak(Box::default());
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::WARN)
                .with_ansi(false)
                .with_writer(move || Kept(lines))
                .init();
            lines
        });
    }

    /// Whether a kept line has every one of `parts`.
    pub fn has(parts: &[&str]) -> bool {
        let lines = LINES.get().expect("logs::keep() first").lock().unwrap();
        String::from_utf8_lossy(&lines)
            .lines()
            .any(|line| parts.iter().all(|part| line.contains(part)))
    }
}
