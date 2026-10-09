//! A relay of the operator's own: its config (`spoolframe.relay.v1`).
//!
//! ```ucl
//! schema = "spoolframe.relay.v1";
//! # The main server, which says who is its owner's.
//! server { key = "<its endpoint id>"; addrs = ["10.10.0.6:7430"]; }
//! # Who may relay through it: owner, friends, everyone else.
//! access { owner = "allow"; friends = "allow"; everyone = "deny"; }
//! # How it is served: one of
//! tls { mode = "files"; https = "0.0.0.0:443"; cert_file = "…"; key_file = "…"; }
//! tls { mode = "acme"; https = "0.0.0.0:443"; hostname = "relay.example.org"; contact = "me@example.org";
//!       directory = "letsencrypt-staging"; }  # "letsencrypt" (the default), its staging, or a URL
//! tls { mode = "proxy"; }   # TLS ends at a proxy in front: plain HTTP on `http`
//! http = "0.0.0.0:80";      # the captive-portal probe; the relay itself under `proxy`
//! quic = "0.0.0.0:7842";    # optional: QUIC address discovery (not under `proxy`)
//! ```

use std::net::SocketAddr;
use std::path::PathBuf;

use iroh::{EndpointAddr, EndpointId, TransportAddr};
use uclicious::Uclicious;

/// A relay config that cannot be used, and why.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RelayConfigError {
    #[error("{0}")]
    Ucl(String),
    #[error("schema {found:?} is not {expected:?}")]
    Schema {
        expected: &'static str,
        found: String,
    },
    #[error("{key}: {message}")]
    Invalid { key: &'static str, message: String },
}

#[derive(Debug, Clone, Uclicious)]
#[ucl(deny_unknown_fields)]
struct RelayFile {
    schema: String,
    server: ServerFile,
    #[ucl(default)]
    access: Option<AccessFile>,
    tls: TlsFile,
    #[ucl(default)]
    http: Option<String>,
    #[ucl(default)]
    quic: Option<String>,
}

#[derive(Debug, Clone, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
struct ServerFile {
    key: String,
    #[ucl(default)]
    addrs: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
struct AccessFile {
    #[ucl(default)]
    owner: Option<String>,
    #[ucl(default)]
    friends: Option<String>,
    #[ucl(default)]
    everyone: Option<String>,
}

#[derive(Debug, Clone, Uclicious)]
#[ucl(skip_builder, deny_unknown_fields)]
struct TlsFile {
    mode: String,
    #[ucl(default)]
    https: Option<String>,
    #[ucl(default)]
    cert_file: Option<String>,
    #[ucl(default)]
    key_file: Option<String>,
    #[ucl(default)]
    hostname: Option<String>,
    #[ucl(default)]
    contact: Option<String>,
    #[ucl(default)]
    directory: Option<String>,
}

/// Who may relay through it, by tier (the user's three): each allowed
/// or not. A rate per tier would go here, if iroh-relay's access hook
/// could give one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Access {
    pub owner: bool,
    pub friends: bool,
    pub everyone: bool,
}

impl Default for Access {
    fn default() -> Self {
        Self {
            owner: true,
            friends: true,
            everyone: false,
        }
    }
}

/// How the relay is served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Serve {
    /// TLS with the operator's certificate chain and key, PEM files.
    Files {
        https: SocketAddr,
        cert_file: PathBuf,
        key_file: PathBuf,
    },
    /// TLS with a certificate from an ACME server (Let's Encrypt by
    /// default), got with the TLS-ALPN-01 challenge on `https` (port 443
    /// as the Internet sees it): `directory` is the server's directory URL.
    Acme {
        https: SocketAddr,
        hostname: String,
        contact: String,
        directory: String,
    },
    /// TLS ends at a proxy in front (nginx): plain HTTP on `http`.
    Proxy,
}

