//! Server configuration — schema `spoolframe.server.v1`, written in UCL.
//!
//! Every section is `deny_unknown_fields`, and a repeated key is an error:
//! a misspelt or duplicated setting fails at startup instead of being
//! quietly ignored.
//!
//! A server's settings come from three layers merged key by key
//! ([`layers`], P3-10): the built-in defaults, the provided file, and the
//! runtime layer the server writes itself. [`ServerConfig`] is what they
//! make together.

pub mod layers;
pub mod secrets;
pub mod settings;
pub mod tree;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use spoolframe_admin_api::SyncDirection;
use spoolframe_media::encoders::{Hardware, HardwarePolicy, TranscodeSettings};
use spoolframe_plex::{PathMap, PathMapping};
use spoolframe_types::LibraryKind;

use crate::encode::JobLimits;
use crate::network::{EndpointOptions, Relay};
use crate::previews::{PreviewSettings, When};
use crate::subtitles::OcrSettings;
use crate::watch::WatchRules;
use std::time::Duration;
use uclicious::Uclicious;
use url::Url;

/// Why a configuration could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read.
    #[error("cannot read config {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The document is not valid UCL, or does not match the schema.
    #[error("invalid config: {0}")]
    Ucl(String),
    /// The `schema` key names a different schema.
    #[error("config schema is {found:?}, expected {expected:?}")]
    SchemaMismatch {
        expected: &'static str,
        found: String,
    },
    /// A value has the right type but cannot be used.
    #[error("invalid config: {key}: {message}")]
    Invalid { key: &'static str, message: String },
    /// The Plex token file could not be read.
    #[error("cannot read plex.token_file {path}: {source}")]
    Token {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The Plex token file holds no token.
    #[error("plex.token_file {path} is empty")]
    EmptyToken { path: PathBuf },
    /// A layer's mistake, with the file it is in.
    #[error("{}: {source}", path.display())]
    File {
        path: PathBuf,
        source: Box<ConfigError>,
    },
    /// The provided file names the state directory, which only the
    /// command line or the environment say now (P3-10).
    #[error(
        "{}: state_dir is no longer read from the config file: remove it, and pass \
         --state-dir {value} (or set SPOOLFRAME_STATE_DIR={value}) instead",
        path.display()
    )]
    StateDirKey { path: PathBuf, value: String },
    /// Nothing says where the state directory is.
    #[error(
        "no state directory: pass --state-dir <dir> or set SPOOLFRAME_STATE_DIR \
         (or XDG_STATE_HOME, or HOME)"
    )]
    NoStateDir,
}

/// A parsed `spoolframe.server.v1` config.
#[derive(Debug, Clone, Uclicious)]
#[ucl(deny_unknown_fields)]
pub struct ServerConfig {
    /// Schema discriminator; must be `spoolframe.server.v1`.
    pub schema: String,
    /// What clients show for this server; "Spoolframe" if left out.
    #[ucl(default)]
    pub name: Option<String>,
    /// The Plex Media Servers the catalog reads from. Without one the
    /// server is not set up (P3-10): clients are told so.
    #[ucl(default)]
    pub plex: Option<PlexConfig>,
    /// The libraries clients see, in order (P3-11). Left out, every
    /// library of the first Plex server, under Plex's names: what a server
    /// configured before libraries were chosen has always served.
    #[ucl(default)]
    pub libraries: Option<Vec<LibraryConfig>>,
    /// The Iroh endpoint clients connect to.
    #[ucl(default)]
    pub network: NetworkConfig,
    /// The admin API (JSON-RPC over WebSocket) the CLI and web UI use.
    #[ucl(default)]
    pub admin: AdminConfig,
    /// The artwork cache.
    #[ucl(default)]
    pub artwork: ArtworkConfig,
    /// When an item counts as started and watched.
    #[ucl(default)]
    pub watch: WatchConfig,
    /// The ffmpeg tools the server runs.
    #[ucl(default)]
    pub media: MediaConfig,
    /// Which encoders to use (P2-02).
    #[ucl(default)]
    pub transcode: TranscodeConfig,
    /// Bitmap subtitles read as text (P2-07).
    #[ucl(default)]
    pub subtitles: SubtitlesConfig,
    /// Frame previews for scrubbing (P3-13).
    #[ucl(default)]
    pub previews: PreviewsConfig,
    /// The setup wizard's progress (P3-10): the server's own, kept in the
    /// runtime layer, not a setting.
    #[ucl(default)]
    pub setup: SetupConfig,
}

/// `setup { done = ["admin", "server"]; finished = false; }`: written by
/// the server as the wizard's steps are confirmed.
#[derive(Debug, Clone, Default, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct SetupConfig {
    /// The ids of the confirmed steps.
    #[ucl(default)]
    pub done: Vec<String>,
    #[ucl(default)]
    pub finished: Option<bool>,
}

/// `watch { started_after_secs = 60; watched_percent = 95; }`, both
/// optional.
#[derive(Debug, Clone, Default, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct WatchConfig {
    #[ucl(default)]
    pub started_after_secs: Option<u64>,
    #[ucl(default)]
    pub watched_percent: Option<u64>,
}

impl WatchConfig {
    /// The rules, defaults filled in.
    pub fn rules(&self) -> Result<WatchRules, ConfigError> {
        let defaults = WatchRules::default();
        let watched_fraction = match self.watched_percent {
            None => defaults.watched_fraction,
            Some(percent) if (1..=100).contains(&percent) => percent as f64 / 100.0,
            Some(percent) => {
                return Err(ConfigError::Invalid {
                    key: "watch.watched_percent",
                    message: format!("{percent} is not between 1 and 100"),
                });
            }
        };
        Ok(WatchRules {
            started_after: self
                .started_after_secs
                .map_or(defaults.started_after, Duration::from_secs),
            watched_fraction,
            ..defaults
        })
    }
}

/// `media { ffmpeg = "ffmpeg"; ffprobe = "ffprobe"; tesseract = "tesseract"; }`,
/// all optional: by default whatever is on `PATH`, which the nix package
/// wraps.
#[derive(Debug, Clone, Default, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct MediaConfig {
    #[ucl(default)]
    pub ffmpeg: Option<PathBuf>,
    #[ucl(default)]
    pub ffprobe: Option<PathBuf>,
    #[ucl(default)]
    pub tesseract: Option<PathBuf>,
}

impl MediaConfig {
    pub fn ffmpeg(&self) -> PathBuf {
        self.ffmpeg.clone().unwrap_or_else(|| "ffmpeg".into())
    }

    pub fn ffprobe(&self) -> PathBuf {
        self.ffprobe.clone().unwrap_or_else(|| "ffprobe".into())
    }

    pub fn tesseract(&self) -> PathBuf {
        self.tesseract.clone().unwrap_or_else(|| "tesseract".into())
    }
}

/// `subtitles { ocr = true; ocr_threads = 4; cache_dir = "…"; }`, all
/// optional.
#[derive(Debug, Clone, Default, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct SubtitlesConfig {
    /// Read bitmap subtitles as text; on if left out (it is off anyway
    /// without tesseract).
    #[ucl(default)]
    pub ocr: Option<bool>,
    /// tesseract processes at once; half the CPUs if left out.
    #[ucl(default)]
    pub ocr_threads: Option<u32>,
    /// Where the text is kept; `state_dir/cache/subtitles` if left out.
    #[ucl(default)]
    pub cache_dir: Option<PathBuf>,
}

