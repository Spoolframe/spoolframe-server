//! The three layers a server's settings come from (P3-10), merged key by
//! key: the built-in defaults, the provided file, and the runtime layer
//! the server writes itself.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use super::secrets::Secrets;
use super::settings::{self, PLEX_TOKEN, SETTINGS};
use super::tree::{self, Table, Value};
use super::{ConfigError, FIRST_PLEX_SERVER, PlexConfig, PlexServerConfig, ServerConfig};

/// The built-in defaults: every setting at its default, with comments.
/// `spoolframe-server config defaults` prints it.
pub const DEFAULTS: &str = include_str!("defaults.ucl");

/// The runtime layer's file in the state directory.
pub const RUNTIME_FILE: &str = "runtime.ucl";

/// The secret the Plex token set at run time is kept as.
pub const PLEX_TOKEN_SECRET: &str = "plex-token";

/// The secret a Plex server's token set at run time is kept as: the first
/// server's as it always was, another's under its id (P3-11).
pub fn plex_token_secret(server: &str) -> String {
    if server == FIRST_PLEX_SERVER {
        PLEX_TOKEN_SECRET.to_owned()
    } else {
        format!("{PLEX_TOKEN_SECRET}-{server}")
    }
}

/// Where a setting's value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    Default,
    File,
    Runtime,
}

/// Where the layers are: not configuration, so they come from the command
/// line or the environment before any layer is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Locations {
    /// The key, the database, the caches and the runtime layer.
    pub state_dir: PathBuf,
    /// The provided file, if there is one.
    pub config: Option<PathBuf>,
}

impl Locations {
    /// The flags' values where given, else the environment's (`env` looks
    /// a variable up; an empty one counts as unset): `--state-dir`, then
    /// `SPOOLFRAME_STATE_DIR`, then systemd's `$STATE_DIRECTORY` (its
    /// first entry), then `$XDG_STATE_HOME/spoolframe`, then
    /// `/var/lib/spoolframe` for root or else XDG's own default,
    /// `~/.local/state/spoolframe`. `--config`, then `SPOOLFRAME_CONFIG`.
    pub fn resolve(
        state_dir: Option<PathBuf>,
        config: Option<PathBuf>,
        env: impl Fn(&str) -> Option<OsString>,
        root: bool,
    ) -> Result<Self, ConfigError> {
        let var = |name: &str| env(name).filter(|v| !v.is_empty());
        let state_dir = state_dir
            .or_else(|| var("SPOOLFRAME_STATE_DIR").map(PathBuf::from))
            .or_else(|| {
                var("STATE_DIRECTORY").and_then(|list| {
                    std::env::split_paths(&list)
                        .next()
                        .filter(|p| !p.as_os_str().is_empty())
                })
            })
            .or_else(|| var("XDG_STATE_HOME").map(|d| PathBuf::from(d).join("spoolframe")))
            .or_else(|| root.then(|| PathBuf::from("/var/lib/spoolframe")))
            .or_else(|| var("HOME").map(|home| PathBuf::from(home).join(".local/state/spoolframe")))
            .ok_or(ConfigError::NoStateDir)?;
        let config = config.or_else(|| var("SPOOLFRAME_CONFIG").map(PathBuf::from));
        Ok(Self { state_dir, config })
    }

    /// As [`Self::resolve`], from this process's environment.
    pub fn from_env(
        state_dir: Option<PathBuf>,
        config: Option<PathBuf>,
    ) -> Result<Self, ConfigError> {
        // SAFETY: geteuid has no preconditions and cannot fail.
        let root = unsafe { libc::geteuid() } == 0;
        Self::resolve(state_dir, config, |name| std::env::var_os(name), root)
    }
}

/// A server's layers, as read.
#[derive(Debug, Clone)]
pub struct Layers {
    defaults: Table,
    /// The provided file and what it sets, `schema` and `locked` taken out.
    file: Option<(PathBuf, Table)>,
    /// `locked = true;` in the provided file: what it sets, the runtime
    /// layer cannot change.
    locked: bool,
    runtime_path: PathBuf,
    runtime: Table,
}

/// Why a setting could not be changed.
#[derive(Debug, thiserror::Error)]
pub enum SettingError {
    #[error("there is no setting {0}")]
    Unknown(String),
    /// A client secret for the runtime layer that is the secret itself,
    /// not `${SF_ENV_…}` (PX-22): runtime.ucl is plain text, shown by
    /// `config show` and copied with the state directory.
    #[error(
        "auth.oidc: {provider:?}'s client_secret must be an environment variable, \
         ${{SF_ENV_<NAME>}}; paste the secret itself as the provider's client secret \
         instead (kept apart from the settings)"
    )]
    PlainSecret { provider: String },
    /// The provided file sets it, and is locked.
    #[error("{key} is set by the server's config file ({path}), which is locked")]
    Locked { key: String, path: PathBuf },
    /// The value is one the server would refuse to load: the same message
    /// a file with it gives.
    #[error("{0}")]
    Invalid(ConfigError),
}

/// Where one setting's value comes from, for `config show` and the admin
/// UI.
#[derive(Debug, Clone, PartialEq)]
pub struct Origin {
    /// `None` when no layer sets it.
    pub layer: Option<Layer>,
    pub locked: bool,
    /// What the provided file says, when the runtime layer overrides it.
    pub overrides: Option<Value>,
}

/// `upper` over `lower`, key by key: objects are merged, anything else is
/// replaced whole. So is an object that is one setting (`network.wan`, the
/// OTLP headers): its keys are its value, not settings of their own.
fn merge(lower: &mut Table, upper: &Table) {
    merge_at(lower, upper, "");
}

fn merge_at(lower: &mut Table, upper: &Table, prefix: &str) {
    for (key, value) in upper {
        let path = tree::join(prefix, key);
        match (lower.get_mut(key), value) {
            (Some(Value::Object(below)), Value::Object(above))
                if settings::setting(&path).is_none() =>
            {
                merge_at(below, above, &path)
            }
            _ => {
                lower.insert(key.clone(), value.clone());
            }
        }
    }
}

