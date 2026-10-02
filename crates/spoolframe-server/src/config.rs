//! Server configuration — schema `spoolframe.server.v1`, written in UCL.
//!
//! Every section is `deny_unknown_fields`, and a repeated key is an error:
//! a misspelt or duplicated setting fails at startup instead of being
//! quietly ignored.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use spoolframe_media::encoders::{Hardware, HardwarePolicy, TranscodeSettings};
use spoolframe_plex::{PathMap, PathMapping};

use crate::encode::JobLimits;
use crate::network::{EndpointOptions, Relay};
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
    /// Where the server keeps its state: its endpoint key, its database
    /// and its caches.
    pub state_dir: PathBuf,
    /// The Plex Media Server the catalog reads from.
    pub plex: PlexConfig,
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
}

/// `watch { started_after_secs = 60; watched_percent = 90;
/// watched_remaining_secs = 300; }`, all optional.
#[derive(Debug, Clone, Default, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct WatchConfig {
    #[ucl(default)]
    pub started_after_secs: Option<u64>,
    #[ucl(default)]
    pub watched_percent: Option<u64>,
    #[ucl(default)]
    pub watched_remaining_secs: Option<u64>,
}

impl WatchConfig {
    /// The rules, defaults filled in.
    pub fn rules(&self) -> Result<WatchRules, ConfigError> {
        let defaults = WatchRules::default();
        let percent = self.watched_percent.unwrap_or(90);
        if !(1..=100).contains(&percent) {
            return Err(ConfigError::Invalid {
                key: "watch.watched_percent",
                message: format!("{percent} is not between 1 and 100"),
            });
        }
        Ok(WatchRules {
            started_after: self
                .started_after_secs
                .map_or(defaults.started_after, Duration::from_secs),
            watched_fraction: percent as f64 / 100.0,
            watched_remaining: self
                .watched_remaining_secs
                .map_or(defaults.watched_remaining, Duration::from_secs),
            ..defaults
        })
    }
}

/// `media { ffmpeg = "ffmpeg"; ffprobe = "ffprobe"; }`, all optional: by
/// default whatever is on `PATH`, which the nix package wraps.
#[derive(Debug, Clone, Default, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct MediaConfig {
    #[ucl(default)]
    pub ffmpeg: Option<PathBuf>,
    #[ucl(default)]
    pub ffprobe: Option<PathBuf>,
}

impl MediaConfig {
    pub fn ffmpeg(&self) -> PathBuf {
        self.ffmpeg.clone().unwrap_or_else(|| "ffmpeg".into())
    }

    pub fn ffprobe(&self) -> PathBuf {
        self.ffprobe.clone().unwrap_or_else(|| "ffprobe".into())
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
    /// Where encodes write; `state_dir/cache/transcode` if left out.
    #[ucl(default)]
    pub scratch_dir: Option<PathBuf>,
    /// The most all encodes may write together, in MiB; 20 GiB if left out.
    #[ucl(default)]
    pub scratch_limit_mb: Option<u64>,
    /// How far an encode may run ahead of its reader; 60 s if left out.
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

/// `admin { listen = "127.0.0.1:7421"; allow_remote = false; }`, all
/// optional.
#[derive(Debug, Clone, Default, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct AdminConfig {
    /// Where the admin API listens; `127.0.0.1:7421` if left out.
    #[ucl(default)]
    pub listen: Option<String>,
    /// Allow a non-loopback `listen`. Passwords and tokens cross the API
    /// in clear, so only behind a TLS reverse proxy.
    #[ucl(default)]
    pub allow_remote: Option<bool>,
}

impl AdminConfig {
    pub const DEFAULT_LISTEN: &'static str = "127.0.0.1:7421";