impl SubtitlesConfig {
    pub fn ocr_settings(&self, state_dir: &Path) -> Result<OcrSettings, ConfigError> {
        let threads = match self.ocr_threads {
            Some(0) => {
                return Err(ConfigError::Invalid {
                    key: "subtitles.ocr_threads",
                    message: "0 would never read anything; use at least 1, or ocr = false".into(),
                });
            }
            Some(n) => n as usize,
            None => std::thread::available_parallelism().map_or(1, |n| (n.get() / 2).max(1)),
        };
        Ok(OcrSettings {
            enabled: self.ocr.unwrap_or(true),
            threads,
            cache_dir: self
                .cache_dir
                .clone()
                .unwrap_or_else(|| state_dir.join("cache").join("subtitles")),
        })
    }
}

/// `previews { when = "ahead"; interval_secs = 10; cache_mb = 4096; }`,
/// all optional.
#[derive(Debug, Clone, Default, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct PreviewsConfig {
    /// `ahead`, `on_play` or `never`; `ahead` if left out.
    #[ucl(default)]
    pub when: Option<String>,
    /// Seconds between two pictures; 10 if left out.
    #[ucl(default)]
    pub interval_secs: Option<u64>,
    /// The most `state_dir/cache/previews` may hold, in MiB; 4096 if left
    /// out.
    #[ucl(default)]
    pub cache_mb: Option<u64>,
}

impl PreviewsConfig {
    pub const DEFAULT_INTERVAL_SECS: u64 = 10;
    pub const DEFAULT_CACHE_MB: u64 = 4096;

    pub fn settings(&self) -> Result<PreviewSettings, ConfigError> {
        let when = match self.when.as_deref().unwrap_or("ahead") {
            "ahead" => When::Ahead,
            "on_play" => When::OnPlay,
            "never" => When::Never,
            other => {
                return Err(ConfigError::Invalid {
                    key: "previews.when",
                    message: format!("{other:?}: expected ahead, on_play or never"),
                });
            }
        };
        let interval = self.interval_secs.unwrap_or(Self::DEFAULT_INTERVAL_SECS);
        if interval == 0 {
            return Err(ConfigError::Invalid {
                key: "previews.interval_secs",
                message: "0 would be a picture for every moment; use at least 1".into(),
            });
        }
        Ok(PreviewSettings {
            when,
            interval: Duration::from_secs(interval),
            cache_limit_bytes: self.cache_mb.unwrap_or(Self::DEFAULT_CACHE_MB) << 20,
        })
    }
}

/// `transcode { hardware = "nvenc,vaapi"; vaapi_device = "/dev/dri/renderD128";
/// max_jobs = 2; scratch_dir = "…"; scratch_limit_mb = 20480;
/// read_ahead_secs = 60; }`, all optional.
#[derive(Debug, Clone, Default, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct TranscodeConfig {
    /// `auto` (every family that works), `none` for software only, or
    /// families in order of preference, comma-separated: `nvenc,vaapi`
    /// encodes on NVIDIA what it can and the rest on VA-API.
    #[ucl(default)]
    pub hardware: Option<String>,
    #[ucl(default)]
    pub vaapi_device: Option<PathBuf>,
    /// Encodes at once; 2 if left out.
    #[ucl(default)]
    pub max_jobs: Option<u32>,
    /// Where encodes write, in a directory of their own made there;
    /// `state_dir/cache/transcode` if left out.
    #[ucl(default)]
    pub scratch_dir: Option<PathBuf>,
    /// The most all encodes may write together, in MiB; 20 GiB if left out.
    #[ucl(default)]
    pub scratch_limit_mb: Option<u64>,
    /// How far an encode may run ahead of its reader; 60 s if left out.
    /// Any value works: 0 holds the encode whenever its reader is behind.
    #[ucl(default)]
    pub read_ahead_secs: Option<u64>,
}

impl TranscodeConfig {
    /// The job limits, with `state_dir` for the default scratch directory.
    pub fn limits(&self, state_dir: &Path) -> Result<JobLimits, ConfigError> {
        let max_jobs = self.max_jobs.unwrap_or(2);
        if max_jobs == 0 {
            return Err(ConfigError::Invalid {
                key: "transcode.max_jobs",
                message: "0 would refuse every conversion; use at least 1".into(),
            });
        }
        Ok(JobLimits {
            max_jobs: max_jobs as usize,
            scratch_dir: self
                .scratch_dir
                .clone()
                .unwrap_or_else(|| state_dir.join("cache").join("transcode")),
            scratch_limit_bytes: self.scratch_limit_mb.unwrap_or(20 * 1024) << 20,
            read_ahead: Duration::from_secs(self.read_ahead_secs.unwrap_or(60)),
            stalled_after: crate::encode::job::STALLED_AFTER,
        })
    }

    pub fn settings(&self) -> Result<TranscodeSettings, ConfigError> {
        let invalid = |message: String| ConfigError::Invalid {
            key: "transcode.hardware",
            message,
        };
        let hardware = match self.hardware.as_deref().unwrap_or("auto").trim() {
            "auto" => HardwarePolicy::Auto,
            "none" => HardwarePolicy::None,
            list => {
                let mut order = Vec::new();
                for name in list.split(',').map(str::trim) {
                    let family = match name {
                        "vaapi" => Hardware::Vaapi,
                        "nvenc" => Hardware::Nvenc,
                        "qsv" => Hardware::Qsv,
                        other => {
                            return Err(invalid(format!(
                                "{other:?}: expected auto, none, or a list of vaapi, nvenc and qsv"
                            )));
                        }
                    };
                    if order.contains(&family) {
                        return Err(invalid(format!("{name:?} is listed twice")));
                    }
                    order.push(family);
                }
                HardwarePolicy::Ordered(order)
            }
        };
        let defaults = TranscodeSettings::default();
        Ok(TranscodeSettings {
            hardware,
            vaapi_device: self.vaapi_device.clone().unwrap_or(defaults.vaapi_device),
        })
    }
}

/// `artwork { cache_mb = 1024; }`, optional.
#[derive(Debug, Clone, Default, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct ArtworkConfig {
    /// The most the cache under `state_dir/cache/artwork` may hold, in
    /// MiB; 1024 if left out.
    #[ucl(default)]
    pub cache_mb: Option<u64>,
}

impl ArtworkConfig {
    pub const DEFAULT_CACHE_MB: u64 = 1024;

    /// The cache limit in bytes.
    pub fn cache_bytes(&self) -> u64 {
        self.cache_mb.unwrap_or(Self::DEFAULT_CACHE_MB) << 20
    }
}