/// `network.wan.host_ipv6`, gone: iroh tells devices the host's IPv6
/// addresses itself, so it added nothing, and turning it off did not stop
/// IPv6 either.
const HOST_IPV6: &str = "network.wan.host_ipv6";

fn host_ipv6_is_gone() -> ConfigError {
    ConfigError::Invalid {
        key: HOST_IPV6,
        message: "is gone: iroh advertises the host's IPv6 addresses itself; network.ipv6 = \
                  false turns IPv6 off"
            .into(),
    }
}

/// A layer's document as a table: `schema` checked and taken out, and the
/// keys only the provided file may have refused anywhere else. A gone key
/// in the provided file (`written_by_hand`) is refused; in the runtime
/// layer, which the server wrote itself, it is dropped with a warning, so
/// a server whose web UI saved it still starts.
fn layer(
    text: &str,
    path: &Path,
    written_by_hand: bool,
) -> Result<(Table, Option<bool>), ConfigError> {
    let in_file = |source: ConfigError| ConfigError::File {
        path: path.to_owned(),
        source: Box::new(source),
    };
    let mut table = tree::parse(text).map_err(|e| in_file(ConfigError::Ucl(e)))?;
    if let Some(Value::String(dir)) = table.get("state_dir") {
        return Err(ConfigError::StateDirKey {
            path: path.to_owned(),
            value: dir.clone(),
        });
    }
    if tree::get(&table, "admin.allow_remote").is_some() {
        return Err(in_file(ConfigError::Invalid {
            key: "admin.allow_remote",
            message: "is gone: the admin listener takes any address now (P3-10); remove it".into(),
        }));
    }
    if tree::get(&table, HOST_IPV6).is_some() {
        if written_by_hand {
            return Err(in_file(host_ipv6_is_gone()));
        }
        tree::remove(&mut table, HOST_IPV6);
        tracing::warn!(
            path = %path.display(),
            "{}; dropped from the runtime layer",
            host_ipv6_is_gone()
        );
    }
    match table.remove("schema") {
        None => {}
        Some(Value::String(s)) if s == ServerConfig::SCHEMA => {}
        Some(other) => {
            return Err(in_file(ConfigError::SchemaMismatch {
                expected: ServerConfig::SCHEMA,
                found: match other {
                    Value::String(s) => s,
                    other => other.to_ucl(),
                },
            }));
        }
    }
    let locked = match table.remove("locked") {
        None => None,
        Some(Value::Bool(locked)) => Some(locked),
        Some(other) => {
            return Err(in_file(ConfigError::Invalid {
                key: "locked",
                message: format!("{} is not true or false", other.to_ucl()),
            }));
        }
    };
    Ok((table, locked))
}

impl Layers {
    /// Read the layers at `locations`. A missing runtime layer is an
    /// empty one: the server has not changed anything yet.
    pub fn load(locations: &Locations) -> Result<Self, ConfigError> {
        Self::read(
            locations.config.as_deref(),
            locations.state_dir.join(RUNTIME_FILE),
        )
    }

    /// The same layers read again from their files (PX-33): the provided
    /// file where it was, and the runtime layer.
    pub fn reread(&self) -> Result<Self, ConfigError> {
        Self::read(self.file_path(), self.runtime_path.clone())
    }

