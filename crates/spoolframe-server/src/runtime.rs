//! Settings changed while the server runs (P3-10): from the web UI, the
//! setup wizard and `config set`, through the admin API.
//!
//! A change is checked by the same parse a config file goes through,
//! written to the runtime layer (or, for a secret, its own file), and then
//! applied: a live setting reaches the running server at once, a restart
//! one waits for the next start and says so. The config file is read
//! again on SIGHUP ([`reload`], PX-33).

pub mod reload;

use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use spoolframe_admin_api::{RestartMode, SettingApply, SettingEntry, SettingLayer, Settings};
use spoolframe_plex::PlexCatalog;
use spoolframe_types::{Library, LibraryId, Namespace};

use crate::admin::CookiePolicy;
use crate::catalog::{Composite, LiveCatalog, PlexSource, Served, ids};
use crate::config::layers::{
    self, Layer, Layers, PLEX_TOKEN_SECRET, SettingError, plex_token_secret,
};
use crate::config::secrets::Secrets;
use crate::config::settings::{self, Apply, PLEX_TOKEN, SETTINGS, Setting};
use crate::config::tree::{self, Table, Value};
use crate::config::{ConfigError, FIRST_PLEX_SERVER, LibraryConfig, PlexConfig, ServerConfig};
use crate::encode::job::Jobs;
use crate::network::ServerName;
use crate::previews::Previews;
use crate::watch::Watch;

/// What the live settings reach in the running server.
#[derive(Debug, Clone, Default)]
pub struct Live {
    /// `name`.
    pub name: Option<ServerName>,
    /// `plex.*`, `libraries` and the Plex tokens: the catalog is rebuilt
    /// and swapped.
    pub catalog: Arc<LiveCatalog>,
    /// `admin.insecure_cookies`.
    pub cookies: CookiePolicy,
    /// `watch.*`.
    pub watch: Option<Arc<Watch>>,
    /// `transcode.max_jobs`, `max_remux`, `max_gpu_jobs`, `scratch_limit_mb`,
    /// `read_ahead_secs` and `gpu_filters`.
    pub jobs: Option<Arc<Jobs>>,
    /// `previews.*`.
    pub previews: Option<Arc<Previews>>,
    /// `auth.oidc` (P3-04).
    pub oidc: Option<Arc<crate::oidc::Oidc>>,
    /// `admin.metrics` and `admin.metrics_from` (P3-14).
    pub metrics: Option<Arc<crate::metrics::Metrics>>,
    /// `logs.buffer_lines` (P3-14).
    pub logs: Option<Arc<crate::logs::LogRing>>,
    /// `telemetry.otlp.*` (P3-14).
    pub telemetry: Option<Arc<crate::telemetry::Telemetry>>,
    /// Not a setting: what a reload detects the encoders again for
    /// (PX-33), as SIGHUP did before.
    pub transcoding: Option<Arc<crate::transcode::Transcoding>>,
    /// `network.uplink.*`, `network.lan_prefixes` and
    /// `network.wan_prefixes` (PX-46).
    pub uplink: Option<Arc<crate::uplink::Uplink>>,
}

