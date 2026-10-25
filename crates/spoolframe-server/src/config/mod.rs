//! Server configuration — schema `spoolframe.server.v1`, written in UCL.
//!
//! Every section is `deny_unknown_fields`, and a repeated key is an error:
//! a misspelt or duplicated setting fails at startup instead of being
//! quietly ignored.
//!
//! A server's settings come from three layers merged key by key
//! ([`layers`], P3-10): the built-in defaults, the provided file, and the
//! runtime layer the server writes itself. [`ServerConfig`] is what they
//! make together. They are read at start, and again on SIGHUP
//! ([`crate::runtime::reload`], PX-33): checked the same way, a reload
//! that does not load changes nothing.

pub mod layers;
pub mod secrets;
pub mod settings;
pub mod tree;

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use spoolframe_admin_api::SyncDirection;
use spoolframe_media::encoders::{Hardware, ToneMapSetting, TranscodeSettings};
use spoolframe_media::gpus::{GpuEntry, Pick, Which};
use spoolframe_media::sandbox::Mode as SandboxMode;
use spoolframe_plex::{PathMap, PathMapping};
use spoolframe_types::{LibraryKind, VideoCodec};

use crate::admission::Admission;
use crate::encode::JobLimits;
use crate::network::{EndpointOptions, Pkarr, Relay};
use crate::previews::{PreviewSettings, When};
use crate::subtitle_sync::SyncSettings;
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
    /// A value names `${SF_ENV_…}` (PX-22), and the server has no such
    /// variable.
    #[error("{key} names ${{{var}}}, but the server has no environment variable {var}")]
    EnvUnset { key: String, var: String },
    /// Nothing says where the state directory is.
    #[error(
        "no state directory: pass --state-dir <dir> or set SPOOLFRAME_STATE_DIR \
         (or XDG_STATE_HOME, or HOME)"
    )]
    NoStateDir,
    #[error("cannot resolve the state directory {}: {source}", path.display())]
    StateDirUnresolved {
        path: PathBuf,
        source: std::io::Error,
    },
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
    /// The catalog's replies kept in memory.
    #[ucl(default)]
    pub catalog: CatalogConfig,
    /// How this server caches what paired servers share with it (P8-02):
    /// read only while `experimental.federation` is on.
    #[ucl(default)]
    pub federation: FederationConfig,
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
    /// The pool of converted copies (PX-37).
    #[ucl(default)]
    pub optimized: OptimizedConfig,
    /// The files written for downloads (PX-38).
    #[ucl(default)]
    pub downloads: DownloadsConfig,
    /// Ways to sign in besides the password and passkeys: SSO (P3-04).
    #[ucl(default)]
    pub auth: AuthConfig,
    /// The log lines kept in memory for the web UI (P3-14).
    #[ucl(default)]
    pub logs: LogsConfig,
    /// OpenTelemetry export (P3-14): off without an endpoint.
    #[ucl(default)]
    pub telemetry: TelemetryConfig,
    /// The setup wizard's progress (P3-10): the server's own, kept in the
    /// runtime layer, not a setting.
    #[ucl(default)]
    pub setup: SetupConfig,
    /// Switches that may change or go away without notice (PGPU-04).
    #[ucl(default)]
    pub experimental: ExperimentalConfig,
    /// The HTTP playback port (PX-49): off unless `listen` is set.
    #[ucl(default)]
    pub http_playback: HttpPlaybackConfig,
    /// MCP and its OAuth sign-in on the admin listener (PX-42): off
    /// unless `enabled`.
    #[ucl(default)]
    pub mcp: McpConfig,
    /// Subtitles from OpenSubtitles on demand (PX-02): off without an
    /// API key.
    #[ucl(default)]
    pub opensubtitles: OpenSubtitlesConfig,
    /// Subtitles from SubDL on demand (PX-02): off without an API key.
    #[ucl(default)]
    pub subdl: SubdlConfig,
}

/// `opensubtitles { username; password = "${SF_ENV_…}"; languages =
/// ["en"]; }` (PX-02), all optional. OpenSubtitles is on whenever
/// `subtitles.sources` lists it: its API key is Spoolframe's own, built
/// in ([`crate::opensubtitles::builtin_key`]), so there is none to set.
/// An `api_key` a layer still has from before is taken out as the layers
/// are read (`config::layers`), never expanded. The password is a secret:
/// in the runtime layer it must be a `${SF_ENV_…}` reference, never the
/// secret itself. A live setting.
#[derive(Clone, Default, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct OpenSubtitlesConfig {
    /// The account downloads count against; without one, the site counts
    /// them by the server's address, a handful a day.
    #[ucl(default)]
    pub username: Option<String>,
    #[ucl(default)]
    pub password: Option<String>,
    /// Searched when no language is asked for, in order; `["en"]` if
    /// left out.
    #[ucl(default)]
    pub languages: Option<Vec<String>>,
    /// Where the API is; the site's own if left out. For tests, in the
    /// config file only: not a setting the web UI changes.
    #[ucl(default)]
    pub api_url: Option<String>,
}

impl std::fmt::Debug for OpenSubtitlesConfig {
    /// Whether the password is set, never what it is.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenSubtitlesConfig")
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "(set)"))
            .field("languages", &self.languages)
            .field("api_url", &self.api_url)
            .finish()
    }
}

impl OpenSubtitlesConfig {
    /// The site as the server uses it.
    pub fn settings(&self) -> Result<crate::subtitle_downloads::SiteSettings, ConfigError> {
        let (username, password) = match (trimmed(&self.username), trimmed(&self.password)) {
            (Some(u), Some(p)) => (Some(u.to_owned()), Some(p.to_owned())),
            (None, None) => (None, None),
            _ => {
                return Err(ConfigError::Invalid {
                    key: "opensubtitles.username",
                    message: "set both the username and the password, or neither".into(),
                });
            }
        };
        Ok(crate::subtitle_downloads::SiteSettings {
            kind: crate::subtitle_downloads::SiteKind::OpenSubtitles(
                crate::opensubtitles::Account {
                    api_url: site_url(
                        "opensubtitles.api_url",
                        self.api_url.as_deref(),
                        crate::opensubtitles::API_URL,
                    )?,
                    username,
                    password,
                },
            ),
            languages: site_languages("opensubtitles.languages", self.languages.as_deref())?,
        })
    }
}

/// `subdl { api_key = "${SF_ENV_…}"; languages = ["en"]; }` (PX-02). Off
/// without `api_key`, or when `subtitles.sources` leaves it out. The key
/// is a secret, as OpenSubtitles' is. A live setting.
#[derive(Clone, Default, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct SubdlConfig {
    /// The API key from the SubDL account's panel.
    #[ucl(default)]
    pub api_key: Option<String>,
    /// Searched when no language is asked for, in order; `["en"]` if
    /// left out.
    #[ucl(default)]
    pub languages: Option<Vec<String>>,
    /// Where the API and the files are; SubDL's own if left out. For
    /// tests, in the config file only: as a setting, an admin could send
    /// the key anywhere.
    #[ucl(default)]
    pub api_url: Option<String>,
    #[ucl(default)]
    pub download_url: Option<String>,
}

impl std::fmt::Debug for SubdlConfig {
    /// Whether the key is set, never what it is.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubdlConfig")
            .field("api_key", &self.api_key.as_ref().map(|_| "(set)"))
            .field("languages", &self.languages)
            .field("api_url", &self.api_url)
            .field("download_url", &self.download_url)
            .finish()
    }
}

impl SubdlConfig {
    /// The site as the server uses it; `None` without an API key.
    pub fn settings(&self) -> Result<Option<crate::subtitle_downloads::SiteSettings>, ConfigError> {
        let Some(api_key) = trimmed(&self.api_key) else {
            return Ok(None);
        };
        Ok(Some(crate::subtitle_downloads::SiteSettings {
            kind: crate::subtitle_downloads::SiteKind::SubDl(crate::subdl::Account {
                api_url: site_url(
                    "subdl.api_url",
                    self.api_url.as_deref(),
                    crate::subdl::API_URL,
                )?,
                download_url: site_url(
                    "subdl.download_url",
                    self.download_url.as_deref(),
                    crate::subdl::DOWNLOAD_URL,
                )?,
                api_key: api_key.to_owned(),
            }),
            languages: site_languages("subdl.languages", self.languages.as_deref())?,
        }))
    }
}

/// `value` trimmed, if anything is left of it.
fn trimmed(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|v| !v.is_empty())
}

/// A subtitle site's URL, `default` if left out.
fn site_url(key: &'static str, value: Option<&str>, default: &str) -> Result<Url, ConfigError> {
    let text = value.unwrap_or(default);
    Url::parse(text)
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some())
        .ok_or_else(|| ConfigError::Invalid {
            key,
            message: format!("{text:?} is not an http(s) URL"),
        })
}

/// A subtitle site's languages, `["en"]` if left out.
fn site_languages(key: &'static str, list: Option<&[String]>) -> Result<Vec<String>, ConfigError> {
    match list {
        None => Ok(vec!["en".to_owned()]),
        Some([]) => Err(ConfigError::Invalid {
            key,
            message: "name at least one language, such as [\"en\"]".into(),
        }),
        Some(list) => list
            .iter()
            .map(|l| {
                crate::subtitle_downloads::language_code(l).map_err(|e| ConfigError::Invalid {
                    key,
                    message: e.to_string(),
                })
            })
            .collect(),
    }
}

impl ServerConfig {
    /// The pool's place and the download area's, with `state_dir` for
    /// their defaults (review 2026-10-20, finding 1). Each removes files
    /// in its directory and empties its `making/` at start, so neither
    /// may be the other, and neither may be, hold or sit in a directory
    /// the server keeps other files in (its caches, subtitle sync, the
    /// transcode scratch, the subtitles' text) or one the media is read
    /// from (a Plex path mapping's `to`). The download area may sit in
    /// the pool (its default, `<optimized dir>/downloads`), not in the
    /// pool's `making/`; the pool may not sit in the area.
    pub fn areas(
        &self,
        state_dir: &Path,
    ) -> Result<(OptimizedSettings, DownloadSettings), ConfigError> {
        let pool = self.optimized.place(state_dir)?;
        let area = self.downloads.place(state_dir, &pool.dir)?;
        let refuse = |key: &'static str, dir: &Path, why: String| {
            Err(ConfigError::Invalid {
                key,
                message: format!(
                    "{} {why}: its files would be removed. Choose a directory of its own",
                    dir.display()
                ),
            })
        };
        if area.dir == pool.dir {
            return refuse("downloads.dir", &area.dir, "is optimized.dir".into());
        }
        if pool.dir.starts_with(&area.dir) {
            return refuse("optimized.dir", &pool.dir, "is in downloads.dir".into());
        }
        if area
            .dir
            .starts_with(pool.dir.join(crate::optimized::MAKING_DIR))
        {
            return refuse(
                "downloads.dir",
                &area.dir,
                "is in the optimized pool's making directory".into(),
            );
        }
        let limits = self.transcode.limits(state_dir)?;
        let ocr = self.subtitles.ocr_settings(state_dir)?;
        let mut kept: Vec<(String, PathBuf)> = vec![
            ("the server's caches".into(), state_dir.join("cache")),
            ("subtitle sync's files".into(), state_dir.join("subtitles")),
            ("transcode.scratch_dir".into(), limits.scratch_dir),
            ("subtitles.cache_dir".into(), ocr.cache_dir),
        ];
        if let Some(plex) = &self.plex {
            let servers = std::iter::once(&plex.path_mapping)
                .chain(plex.servers.iter().map(|server| &server.path_mapping));
            for mapping in servers.flatten() {
                kept.push(("a Plex path mapping's media".into(), mapping.to.clone()));
            }
        }
        for (key, dir) in [("optimized.dir", &pool.dir), ("downloads.dir", &area.dir)] {
            for (what, other) in &kept {
                if dir.starts_with(other) || other.starts_with(dir) {
                    return refuse(
                        key,
                        dir,
                        format!("is, holds or is in {what} ({})", other.display()),
                    );
                }
            }
        }
        Ok((pool, area))
    }

    /// The subtitle sites as the server uses them (PX-02): those
    /// `subtitles.sources` names, in its order; SubDL only with its key.
    pub fn subtitle_downloads(&self) -> Result<crate::subtitle_downloads::Settings, ConfigError> {
        use crate::subtitle_site::Source;
        let order: Vec<Source> = match &self.subtitles.sources {
            None => Source::ALL.to_vec(),
            Some(names) => {
                let mut order = Vec::new();
                for name in names {
                    let source = Source::of(name.trim()).ok_or_else(|| ConfigError::Invalid {
                        key: "subtitles.sources",
                        message: format!(
                            "{name:?} is not a subtitle site; name opensubtitles or subdl"
                        ),
                    })?;
                    if !order.contains(&source) {
                        order.push(source);
                    }
                }
                order
            }
        };
        let opensubtitles = self.opensubtitles.settings()?;
        let subdl = self.subdl.settings()?;
        let sites = order
            .into_iter()
            .filter_map(|source| match source {
                Source::OpenSubtitles => Some(opensubtitles.clone()),
                Source::SubDl => subdl.clone(),
            })
            .collect();
        Ok(crate::subtitle_downloads::Settings {
            sites,
            daily_limit_per_user: self.subtitles.daily_limit_per_user.unwrap_or(0),
            daily_limit_per_peer: self
                .subtitles
                .daily_limit_per_peer
                .unwrap_or(crate::subtitle_downloads::DEFAULT_DAILY_LIMIT_PER_PEER),
        })
    }
}

/// `mcp { enabled = true; }` (PX-42): an MCP server at `/mcp` on the
/// admin listener, with the OAuth endpoints its clients sign in through.
/// Off, every one of them is not found. A live setting.
#[derive(Debug, Clone, Default, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct McpConfig {
    /// Off if left out.
    #[ucl(default)]
    pub enabled: Option<bool>,
    /// The apps allowed to sign in, by the redirect URIs they return to;
    /// Claude and Codex if left out. `[]`: none.
    #[ucl(default)]
    pub allowed_redirects: Option<Vec<AllowedRedirectConfig>>,
}

/// One entry of `mcp.allowed_redirects`:
/// `{ name = "Claude Code"; pattern = "http://localhost:*/callback"; }`.
#[derive(Debug, Clone, PartialEq, Eq, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct AllowedRedirectConfig {
    pub name: String,
    pub pattern: String,
}

impl McpConfig {
    pub fn enabled(&self) -> bool {
        self.enabled.unwrap_or(false)
    }

    /// The apps allowed to sign in, each checked
    /// ([`crate::oauth::redirects::RedirectPattern::parse`]).
    pub fn allowed_redirects(
        &self,
    ) -> Result<Vec<crate::oauth::redirects::RedirectPattern>, ConfigError> {
        use crate::oauth::redirects;
        match &self.allowed_redirects {
            None => Ok(redirects::defaults()),
            Some(entries) => redirects::parse_all(
                entries
                    .iter()
                    .map(|e| (e.name.as_str(), e.pattern.as_str())),
            )
            .map_err(|message| ConfigError::Invalid {
                key: "mcp.allowed_redirects",
                message,
            }),
        }
    }
}

/// `http_playback { listen = "[::]:7422"; address = "media.home.arpa"; }`
/// (PX-49): a playback's HLS or file over plain HTTP, for players with no
/// Iroh (AirPlay first), under a token a signed-in client asks for. Both
/// optional; off unless `listen` is set.
#[derive(Debug, Clone, Default, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct HttpPlaybackConfig {
    /// Where the port listens: an IP address and a port. Left out, it is
    /// off, and nothing listens.
    #[ucl(default)]
    pub listen: Option<String>,
    /// The host a URL names for a client that reached the server through a
    /// relay or over the Internet: an IP address or a name, no port. Left
    /// out, such a client gets no URL; one on the LAN gets the address it
    /// reached the server at, whatever this says.
    #[ucl(default)]
    pub address: Option<String>,
}

impl HttpPlaybackConfig {
    pub fn settings(&self) -> Result<crate::http_playback::Settings, ConfigError> {
        let listen = match self.listen.as_deref().map(str::trim) {
            None => None,
            Some(text) => Some(
                text.parse::<SocketAddr>()
                    .map_err(|e| ConfigError::Invalid {
                        key: "http_playback.listen",
                        message: format!(
                            "{text:?}: {e}; an IP address and a port, as \"[::]:7422\""
                        ),
                    })?,
            ),
        };
        let address = match self.address.as_deref().map(str::trim) {
            None => None,
            Some(text) => Some(url_host(text).ok_or_else(|| ConfigError::Invalid {
                key: "http_playback.address",
                message: format!(
                    "{text:?}: an IP address or a host name, with no scheme, port or path"
                ),
            })?),
        };
        Ok(crate::http_playback::Settings {
            listen,
            address,
            idle: crate::http_playback::IDLE,
        })
    }
}

/// `text`, an IP address or a host name, as a URL's host names it (an
/// IPv6 address in brackets); `None` for anything else.
fn url_host(text: &str) -> Option<String> {
    if let Ok(ip) = text.parse::<std::net::IpAddr>() {
        return Some(match ip {
            std::net::IpAddr::V6(ip) => format!("[{ip}]"),
            ip => ip.to_string(),
        });
    }
    let name = !text.is_empty()
        && text.len() <= 253
        && text.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        });
    name.then(|| text.to_owned())
}

/// `experimental { libplacebo_shared_on_amd = false; }` (PGPU-04): switches
/// that may change or go away in any release, without notice. The server
/// logs the ones turned on at start, and the web UI shows them only with
/// "Show advanced settings" on. The built-in defaults name each one off,
/// so what is logged is what differs from them.
#[derive(Debug, Clone, Default, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct ExperimentalConfig {
    /// On an AMD GPU, libplacebo's tone map uses ffmpeg's Vulkan device,
    /// so the frames stay on the GPU, rather than one of its own, which
    /// takes them through system memory. Tried at detection, and its own
    /// used where that fails, as it does on RADV today. Off if left out.
    #[ucl(default)]
    pub libplacebo_shared_on_amd: Option<bool>,
    /// Read ahead of any play the keyframes of each film and episode
    /// whose file has no index (a transport stream): one file at a time,
    /// at the lowest priority, while nothing converts, and kept on disk
    /// ([`crate::index_ahead`]). Off if left out.
    #[ucl(default)]
    pub index_ahead: Option<bool>,
    /// Server-to-server federation (P8-01): the `spoolframe/federation/1`
    /// ALPN is served, `federation` invites are made, and the admin API's
    /// `federation` methods answer. Off if left out: the ALPN is closed at
    /// once and a `federation` invite refused, as before it. Read at
    /// start: the endpoint's ALPNs are fixed when it binds.
    #[ucl(default)]
    pub federation: Option<bool>,
}

impl ExperimentalConfig {
    /// `libplacebo_shared_on_amd`.
    pub fn libplacebo_shared_on_amd(&self) -> bool {
        self.libplacebo_shared_on_amd.unwrap_or(false)
    }

    /// `index_ahead`.
    pub fn index_ahead(&self) -> bool {
        self.index_ahead.unwrap_or(false)
    }

    /// `federation`.
    pub fn federation(&self) -> bool {
        self.federation.unwrap_or(false)
    }

    /// The switches turned on, by key, as the start's log names them.
    pub fn on(&self) -> Vec<&'static str> {
        [
            (
                "experimental.libplacebo_shared_on_amd",
                self.libplacebo_shared_on_amd(),
            ),
            ("experimental.index_ahead", self.index_ahead()),
            ("experimental.federation", self.federation()),
        ]
        .into_iter()
        .filter_map(|(key, on)| on.then_some(key))
        .collect()
    }
}

/// `auth { oidc [ { name; issuer; client_id; client_secret_file; } ] }`
/// (P3-04): the OpenID Connect providers a user may link to their account
/// and then sign in with. None: no SSO anywhere.
#[derive(Debug, Clone, Default, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct AuthConfig {
    #[ucl(default)]
    pub oidc: Vec<OidcConfig>,
    /// Days a trusted device (P4-01) may go unused before its trust lapses
    /// and it signs in again in full; 180 if left out.
    #[ucl(default)]
    pub trust_idle_days: Option<u64>,
}

/// How long a trusted device may go unused by default (P4-01).
pub const DEFAULT_TRUST_IDLE_DAYS: u64 = 180;
/// The longest `auth.trust_idle_days` may be: ten years.
pub const MAX_TRUST_IDLE_DAYS: u64 = 3650;