/// A relay's config, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayConfig {
    /// The main server: its key, and where to dial it.
    pub server: EndpointAddr,
    pub access: Access,
    pub serve: Serve,
    /// Plain HTTP: the captive-portal probe, and under [`Serve::Proxy`]
    /// the relay itself.
    pub http: SocketAddr,
    /// QUIC address discovery, if wanted.
    pub quic: Option<SocketAddr>,
}

/// Let's Encrypt's ACME directory (`directory = "letsencrypt"`).
pub const LETSENCRYPT: &str = "https://acme-v02.api.letsencrypt.org/directory";
/// Let's Encrypt's staging directory (`"letsencrypt-staging"`): untrusted
/// certificates, generous limits, for trying the setup out.
pub const LETSENCRYPT_STAGING: &str = "https://acme-staging-v02.api.letsencrypt.org/directory";

impl RelayConfig {
    pub const SCHEMA: &'static str = "spoolframe.relay.v1";
    /// HTTP by default: port 80, where the captive-portal probe goes.
    pub const DEFAULT_HTTP: &'static str = "0.0.0.0:80";
    pub const DEFAULT_HTTPS: &'static str = "0.0.0.0:443";

    /// Shapes that load but are likely not what the operator meant, for
    /// the log at start (P6-07): in `check_proxy`'s tradition, warned of
    /// rather than refused, as each can be meant.
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.serve == Serve::Proxy && !self.http.ip().is_loopback() {
            out.push(format!(
                "tls.mode is \"proxy\", but http listens on {}, not loopback: the relay is \
                 reachable in plain HTTP around the proxy that is meant to hold its TLS. \
                 Listen on 127.0.0.1 unless the proxy is on another host",
                self.http
            ));
        }
        if self.access.everyone {
            out.push(
                "access.everyone is \"allow\": anyone may relay through this relay, using its \
                 bandwidth, and the main server is not asked. For your own devices only, \
                 \"allow\" the owner and \"deny\" everyone"
                    .into(),
            );
        }
        out
    }

    pub fn parse(input: &str) -> Result<Self, RelayConfigError> {
        let ucl = |e: uclicious::UclError| RelayConfigError::Ucl(e.to_string());
        let mut builder = RelayFile::builder().map_err(ucl)?;
        builder
            .add_chunk_full(
                input,
                uclicious::Priority::default(),
                uclicious::DuplicateStrategy::UCL_DUPLICATE_ERROR,
            )
            .map_err(ucl)?;
        let file = builder
            .build()
            .map_err(|e| RelayConfigError::Ucl(e.to_string()))?;
        if file.schema != Self::SCHEMA {
            return Err(RelayConfigError::Schema {
                expected: Self::SCHEMA,
                found: file.schema,
            });
        }
        Ok(Self {
            server: server(&file.server)?,
            access: access(file.access.unwrap_or_default())?,
            serve: serve(&file.tls)?,
            http: addr("http", file.http.as_deref().unwrap_or(Self::DEFAULT_HTTP))?,
            quic: match file.quic.as_deref() {
                None => None,
                Some(_) if file.tls.mode == "proxy" => {
                    return Err(invalid(
                        "quic",
                        "QUIC address discovery needs the relay's own TLS: not with tls.mode = \"proxy\"".into(),
                    ));
                }
                Some(text) => Some(addr("quic", text)?),
            },
        })
    }
}

fn invalid(key: &'static str, message: String) -> RelayConfigError {
    RelayConfigError::Invalid { key, message }
}

fn addr(key: &'static str, text: &str) -> Result<SocketAddr, RelayConfigError> {
    text.parse()
        .map_err(|e| invalid(key, format!("{text:?}: {e}")))
}

fn server(file: &ServerFile) -> Result<EndpointAddr, RelayConfigError> {
    let id: EndpointId = file
        .key
        .parse()
        .map_err(|e| invalid("server.key", format!("{:?}: {e}", file.key)))?;
    let mut addrs = Vec::new();
    for text in file.addrs.iter().flatten() {
        addrs.push(TransportAddr::Ip(addr("server.addrs", text)?));
    }
    Ok(EndpointAddr::from_parts(id, addrs))
}