/// `admin { listen = "0.0.0.0:7421"; public_url = "https://media.example.org";
/// trusted_proxies = "10.0.0.5, 172.17.0.0/16"; insecure_cookies = false; }`,
/// all optional.
#[derive(Debug, Clone, Default, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct AdminConfig {
    /// Where the admin API and the web UI listen; every IPv4 interface on
    /// port 7421 if left out (P3-10). Loopback, for a host whose TLS proxy
    /// runs on the same machine.
    #[ucl(default)]
    pub listen: Option<String>,
    /// Where browsers reach the web UI, when it is behind a TLS proxy
    /// (P3-01): the only origin then allowed to sign in and use it. Left
    /// out, a browser must be on the listener's own host, as it is directly
    /// or through an SSH tunnel. Its host is also what passkeys are bound
    /// to (P3-05; `localhost` when left out): they need HTTPS, and one made
    /// under one host works under no other, so changing this orphans them
    /// (the log names each at start).
    #[ucl(default)]
    pub public_url: Option<String>,
    /// The reverse proxies in front of the listener, as addresses and CIDR
    /// ranges, comma-separated. A connection from one of them is counted
    /// by the failure limit under the caller its `X-Forwarded-For` names
    /// (the right-most address that is not a trusted proxy; `Forwarded` is
    /// not read), and under no address if it names none. Left out, no
    /// header is read: every connection counts under its own address, and
    /// a loopback one under none.
    #[ucl(default)]
    pub trusted_proxies: Option<String>,
    /// Send the session cookie without `Secure` and `HttpOnly`, so signing
    /// in over plain HTTP on the LAN works: the cookie then crosses the
    /// network in clear, and page scripts can read it. Off if left out.
    #[ucl(default)]
    pub insecure_cookies: Option<bool>,
}

impl AdminConfig {
    pub const DEFAULT_LISTEN: &'static str = "0.0.0.0:7421";

    pub fn insecure_cookies(&self) -> bool {
        self.insecure_cookies.unwrap_or(false)
    }

    /// The origins browsers may use the admin listener from: `public_url`'s
    /// (scheme, host and port; any path dropped), or the listener's own.
    pub fn origins(&self) -> Result<crate::admin::Origins, ConfigError> {
        let Some(text) = &self.public_url else {
            return Ok(crate::admin::Origins::SameHost);
        };
        let invalid = |message: String| ConfigError::Invalid {
            key: "admin.public_url",
            message,
        };
        let url = url::Url::parse(text).map_err(|e| invalid(format!("{text:?}: {e}")))?;
        if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
            return Err(invalid(format!("{text:?} is not an http(s) URL")));
        }
        Ok(crate::admin::Origins::Exactly(
            url.origin().ascii_serialization(),
        ))
    }

    /// The proxies whose `X-Forwarded-For` is believed; none if left out.
    pub fn trusted_proxies(&self) -> Result<crate::admin::TrustedProxies, ConfigError> {
        let Some(list) = &self.trusted_proxies else {
            return Ok(crate::admin::TrustedProxies::default());
        };
        list.parse().map_err(|message| ConfigError::Invalid {
            key: "admin.trusted_proxies",
            message,
        })
    }

    /// The address to listen on.
    pub fn listen(&self) -> Result<SocketAddr, ConfigError> {
        let text = self.listen.as_deref().unwrap_or(Self::DEFAULT_LISTEN);
        text.parse().map_err(|e| ConfigError::Invalid {
            key: "admin.listen",
            message: format!("{text:?}: {e}"),
        })
    }
}

/// `network { port = 7420; relay = "disabled"; mdns = true; }`, all
/// optional.
#[derive(Debug, Clone, Default, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct NetworkConfig {
    /// The UDP port to listen on; a free one is picked if left out. Fix it
    /// when a firewall needs to know.
    #[ucl(default)]
    pub port: Option<u16>,
    /// `"disabled"` (the default), `"default"` for the public n0 relays,
    /// or a relay URL. Phase 1 is LAN only and accepts only `"disabled"`;
    /// the others are parsed so remote access (P6) only has to lift that.
    #[ucl(default)]
    pub relay: Option<String>,
    /// Announce the server on the LAN over mDNS (P1-13), so clients list
    /// it without being told its address. On unless set to `false`.
    #[ucl(default)]
    pub mdns: Option<bool>,
}

impl NetworkConfig {
    /// How to build the endpoint.
    pub fn endpoint_options(&self) -> Result<EndpointOptions, ConfigError> {
        let invalid = |message: String| ConfigError::Invalid {
            key: "network.relay",
            message,
        };
        let relay = match self.relay.as_deref() {
            None | Some("disabled") => Relay::Disabled,
            Some("default") => Relay::Default,
            Some(url) => {
                Relay::Custom(Url::parse(url).map_err(|e| invalid(format!("{url:?}: {e}")))?)
            }
        };
        if relay != Relay::Disabled {
            return Err(invalid(format!(
                "{:?}: only \"disabled\" is supported until remote access",
                self.relay.as_deref().unwrap_or_default()
            )));
        }
        Ok(EndpointOptions {
            port: self.port,
            relay,
            mdns: self.mdns.unwrap_or(true),
        })
    }
}

/// The id of the Plex server `plex { url = …; }` names: the first one,
/// whose items' IDs have no server in them (`plex:movie:12`), as they had
/// before a server could have several (P3-11).
pub const FIRST_PLEX_SERVER: &str = "plex";

/// `plex { ... }`: where the first PMS is, how to authenticate to it, and
/// the further Plex servers (P3-11).
#[derive(Debug, Clone, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct PlexConfig {
    /// The PMS base URL on the LAN, e.g. `http://192.168.1.20:32400`.
    /// Never plex.tv: the catalog must work without the Internet (§32).
    pub url: String,
    /// What Spoolframe calls the server (P3-11): Plex's own name when it
    /// was added, so a server that is down is still named. Its address's
    /// host if left out.
    #[ucl(default)]
    pub name: Option<String>,
    /// A file holding the `X-Plex-Token`. Leave it out for a PMS that lets
    /// LAN clients in without auth. A file, not the token itself, so the
    /// secret stays out of the config.
    #[ucl(default)]
    pub token_file: Option<PathBuf>,
    /// Rewrites from the paths Plex reports to the paths this host reads
    /// (ARCHITECTURE §7).
    #[ucl(default)]
    pub path_mapping: Vec<PathMappingConfig>,
    /// Two-way watch state sync with the Plex account this server reads
    /// as (PX-04). Off unless set.
    #[ucl(default)]
    pub watch_sync: Option<WatchSyncConfig>,
    /// Further Plex servers (P3-11), each under an id of its own: their
    /// items' IDs carry it (`plex:movie:cabin:12`), so it never changes.
    #[ucl(default)]
    pub servers: Vec<PlexServerConfig>,
}

/// `{ id = "cabin"; name = "Cabin"; url = "http://10.0.1.5:32400";
/// token_file = "…"; path_mapping [ … ]; }`: a Plex server after the first.
#[derive(Debug, Clone, PartialEq, Eq, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct PlexServerConfig {
    /// Lower-case letters, digits and dashes, starting with a letter; not
    /// `plex`, which is the first server's.
    pub id: String,
    #[ucl(default)]
    pub name: Option<String>,
    pub url: String,
    #[ucl(default)]
    pub token_file: Option<PathBuf>,
    #[ucl(default)]
    pub path_mapping: Vec<PathMappingConfig>,
}