/// One OpenID Connect provider (P3-04): Pocket ID, Authentik, Kanidm,
/// Keycloak, Google, …. Spoolframe is its confidential client, with PKCE,
/// and the callback `<admin.public_url>/auth/oidc/callback`.
#[derive(Debug, Clone, PartialEq, Eq, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct OidcConfig {
    /// What the buttons say: "Continue with \<name\>". Unique.
    pub name: String,
    /// The issuer, as its ID tokens' `iss` says it; its discovery document
    /// is at `<issuer>/.well-known/openid-configuration`. HTTPS, or HTTP
    /// on loopback only.
    pub issuer: String,
    pub client_id: String,
    /// A file holding the client secret. Read before `client_secret`.
    #[ucl(default)]
    pub client_secret_file: Option<PathBuf>,
    /// The client secret itself (PX-22). Meant as `${SF_ENV_NAME}`, so
    /// the secret stays in the environment: the runtime layer takes
    /// nothing else, and the provided file is plain text. Without either,
    /// the secret pasted in the web UI, kept as `secrets/oidc-<name>`
    /// under the state directory.
    #[ucl(default)]
    pub client_secret: Option<String>,
    /// The scopes asked for; `openid profile email` if left out. `openid`
    /// is asked for whether it is listed or not.
    #[ucl(default)]
    pub scopes: Option<Vec<String>>,
}

/// The most characters a provider's name has: it is a button's label.
pub const MAX_OIDC_NAME_LEN: usize = 40;

impl OidcConfig {
    /// The issuer, checked: HTTPS, or HTTP on a loopback host (a provider
    /// on this machine, or a test's), with no query or fragment. Its keys
    /// are fetched from it, so plain HTTP across a network would let
    /// anyone on the way sign tokens.
    pub fn issuer_url(&self) -> Result<Url, ConfigError> {
        let invalid = |message: String| ConfigError::Invalid {
            key: "auth.oidc",
            message: format!("{:?}: {message}", self.name),
        };
        let url = Url::parse(&self.issuer)
            .map_err(|e| invalid(format!("issuer {:?}: {e}", self.issuer)))?;
        let loopback = match url.host() {
            Some(url::Host::Domain(d)) => d == "localhost",
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            None => false,
        };
        match url.scheme() {
            "https" => {}
            "http" if loopback => {}
            _ => {
                return Err(invalid(format!(
                    "issuer {:?} is not an https URL (http only on localhost)",
                    self.issuer
                )));
            }
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(invalid(format!(
                "issuer {:?} has a query or a fragment",
                self.issuer
            )));
        }
        Ok(url)
    }

    /// The scopes asked for.
    pub fn scopes(&self) -> Vec<String> {
        match &self.scopes {
            Some(scopes) => scopes.clone(),
            None => crate::oidc::DEFAULT_SCOPES.map(str::to_owned).to_vec(),
        }
    }

    /// The secret it is kept as when set in the web UI: `oidc-<name>`, the
    /// name lower-cased with anything but letters and digits as `-`.
    pub fn secret_name(&self) -> String {
        oidc_secret_name(&self.name)
    }
}

/// See [`OidcConfig::secret_name`].
pub fn oidc_secret_name(name: &str) -> String {
    let mut slug = String::new();
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    format!("oidc-{}", slug.trim_matches('-'))
}

impl AuthConfig {
    /// How long a trusted device may go unused before its trust lapses:
    /// 1 to [`MAX_TRUST_IDLE_DAYS`] days.
    pub fn trust_idle(&self) -> Result<Duration, ConfigError> {
        let days = self.trust_idle_days.unwrap_or(DEFAULT_TRUST_IDLE_DAYS);
        if !(1..=MAX_TRUST_IDLE_DAYS).contains(&days) {
            return Err(ConfigError::Invalid {
                key: "auth.trust_idle_days",
                message: format!("{days} is not between 1 and {MAX_TRUST_IDLE_DAYS}"),
            });
        }
        Ok(Duration::from_secs(days * 24 * 3600))
    }

    /// Every provider checked: a name, an issuer, a client id, none twice;
    /// and somewhere for the provider to send the browser back to.
    fn check(&self, admin: &AdminConfig) -> Result<(), ConfigError> {
        let mut names: Vec<String> = Vec::new();
        let mut issuers: Vec<&str> = Vec::new();
        for p in &self.oidc {
            let invalid = |message: String| ConfigError::Invalid {
                key: "auth.oidc",
                message,
            };
            let name = p.name.trim();
            if name.is_empty() || name.chars().count() > MAX_OIDC_NAME_LEN {
                return Err(invalid(format!(
                    "{:?}: a provider's name is 1 to {MAX_OIDC_NAME_LEN} characters",
                    p.name
                )));
            }
            let secret = p.secret_name();
            if secret == "oidc-" || names.contains(&secret) {
                return Err(invalid(format!(
                    "{:?}: another provider has this name, or one that differs only in case \
                     and punctuation",
                    p.name
                )));
            }
            names.push(secret);
            p.issuer_url()?;
            if issuers.contains(&p.issuer.as_str()) {
                return Err(invalid(format!(
                    "{:?}: issuer {:?} is another provider's too",
                    p.name, p.issuer
                )));
            }
            issuers.push(&p.issuer);
            if p.client_id.trim().is_empty() {
                return Err(invalid(format!("{:?}: client_id is empty", p.name)));
            }
            for scope in p.scopes.iter().flatten() {
                if scope.is_empty() || scope.contains(char::is_whitespace) {
                    return Err(invalid(format!(
                        "{:?}: scope {scope:?} is not one scope",
                        p.name
                    )));
                }
            }
        }
        if !self.oidc.is_empty() && admin.public_url.is_none() {
            return Err(ConfigError::Invalid {
                key: "auth.oidc",
                message:
                    "a provider sends the browser back to <admin.public_url>/auth/oidc/callback: \
                          set admin.public_url"
                        .into(),
            });
        }
        Ok(())
    }
}

/// `logs { buffer_lines = 5000; }` (P3-14), optional.
#[derive(Debug, Clone, Default, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct LogsConfig {
    /// The last log lines kept for the Logs page; 5000 if left out.
    #[ucl(default)]
    pub buffer_lines: Option<u64>,
}

impl LogsConfig {
    /// The most lines a ring may be set to: about 20 MiB of lines.
    pub const MAX_LINES: u64 = 100_000;

    pub fn buffer_lines(&self) -> Result<usize, ConfigError> {
        match self.buffer_lines {
            None => Ok(crate::logs::DEFAULT_LINES),
            Some(n) if (1..=Self::MAX_LINES).contains(&n) => Ok(n as usize),
            Some(n) => Err(ConfigError::Invalid {
                key: "logs.buffer_lines",
                message: format!("{n} is not between 1 and {}", Self::MAX_LINES),
            }),
        }
    }
}

/// `telemetry { otlp { … } }` (P3-14).
#[derive(Debug, Clone, Default, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct TelemetryConfig {
    #[ucl(default)]
    pub otlp: OtlpConfig,
}

/// `otlp { endpoint = "http://alloy:4317"; protocol = "grpc";
/// headers { x-scope-orgid = "home"; } traces = true; metrics = true;
/// logs = true; }`, all optional: without an endpoint nothing is exported.
#[derive(Debug, Clone, Default, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct OtlpConfig {
    /// The collector: `http://host:4317` for gRPC, `http://host:4318` for
    /// HTTP (the paths `/v1/traces`, `/v1/metrics`, `/v1/logs` are added).
    #[ucl(default)]
    pub endpoint: Option<String>,
    /// `grpc` (the default) or `http` (protobuf).
    #[ucl(default)]
    pub protocol: Option<String>,
    /// Sent with every export: a table of header name to value, as
    /// `headers { authorization = "Bearer …"; }`. Never logged.
    #[ucl(default, map = "otlp_headers")]
    pub headers: Option<OtlpHeaders>,
    /// Each signal, on unless set to false.
    #[ucl(default)]
    pub traces: Option<bool>,
    #[ucl(default)]
    pub metrics: Option<bool>,
    #[ucl(default)]
    pub logs: Option<bool>,
}

impl OtlpConfig {
    /// What to export to, checked; `None` without an endpoint.
    pub fn settings(&self) -> Result<Option<crate::telemetry::OtlpSettings>, ConfigError> {
        use crate::telemetry::{OtlpSettings, Protocol};
        let Some(endpoint) = self
            .endpoint
            .as_deref()
            .map(str::trim)
            .filter(|e| !e.is_empty())
        else {
            return Ok(None);
        };
        let invalid = |key: &'static str, message: String| ConfigError::Invalid { key, message };
        let url = Url::parse(endpoint)
            .map_err(|e| invalid("telemetry.otlp.endpoint", format!("{endpoint:?}: {e}")))?;
        if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
            return Err(invalid(
                "telemetry.otlp.endpoint",
                format!("{endpoint:?} is not an http(s) URL"),
            ));
        }
        let protocol = match self.protocol.as_deref().unwrap_or("grpc") {
            "grpc" => Protocol::Grpc,
            "http" => Protocol::Http,
            other => {
                return Err(invalid(
                    "telemetry.otlp.protocol",
                    format!("{other:?}: expected grpc or http"),
                ));
            }
        };
        let headers = match &self.headers {
            None => Vec::new(),
            // The name=value string it was before it took a table; never
            // echoed, as it may carry a key.
            Some(OtlpHeaders::Text) => {
                return Err(invalid(
                    "telemetry.otlp.headers",
                    "a table now, one header each, as headers { authorization = \"Bearer …\"; }"
                        .into(),
                ));
            }
            Some(OtlpHeaders::Table(table)) => otlp_header_list(table)?,
        };
        Ok(Some(OtlpSettings {
            endpoint: endpoint.to_owned(),
            protocol,
            headers,
            traces: self.traces.unwrap_or(true),
            metrics: self.metrics.unwrap_or(true),
            logs: self.logs.unwrap_or(true),
        }))
    }
}

/// `telemetry.otlp.headers` as written: a table, or the string it was
/// before. The string is not kept: it may carry a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OtlpHeaders {
    Table(HashMap<String, String>),
    Text,
}

fn otlp_headers(
    object: uclicious::ObjectRef,
) -> Result<Option<OtlpHeaders>, uclicious::ObjectError> {
    if !object.is_object() {
        return Ok(Some(OtlpHeaders::Text));
    }
    let table: HashMap<String, String> = uclicious::FromObject::try_from(object)?;
    Ok(Some(OtlpHeaders::Table(table)))
}

/// The headers, checked and sorted by name. The value is never in a
/// message: it may be a key.
fn otlp_header_list(table: &HashMap<String, String>) -> Result<Vec<(String, String)>, ConfigError> {
    let invalid = |message: String| ConfigError::Invalid {
        key: "telemetry.otlp.headers",
        message,
    };
    let token = |c: char| c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c);
    let mut headers = Vec::with_capacity(table.len());
    for (name, value) in table {
        let name = name.trim();
        if name.is_empty() || !name.chars().all(token) {
            return Err(invalid(format!("{name:?} is not a header name")));
        }
        // Before trimming: a trailing newline is a control character too.
        if value.chars().any(|c| c.is_control()) {
            return Err(invalid(format!(
                "the value of {name:?} has a control character"
            )));
        }
        headers.push((name.to_owned(), value.trim().to_owned()));
    }
    headers.sort();
    Ok(headers)
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

/// `media { ffmpeg = "ffmpeg"; ffprobe = "ffprobe"; tesseract = "tesseract";
/// sandbox = "auto"; }`, all optional: by default whatever
/// is on `PATH`, which the nix package wraps.
#[derive(Debug, Clone, Default, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct MediaConfig {
    #[ucl(default)]
    pub ffmpeg: Option<PathBuf>,
    #[ucl(default)]
    pub ffprobe: Option<PathBuf>,
    #[ucl(default)]
    pub tesseract: Option<PathBuf>,
    /// Whether the tools run confined by Landlock and seccomp: `required`,
    /// `auto` or `off` (PX-11 finding 34); `auto` if left out.
    #[ucl(default)]
    pub sandbox: Option<String>,
}

impl MediaConfig {
    pub fn sandbox(&self) -> Result<SandboxMode, ConfigError> {
        self.sandbox
            .as_deref()
            .unwrap_or("auto")
            .trim()
            .parse()
            .map_err(|message| ConfigError::Invalid {
                key: "media.sandbox",
                message,
            })
    }

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

/// `subtitles { ocr = true; ocr_threads = 4; cache_dir = "…";
/// sources = ["opensubtitles", "subdl"]; daily_limit_per_user = 0; }`, all
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
    /// Subtitle syncs that run at once (PX-03); 1 if left out.
    #[ucl(default)]
    pub sync_at_once: Option<u32>,
    /// The subtitle sites searched and downloaded from (PX-02), in the
    /// order they are asked: `opensubtitles`, `subdl`. Both if left out:
    /// OpenSubtitles needs nothing set; SubDL is off without its API key
    /// whatever this says. A live setting.
    #[ucl(default)]
    pub sources: Option<Vec<String>>,
    /// A user's subtitle downloads a day, from every site (PX-02); 0 or
    /// left out for no limit. A live setting.
    #[ucl(default)]
    pub daily_limit_per_user: Option<u32>,
    /// The subtitle downloads a day of every user of one paired server
    /// together (P8-07), on this server's items: 40 if left out, 0 for no
    /// limit. Not the limit per user, whose default is none: a pair's
    /// users are on this server's site account unless its admin says so.
    /// A live setting.
    #[ucl(default)]
    pub daily_limit_per_peer: Option<u32>,
}

impl SubtitlesConfig {
    /// Subtitle sync's settings (PX-03): its files in `state_dir`.
    pub fn sync_settings(&self, state_dir: &Path) -> Result<SyncSettings, ConfigError> {
        let at_once = match self.sync_at_once {
            Some(0) => {
                return Err(ConfigError::Invalid {
                    key: "subtitles.sync_at_once",
                    message: "0 would never sync anything; use at least 1".into(),
                });
            }
            Some(n) => n as usize,
            None => 1,
        };
        Ok(SyncSettings {
            at_once,
            dir: state_dir.join("subtitles"),
            speech_dir: state_dir.join("cache").join("subtitle-speech"),
        })
    }

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

/// `previews { when = "ahead"; interval_secs = 10; cache_mb = 4096;
/// memory_mb = 64; workers = 8; }`, all optional.
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
    /// The most of them kept in memory, in MiB; 64 if left out.
    #[ucl(default)]
    pub memory_mb: Option<u64>,
    /// Files made ahead at once while nothing is encoded; the physical
    /// cores the server may use if left out.
    #[ucl(default)]
    pub workers: Option<u32>,
}

impl PreviewsConfig {
    pub const DEFAULT_INTERVAL_SECS: u64 = 10;
    pub const DEFAULT_CACHE_MB: u64 = 4096;
    /// A quarter of the 256 MiB the caches' memory tiers take by default:
    /// a set is read only while its film is scrubbed.
    pub const DEFAULT_MEMORY_MB: u64 = 64;

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
        let workers = match self.workers {
            Some(0) => {
                return Err(ConfigError::Invalid {
                    key: "previews.workers",
                    message: "0 would make none ahead; use when = \"on_play\" for that".into(),
                });
            }
            Some(n) => n as usize,
            None => crate::previews::workers::default_workers().count,
        };
        Ok(PreviewSettings {
            when,
            interval: Duration::from_secs(interval),
            cache_limit_bytes: self.cache_mb.unwrap_or(Self::DEFAULT_CACHE_MB) << 20,
            memory_limit_bytes: self.memory_mb.unwrap_or(Self::DEFAULT_MEMORY_MB) << 20,
            workers,
        })
    }
}

/// `optimized { binge_ahead = true; dir = "…"; max_size_mb = 102400; }`
/// (PX-37), all optional: whether the server makes the next episode
/// ahead while someone watches (it does not unless told to), where the
/// pool of copies is kept, and the most it may hold. The pool always
/// runs: downloads (PX-38) and the admin's on-demand requests make their
/// copies in it, and playback plays from every copy it has made.
#[derive(Debug, Clone, Default, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct OptimizedConfig {
    /// Off if left out: binge-ahead fills the pool on its own while
    /// shows are binged, so it is the admin's choice. It is the only
    /// switch (the user's decision, 2026-10-20): 0.13's `enabled` is
    /// refused, naming this.
    #[ucl(default)]
    pub binge_ahead: Option<bool>,
    /// `state_dir/optimized` if left out.
    #[ucl(default)]
    pub dir: Option<PathBuf>,
    /// In MiB; 102400 (100 GiB) if left out.
    #[ucl(default)]
    pub max_size_mb: Option<u64>,
}

/// The pool's place and size, read from [`OptimizedConfig`], and
/// whether the server makes the next episode ahead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptimizedSettings {
    pub dir: PathBuf,
    pub max_size_bytes: u64,
    /// `optimized.binge_ahead`: off, the pool drops at start the
    /// binge-ahead copies a server that had it on left unmade.
    pub binge_ahead: bool,
}

impl OptimizedConfig {
    /// A few films in each shipped profile: a 1080p film in `TV` is
    /// about 10 GiB.
    pub const DEFAULT_MAX_SIZE_MB: u64 = 100 * 1024;

    /// Whether the server makes the next episode ahead while someone
    /// watches.
    pub fn binge_ahead(&self) -> bool {
        self.binge_ahead.unwrap_or(false)
    }

    /// The pool's place and size, with `state_dir` for the default
    /// directory. A size of 0 and a
    /// directory that is the state directory or holds it are refused: the
    /// media sandbox hides the state directory and would not show the
    /// copies in it.
    pub fn place(&self, state_dir: &Path) -> Result<OptimizedSettings, ConfigError> {
        let max_size_mb = self.max_size_mb.unwrap_or(Self::DEFAULT_MAX_SIZE_MB);
        if max_size_mb == 0 {
            return Err(ConfigError::Invalid {
                key: "optimized.max_size_mb",
                message: "0 would keep no copy: every download would wait forever".into(),
            });
        }
        let dir = self
            .dir
            .clone()
            .unwrap_or_else(|| state_dir.join("optimized"));
        if state_dir.starts_with(&dir) {
            return Err(ConfigError::Invalid {
                key: "optimized.dir",
                message: format!(
                    "{} is the state directory or holds it: the copies would show it whole \
                     to ffmpeg. Choose a directory of its own",
                    dir.display()
                ),
            });
        }
        Ok(OptimizedSettings {
            dir,
            max_size_bytes: max_size_mb << 20,
            binge_ahead: self.binge_ahead(),
        })
    }
}

/// `downloads { dir = "…"; max_size_mb = 65536; }` (PX-38), both
/// optional: where the files written for downloads are kept until the
/// device has them, and the most they may take. Converted copies a
/// download is written from stay in the pool, under
/// `optimized.max_size_mb`.
#[derive(Debug, Clone, Default, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct DownloadsConfig {
    /// `<optimized dir>/downloads` if left out.
    #[ucl(default)]
    pub dir: Option<PathBuf>,
    /// In MiB; 65536 (64 GiB) if left out.
    #[ucl(default)]
    pub max_size_mb: Option<u64>,
}

/// The download area's place and size, read from [`DownloadsConfig`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadSettings {
    pub dir: PathBuf,
    pub max_size_bytes: u64,
}

impl DownloadsConfig {
    /// A few films at once, as they are, while their devices fetch them.
    pub const DEFAULT_MAX_SIZE_MB: u64 = 64 * 1024;

    /// The area's place and size, `optimized_dir` (the pool's) holding
    /// it unless `dir` says otherwise. A size of 0 and a directory that
    /// is the state directory or holds it are refused.
    pub fn place(
        &self,
        state_dir: &Path,
        optimized_dir: &Path,
    ) -> Result<DownloadSettings, ConfigError> {
        let max_size_mb = self.max_size_mb.unwrap_or(Self::DEFAULT_MAX_SIZE_MB);
        if max_size_mb == 0 {
            return Err(ConfigError::Invalid {
                key: "downloads.max_size_mb",
                message: "0 would hold no file: every download would wait forever".into(),
            });
        }
        let dir = self
            .dir
            .clone()
            .unwrap_or_else(|| optimized_dir.join("downloads"));
        if state_dir.starts_with(&dir) {
            return Err(ConfigError::Invalid {
                key: "downloads.dir",
                message: format!(
                    "{} is the state directory or holds it: choose a directory of its own",
                    dir.display()
                ),
            });
        }
        Ok(DownloadSettings {
            dir,
            max_size_bytes: max_size_mb << 20,
        })
    }
}

