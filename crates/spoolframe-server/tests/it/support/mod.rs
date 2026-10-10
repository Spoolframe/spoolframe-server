//! A server with a fake catalog, and in-process Iroh client endpoints that
//! talk `spoolframe/client/1` to it, relays off. Shared by the integration
//! tests (and P1-13's LAN test).

// Each test file uses part of this module.
#![allow(dead_code)]

pub mod idp;

pub use spoolframe_server::test_support::{
    A_TITLE_OVER_A_FRAME, FakeCatalog, episode, fake_ffprobe, library, markers, movie, poster,
    show, write_script,
};

use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use iroh::endpoint::{Connection, presets};
use iroh::{Endpoint, RelayMode};
use serde::Serialize;
use spoolframe_catalog::Catalog;
use spoolframe_media::Prober;
use spoolframe_media::ocr::Ocr;
use spoolframe_media::sandbox::{GpuDevices, Mode, Sandbox, Tool};
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
use spoolframe_server::clock::Clock;
use spoolframe_server::encode::job::Jobs;
use spoolframe_server::encode::{JobLimits, Units};
use spoolframe_server::identity;
use spoolframe_server::network::{EndpointOptions, Server, ServerName, Services};
use spoolframe_server::playback::Playbacks;
use spoolframe_server::previews::{PreviewSettings, PreviewSources, Previews, When};
use spoolframe_server::store::Store;
use spoolframe_server::subtitles::{OcrSettings, Subtitles};
use spoolframe_server::transcode::Transcoding;
use spoolframe_server::watch::{Watch, WatchRules};
use spoolframe_types::{ArtworkId, ClientCapabilities, SearchScope};
use tokio::io::AsyncWriteExt;

/// An SDK client that does not listen on the LAN (PX-10). A developer's
/// own server announces itself there, and a client that hears it lists it
/// in `Client::servers`, so a test's result would depend on the network.
/// The test servers are dialled at the addresses a test gives. Only the
/// LAN tests, in their private network namespace, listen.
pub async fn sdk_client(
    storage: Arc<dyn spoolframe_client_sdk::Storage>,
    device: spoolframe_client_sdk::Device,
) -> Result<spoolframe_client_sdk::Client, spoolframe_client_sdk::SdkError> {
    let options = spoolframe_client_sdk::ClientOptions {
        lan_discovery: false,
        ..spoolframe_client_sdk::ClientOptions::default()
    };
    spoolframe_client_sdk::Client::open_with_options(storage, device, options).await
}

/// Transcoding that has not detected anything: the software baseline.
/// No test runs a real ffmpeg.
pub fn transcoding() -> Arc<Transcoding> {
    Arc::new(Transcoding::new(
        Tool::unconfined("/nonexistent/ffmpeg"),
        Vec::new(),
        std::env::temp_dir(),
    ))
}

/// The media tools' sandbox for a server on `dir`, as the server's own
/// (PX-11 finding 34), but for what it hides: the tests' films are in the
/// state directory, so the database, the key and the secrets beside them
/// are hidden rather than the whole of it.
pub fn media_sandbox(dir: &Path) -> Sandbox {
    Sandbox::new(
        Mode::Required,
        vec![
            dir.join("spoolframe.db"),
            dir.join("endpoint.key"),
            dir.join("secrets"),
        ],
        GpuDevices::default(),
    )
    .expect("this kernel has Landlock")
}

/// A real tool (a bare name, looked up in `PATH`) confined in `sandbox`; a
/// stand-in script (a path) as it is: it lives in the state directory,
/// and is a shell script whose tools a confined shell cannot start.
pub fn tool(program: &Path, sandbox: &Sandbox) -> Tool {
    if program.components().count() == 1 {
        Tool::new(program, sandbox.clone())
    } else {
        Tool::unconfined(program)
    }
}

/// The server's own binary as its subtitle decoder (finding 45), confined
/// in `sandbox` as the server confines it.
pub fn decoder(sandbox: &Sandbox) -> Tool {
    Tool::new(env!("CARGO_BIN_EXE_spoolframe-server"), sandbox.clone())
}

/// A search of everything for `query`, up to 10 per group.
pub fn search(query: &str) -> spoolframe_protocol::SearchRequest {
    spoolframe_protocol::SearchRequest {
        query: query.into(),
        limit: 10,
        scope: SearchScope::All,
        kind: None,
        library: None,
    }
}

/// The playback timings of a test server. Idling out is the server's
/// own limit (30 minutes): a test that needs a playback idle moves
/// [`TestServer::clock`] on, so a slow machine never idles one out
/// mid-test. Rechecks stay short, for the requests a test makes in real
/// time.
pub const PLAYBACK_IDLE: Duration = spoolframe_server::playback::IDLE;
pub const PLAYBACK_RECHECK: Duration = Duration::from_millis(300);

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
/// Marks the copy of a test that [`alone`] runs.
const ALONE: &str = "SPOOLFRAME_TEST_ALONE";