/// Why a change was not made.
#[derive(Debug, thiserror::Error)]
pub enum ChangeError {
    #[error(transparent)]
    Setting(#[from] SettingError),
    /// A secret that is no string, or an empty one.
    #[error("{key} is a string, and cannot be empty")]
    Secret { key: String },
    #[error("cannot save the setting: {0}")]
    Write(std::io::Error),
    /// The settings an edit was worked out from changed before it was
    /// saved (another admin's edit, or the same one sent twice): saving
    /// it would undo that one (review 2026-10-11 M25).
    #[error("{key} changed meanwhile; look again and repeat the change")]
    Conflict { key: String },
}

/// What [`RuntimeSettings::apply`] does with the Plex catalog.
enum Plex {
    /// Its settings did not change: kept.
    Same,
    /// Made from the settings already, and checked.
    Made(Option<Composite>),
    /// Made here (a token set): one that cannot be made is logged.
    Remake,
}

/// The server's settings as they change at run time.
#[derive(Debug)]
pub struct RuntimeSettings {
    layers: Mutex<Layers>,
    /// Held while a change is worked out and saved, or the files reread:
    /// one at a time, so `layers` is held only to read or swap it, never
    /// across a file written and synced (review 2026-10-11 M26).
    writing: Mutex<()>,
    secrets: Secrets,
    state_dir: PathBuf,
    /// Every layer merged as the server started: what the restart
    /// settings in effect are.
    started: Table,
    live: Live,
    /// How a restart brings the server back: its supervisor
    /// (`SPOOLFRAME_SUPERVISED`, which the NixOS module sets), or itself.
    mode: RestartMode,
    restart: tokio::sync::Notify,
    /// Held for a whole reload (PX-33), detection included: one at a
    /// time.
    reloading: tokio::sync::Mutex<()>,
}

/// The libraries as clients see them: each with its Spoolframe name, an id
/// naming its Plex server and section, and which way it syncs.
fn served(plex: &PlexConfig, libraries: &[LibraryConfig]) -> Result<Vec<Served>, ConfigError> {
    libraries
        .iter()
        .map(|l| {
            let section =
                LibraryId::new(Namespace::Plex, l.plex.section.as_str()).map_err(|e| {
                    ConfigError::Invalid {
                        key: "libraries",
                        message: e.to_string(),
                    }
                })?;
            Ok(Served {
                library: Library {
                    id: ids::library(&l.plex.server, section),
                    title: l.name.clone(),
                    kind: l.kind()?,
                },
                server: l.plex.server.clone(),
                section: l.plex.section.clone(),
                sync: plex.sync(&l.plex),
            })
        })
        .collect()
}

/// The catalog `config` and the layers' tokens make, if `config` has
/// Plex: every Plex server, each serving only its sections that are
/// libraries (all of the first one's while `libraries` is left out).
pub fn plex_catalog(
    config: &ServerConfig,
    layers: &Layers,
    secrets: &Secrets,
) -> Result<Option<Composite>, ConfigError> {
    let Some(plex) = &config.plex else {
        return Ok(None);
    };
    let libraries = config
        .libraries
        .as_deref()
        .map(|l| served(plex, l))
        .transpose()?;
    let mut sources = Vec::new();
    for server in plex.all_servers() {
        let token = layers.server_token(&server, secrets)?;
        let sections = match &libraries {
            None if server.id == FIRST_PLEX_SERVER => None,
            None => Some(Default::default()),
            Some(served) => Some(
                served
                    .iter()
                    .filter(|l| l.server == server.id)
                    .map(|l| l.section.clone())
                    .collect(),
            ),
        };
        let url = server.url()?;
        let catalog = PlexCatalog::new(url, token).map_err(|e| ConfigError::Invalid {
            key: "plex.url",
            message: e.to_string(),
        })?;
        sources.push(PlexSource {
            name: server.display_name(),
            catalog: Arc::new(catalog.with_paths(server.path_map()?).serving(sections)),
            id: server.id,
        });
    }
    Ok(Some(
        Composite::new(sources, libraries).with_default_sync(plex.default_sync(FIRST_PLEX_SERVER)),
    ))
}

/// Whether `layers` turn insecure cookies on behind an `https` public URL
/// (PX-11 finding 32). A change that makes it so is refused, whichever of
/// the two it sets; a config file that already says so only gets the
/// warning at start.
fn insecure_over_https(layers: &Layers) -> bool {
    layers.config().is_ok_and(|config| {
        config.admin.insecure_cookies()
            && config
                .admin
                .public_url
                .as_deref()
                .and_then(|u| url::Url::parse(u).ok())
                .is_some_and(|u| u.scheme() == "https")
    })
}

impl RuntimeSettings {
    pub fn new(layers: Layers, state_dir: &Path, live: Live, mode: RestartMode) -> Self {
        Self {
            started: layers.merged(),
            layers: Mutex::new(layers),
            writing: Mutex::new(()),
            secrets: Secrets::new(state_dir),
            state_dir: state_dir.to_owned(),
            live,
            mode,
            restart: tokio::sync::Notify::new(),
            reloading: tokio::sync::Mutex::new(()),
        }
    }

    /// The layers now.
    pub fn layers(&self) -> Layers {
        self.layers.lock().clone()
    }

    pub fn secrets(&self) -> &Secrets {
        &self.secrets
    }

    /// Where the server keeps its state.
    pub(crate) fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Every setting, as `admin.settings` answers.
    pub fn settings(&self) -> Settings {
        let layers = self.layers();
        let merged = layers.merged();
        Settings {
            entries: SETTINGS
                .iter()
                .map(|s| self.entry(&layers, &merged, s))
                .collect(),
            config_file: layers.file_path().map(|p| p.display().to_string()),
            can_restart: true,
            restart_mode: self.mode,
        }
    }