/// `transcode { gpus = [ { api = "nvenc"; pick = "all"; } ]; max_jobs = 2;
/// max_remux = 32; max_gpu_jobs = 3; scratch_dir = "…"; scratch_limit_mb =
/// 20480; read_ahead_secs = 60; copy_segment_secs = 2; gpu_filters = false;
/// tone_map = "auto"; video_codecs = ["av1", "hevc", "h264"]; }`, all
/// optional.
#[derive(Debug, Clone, Default, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct TranscodeConfig {
    /// The GPUs encodes run on (PGPU-05), in order of preference: each
    /// entry an `api` (`nvenc`, `vaapi`, `qsv`) and either `pick` (`all`,
    /// `best`, `dgpu`, `igpu`; `all` if left out) or `device` (a CUDA
    /// index for NVIDIA, a render node for VA-API and QSV), and
    /// `max_jobs`, GPU video encodes at once on each of its cards. Left
    /// out, every card of every API, NVIDIA first.
    #[ucl(default)]
    pub gpus: Option<Vec<GpuConfig>>,
    /// The encode budget, in software video encodes; 2 if left out. A
    /// GPU video encode weighs half of one, or a quarter when decoded and
    /// scaled on the GPU too; an audio-only encode a quarter; a remux
    /// nothing.
    #[ucl(default)]
    pub max_jobs: Option<u32>,
    /// Remuxes at once; 32 if left out.
    #[ucl(default)]
    pub max_remux: Option<u32>,
    /// GPU video encodes at once, within the budget too; 3 if left out.
    #[ucl(default)]
    pub max_gpu_jobs: Option<u32>,
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
    /// How long a copied (remuxed) video's HLS segments are at least, in
    /// s: each is cut at the first keyframe this far past the last cut
    /// (review 2026-10-11 M2). 2 if left out; 1 to 10. Shorter switches
    /// quality and resumes sooner, for more requests and a longer
    /// playlist. Encodes keep their own grid. A conversion keeps the
    /// length it started with.
    #[ucl(default)]
    pub copy_segment_secs: Option<u64>,
    /// Run a GPU encode's filters on the GPU, where it can, rather than on
    /// the CPU: on VA-API the tone map (`tonemap_vaapi`) and picture
    /// burn-in (`overlay_vaapi`); on NVIDIA the scale (`scale_cuda`) and
    /// picture burn-in (`overlay_cuda`), NVIDIA's tone map staying on the
    /// CPU. Off if left out. The tone map is `tone_map`'s.
    #[ucl(default)]
    pub gpu_filters: Option<bool>,
    /// How HDR is tone mapped to SDR (PGPU-04): `"auto"` (if left out) the
    /// job's GPU's own tone map (`tonemap_vaapi`) while `gpu_filters` is
    /// on, else libplacebo on the job's GPU (whatever `gpu_filters` says),
    /// else the CPU;
    /// `"cpu"`, `"gpu"` (the driver's own) or `"libplacebo"` forces one,
    /// whatever `gpu_filters` says, a job whose GPU cannot run it tone
    /// mapping on the CPU.
    #[ucl(default)]
    pub tone_map: Option<String>,
    /// The codecs a GPU encodes to (PGPU-06), the first preferred: of
    /// `"h264"`, `"hevc"` and `"av1"`, each at most once;
    /// `["av1", "hevc", "h264"]` if left out. A conversion takes the first
    /// the client plays (in hardware, where it says) and a GPU here
    /// encodes, keeping an HDR film's HDR where it can; with none, it is
    /// encoded in software, which is H.264 only. `[]`: no GPU encodes
    /// video.
    #[ucl(default)]
    pub video_codecs: Option<Vec<String>>,
}

/// One entry of `transcode.gpus` as written (PGPU-05).
#[derive(Debug, Clone, PartialEq, Eq, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct GpuConfig {
    /// `nvenc`, `vaapi` or `qsv`.
    pub api: String,
    /// `all`, `best`, `dgpu` or `igpu`; `all` if neither this nor
    /// `device` is set.
    #[ucl(default)]
    pub pick: Option<String>,
    /// One card: a CUDA index (`0`, as a number or a string) or a render
    /// node path.
    #[ucl(default, map = "gpu_device")]
    pub device: Option<String>,
    /// GPU video encodes at once on each of the entry's cards;
    /// `transcode.max_gpu_jobs` if left out.
    #[ucl(default)]
    pub max_jobs: Option<u32>,
}

/// A GPU entry's `device`: a CUDA index may be written as a number.
fn gpu_device(object: uclicious::ObjectRef) -> Result<Option<String>, uclicious::ObjectError> {
    if let Some(index) = object.as_i64().filter(|_| object.is_integer()) {
        return Ok(Some(index.to_string()));
    }
    let text: String = uclicious::FromObject::try_from(object)?;
    Ok(Some(text))
}

/// `transcode.gpus`' key, for its messages.
const GPUS: &str = "transcode.gpus";

impl GpuConfig {
    /// The entry it says, or why it says none.
    fn entry(&self, n: usize) -> Result<GpuEntry, ConfigError> {
        let invalid = |message: String| ConfigError::Invalid {
            key: GPUS,
            message: format!("entry {}: {message}", n + 1),
        };
        let api = match self.api.trim() {
            "nvenc" => Hardware::Nvenc,
            "vaapi" => Hardware::Vaapi,
            "qsv" => Hardware::Qsv,
            other => {
                return Err(invalid(format!(
                    "api {other:?}: expected \"nvenc\", \"vaapi\" or \"qsv\""
                )));
            }
        };
        let which = match (&self.pick, &self.device) {
            (Some(_), Some(_)) => {
                return Err(invalid(
                    "pick or device, not both: pick chooses among the cards found, device \
                     names one"
                        .into(),
                ));
            }
            (None, None) => Which::Pick(Pick::All),
            (Some(pick), None) => Which::Pick(Pick::parse(pick.trim()).ok_or_else(|| {
                invalid(format!(
                    "pick {pick:?}: expected \"all\", \"best\", \"dgpu\" or \"igpu\""
                ))
            })?),
            (None, Some(device)) => {
                let device = device.trim();
                let fits = match api {
                    Hardware::Nvenc => device.parse::<u32>().is_ok(),
                    Hardware::Vaapi | Hardware::Qsv => device.starts_with('/'),
                };
                if !fits {
                    return Err(invalid(match api {
                        Hardware::Nvenc => format!(
                            "device {device:?}: an NVIDIA card is named by its CUDA index, as \
                             nvidia-smi numbers them: 0, 1, …"
                        ),
                        _ => format!(
                            "device {device:?}: a render node's path, as \
                             /dev/dri/by-path/pci-0000:03:00.0-render"
                        ),
                    }));
                }
                Which::Device(device.to_owned())
            }
        };
        let max_jobs = match self.max_jobs {
            Some(0) => {
                return Err(invalid(
                    "max_jobs = 0 would keep every job off its cards; leave the entry out \
                     instead"
                        .into(),
                ));
            }
            max => max.map(|n| n as usize),
        };
        Ok(GpuEntry {
            api,
            which,
            max_jobs,
        })
    }
}

/// `transcode.video_codecs`' key, for its messages.
const VIDEO_CODECS: &str = "transcode.video_codecs";

/// `transcode.video_codecs` left out (PGPU-06): AV1 first, the smallest
/// for what it shows and as cheap as HEVC on a GPU that encodes it, then
/// HEVC, then H.264.
pub const DEFAULT_VIDEO_CODECS: [VideoCodec; 3] =
    [VideoCodec::Av1, VideoCodec::Hevc, VideoCodec::H264];

impl TranscodeConfig {
    /// `video_codecs`, in order (PGPU-06).
    pub fn video_codecs(&self) -> Result<Vec<VideoCodec>, ConfigError> {
        let Some(words) = &self.video_codecs else {
            return Ok(DEFAULT_VIDEO_CODECS.to_vec());
        };
        let invalid = |message: String| ConfigError::Invalid {
            key: VIDEO_CODECS,
            message,
        };
        let mut codecs = Vec::new();
        for word in one_each(VIDEO_CODECS, words, r#"["av1", "hevc", "h264"]"#)? {
            let codec = match word {
                "h264" => VideoCodec::H264,
                "hevc" => VideoCodec::Hevc,
                "av1" => VideoCodec::Av1,
                other => {
                    return Err(invalid(format!(
                        "{other:?}: expected \"h264\", \"hevc\" or \"av1\""
                    )));
                }
            };
            if codecs.contains(&codec) {
                return Err(invalid(format!("{word:?} is listed twice")));
            }
            codecs.push(codec);
        }
        Ok(codecs)
    }

    /// `tone_map`.
    pub fn tone_map(&self) -> Result<ToneMapSetting, ConfigError> {
        self.tone_map
            .as_deref()
            .unwrap_or("auto")
            .parse()
            .map_err(|message| ConfigError::Invalid {
                key: "transcode.tone_map",
                message,
            })
    }

    /// The job limits, with `state_dir` for the default scratch directory.
    pub fn limits(&self, state_dir: &Path) -> Result<JobLimits, ConfigError> {
        let at_least_one =
            |key: &'static str, value: Option<u32>, default: u32| match value.unwrap_or(default) {
                0 => Err(ConfigError::Invalid {
                    key,
                    message: "0 would refuse every conversion; use at least 1".into(),
                }),
                n => Ok(n),
            };
        let max_jobs = at_least_one("transcode.max_jobs", self.max_jobs, 2)?;
        let max_remux = at_least_one("transcode.max_remux", self.max_remux, 32)?;
        let max_gpu_jobs = at_least_one("transcode.max_gpu_jobs", self.max_gpu_jobs, 3)?;
        let scratch_dir = self
            .scratch_dir
            .clone()
            .unwrap_or_else(|| state_dir.join("cache").join("transcode"));
        // Text subtitles are burned in by an ffmpeg filter that reads the
        // film through the job's link in here: a path its filter syntax
        // misreads would fail every such burn-in (Review 2026-10-11 N5.1).
        if scratch_dir
            .display()
            .to_string()
            .contains(crate::encode::args::FILTER_SPECIAL)
        {
            return Err(ConfigError::Invalid {
                key: "transcode.scratch_dir",
                message: format!(
                    "{} has one of ' : \\ , ; [ ] in it, which ffmpeg's subtitle filter \
                     cannot read: burning text subtitles in would fail. Choose a path \
                     without them",
                    scratch_dir.display()
                ),
            });
        }
        let copy_segment_secs = match self.copy_segment_secs.unwrap_or(2) {
            secs @ 1..=10 => secs,
            other => {
                return Err(ConfigError::Invalid {
                    key: "transcode.copy_segment_secs",
                    message: format!(
                        "{other} s: a copied video's segments are 1 to 10 s long; shorter ones \
                         make more requests and a longer playlist, longer ones slower quality \
                         switches and resumes"
                    ),
                });
            }
        };
        Ok(JobLimits {
            budget: crate::encode::Units::jobs(max_jobs),
            max_remux: max_remux as usize,
            max_gpu_jobs: max_gpu_jobs as usize,
            scratch_dir,
            scratch_limit_bytes: self.scratch_limit_mb.unwrap_or(20 * 1024) << 20,
            read_ahead: Duration::from_secs(self.read_ahead_secs.unwrap_or(60)),
            copy_segment_secs,
            stalled_after: crate::encode::job::STALLED_AFTER,
            gpu_filters: self.gpu_filters.unwrap_or(false),
            tone_map: self.tone_map()?,
            video_codecs: self.video_codecs()?,
        })
    }

    /// The GPUs to use (PGPU-05): `gpus`, or every card of every API.
    pub fn gpus(&self) -> Result<Vec<GpuEntry>, ConfigError> {
        match &self.gpus {
            Some(entries) => entries
                .iter()
                .enumerate()
                .map(|(n, entry)| entry.entry(n))
                .collect(),
            None => Ok(GpuEntry::defaults()),
        }
    }

    /// What encoder detection starts from: the tone map, and whether
    /// libplacebo tries ffmpeg's Vulkan device on AMD. Each card's test
    /// sets its own API and node over it ([`Card::settings`]).
    ///
    /// [`Card::settings`]: spoolframe_media::gpus::Card::settings
    pub fn settings(
        &self,
        experimental: &ExperimentalConfig,
    ) -> Result<TranscodeSettings, ConfigError> {
        Ok(TranscodeSettings {
            tone_map: self.tone_map()?,
            libplacebo_shared_on_amd: experimental.libplacebo_shared_on_amd(),
            ..TranscodeSettings::default()
        })
    }
}

/// `artwork { cache_mb = 1024; memory_mb = 128; }`, optional.
#[derive(Debug, Clone, Default, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct ArtworkConfig {
    /// The most the cache under `state_dir/cache/artwork` may hold, in
    /// MiB; 1024 if left out.
    #[ucl(default)]
    pub cache_mb: Option<u64>,
    /// The most of it kept in memory, in MiB; 128 if left out.
    #[ucl(default)]
    pub memory_mb: Option<u64>,
}

impl ArtworkConfig {
    pub const DEFAULT_CACHE_MB: u64 = 1024;
    /// Half of the 256 MiB the caches' memory tiers take by default: the
    /// artwork is what every page shows.
    pub const DEFAULT_MEMORY_MB: u64 = 128;

    /// The cache limit in bytes.
    pub fn cache_bytes(&self) -> u64 {
        self.cache_mb.unwrap_or(Self::DEFAULT_CACHE_MB) << 20
    }

    /// The cache's memory and disk limits in bytes.
    pub fn tiers(&self) -> crate::hybrid::Tiers {
        crate::hybrid::Tiers {
            memory: self.memory_mb.unwrap_or(Self::DEFAULT_MEMORY_MB) << 20,
            disk: self.cache_bytes(),
        }
    }
}

/// `catalog { memory_mb = 64; }`, optional: the catalog's replies kept in
/// memory for a minute. Its own section, not under `plex`, which is left
/// out until a Plex server is set up; it covers every Plex server.
#[derive(Debug, Clone, Default, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct CatalogConfig {
    /// The most the replies may take, in MiB; 64 if left out.
    #[ucl(default)]
    pub memory_mb: Option<u64>,
}

impl CatalogConfig {
    /// A quarter of the 256 MiB the caches' memory tiers take by default.
    pub const DEFAULT_MEMORY_MB: u64 = 64;

    /// The replies' limit in bytes.
    pub fn memory_bytes(&self) -> u64 {
        self.memory_mb.unwrap_or(Self::DEFAULT_MEMORY_MB) << 20
    }
}

/// `federation { sync_interval_secs = 3600; cache_max_mb = 2048;
/// cache_total_mb = 8192; … }`, optional (P8-02): how often this server
/// syncs what each paired server shares with it, and the most each one's
/// artwork, and all of theirs together, may take under
/// `state_dir/cache/federation`; and the limits on what a pair may send
/// and ask (P8-04, the 2026-10-23 decision), each with today's value as
/// its default and a floor. Live settings; read only while
/// `experimental.federation` is on. A connection's streams change for
/// the connections made after.
#[derive(Debug, Clone, Default, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct FederationConfig {
    /// Seconds between two syncs of a pair; an hour if left out, a minute
    /// at the least, a week at the most.
    #[ucl(default)]
    pub sync_interval_secs: Option<u64>,
    /// MiB; 2048 if left out.
    #[ucl(default)]
    pub cache_max_mb: Option<u64>,
    /// MiB, every paired server's together; 8192 if left out.
    #[ucl(default)]
    pub cache_total_mb: Option<u64>,
    /// Pages in one library's sync; 2,000 if left out, 10 at the least,
    /// 1,000,000 at the most.
    #[ucl(default)]
    pub sync_max_pages: Option<u64>,
    /// Items in one library's sync; 200,000 if left out, 1,000 at the
    /// least, 100,000,000 at the most.
    #[ucl(default)]
    pub sync_max_items: Option<u64>,
    /// Items cached of one paired server, every library together; 500,000
    /// if left out, 1,000 at the least, 100,000,000 at the most.
    #[ucl(default)]
    pub max_cached_items: Option<u64>,
    /// MiB of metadata in one library's sync, as the cache keeps it (JSON,
    /// escaped); 1024 if left out, 16 at the least, 65,536 at the most
    /// (C8-02).
    #[ucl(default)]
    pub sync_max_metadata_mb: Option<u64>,
    /// MiB of metadata cached of one paired server, every library
    /// together; 2048 if left out, 64 at the least, 262,144 at the most.
    #[ucl(default)]
    pub max_cached_metadata_mb: Option<u64>,
    /// Seconds one walk of a library answers every pair's sync; 60 if left
    /// out, 10 at the least, a day at the most.
    #[ucl(default)]
    pub walk_reuse_secs: Option<u64>,
    /// Streams a pair's connection is answered on at once; 16 if left out,
    /// 4 at the least, 1,024 at the most.
    #[ucl(default)]
    pub max_streams: Option<u64>,
    /// Images a pair may ask for in a burst, then a hundred a second; 200
    /// if left out, 10 at the least, 100,000 at the most.
    #[ucl(default)]
    pub artwork_burst: Option<u64>,
    /// Plays a pair's users may start in a burst, then one a second; 30 if
    /// left out, 5 at the least, 10,000 at the most.
    #[ucl(default)]
    pub assertion_burst: Option<u64>,
    /// Play tokens that work at once for one user of a pair; 10 if left
    /// out, 2 at the least, 10,000 at the most.
    #[ucl(default)]
    pub tokens_per_user: Option<u64>,
    /// Play tokens that work at once for a pair; 500 if left out, 10 at the
    /// least, 1,000,000 at the most.
    #[ucl(default)]
    pub tokens_per_server: Option<u64>,
    /// Download tokens (P8-07) that work at once for one user of a pair,
    /// each a download being made or read; 200 if left out, 10 at the
    /// least, 100,000 at the most. Apart from play tokens, so downloads
    /// never take a play's place.
    #[ucl(default)]
    pub download_tokens_per_user: Option<u64>,
    /// Download tokens that work at once for a pair; 2,000 if left out, 50
    /// at the least, 10,000,000 at the most.
    #[ucl(default)]
    pub download_tokens_per_server: Option<u64>,
    /// Play sessions a pair's users hold open at once (P8-06); 200 if
    /// left out, 10 at the least, 10,000 at the most, which is also the
    /// most of every pair's together.
    #[ucl(default)]
    pub play_sessions_per_server: Option<u64>,
    /// Subtitle searches and downloads a pair's users may ask in a burst,
    /// then one every 10 s (P8-07); 20 if left out, 2 at the least, 10,000
    /// at the most. Downloads are also held to `subtitles {
    /// daily_limit_per_peer }`.
    #[ucl(default)]
    pub subtitle_burst: Option<u64>,
}

impl FederationConfig {
    pub const DEFAULT_SYNC_INTERVAL_SECS: u64 = 3600;
    pub const MIN_SYNC_INTERVAL_SECS: u64 = 60;
    /// A week: past it a pair's sync is as good as never, and a time
    /// added to now stays far from overflowing.
    pub const MAX_SYNC_INTERVAL_SECS: u64 = 7 * 86_400;
    pub const DEFAULT_CACHE_MAX_MB: u64 = 2048;
    pub const DEFAULT_CACHE_TOTAL_MB: u64 = 8192;

    /// The time between two syncs of a pair.
    pub fn sync_every(&self) -> Duration {
        Duration::from_secs(
            self.sync_interval_secs
                .unwrap_or(Self::DEFAULT_SYNC_INTERVAL_SECS)
                .clamp(Self::MIN_SYNC_INTERVAL_SECS, Self::MAX_SYNC_INTERVAL_SECS),
        )
    }

    /// The most each pair's artwork may take, in bytes.
    pub fn cache_max_bytes(&self) -> u64 {
        self.cache_max_mb
            .unwrap_or(Self::DEFAULT_CACHE_MAX_MB)
            .saturating_mul(1 << 20)
    }

    /// The most every pair's artwork may take together, in bytes.
    pub fn cache_total_bytes(&self) -> u64 {
        self.cache_total_mb
            .unwrap_or(Self::DEFAULT_CACHE_TOTAL_MB)
            .saturating_mul(1 << 20)
    }