/// Run `test` (as `--exact` names it: `refusals::a_test`) again in a
/// process where no other test runs, and assert it passed there. `true`
/// in that copy, which runs the body; `false` here.
///
/// A test that reads the server's log ([`Warnings`], [`LogCapture`])
/// needs it. tracing decides once per call site, for every thread,
/// whether a line there is wanted at all. While the test's subscriber is
/// the only one, a call site another test's server reaches first is asked
/// of that thread's subscriber, which is none, and stays off for the test
/// too: its line is never logged.
pub fn alone(test: &str) -> bool {
    if std::env::var_os(ALONE).is_some() {
        return true;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(ALONE, "1")
        .output()
        .unwrap();
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{test} alone:\n{log}");
    assert!(log.contains("1 passed"), "{test} did not run:\n{log}");
    false
}

/// A capture of the log is sound only in a test run [`alone`].
fn assert_alone() {
    assert!(
        std::env::var_os(ALONE).is_some(),
        "a test that reads the log runs alone: see `support::alone`"
    );
}

/// The server's log at WARN and above, as text, for as long as the guard
/// is held. Captured on this thread only: a `#[tokio::test]` runtime runs
/// the server's tasks on the test's own thread. Only in a test run
/// [`alone`].
#[derive(Clone, Default)]
pub struct Warnings(Arc<Mutex<Vec<u8>>>);

impl Warnings {
    pub fn capture() -> (Self, tracing::subscriber::DefaultGuard) {
        assert_alone();
        let warnings = Self::default();
        let writer = warnings.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        (warnings, tracing::subscriber::set_default(subscriber))
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock()).into_owned()
    }
}

impl std::io::Write for Warnings {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// One event the server logged, with its fields as text (`message` among
/// them), or one span it opened.
#[derive(Debug, Clone)]
pub struct Logged {
    pub level: tracing::Level,
    pub target: String,
    pub fields: std::collections::BTreeMap<String, String>,
}

impl Logged {
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields.get(name).map(String::as_str)
    }
}

/// Everything the server logs at every level, events and spans, field by
/// field, for as long as the guard is held. Captured on this thread only,
/// as [`Warnings`] is. Only in a test run [`alone`].
#[derive(Clone, Default)]
pub struct LogCapture(Arc<Mutex<Vec<Logged>>>);

/// The target of the refused-request log (finding 46).
pub const REFUSED: &str = "spoolframe::refused";

impl LogCapture {
    pub fn start() -> (Self, tracing::subscriber::DefaultGuard) {
        use tracing_subscriber::layer::SubscriberExt as _;
        assert_alone();
        let capture = Self::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        (capture, tracing::subscriber::set_default(subscriber))
    }

    /// Every line of the refused-request log so far.
    pub fn refused(&self) -> Vec<Logged> {
        self.0
            .lock()
            .iter()
            .filter(|line| line.target == REFUSED)
            .cloned()
            .collect()
    }

    /// What was captured, as text: what a secret must never be in. That
    /// is every line at DEBUG and above (`-v`), and the server's own at
    /// every level. Libraries' TRACE (`-vv`) is left out: jsonrpsee's
    /// prints whole requests, headers and parameters included.
    pub fn text(&self) -> String {
        let logged = self.0.lock();
        let lines: Vec<&Logged> = logged
            .iter()
            .filter(|line| {
                line.level <= tracing::Level::DEBUG || line.target.starts_with("spoolframe")
            })
            .collect();
        format!("{lines:?}")
    }

    /// Every event so far whose message is `message`.
    pub fn saying(&self, message: &str) -> Vec<Logged> {
        self.0
            .lock()
            .iter()
            .filter(|line| line.field("message") == Some(message))
            .cloned()
            .collect()
    }

    /// Every event so far, with a message: lines, not spans.
    pub fn saying_anything(&self) -> Vec<Logged> {
        self.0
            .lock()
            .iter()
            .filter(|line| line.field("message").is_some())
            .cloned()
            .collect()
    }

    pub fn clear(&self) {
        self.0.lock().clear();
    }
}

struct Fields<'a>(&'a mut std::collections::BTreeMap<String, String>);