    fn read(config: Option<&Path>, runtime_path: PathBuf) -> Result<Self, ConfigError> {
        let file = match config {
            None => None,
            Some(path) => {
                let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
                    path: path.to_owned(),
                    source,
                })?;
                Some((path.to_owned(), text))
            }
        };
        let runtime = match std::fs::read_to_string(&runtime_path) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(source) => {
                return Err(ConfigError::Read {
                    path: runtime_path,
                    source,
                });
            }
        };
        Self::from_texts(
            file.as_ref().map(|(p, t)| (p.as_path(), t.as_str())),
            runtime.as_deref(),
            runtime_path,
        )
    }

    /// The layers from their texts: the provided file and where it is,
    /// and the runtime layer, which is kept at `runtime_path`.
    pub fn from_texts(
        file: Option<(&Path, &str)>,
        runtime: Option<&str>,
        runtime_path: PathBuf,
    ) -> Result<Self, ConfigError> {
        let defaults = tree::parse(DEFAULTS).expect("the built-in defaults parse");
        let (file, locked) = match file {
            None => (None, false),
            Some((path, text)) => {
                let (table, locked) = layer(text, path, true)?;
                (Some((path.to_owned(), table)), locked.unwrap_or(false))
            }
        };
        let runtime = match runtime {
            None => Table::new(),
            Some(text) => {
                let (table, locked) = layer(text, &runtime_path, false)?;
                if locked.is_some() {
                    return Err(ConfigError::File {
                        path: runtime_path,
                        source: Box::new(ConfigError::Invalid {
                            key: "locked",
                            message: "only the provided config file can lock settings".into(),
                        }),
                    });
                }
                table
            }
        };
        Ok(Self {
            defaults,
            file,
            locked,
            runtime_path,
            runtime,
        })
    }

    /// The provided file, if there is one.
    pub fn file_path(&self) -> Option<&Path> {
        self.file.as_ref().map(|(p, _)| p.as_path())
    }

    pub fn runtime_path(&self) -> &Path {
        &self.runtime_path
    }

    fn file(&self) -> Option<&Table> {
        self.file.as_ref().map(|(_, t)| t)
    }

    /// Whether the provided file sets `key` and locks it.
    pub fn is_locked(&self, key: &str) -> bool {
        self.locked && self.file().is_some_and(|f| tree::get(f, key).is_some())
    }

    /// The runtime layer as it would be used: without what a locked file
    /// sets, which wins.
    fn runtime_in_effect(&self) -> Table {
        let mut runtime = self.runtime.clone();
        for (key, _) in tree::leaves(&self.runtime) {
            if self.is_locked(&key) {
                tree::remove(&mut runtime, &key);
            }
        }
        runtime
    }

    /// Runtime-layer keys that a locked file overrides; the server warns
    /// about them at start.
    pub fn shadowed(&self) -> Vec<String> {
        tree::leaves(&self.runtime)
            .into_iter()
            .map(|(k, _)| k)
            .filter(|k| self.is_locked(k))
            .collect()
    }

    /// Every layer merged: runtime over the file over the defaults.
    pub fn merged(&self) -> Table {
        let mut merged = self.defaults.clone();
        if let Some(file) = self.file() {
            merge(&mut merged, file);
        }
        merge(&mut merged, &self.runtime_in_effect());
        merged
    }

    /// Where `key`'s value comes from.
    pub fn origin(&self, key: &str) -> Origin {
        let in_file = self.file().and_then(|f| tree::get(f, key));
        if self.is_locked(key) {
            return Origin {
                layer: Some(Layer::File),
                locked: true,
                overrides: None,
            };
        }
        if tree::get(&self.runtime, key).is_some() {
            return Origin {
                layer: Some(Layer::Runtime),
                locked: false,
                overrides: in_file.cloned(),
            };
        }
        let layer = if in_file.is_some() {
            Some(Layer::File)
        } else if tree::get(&self.defaults, key).is_some() {
            Some(Layer::Default)
        } else {
            None
        };
        Origin {
            layer,
            locked: false,
            overrides: None,
        }
    }

    /// The merged settings, typed and checked. A mistake is put on the
    /// provided file if the file with the defaults has it, else on the
    /// runtime layer.
    pub fn config(&self) -> Result<ServerConfig, ConfigError> {
        if let Some((path, file)) = &self.file {
            let mut alone = self.defaults.clone();
            merge(&mut alone, file);
            ServerConfig::from_table(&alone).map_err(|source| ConfigError::File {
                path: path.clone(),
                source: Box::new(source),
            })?;
        }
        ServerConfig::from_table(&self.merged()).map_err(|source| ConfigError::File {
            path: self.runtime_path.clone(),
            source: Box::new(source),
        })
    }

    /// The layers with `key` set to `value` in the runtime layer, checked
    /// as a file is at start.
    pub fn with_set(&self, key: &str, value: Value) -> Result<Self, SettingError> {
        self.changeable(key)?;
        let mut changed = self.clone();
        tree::set(&mut changed.runtime, key, value);
        changed.check()?;
        Ok(changed)
    }

    /// The layers without `key` in the runtime layer: the file's value or
    /// the default applies again.
    pub fn with_unset(&self, key: &str) -> Result<Self, SettingError> {
        self.changeable(key)?;
        let mut changed = self.clone();
        tree::remove(&mut changed.runtime, key);
        changed.check()?;
        Ok(changed)
    }

    /// The layers with each of `keys` set (or, with `None`, unset) in the
    /// runtime layer, checked once all are: one admin action that changes
    /// several keys may pass through a state that would not load, such as
    /// the first Plex server's address gone before its name (P3-18).
    pub fn with_changes(&self, keys: Vec<(&str, Option<Value>)>) -> Result<Self, SettingError> {
        let mut changed = self.clone();
        for (key, value) in keys {
            self.changeable(key)?;
            match value {
                Some(value) => tree::set(&mut changed.runtime, key, value),
                None => {
                    tree::remove(&mut changed.runtime, key);
                }
            }
        }
        changed.check()?;
        Ok(changed)
    }

    /// Whether the provided file sets `key`, locked or not.
    pub fn file_sets(&self, key: &str) -> bool {
        self.file().is_some_and(|f| tree::get(f, key).is_some())
    }

    /// Whether the runtime layer may hold `key`: a setting that is no
    /// secret and that a locked file does not set.
    fn changeable(&self, key: &str) -> Result<(), SettingError> {
        match settings::setting(key) {
            Some(s) if !s.secret => {}
            _ => return Err(SettingError::Unknown(key.to_owned())),
        }
        if self.is_locked(key) {
            return Err(SettingError::Locked {
                key: key.to_owned(),
                path: self.file_path().map(Path::to_owned).unwrap_or_default(),
            });
        }
        Ok(())
    }

    /// The merged settings checked, with a mistake as the file would have
    /// it without naming the file.
    fn check(&self) -> Result<ServerConfig, SettingError> {
        if let Some(provider) = plain_client_secret(&self.runtime) {
            return Err(SettingError::PlainSecret { provider });
        }
        ServerConfig::from_table(&self.merged()).map_err(SettingError::Invalid)
    }

    /// The layers with the wizard's progress in the runtime layer: not a
    /// setting, so neither the settings list nor a lock applies.
    pub fn with_setup(&self, done: &[String], finished: bool) -> Self {
        let mut changed = self.clone();
        let done = Value::Array(done.iter().cloned().map(Value::String).collect());
        tree::set(&mut changed.runtime, "setup.done", done);
        tree::set(
            &mut changed.runtime,
            "setup.finished",
            Value::Bool(finished),
        );
        changed
    }

    /// The wizard's progress: the steps confirmed, and whether it is over.
    /// A server set up before the wizard existed (Plex in its file, nothing
    /// written by a wizard) counts as set up.
    pub fn setup(&self) -> (Vec<String>, bool) {
        let done = match tree::get(&self.runtime, "setup.done") {
            Some(Value::Array(steps)) => steps
                .iter()
                .filter_map(|s| match s {
                    Value::String(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        let finished = match tree::get(&self.runtime, "setup") {
            Some(_) => tree::get(&self.runtime, "setup.finished") == Some(&Value::Bool(true)),
            None => self
                .file()
                .is_some_and(|f| tree::get(f, "plex.url").is_some()),
        };
        (done, finished)
    }

    /// Whether the Plex token may be set at run time: not when a locked
    /// file names a token file.
    pub fn plex_token_changeable(&self) -> Result<(), SettingError> {
        self.server_token_changeable(FIRST_PLEX_SERVER)
    }

    /// Whether the token of Plex server `server` may be set at run time:
    /// not when a locked file says where it is (the first server's
    /// `token_file`, or the other servers' list).
    pub fn server_token_changeable(&self, server: &str) -> Result<(), SettingError> {
        let (key, locked_by) = if server == FIRST_PLEX_SERVER {
            (PLEX_TOKEN.to_owned(), "plex.token_file")
        } else {
            (format!("the token of Plex server {server}"), "plex.servers")
        };
        if self.is_locked(locked_by) {
            return Err(SettingError::Locked {
                key,
                path: self.file_path().map(Path::to_owned).unwrap_or_default(),
            });
        }
        Ok(())
    }

    /// The Plex token: the one set at run time (a secret) unless a locked
    /// file names a token file, else `plex.token_file`'s.
    pub fn plex_token(
        &self,
        plex: &PlexConfig,
        secrets: &Secrets,
    ) -> Result<Option<String>, ConfigError> {
        let first = plex
            .all_servers()
            .into_iter()
            .next()
            .expect("the first server is always there");
        self.server_token(&first, secrets)
    }

    /// A Plex server's token, as [`Self::plex_token`] is the first one's.
    pub fn server_token(
        &self,
        server: &PlexServerConfig,
        secrets: &Secrets,
    ) -> Result<Option<String>, ConfigError> {
        if self.server_token_changeable(&server.id).is_ok() {
            let name = plex_token_secret(&server.id);
            let secret = secrets.get(&name).map_err(|source| ConfigError::Token {
                path: secrets.path(&name),
                source,
            })?;
            if secret.is_some() {
                return Ok(secret);
            }
        }
        server.token()
    }

    /// Write the runtime layer: a new file (0600) that replaces the old
    /// one whole, so a crash leaves one or the other.
    pub fn write_runtime(&self) -> std::io::Result<()> {
        let dir = self.runtime_path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir)?;
        let partial = dir.join(format!(".{RUNTIME_FILE}.new"));
        let mut text = String::from(
            "# Written by spoolframe-server when settings are changed in the web UI\n\
             # or with `spoolframe-server config set`. Do not edit: the server\n\
             # rewrites it whole. `spoolframe-server config show` shows every\n\
             # setting and where it comes from.\n",
        );
        let _ = writeln!(text, "schema = {:?};", ServerConfig::SCHEMA);
        text.push_str(&tree::to_document(&self.runtime));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&partial)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&partial, &self.runtime_path)
    }

    /// A line each deprecated setting (PGPU-05) a layer sets (the
    /// defaults set none): which, where, and the `transcode.gpus` to
    /// write in its place, exactly what the server reads it as.
    pub fn deprecations(&self) -> Vec<String> {
        let instead = self.gpus_instead();
        DEPRECATED
            .iter()
            .filter_map(|key| {
                let place = match self.origin(key).layer? {
                    Layer::Default => return None,
                    Layer::File => "config file",
                    Layer::Runtime => "runtime settings",
                };
                Some(format!("{key} is deprecated ({place}): use {instead}"))
            })
            .collect()
    }

    /// `transcode.gpus = [ … ]`: what these layers' transcode settings
    /// mean, as `transcode.gpus` says it; the key alone when they do not
    /// parse.
    pub fn gpus_instead(&self) -> String {
        match self.config().and_then(|c| c.transcode.gpus()) {
            Ok(entries) => format!(
                "transcode.gpus = {}",
                spoolframe_media::gpus::GpuEntry::ucl_list(&entries)
            ),
            Err(_) => "transcode.gpus".into(),
        }
    }

    /// `config show`: every setting, its value and where it comes from.
    /// Secrets only as set or not set.
    pub fn show(&self, secrets: &Secrets) -> String {
        let merged = self.merged();
        let mut out = String::from("# Built-in defaults");
        if let Some(path) = self.file_path() {
            let lock = if self.locked { " (locked)" } else { "" };
            let _ = write!(out, ", {}{lock}", path.display());
        }
        let _ = writeln!(out, ", {}", self.runtime_path.display());
        for setting in SETTINGS {
            let origin = self.origin(setting.key);
            let (value, origin) = if setting.secret {
                let set = secrets.is_set(PLEX_TOKEN_SECRET);
                let value = if set { "(set)" } else { "(not set)" };
                let origin = match (set, self.plex_token_changeable()) {
                    (true, Ok(())) => format!(
                        "runtime secret, {}",
                        secrets.path(PLEX_TOKEN_SECRET).display()
                    ),
                    (true, Err(_)) => {
                        "runtime secret, unused: the locked config file names plex.token_file"
                            .into()
                    }
                    (false, _) => "not set".into(),
                };
                (value.to_owned(), origin)
            } else if setting.write_only {
                let value = match tree::get(&merged, setting.key) {
                    Some(_) => "(set)",
                    None => "(not set)",
                };
                (value.to_owned(), describe(&origin))
            } else {
                let value = tree::get(&merged, setting.key).map_or_else(
                    || "(not set)".to_owned(),
                    |v| shown(setting.key, v).to_ucl(),
                );
                (value, describe(&origin))
            };
            let _ = writeln!(out, "{} = {value};  # {origin}", setting.key);
        }
        for deprecated in self.deprecations() {
            let _ = writeln!(out, "# {deprecated}");
        }
        out
    }
}