/// `{ name = "Films"; kind = "movies"; plex { server = "plex"; section = "2"; } }`:
/// one library as clients see it (P3-11). Its place in `libraries` is its
/// position.
#[derive(Debug, Clone, PartialEq, Eq, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct LibraryConfig {
    /// What clients call it; Plex keeps its own name.
    pub name: String,
    /// `movies`, `shows` or `music`: what it holds, kept so that a library
    /// whose Plex server is down is still listed, as unavailable.
    pub kind: String,
    /// Where it comes from. Plex is the only source until Spoolframe has
    /// libraries of its own (P9).
    pub plex: PlexLibraryConfig,
}

/// `plex { server = "cabin"; section = "3"; }`: one library section of a
/// configured Plex server.
#[derive(Debug, Clone, PartialEq, Eq, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct PlexLibraryConfig {
    /// `plex` for the first server, else one of `plex.servers`' ids.
    pub server: String,
    /// The section's key in Plex, as `/library/sections` lists it.
    pub section: String,
    /// Which way watch state syncs with each linked user's Plex account
    /// (P3-12): `off`, `from_plex`, `to_plex` or `both`. Left out, `both`
    /// for the first server's libraries while `plex.watch_sync.user` is
    /// set, as PX-04 synced them, and `off` otherwise.
    #[ucl(default)]
    pub sync: Option<String>,
}

impl LibraryConfig {
    pub fn kind(&self) -> Result<LibraryKind, ConfigError> {
        library_kind(&self.kind).ok_or_else(|| ConfigError::Invalid {
            key: "libraries",
            message: format!(
                "{:?}: kind {:?} is not movies, shows or music",
                self.name, self.kind
            ),
        })
    }
}

/// A library kind by its name in the config: as on the wire.
pub fn library_kind(name: &str) -> Option<LibraryKind> {
    match name {
        "movies" => Some(LibraryKind::Movies),
        "shows" => Some(LibraryKind::Shows),
        "music" => Some(LibraryKind::Music),
        _ => None,
    }
}

/// The name a library kind has in the config.
pub fn library_kind_name(kind: LibraryKind) -> &'static str {
    match kind {
        LibraryKind::Movies => "movies",
        LibraryKind::Shows => "shows",
        LibraryKind::Music => "music",
    }
}

/// Whether `id` may name a Plex server after the first.
fn is_server_id(id: &str) -> bool {
    id.len() <= 32
        && id.starts_with(|c: char| c.is_ascii_lowercase())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && id != FIRST_PLEX_SERVER
}

/// A PMS base URL, checked: http or https.
pub(crate) fn plex_url(text: &str, key: &'static str) -> Result<Url, ConfigError> {
    let url = Url::parse(text).map_err(|e| ConfigError::Invalid {
        key,
        message: format!("{text:?}: {e}"),
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ConfigError::Invalid {
            key,
            message: format!("{text:?}: not an http or https URL"),
        });
    }
    Ok(url)
}

/// Path mappings, each with absolute `from` and `to`.
pub(crate) fn path_map(
    mappings: &[PathMappingConfig],
    key: &'static str,
) -> Result<PathMap, ConfigError> {
    mappings
        .iter()
        .map(|m| {
            for (side, path) in [("from", &m.from), ("to", &m.to)] {
                if !path.is_absolute() {
                    return Err(ConfigError::Invalid {
                        key,
                        message: format!("{side} {:?} is not an absolute path", path),
                    });
                }
            }
            Ok(PathMapping {
                library: m.library.clone(),
                from: m.from.clone(),
                to: m.to.clone(),
            })
        })
        .collect::<Result<_, _>>()
        .map(PathMap::new)
}

/// A token file's token, without surrounding whitespace.
fn read_token(path: &Path) -> Result<String, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Token {
        path: path.to_owned(),
        source,
    })?;
    let token = text.trim();
    if token.is_empty() {
        return Err(ConfigError::EmptyToken {
            path: path.to_owned(),
        });
    }
    Ok(token.to_owned())
}

impl PlexServerConfig {
    pub fn url(&self) -> Result<Url, ConfigError> {
        plex_url(&self.url, "plex.servers")
    }

    pub fn path_map(&self) -> Result<PathMap, ConfigError> {
        path_map(&self.path_mapping, "plex.servers")
    }

    /// The token from `token_file`, or `None` when there is none.
    pub fn token(&self) -> Result<Option<String>, ConfigError> {
        self.token_file.as_deref().map(read_token).transpose()
    }

    /// What Spoolframe calls it: its name, else its address's host.
    pub fn display_name(&self) -> String {
        display_name(self.name.as_deref(), &self.url)
    }
}

fn display_name(name: Option<&str>, url: &str) -> String {
    name.map(str::to_owned)
        .or_else(|| {
            Url::parse(url)
                .ok()
                .and_then(|u| u.host_str().map(str::to_owned))
        })
        .unwrap_or_else(|| url.to_owned())
}

/// `watch_sync { user = "andoriyu"; interval_secs = 120; }`
#[derive(Debug, Clone, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct WatchSyncConfig {
    /// The Spoolframe user PX-04 synced with the Plex servers' owner: now
    /// that user's Plex link (P3-12), made with each server's own token
    /// (or none, for a LAN PMS that lets clients in without one) rather
    /// than one from Sign in with Plex. Optional, so the interval can be
    /// set without it.
    #[ucl(default)]
    pub user: Option<String>,
    /// How often to sync when nothing changes here; 120 if left out.
    #[ucl(default)]
    pub interval_secs: Option<u64>,
}

impl WatchSyncConfig {
    pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(120);

    pub fn interval(&self) -> Duration {
        self.interval_secs
            .map_or(Self::DEFAULT_INTERVAL, |s| Duration::from_secs(s.max(10)))
    }
}

/// `{ library = "Music"; from = "/data"; to = "/mnt/nas/music"; }`
#[derive(Debug, Clone, PartialEq, Eq, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct PathMappingConfig {
    /// The Plex library title this rule is for; every library if left out.
    #[ucl(default)]
    pub library: Option<String>,
    /// The prefix as Plex reports it.
    pub from: PathBuf,
    /// The same directory on this host.
    pub to: PathBuf,
}

impl PlexConfig {
    /// The Spoolframe user linked as the Plex servers' owner by the config
    /// (`watch_sync.user`), if any.
    pub fn owner(&self) -> Option<&str> {
        self.watch_sync.as_ref()?.user.as_deref()
    }

    /// How often each linked user is synced when nothing changes.
    pub fn sync_interval(&self) -> Duration {
        self.watch_sync
            .as_ref()
            .map_or(WatchSyncConfig::DEFAULT_INTERVAL, WatchSyncConfig::interval)
    }

    /// Which way a library on `server` syncs when it does not say: `both`
    /// for the first server's while an owner is set, which is everything
    /// PX-04 synced, so a config written for it syncs as it did; `off`
    /// otherwise, so a library syncs only once an admin says it should.
    pub fn default_sync(&self, server: &str) -> SyncDirection {
        if self.owner().is_some() && server == FIRST_PLEX_SERVER {
            SyncDirection::Both
        } else {
            SyncDirection::Off
        }
    }

    /// Which way `library` syncs.
    pub fn sync(&self, library: &PlexLibraryConfig) -> SyncDirection {
        library
            .sync
            .as_deref()
            .and_then(SyncDirection::from_config_name)
            .unwrap_or_else(|| self.default_sync(&library.server))
    }