    fn entry(&self, layers: &Layers, merged: &Table, setting: &Setting) -> SettingEntry {
        let apply = match setting.apply {
            Apply::Live => SettingApply::Live,
            Apply::Restart => SettingApply::Restart,
        };
        if setting.secret {
            let locked = layers.plex_token_changeable().is_err();
            let set = !locked && self.secrets.is_set(PLEX_TOKEN_SECRET);
            return SettingEntry {
                key: setting.key.to_owned(),
                value: None,
                secret: true,
                set,
                layer: set.then_some(SettingLayer::Runtime),
                locked,
                overrides: None,
                apply,
                restart_pending: false,
                running: None,
                deprecated: None,
            };
        }
        let value = tree::get(merged, setting.key);
        let origin = layers.origin(setting.key);
        if setting.write_only {
            // Write-only (the OTLP headers): whether it is set, never what.
            return SettingEntry {
                key: setting.key.to_owned(),
                value: None,
                secret: true,
                set: value.is_some(),
                layer: origin.layer.map(|layer| match layer {
                    Layer::Default => SettingLayer::Default,
                    Layer::File => SettingLayer::File,
                    Layer::Runtime => SettingLayer::Runtime,
                }),
                locked: origin.locked,
                overrides: None,
                apply,
                restart_pending: false,
                running: None,
                deprecated: None,
            };
        }
        let running = tree::get(&self.started, setting.key);
        let restart_pending = setting.apply == Apply::Restart && value != running;
        SettingEntry {
            key: setting.key.to_owned(),
            value: value.map(|v| layers::shown(setting.key, v).to_json()),
            secret: false,
            set: value.is_some(),
            layer: origin.layer.map(|layer| match layer {
                Layer::Default => SettingLayer::Default,
                Layer::File => SettingLayer::File,
                Layer::Runtime => SettingLayer::Runtime,
            }),
            locked: origin.locked,
            overrides: origin
                .overrides
                .as_ref()
                .map(|v| layers::shown(setting.key, v).to_json()),
            apply,
            restart_pending,
            running: restart_pending
                .then(|| running.map(|v| layers::shown(setting.key, v).to_json()))
                .flatten(),
            deprecated: layers::DEPRECATED
                .contains(&setting.key)
                .then(|| layers.gpus_instead()),
        }
    }

    /// The setting `key` now.
    pub fn entry_for(&self, key: &str) -> Option<SettingEntry> {
        let layers = self.layers();
        let merged = layers.merged();
        settings::setting(key).map(|s| self.entry(&layers, &merged, s))
    }

    /// Set `key` to `value`, as admin `by`.
    pub fn set(
        &self,
        key: &str,
        value: &serde_json::Value,
        by: &str,
    ) -> Result<SettingEntry, ChangeError> {
        if key == PLEX_TOKEN {
            let token = value
                .as_str()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .ok_or_else(|| ChangeError::Secret { key: key.into() })?;
            return self.set_secret(key, Some(token), by);
        }
        let value = Value::from_json(value).map_err(|message| {
            SettingError::Invalid(ConfigError::Invalid {
                key: "value",
                message,
            })
        })?;
        // The OTLP headers may carry a collector's key: never logged.
        let shown = if settings::setting(key).is_some_and(|s| s.write_only) {
            "(not shown)".to_owned()
        } else {
            value.to_ucl()
        };
        self.change(key, by, |layers| Ok(layers.with_set(key, value)?))?;
        tracing::info!(by, key, value = shown, "setting changed");
        if layers::DEPRECATED.contains(&key) {
            tracing::warn!(
                by,
                key,
                "{key} is deprecated: use {}",
                self.layers().gpus_instead()
            );
        }
        Ok(self.entry_for(key).expect("a known setting"))
    }

    /// Take `key` out of the runtime settings (or forget a secret), as
    /// admin `by`.
    pub fn unset(&self, key: &str, by: &str) -> Result<SettingEntry, ChangeError> {
        if key == PLEX_TOKEN {
            return self.set_secret(key, None, by);
        }
        self.change(key, by, |layers| Ok(layers.with_unset(key)?))?;
        tracing::info!(
            by,
            key,
            "setting unset: the config file's value or the default applies"
        );
        Ok(self.entry_for(key).expect("a known setting"))
    }

    /// Make `change` to the layers, write the runtime layer, and apply it.
    fn change(
        &self,
        key: &str,
        by: &str,
        change: impl FnOnce(&Layers) -> Result<Layers, ChangeError>,
    ) -> Result<(), ChangeError> {
        let _writing = self.writing.lock();
        let layers = self.layers();
        let changed = change(&layers)?;
        if insecure_over_https(&changed) && !insecure_over_https(&layers) {
            return Err(SettingError::Invalid(ConfigError::Invalid {
                key: "admin.insecure_cookies",
                message: "insecure cookies cannot be on while admin.public_url is https: \
                          behind a TLS proxy they would only send the session cookie \
                          without Secure"
                    .into(),
            })
            .into());
        }
        let (before, after) = (layers.merged(), changed.merged());
        let plex_changed = ["plex", "libraries"]
            .iter()
            .any(|key| tree::get(&before, key) != tree::get(&after, key));
        // The catalog the change makes, made before anything is saved: a
        // token file that cannot be read refuses the change, as it
        // refuses the server's start (review 2026-10-11 M24), rather than
        // saving it while the catalog keeps the old token.
        let plex = if plex_changed {
            let config = changed.config().map_err(SettingError::Invalid)?;
            Plex::Made(
                plex_catalog(&config, &changed, &self.secrets).map_err(SettingError::Invalid)?,
            )
        } else {
            Plex::Same
        };
        changed.write_runtime().map_err(ChangeError::Write)?;
        *self.layers.lock() = changed.clone();
        self.apply(&changed, plex, key, by);
        Ok(())
    }