/// Settings still read but deprecated (PGPU-05): `transcode.gpus` says
/// what they did and more. To be removed in a later release.
pub const DEPRECATED: [&str; 2] = ["transcode.hardware", "transcode.vaapi_device"];

/// Where a value comes from, in words.
fn describe(origin: &Origin) -> String {
    match (origin.layer, &origin.overrides) {
        (None, _) => "not set".into(),
        (Some(Layer::Default), _) => "default".into(),
        (Some(Layer::File), _) if origin.locked => "config file, locked".into(),
        (Some(Layer::File), _) => "config file".into(),
        (Some(Layer::Runtime), None) => "runtime".into(),
        (Some(Layer::Runtime), Some(file)) => {
            format!("runtime, over the config file's {}", file.to_ucl())
        }
    }
}

/// What a provider's literal client secret is shown as, in `config show`
/// and the settings API: a provided file may hold one, and it is never
/// shown. The runtime layer refuses it back, as any literal.
pub const HIDDEN_SECRET: &str = "(hidden)";

/// `value` as shown for setting `key`: `auth.oidc` with each client
/// secret that is not a `${SF_ENV_…}` reference hidden.
pub fn shown(key: &str, value: &Value) -> Value {
    let Value::Array(providers) = value else {
        return value.clone();
    };
    if key != "auth.oidc" {
        return value.clone();
    }
    Value::Array(
        providers
            .iter()
            .map(|p| match p {
                Value::Object(p) => {
                    let mut p = p.clone();
                    if let Some(secret) = p.get_mut("client_secret")
                        && !matches!(secret, Value::String(s) if tree::is_env_reference(s))
                    {
                        *secret = Value::String(HIDDEN_SECRET.to_owned());
                    }
                    Value::Object(p)
                }
                other => other.clone(),
            })
            .collect(),
    )
}