    /// The PMS base URL.
    pub fn url(&self) -> Result<Url, ConfigError> {
        plex_url(&self.url, "plex.url")
    }

    /// The path mappings, each with absolute `from` and `to`.
    pub fn path_map(&self) -> Result<PathMap, ConfigError> {
        path_map(&self.path_mapping, "plex.path_mapping")
    }

    /// The token from `token_file`, without surrounding whitespace, or
    /// `None` when no file is configured. Read at startup, not at parse
    /// time, so the config can be checked where the secret is not present.
    pub fn token(&self) -> Result<Option<String>, ConfigError> {
        self.token_file.as_deref().map(read_token).transpose()
    }

    /// Every Plex server, the first (as id [`FIRST_PLEX_SERVER`]) first,
    /// then `servers` in order.
    pub fn all_servers(&self) -> Vec<PlexServerConfig> {
        let first = PlexServerConfig {
            id: FIRST_PLEX_SERVER.to_owned(),
            name: self.name.clone(),
            url: self.url.clone(),
            token_file: self.token_file.clone(),
            path_mapping: self.path_mapping.clone(),
        };
        std::iter::once(first)
            .chain(self.servers.iter().cloned())
            .collect()
    }

    /// The servers' ids and URLs checked, and every library's source a
    /// server of these.
    fn check(&self, libraries: Option<&[LibraryConfig]>) -> Result<(), ConfigError> {
        self.url()?;
        self.path_map()?;
        let mut ids = vec![FIRST_PLEX_SERVER];
        for server in &self.servers {
            if !is_server_id(&server.id) {
                return Err(ConfigError::Invalid {
                    key: "plex.servers",
                    message: format!(
                        "id {:?}: use lower-case letters, digits and dashes, starting with a \
                         letter, at most 32, and not \"plex\" (the first server's)",
                        server.id
                    ),
                });
            }
            if ids.contains(&server.id.as_str()) {
                return Err(ConfigError::Invalid {
                    key: "plex.servers",
                    message: format!("id {:?} is used twice", server.id),
                });
            }
            ids.push(&server.id);
            server.url()?;
            server.path_map()?;
        }
        let mut sources: Vec<&PlexLibraryConfig> = Vec::new();
        for library in libraries.unwrap_or_default() {
            let invalid = |message: String| ConfigError::Invalid {
                key: "libraries",
                message: format!("{:?}: {message}", library.name),
            };
            library.kind()?;
            if library.name.trim().is_empty() {
                return Err(invalid("a library needs a name".into()));
            }
            let source = &library.plex;
            if !ids.contains(&source.server.as_str()) {
                return Err(invalid(format!(
                    "no Plex server {:?}: \"plex\" is the first, and the others are \
                     plex.servers' ids",
                    source.server
                )));
            }
            if source.section.is_empty() || !source.section.bytes().all(|b| b.is_ascii_digit()) {
                return Err(invalid(format!(
                    "section {:?} is not a Plex library key",
                    source.section
                )));
            }
            if let Some(sync) = &source.sync
                && SyncDirection::from_config_name(sync).is_none()
            {
                return Err(invalid(format!(
                    "sync {sync:?} is not off, from_plex, to_plex or both"
                )));
            }
            if sources
                .iter()
                .any(|s| (&s.server, &s.section) == (&source.server, &source.section))
            {
                return Err(invalid(format!(
                    "section {} of {:?} is already a library",
                    source.section, source.server
                )));
            }
            sources.push(source);
        }
        Ok(())
    }
}

impl ServerConfig {
    /// The server's display name.
    pub fn server_name(&self) -> &str {
        self.name.as_deref().unwrap_or("Spoolframe")
    }

    /// The `schema` value this type accepts.
    pub const SCHEMA: &'static str = "spoolframe.server.v1";

    /// Parse a UCL document.
    pub fn parse(input: &str) -> Result<Self, ConfigError> {
        let ucl = |e: uclicious::UclError| ConfigError::Ucl(e.to_string());
        let mut builder = Self::builder().map_err(ucl)?;
        builder
            .add_chunk_full(
                input,
                uclicious::Priority::default(),
                uclicious::DuplicateStrategy::UCL_DUPLICATE_ERROR,
            )
            .map_err(ucl)?;
        let parsed = builder
            .build()
            .map_err(|e| ConfigError::Ucl(e.to_string()))?;
        if parsed.schema != Self::SCHEMA {
            return Err(ConfigError::SchemaMismatch {
                expected: Self::SCHEMA,
                found: parsed.schema,
            });
        }
        match (&parsed.plex, &parsed.libraries) {
            (Some(plex), libraries) => plex.check(libraries.as_deref())?,
            (None, Some(libraries)) if !libraries.is_empty() => {
                return Err(ConfigError::Invalid {
                    key: "libraries",
                    message: "every library comes from a Plex server, and none is configured"
                        .into(),
                });
            }
            (None, _) => {}
        }
        parsed.network.endpoint_options()?;
        parsed.admin.listen()?;
        parsed.admin.origins()?;
        parsed.admin.trusted_proxies()?;
        parsed.watch.rules()?;
        parsed.transcode.settings()?;
        // Any directory: only the values are checked here.
        parsed.transcode.limits(Path::new("/"))?;
        parsed.subtitles.ocr_settings(Path::new("/"))?;
        parsed.previews.settings()?;
        Ok(parsed)
    }

    /// Parse merged layers.
    pub fn from_table(table: &tree::Table) -> Result<Self, ConfigError> {
        Self::parse(&tree::to_document(table))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minimal_config_parses() {
        let config = ServerConfig::parse(
            r#"
            schema = "spoolframe.server.v1";
            plex { url = "http://nas:32400"; }
            "#,
        )
        .unwrap();
        assert_eq!(config.plex.unwrap().url, "http://nas:32400");
    }

    #[test]
    fn an_unknown_key_is_rejected() {
        let err = ServerConfig::parse(
            r#"
            schema = "spoolframe.server.v1";
            nmae = "Den";
            plex { url = "http://nas:32400"; }
            "#,
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::Ucl(_)), "{err:?}");
    }