    fn set_secret(
        &self,
        key: &str,
        value: Option<&str>,
        by: &str,
    ) -> Result<SettingEntry, ChangeError> {
        self.set_server_token(FIRST_PLEX_SERVER, value, by)?;
        Ok(self.entry_for(key).expect("a known setting"))
    }

    /// Keep `value` as Plex server `server`'s token, or forget it, and
    /// make the catalog use it.
    pub(crate) fn set_server_token(
        &self,
        server: &str,
        value: Option<&str>,
        by: &str,
    ) -> Result<(), ChangeError> {
        let _writing = self.writing.lock();
        let layers = self.layers();
        layers.server_token_changeable(server)?;
        let secret = plex_token_secret(server);
        match value {
            Some(token) => self
                .secrets
                .set(&secret, token)
                .map_err(ChangeError::Write)?,
            None => {
                self.secrets.remove(&secret).map_err(ChangeError::Write)?;
            }
        }
        // Never the value: only that it changed, and who changed it.
        tracing::info!(by, server, set = value.is_some(), "secret setting changed");
        self.apply(&layers, Plex::Remake, &secret, by);
        Ok(())
    }

    /// Set (or, with `None`, unset) several keys in one change, checked
    /// together: what one admin action changes at once (P3-11).
    pub(crate) fn change_keys(
        &self,
        what: &str,
        by: &str,
        keys: Vec<(&str, Option<Value>)>,
    ) -> Result<(), ChangeError> {
        self.change(what, by, |layers| Ok(layers.with_changes(keys)?))?;
        tracing::info!(by, what, "settings changed");
        Ok(())
    }

    /// [`RuntimeSettings::change_keys`] for an edit worked out from
    /// `base` (read, then Plex asked, then the new list made): refused
    /// with [`ChangeError::Conflict`] if a key it sets is no longer what
    /// `base` had, rather than writing over another edit (M25).
    pub(crate) fn change_keys_from(
        &self,
        base: &Layers,
        what: &str,
        by: &str,
        keys: Vec<(&str, Option<Value>)>,
    ) -> Result<(), ChangeError> {
        let was = base.merged();
        self.change(what, by, |layers| {
            let now = layers.merged();
            if let Some((key, _)) = keys
                .iter()
                .find(|(key, _)| tree::get(&now, key) != tree::get(&was, key))
            {
                return Err(ChangeError::Conflict {
                    key: (*key).to_owned(),
                });
            }
            Ok(layers.with_changes(keys)?)
        })?;
        tracing::info!(by, what, "settings changed");
        Ok(())
    }

    /// The catalog the server serves from.
    pub fn catalog(&self) -> &Arc<LiveCatalog> {
        &self.live.catalog
    }

    /// Bring the live settings to what `layers` say. Every one is set
    /// again (it is cheap), but the Plex catalog only when its settings
    /// or token changed: a new one starts without the old one's
    /// connections.
    fn apply(&self, layers: &Layers, plex: Plex, key: &str, by: &str) {
        // Checked when the change was made; a failure here is a file
        // changed under the server, and only logged.
        let config = match layers.config() {
            Ok(config) => config,
            Err(e) => {
                tracing::error!(error = %e, key, by, "the changed settings do not load");
                return;
            }
        };
        let live = &self.live;
        if let Some(name) = &live.name {
            name.set(config.server_name());
        }
        live.cookies.set_insecure(config.admin.insecure_cookies());
        if let (Some(metrics), Ok(from)) = (&live.metrics, config.admin.metrics_from()) {
            metrics.access().set(config.admin.metrics(), from);
        }
        if let (Some(logs), Ok(lines)) = (&live.logs, config.logs.buffer_lines()) {
            logs.set_capacity(lines);
        }
        if let (Some(telemetry), Ok(otlp)) = (&live.telemetry, config.telemetry.otlp.settings()) {
            telemetry.apply(otlp);
        }
        if let (Some(watch), Ok(rules)) = (&live.watch, config.watch.rules()) {
            let mut now = watch.rules();
            now.started_after = rules.started_after;
            now.watched_fraction = rules.watched_fraction;
            watch.set_rules(now);
        }
        if let (Some(jobs), Ok(limits)) = (&live.jobs, config.transcode.limits(&self.state_dir)) {
            jobs.set_limits(limits);
        }
        if let (Some(uplink), Ok(settings)) = (&live.uplink, config.network.uplink()) {
            uplink.set_settings(settings);
        }
        if let (Some(previews), Ok(settings)) = (&live.previews, config.previews.settings()) {
            previews.set_settings(settings);
        }
        if let Some(oidc) = &live.oidc {
            oidc.set_providers(&config.auth.oidc);
        }
        let made = match plex {
            Plex::Same => return,
            Plex::Made(made) => made,
            Plex::Remake => match plex_catalog(&config, layers, &self.secrets) {
                Ok(made) => made,
                Err(e) => {
                    tracing::error!(error = %e, "cannot make the Plex catalog");
                    return;
                }
            },
        };
        let set_up = made.is_some();
        live.catalog.replace(made.map(Arc::new));
        tracing::info!(set_up, "the catalog follows the new settings");
    }