impl tracing::field::Visit for Fields<'_> {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for LogCapture {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let mut fields = std::collections::BTreeMap::new();
        event.record(&mut Fields(&mut fields));
        self.0.lock().push(Logged {
            level: *event.metadata().level(),
            target: event.metadata().target().to_owned(),
            fields,
        });
    }

    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _: &tracing::span::Id,
        _: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = std::collections::BTreeMap::new();
        attrs.record(&mut Fields(&mut fields));
        self.0.lock().push(Logged {
            level: *attrs.metadata().level(),
            target: attrs.metadata().target().to_owned(),
            fields,
        });
    }
}

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
    /// `network.ipv6`: on unless a test is about turning it off.
    pub ipv6: bool,
    pub mdns: bool,
    /// Real ffprobe and ffmpeg, on a file made with [`TestServer::generate_media`],
    /// for converted playback (P2-03). Off: the stand-in ffprobe and its
    /// fixtures.
    pub real_media: bool,
    /// The encode budget (`transcode.max_jobs`, in units): an audio-only
    /// conversion of the generated media weighs [`Units::quarters`]`(1)`.
    pub budget: Units,
    pub max_remux: usize,
    pub max_gpu_jobs: usize,
    /// The most all conversions may write together, in bytes.
    pub scratch_limit: u64,
    pub read_ahead: Duration,
    /// How long an encode may make no progress before it is ended; tests
    /// of a stalled encode shorten it.
    pub stalled_after: Duration,
    /// The encoder jobs run: real ffmpeg, or a stand-in script.
    pub ffmpeg: PathBuf,
    /// With `real_media`, the ffprobe run instead of `ffprobe` (a script
    /// around it, say).
    pub real_ffprobe: Option<PathBuf>,
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
    /// Serve from a catalog that is not set up yet (P3-10) instead of the
    /// fake one.
    pub not_set_up: bool,
    /// Serve from this catalog (Plex libraries, P3-11) instead of the fake
    /// one.
    pub catalog: Option<Arc<spoolframe_server::catalog::LiveCatalog>>,
    /// When and how frame previews are made (P3-13). Never, unless a test
    /// is about them: a playback would start making some.
    pub previews: PreviewSettings,
    /// The ffmpeg previews are made with: real, or a stand-in script.
    pub previews_ffmpeg: PathBuf,
    /// How long an app passkey's challenge lives (P3-19).
    pub passkey_lifetime: Duration,
    /// What the stand-in ffprobe says the movie is: a file in
    /// spoolframe-media's fixtures.
    pub ffprobe_fixture: &'static str,
    /// Who may hold connections (PX-10); tests of the limits shrink them.
    pub admission: spoolframe_server::admission::Admission,
    /// The sign-in attempts devices that are not signed in share (PX-10).
    pub stranger_attempts: spoolframe_server::auth::Bucket,
    /// Which addresses a signed-in device is told (PX-12): all of them,
    /// unless a test is about the filter.
    pub announce: spoolframe_server::announce::Announce,
    /// The public addresses (`network.wan`, P6-02): none unless a test is
    /// about them.
    pub wan: Option<spoolframe_server::wan::Wan>,
    /// What finds them: stand-ins, which never reach a router or the
    /// Internet.
    pub finders: spoolframe_server::wan::Finders,
    /// `network.relays` (P6-03): none unless a test is about them.
    pub relays: Vec<spoolframe_server::network::Relay>,
    /// `network.pkarr` (P6-05): off unless a test is about it.
    pub pkarr: spoolframe_server::network::Pkarr,
    /// `network.own_relays` (P6-04): none unless a test is about them.
    pub own_relays: Vec<iroh::EndpointId>,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            name: "Home Media".into(),
            port: None,
            ipv6: true,
            mdns: false,
            real_media: false,
            budget: Units::jobs(2),
            max_remux: 32,
            max_gpu_jobs: 3,
            scratch_limit: 1 << 30,
            read_ahead: Duration::from_secs(5),
            stalled_after: spoolframe_server::encode::job::STALLED_AFTER,
            ffmpeg: PathBuf::from("ffmpeg"),
            real_ffprobe: None,
            ocr: false,
            tesseract: PathBuf::from("tesseract"),
            failure_window: spoolframe_server::auth::FAILURE_WINDOW,
            sign_in_lifetime: spoolframe_server::device_sign_in::LIFETIME,
            disconnect_grace: spoolframe_server::playback::DISCONNECT_GRACE,
            public_url: None,
            not_set_up: false,
            catalog: None,
            previews: PreviewSettings {
                when: When::Never,
                interval: Duration::from_secs(10),
                cache_limit_bytes: 1 << 30,
                workers: 1,
            },
            previews_ffmpeg: PathBuf::from("ffmpeg"),
            passkey_lifetime: spoolframe_server::passkeys::CEREMONY_LIFETIME,
            ffprobe_fixture: "mkv-forced-subs.json",
            admission: spoolframe_server::admission::Admission::default(),
            stranger_attempts: spoolframe_server::auth::STRANGER_ATTEMPTS,
            announce: spoolframe_server::announce::Announce::default(),
            wan: None,
            relays: Vec::new(),
            pkarr: spoolframe_server::network::Pkarr::Off,
            own_relays: Vec::new(),
            finders: offline_finders(),
        }
    }
}

/// An IP lookup that asks `url` instead of wtfismyip, once, or again
/// after each of `backoff`.
pub fn lookup_at(url: &str, backoff: &[Duration]) -> spoolframe_server::wan::Lookup {
    spoolframe_server::wan::Lookup {
        client: reqwest::Client::new(),
        wtfismyip: url.parse().unwrap(),
        timeout: Duration::from_secs(5),
        backoff: backoff.to_vec(),
    }
}