fn access(file: AccessFile) -> Result<Access, RelayConfigError> {
    let tier = |key: &'static str, value: Option<&str>, default: bool| match value {
        None => Ok(default),
        Some("allow") => Ok(true),
        Some("deny") => Ok(false),
        Some(other) => Err(invalid(key, format!("{other:?}: \"allow\" or \"deny\""))),
    };
    let default = Access::default();
    Ok(Access {
        owner: tier("access.owner", file.owner.as_deref(), default.owner)?,
        friends: tier("access.friends", file.friends.as_deref(), default.friends)?,
        everyone: tier(
            "access.everyone",
            file.everyone.as_deref(),
            default.everyone,
        )?,
    })
}

fn serve(file: &TlsFile) -> Result<Serve, RelayConfigError> {
    let https = || {
        addr(
            "tls.https",
            file.https.as_deref().unwrap_or(RelayConfig::DEFAULT_HTTPS),
        )
    };
    let needed = |key: &'static str, value: &Option<String>| {
        value
            .clone()
            .ok_or_else(|| invalid(key, format!("needed with tls.mode = {:?}", file.mode)))
    };
    let unwanted = |pairs: &[(&'static str, bool)]| match pairs.iter().find(|(_, set)| *set) {
        Some((key, _)) => Err(invalid(
            key,
            format!("not used with tls.mode = {:?}", file.mode),
        )),
        None => Ok(()),
    };
    match file.mode.as_str() {
        "files" => {
            unwanted(&[
                ("tls.hostname", file.hostname.is_some()),
                ("tls.contact", file.contact.is_some()),
                ("tls.directory", file.directory.is_some()),
            ])?;
            Ok(Serve::Files {
                https: https()?,
                cert_file: needed("tls.cert_file", &file.cert_file)?.into(),
                key_file: needed("tls.key_file", &file.key_file)?.into(),
            })
        }
        "acme" => {
            unwanted(&[
                ("tls.cert_file", file.cert_file.is_some()),
                ("tls.key_file", file.key_file.is_some()),
            ])?;
            let directory = match file.directory.as_deref() {
                None | Some("letsencrypt") => LETSENCRYPT.to_owned(),
                Some("letsencrypt-staging") => LETSENCRYPT_STAGING.to_owned(),
                Some(url) => match url::Url::parse(url) {
                    Ok(parsed) if parsed.scheme() == "https" => url.to_owned(),
                    _ => {
                        return Err(invalid(
                            "tls.directory",
                            format!(
                                "{url:?}: \"letsencrypt\", \"letsencrypt-staging\" or an \
                                 ACME directory's https URL"
                            ),
                        ));
                    }
                },
            };
            Ok(Serve::Acme {
                https: https()?,
                hostname: needed("tls.hostname", &file.hostname)?,
                contact: needed("tls.contact", &file.contact)?,
                directory,
            })
        }
        "proxy" => {
            unwanted(&[
                ("tls.https", file.https.is_some()),
                ("tls.cert_file", file.cert_file.is_some()),
                ("tls.key_file", file.key_file.is_some()),
                ("tls.hostname", file.hostname.is_some()),
                ("tls.contact", file.contact.is_some()),
                ("tls.directory", file.directory.is_some()),
            ])?;
            Ok(Serve::Proxy)
        }
        other => Err(invalid(
            "tls.mode",
            format!("{other:?}: \"files\", \"acme\" or \"proxy\""),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The main server's key, as `spoolframe-server id` prints it.
    fn key() -> String {
        iroh::SecretKey::from_bytes(&[7; 32]).public().to_string()
    }

    fn config(rest: &str) -> Result<RelayConfig, RelayConfigError> {
        RelayConfig::parse(&format!(
            "schema = \"spoolframe.relay.v1\";\nserver {{ key = \"{}\"; addrs = [\"10.0.0.2:7430\"]; }}\n{rest}",
            key()
        ))
    }

    #[test]
    fn behind_a_proxy_it_serves_plain_http_and_lets_the_owner_s_through() {
        let relay = config(r#"tls { mode = "proxy"; } http = "127.0.0.1:3340";"#).unwrap();
        assert_eq!(relay.serve, Serve::Proxy);
        assert_eq!(relay.http, "127.0.0.1:3340".parse().unwrap());
        assert_eq!(relay.access, Access::default());
        assert_eq!(relay.server.id.to_string(), key());
    }

    #[test]
    fn files_and_acme_each_take_what_they_need() {
        let files = config(r#"tls { mode = "files"; cert_file = "/c.pem"; key_file = "/k.pem"; }"#)
            .unwrap();
        assert!(matches!(files.serve, Serve::Files { .. }), "{files:?}");
        let acme = config(
            r#"tls { mode = "acme"; hostname = "relay.example.org"; contact = "me@example.org"; }"#,
        )
        .unwrap();
        assert!(
            matches!(&acme.serve, Serve::Acme { directory, .. } if directory == LETSENCRYPT),
            "{acme:?}"
        );
    }

    #[test]
    fn acme_takes_let_s_encrypt_s_staging_or_another_directory() {
        let directory = |value: &str| {
            config(&format!(
                r#"tls {{ mode = "acme"; hostname = "r.example.org"; contact = "me@example.org"; directory = "{value}"; }}"#
            ))
            .map(|relay| match relay.serve {
                Serve::Acme { directory, .. } => directory,
                other => panic!("{other:?}"),
            })
        };
        assert_eq!(
            directory("letsencrypt-staging").unwrap(),
            LETSENCRYPT_STAGING
        );
        assert_eq!(
            directory("https://acme.example.org/dir").unwrap(),
            "https://acme.example.org/dir"
        );
        assert!(directory("staging").is_err());
    }

    #[test]
    fn a_mode_missing_what_it_needs_or_given_another_s_is_refused() {
        for (rest, key) in [
            (
                r#"tls { mode = "files"; cert_file = "/c.pem"; }"#,
                "tls.key_file",
            ),
            (
                r#"tls { mode = "acme"; hostname = "r.example.org"; }"#,
                "tls.contact",
            ),
            (
                r#"tls { mode = "proxy"; hostname = "r.example.org"; }"#,
                "tls.hostname",
            ),
            (r#"tls { mode = "proxy"; } quic = "0.0.0.0:7842";"#, "quic"),
            (r#"tls { mode = "letsencrypt"; }"#, "tls.mode"),
        ] {
            assert!(
                matches!(config(rest), Err(RelayConfigError::Invalid { key: k, .. }) if k == key),
                "{rest}: {:?}",
                config(rest)
            );
        }
    }

    #[test]
    fn each_tier_is_allowed_or_denied() {
        let relay = config(
            r#"tls { mode = "proxy"; } access { owner = "allow"; friends = "deny"; everyone = "allow"; }"#,
        )
        .unwrap();
        assert_eq!(
            relay.access,
            Access {
                owner: true,
                friends: false,
                everyone: true
            }
        );
        assert!(config(r#"tls { mode = "proxy"; } access { everyone = "1 Mbit/s"; }"#).is_err());
    }

    /// P6-07 (ds4 P6-2 and P6-3, glm E-5): a plain-HTTP relay off loopback
    /// behind a proxy, and a relay open to everyone, load but are warned of.
    #[test]
    fn a_proxied_relay_off_loopback_or_one_open_to_everyone_is_warned_of() {
        let warned = |rest: &str| config(rest).unwrap().warnings();
        assert_eq!(warned(r#"tls { mode = "proxy"; }"#).len(), 1);
        assert!(warned(r#"tls { mode = "proxy"; }"#)[0].contains("0.0.0.0:80"));
        assert!(warned(r#"http = "127.0.0.1:3340"; tls { mode = "proxy"; }"#).is_empty());
        let open = warned(
            r#"http = "127.0.0.1:3340"; tls { mode = "proxy"; } access { everyone = "allow"; }"#,
        );
        assert_eq!(open.len(), 1);
        assert!(open[0].contains("access.everyone"));
    }
}