    /// What a pair may send and ask (P8-04): each setting, or its default,
    /// held between its floor and its ceiling. The ceilings keep a value
    /// typed too large from what it is used for: a semaphore's permits, a
    /// time added to now, a count bound into SQL as an i64.
    pub fn limits(&self) -> crate::federation::Limits {
        use crate::federation::Limits;
        let d = Limits::default();
        let within = |set: Option<u64>, default: u64, (floor, ceiling): (u64, u64)| {
            set.unwrap_or(default).clamp(floor, ceiling)
        };
        let small = |set: Option<u64>, default: u32, range: (u64, u64)| {
            u32::try_from(within(set, default.into(), range)).unwrap_or(u32::MAX)
        };
        Limits {
            sync: crate::federation::SyncLimits {
                pages: within(self.sync_max_pages, d.sync.pages, (10, 1_000_000)),
                items: within(self.sync_max_items, d.sync.items, (1_000, 100_000_000)),
                origin_items: within(
                    self.max_cached_items,
                    d.sync.origin_items,
                    (1_000, 100_000_000),
                ),
                bytes: within(self.sync_max_metadata_mb, d.sync.bytes >> 20, (16, 65_536)) << 20,
                origin_bytes: within(
                    self.max_cached_metadata_mb,
                    d.sync.origin_bytes >> 20,
                    (64, 262_144),
                ) << 20,
            },
            walk_reuse: Duration::from_secs(within(
                self.walk_reuse_secs,
                d.walk_reuse.as_secs(),
                (10, 86_400),
            )),
            max_streams: usize::try_from(within(
                self.max_streams,
                d.max_streams as u64,
                (4, 1_024),
            ))
            .unwrap_or(1_024),
            artwork_burst: small(self.artwork_burst, d.artwork_burst, (10, 100_000)),
            assertion_burst: small(self.assertion_burst, d.assertion_burst, (5, 10_000)),
            tokens: crate::store::federation_tokens::TokenLimits {
                per_user: within(self.tokens_per_user, d.tokens.per_user, (2, 10_000)),
                per_peer: within(self.tokens_per_server, d.tokens.per_peer, (10, 1_000_000)),
            },
            subtitle_burst: small(self.subtitle_burst, d.subtitle_burst, (2, 10_000)),
            download_tokens: crate::store::federation_tokens::TokenLimits {
                per_user: within(
                    self.download_tokens_per_user,
                    d.download_tokens.per_user,
                    (10, 100_000),
                ),
                per_peer: within(
                    self.download_tokens_per_server,
                    d.download_tokens.per_peer,
                    (50, 10_000_000),
                ),
            },
            play_sessions: within(
                self.play_sessions_per_server,
                d.play_sessions,
                (10, crate::remote_sessions::MAX_PLAY_SESSIONS),
            ),
        }
    }
}

/// `admin { listen = "0.0.0.0:7421"; public_url = "https://media.example.org";
/// trusted_proxies = ["10.0.0.5", "172.17.0.0/16"]; insecure_cookies = false; }`,
/// all optional.
#[derive(Debug, Clone, Default, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct AdminConfig {
    /// Where the admin API and the web UI listen; every IPv4 interface on
    /// port 7421 if left out (P3-10). Loopback, with `trusted_proxies`
    /// naming it, for a host whose TLS proxy runs on the same machine.
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
    /// The reverse proxies in front of the listener: a list of addresses
    /// and CIDR ranges. A connection from one of them is counted
    /// by the failure limit under the caller its `X-Forwarded-For` names
    /// (the right-most address that is not a trusted proxy; `Forwarded` is
    /// not read), and under no address if it names none. Left out, no
    /// header is read: every connection counts under its own address, and
    /// a loopback one under none.
    #[ucl(default)]
    pub trusted_proxies: Option<Vec<String>>,
    /// Nothing proxies to this listener: a loopback caller is on this
    /// host, or comes through an SSH tunnel. Only read with a public URL
    /// and no `trusted_proxies`: a loopback listener then means a proxy
    /// on this host, and is refused without it (PX-11 finding 38); one on
    /// every interface may have one, and is warned of at start without it
    /// (finding 58). Off if left out.
    #[ucl(default)]
    pub not_proxied: Option<bool>,
    /// Send the session cookie without `Secure`, so signing in over plain
    /// HTTP on the LAN works: the cookie then crosses the network in
    /// clear. It stays `HttpOnly`. Off if left out, and refused while
    /// `public_url` is `https` (PX-11).
    #[ucl(default)]
    pub insecure_cookies: Option<bool>,
    /// Answer `GET /metrics` with Prometheus text (P3-14). Off if left
    /// out: the listener says nothing about it.
    #[ucl(default)]
    pub metrics: Option<bool>,
    /// The addresses and CIDR ranges `GET /metrics` answers, a list;
    /// loopback if left out, so a scraper on the same
    /// host works and nobody else does. Behind a trusted proxy, the
    /// address it forwards for.
    #[ucl(default)]
    pub metrics_from: Option<Vec<String>>,
}

/// A list setting's entries, trimmed. One with a comma in it is the
/// comma-separated string these settings were before they were lists,
/// refused with how to write it as one: `example`.
fn one_each<'a>(
    key: &'static str,
    entries: &'a [String],
    example: &str,
) -> Result<Vec<&'a str>, ConfigError> {
    entries
        .iter()
        .map(|entry| {
            let entry = entry.trim();
            if entry.contains(',') {
                return Err(ConfigError::Invalid {
                    key,
                    message: format!("{entry:?}: a list now, one entry each, as {example}"),
                });
            }
            Ok(entry)
        })
        .collect()
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
        const KEY: &str = "admin.trusted_proxies";
        let Some(list) = &self.trusted_proxies else {
            return Ok(crate::admin::TrustedProxies::default());
        };
        let entries = one_each(KEY, list, r#"["127.0.0.1", "10.0.0.0/8"]"#)?;
        crate::admin::TrustedProxies::new(&entries)
            .map_err(|message| ConfigError::Invalid { key: KEY, message })
    }

    pub fn metrics(&self) -> bool {
        self.metrics.unwrap_or(false)
    }

    pub fn not_proxied(&self) -> bool {
        self.not_proxied.unwrap_or(false)
    }

    /// Who may read `GET /metrics`.
    pub fn metrics_from(&self) -> Result<crate::admin::AddressRanges, ConfigError> {
        const KEY: &str = "admin.metrics_from";
        let Some(list) = &self.metrics_from else {
            return Ok(crate::admin::AddressRanges::loopback());
        };
        let entries = one_each(KEY, list, r#"["127.0.0.0/8", "::1"]"#)?;
        crate::admin::AddressRanges::new(&entries)
            .map_err(|message| ConfigError::Invalid { key: KEY, message })
    }

    /// Whether this is the shape of a proxy on the same host that the
    /// server is not told about (PX-11 finding 38): a loopback listener
    /// with a public URL and no `trusted_proxies`. Every caller through
    /// such a proxy looks like loopback: failed logins then share no
    /// source, and `/metrics`, when on, answers anyone the proxy lets in
    /// (PX-09 finding 29).
    pub fn proxy_unnamed(&self) -> Result<bool, ConfigError> {
        Ok(self.listen()?.ip().is_loopback()
            && self.public_url.is_some()
            && self.trusted_proxies()? == crate::admin::TrustedProxies::default())
    }

    /// Whether a proxy on this host may be in front of a listener on
    /// every interface without the server told (finding 58, the third
    /// review's G-9): a public URL, no `trusted_proxies`, and no
    /// `not_proxied`. Through such a proxy every caller looks like
    /// loopback, as in [`Self::proxy_unnamed`]'s shape; but browsers may
    /// as well reach this listener directly, or through an SSH tunnel,
    /// so it is warned of at start, not refused.
    pub fn proxy_maybe_unnamed(&self) -> Result<bool, ConfigError> {
        Ok(self.listen()?.ip().is_unspecified()
            && self.public_url.is_some()
            && self.trusted_proxies()? == crate::admin::TrustedProxies::default()
            && !self.not_proxied())
    }

    /// Refuses [`Self::proxy_unnamed`]'s shape unless `not_proxied` says
    /// nothing proxies to the listener (the second review's M-4). Checked
    /// at every parse, so a change at run time that would make it is
    /// refused too, and the server is never left unable to start.
    fn check_proxy(&self) -> Result<(), ConfigError> {
        if self.proxy_unnamed()? && !self.not_proxied() {
            return Err(ConfigError::Invalid {
                key: "admin.trusted_proxies",
                message: "is empty, but admin.listen is loopback and admin.public_url is set, \
                          so a proxy on this host is in front, and every caller through it \
                          would look like loopback: failed logins would not be limited per \
                          caller, and /metrics, when on, would answer whoever the proxy lets \
                          in. Set admin.trusted_proxies to the proxy's address (\"127.0.0.1\" \
                          for one on this host); or, if nothing proxies to this listener, \
                          set admin.not_proxied = true"
                    .into(),
            });
        }
        Ok(())
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

/// `network { port = 7420; relays = ["default"]; mdns = true; }`, all
/// optional.
#[derive(Debug, Clone, Default, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct NetworkConfig {
    /// The UDP port to listen on; a free one is picked if left out. Fix it
    /// when a firewall needs to know.
    #[ucl(default)]
    pub port: Option<u16>,
    /// Listen on IPv6 too. On unless set to false. Off, the server listens
    /// on IPv4 only, so no device reaches it over IPv6, directly or by hole
    /// punching.
    #[ucl(default)]
    pub ipv6: Option<bool>,
    /// The relays the endpoint may take its home relay from (P6-03): a
    /// list of `default` (n0's public relays) and relay URLs. Empty or left
    /// out: relays off, the server reached on the LAN or at a public
    /// address.
    #[ucl(default)]
    pub relays: Option<Vec<String>>,
    /// The single relay before P6-03. Refused, naming `relays`: a config
    /// that still sets it meant something by it.
    #[ucl(default)]
    pub relay: Option<String>,
    /// Optional, for one case only (P6-05): a device that knows the
    /// server's identity but no longer an address that reaches it (the
    /// public IP changed while it was away). The server publishes where it
    /// is, signed with its key, for the device to look it up by that key.
    /// Not needed on the LAN, with a public address that does not change,
    /// or with relays on. `"off"` (the default), `"n0"` (n0's public
    /// service at dns.iroh.link), or the URL of a pkarr relay of the
    /// operator's own (an `iroh-dns-server`'s `/pkarr`). The record is
    /// public: relays and public addresses only, never a LAN address.
    #[ucl(default)]
    pub pkarr: Option<String>,
    /// The relays of the operator's own (P6-04, `spoolframe-server relay`)
    /// this server tells who an endpoint is, so they let its devices
    /// through: a list of their keys (`spoolframe-server relay id`). None
    /// by default.
    #[ucl(default)]
    pub own_relays: Option<Vec<String>>,
    /// Announce the server on the LAN over mDNS (P1-13), so clients list
    /// it without being told its address. On unless set to `false`.
    #[ucl(default)]
    pub mdns: Option<bool>,
    /// The UDP port Plex's LAN discovery (GDM) searches from, which each
    /// PMS answers to. A host firewall drops those answers unless it is
    /// open: it cannot match them to a search sent to a multicast group.
    /// [`spoolframe_plex::gdm::PORT`] if left out; 0 is any free port.
    #[ucl(default)]
    pub plex_discovery_port: Option<u16>,
    /// The most connections held at once from devices with no live
    /// session (PX-10); one more is refused until one goes. A signed-in
    /// device is always let in.
    #[ucl(default)]
    pub max_stranger_connections: Option<u32>,
    /// How long a stream's first request may take to arrive before the
    /// stream is reset (PX-10).
    #[ucl(default)]
    pub first_frame_timeout_secs: Option<u64>,
    /// Seconds a media or artwork stream may wait for its reader to take
    /// any of it before it is reset; 120 if left out.
    #[ucl(default)]
    pub stalled_read_secs: Option<u64>,
    /// Which of its addresses a signed-in device is told (PX-12).
    #[ucl(default)]
    pub announce: AnnounceConfig,
    /// The server's public addresses (P6-02); absent, none is advertised.
    #[ucl(default)]
    pub wan: Option<WanConfig>,
    /// Address ranges whose devices are on the LAN besides this host's
    /// own subnets (PX-46): a WireGuard network, another VLAN. A playback
    /// on the LAN is not counted against the uplink budget.
    #[ucl(default)]
    pub lan_prefixes: Option<Vec<String>>,
    /// Address ranges whose devices are off the LAN even at a private
    /// address or within `lan_prefixes`: a WireGuard peer known to be
    /// away. Between two ranges that both hold an address, the longer
    /// prefix decides.
    #[ucl(default)]
    pub wan_prefixes: Option<Vec<String>>,
    /// The WAN uplink budget and its share (PX-46).
    #[ucl(default)]
    pub uplink: UplinkConfig,
    /// How QUIC paths share the link: `"cubic"` (the default) or `"bbr"`,
    /// which holds its rate on a lossy link such as cellular. Every path,
    /// LAN ones too: noq picks one controller for all of them.
    #[ucl(default)]
    pub congestion_control: Option<String>,
}

/// `uplink { budget_mbps = 20; max_streams = 4; owner_share_percent =
/// 50; }` (PX-46), all optional.
#[derive(Debug, Clone, Default, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct UplinkConfig {
    /// What the playbacks off the LAN may take together, in Mbit/s; 0 or
    /// left out is no limit.
    #[ucl(default)]
    pub budget_mbps: Option<u64>,
    /// The most playbacks off the LAN at once; 0 or left out is no limit.
    #[ucl(default)]
    pub max_streams: Option<u32>,
    /// The owners' (admins') part of the budget while one of them plays
    /// off the LAN beside anyone else, in percent; 50 if left out.
    #[ucl(default)]
    pub owner_share_percent: Option<u32>,
}

impl NetworkConfig {
    /// `network.uplink`, `network.lan_prefixes` and `network.wan_prefixes`,
    /// checked (PX-46).
    pub fn uplink(&self) -> Result<crate::uplink::UplinkSettings, ConfigError> {
        let share = self.uplink.owner_share_percent.map_or(
            Ok(crate::uplink::DEFAULT_OWNER_SHARE_PERCENT),
            |p| {
                u8::try_from(p)
                    .ok()
                    .filter(|p| *p <= 100)
                    .ok_or_else(|| ConfigError::Invalid {
                        key: "network.uplink.owner_share_percent",
                        message: format!("{p}: a percentage, 0 to 100"),
                    })
            },
        )?;
        let ranges = |list: &Option<Vec<String>>, key: &'static str| {
            list.as_deref()
                .unwrap_or_default()
                .iter()
                .map(|text| {
                    text.trim()
                        .parse()
                        .map_err(|message| ConfigError::Invalid { key, message })
                })
                .collect::<Result<Vec<_>, _>>()
        };
        let lan_prefixes = ranges(&self.lan_prefixes, "network.lan_prefixes")?;
        let wan_prefixes = ranges(&self.wan_prefixes, "network.wan_prefixes")?;
        Ok(crate::uplink::UplinkSettings {
            budget: self
                .uplink
                .budget_mbps
                .filter(|m| *m > 0)
                .map(|m| m.saturating_mul(1_000_000)),
            max_streams: self
                .uplink
                .max_streams
                .filter(|m| *m > 0)
                .map(|m| m as usize),
            owner_share_percent: share,
            lan_prefixes,
            wan_prefixes,
        })
    }
}

/// `wan { static = ["1.2.3.4:7420"]; mapped_port { port = 7420;
/// mode = "auto"; public_ip = "lookup"; lookup_via = "wtfismyip"; } }`:
/// what a device off the LAN dials (P6-02). Each part is optional;
/// `mapped_port` is on with its defaults unless set to `false`. The host's
/// own IPv6 addresses need no part: iroh tells devices about them itself.
#[derive(Debug, Clone, Default, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct WanConfig {
    /// Public addresses with their ports, advertised as written.
    #[ucl(default, path = "static")]
    pub statics: Option<Vec<String>>,
    /// The port forwarded on the router, or `false` for none.
    #[ucl(default, map = "mapped_port_setting")]
    pub mapped_port: Option<MappedPortSetting>,
}

/// `mapped_port`: `false`, or its table.
#[derive(Debug, Clone)]
pub enum MappedPortSetting {
    Off,
    On(MappedPortConfig),
}

/// `mapped_port { port; mode; public_ip; lookup_via; }`, all optional.
#[derive(Debug, Clone, Default, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct MappedPortConfig {
    /// The public port; `network.port` if left out.
    #[ucl(default)]
    pub port: Option<u16>,
    /// `"auto"` (the default): ask the router; `"manual"`: the operator
    /// forwarded the port.
    #[ucl(default)]
    pub mode: Option<String>,
    /// `"lookup"` (the default), or the public address.
    #[ucl(default)]
    pub public_ip: Option<String>,
    /// `"wtfismyip"` (the default), or `false`: never look the address up.
    #[ucl(default, map = "lookup_via_setting")]
    pub lookup_via: Option<LookupViaSetting>,
}

/// `lookup_via`: `false`, or a service's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LookupViaSetting {
    Off,
    Named(String),
}

fn mapped_port_setting(
    object: uclicious::ObjectRef,
) -> Result<Option<MappedPortSetting>, uclicious::ObjectError> {
    if let Some(on) = object.as_bool() {
        return Ok(Some(if on {
            MappedPortSetting::On(MappedPortConfig::default())
        } else {
            MappedPortSetting::Off
        }));
    }
    let table: MappedPortConfig = uclicious::FromObject::try_from(object)?;
    Ok(Some(MappedPortSetting::On(table)))
}

fn lookup_via_setting(
    object: uclicious::ObjectRef,
) -> Result<Option<LookupViaSetting>, uclicious::ObjectError> {
    match object.as_bool() {
        Some(false) => Ok(Some(LookupViaSetting::Off)),
        Some(true) => Err(uclicious::ObjectError::other(
            "lookup_via is false or a service's name (\"wtfismyip\"), not true",
        )),
        None => {
            let name: String = uclicious::FromObject::try_from(object)?;
            Ok(Some(LookupViaSetting::Named(name)))
        }
    }
}