    /// The Plex token the server has now: the one set at run time, else
    /// the config's token file's.
    pub fn plex_token(&self) -> Option<String> {
        let layers = self.layers();
        let config = layers.config().ok()?;
        let token = match &config.plex {
            Some(plex) => layers.plex_token(plex, &self.secrets),
            None => self.secrets.get(PLEX_TOKEN_SECRET).map_err(|source| {
                crate::config::ConfigError::Token {
                    path: self.secrets.path(PLEX_TOKEN_SECRET),
                    source,
                }
            }),
        };
        match token {
            Ok(token) => token,
            Err(e) => {
                // Read as no token, but not quietly.
                tracing::warn!(error = %e, "cannot read the Plex token");
                None
            }
        }
    }

    /// The UDP port Plex's discovery searches from now
    /// (`network.plex_discovery_port`): read at each search, so a change
    /// applies to the next one.
    pub fn plex_discovery_port(&self) -> u16 {
        self.layers()
            .config()
            .map_or(spoolframe_plex::gdm::PORT, |c| {
                c.network.plex_discovery_port()
            })
    }

    /// This installation's id at plex.tv, which a PIN is bound to: made
    /// once, and kept beside the secrets.
    pub fn plex_client_id(&self) -> std::io::Result<String> {
        self.secrets.plex_client_id()
    }

    /// The wizard's progress: the confirmed steps, and whether it is over.
    pub fn setup(&self) -> (Vec<String>, bool) {
        self.layers().setup()
    }

    /// Step `step` confirmed, and with `finish` the wizard over, as admin
    /// `by`. Written to the runtime layer, so a wizard left half-way
    /// resumes after a restart.
    pub fn setup_progress(
        &self,
        step: Option<&str>,
        finish: bool,
        by: &str,
    ) -> Result<(Vec<String>, bool), ChangeError> {
        let mut layers = self.layers.lock();
        let (mut done, finished) = layers.setup();
        if let Some(step) = step
            && !done.iter().any(|d| d == step)
        {
            done.push(step.to_owned());
        }
        let changed = layers.with_setup(&done, finished || finish);
        changed.write_runtime().map_err(ChangeError::Write)?;
        *layers = changed;
        tracing::info!(by, step, finish, "setup wizard step confirmed");
        Ok(layers.setup())
    }

    /// Ask for a restart: the server stops, then its supervisor starts it
    /// again, or it runs itself again in place (`RestartMode`).
    pub fn restart(&self, by: &str) {
        tracing::warn!(by, mode = ?self.mode, "restart asked for");
        self.restart.notify_one();
    }

    /// How a restart brings the server back.
    pub fn restart_mode(&self) -> RestartMode {
        self.mode
    }

