//! The config read again while the server runs (PX-33): on SIGHUP, or
//! when an admin asks (`configReload`, `spoolframe-server config
//! reload`).
//!
//! The layers are read and checked as at start. A bad one changes
//! nothing: the server keeps the config it had, and says what is wrong
//! and where. Otherwise the live settings apply through the path a change
//! in the web UI takes (`RuntimeSettings::apply`), the restart settings
//! wait as such a change's do, the config warnings are logged again and
//! the encoders are detected again.

use std::sync::Arc;

use crate::config::layers::Layers;
use crate::config::settings::{Apply, SETTINGS};
use crate::config::tree::{self, Table};
use crate::config::{ConfigError, ServerConfig};

use super::{Plex, RuntimeSettings, plex_catalog};

/// What a reload changed: the settings by key, in `config show`'s order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reloaded {
    /// Live settings now in effect; and a restart setting set back to
    /// what the server runs with, which waits for nothing any more.
    pub applied: Vec<String>,
    /// Restart settings that apply at the next start.
    pub after_restart: Vec<String>,
}

impl Reloaded {
    /// What changed from `before` to `after`, the server having started
    /// with `started`. Secrets are not in the layers, so never here.
    pub fn between(before: &Table, after: &Table, started: &Table) -> Self {
        let mut reloaded = Self::default();
        for setting in SETTINGS.iter().filter(|s| !s.secret) {
            let now = tree::get(after, setting.key);
            if tree::get(before, setting.key) == now {
                continue;
            }
            let waits = setting.apply == Apply::Restart && now != tree::get(started, setting.key);
            let list = if waits {
                &mut reloaded.after_restart
            } else {
                &mut reloaded.applied
            };
            list.push(setting.key.to_owned());
        }
        reloaded
    }

    /// The summary line's words after `config reloaded: `.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.applied.is_empty() {
            parts.push(format!("applied {}", self.applied.join(", ")));
        }
        if !self.after_restart.is_empty() {
            parts.push(format!(
                "after a restart: {}",
                self.after_restart.join(", ")
            ));
        }
        if parts.is_empty() {
            return "nothing changed".to_owned();
        }
        parts.push("unchanged otherwise".to_owned());
        parts.join("; ")
    }
}

/// The warnings a config earns, logged at start and again on each reload:
/// every place that warns about what a config says goes through here.
pub fn config_warnings(config: &ServerConfig) {
    for key in config.experimental.on() {
        tracing::warn!(
            setting = key,
            "an experimental setting is on: it may change or go away in any release"
        );
    }
}

/// What the layers earn, logged at start and on each reload: the
/// runtime settings a locked file overrides, and each retired key a
/// layer still has.
pub fn shadowed_warnings(layers: &Layers) {
    for key in layers.shadowed() {
        tracing::warn!(
            key,
            "the locked config file sets this; the runtime settings' value is not used"
        );
    }
    for retired in layers.retired() {
        tracing::warn!("{retired}");
    }
}

impl RuntimeSettings {
    /// Read the config again and apply it, as `trigger` (`SIGHUP`, or
    /// the admin who asked) says in the log. One reload at a time; a
    /// settings change waits for it, and it for a settings change.
    pub async fn reload(&self, trigger: &str) -> Result<Reloaded, ConfigError> {
        let _one = self.reloading.lock().await;
        let reloaded = match self.reload_layers(trigger) {
            Ok((reloaded, layers, config)) => {
                tracing::info!("{trigger}: config reloaded: {}", reloaded.summary());
                shadowed_warnings(&layers);
                config_warnings(&config);
                Ok(reloaded)
            }
            Err(e) => {
                tracing::error!(
                    "{trigger}: the config was not reloaded, the server keeps the one it had: {e}"
                );
                Err(e)
            }
        };
        // As SIGHUP always did (P2-02): a GPU may have come or gone,
        // whatever the files say.
        if let Some(transcoding) = &self.live.transcoding {
            transcoding.detect().await;
        }
        reloaded
    }