impl WanConfig {
    /// The settings, checked; `network_port` is `network.port`.
    pub fn wan(&self, network_port: Option<u16>) -> Result<crate::wan::Wan, ConfigError> {
        use crate::wan::{LookupVia, MappedPort, Mode, PublicIp};
        let statics = self
            .statics
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|entry| {
                crate::wan::parse_static(entry).map_err(|message| ConfigError::Invalid {
                    key: "network.wan.static",
                    message,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let table = match &self.mapped_port {
            Some(MappedPortSetting::Off) => None,
            Some(MappedPortSetting::On(table)) => Some(table.clone()),
            None => Some(MappedPortConfig::default()),
        };
        let mapped_port = match table {
            None => None,
            Some(table) => {
                let mode = match table.mode.as_deref() {
                    None | Some("auto") => Mode::Auto,
                    Some("manual") => Mode::Manual,
                    Some(other) => {
                        return Err(ConfigError::Invalid {
                            key: "network.wan.mapped_port.mode",
                            message: format!("{other:?}: \"auto\" or \"manual\""),
                        });
                    }
                };
                let public_ip = match table.public_ip.as_deref().map(str::trim) {
                    None | Some("lookup") => PublicIp::Lookup,
                    Some(text) => {
                        let ip: IpAddr = text.parse().map_err(|_| ConfigError::Invalid {
                            key: "network.wan.mapped_port.public_ip",
                            message: format!("{text:?}: \"lookup\", or an IP address"),
                        })?;
                        let ip = ip.to_canonical();
                        if let Some(why) = crate::wan::not_public(ip) {
                            return Err(ConfigError::Invalid {
                                key: "network.wan.mapped_port.public_ip",
                                message: format!(
                                    "{text:?} is {why}, which no device on the Internet reaches"
                                ),
                            });
                        }
                        PublicIp::Fixed(ip)
                    }
                };
                let lookup_via = match &table.lookup_via {
                    None => Some(LookupVia::Wtfismyip),
                    Some(LookupViaSetting::Off) => None,
                    Some(LookupViaSetting::Named(name)) => match name.trim() {
                        "wtfismyip" => Some(LookupVia::Wtfismyip),
                        other => {
                            return Err(ConfigError::Invalid {
                                key: "network.wan.mapped_port.lookup_via",
                                message: format!("{other:?}: false, or \"wtfismyip\""),
                            });
                        }
                    },
                };
                if public_ip == PublicIp::Lookup && lookup_via.is_none() {
                    return Err(ConfigError::Invalid {
                        key: "network.wan.mapped_port.public_ip",
                        message: "is \"lookup\", but network.wan.mapped_port.lookup_via = \
                                  false forbids looking the address up: set public_ip to this \
                                  network's public address, or lookup_via to \"wtfismyip\""
                            .into(),
                    });
                }
                let port = table.port.or(network_port);
                if port == Some(0) {
                    return Err(ConfigError::Invalid {
                        key: "network.wan.mapped_port.port",
                        message: "must not be 0".into(),
                    });
                }
                if mode == Mode::Manual && port.is_none() {
                    return Err(ConfigError::Invalid {
                        key: "network.wan.mapped_port.port",
                        message: "is needed with mode = \"manual\": the port forwarded on the \
                                  router (or set network.port, which it defaults to)"
                            .into(),
                    });
                }
                Some(MappedPort {
                    port,
                    mode,
                    public_ip,
                    lookup_via,
                })
            }
        };
        Ok(crate::wan::Wan {
            statics,
            mapped_port,
        })
    }
}

/// `announce { add = ["203.0.113.7"]; ignore = ["172.17.0.0/16"];
/// ignore_interfaces = ["docker*"]; interfaces = ["enp*"]; }`, all
/// optional: which of the endpoint's addresses a signed-in device is told
/// (PX-12). The endpoint has an address on every interface, container
/// bridges included, and no device reaches those. Interface names may
/// have `*` and `?` in them.
#[derive(Debug, Clone, Default, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct AnnounceConfig {
    /// Addresses to tell besides the endpoint's own, as `ip` or
    /// `ip:port` (`[ipv6]:port`): one the host does not see, as a NAT's
    /// public address. Without a port, the endpoint's. Told even when an
    /// ignore below matches it.
    #[ucl(default)]
    pub add: Option<Vec<String>>,
    /// Addresses and CIDR ranges never told.
    #[ucl(default)]
    pub ignore: Option<Vec<String>>,
    /// Interfaces whose addresses are never told, even when `interfaces`
    /// names them. Virtual ones ([`crate::announce::VIRTUAL_INTERFACES`])
    /// are left out without being listed here.
    #[ucl(default)]
    pub ignore_interfaces: Option<Vec<String>>,
    /// If set, only these interfaces' addresses are told. A virtual
    /// interface's are told only when one of these names it.
    #[ucl(default)]
    pub interfaces: Option<Vec<String>>,
}

impl AnnounceConfig {
    /// The settings, checked.
    pub fn announce(&self) -> Result<crate::announce::Announce, ConfigError> {
        fn each<T: std::str::FromStr<Err = String>>(
            key: &'static str,
            list: Option<&[String]>,
        ) -> Result<Vec<T>, ConfigError> {
            list.unwrap_or_default()
                .iter()
                .map(|entry| {
                    entry
                        .parse()
                        .map_err(|message| ConfigError::Invalid { key, message })
                })
                .collect()
        }
        Ok(crate::announce::Announce {
            add: each("network.announce.add", self.add.as_deref())?,
            ignore: each("network.announce.ignore", self.ignore.as_deref())?,
            ignore_interfaces: each(
                "network.announce.ignore_interfaces",
                self.ignore_interfaces.as_deref(),
            )?,
            interfaces: each("network.announce.interfaces", self.interfaces.as_deref())?,
        })
    }
}

impl NetworkConfig {
    /// `plex_discovery_port`, its default filled in.
    pub fn plex_discovery_port(&self) -> u16 {
        self.plex_discovery_port
            .unwrap_or(spoolframe_plex::gdm::PORT)
    }

    /// `relays`, each entry checked: `default` or an http(s) URL, none
    /// twice. `disabled` is not one: an empty list says that.
    fn relays(&self) -> Result<Vec<Relay>, ConfigError> {
        let invalid = |message: String| ConfigError::Invalid {
            key: "network.relays",
            message,
        };
        let mut relays = Vec::new();
        let entries = one_each(
            "network.relays",
            self.relays.as_deref().unwrap_or_default(),
            r#"["default", "https://relay.example.com"]"#,
        )?;
        for entry in entries {
            let relay = if entry == "default" {
                Relay::Default
            } else {
                let url = Url::parse(entry).map_err(|e| {
                    invalid(format!(
                        "{entry:?}: {e}; an entry is \"default\" or a relay URL, and an \
                         empty list turns relays off"
                    ))
                })?;
                if !matches!(url.scheme(), "https" | "http") {
                    return Err(invalid(format!("{entry:?}: a relay URL is http(s)")));
                }
                Relay::Custom(url)
            };
            if relays.contains(&relay) {
                return Err(invalid(format!("{entry:?} is listed twice")));
            }
            relays.push(relay);
        }
        Ok(relays)
    }

    /// Whether `own_relays` names relays while `relays` is empty (P6-07,
    /// glm E-6 and ds4 P6-5): the server answers them, but tells no
    /// device about any relay, so they serve only devices that know their
    /// URL another way. Warned of at start, not refused: that may be meant.
    pub fn own_relays_untold(&self) -> Result<bool, ConfigError> {
        Ok(!self.own_relays()?.is_empty() && self.relays()?.is_empty())
    }

    /// `own_relays`: a list of endpoint keys.
    fn own_relays(&self) -> Result<Vec<iroh::EndpointId>, ConfigError> {
        const KEY: &str = "network.own_relays";
        let mut keys = Vec::new();
        let entries = one_each(
            KEY,
            self.own_relays.as_deref().unwrap_or_default(),
            r#"["<key>", "<key>"]"#,
        )?;
        for entry in entries {
            let key = entry.parse().map_err(|e| ConfigError::Invalid {
                key: KEY,
                message: format!("{entry:?}: {e}; a relay's key is `spoolframe-server relay id`'s"),
            })?;
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
        Ok(keys)
    }

    /// `pkarr`: off, n0's, or a pkarr relay's http(s) URL.
    fn pkarr(&self) -> Result<Pkarr, ConfigError> {
        Ok(match self.pkarr.as_deref().map(str::trim) {
            None | Some("off") => Pkarr::Off,
            Some("n0") => Pkarr::N0,
            Some(text) => {
                let invalid = |message: String| ConfigError::Invalid {
                    key: "network.pkarr",
                    message,
                };
                let url = Url::parse(text).map_err(|e| {
                    invalid(format!(
                        "{text:?}: {e}; it is \"off\", \"n0\" or a pkarr relay's URL"
                    ))
                })?;
                if !matches!(url.scheme(), "https" | "http") {
                    return Err(invalid(format!("{text:?}: a pkarr relay's URL is http(s)")));
                }
                Pkarr::Custom(url)
            }
        })
    }

    /// How to build the endpoint.
    pub fn endpoint_options(&self) -> Result<EndpointOptions, ConfigError> {
        if let Some(old) = &self.relay {
            return Err(ConfigError::Invalid {
                key: "network.relay",
                message: format!(
                    "{old:?}: replaced by network.relays, a list of \"default\" and relay \
                     URLs; empty for none"
                ),
            });
        }
        let relays = self.relays()?;
        let pkarr = self.pkarr()?;
        let own_relays = self.own_relays()?;
        let congestion = match self.congestion_control.as_deref().map(str::trim) {
            None | Some("cubic") => crate::network::CongestionControl::Cubic,
            Some("bbr") => crate::network::CongestionControl::Bbr,
            Some(other) => {
                return Err(ConfigError::Invalid {
                    key: "network.congestion_control",
                    message: format!("{other:?}: it is \"cubic\" or \"bbr\""),
                });
            }
        };
        let admission = Admission {
            max_strangers: self
                .max_stranger_connections
                .map_or(Admission::DEFAULT_MAX_STRANGERS, |n| n as usize),
            first_frame: self
                .first_frame_timeout_secs
                .map_or(Admission::DEFAULT_FIRST_FRAME, Duration::from_secs),
            stalled_read: self
                .stalled_read_secs
                .map_or(Admission::DEFAULT_STALLED_READ, Duration::from_secs),
            ..Admission::default()
        };
        if admission.max_strangers == 0 {
            return Err(ConfigError::Invalid {
                key: "network.max_stranger_connections",
                message: "must be at least 1, or no device could sign in".into(),
            });
        }
        if admission.first_frame.is_zero() {
            return Err(ConfigError::Invalid {
                key: "network.first_frame_timeout_secs",
                message: "must be at least 1".into(),
            });
        }
        if admission.stalled_read.is_zero() {
            return Err(ConfigError::Invalid {
                key: "network.stalled_read_secs",
                message: "must be at least 1".into(),
            });
        }
        Ok(EndpointOptions {
            port: self.port,
            ipv6: self.ipv6.unwrap_or(true),
            relays,
            pkarr,
            own_relays,
            mdns: self.mdns.unwrap_or(true),
            admission,
            announce: self.announce.announce()?,
            wan: self
                .wan
                .as_ref()
                .map(|wan| wan.wan(self.port))
                .transpose()?,
            congestion,
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
    secrets::warn_if_open(path);
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
    /// The shortest interval taken: a sync asks Plex for every library's
    /// watch state, and more often than this only loads the PMS.
    pub const MIN_INTERVAL_SECS: u64 = 10;

    pub fn interval(&self) -> Duration {
        self.interval_secs
            .map_or(Self::DEFAULT_INTERVAL, Duration::from_secs)
    }

    /// An interval under [`Self::MIN_INTERVAL_SECS`] is refused, not
    /// quietly raised to it.
    fn check(&self) -> Result<(), ConfigError> {
        match self.interval_secs {
            Some(secs) if secs < Self::MIN_INTERVAL_SECS => Err(ConfigError::Invalid {
                key: "plex.watch_sync.interval_secs",
                message: format!(
                    "{secs}: a sync runs at most every {} seconds",
                    Self::MIN_INTERVAL_SECS
                ),
            }),
            _ => Ok(()),
        }
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
        if let Some(sync) = &self.watch_sync {
            sync.check()?;
        }
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
        parsed.admin.check_proxy()?;
        parsed.admin.metrics_from()?;
        parsed.auth.check(&parsed.admin)?;
        parsed.auth.trust_idle()?;
        parsed.watch.rules()?;
        parsed.transcode.settings(&parsed.experimental)?;
        parsed.transcode.gpus()?;
        // Any directory: only the values are checked here.
        parsed.transcode.limits(Path::new("/"))?;
        parsed.subtitles.ocr_settings(Path::new("/"))?;
        parsed.previews.settings()?;
        parsed.areas(Path::new("/"))?;
        parsed.logs.buffer_lines()?;
        parsed.telemetry.otlp.settings()?;
        parsed.http_playback.settings()?;
        parsed.mcp.allowed_redirects()?;
        parsed.subtitle_downloads()?;
        Ok(parsed)
    }

    /// Parse merged layers.
    /// `${SF_ENV_…}` is expanded here and only here (PX-22): the layers,
    /// `config show`, the settings API and the runtime layer written back
    /// all keep the reference.
    pub fn from_table(table: &tree::Table) -> Result<Self, ConfigError> {
        Self::from_table_with_env(table, &|name| std::env::var(name).ok())
    }

    pub fn from_table_with_env(
        table: &tree::Table,
        env: &impl Fn(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
        let expanded = tree::expand_env(table, env)
            .map_err(|(key, var)| ConfigError::EnvUnset { key, var })?;
        Self::parse(&tree::to_document(&expanded))
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

    fn opensubtitles(block: &str) -> Result<ServerConfig, ConfigError> {
        ServerConfig::parse(&format!(
            r#"schema = "spoolframe.server.v1"; opensubtitles {{ {block} }}"#
        ))
    }

    fn sites(config: &ServerConfig) -> Vec<crate::subtitle_downloads::SiteSettings> {
        config.subtitle_downloads().unwrap().sites
    }

    /// PX-02: OpenSubtitles is on with nothing set, with its built-in
    /// key; English and no limit unless set; the password never printed.
    #[test]
    fn opensubtitles_is_on_with_nothing_set() {
        let none = ServerConfig::parse(r#"schema = "spoolframe.server.v1";"#).unwrap();
        let [
            crate::subtitle_downloads::SiteSettings {
                languages,
                kind: crate::subtitle_downloads::SiteKind::OpenSubtitles(account),
            },
        ] = &sites(&none)[..]
        else {
            panic!("{:?}", sites(&none))
        };
        assert_eq!(languages, &["en"]);
        assert_eq!(none.subtitle_downloads().unwrap().daily_limit_per_user, 0);
        assert_eq!(none.subtitle_downloads().unwrap().daily_limit_per_peer, 40);
        assert_eq!(
            account.api_url.as_str(),
            "https://api.opensubtitles.com/api/v1"
        );
        assert_eq!(account.username, None);
        let config = opensubtitles(r#"username = "u"; password = "p4ss";"#).unwrap();
        let shown = format!("{:?} {:?}", config.opensubtitles, sites(&config));
        assert!(
            shown.contains("\"u\"") && !shown.contains("p4ss"),
            "{shown}"
        );
    }

    /// The languages are a site's own; the limit is every site's.
    #[test]
    fn opensubtitles_takes_languages_and_the_limit_is_every_site_s() {
        let config = ServerConfig::parse(
            r#"schema = "spoolframe.server.v1";
               opensubtitles { languages = ["en", "pt-BR"]; }
               subtitles { daily_limit_per_user = 5; }"#,
        )
        .unwrap();
        assert_eq!(sites(&config)[0].languages, ["en", "pt-br"]);
        assert_eq!(config.subtitle_downloads().unwrap().daily_limit_per_user, 5);
    }

    #[test]
    fn opensubtitles_refuses_half_an_account_or_a_bad_language() {
        for block in [
            r#"username = "u";"#,
            r#"languages = [];"#,
            r#"languages = ["english!"];"#,
            r#"api_url = "ftp://example";"#,
        ] {
            assert!(
                matches!(opensubtitles(block), Err(ConfigError::Invalid { .. })),
                "{block}"
            );
        }
    }

    /// SubDL beside OpenSubtitles: SubDL on with its key, each asked in
    /// the order `subtitles.sources` gives, or left out by it; SubDL's
    /// key never printed.
    #[test]
    fn the_admin_picks_the_subtitle_sites_and_their_order() {
        use crate::subtitle_site::Source;
        let parse = |rest: &str| {
            ServerConfig::parse(&format!(
                r#"schema = "spoolframe.server.v1";
                   subdl {{ api_key = "subdl-k3y"; languages = ["en", "pt-BR"]; }}
                   {rest}"#
            ))
        };
        let order =
            |config: &ServerConfig| sites(config).iter().map(|s| s.source()).collect::<Vec<_>>();
        let both = parse("").unwrap();
        assert_eq!(order(&both), [Source::OpenSubtitles, Source::SubDl]);
        let crate::subtitle_downloads::SiteSettings {
            languages,
            kind: crate::subtitle_downloads::SiteKind::SubDl(account),
        } = &sites(&both)[1]
        else {
            panic!()
        };
        assert_eq!(account.api_url.as_str(), "https://api.subdl.com/api/v1");
        assert_eq!(account.download_url.as_str(), "https://dl.subdl.com/");
        assert_eq!(languages, &["en", "pt-br"]);
        let shown = format!("{:?} {:?}", both.subdl, sites(&both));
        assert!(!shown.contains("subdl-k3y"), "{shown}");

        let subdl_first =
            parse(r#"subtitles { sources = ["subdl", "opensubtitles", "subdl"]; }"#).unwrap();
        assert_eq!(order(&subdl_first), [Source::SubDl, Source::OpenSubtitles]);
        let subdl_only = parse(r#"subtitles { sources = ["subdl"]; }"#).unwrap();
        assert_eq!(order(&subdl_only), [Source::SubDl]);
        let none = parse(r#"subtitles { sources = []; }"#).unwrap();
        assert!(order(&none).is_empty());
        // SubDL named but without its key: off.
        let keyless = ServerConfig::parse(
            r#"schema = "spoolframe.server.v1";
               subtitles { sources = ["subdl", "opensubtitles"]; }"#,
        )
        .unwrap();
        assert_eq!(order(&keyless), [Source::OpenSubtitles]);
        assert!(matches!(
            parse(r#"subtitles { sources = ["podnapisi"]; }"#),
            Err(ConfigError::Invalid {
                key: "subtitles.sources",
                ..
            })
        ));
        assert!(matches!(
            ServerConfig::parse(
                r#"schema = "spoolframe.server.v1";
                   subdl { api_key = "k"; download_url = "file:///etc"; }"#
            ),
            Err(ConfigError::Invalid {
                key: "subdl.download_url",
                ..
            })
        ));
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
        assert_eq!(options.relays, []);
        assert_eq!(options.port, None);
        assert!(options.mdns);
    }

    #[test]
    fn transcoding_defaults_to_auto_on_the_first_render_node() {
        let config = plex(r#"plex { url = "http://nas:32400"; }"#).unwrap();
        assert_eq!(
            config.transcode.settings(&config.experimental).unwrap(),
            TranscodeSettings::default()
        );
        assert_eq!(config.media.ffmpeg(), PathBuf::from("ffmpeg"));
    }

    #[test]
    fn the_media_tools_are_confined_when_they_can_be_by_default() {
        let config = plex(r#"plex { url = "http://nas:32400"; }"#).unwrap();
        assert_eq!(config.media.sandbox().unwrap(), SandboxMode::Auto);
        let config =
            plex(r#"plex { url = "http://nas:32400"; } media { sandbox = "required"; }"#).unwrap();
        assert_eq!(config.media.sandbox().unwrap(), SandboxMode::Required);
        let config =
            plex(r#"plex { url = "http://nas:32400"; } media { sandbox = "on"; }"#).unwrap();
        assert!(config.media.sandbox().is_err());
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
    fn subtitle_syncs_run_one_at_a_time_unless_set() {
        let config = plex(r#"plex { url = "http://nas:32400"; }"#).unwrap();
        let sync = config
            .subtitles
            .sync_settings(Path::new("/var/lib/sf"))
            .unwrap();
        assert_eq!(sync.at_once, 1);
        assert_eq!(sync.dir, PathBuf::from("/var/lib/sf/subtitles"));
        let config =
            plex(r#"plex { url = "http://nas:32400"; } subtitles { sync_at_once = 2; }"#).unwrap();
        assert_eq!(
            config
                .subtitles
                .sync_settings(Path::new("/s"))
                .unwrap()
                .at_once,
            2
        );
        let config =
            plex(r#"plex { url = "http://nas:32400"; } subtitles { sync_at_once = 0; }"#).unwrap();
        assert!(matches!(
            config.subtitles.sync_settings(Path::new("/s")),
            Err(ConfigError::Invalid {
                key: "subtitles.sync_at_once",
                ..
            })
        ));
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
                memory_limit_bytes: 64 << 20,
                workers: crate::previews::workers::default_workers().count,
            }
        );
        let config = plex(
            r#"plex { url = "http://nas:32400"; }
               previews { when = "on_play"; interval_secs = 5; cache_mb = 100; memory_mb = 16;
                          workers = 3; }"#,
        )
        .unwrap();
        assert_eq!(
            config.previews.settings().unwrap(),
            PreviewSettings {
                when: When::OnPlay,
                interval: Duration::from_secs(5),
                cache_limit_bytes: 100 << 20,
                memory_limit_bytes: 16 << 20,
                workers: 3,
            }
        );
    }

    #[test]
    fn a_previews_setting_that_cannot_work_is_refused() {
        for (text, key) in [
            (r#"previews { when = "sometimes"; }"#, "previews.when"),
            ("previews { interval_secs = 0; }", "previews.interval_secs"),
            ("previews { workers = 0; }", "previews.workers"),
        ] {
            let text = format!(r#"plex {{ url = "http://nas:32400"; }} {text}"#);
            assert!(
                matches!(plex(&text), Err(ConfigError::Invalid { key: k, .. }) if k == key),
                "{text}"
            );
        }
    }

    #[test]
    fn metrics_are_off_and_for_loopback_unless_set() {
        let config = plex("").unwrap();
        assert!(!config.admin.metrics());
        let from = config.admin.metrics_from().unwrap();
        assert!(from.contains("127.0.0.1".parse().unwrap()));
        assert!(!from.contains("192.168.1.5".parse().unwrap()));
        let on = plex(r#"admin { metrics = true; metrics_from = ["192.168.1.0/24"]; }"#).unwrap();
        assert!(on.admin.metrics());
        assert!(
            on.admin
                .metrics_from()
                .unwrap()
                .contains("192.168.1.5".parse().unwrap())
        );
        assert!(matches!(
            plex(r#"admin { metrics_from = "localhost"; }"#),
            Err(ConfigError::Invalid {
                key: "admin.metrics_from",
                ..
            })
        ));
    }

    #[test]
    fn nothing_is_exported_without_an_otlp_endpoint() {
        assert_eq!(plex("").unwrap().telemetry.otlp.settings().unwrap(), None);
        let on = plex(
            r#"telemetry { otlp { endpoint = "http://alloy:4318"; protocol = "http";
               headers { x-scope-orgid = "home"; authorization = "Bearer k"; } logs = false; } }"#,
        )
        .unwrap()
        .telemetry
        .otlp
        .settings()
        .unwrap()
        .unwrap();
        assert_eq!(on.protocol, crate::telemetry::Protocol::Http);
        // By name: a table has no order.
        assert_eq!(
            on.headers,
            [
                ("authorization".to_owned(), "Bearer k".to_owned()),
                ("x-scope-orgid".to_owned(), "home".to_owned()),
            ]
        );
        assert!(on.traces && on.metrics && !on.logs);
        let grpc = plex(r#"telemetry { otlp { endpoint = "http://alloy:4317"; } }"#)
            .unwrap()
            .telemetry
            .otlp
            .settings()
            .unwrap()
            .unwrap();
        assert_eq!(grpc.protocol, crate::telemetry::Protocol::Grpc);
    }

    #[test]
    fn an_otlp_setting_that_cannot_work_is_refused_without_its_secret() {
        for (text, key) in [
            (r#"endpoint = "alloy:4317";"#, "telemetry.otlp.endpoint"),
            (
                r#"endpoint = "http://a:4317"; protocol = "udp";"#,
                "telemetry.otlp.protocol",
            ),
            // The name=value string it was before it took a table.
            (
                r#"endpoint = "http://a:4317"; headers = "authorization=s3cr3t";"#,
                "telemetry.otlp.headers",
            ),
            (
                r#"endpoint = "http://a:4317"; headers { "bad name" = "s3cr3t"; }"#,
                "telemetry.otlp.headers",
            ),
            (
                r#"endpoint = "http://a:4317"; headers { authorization = "s3cr3t\n"; }"#,
                "telemetry.otlp.headers",
            ),
        ] {
            let err = plex(&format!("telemetry {{ otlp {{ {text} }} }}")).unwrap_err();
            assert!(
                matches!(err, ConfigError::Invalid { key: k, .. } if k == key),
                "{text}: {err:?}"
            );
            assert!(!err.to_string().contains("s3cr3t"), "{err}");
        }
    }

    #[test]
    fn five_thousand_log_lines_are_kept_unless_set() {
        assert_eq!(plex("").unwrap().logs.buffer_lines().unwrap(), 5000);
        let set = plex("logs { buffer_lines = 200; }").unwrap();
        assert_eq!(set.logs.buffer_lines().unwrap(), 200);
        for bad in ["0", "100001"] {
            assert!(matches!(
                plex(&format!("logs {{ buffer_lines = {bad}; }}")),
                Err(ConfigError::Invalid {
                    key: "logs.buffer_lines",
                    ..
                })
            ));
        }
    }

    /// A scratch space ffmpeg's subtitle filter cannot read is refused at
    /// load, its key named, here or under the state directory (Review
    /// 2026-10-11 N5.1).
    #[test]
    fn a_scratch_dir_the_subtitle_filter_misreads_is_refused() {
        assert!(matches!(
            plex(
                r#"plex { url = "http://nas:32400"; } transcode { scratch_dir = "/srv/scratch:1"; }"#
            ),
            Err(ConfigError::Invalid {
                key: "transcode.scratch_dir",
                ..
            })
        ));
        let config = plex(r#"plex { url = "http://nas:32400"; }"#).unwrap();
        assert!(matches!(
            config.transcode.limits(Path::new("/var/lib/[sf]")),
            Err(ConfigError::Invalid {
                key: "transcode.scratch_dir",
                ..
            })
        ));
    }

    #[test]
    fn job_limits_default_under_the_state_dir() {
        let config = plex(r#"plex { url = "http://nas:32400"; }"#).unwrap();
        let limits = config.transcode.limits(Path::new("/var/lib/sf")).unwrap();
        assert_eq!(limits.budget, crate::encode::Units::jobs(2));
        assert_eq!(limits.max_remux, 32);
        assert_eq!(limits.max_gpu_jobs, 3);
        assert_eq!(
            limits.scratch_dir,
            PathBuf::from("/var/lib/sf/cache/transcode")
        );
        assert_eq!(limits.scratch_limit_bytes, 20 << 30);
        assert_eq!(limits.read_ahead, Duration::from_secs(60));
        assert!(!limits.gpu_filters);
        assert_eq!(limits.tone_map, ToneMapSetting::Auto);
    }

    /// PX-49: `http_playback` is off unless `listen` is set; `listen` is
    /// an IP address and a port, and `address` a host alone.
    #[test]
    fn the_http_playback_port_is_off_unless_set() {
        let settings = |section: &str| plex(section).and_then(|c| c.http_playback.settings());
        assert_eq!(
            settings("").unwrap(),
            crate::http_playback::Settings::default()
        );
        assert!(settings("").unwrap().listen.is_none());
        let on =
            settings(r#"http_playback { listen = "0.0.0.0:7422"; address = "media.home.arpa"; }"#)
                .unwrap();
        assert_eq!(on.listen, Some("0.0.0.0:7422".parse().unwrap()));
        assert_eq!(on.address.as_deref(), Some("media.home.arpa"));
        assert_eq!(on.idle, crate::http_playback::IDLE);
        let v6 =
            settings(r#"http_playback { listen = "[::]:7422"; address = "fd00::5"; }"#).unwrap();
        assert_eq!(v6.address.as_deref(), Some("[fd00::5]"));
        for bad in [
            r#"http_playback { listen = "media:7422"; }"#,
            r#"http_playback { listen = "0.0.0.0"; }"#,
        ] {
            assert!(
                matches!(
                    plex(bad),
                    Err(ConfigError::Invalid {
                        key: "http_playback.listen",
                        ..
                    })
                ),
                "{bad}"
            );
        }
        for bad in [
            r#"http_playback { listen = "0.0.0.0:7422"; address = "http://media"; }"#,
            r#"http_playback { listen = "0.0.0.0:7422"; address = "media:80"; }"#,
            r#"http_playback { listen = "0.0.0.0:7422"; address = "media/x"; }"#,
            r#"http_playback { listen = "0.0.0.0:7422"; address = ""; }"#,
        ] {
            assert!(
                matches!(
                    plex(bad),
                    Err(ConfigError::Invalid {
                        key: "http_playback.address",
                        ..
                    })
                ),
                "{bad}"
            );
        }
    }

    /// PX-46: `network.uplink` and `network.lan_prefixes`. Left out, no
    /// budget, no stream limit, the owners' half; 0 is no limit; a
    /// percentage over 100 or a range that is none is refused.
    #[test]
    fn the_uplink_budget_is_read() {
        let uplink = |section: &str| {
            plex(&format!(
                r#"plex {{ url = "http://nas:32400"; }} network {{ {section} }}"#
            ))
            .and_then(|config| config.network.uplink())
        };
        assert_eq!(
            uplink("").unwrap(),
            crate::uplink::UplinkSettings::default()
        );
        let set = uplink(
            r#"lan_prefixes = ["10.10.0.0/24"]; wan_prefixes = ["10.10.0.7/32"]; uplink { budget_mbps = 20; max_streams = 3; owner_share_percent = 60; }"#,
        )
        .unwrap();
        assert_eq!(set.budget, Some(20_000_000));
        assert_eq!(set.max_streams, Some(3));
        assert_eq!(set.owner_share_percent, 60);
        assert!(set.lan_prefixes[0].contains("10.10.0.7".parse().unwrap()));
        assert_eq!(set.wan_prefixes[0].prefix(), 32);
        let none = uplink("uplink { budget_mbps = 0; max_streams = 0; }").unwrap();
        assert_eq!((none.budget, none.max_streams), (None, None));
        assert!(matches!(
            uplink("uplink { owner_share_percent = 101; }"),
            Err(ConfigError::Invalid {
                key: "network.uplink.owner_share_percent",
                ..
            })
        ));
        assert!(matches!(
            uplink(r#"lan_prefixes = ["home"];"#),
            Err(ConfigError::Invalid {
                key: "network.lan_prefixes",
                ..
            })
        ));
        assert!(matches!(
            uplink(r#"wan_prefixes = ["away"];"#),
            Err(ConfigError::Invalid {
                key: "network.wan_prefixes",
                ..
            })
        ));
    }

    /// PGPU-06: `transcode.video_codecs` orders the GPU's codecs; AV1,
    /// HEVC, H.264 if left out; a word it does not know, or one twice, is
    /// refused at load; `[]` is no GPU codec.
    #[test]
    fn the_gpu_s_codecs_are_ordered() {
        let codecs = |section: &str| {
            plex(&format!(
                r#"plex {{ url = "http://nas:32400"; }} transcode {{ {section} }}"#
            ))
            .map(|config| {
                config
                    .transcode
                    .limits(Path::new("/"))
                    .unwrap()
                    .video_codecs
            })
        };
        assert_eq!(
            codecs("").unwrap(),
            [VideoCodec::Av1, VideoCodec::Hevc, VideoCodec::H264]
        );
        assert_eq!(
            codecs(r#"video_codecs = ["hevc", "h264"];"#).unwrap(),
            [VideoCodec::Hevc, VideoCodec::H264]
        );
        assert_eq!(codecs("video_codecs = [];").unwrap(), []);
        let unknown = codecs(r#"video_codecs = ["vp9"];"#);
        assert!(
            matches!(
                &unknown,
                Err(ConfigError::Invalid { key: "transcode.video_codecs", message })
                    if message.contains("\"av1\"")
            ),
            "{unknown:?}"
        );
        let twice = codecs(r#"video_codecs = ["hevc", "av1", "hevc"];"#);
        assert!(
            matches!(
                &twice,
                Err(ConfigError::Invalid { key: "transcode.video_codecs", message })
                    if message.contains("twice")
            ),
            "{twice:?}"
        );
    }

    /// PGPU-04: `transcode.tone_map` is one of four words, for the jobs
    /// (live) and for detection's warning alike.
    #[test]
    fn the_tone_map_is_chosen_or_forced() {
        for (word, setting) in [
            ("auto", ToneMapSetting::Auto),
            ("cpu", ToneMapSetting::Cpu),
            ("gpu", ToneMapSetting::Gpu),
            ("libplacebo", ToneMapSetting::Libplacebo),
        ] {
            let config = plex(&format!(
                r#"plex {{ url = "http://nas:32400"; }} transcode {{ tone_map = "{word}"; }}"#
            ))
            .unwrap();
            assert_eq!(
                config.transcode.limits(Path::new("/")).unwrap().tone_map,
                setting
            );
            assert_eq!(
                config
                    .transcode
                    .settings(&config.experimental)
                    .unwrap()
                    .tone_map,
                setting
            );
        }
        let refused =
            plex(r#"plex { url = "http://nas:32400"; } transcode { tone_map = "opencl"; }"#);
        assert!(
            matches!(
                &refused,
                Err(ConfigError::Invalid { key: "transcode.tone_map", message })
                    if message.contains("\"libplacebo\"")
            ),
            "{refused:?}"
        );
    }

    /// PGPU-04: the experimental block, off if left out; on, it reaches
    /// detection and is named for the start's log.
    #[test]
    fn experimental_switches_are_off_unless_set() {
        let config = plex(r#"plex { url = "http://nas:32400"; }"#).unwrap();
        assert!(!config.experimental.libplacebo_shared_on_amd());
        assert!(config.experimental.on().is_empty());
        assert!(
            !config
                .transcode
                .settings(&config.experimental)
                .unwrap()
                .libplacebo_shared_on_amd
        );
        let on = plex(
            r#"plex { url = "http://nas:32400"; } experimental { libplacebo_shared_on_amd = true; }"#,
        )
        .unwrap();
        assert!(
            on.transcode
                .settings(&on.experimental)
                .unwrap()
                .libplacebo_shared_on_amd
        );
        assert_eq!(
            on.experimental.on(),
            ["experimental.libplacebo_shared_on_amd"]
        );
        assert!(
            plex(r#"plex { url = "http://nas:32400"; } experimental { shared = true; }"#).is_err()
        );
    }

    /// Keyframes read ahead: off if left out, and named for the start's
    /// log when on.
    #[test]
    fn index_ahead_is_off_unless_set() {
        let config = plex(r#"plex { url = "http://nas:32400"; }"#).unwrap();
        assert!(!config.experimental.index_ahead());
        let on = plex(r#"plex { url = "http://nas:32400"; } experimental { index_ahead = true; }"#)
            .unwrap();
        assert!(on.experimental.index_ahead());
        assert_eq!(on.experimental.on(), ["experimental.index_ahead"]);
    }

    /// P8-01: federation is off if left out, and named for the start's
    /// log when on.
    /// P8-02: the replica's settings, their defaults and their floor.
    #[test]
    fn the_federation_cache_is_hourly_and_two_gigabytes_unless_set() {
        let config = plex(r#"plex { url = "http://nas:32400"; }"#).unwrap();
        assert_eq!(config.federation.sync_every(), Duration::from_secs(3600));
        assert_eq!(config.federation.cache_max_bytes(), 2 << 30);
        assert_eq!(config.federation.cache_total_bytes(), 8 << 30);
        let set = plex(
            r#"plex { url = "http://nas:32400"; }
               federation { sync_interval_secs = 5; cache_max_mb = 10; cache_total_mb = 30; }"#,
        )
        .unwrap();
        assert_eq!(set.federation.sync_every(), Duration::from_secs(60));
        assert_eq!(set.federation.cache_max_bytes(), 10 << 20);
        assert_eq!(set.federation.cache_total_bytes(), 30 << 20);
    }

    /// P8-04 (2026-10-23): a pair's limits are today's values unless set,
    /// and none goes under its floor.
    #[test]
    fn a_pair_s_limits_are_today_s_unless_set_and_never_under_their_floor() {
        let config = plex(r#"plex { url = "http://nas:32400"; }"#).unwrap();
        assert_eq!(
            config.federation.limits(),
            crate::federation::Limits::default()
        );
        let raised = plex(
            r#"plex { url = "http://nas:32400"; }
               federation { sync_max_items = 300000; max_cached_items = 1000000;
                            tokens_per_user = 40; max_streams = 64; }"#,
        )
        .unwrap()
        .federation
        .limits();
        assert_eq!(raised.sync.items, 300_000);
        assert_eq!(raised.sync.origin_items, 1_000_000);
        assert_eq!(raised.tokens.per_user, 40);
        assert_eq!(raised.max_streams, 64);
        let floored = plex(
            r#"plex { url = "http://nas:32400"; }
               federation { sync_max_pages = 0; sync_max_items = 1; max_cached_items = 1;
                            sync_max_metadata_mb = 0; max_cached_metadata_mb = 0;
                            walk_reuse_secs = 0; max_streams = 0; artwork_burst = 0;
                            assertion_burst = 0; tokens_per_user = 0; tokens_per_server = 0;
                            download_tokens_per_user = 0; download_tokens_per_server = 0;
                            subtitle_burst = 0; play_sessions_per_server = 0; }"#,
        )
        .unwrap()
        .federation
        .limits();
        assert_eq!(floored.sync.pages, 10);
        assert_eq!(floored.sync.items, 1_000);
        assert_eq!(floored.sync.origin_items, 1_000);
        assert_eq!(floored.sync.bytes, 16 << 20);
        assert_eq!(floored.sync.origin_bytes, 64 << 20);
        assert_eq!(floored.walk_reuse, Duration::from_secs(10));
        assert_eq!(floored.max_streams, 4);
        assert_eq!(floored.artwork_burst, 10);
        assert_eq!(floored.assertion_burst, 5);
        assert_eq!(floored.tokens.per_user, 2);
        assert_eq!(floored.tokens.per_peer, 10);
        assert_eq!(floored.download_tokens.per_user, 10);
        assert_eq!(floored.download_tokens.per_peer, 50);
        assert_eq!(floored.subtitle_burst, 2);
        assert_eq!(floored.play_sessions, 10);
    }

    /// P8-04: a limit typed too large is held to its ceiling, not used as
    /// it is: a semaphore of more permits than it takes panics, a time
    /// added to now overflows, a count past i64 does not bind.
    #[test]
    fn a_pair_s_limits_never_go_over_their_ceiling() {
        let huge = i64::MAX;
        let config = plex(&format!(
            r#"plex {{ url = "http://nas:32400"; }}
               federation {{ sync_interval_secs = {huge}; sync_max_pages = {huge};
                            sync_max_items = {huge}; max_cached_items = {huge};
                            sync_max_metadata_mb = {huge}; max_cached_metadata_mb = {huge};
                            walk_reuse_secs = {huge}; max_streams = {huge};
                            artwork_burst = {huge}; assertion_burst = {huge};
                            tokens_per_user = {huge}; tokens_per_server = {huge};
                            download_tokens_per_user = {huge};
                            download_tokens_per_server = {huge}; subtitle_burst = {huge};
                            play_sessions_per_server = {huge}; }}"#
        ))
        .unwrap()
        .federation;
        assert_eq!(config.sync_every(), Duration::from_secs(7 * 86_400));
        let capped = config.limits();
        assert_eq!(capped.sync.pages, 1_000_000);
        assert_eq!(capped.sync.items, 100_000_000);
        assert_eq!(capped.sync.origin_items, 100_000_000);
        assert_eq!(capped.sync.bytes, 65_536 << 20);
        assert_eq!(capped.sync.origin_bytes, 262_144 << 20);
        assert_eq!(capped.walk_reuse, Duration::from_secs(86_400));
        assert_eq!(capped.max_streams, 1_024);
        assert_eq!(capped.artwork_burst, 100_000);
        assert_eq!(capped.assertion_burst, 10_000);
        assert_eq!(capped.tokens.per_user, 10_000);
        assert_eq!(capped.tokens.per_peer, 1_000_000);
        assert_eq!(capped.download_tokens.per_user, 100_000);
        assert_eq!(capped.download_tokens.per_peer, 10_000_000);
        assert_eq!(capped.subtitle_burst, 10_000);
        assert_eq!(capped.play_sessions, 10_000);
    }

    #[test]
    fn federation_is_off_unless_set() {
        let config = plex(r#"plex { url = "http://nas:32400"; }"#).unwrap();
        assert!(!config.experimental.federation());
        let on = plex(r#"plex { url = "http://nas:32400"; } experimental { federation = true; }"#)
            .unwrap();
        assert!(on.experimental.federation());
        assert_eq!(on.experimental.on(), ["experimental.federation"]);
    }

    /// Review 2026-10-11 M2: how long a copied video's HLS segments are,
    /// at least: 2 s if left out, 1 to 10 s.
    #[test]
    fn a_copied_video_s_segment_length_is_one_to_ten_seconds() {
        let secs = |value: Option<u64>| {
            let section = value.map_or(String::new(), |v| format!("copy_segment_secs = {v};"));
            plex(&format!(
                r#"plex {{ url = "http://nas:32400"; }} transcode {{ {section} }}"#
            ))
            .and_then(|config| config.transcode.limits(Path::new("/")))
            .map(|limits| limits.copy_segment_secs)
        };
        assert_eq!(secs(None).unwrap(), 2);
        assert_eq!(secs(Some(1)).unwrap(), 1);
        assert_eq!(secs(Some(10)).unwrap(), 10);
        for out in [0, 11] {
            match secs(Some(out)) {
                Err(ConfigError::Invalid { key, message }) => {
                    assert_eq!(key, "transcode.copy_segment_secs");
                    assert!(message.contains("1 to 10"), "{message}");
                }
                other => panic!("{out} s: {other:?}"),
            }
        }
    }

    #[test]
    fn gpu_filters_are_turned_on() {
        let config =
            plex(r#"plex { url = "http://nas:32400"; } transcode { gpu_filters = true; }"#)
                .unwrap();
        assert!(config.transcode.limits(Path::new("/")).unwrap().gpu_filters);
    }

    #[test]
    fn zero_jobs_is_refused() {
        for key in ["max_jobs", "max_remux", "max_gpu_jobs"] {
            let text =
                format!(r#"plex {{ url = "http://nas:32400"; }} transcode {{ {key} = 0; }}"#);
            let err = plex(&text).unwrap_err();
            assert!(
                matches!(&err, ConfigError::Invalid { key: k, .. } if *k == format!("transcode.{key}")),
                "{text}: {err:?}"
            );
        }
    }

    #[test]
    fn the_remux_and_gpu_caps_are_read() {
        let config = plex(
            r#"plex { url = "http://nas:32400"; } transcode { max_jobs = 3; max_remux = 8; max_gpu_jobs = 1; }"#,
        )
        .unwrap();
        let limits = config.transcode.limits(Path::new("/")).unwrap();
        assert_eq!(limits.budget, crate::encode::Units::jobs(3));
        assert_eq!((limits.max_remux, limits.max_gpu_jobs), (8, 1));
    }

    /// PGPU-05: `transcode.gpus` as written, each entry an API and a
    /// pick, a device, or neither (every card).
    #[test]
    fn gpus_are_read_in_order() {
        let config = plex(
            r#"plex { url = "http://nas:32400"; } transcode { gpus = [
                { api = "nvenc"; pick = "all"; },
                { api = "vaapi"; pick = "dgpu"; max_jobs = 1; },
                { api = "vaapi"; device = "/dev/dri/by-path/pci-0000:c8:00.0-render"; },
                { api = "nvenc"; device = 1; },
                { api = "qsv"; },
            ]; }"#,
        )
        .unwrap();
        assert_eq!(
            config.transcode.gpus().unwrap(),
            [
                GpuEntry::all(Hardware::Nvenc),
                GpuEntry {
                    api: Hardware::Vaapi,
                    which: Which::Pick(Pick::Dgpu),
                    max_jobs: Some(1),
                },
                GpuEntry {
                    api: Hardware::Vaapi,
                    which: Which::Device("/dev/dri/by-path/pci-0000:c8:00.0-render".into()),
                    max_jobs: None,
                },
                GpuEntry {
                    api: Hardware::Nvenc,
                    which: Which::Device("1".into()),
                    max_jobs: None,
                },
                GpuEntry::all(Hardware::Qsv),
            ]
        );
        // An empty list: no GPU.
        let none = plex(r#"plex { url = "http://nas:32400"; } transcode { gpus = []; }"#).unwrap();
        assert!(none.transcode.gpus().unwrap().is_empty());
    }

    /// The NixOS module writes each entry as JSON, which UCL reads too.
    #[test]
    fn gpus_written_as_json_are_read() {
        let config = plex(
            r#"plex { url = "http://nas:32400"; } transcode { gpus = [{"api":"vaapi","pick":"dgpu"},{"api":"nvenc","device":"1","max_jobs":2}]; }"#,
        )
        .unwrap();
        assert_eq!(
            config.transcode.gpus().unwrap(),
            [
                GpuEntry {
                    api: Hardware::Vaapi,
                    which: Which::Pick(Pick::Dgpu),
                    max_jobs: None,
                },
                GpuEntry {
                    api: Hardware::Nvenc,
                    which: Which::Device("1".into()),
                    max_jobs: Some(2),
                },
            ]
        );
    }

    #[test]
    fn gpus_default_to_every_card_of_every_api() {
        let config = plex(r#"plex { url = "http://nas:32400"; }"#).unwrap();
        assert_eq!(config.transcode.gpus().unwrap(), GpuEntry::defaults());
    }

    #[test]
    fn a_bad_gpu_entry_is_refused_with_its_number() {
        for (entry, says) in [
            (r#"{ api = "cuda"; }"#, "api \"cuda\""),
            (
                r#"{ api = "vaapi"; pick = "dgpu"; device = "/dev/dri/renderD128"; }"#,
                "pick or device, not both",
            ),
            (
                r#"{ api = "vaapi"; pick = "discrete"; }"#,
                "pick \"discrete\"",
            ),
            (
                r#"{ api = "nvenc"; device = "/dev/nvidia0"; }"#,
                "CUDA index",
            ),
            (
                r#"{ api = "vaapi"; device = "renderD128"; }"#,
                "render node",
            ),
            (r#"{ api = "vaapi"; max_jobs = 0; }"#, "max_jobs = 0"),
        ] {
            let config = plex(&format!(
                r#"plex {{ url = "http://nas:32400"; }} transcode {{ gpus = [ {{ api = "nvenc"; }}, {entry} ]; }}"#
            ));
            match &config {
                Err(ConfigError::Invalid {
                    key: "transcode.gpus",
                    message,
                }) => {
                    assert!(message.starts_with("entry 2: "), "{entry}: {message}");
                    assert!(message.contains(says), "{entry}: {message}");
                }
                other => panic!("{entry}: {other:?}"),
            }
        }
        // An unknown key is refused as everywhere.
        assert!(
            plex(
                r#"plex { url = "http://nas:32400"; } transcode { gpus = [ { api = "vaapi"; node = "x"; } ]; }"#
            )
            .is_err()
        );
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

    /// An interval too short is refused with its key, not raised to 10 s
    /// without a word (review 2026-10-11 L33).
    #[test]
    fn a_watch_sync_interval_under_ten_seconds_is_refused() {
        let text = |secs: u64| {
            format!(
                r#"plex {{ url = "http://nas:32400"; watch_sync {{ interval_secs = {secs}; }} }}"#
            )
        };
        assert!(matches!(
            plex(&text(1)),
            Err(ConfigError::Invalid {
                key: "plex.watch_sync.interval_secs",
                ..
            })
        ));
        let ten = plex(&text(10)).unwrap();
        assert_eq!(
            ten.plex.unwrap().watch_sync.unwrap().interval(),
            Duration::from_secs(10)
        );
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
    fn ipv6_is_on_unless_set() {
        let config = plex(r#"plex { url = "http://nas:32400"; }"#).unwrap();
        assert!(config.network.endpoint_options().unwrap().ipv6);
    }

    #[test]
    fn ipv6_can_be_turned_off() {
        let config =
            plex(r#"plex { url = "http://nas:32400"; } network { ipv6 = false; }"#).unwrap();
        assert!(!config.network.endpoint_options().unwrap().ipv6);
    }

    #[test]
    fn a_fixed_port_is_read() {
        let config =
            plex(r#"plex { url = "http://nas:32400"; } network { port = 7420; }"#).unwrap();
        assert_eq!(config.network.endpoint_options().unwrap().port, Some(7420));
    }

    #[test]
    fn plex_discovery_searches_from_32415_unless_told_another_port() {
        let bare = plex(r#"plex { url = "http://nas:32400"; }"#).unwrap();
        assert_eq!(bare.network.plex_discovery_port(), 32415);
        let set =
            plex(r#"plex { url = "http://nas:32400"; } network { plex_discovery_port = 40000; }"#)
                .unwrap();
        assert_eq!(set.network.plex_discovery_port(), 40000);
        // 0 is any free port, as before it was fixed.
        let any =
            plex(r#"plex { url = "http://nas:32400"; } network { plex_discovery_port = 0; }"#)
                .unwrap();
        assert_eq!(any.network.plex_discovery_port(), 0);
        assert!(
            plex(r#"plex { url = "http://nas:32400"; } network { plex_discovery_port = 70000; }"#)
                .is_err()
        );
    }

    /// A host with a LAN interface, Docker's bridges and a veth pair, a
    /// libvirt bridge, and what the endpoint has on them.
    fn docker_host() -> (Vec<std::net::SocketAddr>, Vec<crate::announce::Interface>) {
        let interface = |name: &str, ip: &str| crate::announce::Interface {
            name: name.into(),
            addrs: vec![ip.parse().unwrap()],
        };
        let interfaces = vec![
            interface("enp5s0", "192.168.86.176"),
            interface("docker0", "172.17.0.1"),
            interface("br-0123abcd", "172.18.0.1"),
            interface("veth12ab", "172.19.0.1"),
            interface("virbr0", "192.168.122.1"),
        ];
        let direct = interfaces
            .iter()
            .map(|i| std::net::SocketAddr::new(i.addrs[0], 7420))
            .collect();
        (direct, interfaces)
    }

    #[test]
    fn docker_and_libvirt_bridges_are_not_told_by_default() {
        let config = plex(r#"plex { url = "http://nas:32400"; }"#).unwrap();
        let announce = config.network.endpoint_options().unwrap().announce;
        let (direct, interfaces) = docker_host();
        assert_eq!(
            announce.told(&direct, &[], &interfaces),
            ["192.168.86.176:7420"
                .parse::<std::net::SocketAddr>()
                .unwrap()]
        );
        assert!(announce.add.is_empty());
        assert!(announce.ignore.is_empty());
        assert!(announce.interfaces.is_empty());
    }

    /// The user can still have a bridge told, by naming it.
    #[test]
    fn a_bridge_named_in_interfaces_is_told() {
        let config = plex(
            r#"plex { url = "http://nas:32400"; }
            network { announce { interfaces = ["enp*", "docker0"]; } }"#,
        )
        .unwrap();
        let announce = config.network.endpoint_options().unwrap().announce;
        let (direct, interfaces) = docker_host();
        assert_eq!(
            announce.told(&direct, &[], &interfaces),
            [
                "192.168.86.176:7420"
                    .parse::<std::net::SocketAddr>()
                    .unwrap(),
                "172.17.0.1:7420".parse().unwrap(),
            ]
        );
    }

    #[test]
    fn what_is_told_is_read_from_network_announce() {
        let config = plex(
            r#"plex { url = "http://nas:32400"; }
            network { announce {
                add = ["203.0.114.7", "203.0.114.8:443"];
                ignore = ["172.17.0.0/16", "10.0.0.9"];
                ignore_interfaces = [];
                interfaces = ["enp*", "wg0"];
            } }"#,
        )
        .unwrap();
        let announce = config.network.endpoint_options().unwrap().announce;
        assert_eq!(
            announce.add,
            [
                "203.0.114.7".parse().unwrap(),
                "203.0.114.8:443".parse().unwrap()
            ]
        );
        assert_eq!(
            announce.ignore,
            [
                "172.17.0.0/16".parse().unwrap(),
                "10.0.0.9".parse().unwrap()
            ]
        );
        assert!(announce.ignore_interfaces.is_empty());
        assert_eq!(
            announce.interfaces,
            ["enp*".parse().unwrap(), "wg0".parse().unwrap()]
        );
    }

    #[test]
    fn a_bad_announce_setting_names_its_key() {
        for (setting, key) in [
            (r#"add = ["nas.local"]"#, "network.announce.add"),
            (r#"add = ["203.0.114.7:0"]"#, "network.announce.add"),
            (r#"ignore = ["172.17.0.0/33"]"#, "network.announce.ignore"),
            (r#"ignore = ["docker0"]"#, "network.announce.ignore"),
            (
                r#"ignore_interfaces = ["docker*", ""]"#,
                "network.announce.ignore_interfaces",
            ),
            (r#"interfaces = [" "]"#, "network.announce.interfaces"),
        ] {
            let err = plex(&format!(
                r#"plex {{ url = "http://nas:32400"; }} network {{ announce {{ {setting}; }} }}"#
            ))
            .unwrap_err();
            assert!(
                matches!(&err, ConfigError::Invalid { key: k, .. } if *k == key),
                "{setting}: {err:?}"
            );
        }
    }

    fn wan(section: &str) -> Result<Option<crate::wan::Wan>, ConfigError> {
        plex(&format!("network {{ {section} }}"))
            .map(|config| config.network.endpoint_options().unwrap().wan)
    }

    #[test]
    fn without_network_wan_no_public_address_is_advertised() {
        assert_eq!(wan("").unwrap(), None);
        assert_eq!(wan("port = 7420;").unwrap(), None);
    }

    #[test]
    fn wan_static_addresses_are_read_as_written() {
        let wan =
            wan(r#"wan { static = ["1.2.3.4:7420", "[2001:db9::1]:7420"]; mapped_port = false; }"#)
                .unwrap()
                .unwrap();
        assert_eq!(
            wan,
            crate::wan::Wan {
                statics: vec![
                    "1.2.3.4:7420".parse().unwrap(),
                    "[2001:db9::1]:7420".parse().unwrap()
                ],
                mapped_port: None,
            }
        );
    }

    #[test]
    fn wan_present_maps_the_network_port_and_looks_the_ip_up_by_default() {
        use crate::wan::{LookupVia, MappedPort, Mode, PublicIp};
        let wan = wan("port = 7420; wan { }").unwrap().unwrap();
        assert_eq!(wan.statics, []);
        assert_eq!(
            wan.mapped_port,
            Some(MappedPort {
                port: Some(7420),
                mode: Mode::Auto,
                public_ip: PublicIp::Lookup,
                lookup_via: Some(LookupVia::Wtfismyip),
            })
        );
    }

    #[test]
    fn a_manual_mapped_port_with_its_public_ip_is_read() {
        use crate::wan::{MappedPort, Mode, PublicIp};
        let wan = wan(
            r#"wan { mapped_port { port = 443; mode = "manual"; public_ip = "203.0.114.7";
                     lookup_via = false; } }"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            wan.mapped_port,
            Some(MappedPort {
                port: Some(443),
                mode: Mode::Manual,
                public_ip: PublicIp::Fixed("203.0.114.7".parse().unwrap()),
                lookup_via: None,
            })
        );
    }

    /// The `check_proxy` tradition: a shape that cannot work is refused at
    /// load, saying what to do instead.
    #[test]
    fn a_lookup_with_lookups_forbidden_is_refused_at_load() {
        let err = wan(r#"wan { mapped_port { lookup_via = false; } }"#).unwrap_err();
        let ConfigError::Invalid { key, message } = &err else {
            panic!("{err:?}");
        };
        assert_eq!(*key, "network.wan.mapped_port.public_ip");
        assert!(message.contains("lookup_via = false"), "{message}");
        assert!(message.contains("set public_ip"), "{message}");
        let explicit = wan(r#"wan { mapped_port { public_ip = "lookup"; lookup_via = false; } }"#);
        assert!(
            matches!(
                explicit,
                Err(ConfigError::Invalid {
                    key: "network.wan.mapped_port.public_ip",
                    ..
                })
            ),
            "{explicit:?}"
        );
    }

    #[test]
    fn a_private_loopback_or_link_local_static_address_is_refused() {
        for (entry, why) in [
            ("192.168.1.5:7420", "a private address"),
            ("10.0.0.2:7420", "a private address"),
            ("127.0.0.1:7420", "a loopback address"),
            ("[::1]:7420", "a loopback address"),
            ("169.254.0.9:7420", "a link-local address"),
            ("[fe80::1]:7420", "a link-local address"),
            ("[fd00::1]:7420", "a unique local address (fc00::/7)"),
            (
                "100.64.0.1:7420",
                "a carrier-grade NAT address (100.64.0.0/10)",
            ),
            ("100.100.100.100:7420", "(100.64.0.0/10)"),
            ("172.20.0.1:7420", "(172.16.0.0/12)"),
        ] {
            let err = wan(&format!(r#"wan {{ static = ["{entry}"]; }}"#)).unwrap_err();
            let ConfigError::Invalid { key, message } = &err else {
                panic!("{entry}: {err:?}");
            };
            assert_eq!(*key, "network.wan.static", "{entry}");
            assert!(message.contains(why), "{entry}: {message}");
        }
    }

    #[test]
    fn a_public_ip_that_is_not_public_is_refused_naming_its_range() {
        for (ip, range) in [
            ("100.64.0.1", "(100.64.0.0/10)"),
            ("192.168.1.1", "(192.168.0.0/16)"),
            ("127.0.0.1", "(127.0.0.0/8)"),
            ("fe80::1", "(fe80::/10)"),
        ] {
            let err = wan(&format!(
                r#"wan {{ mapped_port {{ public_ip = "{ip}"; }} }}"#
            ))
            .unwrap_err();
            let ConfigError::Invalid { key, message } = &err else {
                panic!("{ip}: {err:?}");
            };
            assert_eq!(*key, "network.wan.mapped_port.public_ip", "{ip}");
            assert!(message.contains(range), "{ip}: {message}");
        }
    }

    #[test]
    fn a_bad_wan_setting_names_its_key() {
        for (setting, key) in [
            (r#"static = ["nas.local:7420"]"#, "network.wan.static"),
            (r#"static = ["1.2.3.4"]"#, "network.wan.static"),
            (
                r#"mapped_port { mode = "upnp"; }"#,
                "network.wan.mapped_port.mode",
            ),
            (
                r#"mapped_port { public_ip = "nas.local"; }"#,
                "network.wan.mapped_port.public_ip",
            ),
            (
                r#"mapped_port { public_ip = "192.168.1.1"; }"#,
                "network.wan.mapped_port.public_ip",
            ),
            (
                r#"mapped_port { lookup_via = "ifconfig.me"; }"#,
                "network.wan.mapped_port.lookup_via",
            ),
            (
                r#"mapped_port { port = 0; }"#,
                "network.wan.mapped_port.port",
            ),
            (
                r#"mapped_port { mode = "manual"; public_ip = "1.2.3.4"; }"#,
                "network.wan.mapped_port.port",
            ),
        ] {
            let err = wan(&format!("wan {{ {setting}; }}")).unwrap_err();
            assert!(
                matches!(&err, ConfigError::Invalid { key: k, .. } if *k == key),
                "{setting}: {err:?}"
            );
        }
        for unknown in [
            "wan { statics = []; }",
            "wan { mapped_port { ip = \"1.2.3.4\"; } }",
        ] {
            assert!(wan(unknown).is_err(), "{unknown}");
        }
        assert!(wan("wan { mapped_port { lookup_via = true; } }").is_err());
    }

    /// `network.relays` set to `value`, as UCL.
    fn relays(value: &str) -> Result<Vec<Relay>, ConfigError> {
        plex(&format!(
            r#"plex {{ url = "http://nas:32400"; }} network {{ relays = {value}; }}"#
        ))?
        .network
        .endpoint_options()
        .map(|options| options.relays)
    }

    #[test]
    fn relays_are_a_list_of_default_and_urls_in_order() {
        let custom = Url::parse("https://relay.example.com").unwrap();
        assert_eq!(
            relays(r#"["https://relay.example.com", "default"]"#).unwrap(),
            [Relay::Custom(custom), Relay::Default]
        );
    }

    #[test]
    fn an_empty_relay_list_is_relays_off() {
        assert_eq!(relays("[]").unwrap(), []);
    }

    #[test]
    fn relays_written_as_one_comma_separated_string_are_refused_naming_the_list() {
        let err = relays(r#""https://relay.example.com, default""#).unwrap_err();
        let ConfigError::Invalid {
            key: "network.relays",
            message,
        } = err
        else {
            panic!("{err:?}");
        };
        assert!(message.contains("a list now"), "{message}");
    }

    #[test]
    fn a_relay_list_with_a_bad_or_repeated_entry_is_refused() {
        for list in [
            r#"["disabled"]"#,
            r#"[""]"#,
            r#"["relay.example.com"]"#,
            r#"["ftp://relay.example.com"]"#,
            r#"["default", "default"]"#,
            r#"["https://relay.example.com", "https://relay.example.com"]"#,
        ] {
            let err = relays(list).unwrap_err();
            assert!(
                matches!(
                    err,
                    ConfigError::Invalid {
                        key: "network.relays",
                        ..
                    }
                ),
                "{list}: {err:?}"
            );
        }
    }

    /// `network.own_relays` set to `value`, as UCL.
    fn own_relays(value: &str) -> Result<Vec<iroh::EndpointId>, ConfigError> {
        plex(&format!(
            r#"plex {{ url = "http://nas:32400"; }} network {{ own_relays = {value}; }}"#
        ))?
        .network
        .endpoint_options()
        .map(|options| options.own_relays)
    }

    #[test]
    fn own_relays_with_no_relays_told_are_warned_of() {
        let key = iroh::SecretKey::from_bytes(&[1; 32]).public();
        let network = |rest: &str| {
            plex(&format!(
                r#"plex {{ url = "http://nas:32400"; }} network {{ {rest} }}"#
            ))
            .unwrap()
            .network
        };
        let untold = network(&format!(r#"own_relays = ["{key}"];"#));
        assert!(untold.own_relays_untold().unwrap());
        let told = network(&format!(
            r#"own_relays = ["{key}"]; relays = ["https://relay.example.org"];"#
        ));
        assert!(!told.own_relays_untold().unwrap());
        assert!(!network("").own_relays_untold().unwrap());
    }

    #[test]
    fn own_relays_are_a_list_of_relay_keys() {
        let (a, b) = (
            iroh::SecretKey::from_bytes(&[1; 32]).public(),
            iroh::SecretKey::from_bytes(&[2; 32]).public(),
        );
        assert_eq!(own_relays(&format!(r#"["{a}", "{b}"]"#)).unwrap(), [a, b]);
        assert_eq!(own_relays("[]").unwrap(), []);
        for refused in [format!(r#""{a}, {b}""#), r#"["not a key"]"#.to_owned()] {
            let err = own_relays(&refused).unwrap_err();
            assert!(
                matches!(
                    err,
                    ConfigError::Invalid {
                        key: "network.own_relays",
                        ..
                    }
                ),
                "{refused}: {err:?}"
            );
        }
    }

    fn congestion(network: &str) -> Result<crate::network::CongestionControl, ConfigError> {
        plex(&format!(
            r#"plex {{ url = "http://nas:32400"; }} network {{ {network} }}"#
        ))?
        .network
        .endpoint_options()
        .map(|options| options.congestion)
    }

    #[test]
    fn congestion_control_is_cubic_unless_bbr_is_asked() {
        use crate::network::CongestionControl;
        assert_eq!(congestion("").unwrap(), CongestionControl::Cubic);
        assert_eq!(
            congestion(r#"congestion_control = "cubic";"#).unwrap(),
            CongestionControl::Cubic
        );
        assert_eq!(
            congestion(r#"congestion_control = "bbr";"#).unwrap(),
            CongestionControl::Bbr
        );
        assert!(matches!(
            congestion(r#"congestion_control = "reno";"#),
            Err(ConfigError::Invalid {
                key: "network.congestion_control",
                ..
            })
        ));
    }

    fn pkarr(value: &str) -> Result<Pkarr, ConfigError> {
        plex(&format!(
            r#"plex {{ url = "http://nas:32400"; }} network {{ pkarr = "{value}"; }}"#
        ))?
        .network
        .endpoint_options()
        .map(|options| options.pkarr)
    }

    #[test]
    fn pkarr_is_off_unless_set() {
        let config = plex(r#"plex { url = "http://nas:32400"; }"#).unwrap();
        assert_eq!(config.network.endpoint_options().unwrap().pkarr, Pkarr::Off);
        assert_eq!(pkarr("off").unwrap(), Pkarr::Off);
    }

    #[test]
    fn pkarr_is_n0_or_a_relay_of_one_s_own() {
        assert_eq!(pkarr("n0").unwrap(), Pkarr::N0);
        let url = Url::parse("https://dns.example.org/pkarr").unwrap();
        assert_eq!(
            pkarr("https://dns.example.org/pkarr").unwrap(),
            Pkarr::Custom(url)
        );
    }

    #[test]
    fn a_pkarr_setting_that_is_no_relay_is_refused() {
        for bad in ["on", "dns.example.org", "ftp://dns.example.org/pkarr"] {
            assert!(
                matches!(
                    pkarr(bad),
                    Err(ConfigError::Invalid {
                        key: "network.pkarr",
                        ..
                    })
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn the_old_relay_key_is_refused_with_a_pointer_to_relays() {
        let err = plex(r#"plex { url = "http://nas:32400"; } network { relay = "disabled"; }"#)
            .and_then(|config| config.network.endpoint_options())
            .unwrap_err();
        let ConfigError::Invalid { key, message } = err else {
            panic!("{err:?}");
        };
        assert_eq!(key, "network.relay");
        assert!(message.contains("network.relays"), "{message}");
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

    /// PX-11 finding 38, the second review's M-4: a same-host proxy whose
    /// callers the server cannot tell apart (PX-09 finding 29) does not
    /// load, and the refusal names the setting that fixes it.
    #[test]
    fn a_loopback_listener_behind_a_public_url_without_trusted_proxies_is_refused() {
        for admin in [
            r#"listen = "127.0.0.1:7421"; public_url = "https://media.example.org";"#,
            r#"listen = "[::1]:7421"; public_url = "https://media.example.org";"#,
        ] {
            let err = plex(&format!("admin {{ {admin} }}")).unwrap_err();
            assert!(
                matches!(
                    &err,
                    ConfigError::Invalid {
                        key: "admin.trusted_proxies",
                        message,
                    } if message.contains("admin.not_proxied")
                ),
                "{admin}: {err:?}"
            );
        }
    }

    /// The same shape loads once the proxy is named, or once the config
    /// says nothing proxies to the listener; and a listener that is not
    /// loopback, or has no public URL, is not that shape.
    #[test]
    fn a_loopback_listener_behind_a_public_url_loads_with_its_proxy_named_or_none_said() {
        for admin in [
            r#"listen = "127.0.0.1:7421"; public_url = "https://media.example.org"; trusted_proxies = "127.0.0.1";"#,
            r#"listen = "127.0.0.1:7421"; public_url = "http://localhost:7421"; not_proxied = true;"#,
            r#"listen = "127.0.0.1:7421";"#,
            r#"listen = "0.0.0.0:7421"; public_url = "https://media.example.org";"#,
            // The dev server's shape.
            r#"listen = "0.0.0.0:7431"; public_url = "http://localhost:7431"; insecure_cookies = true;"#,
        ] {
            let config = plex(&format!("admin {{ {admin} }}"));
            assert!(config.is_ok(), "{admin}: {config:?}");
        }
    }

    /// Finding 58 (the third review's G-9): a listener on every interface
    /// with a public URL and no `trusted_proxies` may have a proxy on this
    /// host in front. It loads, the dev server's shape among them, and is
    /// to be warned of; naming the proxy, saying there is none, or a
    /// listener on one address, is not.
    #[test]
    fn a_public_url_on_every_interface_without_trusted_proxies_may_be_proxied_unnamed() {
        for (admin, warned) in [
            (
                r#"listen = "0.0.0.0:7421"; public_url = "https://media.example.org";"#,
                true,
            ),
            (
                r#"listen = "[::]:7421"; public_url = "https://media.example.org";"#,
                true,
            ),
            // The server's default listener.
            (r#"public_url = "https://media.example.org";"#, true),
            // The dev server's shape.
            (
                r#"listen = "0.0.0.0:7431"; public_url = "http://localhost:7431"; insecure_cookies = true;"#,
                true,
            ),
            (
                r#"listen = "0.0.0.0:7421"; public_url = "https://media.example.org"; trusted_proxies = "127.0.0.1";"#,
                false,
            ),
            (
                r#"listen = "0.0.0.0:7421"; public_url = "https://media.example.org"; not_proxied = true;"#,
                false,
            ),
            (r#"listen = "0.0.0.0:7421";"#, false),
            (
                r#"listen = "192.168.1.20:7421"; public_url = "https://media.example.org";"#,
                false,
            ),
            (
                r#"listen = "127.0.0.1:7421"; public_url = "https://media.example.org"; trusted_proxies = "127.0.0.1";"#,
                false,
            ),
        ] {
            let config = plex(&format!("admin {{ {admin} }}"));
            let config = config.unwrap_or_else(|e| panic!("{admin}: {e:?}"));
            assert_eq!(
                config.admin.proxy_maybe_unnamed().unwrap(),
                warned,
                "{admin}"
            );
        }
    }

    #[test]
    fn trusted_proxies_are_none_by_default_and_a_list_when_set() {
        let config = plex(r#"plex { url = "http://nas:32400"; }"#).unwrap();
        assert_eq!(
            config.admin.trusted_proxies().unwrap(),
            crate::admin::TrustedProxies::default()
        );
        let config = plex(
            r#"plex { url = "http://nas:32400"; } admin { trusted_proxies = ["10.0.0.5", "172.17.0.0/16"]; }"#,
        )
        .unwrap();
        assert_eq!(
            config.admin.trusted_proxies().unwrap(),
            crate::admin::TrustedProxies::new(&["10.0.0.5", "172.17.0.0/16"]).unwrap()
        );
    }

    /// The comma-separated strings these lists were refused, saying how
    /// to write each as a list.
    #[test]
    fn a_list_setting_written_as_one_comma_separated_string_is_refused_naming_the_list() {
        for (config, key) in [
            (
                r#"admin { trusted_proxies = "10.0.0.5, 172.17.0.0/16"; }"#,
                "admin.trusted_proxies",
            ),
            (
                r#"admin { metrics_from = "127.0.0.0/8, ::1"; }"#,
                "admin.metrics_from",
            ),
        ] {
            let err =
                plex(&format!(r#"plex {{ url = "http://nas:32400"; }} {config}"#)).unwrap_err();
            let ConfigError::Invalid {
                key: found,
                message,
            } = &err
            else {
                panic!("{config}: {err:?}");
            };
            assert_eq!(*found, key, "{config}");
            assert!(message.contains("a list now"), "{config}: {message}");
        }
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

    fn oidc(providers: &str, admin: &str) -> Result<ServerConfig, ConfigError> {
        ServerConfig::parse(&format!(
            "schema = \"spoolframe.server.v1\"; admin {{ {admin} }} auth {{ oidc [ {providers} ] }}"
        ))
    }

    const PUBLIC: &str = r#"public_url = "https://media.example.org";"#;

    #[test]
    fn oidc_providers_are_read_in_order() {
        let config = oidc(
            r#"{ name = "Pocket ID"; issuer = "https://id.example.org";
                 client_id = "abc"; client_secret_file = "/run/secrets/oidc"; },
               { name = "Local"; issuer = "http://127.0.0.1:9000/o/"; client_id = "x"; }"#,
            PUBLIC,
        )
        .unwrap();
        let p = &config.auth.oidc;
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].name, "Pocket ID");
        assert_eq!(p[0].secret_name(), "oidc-pocket-id");
        assert_eq!(
            p[0].client_secret_file,
            Some(PathBuf::from("/run/secrets/oidc"))
        );
        assert_eq!(p[1].client_secret_file, None);
        assert_eq!(p[0].scopes(), ["openid", "profile", "email"]);
        // None by default.
        assert!(
            ServerConfig::parse(r#"schema = "spoolframe.server.v1";"#)
                .unwrap()
                .auth
                .oidc
                .is_empty()
        );
    }

    #[test]
    fn an_oidc_provider_that_cannot_work_is_refused() {
        let one = |p: &str| oidc(p, PUBLIC).unwrap_err().to_string();
        assert!(
            one(r#"{ name = "A"; issuer = "http://id.example.org"; client_id = "x"; }"#)
                .contains("not an https URL")
        );
        assert!(
            one(r#"{ name = "A"; issuer = "https://id.example.org/?x=1"; client_id = "x"; }"#)
                .contains("query")
        );
        assert!(
            one(r#"{ name = ""; issuer = "https://id.example.org"; client_id = "x"; }"#)
                .contains("1 to 40")
        );
        assert!(
            one(r#"{ name = "A"; issuer = "https://id.example.org"; client_id = " "; }"#)
                .contains("client_id")
        );
        assert!(
            one(
                r#"{ name = "Pocket ID"; issuer = "https://a.example.org"; client_id = "x"; },
                   { name = "pocket-id"; issuer = "https://b.example.org"; client_id = "x"; }"#
            )
            .contains("another provider has this name")
        );
        assert!(
            one(
                r#"{ name = "A"; issuer = "https://a.example.org"; client_id = "x"; },
                   { name = "B"; issuer = "https://a.example.org"; client_id = "y"; }"#
            )
            .contains("another provider's too")
        );
        assert!(
            one(r#"{ name = "A"; issuer = "https://a.example.org"; client_id = "x"; extra = 1; }"#)
                .contains("extra")
        );
        assert!(
            one(r#"{ name = "A"; issuer = "https://a.example.org"; client_id = "x"; scopes = ["openid email"]; }"#)
                .contains("not one scope")
        );
        let no_public_url = oidc(
            r#"{ name = "A"; issuer = "https://a.example.org"; client_id = "x"; }"#,
            "",
        )
        .unwrap_err();
        assert!(
            no_public_url.to_string().contains("set admin.public_url"),
            "{no_public_url}"
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

    const PLEX: &str = r#"plex { url = "http://nas:32400"; } "#;

    fn on(optimized: &str) -> Result<ServerConfig, ConfigError> {
        plex(&format!(
            "{PLEX}optimized {{ binge_ahead = true; {optimized} }}"
        ))
    }

    #[test]
    fn binge_ahead_is_off_when_left_out() {
        let default = plex(PLEX).unwrap();
        assert!(!default.optimized.binge_ahead());
        // Its other keys alone do not turn it on.
        let sized = plex(&format!("{PLEX}optimized {{ max_size_mb = 64; }}")).unwrap();
        assert!(!sized.optimized.binge_ahead());
    }

    #[test]
    fn binge_ahead_is_read() {
        assert!(on("").unwrap().optimized.binge_ahead());
    }

    /// The pool serves downloads and on-demand requests with binge-ahead
    /// off.
    #[test]
    fn the_optimized_pool_has_a_place_when_off() {
        let default = plex(PLEX).unwrap();
        let place = default.optimized.place(Path::new("/s")).unwrap();
        assert_eq!(place.dir, Path::new("/s/optimized"));
    }

    #[test]
    fn the_optimized_pool_is_under_the_state_dir_unless_set() {
        let settings = on("").unwrap().optimized.place(Path::new("/s")).unwrap();
        assert_eq!(settings.dir, Path::new("/s/optimized"));
        assert_eq!(settings.max_size_bytes, 100 << 30);
        let elsewhere = on(r#"dir = "/srv/copies";"#)
            .unwrap()
            .optimized
            .place(Path::new("/s"))
            .unwrap();
        assert_eq!(elsewhere.dir, Path::new("/srv/copies"));
    }

    #[test]
    fn the_optimized_pool_size_is_read_in_mib() {
        let config = on("max_size_mb = 64;").unwrap();
        let settings = config.optimized.place(Path::new("/s")).unwrap();
        assert_eq!(settings.max_size_bytes, 64 << 20);
    }

    /// The pool always runs: its size counts with binge-ahead off.
    #[test]
    fn an_optimized_pool_of_nothing_is_refused_on_or_off() {
        for config in [
            on("max_size_mb = 0;"),
            plex(&format!("{PLEX}optimized {{ max_size_mb = 0; }}")),
        ] {
            assert!(
                matches!(
                    config,
                    Err(ConfigError::Invalid {
                        key: "optimized.max_size_mb",
                        ..
                    })
                ),
                "{config:?}"
            );
        }
    }

    #[test]
    fn the_download_area_is_in_the_pool_s_dir_with_64_gib_unless_set() {
        let config = plex(PLEX).unwrap();
        let area = config
            .downloads
            .place(Path::new("/s"), Path::new("/s/optimized"))
            .unwrap();
        assert_eq!(
            (area.dir, area.max_size_bytes),
            (PathBuf::from("/s/optimized/downloads"), 64 << 30)
        );
    }

    #[test]
    fn the_download_area_s_dir_and_size_are_read() {
        let config = plex(&format!(
            r#"{PLEX}downloads {{ dir = "/d"; max_size_mb = 16384; }}"#
        ))
        .unwrap();
        let area = config
            .downloads
            .place(Path::new("/s"), Path::new("/s/optimized"))
            .unwrap();
        assert_eq!(
            (area.dir, area.max_size_bytes),
            (PathBuf::from("/d"), 16 << 30)
        );
    }

    #[test]
    fn a_download_area_of_nothing_is_refused() {
        let config = plex(&format!("{PLEX}downloads {{ max_size_mb = 0; }}"));
        assert!(
            matches!(
                config,
                Err(ConfigError::Invalid {
                    key: "downloads.max_size_mb",
                    ..
                })
            ),
            "{config:?}"
        );
    }

    /// `optimized { … }` and `downloads { … }` beside a Plex server whose
    /// films are read from /srv/media.
    fn areas(optimized: &str, downloads: &str) -> Result<(), ConfigError> {
        plex(&format!(
            r#"plex {{ url = "http://nas:32400"; path_mapping [ {{ from = "/data"; to = "/srv/media"; }} ] }}
            optimized {{ {optimized} }} downloads {{ {downloads} }}"#
        ))?
        .areas(Path::new("/var/lib/sf"))
        .map(|_| ())
    }

    fn refused_for(result: Result<(), ConfigError>) -> Option<&'static str> {
        match result {
            Err(ConfigError::Invalid { key, .. }) => Some(key),
            _ => None,
        }
    }

    #[test]
    fn the_default_areas_and_a_download_area_of_its_own_are_taken() {
        assert_eq!(
            [
                areas("", ""),
                areas(r#"dir = "/srv/copies";"#, r#"dir = "/srv/downloads";"#),
                areas(
                    r#"dir = "/srv/copies";"#,
                    r#"dir = "/srv/copies/downloads";"#
                ),
            ]
            .map(|r| r.is_ok()),
            [true; 3]
        );
    }

    /// Review 2026-10-20, finding 1: the sweeps would remove each other's
    /// files, the media, or the server's own.
    #[test]
    fn a_download_area_that_overlaps_the_pool_the_media_or_the_caches_is_refused() {
        let cases = [
            (r#"dir = "/srv/x";"#, r#"dir = "/srv/x";"#),
            (r#"dir = "/srv/x/copies";"#, r#"dir = "/srv/x";"#),
            (r#"dir = "/srv/x";"#, r#"dir = "/srv/x/making";"#),
            ("", r#"dir = "/srv/media";"#),
            ("", r#"dir = "/srv/media/films";"#),
            ("", r#"dir = "/srv";"#),
            ("", r#"dir = "/var/lib/sf/cache/previews";"#),
            (r#"dir = "/srv/media/.copies";"#, ""),
        ];
        assert_eq!(
            cases.map(|(optimized, downloads)| refused_for(areas(optimized, downloads))),
            [
                Some("downloads.dir"),
                Some("optimized.dir"),
                Some("downloads.dir"),
                Some("downloads.dir"),
                Some("downloads.dir"),
                Some("downloads.dir"),
                Some("downloads.dir"),
                Some("optimized.dir"),
            ]
        );
    }

    #[test]
    fn an_optimized_dir_that_is_or_holds_the_state_dir_is_refused() {
        for dir in ["/var/lib/sf", "/var/lib", "/"] {
            let config = on(&format!(r#"dir = "{dir}";"#));
            let err = config.and_then(|c| c.optimized.place(Path::new("/var/lib/sf")));
            assert!(
                matches!(
                    err,
                    Err(ConfigError::Invalid {
                        key: "optimized.dir",
                        ..
                    })
                ),
                "{dir}: {err:?}"
            );
        }
    }

    #[test]
    fn the_artwork_cache_limit_is_read_in_mib() {
        let config =
            plex(r#"plex { url = "http://nas:32400"; } artwork { cache_mb = 64; memory_mb = 8; }"#)
                .unwrap();
        assert_eq!(config.artwork.cache_bytes(), 64 << 20);
        assert_eq!(config.artwork.tiers().memory, 8 << 20);
        let default = plex(r#"plex { url = "http://nas:32400"; }"#).unwrap();
        assert_eq!(default.artwork.cache_bytes(), 1024 << 20);
        assert_eq!(default.artwork.tiers().memory, 128 << 20);
    }

    /// The caches' memory tiers take 256 MiB by default: the artwork 128,
    /// the previews 64 and the catalog's replies 64.
    #[test]
    fn the_caches_take_256_mib_of_memory_by_default() {
        let config = plex(r#"plex { url = "http://nas:32400"; }"#).unwrap();
        let total = config.artwork.tiers().memory
            + config.previews.settings().unwrap().memory_limit_bytes
            + config.catalog.memory_bytes();
        assert_eq!(total, 256 << 20);
        let set = plex(r#"plex { url = "http://nas:32400"; } catalog { memory_mb = 8; }"#).unwrap();
        assert_eq!(set.catalog.memory_bytes(), 8 << 20);
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
    fn trusted_devices_lapse_after_180_days_unless_set() {
        let default = ServerConfig::parse(r#"schema = "spoolframe.server.v1";"#).unwrap();
        assert_eq!(
            default.auth.trust_idle().unwrap(),
            Duration::from_secs(180 * 24 * 3600)
        );
        let set = ServerConfig::parse(
            r#"schema = "spoolframe.server.v1"; auth { trust_idle_days = 30; }"#,
        )
        .unwrap();
        assert_eq!(
            set.auth.trust_idle().unwrap(),
            Duration::from_secs(30 * 24 * 3600)
        );
        for bad in [0, MAX_TRUST_IDLE_DAYS + 1] {
            let refused = ServerConfig::parse(&format!(
                r#"schema = "spoolframe.server.v1"; auth {{ trust_idle_days = {bad}; }}"#
            ))
            .unwrap_err();
            assert!(
                matches!(
                    refused,
                    ConfigError::Invalid {
                        key: "auth.trust_idle_days",
                        ..
                    }
                ),
                "{bad}: {refused:?}"
            );
        }
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