    /// Done once a restart is asked for.
    pub async fn restart_requested(&self) {
        self.restart.notified().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn settings(file: Option<&str>) -> (RuntimeSettings, spoolframe_test_dir::TestDir) {
        let dir = spoolframe_test_dir::TestDir::new("spoolframe-runtime").await;
        let file_path = dir.as_ref().join("server.ucl");
        let layers = Layers::from_texts(
            file.map(|t| (file_path.as_path(), t)),
            None,
            dir.as_ref().join("runtime.ucl"),
        )
        .unwrap();
        let live = Live {
            name: Some(ServerName::new("Spoolframe")),
            ..Live::default()
        };
        (
            RuntimeSettings::new(layers, dir.as_ref(), live, RestartMode::InPlace),
            dir,
        )
    }

    #[tokio::test]
    async fn a_live_setting_reaches_the_running_server_at_once() {
        let (settings, _dir) = settings(None).await;
        let entry = settings
            .set("name", &serde_json::json!("Den"), "root")
            .unwrap();
        assert_eq!(entry.value, Some(serde_json::json!("Den")));
        assert_eq!(entry.layer, Some(SettingLayer::Runtime));
        assert_eq!(settings.live.name.as_ref().unwrap().get(), "Den");
        settings
            .set("admin.insecure_cookies", &serde_json::json!(true), "root")
            .unwrap();
        assert!(settings.live.cookies.insecure());
        settings.unset("admin.insecure_cookies", "root").unwrap();
        assert!(!settings.live.cookies.insecure());
    }

    #[tokio::test]
    async fn watch_rules_change_for_the_next_report_and_keep_what_the_config_does_not_set() {
        use crate::watch::WatchRules;
        let dir = spoolframe_test_dir::TestDir::new("spoolframe-runtime-watch").await;
        let store = crate::store::Store::open(dir.as_ref()).await.unwrap();
        let rules = WatchRules {
            throttle: std::time::Duration::from_millis(1),
            ..WatchRules::default()
        };
        let watch = Arc::new(Watch::new(
            store,
            Arc::new(LiveCatalog::default()),
            Arc::new(spoolframe_media::Prober::new(
                spoolframe_media::sandbox::Tool::unconfined("ffprobe"),
                spoolframe_media::Prober::DEFAULT_TIMEOUT,
            )),
            rules,
        ));
        let layers = Layers::from_texts(None, None, dir.as_ref().join("runtime.ucl")).unwrap();
        let live = Live {
            watch: Some(watch.clone()),
            ..Live::default()
        };
        let settings = RuntimeSettings::new(layers, dir.as_ref(), live, RestartMode::InPlace);
        settings
            .set("watch.watched_percent", &serde_json::json!(80), "root")
            .unwrap();
        assert_eq!(watch.rules().watched_fraction, 0.8);
        assert_eq!(watch.rules().throttle, std::time::Duration::from_millis(1));
    }

    #[tokio::test]
    async fn previews_settings_reach_the_running_server_at_once() {
        use crate::previews::{PreviewSources, When};
        let (mut settings, dir) = settings(None).await;
        let previews = Previews::start(
            &dir.as_ref().join("previews"),
            crate::config::PreviewsConfig::default().settings().unwrap(),
            PreviewSources {
                catalog: Arc::new(LiveCatalog::default()),
                prober: Arc::new(spoolframe_media::Prober::new(
                    spoolframe_media::sandbox::Tool::unconfined("ffprobe"),
                    spoolframe_media::Prober::DEFAULT_TIMEOUT,
                )),
                transcoding: Arc::new(crate::transcode::Transcoding::new(
                    spoolframe_media::sandbox::Tool::unconfined("ffmpeg"),
                    Vec::new(),
                    dir.as_ref().join("encoder-test"),
                )),
                ffmpeg: spoolframe_media::sandbox::Tool::unconfined("ffmpeg"),
                encoding: Default::default(),
            },
        )
        .unwrap();
        settings.live.previews = Some(previews.clone());
        settings
            .set("previews.when", &serde_json::json!("never"), "root")
            .unwrap();
        settings
            .set("previews.interval_secs", &serde_json::json!(4), "root")
            .unwrap();
        settings
            .set("previews.workers", &serde_json::json!(3), "root")
            .unwrap();
        let now = previews.settings();
        assert_eq!(now.when, When::Never);
        assert_eq!(now.interval, std::time::Duration::from_secs(4));
        assert_eq!(now.workers, 3);
    }

    /// A token file the server cannot read refuses the change that names
    /// it, as it would refuse the start, and nothing is saved: the setting
    /// keeps saying what the catalog uses. Review 2026-10-11 M24.
    #[tokio::test]
    async fn an_unreadable_plex_token_file_is_refused_not_saved() {
        let (settings, dir) = settings(Some(r#"plex { url = "http://127.0.0.1:32400"; }"#)).await;
        let missing = dir.as_ref().join("no-such-token");
        let refused = settings
            .set(
                "plex.token_file",
                &serde_json::json!(missing.display().to_string()),
                "root",
            )
            .unwrap_err();
        assert!(refused.to_string().contains("token_file"), "{refused}");
        assert!(!dir.as_ref().join("runtime.ucl").exists());
        let merged = settings.layers().merged();
        assert_eq!(tree::get(&merged, "plex.token_file"), None);
    }

    /// An edit worked out from settings that changed before it was saved
    /// is refused, not written over the other: two admins renaming
    /// libraries at once lost one rename. Review 2026-10-11 M25.
    #[tokio::test]
    async fn an_edit_from_stale_settings_is_refused() {
        let (settings, _dir) = settings(None).await;
        let base = settings.layers();
        settings
            .change_keys(
                "name",
                "a",
                vec![("name", Some(tree::Value::String("First".into())))],
            )
            .unwrap();
        let stale = settings.change_keys_from(
            &base,
            "name",
            "b",
            vec![("name", Some(tree::Value::String("Second".into())))],
        );
        assert!(
            matches!(stale, Err(ChangeError::Conflict { .. })),
            "{stale:?}"
        );
        assert_eq!(
            tree::get(&settings.layers().merged(), "name"),
            Some(&tree::Value::String("First".into()))
        );
        // From the settings as they are now, it is saved.
        let base = settings.layers();
        settings
            .change_keys_from(
                &base,
                "name",
                "b",
                vec![("name", Some(tree::Value::String("Second".into())))],
            )
            .unwrap();
    }

    #[tokio::test]
    async fn sign_in_providers_follow_their_setting_at_once() {
        let (mut settings, _dir) = settings(Some(
            r#"admin { public_url = "https://media.example.org"; }"#,
        ))
        .await;
        let oidc = Arc::new(crate::oidc::Oidc::new(
            Some("https://media.example.org"),
            None,
        ));
        settings.live.oidc = Some(oidc.clone());
        let provider = serde_json::json!([{
            "name": "Pocket ID",
            "issuer": "https://id.example.org",
            "client_id": "spoolframe",
        }]);
        let entry = settings.set("auth.oidc", &provider, "root").unwrap();
        assert_eq!(entry.apply, SettingApply::Live);
        assert_eq!(oidc.names(), ["Pocket ID"]);
        settings.unset("auth.oidc", "root").unwrap();
        assert!(oidc.names().is_empty());
        // One the server would refuse is refused here, saying why.
        let refused = settings
            .set(
                "auth.oidc",
                &serde_json::json!([{ "name": "A", "issuer": "http://id.example.org", "client_id": "x" }]),
                "root",
            )
            .unwrap_err();
        assert!(
            refused.to_string().contains("not an https URL"),
            "{refused}"
        );
    }

    #[tokio::test]
    async fn the_metrics_endpoint_follows_its_settings_at_once() {
        let (mut settings, _dir) = settings(None).await;
        let metrics = Arc::new(crate::metrics::Metrics::default());
        settings.live.metrics = Some(metrics.clone());
        let entry = settings
            .set("admin.metrics", &serde_json::json!(true), "root")
            .unwrap();
        assert_eq!(entry.apply, SettingApply::Live);
        assert!(metrics.access().enabled());
        settings
            .set(
                "admin.metrics_from",
                &serde_json::json!("10.0.0.0/8"),
                "root",
            )
            .unwrap();
        assert!(metrics.access().allows("10.9.8.7".parse().unwrap()));
        assert!(!metrics.access().allows("127.0.0.1".parse().unwrap()));
        settings.unset("admin.metrics", "root").unwrap();
        assert!(!metrics.access().enabled());
    }

    /// Marks the copy of a test that [`alone`] runs.
    const ALONE: &str = "SPOOLFRAME_TEST_ALONE";

    /// Run `test`, a test of this module, again in a process where no
    /// other test runs, and assert it passed there. `true` in that copy,
    /// which runs the body; `false` here.
    ///
    /// For a test that reads the log through a subscriber of its own:
    /// tracing decides once per call site, for every thread, whether a
    /// line there is wanted at all. While that subscriber is the only
    /// one, a call site another test's thread reaches first is asked of
    /// that thread's subscriber, which is none, and stays off for this
    /// thread too.
    fn alone(test: &str) -> bool {
        if std::env::var_os(ALONE).is_some() {
            return true;
        }
        let (_, module) = module_path!().split_once("::").unwrap();
        let test = format!("{module}::{test}");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &test, "--nocapture"])
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

    #[tokio::test]
    async fn otlp_headers_are_never_logged_shown_or_returned() {
        if !alone("otlp_headers_are_never_logged_shown_or_returned") {
            return;
        }
        let (settings, _dir) = settings(None).await;
        let (logged, _guard) = {
            let ring = Arc::new(crate::logs::LogRing::default());
            use tracing_subscriber::layer::SubscriberExt as _;
            let subscriber =
                tracing_subscriber::registry().with(crate::logs::RingLayer(ring.clone()));
            (ring, tracing::subscriber::set_default(subscriber))
        };
        settings
            .set(
                "telemetry.otlp.headers",
                &serde_json::json!({ "authorization": "Bearer s3cr3t" }),
                "root",
            )
            .unwrap();
        let page = logged.page(&Default::default());
        assert!(
            page.lines
                .iter()
                .any(|l| l.text.contains("telemetry.otlp.headers")),
            "{page:?}"
        );
        assert!(
            page.lines.iter().all(|l| !l.text.contains("s3cr3t")),
            "{page:?}"
        );
        // Nor returned, nor shown: only that it is set.
        let entry = settings.entry_for("telemetry.otlp.headers").unwrap();
        assert!(entry.set && entry.secret && entry.value.is_none());
        let all = serde_json::to_string(&settings.settings()).unwrap();
        assert!(!all.contains("s3cr3t"), "{all}");
        let shown = settings.layers().show(settings.secrets());
        assert!(shown.contains("telemetry.otlp.headers = (set);"), "{shown}");
        assert!(!shown.contains("s3cr3t"), "{shown}");
    }

    #[tokio::test]
    async fn the_log_buffer_follows_its_size_at_once() {
        let (mut settings, _dir) = settings(None).await;
        let logs = Arc::new(crate::logs::LogRing::default());
        settings.live.logs = Some(logs.clone());
        settings
            .set("logs.buffer_lines", &serde_json::json!(50), "root")
            .unwrap();
        assert_eq!(logs.capacity(), 50);
        settings.unset("logs.buffer_lines", "root").unwrap();
        assert_eq!(logs.capacity(), 5000);
    }

    #[tokio::test]
    async fn a_restart_setting_is_saved_and_flagged() {
        let (settings, dir) = settings(None).await;
        let entry = settings
            .set("network.port", &serde_json::json!(7440), "root")
            .unwrap();
        assert_eq!(entry.apply, SettingApply::Restart);
        assert!(entry.restart_pending);
        // Before the restart: no port was set at start (a free one is
        // picked); after it, 7440.
        assert_eq!(entry.running, None);
        assert_eq!(entry.value, Some(serde_json::json!(7440)));
        let saved = std::fs::read_to_string(dir.as_ref().join("runtime.ucl")).unwrap();
        assert!(saved.contains("port = 7440;"), "{saved}");
        let entry = settings
            .set("network.mdns", &serde_json::json!(false), "root")
            .unwrap();
        assert_eq!(entry.running, Some(serde_json::json!(true)));
        assert_eq!(entry.value, Some(serde_json::json!(false)));
        // Back to what the server started with: nothing waits.
        let entry = settings.unset("network.port", "root").unwrap();
        assert!(!entry.restart_pending);
        assert_eq!(entry.running, None);
    }

    /// Without a supervisor a restart is still asked for: the server
    /// starts itself again in place.
    #[tokio::test]
    async fn an_unsupervised_restart_is_asked_for() {
        let (settings, _dir) = settings(None).await;
        settings.restart("root");
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            settings.restart_requested(),
        )
        .await
        .expect("the restart is asked for");
    }

    #[tokio::test]
    async fn plex_comes_and_goes_with_its_settings() {
        let (settings, _dir) = settings(None).await;
        assert!(settings.live.catalog.plex().is_none());
        settings
            .set("plex.url", &serde_json::json!("http://127.0.0.1:9"), "root")
            .unwrap();
        let plex = settings.live.catalog.plex().expect("set up");
        assert_eq!(plex.base_url(), "http://127.0.0.1:9");
        settings.unset("plex.url", "root").unwrap();
        assert!(settings.live.catalog.plex().is_none());
    }

    #[tokio::test]
    async fn the_token_is_kept_as_a_secret_and_never_shown() {
        let (settings, dir) = settings(Some(r#"plex { url = "http://127.0.0.1:9"; }"#)).await;
        let entry = settings
            .set(PLEX_TOKEN, &serde_json::json!("s3cr3t-t0ken"), "root")
            .unwrap();
        assert!(entry.secret && entry.set);
        assert_eq!(entry.value, None);
        assert_eq!(
            settings
                .secrets()
                .get(PLEX_TOKEN_SECRET)
                .unwrap()
                .as_deref(),
            Some("s3cr3t-t0ken")
        );
        // Not in the runtime layer, nor in what the API answers.
        assert!(!dir.as_ref().join("runtime.ucl").exists());
        let all = serde_json::to_string(&settings.settings()).unwrap();
        assert!(!all.contains("s3cr3t"), "{all}");
        assert!(matches!(
            settings.set(PLEX_TOKEN, &serde_json::json!(""), "root"),
            Err(ChangeError::Secret { .. })
        ));
        let entry = settings.unset(PLEX_TOKEN, "root").unwrap();
        assert!(!entry.set);
    }

    #[tokio::test]
    async fn a_locked_key_shows_its_lock_and_is_not_changed() {
        let (settings, _dir) = settings(Some(r#"locked = true; name = "Nimble";"#)).await;
        let entry = settings.entry_for("name").unwrap();
        assert!(entry.locked);
        assert_eq!(entry.layer, Some(SettingLayer::File));
        let refused = settings
            .set("name", &serde_json::json!("Den"), "root")
            .unwrap_err();
        assert!(
            matches!(refused, ChangeError::Setting(SettingError::Locked { .. })),
            "{refused}"
        );
        assert_eq!(settings.live.name.as_ref().unwrap().get(), "Spoolframe");
    }

    #[tokio::test]
    async fn an_unlocked_file_value_is_overridden_and_named() {
        let (settings, _dir) = settings(Some(r#"name = "Nimble";"#)).await;
        let entry = settings
            .set("name", &serde_json::json!("Den"), "root")
            .unwrap();
        assert_eq!(entry.overrides, Some(serde_json::json!("Nimble")));
        assert!(!entry.locked);
    }
}