/// The name of a provider in `table`'s `auth.oidc` whose `client_secret`
/// is not a `${SF_ENV_…}` reference, if any.
pub fn plain_client_secret(table: &Table) -> Option<String> {
    let Some(Value::Array(providers)) = tree::get(table, "auth.oidc") else {
        return None;
    };
    providers.iter().find_map(|p| match p {
        Value::Object(p) => match p.get("client_secret") {
            Some(Value::String(s)) if tree::is_env_reference(s) => None,
            Some(_) => Some(match p.get("name") {
                Some(Value::String(name)) => name.clone(),
                _ => String::new(),
            }),
            None => None,
        },
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            vars.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn the_flag_wins_over_each_variable_in_turn() {
        let all = [
            ("SPOOLFRAME_STATE_DIR", "/from/spoolframe"),
            ("STATE_DIRECTORY", "/from/systemd:/second"),
            ("XDG_STATE_HOME", "/from/xdg"),
            ("HOME", "/home/me"),
            ("SPOOLFRAME_CONFIG", "/etc/env.ucl"),
        ];
        let dir = |flag: Option<&str>, vars: &[(&str, &str)], root: bool| {
            Locations::resolve(flag.map(PathBuf::from), None, env(vars), root)
                .unwrap()
                .state_dir
        };
        assert_eq!(dir(Some("/flag"), &all, false), PathBuf::from("/flag"));
        assert_eq!(dir(None, &all, false), PathBuf::from("/from/spoolframe"));
        assert_eq!(dir(None, &all[1..], false), PathBuf::from("/from/systemd"));
        assert_eq!(
            dir(None, &all[2..], true),
            PathBuf::from("/from/xdg/spoolframe")
        );
        assert_eq!(
            dir(None, &all[3..], true),
            PathBuf::from("/var/lib/spoolframe")
        );
        assert_eq!(
            dir(None, &all[3..], false),
            PathBuf::from("/home/me/.local/state/spoolframe")
        );
        // Empty is unset.
        assert_eq!(
            dir(
                None,
                &[("SPOOLFRAME_STATE_DIR", ""), ("STATE_DIRECTORY", "/s")],
                false
            ),
            PathBuf::from("/s")
        );
        assert!(matches!(
            Locations::resolve(None, None, env(&[]), false),
            Err(ConfigError::NoStateDir)
        ));

        let config = |flag: Option<&str>| {
            Locations::resolve(None, flag.map(PathBuf::from), env(&all), false)
                .unwrap()
                .config
        };
        assert_eq!(config(Some("/flag.ucl")), Some(PathBuf::from("/flag.ucl")));
        assert_eq!(config(None), Some(PathBuf::from("/etc/env.ucl")));
        let none = Locations::resolve(None, None, env(&all[..1]), false).unwrap();
        assert_eq!(none.config, None);
    }

    fn layers(file: Option<&str>, runtime: Option<&str>) -> Result<Layers, ConfigError> {
        Layers::from_texts(
            file.map(|t| (Path::new("/etc/spoolframe.ucl"), t)),
            runtime,
            PathBuf::from("/var/lib/spoolframe/runtime.ucl"),
        )
    }

    /// PGPU-05: the old GPU settings, set in a layer, are a warning each,
    /// naming the `gpus` to write instead.
    #[test]
    fn the_old_gpu_settings_are_deprecated_where_they_are_set() {
        let file = layers(
            Some(
                r#"schema = "spoolframe.server.v1"; transcode { hardware = ["nvenc", "vaapi"]; }"#,
            ),
            Some(r#"transcode { vaapi_device = "/dev/dri/renderD128"; }"#),
        )
        .unwrap();
        let instead = r#"use transcode.gpus = [{ api = "nvenc"; pick = "all"; }, { api = "vaapi"; device = "/dev/dri/renderD128"; }]"#;
        assert_eq!(
            file.deprecations(),
            [
                format!("transcode.hardware is deprecated (config file): {instead}"),
                format!("transcode.vaapi_device is deprecated (runtime settings): {instead}"),
            ]
        );
        assert!(
            file.show(&Secrets::new(Path::new("/nonexistent")))
                .contains(&format!(
                    "# transcode.hardware is deprecated (config file): {instead}"
                ))
        );
    }

    #[test]
    fn gpus_or_nothing_is_no_deprecation() {
        assert!(layers(None, None).unwrap().deprecations().is_empty());
        let gpus = layers(
            None,
            Some(r#"transcode { gpus = [ { api = "vaapi"; pick = "dgpu"; } ]; }"#),
        )
        .unwrap();
        assert!(gpus.deprecations().is_empty());
    }

    /// What the warning says to write reads as exactly what the old
    /// settings were read as.
    #[test]
    fn the_suggested_gpus_are_what_the_old_settings_meant() {
        for old in [
            r#"hardware = ["nvenc", "vaapi"]; vaapi_device = "/dev/dri/renderD129";"#,
            r#"hardware = "auto";"#,
            r#"hardware = "none";"#,
            r#"hardware = "qsv";"#,
            r#"vaapi_device = "/dev/dri/by-path/pci-0000:03:00.0-render";"#,
        ] {
            let before = layers(
                Some(&format!(
                    r#"schema = "spoolframe.server.v1"; transcode {{ {old} }}"#
                )),
                None,
            )
            .unwrap();
            let meant = before.config().unwrap().transcode.gpus().unwrap();
            let instead = before.gpus_instead();
            let written = instead.strip_prefix("transcode.gpus = ").unwrap();
            let after = layers(
                Some(&format!(
                    r#"schema = "spoolframe.server.v1"; transcode {{ gpus = {written}; }}"#
                )),
                None,
            )
            .unwrap();
            assert_eq!(
                after.config().unwrap().transcode.gpus().unwrap(),
                meant,
                "{old}"
            );
            assert!(after.deprecations().is_empty());
        }
    }

    #[test]
    fn the_defaults_alone_are_a_server_that_is_not_set_up() {
        let config = layers(None, None).unwrap().config().unwrap();
        assert!(config.plex.is_none());
        assert_eq!(config.server_name(), "Spoolframe");
    }

    #[test]
    fn the_defaults_document_says_what_the_code_does_without_it() {
        let documented = ServerConfig::parse(DEFAULTS).unwrap();
        let bare = ServerConfig::parse(&format!("schema = {:?};", ServerConfig::SCHEMA)).unwrap();
        let dir = Path::new("/s");
        assert_eq!(documented.server_name(), bare.server_name());
        assert_eq!(
            documented.network.endpoint_options().unwrap(),
            bare.network.endpoint_options().unwrap()
        );
        assert_eq!(
            documented.network.plex_discovery_port(),
            bare.network.plex_discovery_port()
        );
        assert_eq!(
            documented.admin.listen().unwrap(),
            bare.admin.listen().unwrap()
        );
        assert_eq!(documented.artwork.cache_bytes(), bare.artwork.cache_bytes());
        assert_eq!(
            documented.watch.rules().unwrap(),
            bare.watch.rules().unwrap()
        );
        assert_eq!(documented.media.ffmpeg(), bare.media.ffmpeg());
        assert_eq!(documented.media.ffprobe(), bare.media.ffprobe());
        assert_eq!(documented.media.tesseract(), bare.media.tesseract());
        assert_eq!(
            documented.media.sandbox().unwrap(),
            bare.media.sandbox().unwrap()
        );
        assert_eq!(
            documented
                .transcode
                .settings(&documented.experimental)
                .unwrap(),
            bare.transcode.settings(&bare.experimental).unwrap()
        );
        assert_eq!(
            documented.transcode.limits(dir).unwrap(),
            bare.transcode.limits(dir).unwrap()
        );
        assert_eq!(
            documented.subtitles.ocr_settings(dir).unwrap(),
            bare.subtitles.ocr_settings(dir).unwrap()
        );
        assert_eq!(
            documented.previews.settings().unwrap(),
            bare.previews.settings().unwrap()
        );
        assert_eq!(documented.admin.metrics(), bare.admin.metrics());
        assert_eq!(
            documented.logs.buffer_lines().unwrap(),
            bare.logs.buffer_lines().unwrap()
        );
        assert_eq!(
            documented.telemetry.otlp.settings().unwrap(),
            bare.telemetry.otlp.settings().unwrap()
        );
        assert_eq!(
            documented.admin.metrics_from().unwrap(),
            bare.admin.metrics_from().unwrap()
        );
        assert_eq!(
            documented.auth.trust_idle().unwrap(),
            bare.auth.trust_idle().unwrap()
        );
    }

    #[test]
    fn objects_merge_key_by_key_and_later_layers_win() {
        let layers = layers(
            Some(
                r#"
                name = "Nimble";
                plex { url = "http://nas:32400"; path_mapping [ { from = "/a"; to = "/b"; } ] }
                transcode { max_jobs = 4; }
                "#,
            ),
            Some(r#"name = "Den"; plex { path_mapping [ { from = "/a"; to = "/c"; } ] }"#),
        )
        .unwrap();
        let config = layers.config().unwrap();
        assert_eq!(config.server_name(), "Den");
        let plex = config.plex.unwrap();
        assert_eq!(plex.url, "http://nas:32400");
        assert_eq!(plex.path_mapping[0].to, PathBuf::from("/c"));
        // The file's, beside the defaults' other transcode keys.
        assert_eq!(config.transcode.max_jobs, Some(4));
        assert_eq!(config.transcode.read_ahead_secs, Some(60));

        assert_eq!(
            layers.origin("name"),
            Origin {
                layer: Some(Layer::Runtime),
                locked: false,
                overrides: Some(Value::String("Nimble".into())),
            }
        );
        assert_eq!(layers.origin("plex.url").layer, Some(Layer::File));
        assert_eq!(
            layers.origin("transcode.read_ahead_secs").layer,
            Some(Layer::Default)
        );
        assert_eq!(layers.origin("network.port").layer, None);
    }

    #[test]
    fn a_setting_that_is_a_table_is_replaced_whole() {
        // network.wan is one setting: the runtime's table is the whole of
        // it, not added to the file's, so a static address the web UI took
        // out stays out.
        let layers = layers(
            Some(
                r#"network { port = 7420; wan { static = ["203.0.113.7:7420"];
                   mapped_port { port = 7420; } } }"#,
            ),
            Some(r#"network { wan { static = ["198.51.100.7:7420"]; } }"#),
        )
        .unwrap();
        let config = layers.config().unwrap();
        let wan = config.network.wan.unwrap();
        assert_eq!(wan.statics, Some(vec!["198.51.100.7:7420".to_owned()]));
        assert!(wan.mapped_port.is_none());
        // The table's neighbours still merge key by key.
        assert_eq!(config.network.port, Some(7420));
        assert_eq!(
            tree::leaves(&layers.runtime)
                .into_iter()
                .map(|(k, _)| k)
                .collect::<Vec<_>>(),
            ["network.wan"]
        );
    }

    #[test]
    fn a_locked_file_wins_over_the_runtime_layer_and_refuses_changes_to_what_it_sets() {
        let layers = layers(
            Some(r#"locked = true; name = "Nimble"; plex { url = "http://nas:32400"; }"#),
            Some(r#"name = "Den"; transcode { max_jobs = 3; }"#),
        )
        .unwrap();
        let config = layers.config().unwrap();
        assert_eq!(config.server_name(), "Nimble");
        assert_eq!(config.transcode.max_jobs, Some(3));
        assert_eq!(layers.shadowed(), ["name"]);
        assert!(layers.origin("name").locked);
        assert!(!layers.origin("transcode.max_jobs").locked);

        let refused = layers
            .with_set("name", Value::String("Attic".into()))
            .unwrap_err();
        assert!(matches!(refused, SettingError::Locked { .. }), "{refused}");
        assert_eq!(
            refused.to_string(),
            "name is set by the server's config file (/etc/spoolframe.ucl), which is locked"
        );
        assert!(matches!(
            layers.with_unset("name"),
            Err(SettingError::Locked { .. })
        ));
        // What it leaves out stays editable.
        let changed = layers
            .with_set("plex.path_mapping", Value::Array(vec![]))
            .unwrap();
        assert!(changed.config().is_ok());
    }

    fn provider(secret: &str) -> Value {
        Value::from_json(&serde_json::json!([{
            "name": "Pocket ID", "issuer": "https://id.example.org",
            "client_id": "spoolframe", "client_secret": secret,
        }]))
        .unwrap()
    }

    #[test]
    fn the_runtime_layer_takes_a_client_secret_only_as_an_sf_env_reference() {
        let layers = layers(None, None).unwrap();
        let refused = layers
            .with_set("auth.oidc", provider("hunter2"))
            .unwrap_err();
        assert!(
            matches!(&refused, SettingError::PlainSecret { provider } if provider == "Pocket ID"),
            "{refused}"
        );
        // A reference is checked like any other value: this variable is
        // not set, so the change is refused naming it.
        let unset = layers
            .with_set("auth.oidc", provider("${SF_ENV_PX22_NEVER_SET}"))
            .unwrap_err();
        assert_eq!(
            unset.to_string(),
            "auth.oidc.client_secret names ${SF_ENV_PX22_NEVER_SET}, but the server has no \
             environment variable SF_ENV_PX22_NEVER_SET"
        );
    }

    #[test]
    fn a_files_literal_client_secret_is_never_shown() {
        let layers = layers(
            Some(
                r#"auth { oidc [ { name = "Pocket ID"; issuer = "https://id.example.org";
                   client_id = "spoolframe"; client_secret = "hunter2"; } ] }"#,
            ),
            None,
        )
        .unwrap();
        let text = layers.show(&Secrets::new(Path::new("/nonexistent")));
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.contains(r#"client_secret = "(hidden)""#), "{text}");
        // Written back as shown, it is refused: "(hidden)" is no reference.
        let back = shown(
            "auth.oidc",
            tree::get(&layers.merged(), "auth.oidc").unwrap(),
        );
        assert!(matches!(
            layers.with_set("auth.oidc", back),
            Err(SettingError::PlainSecret { .. })
        ));
    }

    #[test]
    fn a_reference_is_kept_as_written_and_expanded_only_in_the_typed_config() {
        let layers = layers(
            None,
            Some(r#"plex { url = "http://${SF_ENV_PLEX_HOST}:32400"; }"#),
        )
        .unwrap();
        let env = |name: &str| (name == "SF_ENV_PLEX_HOST").then(|| "nas".to_owned());
        let config = ServerConfig::from_table_with_env(&layers.merged(), &env).unwrap();
        assert_eq!(config.plex.unwrap().url, "http://nas:32400");
        assert_eq!(
            tree::get(&layers.merged(), "plex.url"),
            Some(&Value::String("http://${SF_ENV_PLEX_HOST}:32400".into()))
        );
    }

    #[test]
    fn without_lock_a_runtime_change_wins_and_unset_brings_the_file_back() {
        let file = layers(Some(r#"name = "Nimble";"#), None).unwrap();
        let changed = file.with_set("name", Value::String("Den".into())).unwrap();
        assert_eq!(changed.config().unwrap().server_name(), "Den");
        let back = changed.with_unset("name").unwrap();
        assert_eq!(back.config().unwrap().server_name(), "Nimble");
        assert_eq!(back.origin("name").layer, Some(Layer::File));
    }

    #[test]
    fn a_value_the_file_would_refuse_is_refused_with_the_file_s_message() {
        let layers = layers(None, None).unwrap();
        let refused = layers
            .with_set("watch.watched_percent", Value::Int(0))
            .unwrap_err();
        let file = ServerConfig::parse(&format!(
            "schema = {:?}; watch {{ watched_percent = 0; }}",
            ServerConfig::SCHEMA
        ))
        .unwrap_err();
        assert_eq!(refused.to_string(), file.to_string());

        let wrong_type = layers
            .with_set("transcode.max_jobs", Value::String("two".into()))
            .unwrap_err();
        let file = ServerConfig::parse(&format!(
            "schema = {:?}; transcode {{ max_jobs = \"two\"; }}",
            ServerConfig::SCHEMA
        ))
        .unwrap_err();
        assert_eq!(wrong_type.to_string(), file.to_string());

        assert!(matches!(
            layers.with_set("plex.urll", Value::String("x".into())),
            Err(SettingError::Unknown(_))
        ));
        // A secret is never a layer's.
        assert!(matches!(
            layers.with_set(PLEX_TOKEN, Value::String("abc".into())),
            Err(SettingError::Unknown(_))
        ));
    }

    #[test]
    fn a_file_that_still_names_its_state_dir_is_refused_saying_what_to_do() {
        let err = layers(
            Some(r#"schema = "spoolframe.server.v1"; state_dir = "/srv/spoolframe";"#),
            None,
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "/etc/spoolframe.ucl: state_dir is no longer read from the config file: remove it, \
             and pass --state-dir /srv/spoolframe (or set SPOOLFRAME_STATE_DIR=/srv/spoolframe) \
             instead"
        );
    }

    #[test]
    fn allow_remote_is_refused_saying_it_is_gone() {
        let err = layers(
            Some(r#"admin { listen = "0.0.0.0:7431"; allow_remote = true; }"#),
            None,
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "/etc/spoolframe.ucl: invalid config: admin.allow_remote: is gone: the admin \
             listener takes any address now (P3-10); remove it"
        );
    }

    #[test]
    fn host_ipv6_in_the_file_is_refused_saying_it_is_gone() {
        let err = layers(Some("network { wan { host_ipv6 = true; } }"), None).unwrap_err();
        assert_eq!(
            err.to_string(),
            "/etc/spoolframe.ucl: invalid config: network.wan.host_ipv6: is gone: iroh \
             advertises the host's IPv6 addresses itself; network.ipv6 = false turns IPv6 off"
        );
    }

    #[test]
    fn host_ipv6_the_web_ui_saved_is_dropped_from_the_runtime_layer() {
        let layers = layers(
            None,
            Some(r#"network { wan { static = ["203.0.113.7:7420"]; host_ipv6 = false; } }"#),
        )
        .unwrap();
        assert_eq!(tree::get(&layers.runtime, "network.wan.host_ipv6"), None);
        let wan = layers.config().unwrap().network.wan.unwrap();
        assert_eq!(wan.statics, Some(vec!["203.0.113.7:7420".to_owned()]));
    }

    #[test]
    fn the_wizard_s_progress_is_kept_and_an_older_server_counts_as_set_up() {
        let fresh = layers(None, None).unwrap();
        assert_eq!(fresh.setup(), (vec![], false));
        let halfway = fresh.with_setup(&["admin".into(), "server".into()], false);
        assert!(halfway.config().is_ok());
        assert_eq!(
            halfway.setup(),
            (vec!["admin".into(), "server".into()], false)
        );
        let older = layers(Some(r#"plex { url = "http://nas:32400"; }"#), None).unwrap();
        assert!(older.setup().1);
        // Once a wizard has written its progress, that is what counts.
        assert!(!older.with_setup(&[], false).setup().1);
    }

    #[test]
    fn a_mistake_names_the_layer_it_is_in() {
        let err = layers(Some("transcode { max_jobs = 0; }"), None)
            .unwrap()
            .config()
            .unwrap_err();
        assert!(
            err.to_string().starts_with("/etc/spoolframe.ucl: "),
            "{err}"
        );
        let err = layers(Some("name = \"x\";"), Some("transcode { max_jobs = 0; }"))
            .unwrap()
            .config()
            .unwrap_err();
        assert!(
            err.to_string()
                .starts_with("/var/lib/spoolframe/runtime.ucl: "),
            "{err}"
        );
        let err = layers(Some("namee = \"x\";"), None)
            .unwrap()
            .config()
            .unwrap_err();
        assert!(
            err.to_string().starts_with("/etc/spoolframe.ucl: "),
            "{err}"
        );
        assert!(matches!(
            layers(None, Some("locked = true;")),
            Err(ConfigError::File { .. })
        ));
        assert!(layers(Some("schema = \"spoolframe.server.v2\";"), None).is_err());
    }

    #[tokio::test]
    async fn the_runtime_layer_is_written_whole_for_the_server_s_user_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = spoolframe_test_dir::TestDir::new("spoolframe-runtime-layer").await;
        let locations = Locations {
            state_dir: dir.as_ref().join("state"),
            config: None,
        };
        let layers = Layers::load(&locations).unwrap();
        let changed = layers
            .with_set("name", Value::String("Den".into()))
            .unwrap()
            .with_set("transcode.max_jobs", Value::Int(3))
            .unwrap();
        changed.write_runtime().unwrap();
        let path = locations.state_dir.join(RUNTIME_FILE);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let again = Layers::load(&locations).unwrap();
        assert_eq!(again.config().unwrap().server_name(), "Den");
        assert_eq!(
            again.origin("transcode.max_jobs").layer,
            Some(Layer::Runtime)
        );
        // Rewritten whole: what was unset is gone from the file.
        again.with_unset("name").unwrap().write_runtime().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("Den"), "{text}");
        assert!(text.contains("max_jobs = 3;"), "{text}");
    }

    #[tokio::test]
    async fn show_names_each_value_s_layer_and_never_a_secret() {
        let dir = spoolframe_test_dir::TestDir::new("spoolframe-show").await;
        let secrets = Secrets::new(dir.as_ref());
        secrets.set(PLEX_TOKEN_SECRET, "s3cr3t-t0ken").unwrap();
        let layers = layers(
            Some(r#"name = "Nimble"; plex { url = "http://nas:32400"; }"#),
            Some(r#"name = "Den";"#),
        )
        .unwrap();
        let shown = layers.show(&secrets);
        let line = |key: &str| {
            shown
                .lines()
                .find(|l| l.starts_with(&format!("{key} = ")))
                .unwrap_or_else(|| panic!("no {key} in {shown}"))
                .to_owned()
        };
        assert_eq!(
            line("name"),
            r#"name = "Den";  # runtime, over the config file's "Nimble""#
        );
        assert_eq!(
            line("plex.url"),
            r#"plex.url = "http://nas:32400";  # config file"#
        );
        assert_eq!(
            line("transcode.max_jobs"),
            "transcode.max_jobs = 2;  # default"
        );
        assert_eq!(line("network.port"), "network.port = (not set);  # not set");
        assert!(line("plex.token").starts_with("plex.token = (set);  # runtime secret"));
        assert!(!shown.contains("s3cr3t"), "{shown}");
    }

    #[tokio::test]
    async fn the_token_set_at_run_time_wins_unless_a_locked_file_names_a_token_file() {
        let dir = spoolframe_test_dir::TestDir::new("spoolframe-token-layers").await;
        let file_token = dir.as_ref().join("file-token");
        std::fs::write(&file_token, "from-file\n").unwrap();
        let secrets = Secrets::new(dir.as_ref());
        let text = |locked: bool| {
            format!(
                r#"locked = {locked}; plex {{ url = "http://nas:32400"; token_file = "{}"; }}"#,
                file_token.display()
            )
        };
        let token = |locked: bool| {
            let layers = layers(Some(&text(locked)), None).unwrap();
            let plex = layers.config().unwrap().plex.unwrap();
            layers.plex_token(&plex, &secrets).unwrap()
        };
        assert_eq!(token(false).as_deref(), Some("from-file"));
        secrets.set(PLEX_TOKEN_SECRET, "from-ui").unwrap();
        assert_eq!(token(false).as_deref(), Some("from-ui"));
        assert_eq!(token(true).as_deref(), Some("from-file"));
    }
}