    /// Read and check the layers, and, if they load, make them the
    /// server's and apply them; all under the settings lock.
    fn reload_layers(&self, by: &str) -> Result<(Reloaded, Layers, ServerConfig), ConfigError> {
        // The files read with the settings lock let go (M26); one reload
        // or change at a time.
        let _writing = self.writing.lock();
        let layers = self.layers();
        let fresh = layers.reread()?;
        let config = fresh.config()?;
        let (before, after) = (layers.merged(), fresh.merged());
        let reloaded = Reloaded::between(&before, &after, &self.started);
        let plex_changed = ["plex", "libraries"]
            .iter()
            .any(|key| tree::get(&before, key) != tree::get(&after, key));
        // A catalog the files cannot make refuses the reload, as it does
        // the start: the server goes on as it was (review 2026-10-11 M24).
        let plex = if plex_changed {
            Plex::Made(plex_catalog(&config, &fresh, &self.secrets)?)
        } else {
            Plex::Same
        };
        *self.layers.lock() = fresh.clone();
        if !reloaded.applied.is_empty() {
            self.apply(&fresh, plex, "config", by);
        }
        Ok((reloaded, fresh, config))
    }
}

/// SIGHUP, listened for from now: before this, it would end the process.
/// `None`, said in the log, when it cannot be.
pub fn listen() -> Option<tokio::signal::unix::Signal> {
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
        Ok(hangup) => Some(hangup),
        Err(e) => {
            tracing::warn!(error = %e, "cannot listen for SIGHUP: the config is read only at start");
            None
        }
    }
}

