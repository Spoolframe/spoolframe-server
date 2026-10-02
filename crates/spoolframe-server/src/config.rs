//! Server configuration — schema `spoolframe.server.v1`, written in UCL.
//!
//! Every section is `deny_unknown_fields`, and a repeated key is an error:
//! a misspelt or duplicated setting fails at startup instead of being
//! quietly ignored.

use std::path::{Path, PathBuf};

use spoolframe_plex::{PathMap, PathMapping};

use crate::network::{EndpointOptions, Relay};
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
    /// Where the server keeps its state: its endpoint key, its database
    /// and its caches.
    pub state_dir: PathBuf,
    /// The Plex Media Server the catalog reads from.
    pub plex: PlexConfig,
    /// The Iroh endpoint clients connect to.
    #[ucl(default)]
    pub network: NetworkConfig,
}

/// `network { port = 7420; relay = "disabled"; }`, all optional.
#[derive(Debug, Clone, Default, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
pub struct NetworkConfig {
    /// The UDP port to listen on; a free one is picked if left out. Fix it
    /// when a firewall needs to know.
    #[ucl(default)]
    pub port: Option<u16>,
    /// `"disabled"` (the default), `"default"` for the public n0 relays,
    /// or a relay URL. Phase 1 is LAN only and accepts only `"disabled"`;
    /// the others are parsed so remote access (P3) only has to lift that.
    #[ucl(default)]
    pub relay: Option<String>,
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
        };
        assert_eq!(config.token().unwrap().as_deref(), Some("abc123"));
    }

    #[test]
    fn a_missing_token_file_names_its_path() {
        let config = PlexConfig {
            url: "http://nas:32400".into(),
            token_file: Some(PathBuf::from("/nonexistent/plex-token")),
            path_mapping: Vec::new(),
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
        };
        assert!(matches!(
            config.token(),
            Err(ConfigError::EmptyToken { .. })
        ));
    }
}