    /// The address to listen on, refused if it is not loopback and
    /// `allow_remote` is not set.
    pub fn listen(&self) -> Result<SocketAddr, ConfigError> {
        let text = self.listen.as_deref().unwrap_or(Self::DEFAULT_LISTEN);
        let addr: SocketAddr = text.parse().map_err(|e| ConfigError::Invalid {
            key: "admin.listen",
            message: format!("{text:?}: {e}"),
        })?;
        if !addr.ip().is_loopback() && !self.allow_remote.unwrap_or(false) {
            return Err(ConfigError::Invalid {
                key: "admin.listen",
                message: format!(
                    "{addr} is not a loopback address; passwords cross the admin API \
                     in clear, so put it behind a TLS proxy and set allow_remote = true"
                ),
            });
        }
        Ok(addr)
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

/// `plex { ... }`: where the PMS is, and how to authenticate to it.
#[derive(Debug, Clone, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct PlexConfig {
    /// The PMS base URL on the LAN, e.g. `http://192.168.1.20:32400`.
    /// Never plex.tv: the catalog must work without the Internet (§32).
    pub url: String,
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
}

/// `watch_sync { user = "andoriyu"; interval_secs = 120; }`
#[derive(Debug, Clone, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct WatchSyncConfig {
    /// The Spoolframe user whose state is the Plex account's.
    pub user: String,
    /// How often to sync when nothing changes here; 120 if left out.
    #[ucl(default)]
    pub interval_secs: Option<u64>,
}

impl WatchSyncConfig {
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_secs.unwrap_or(120).max(10))
    }
}