/// Reload `settings` on each SIGHUP `hangup` gets, in the background.
/// Those that come while a reload runs make one more.
pub fn on_hangup(mut hangup: tokio::signal::unix::Signal, settings: Arc<RuntimeSettings>) {
    tokio::spawn(async move {
        while hangup.recv().await.is_some() {
            // Logged by the reload, either way.
            let _ = settings.reload("SIGHUP").await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(text: &str) -> Table {
        tree::parse(text).unwrap()
    }

    #[test]
    fn the_summary_names_what_was_applied_and_what_waits() {
        let reloaded = Reloaded {
            applied: vec!["name".into(), "transcode.max_jobs".into()],
            after_restart: vec!["network.port".into()],
        };
        assert_eq!(
            reloaded.summary(),
            "applied name, transcode.max_jobs; after a restart: network.port; unchanged otherwise"
        );
    }

    #[test]
    fn the_summary_says_when_nothing_changed() {
        assert_eq!(Reloaded::default().summary(), "nothing changed");
    }

    #[test]
    fn the_summary_leaves_out_an_empty_part() {
        let only_restart = Reloaded {
            applied: vec![],
            after_restart: vec!["admin.listen".into()],
        };
        assert_eq!(
            only_restart.summary(),
            "after a restart: admin.listen; unchanged otherwise"
        );
    }

    #[test]
    fn changes_are_sorted_into_live_and_restart_in_settings_order() {
        let started = table(r#"name = "A"; network { mdns = true; port = 1; }"#);
        let before = started.clone();
        let after =
            table(r#"name = "B"; network { mdns = false; port = 1; } transcode { max_jobs = 3; }"#);
        assert_eq!(
            Reloaded::between(&before, &after, &started),
            Reloaded {
                applied: vec!["name".into(), "transcode.max_jobs".into()],
                after_restart: vec!["network.mdns".into()],
            }
        );
    }

    #[test]
    fn a_restart_setting_set_back_to_what_runs_waits_for_nothing() {
        let started = table("network { mdns = true; }");
        let before = table("network { mdns = false; }");
        assert_eq!(
            Reloaded::between(&before, &started, &started),
            Reloaded {
                applied: vec!["network.mdns".into()],
                after_restart: vec![],
            }
        );
    }

    /// The endpoint's limits are read only at start: a reload that
    /// changes one waits for a restart instead of "nothing changed"
    /// (review 2026-10-11 L37).
    #[test]
    fn the_endpoint_s_limits_wait_for_a_restart() {
        let started = table("network { port = 1; }");
        let after = table(
            "network { port = 1; max_stranger_connections = 5; \
             first_frame_timeout_secs = 3; stalled_read_secs = 9; }",
        );
        assert_eq!(
            Reloaded::between(&started, &after, &started),
            Reloaded {
                applied: vec![],
                after_restart: vec![
                    "network.max_stranger_connections".into(),
                    "network.first_frame_timeout_secs".into(),
                    "network.stalled_read_secs".into(),
                ],
            }
        );
    }

    #[test]
    fn the_same_tables_change_nothing() {
        let t = table(r#"name = "A";"#);
        assert_eq!(Reloaded::between(&t, &t, &t), Reloaded::default());
    }

    use crate::config::layers::Locations;
    use crate::network::ServerName;
    use spoolframe_admin_api::RestartMode;
    use std::path::PathBuf;

    /// Runtime settings over the file `file`, written in a directory of
    /// the test's own, which is the state directory too.
    async fn started(
        name: &str,
        file: &str,
    ) -> (RuntimeSettings, spoolframe_test_dir::TestDir, PathBuf) {
        let dir = spoolframe_test_dir::TestDir::new(name).await;
        let path = dir.as_ref().join("server.ucl");
        std::fs::write(&path, file).unwrap();
        let layers = Layers::load(&Locations {
            state_dir: dir.as_ref().to_owned(),
            config: Some(path.clone()),
        })
        .unwrap();
        let name = ServerName::new(layers.config().unwrap().server_name());
        let live = super::super::Live {
            name: Some(name),
            ..Default::default()
        };
        let settings = RuntimeSettings::new(layers, dir.as_ref(), live, RestartMode::InPlace);
        (settings, dir, path)
    }

    #[tokio::test]
    async fn a_live_setting_changed_in_the_file_applies_on_reload() {
        let (settings, _dir, file) = started("spoolframe-reload-live", r#"name = "Nimble";"#).await;
        std::fs::write(&file, r#"name = "Den";"#).unwrap();
        let reloaded = settings.reload("test").await.unwrap();
        assert_eq!(reloaded.applied, ["name"]);
        assert_eq!(settings.live.name.as_ref().unwrap().get(), "Den");
        let entry = settings.entry_for("name").unwrap();
        assert_eq!(entry.value, Some(serde_json::json!("Den")));
        assert!(!entry.restart_pending);
    }

    #[tokio::test]
    async fn a_restart_setting_changed_in_the_file_waits_and_the_old_value_runs() {
        let (settings, _dir, file) =
            started("spoolframe-reload-restart", "network { mdns = false; }").await;
        std::fs::write(&file, "network { mdns = true; }").unwrap();
        let reloaded = settings.reload("test").await.unwrap();
        assert_eq!(reloaded.after_restart, ["network.mdns"]);
        assert!(reloaded.applied.is_empty());
        let entry = settings.entry_for("network.mdns").unwrap();
        assert!(entry.restart_pending);
        assert_eq!(entry.value, Some(serde_json::json!(true)));
        assert_eq!(entry.running, Some(serde_json::json!(false)));
    }

    #[tokio::test]
    async fn a_file_that_does_not_load_changes_nothing_and_says_where() {
        let (settings, _dir, file) = started("spoolframe-reload-bad", r#"name = "Nimble";"#).await;
        for bad in [
            r#"name = "Den"; nmae = "Den";"#,
            r#"name = "Den"; transcode { max_jobs = 0; }"#,
            r#"name = "${SF_ENV_SPOOLFRAME_RELOAD_TEST_NEVER_SET}";"#,
            r#"name = "Den"; network {"#,
        ] {
            std::fs::write(&file, bad).unwrap();
            let refused = settings.reload("test").await.unwrap_err().to_string();
            assert!(
                refused.contains(&file.display().to_string())
                    || refused.contains("SF_ENV_SPOOLFRAME_RELOAD_TEST_NEVER_SET"),
                "{bad}: {refused}"
            );
            assert_eq!(settings.live.name.as_ref().unwrap().get(), "Nimble");
            assert_eq!(
                settings.entry_for("name").unwrap().value,
                Some(serde_json::json!("Nimble"))
            );
        }
        std::fs::remove_file(&file).unwrap();
        let refused = settings.reload("test").await.unwrap_err().to_string();
        assert!(refused.contains("cannot read config"), "{refused}");
        assert_eq!(settings.live.name.as_ref().unwrap().get(), "Nimble");
    }

    #[tokio::test]
    async fn an_unchanged_file_changes_nothing() {
        let (settings, _dir, _file) =
            started("spoolframe-reload-same", r#"name = "Nimble";"#).await;
        assert_eq!(settings.reload("test").await.unwrap(), Reloaded::default());
    }

    #[tokio::test]
    async fn a_runtime_value_still_wins_over_a_change_in_the_file() {
        let (settings, _dir, file) =
            started("spoolframe-reload-runtime-wins", r#"name = "Nimble";"#).await;
        settings
            .set("name", &serde_json::json!("Den"), "root")
            .unwrap();
        std::fs::write(&file, r#"name = "Attic";"#).unwrap();
        assert_eq!(settings.reload("test").await.unwrap(), Reloaded::default());
        assert_eq!(settings.live.name.as_ref().unwrap().get(), "Den");
        let entry = settings.entry_for("name").unwrap();
        assert_eq!(entry.value, Some(serde_json::json!("Den")));
        assert_eq!(entry.overrides, Some(serde_json::json!("Attic")));
        // Locked, the file wins.
        std::fs::write(&file, r#"locked = true; name = "Attic";"#).unwrap();
        assert_eq!(settings.reload("test").await.unwrap().applied, ["name"]);
        assert_eq!(settings.live.name.as_ref().unwrap().get(), "Attic");
    }

    /// A reload and a settings change take the same lock: neither sees the
    /// other half done.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reloads_and_changes_do_not_interleave() {
        let (settings, _dir, file) =
            started("spoolframe-reload-race", "transcode { max_jobs = 2; }").await;
        let settings = Arc::new(settings);
        std::fs::write(&file, "transcode { max_jobs = 3; }").unwrap();
        let mut tasks = Vec::new();
        for i in 0..20 {
            let settings = settings.clone();
            tasks.push(tokio::spawn(async move {
                if i % 2 == 0 {
                    settings.reload("test").await.map(|_| ()).unwrap();
                } else {
                    settings
                        .set("name", &serde_json::json!(format!("N{i}")), "root")
                        .map(|_| ())
                        .unwrap();
                }
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        // The runtime layer the changes wrote is what the server has.
        let saved = std::fs::read_to_string(settings.layers().runtime_path()).unwrap();
        let name = settings.entry_for("name").unwrap().value.unwrap();
        assert!(
            saved.contains(&format!("name = {name};")),
            "{saved} / {name}"
        );
        assert_eq!(
            settings.entry_for("transcode.max_jobs").unwrap().value,
            Some(serde_json::json!(3))
        );
    }

    /// A retired key (PGPU-05's `transcode.hardware`) is warned of again
    /// on each reload, as at start, through the same hook: once a reload.
    #[tokio::test]
    async fn a_retired_gpu_key_is_warned_of_on_each_reload() {
        use crate::logs::{LogRing, RingLayer};
        use spoolframe_admin_api::{LogLevel, LogQuery};
        use tracing_subscriber::layer::SubscriberExt as _;
        let (settings, _dir, _file) = started(
            "spoolframe-reload-retired",
            r#"transcode { hardware = "auto"; }"#,
        )
        .await;
        let ring = Arc::new(LogRing::default());
        let _logged = tracing::subscriber::set_default(
            tracing_subscriber::registry().with(RingLayer(ring.clone())),
        );
        let warned = || {
            ring.page(&LogQuery {
                level: Some(LogLevel::Warn),
                text: Some("transcode.hardware is no longer read".into()),
                ..LogQuery::default()
            })
            .lines
            .len()
        };
        settings.reload("SIGHUP").await.unwrap();
        assert_eq!(warned(), 1);
        settings.reload("SIGHUP").await.unwrap();
        assert_eq!(warned(), 2);
    }
}