/// A router that answers as a test says: [`FakeRouter::answer`] sets what
/// it says about every port it was asked to map.
#[derive(Debug)]
pub struct FakeRouter {
    answer: tokio::sync::watch::Sender<spoolframe_server::wan::Mapping>,
    asked: parking_lot::Mutex<Vec<u16>>,
}

impl FakeRouter {
    /// One that has not answered yet.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            answer: tokio::sync::watch::Sender::new(spoolframe_server::wan::Mapping::Asking),
            asked: parking_lot::Mutex::default(),
        })
    }

    pub fn answer(&self, mapping: spoolframe_server::wan::Mapping) {
        self.answer.send_replace(mapping);
    }

    /// The local ports it was asked to map.
    pub fn asked(&self) -> Vec<u16> {
        self.asked.lock().clone()
    }
}

impl spoolframe_server::wan::PortMapper for FakeRouter {
    fn map(
        &self,
        local_port: std::num::NonZeroU16,
    ) -> tokio::sync::watch::Receiver<spoolframe_server::wan::Mapping> {
        self.asked.lock().push(local_port.get());
        self.answer.subscribe()
    }
}

/// Finders that find nothing and reach nothing: a lookup at a port
/// nothing listens on (discard), and a router that never answers.
pub fn offline_finders() -> spoolframe_server::wan::Finders {
    spoolframe_server::wan::Finders {
        lookup: lookup_at("http://127.0.0.1:9/text", &[]),
        mapper: FakeRouter::new(),
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
    /// Its server id, kept in its state directory.
    pub server_id: spoolframe_protocol::ServerId,
    /// What its store, logins, sign-in codes, playbacks and app passkeys
    /// time expiry by: a test moves it on instead of sleeping (PX-09).
    pub clock: Clock,
    /// What it is called: a live setting (P3-10).
    pub name: ServerName,
    pub store: Store,
    /// Its logins, sessions and pending device sign-ins: what an admin
    /// API beside it shares.
    pub auth: Auth,
    pub catalog: Arc<FakeCatalog>,
    pub media: PathBuf,
    pub dir: StateDir,
    /// Where browsers reach its web UI, if anywhere.
    pub public_url: Option<String>,
    /// Users' Plex links (P3-12). Its plex.tv is an address nothing
    /// answers at: tests save links directly and never reach Plex.
    pub plex_accounts: Arc<spoolframe_server::plex_accounts::PlexAccounts>,
    /// Its metrics (P3-14), read from its parts.
    pub metrics: Arc<spoolframe_server::metrics::Metrics>,
    /// Playback debug (P3-15) over its parts, for an admin API beside it.
    pub debug: Arc<spoolframe_server::playback_debug::PlaybackDebug>,
    /// Its conversions, and the encode budget they take.
    pub jobs: Arc<Jobs>,
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
        let server_id = identity::load_or_create_server_id(&dir).unwrap();
        let clock = Clock::default();
        let store = Store::open(&dir).await.unwrap().with_clock(clock.clone());
        let media = dir.join("media").join("movie.mkv");
        let fake = Arc::new(FakeCatalog::new(media.clone()));
        let catalog: Arc<dyn Catalog> = match &options.catalog {
            Some(live) => live.clone(),
            None if options.not_set_up => {
                Arc::new(spoolframe_server::catalog::LiveCatalog::default())
            }
            None => fake.clone(),
        };
        let artwork =
            ArtworkCache::open(&dir.join("cache/artwork"), 64 << 20, catalog.clone()).unwrap();
        let ffprobe = if let Some(ffprobe) = &options.real_ffprobe {
            ffprobe.clone()
        } else if options.real_media {
            PathBuf::from("ffprobe")
        } else {
            fake_ffprobe(&dir, options.ffprobe_fixture)
        };
        let sandbox = media_sandbox(&dir);
        let ffprobe = tool(&ffprobe, &sandbox);
        let prober = Arc::new(Prober::new(ffprobe.clone(), Prober::DEFAULT_TIMEOUT));
        let jobs = Jobs::new(
            JobLimits {
                budget: options.budget,
                max_remux: options.max_remux,
                max_gpu_jobs: options.max_gpu_jobs,
                scratch_dir: dir.join("cache").join("transcode"),
                scratch_limit_bytes: options.scratch_limit,
                read_ahead: options.read_ahead,
                stalled_after: options.stalled_after,
                gpu_filters: false,
                tone_map: spoolframe_media::encoders::ToneMapSetting::Auto,
            },
            tool(&options.ffmpeg, &sandbox),
        )
        .unwrap();
        let jobs = Arc::new(jobs);
        let ocr = if options.ocr {
            Ocr::detect(&tool(&options.tesseract, &sandbox)).await
        } else {
            Ocr::disabled()
        };
        let subtitles = Subtitles::new(
            ocr,
            tool(Path::new("ffmpeg"), &sandbox),
            ffprobe,
            decoder(&sandbox),
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
        let previews = Previews::start(
            &dir.join("cache").join("previews"),
            options.previews.clone(),
            PreviewSources {
                catalog: catalog.clone(),
                prober: prober.clone(),
                transcoding: transcoding(),
                ffmpeg: tool(&options.previews_ffmpeg, &sandbox),
                encoding: jobs.encoding(),
            },
        )
        .unwrap();
        spoolframe_server::previews::ahead::spawn(&previews, catalog.clone(), store.clone());
        let auth = Auth::with_failure_window(store.clone(), options.failure_window)
            .with_sign_in_lifetime(options.sign_in_lifetime)
            .with_stranger_attempts(options.stranger_attempts);
        let plex_accounts = Arc::new(spoolframe_server::plex_accounts::PlexAccounts::new(
            spoolframe_server::config::secrets::Secrets::new(dir.as_ref()),
            store.clone(),
            spoolframe_server::plex_accounts::PlexSite {
                api: "http://127.0.0.1:9".parse().unwrap(),
                app: "http://127.0.0.1:9".parse().unwrap(),
            },
            None,
        ));
        let metrics = Arc::new(spoolframe_server::metrics::Metrics::default());
        metrics.observe_store(store.clone());
        let services = Services {
            auth: auth.clone(),
            public_url: options.public_url.clone(),
            plex_accounts: Some(plex_accounts.clone()),
            plex_sync: None,
            watch,
            catalog,
            catalog_changes: options.catalog.as_ref().map(|live| live.changes()),
            prober,
            artwork: Arc::new(artwork),
            playbacks: Arc::new(
                Playbacks::new(PLAYBACK_IDLE, PLAYBACK_RECHECK)
                    .with_disconnect_grace(options.disconnect_grace)
                    .with_clock(clock.clone()),
            ),
            transcoding: transcoding(),
            jobs: jobs.clone(),
            subtitles: Arc::new(subtitles),
            previews,
            app_passkeys: Arc::new(
                spoolframe_server::app_passkeys::AppPasskeys::with_lifetime(
                    server_id.clone(),
                    options.passkey_lifetime,
                )
                .with_clock(clock.clone()),
            ),
            metrics: metrics.clone(),
            connected: Arc::default(),
            wan: options.finders.clone(),
        };
        let debug = Arc::new(spoolframe_server::playback_debug::PlaybackDebug {
            playbacks: services.playbacks.clone(),
            connected: services.connected.clone(),
            catalog: services.catalog.clone(),
            prober: services.prober.clone(),
            transcoding: services.transcoding.clone(),
            subtitles: services.subtitles.clone(),
            previews: services.previews.clone(),
            jobs: Some(jobs.clone()),
        });
        let endpoint = EndpointOptions {
            port: options.port,
            ipv6: options.ipv6,
            relays: options.relays.clone(),
            pkarr: options.pkarr.clone(),
            own_relays: options.own_relays.clone(),
            mdns: options.mdns,
            admission: options.admission,
            announce: options.announce.clone(),
            wan: options.wan.clone(),
        };
        let name = ServerName::new(options.name.clone());
        let server = Server::start(key, &endpoint, &name, services)
            .await
            .unwrap();
        Self {
            server,
            server_id,
            clock,
            name,
            store,
            auth,
            public_url: options.public_url,
            plex_accounts,
            catalog: fake,
            dir,
            media,
            metrics,
            debug,
            jobs,
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
        spoolframe_server::test_support::write_episode(&self.media, key);
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

    /// `secs` of HEVC (libx265) and AC-3 in Matroska whose parameter sets
    /// are only in the stream, before each keyframe: the track's
    /// `CodecPrivate` is an `hvcC` record with no arrays, as in some 4K
    /// WEB-DLs. ffmpeg never writes one like that (it fills the record from
    /// the stream), so the record it wrote is cut back to its 23-byte
    /// header and the bytes freed are covered by an EBML Void element: no
    /// size of anything around it changes.
    pub fn generate_hevc_with_parameter_sets_in_the_stream_only(&self, secs: u32) {
        std::fs::create_dir_all(self.media.parent().unwrap()).unwrap();
        let status = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .args(["-f", "lavfi", "-i", "testsrc2=size=640x360:rate=25"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"])
            .args(["-t", &secs.to_string(), "-pix_fmt", "yuv420p", "-g", "50"])
            .args(["-c:v", "libx265"])
            .args(["-x265-params", "log-level=error:repeat-headers=1"])
            .args(["-c:a", "ac3", "-ac", "2"])
            .arg(&self.media)
            .status()
            .expect("ffmpeg runs");
        assert!(status.success(), "ffmpeg could not make the test media");
        let mut file = std::fs::read(&self.media).unwrap();
        // The video track's CodecPrivate (0x63A2), after its CodecID; its
        // size as an EBML number, then the record: version 1, and its
        // array count at byte 22.
        let codec_id = b"V_MPEGH/ISO/HEVC";
        let track = file
            .windows(codec_id.len())
            .position(|w| w == codec_id)
            .expect("an HEVC track");
        let at = track
            + file[track..]
                .windows(2)
                .position(|w| w == [0x63, 0xA2])
                .expect("a CodecPrivate");
        let first = file[at + 2];
        let width = first.leading_zeros() as usize + 1;
        let size = file[at + 3..at + 2 + width]
            .iter()
            .fold(u64::from(first) & (0xFF >> width), |n, b| {
                n << 8 | u64::from(*b)
            }) as usize;
        let record = at + 2 + width;
        assert_eq!(file[record], 1, "CodecPrivate is not an hvcC record");
        let end = record + size;
        // [ID ID size] + 23 bytes; then the Void: ID 0xEC and an 8-byte size.
        let kept = at + 3 + 23;
        let void = end - kept - 9;
        assert!(end >= kept + 9, "an hvcC record too short to cut");
        let mut cut = vec![0x63, 0xA2, 0x80 | 23];
        cut.extend_from_slice(&file[record..record + 22]);
        cut.push(0);
        cut.push(0xEC);
        cut.push(0x01);
        cut.extend_from_slice(&(void as u64).to_be_bytes()[1..]);
        cut.resize(end - at, 0);
        file.splice(at..end, cut);
        std::fs::write(&self.media, file).unwrap();
    }

    /// Make the movie's file as music: `secs` of white noise, which FLAC
    /// cannot make much smaller, as 24-bit 48 kHz stereo FLAC in Matroska
    /// with no picture (about 2.3 Mbit/s).
    pub fn generate_flac_noise(&self, secs: u32) {
        std::fs::create_dir_all(self.media.parent().unwrap()).unwrap();
        let status = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .args(["-f", "lavfi", "-i"])
            .arg(format!(
                "anoisesrc=color=white:sample_rate=48000:duration={secs}"
            ))
            .args(["-ac", "2", "-c:a", "flac", "-sample_fmt", "s32"])
            .args(["-bits_per_raw_sample", "24"])
            .arg(&self.media)
            .status()
            .expect("ffmpeg runs");
        assert!(status.success(), "ffmpeg could not make the test music");
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

    /// Give the movie made with [`TestServer::generate_media`] an English
    /// track (stream 2) of what [`TestServer::add_pgs`] shows, encoded by
    /// ffmpeg's `codec`: `dvdsub` (VobSub) or `dvbsub`. `-fix_sub_duration`
    /// gives each cue the length PGS says with a packet of its own.
    pub fn add_bitmap_track(&self, codec: &str, cues: &[(&str, u64, u64)]) {
        let sup = self.dir.join("bitmap.sup");
        std::fs::write(&sup, Self::pgs(cues)).unwrap();
        let with = self.media.with_extension("bitmap.mkv");
        let status = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(&self.media)
            .args(["-copyts", "-fix_sub_duration", "-i"])
            .arg(&sup)
            .args(["-map", "0", "-map", "1", "-c", "copy", "-c:s", codec])
            .args(["-metadata:s:s:0", "language=eng"])
            .arg(&with)
            .status()
            .expect("ffmpeg runs");
        assert!(status.success(), "ffmpeg could not add the {codec} track");
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
        TestClient {
            endpoint,
            conn,
            instance: Arc::default(),
            controls: Arc::default(),
        }
    }

    /// A client whose only way to the server is loopback: its one socket is
    /// bound to `127.0.0.1` and it dials the server's IPv4 port there, so
    /// the server sees it at `127.0.0.1` whatever other paths this host has.
    pub async fn loopback_client(&self) -> TestClient {
        let server = self.server.addr();
        let port = server
            .ip_addrs()
            .find(|addr| addr.is_ipv4())
            .expect("the server has an IPv4 address")
            .port();
        let endpoint = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .clear_ip_transports()
            .bind_addr("127.0.0.1:0")
            .unwrap()
            .bind()
            .await
            .unwrap();
        let loopback = iroh::EndpointAddr::new(server.id)
            .with_ip_addr(std::net::SocketAddr::from(([127, 0, 0, 1], port)));
        let conn = endpoint.connect(loopback, CLIENT_ALPN).await.unwrap();
        TestClient {
            endpoint,
            conn,
            instance: Arc::default(),
            controls: Arc::default(),
        }
    }

    pub async fn stop(self) {
        self.server.shutdown().await;
    }

    /// Stop and start again on the same directory (key, database), as a
    /// restarted server is: the same key, on any free UDP port. The old
    /// port is not asked for again: under the load of the workspace tests
    /// another endpoint may have taken it, and the bind fails. A client
    /// dials the restarted server at its new `server.addr()`; an SDK client
    /// is given it with `Client::add_address`.
    pub async fn restart(self) -> Self {
        let dir = self.dir.clone();
        self.stop().await;
        Self::start_with(dir, ServerOptions::default()).await
    }
}

#[derive(Clone)]
pub struct TestClient {
    pub endpoint: Endpoint,
    pub conn: Connection,
    pub instance: Arc<parking_lot::Mutex<Option<spoolframe_protocol::rpc::ServerInstance>>>,
    pub controls: Arc<tokio::sync::Mutex<()>>,
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
            instance: Arc::default(),
            controls: Arc::default(),
        };
        assert!(matches!(
            client.hello_with(None, capabilities).await,
            Response::Ok(Reply::ServerInfo(_))
        ));
        client
    }

    pub fn operation(&self) -> spoolframe_protocol::rpc::Operation {
        let instance = self.instance.lock().clone();
        spoolframe_protocol::rpc::Operation {
            id: uuid::Uuid::now_v7().to_string(),
            instance: instance.map(|i| i.id).unwrap_or_default(),
            expires_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64
                + 60_000,
        }
    }

    pub async fn wire<T: Serialize>(&self, request: &T) -> spoolframe_protocol::rpc::RpcResponse {
        let (mut send, mut recv) = self.conn.open_bi().await.unwrap();
        send.write_u8(bi::RPC).await.unwrap();
        write_frame(&mut send, request).await.unwrap();
        send.finish().unwrap();
        let response: spoolframe_protocol::rpc::RpcResponse =
            spoolframe_protocol::codec::read_reply(&mut recv)
                .await
                .unwrap();
        *self.instance.lock() = Some(response.server.clone());
        response
    }

    /// Wrap arbitrary envelopes too, to test malformed and unknown requests.
    pub async fn send<T: Serialize>(&self, envelope: &T) -> Response {
        #[derive(Serialize)]
        struct Wire<'a, T> {
            envelope: &'a T,
            operation: spoolframe_protocol::rpc::Operation,
            expected_revision: Option<u64>,
        }
        self.wire(&Wire {
            envelope,
            operation: self.operation(),
            expected_revision: None,
        })
        .await
        .reply
        .response
    }

    pub async fn revision(
        &self,
        session: &SessionId,
        handle: &PlaybackHandle,
    ) -> Result<u64, spoolframe_protocol::Error> {
        match self
            .send(&RequestEnvelope {
                session: Some(session.clone()),
                request: Request::PlaybackRevision {
                    handle: handle.clone(),
                },
            })
            .await
        {
            Response::Ok(Reply::PlaybackRevision { revision }) => Ok(revision),
            Response::Err(error) => Err(error),
            other => panic!("unexpected revision: {other:?}"),
        }
    }

    pub async fn call(&self, session: Option<&SessionId>, request: Request) -> Response {
        use spoolframe_protocol::resource::{ResourceMetadata, ResourceRequest, ResourceResponse};
        let control = match &request {
            Request::ChangeSubtitle { handle, .. }
            | Request::ChangeQuality { handle, .. }
            | Request::SeekPlayback { handle, .. }
            | Request::StopPlayback { handle } => Some(handle),
            _ => None,
        };
        let _control = if control.is_some() {
            Some(self.controls.lock().await)
        } else {
            None
        };
        let expected_revision = match (session, control) {
            (Some(session), Some(handle)) => match self.revision(session, handle).await {
                Ok(revision) => Some(revision),
                Err(error) => return Response::Err(error),
            },
            _ => None,
        };
        let subtitle = match (&request, session) {
            (
                Request::ReadSubtitle {
                    handle,
                    offset,
                    version,
                },
                Some(session),
            ) => {
                let revision = match self.revision(session, handle).await {
                    Ok(v) => v,
                    Err(e) => return Response::Err(e),
                };
                Some(ResourceRequest::Subtitle {
                    session: session.clone(),
                    handle: handle.clone(),
                    revision,
                    offset: *offset,
                    version: *version,
                })
            }
            (Request::ReadSubtitleSegment { handle, n: segment }, Some(session)) => {
                let revision = match self.revision(session, handle).await {
                    Ok(v) => v,
                    Err(e) => return Response::Err(e),
                };
                Some(ResourceRequest::SubtitleSegment {
                    session: session.clone(),
                    handle: handle.clone(),
                    revision,
                    segment: *segment,
                })
            }
            _ => None,
        };
        if let Some(subtitle) = subtitle {
            let (response, data) = self.resource(&subtitle).await;
            return match response {
                ResourceResponse::Error { error } => Response::Err(error),
                ResourceResponse::Ready {
                    metadata:
                        ResourceMetadata::Subtitle {
                            format,
                            total,
                            version,
                            ..
                        },
                    ..
                } => Response::Ok(match request {
                    Request::ReadSubtitle { .. } => Reply::SubtitleChunk {
                        format,
                        total,
                        version,
                        bytes: data,
                    },
                    _ => Reply::Subtitle {
                        format,
                        bytes: data,
                    },
                }),
                other => panic!("unexpected subtitle: {other:?}"),
            };
        }
        self.wire(&spoolframe_protocol::rpc::RpcRequest {
            envelope: RequestEnvelope {
                session: session.cloned(),
                request,
            },
            operation: Some(self.operation()),
            expected_revision,
        })
        .await
        .reply
        .response
    }

    pub async fn resource(
        &self,
        request: &spoolframe_protocol::resource::ResourceRequest,
    ) -> (spoolframe_protocol::resource::ResourceResponse, Vec<u8>) {
        let (mut send, mut recv) = self.conn.open_bi().await.unwrap();
        send.write_u8(bi::RESOURCE).await.unwrap();
        write_frame(&mut send, request).await.unwrap();
        send.finish().unwrap();
        let response = read_frame(&mut recv).await.unwrap();
        let bytes = recv.read_to_end(64 << 20).await.unwrap();
        (response, bytes)
    }

    /// `Hello`, as a device named `device_name`.
    pub async fn hello_as(&self, device_name: Option<&str>) -> Response {
        self.hello_with(device_name, None).await
    }

    /// `Hello` with everything a client can say, from a Linux app.
    pub async fn hello_with(
        &self,
        device_name: Option<&str>,
        capabilities: Option<ClientCapabilities>,
    ) -> Response {
        self.hello_from(spoolframe_types::Platform::Linux, device_name, capabilities)
            .await
    }

    /// The server id the server names in `ServerInfo` (PX-14): what an app
    /// makes its passkeys' relying party of, once pinned.
    pub async fn server_id(&self) -> spoolframe_protocol::ServerId {
        match self.hello_as(None).await {
            Response::Ok(Reply::ServerInfo(info)) => info.server_id.parse().unwrap(),
            other => panic!("no ServerInfo: {other:?}"),
        }
    }

    /// `Hello` from an app on `platform`.
    pub async fn hello_from(
        &self,
        platform: spoolframe_types::Platform,
        device_name: Option<&str>,
        capabilities: Option<ClientCapabilities>,
    ) -> Response {
        self.call(
            None,
            Request::Hello(Hello {
                protocol_version: PROTOCOL_VERSION,
                client_name: "test".into(),
                client_version: "0".into(),
                platform,
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
            remember: false,
        });
        match self.call(None, request).await {
            Response::Ok(Reply::LoggedIn { session, .. }) => session,
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
        use spoolframe_protocol::resource::{ResourceMetadata, ResourceRequest, ResourceResponse};
        let (response, bytes) = self
            .resource(&ResourceRequest::Artwork {
                request: ArtworkRequest {
                    session: session.clone(),
                    id,
                    size,
                },
                max_bytes: 64 << 20,
            })
            .await;
        let response = match response {
            ResourceResponse::Ready {
                length,
                metadata: ResourceMetadata::Artwork { content_type, etag },
            } => ArtworkResponse::Ok(spoolframe_protocol::ArtworkReply {
                length,
                content_type,
                etag,
            }),
            ResourceResponse::Error { error } => ArtworkResponse::Err(error),
            other => panic!("unexpected artwork: {other:?}"),
        };
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
            variant: 0,
        })
        .await
    }

    /// One MEDIA stream: `read` out, the response frame and the bytes back.
    pub async fn media(&self, read: MediaRead) -> (MediaReadResponse, Vec<u8>) {
        use spoolframe_protocol::resource::{ResourceMetadata, ResourceRequest, ResourceResponse};
        let revision = match self.revision(&read.session, &read.handle).await {
            Ok(revision) => revision,
            Err(error) => return (MediaReadResponse::Err(error), Vec::new()),
        };
        let (response, bytes) = self
            .resource(&ResourceRequest::Media {
                request: read,
                revision,
            })
            .await;
        let response = match response {
            ResourceResponse::Ready {
                length,
                metadata:
                    ResourceMetadata::Media {
                        size,
                        complete,
                        epoch,
                        ..
                    },
            } => MediaReadResponse::Ok(spoolframe_protocol::MediaReadReply {
                sent: length,
                size,
                complete,
                epoch,
            }),
            ResourceResponse::Error { error } => MediaReadResponse::Err(error),
            other => panic!("unexpected media: {other:?}"),
        };
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

/// The warnings the servers in this test binary log, kept so a test can
/// check that something was said. All tests share them: look for a line
/// by what only that test's server would say.
pub mod logs {
    use parking_lot::Mutex;
    use std::io::Write;
    use std::sync::OnceLock;

    static LINES: OnceLock<&'static Mutex<Vec<u8>>> = OnceLock::new();

    struct Kept(&'static Mutex<Vec<u8>>);

    impl Write for Kept {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
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
        let lines = LINES.get().expect("logs::keep() first").lock();
        String::from_utf8_lossy(&lines)
            .lines()
            .any(|line| parts.iter().all(|part| line.contains(part)))
    }
}