/// `{ library = "Music"; from = "/data"; to = "/mnt/nas/music"; }`
#[derive(Debug, Clone, Uclicious)]
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
    /// The PMS base URL.
    pub fn url(&self) -> Result<Url, ConfigError> {
        let url = Url::parse(&self.url).map_err(|e| ConfigError::Invalid {
            key: "plex.url",
            message: format!("{:?}: {e}", self.url),
        })?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(ConfigError::Invalid {
                key: "plex.url",
                message: format!("{:?}: not an http or https URL", self.url),
            });
        }
        Ok(url)
    }

    /// The path mappings, each with absolute `from` and `to`.
    pub fn path_map(&self) -> Result<PathMap, ConfigError> {
        self.path_mapping
            .iter()
            .map(|m| {
                for (key, path) in [("from", &m.from), ("to", &m.to)] {
                    if !path.is_absolute() {
                        return Err(ConfigError::Invalid {
                            key: "plex.path_mapping",
                            message: format!("{key} {:?} is not an absolute path", path),
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

    /// The token from `token_file`, without surrounding whitespace, or
    /// `None` when no file is configured. Read at startup, not at parse
    /// time, so the config can be checked where the secret is not present.
    pub fn token(&self) -> Result<Option<String>, ConfigError> {
        let Some(path) = &self.token_file else {
            return Ok(None);
        };
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Token {
            path: path.clone(),
            source,
        })?;
        let token = text.trim();
        if token.is_empty() {
            return Err(ConfigError::EmptyToken { path: path.clone() });
        }
        Ok(Some(token.to_owned()))
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
        parsed.plex.url()?;
        parsed.plex.path_map()?;
        parsed.network.endpoint_options()?;
        parsed.admin.listen()?;
        parsed.watch.rules()?;
        parsed.transcode.settings()?;
        parsed.transcode.limits(&parsed.state_dir)?;
        Ok(parsed)
    }

    /// Read and parse the config file at `path`.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        Self::parse(&text)
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
            state_dir = "/var/lib/spoolframe";
            plex { url = "http://nas:32400"; }
            "#,
        )
        .unwrap();
        assert_eq!(config.state_dir, PathBuf::from("/var/lib/spoolframe"));
    }

    #[test]
    fn an_unknown_key_is_rejected() {
        let err = ServerConfig::parse(
            r#"
            schema = "spoolframe.server.v1";
            state_dir = "/var/lib/spoolframe";
            state_dri = "/tmp";
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
            state_dir = "/var/lib/spoolframe";
            state_dir = "/srv/spoolframe";
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
            state_dir = "/var/lib/spoolframe";
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
        let err = ServerConfig::load(Path::new("/nonexistent/spoolframe.ucl")).unwrap_err();
        assert!(
            err.to_string().contains("/nonexistent/spoolframe.ucl"),
            "{err}"
        );
    }

    fn plex(section: &str) -> Result<ServerConfig, ConfigError> {
        ServerConfig::parse(&format!(
            r#"
            schema = "spoolframe.server.v1";
            state_dir = "/var/lib/spoolframe";
            {section}
            "#
        ))
    }

    #[test]
    fn the_plex_section_is_required() {
        let err = plex("").unwrap_err();
        assert!(matches!(err, ConfigError::Ucl(_)), "{err:?}");
    }

    #[test]
    fn the_plex_url_and_token_file_are_read() {
        let config = plex(
            r#"plex { url = "http://192.168.1.20:32400"; token_file = "/run/secrets/plex"; }"#,
        )
        .unwrap();
        assert_eq!(
            config.plex.url().unwrap().as_str(),
            "http://192.168.1.20:32400/"
        );
        assert_eq!(
            config.plex.token_file,
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
        let mappings = &config.plex.path_mapping;
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
        assert!(off.plex.watch_sync.is_none());
        let on = plex(r#"plex { url = "http://nas:32400"; watch_sync { user = "andoriyu"; } }"#)
            .unwrap();
        let sync = on.plex.watch_sync.unwrap();
        assert_eq!(sync.user, "andoriyu");
        assert_eq!(sync.interval(), Duration::from_secs(120));
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
    fn the_admin_api_listens_on_loopback_by_default() {
        let config = plex(r#"plex { url = "http://nas:32400"; }"#).unwrap();
        assert_eq!(
            config.admin.listen().unwrap(),
            AdminConfig::DEFAULT_LISTEN.parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn a_non_loopback_admin_address_needs_allow_remote() {
        let err = plex(r#"plex { url = "http://nas:32400"; } admin { listen = "0.0.0.0:7421"; }"#)
            .unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::Invalid {
                    key: "admin.listen",
                    ..
                }
            ),
            "{err:?}"
        );
        let config = plex(
            r#"plex { url = "http://nas:32400"; } admin { listen = "0.0.0.0:7421"; allow_remote = true; }"#,
        )
        .unwrap();
        assert!(config.admin.listen().is_ok());
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
    fn watch_rules_default_and_can_be_set() {
        let default = plex(r#"plex { url = "http://nas:32400"; }"#).unwrap();
        assert_eq!(default.watch.rules().unwrap(), WatchRules::default());
        let set = plex(
            r#"plex { url = "http://nas:32400"; } watch { watched_percent = 95; started_after_secs = 30; }"#,
        )
        .unwrap();
        let rules = set.watch.rules().unwrap();
        assert_eq!(rules.watched_fraction, 0.95);
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
        assert!(config.plex.path_mapping.is_empty());
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
        assert_eq!(config.plex.token().unwrap(), None);
    }

    #[test]
    fn the_token_is_trimmed() {
        let dir = std::env::temp_dir().join(format!("spoolframe-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("token");
        std::fs::write(&file, "abc123\n").unwrap();
        let config = PlexConfig {
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
            url: "http://nas:32400".into(),
            token_file: Some(PathBuf::from("/nonexistent/plex-token")),
            path_mapping: Vec::new(),
            watch_sync: None,
        };
        let err = config.token().unwrap_err();
        assert!(err.to_string().contains("/nonexistent/plex-token"), "{err}");
    }

    #[test]
    fn an_empty_token_file_is_rejected() {
        let dir = std::env::temp_dir().join(format!("spoolframe-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("empty-token");
        std::fs::write(&file, " \n").unwrap();
        let config = PlexConfig {
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