    #[test]
    fn a_repeated_key_is_rejected() {
        let err = ServerConfig::parse(
            r#"
            schema = "spoolframe.server.v1";
            name = "Den";
            name = "Attic";
            plex { url = "http://nas:32400"; }
            "#,
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::Ucl(_)), "{err:?}");
    }

    #[test]
    fn another_schema_is_rejected() {
        let err = ServerConfig::parse(
            r#"
            schema = "spoolframe.server.v2";
            plex { url = "http://nas:32400"; }
            "#,
        )
        .unwrap_err();
        assert!(
            matches!(err, ConfigError::SchemaMismatch { ref found, .. } if found == "spoolframe.server.v2"),
            "{err:?}"
        );
    }

    #[test]
    fn a_missing_file_names_its_path() {
        let locations = layers::Locations {
            state_dir: PathBuf::from("/nonexistent/state"),
            config: Some(PathBuf::from("/nonexistent/spoolframe.ucl")),
        };
        let err = layers::Layers::load(&locations).unwrap_err();
        assert!(
            err.to_string().contains("/nonexistent/spoolframe.ucl"),
            "{err}"
        );
    }

    fn plex(section: &str) -> Result<ServerConfig, ConfigError> {
        ServerConfig::parse(&format!(
            r#"
            schema = "spoolframe.server.v1";
            {section}
            "#
        ))
    }

    #[test]
    fn the_plex_section_is_optional() {
        assert!(plex("").unwrap().plex.is_none());
    }

    #[test]
    fn the_plex_url_and_token_file_are_read() {
        let config = plex(
            r#"plex { url = "http://192.168.1.20:32400"; token_file = "/run/secrets/plex"; }"#,
        )
        .unwrap();
        assert_eq!(
            config.plex.as_ref().unwrap().url().unwrap().as_str(),
            "http://192.168.1.20:32400/"
        );
        assert_eq!(
            config.plex.unwrap().token_file,
            Some(PathBuf::from("/run/secrets/plex"))
        );
    }

    #[test]
    fn an_unknown_key_in_the_plex_section_is_rejected() {
        let err = plex(r#"plex { url = "http://nas:32400"; token = "abc"; }"#).unwrap_err();
        assert!(matches!(err, ConfigError::Ucl(_)), "{err:?}");
    }

    #[test]
    fn a_plex_url_that_is_not_http_is_rejected() {
        let err = plex(r#"plex { url = "nas:32400"; }"#).unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::Invalid {
                    key: "plex.url",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn path_mappings_are_read_in_order() {
        let config = plex(
            r#"
            plex {
              url = "http://nas:32400";
              path_mapping [
                { from = "/data/movies"; to = "/media/movies"; },
                { library = "Music"; from = "/data"; to = "/mnt/nas/music"; },
              ]
            }
            "#,
        )
        .unwrap();
        let mappings = &config.plex.as_ref().unwrap().path_mapping;
        assert_eq!(mappings.len(), 2);
        assert_eq!(mappings[0].library, None);
        assert_eq!(mappings[1].library.as_deref(), Some("Music"));
        assert_eq!(mappings[1].to, PathBuf::from("/mnt/nas/music"));
    }

    #[test]
    fn the_network_section_is_optional_and_lan_only() {
        let config = plex(r#"plex { url = "http://nas:32400"; }"#).unwrap();
        let options = config.network.endpoint_options().unwrap();
        assert_eq!(options.relay, Relay::Disabled);
        assert_eq!(options.port, None);
        assert!(options.mdns);
    }

    #[test]
    fn transcoding_defaults_to_auto_on_the_first_render_node() {
        let config = plex(r#"plex { url = "http://nas:32400"; }"#).unwrap();
        assert_eq!(
            config.transcode.settings().unwrap(),
            TranscodeSettings::default()
        );
        assert_eq!(config.media.ffmpeg(), PathBuf::from("ffmpeg"));
    }

    #[test]
    fn ocr_is_on_with_its_cache_under_the_state_dir() {
        let config = plex(r#"plex { url = "http://nas:32400"; }"#).unwrap();
        let ocr = config
            .subtitles
            .ocr_settings(Path::new("/var/lib/sf"))
            .unwrap();
        assert!(ocr.enabled);
        assert!(ocr.threads >= 1);
        assert_eq!(ocr.cache_dir, PathBuf::from("/var/lib/sf/cache/subtitles"));
        assert_eq!(config.media.tesseract(), PathBuf::from("tesseract"));
    }

    #[test]
    fn ocr_with_no_threads_is_refused() {
        assert!(matches!(
            plex(r#"plex { url = "http://nas:32400"; } subtitles { ocr_threads = 0; }"#),
            Err(ConfigError::Invalid {
                key: "subtitles.ocr_threads",
                ..
            })
        ));
        let off = plex(
            r#"plex { url = "http://nas:32400"; } subtitles { ocr = false; ocr_threads = 2; }"#,
        )
        .unwrap()
        .subtitles
        .ocr_settings(Path::new("/s"))
        .unwrap();
        assert!(!off.enabled);
        assert_eq!(off.threads, 2);
    }

    #[test]
    fn previews_are_made_ahead_every_10_s_into_4_gib_by_default() {
        let config = plex(r#"plex { url = "http://nas:32400"; }"#).unwrap();
        assert_eq!(
            config.previews.settings().unwrap(),
            PreviewSettings {
                when: When::Ahead,
                interval: Duration::from_secs(10),
                cache_limit_bytes: 4096 << 20,
            }
        );
        let config = plex(
            r#"plex { url = "http://nas:32400"; }
               previews { when = "on_play"; interval_secs = 5; cache_mb = 100; }"#,
        )
        .unwrap();
        assert_eq!(
            config.previews.settings().unwrap(),
            PreviewSettings {
                when: When::OnPlay,
                interval: Duration::from_secs(5),
                cache_limit_bytes: 100 << 20,
            }
        );
    }

    #[test]
    fn a_previews_setting_that_cannot_work_is_refused() {
        for (text, key) in [
            (r#"previews { when = "sometimes"; }"#, "previews.when"),
            ("previews { interval_secs = 0; }", "previews.interval_secs"),
        ] {
            let text = format!(r#"plex {{ url = "http://nas:32400"; }} {text}"#);
            assert!(
                matches!(plex(&text), Err(ConfigError::Invalid { key: k, .. }) if k == key),
                "{text}"
            );
        }
    }

    #[test]
    fn job_limits_default_under_the_state_dir() {
        let config = plex(r#"plex { url = "http://nas:32400"; }"#).unwrap();
        let limits = config.transcode.limits(Path::new("/var/lib/sf")).unwrap();
        assert_eq!(limits.max_jobs, 2);
        assert_eq!(
            limits.scratch_dir,
            PathBuf::from("/var/lib/sf/cache/transcode")
        );
        assert_eq!(limits.scratch_limit_bytes, 20 << 30);
        assert_eq!(limits.read_ahead, Duration::from_secs(60));
    }

    #[test]
    fn zero_jobs_is_refused() {
        assert!(matches!(
            plex(r#"plex { url = "http://nas:32400"; } transcode { max_jobs = 0; }"#),
            Err(ConfigError::Invalid {
                key: "transcode.max_jobs",
                ..
            })
        ));
    }

    #[test]
    fn a_hardware_family_is_read_and_a_bad_one_refused() {
        let config = plex(
            r#"plex { url = "http://nas:32400"; } transcode { hardware = "none"; vaapi_device = "/dev/dri/renderD129"; }"#,
        )
        .unwrap();
        let settings = config.transcode.settings().unwrap();
        assert_eq!(settings.hardware, HardwarePolicy::None);
        assert_eq!(settings.vaapi_device, PathBuf::from("/dev/dri/renderD129"));
        let ordered =
            plex(r#"plex { url = "http://nas:32400"; } transcode { hardware = "nvenc, vaapi"; }"#)
                .unwrap();
        assert_eq!(
            ordered.transcode.settings().unwrap().hardware,
            HardwarePolicy::Ordered(vec![Hardware::Nvenc, Hardware::Vaapi])
        );
        assert!(matches!(
            plex(r#"plex { url = "http://nas:32400"; } transcode { hardware = "cuda"; }"#),
            Err(ConfigError::Invalid {
                key: "transcode.hardware",
                ..
            })
        ));
    }

    #[test]
    fn plex_watch_sync_is_off_unless_set() {
        let off = plex(r#"plex { url = "http://nas:32400"; }"#).unwrap();
        assert!(off.plex.unwrap().watch_sync.is_none());
        let on = plex(r#"plex { url = "http://nas:32400"; watch_sync { user = "andoriyu"; } }"#)
            .unwrap();
        let sync = on.plex.unwrap().watch_sync.unwrap();
        assert_eq!(sync.user.as_deref(), Some("andoriyu"));
        assert_eq!(sync.interval(), Duration::from_secs(120));
    }

    /// PX-04's config syncs what it synced: every library of the first
    /// server, both ways; nothing else syncs until an admin says so.
    #[test]
    fn a_library_syncs_as_it_says_else_as_px_04_did() {
        let owner = r#"plex { url = "http://tower:32400"; watch_sync { user = "andoriyu"; }
            servers [ { id = "cabin"; url = "http://cabin:32400"; } ] }"#;
        let config = plex(owner).unwrap();
        let plex_config = config.plex.unwrap();
        let on = |server: &str, sync: Option<&str>| PlexLibraryConfig {
            server: server.into(),
            section: "1".into(),
            sync: sync.map(Into::into),
        };
        assert_eq!(plex_config.sync(&on("plex", None)), SyncDirection::Both);
        assert_eq!(plex_config.sync(&on("cabin", None)), SyncDirection::Off);
        assert_eq!(
            plex_config.sync(&on("plex", Some("from_plex"))),
            SyncDirection::FromPlex
        );
        assert_eq!(
            plex_config.sync(&on("cabin", Some("to_plex"))),
            SyncDirection::ToPlex
        );
        let no_owner = plex(r#"plex { url = "http://tower:32400"; }"#).unwrap();
        assert_eq!(
            no_owner.plex.unwrap().sync(&on("plex", None)),
            SyncDirection::Off
        );
        refused(
            r#"plex { url = "http://tower:32400"; }
               libraries [ { name = "F"; kind = "movies"; plex { server = "plex"; section = "1"; sync = "sideways"; } } ]"#,
            "libraries",
            "not off, from_plex, to_plex or both",
        );
    }

    /// Two Plex servers and three libraries chosen from them (P3-11).
    const TWO_SERVERS: &str = r#"
        plex {
          url = "http://tower:32400";
          name = "Tower";
          servers [
            { id = "cabin"; name = "Cabin"; url = "http://cabin:32400";
              path_mapping [ { from = "/data"; to = "/mnt/cabin"; } ] },
          ]
        }
        libraries [
          { name = "Films"; kind = "movies"; plex { server = "plex"; section = "2"; } },
          { name = "Cabin movies"; kind = "movies"; plex { server = "cabin"; section = "1"; } },
          { name = "TV"; kind = "shows"; plex { server = "plex"; section = "1"; } },
        ]
    "#;

    #[test]
    fn libraries_and_further_plex_servers_are_read_in_order() {
        let config = plex(TWO_SERVERS).unwrap();
        let servers = config.plex.as_ref().unwrap().all_servers();
        let ids: Vec<_> = servers.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["plex", "cabin"]);
        assert_eq!(servers[0].display_name(), "Tower");
        assert_eq!(
            servers[1]
                .path_map()
                .unwrap()
                .map(None, Path::new("/data/a.mkv"))
                .local,
            PathBuf::from("/mnt/cabin/a.mkv")
        );
        let libraries = config.libraries.unwrap();
        let names: Vec<_> = libraries.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["Films", "Cabin movies", "TV"]);
        assert_eq!(libraries[2].kind().unwrap(), LibraryKind::Shows);
    }

    #[test]
    fn a_server_without_a_name_is_called_by_its_host() {
        let config = plex(r#"plex { url = "http://192.168.1.20:32400"; }"#).unwrap();
        assert_eq!(
            config.plex.unwrap().all_servers()[0].display_name(),
            "192.168.1.20"
        );
    }

    #[test]
    fn without_libraries_the_first_server_s_are_served_as_before() {
        // The shape of every config written before P3-11.
        let config =
            plex(r#"plex { url = "http://nas:32400"; watch_sync { user = "andoriyu"; } }"#)
                .unwrap();
        assert!(config.libraries.is_none());
        assert_eq!(config.plex.unwrap().all_servers().len(), 1);
    }

    fn refused(text: &str, key: &'static str, says: &str) {
        match plex(text) {
            Err(ConfigError::Invalid { key: k, message }) if k == key => {
                assert!(message.contains(says), "{message}");
            }
            other => panic!("{text}: {other:?}"),
        }
    }

    #[test]
    fn a_server_id_is_unique_and_never_the_first_one_s() {
        for (id, says) in [
            ("plex", "not \"plex\""),
            ("Cabin", "lower-case"),
            ("2nd", "starting with a letter"),
            ("a:b", "lower-case"),
        ] {
            refused(
                &format!(
                    r#"plex {{ url = "http://a:32400"; servers [ {{ id = "{id}"; url = "http://b:32400"; }} ] }}"#
                ),
                "plex.servers",
                says,
            );
        }
        refused(
            r#"plex { url = "http://a:32400"; servers [
                { id = "b"; url = "http://b:32400"; }, { id = "b"; url = "http://c:32400"; } ] }"#,
            "plex.servers",
            "used twice",
        );
        refused(
            r#"plex { url = "http://a:32400"; servers [ { id = "b"; url = "b:32400"; } ] }"#,
            "plex.servers",
            "not an http",
        );
    }

    #[test]
    fn a_library_must_come_from_a_configured_section_once() {
        let library = |server: &str, section: &str, kind: &str| {
            format!(
                r#"plex {{ url = "http://a:32400"; }}
                libraries [ {{ name = "Films"; kind = "{kind}"; plex {{ server = "{server}"; section = "{section}"; }} }} ]"#
            )
        };
        refused(
            &library("cabin", "1", "movies"),
            "libraries",
            "no Plex server \"cabin\"",
        );
        refused(
            &library("plex", "x1", "movies"),
            "libraries",
            "not a Plex library key",
        );
        refused(
            &library("plex", "1", "photos"),
            "libraries",
            "not movies, shows or music",
        );
        refused(
            r#"plex { url = "http://a:32400"; }
            libraries [
              { name = "A"; kind = "movies"; plex { server = "plex"; section = "1"; } },
              { name = "B"; kind = "movies"; plex { server = "plex"; section = "1"; } },
            ]"#,
            "libraries",
            "already a library",
        );
        refused(
            r#"libraries [ { name = "A"; kind = "movies"; plex { server = "plex"; section = "1"; } } ]"#,
            "libraries",
            "none is configured",
        );
        // None at all is a server that serves nothing, and fine.
        assert!(plex(r#"plex { url = "http://a:32400"; } libraries []"#).is_ok());
    }

    #[test]
    fn mdns_can_be_turned_off() {
        let config =
            plex(r#"plex { url = "http://nas:32400"; } network { mdns = false; }"#).unwrap();
        assert!(!config.network.endpoint_options().unwrap().mdns);
    }

    #[test]
    fn a_fixed_port_is_read() {
        let config =
            plex(r#"plex { url = "http://nas:32400"; } network { port = 7420; }"#).unwrap();
        assert_eq!(config.network.endpoint_options().unwrap().port, Some(7420));
    }

    #[test]
    fn relays_are_refused_in_phase_1() {
        for relay in ["default", "https://relay.example.com"] {
            let err = plex(&format!(
                r#"plex {{ url = "http://nas:32400"; }} network {{ relay = "{relay}"; }}"#
            ))
            .unwrap_err();
            assert!(
                matches!(
                    err,
                    ConfigError::Invalid {
                        key: "network.relay",
                        ..
                    }
                ),
                "{relay}: {err:?}"
            );
        }
    }

    #[test]
    fn the_admin_api_listens_on_every_interface_by_default() {
        let config = plex("").unwrap();
        let listen = config.admin.listen().unwrap();
        assert!(listen.ip().is_unspecified(), "{listen}");
        assert_eq!(listen.port(), 7421);
        assert!(!config.admin.insecure_cookies());
    }

    #[test]
    fn the_admin_api_can_still_listen_on_loopback_only() {
        let config =
            plex(r#"admin { listen = "127.0.0.1:7421"; insecure_cookies = true; }"#).unwrap();
        assert!(config.admin.listen().unwrap().ip().is_loopback());
        assert!(config.admin.insecure_cookies());
    }

    #[test]
    fn trusted_proxies_are_none_by_default_and_a_list_when_set() {
        let config = plex(r#"plex { url = "http://nas:32400"; }"#).unwrap();
        assert_eq!(
            config.admin.trusted_proxies().unwrap(),
            crate::admin::TrustedProxies::default()
        );
        let config = plex(
            r#"plex { url = "http://nas:32400"; } admin { trusted_proxies = "10.0.0.5, 172.17.0.0/16"; }"#,
        )
        .unwrap();
        assert_eq!(
            config.admin.trusted_proxies().unwrap(),
            "10.0.0.5,172.17.0.0/16".parse().unwrap()
        );
    }

    #[test]
    fn a_trusted_proxy_that_is_not_an_address_or_range_is_rejected() {
        // At parse, not only at start: a setting changed at run time is
        // checked by the parse alone.
        let err = plex(
            r#"plex { url = "http://nas:32400"; } admin { trusted_proxies = "proxy.example.org"; }"#,
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::Invalid {
                    key: "admin.trusted_proxies",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn a_public_url_is_the_one_origin_browsers_may_use() {
        let config = plex(r#"plex { url = "http://nas:32400"; }"#).unwrap();
        assert_eq!(
            config.admin.origins().unwrap(),
            crate::admin::Origins::SameHost
        );
        let config = plex(
            r#"plex { url = "http://nas:32400"; } admin { public_url = "https://Media.Example.org/spoolframe/"; }"#,
        )
        .unwrap();
        assert_eq!(
            config.admin.origins().unwrap(),
            crate::admin::Origins::Exactly("https://media.example.org".into())
        );
        let err = plex(
            r#"plex { url = "http://nas:32400"; } admin { public_url = "media.example.org"; }"#,
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::Invalid {
                    key: "admin.public_url",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn the_artwork_cache_limit_is_read_in_mib() {
        let config =
            plex(r#"plex { url = "http://nas:32400"; } artwork { cache_mb = 64; }"#).unwrap();
        assert_eq!(config.artwork.cache_bytes(), 64 << 20);
        let default = plex(r#"plex { url = "http://nas:32400"; }"#).unwrap();
        assert_eq!(default.artwork.cache_bytes(), 1024 << 20);
    }

    #[test]
    fn an_item_is_watched_at_95_percent_by_default() {
        let config = plex(r#"plex { url = "http://nas:32400"; }"#).unwrap();
        assert_eq!(config.watch.rules().unwrap().watched_fraction, 0.95);
    }

    #[test]
    fn the_time_left_key_of_the_old_watched_rule_is_rejected() {
        let result =
            plex(r#"plex { url = "http://nas:32400"; } watch { watched_remaining_secs = 300; }"#);
        assert!(result.is_err(), "{result:?}");
    }

    #[test]
    fn watch_rules_default_and_can_be_set() {
        let default = plex(r#"plex { url = "http://nas:32400"; }"#).unwrap();
        assert_eq!(default.watch.rules().unwrap(), WatchRules::default());
        let set = plex(
            r#"plex { url = "http://nas:32400"; } watch { watched_percent = 80; started_after_secs = 30; }"#,
        )
        .unwrap();
        let rules = set.watch.rules().unwrap();
        assert_eq!(rules.watched_fraction, 0.8);
        assert_eq!(rules.started_after, Duration::from_secs(30));
        let bad = plex(r#"plex { url = "http://nas:32400"; } watch { watched_percent = 0; }"#)
            .unwrap_err();
        assert!(matches!(
            bad,
            ConfigError::Invalid {
                key: "watch.watched_percent",
                ..
            }
        ));
    }

    #[test]
    fn path_mapping_is_optional() {
        let config = plex(r#"plex { url = "http://nas:32400"; }"#).unwrap();
        assert!(config.plex.unwrap().path_mapping.is_empty());
    }

    #[test]
    fn a_relative_mapping_path_is_rejected() {
        let err = plex(
            r#"plex { url = "http://nas:32400"; path_mapping [ { from = "data"; to = "/media"; } ] }"#,
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::Invalid {
                    key: "plex.path_mapping",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn an_unknown_key_in_a_mapping_is_rejected() {
        let err = plex(
            r#"plex { url = "http://nas:32400"; path_mapping [ { from = "/a"; to = "/b"; too = "/c"; } ] }"#,
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::Ucl(_)), "{err:?}");
    }

    #[test]
    fn without_a_token_file_there_is_no_token() {
        let config = plex(r#"plex { url = "http://nas:32400"; }"#).unwrap();
        assert_eq!(config.plex.unwrap().token().unwrap(), None);
    }

    #[tokio::test]
    async fn the_token_is_trimmed() {
        let dir = spoolframe_test_dir::TestDir::new("spoolframe-config").await;
        let file = dir.as_ref().join("token");
        std::fs::write(&file, "abc123\n").unwrap();
        let config = PlexConfig {
            name: None,
            servers: Vec::new(),
            url: "http://nas:32400".into(),
            token_file: Some(file),
            path_mapping: Vec::new(),
            watch_sync: None,
        };
        assert_eq!(config.token().unwrap().as_deref(), Some("abc123"));
    }

    #[test]
    fn a_missing_token_file_names_its_path() {
        let config = PlexConfig {
            name: None,
            servers: Vec::new(),
            url: "http://nas:32400".into(),
            token_file: Some(PathBuf::from("/nonexistent/plex-token")),
            path_mapping: Vec::new(),
            watch_sync: None,
        };
        let err = config.token().unwrap_err();
        assert!(err.to_string().contains("/nonexistent/plex-token"), "{err}");
    }

    #[tokio::test]
    async fn an_empty_token_file_is_rejected() {
        let dir = spoolframe_test_dir::TestDir::new("spoolframe-config").await;
        let file = dir.as_ref().join("empty-token");
        std::fs::write(&file, " \n").unwrap();
        let config = PlexConfig {
            name: None,
            servers: Vec::new(),
            url: "http://nas:32400".into(),
            token_file: Some(file),
            path_mapping: Vec::new(),
            watch_sync: None,
        };
        assert!(matches!(
            config.token(),
            Err(ConfigError::EmptyToken { .. })
        ));
    }
}
