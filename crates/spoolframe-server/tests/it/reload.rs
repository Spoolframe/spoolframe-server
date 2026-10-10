//! The config read again while the server runs (PX-33), through the
//! admin API's `configReload`: what SIGHUP does. `cli` sends a real
//! SIGHUP to a real server.

use serde_json::json;
use spoolframe_admin_api::{AdminApiClient, ConfigReload, Settings, error_code};

use crate::settings::{pms, served};

fn entry<'a>(settings: &'a Settings, key: &str) -> &'a spoolframe_admin_api::SettingEntry {
    settings.entries.iter().find(|e| e.key == key).unwrap()
}

/// A live setting changed in the file applies at once, with its effects:
/// a new path mapping makes the Plex catalog again, which finds the file.
#[tokio::test]
async fn a_live_setting_changed_in_the_file_takes_effect_on_reload() {
    let pms = pms().await;
    let (_addr, root, dir) = served(
        "reload-live",
        &format!(r#"plex {{ url = "{}"; }}"#, pms.uri()),
    )
    .await;
    let media = dir.as_ref().join("media");
    std::fs::create_dir_all(&media).unwrap();
    std::fs::write(media.join("a.mkv"), b"movie").unwrap();
    let before = root.plex_check_paths(None).await.unwrap().reports;
    assert_eq!((before[0].ok, before[0].no_mapping.count), (0, 1));

    std::fs::write(
        dir.as_ref().join("server.ucl"),
        format!(
            r#"plex {{ url = "{}"; path_mapping = [{{ from = "/data/movies"; to = "{}"; }}]; }}"#,
            pms.uri(),
            media.display()
        ),
    )
    .unwrap();
    let reloaded = root.config_reload().await.unwrap();
    assert_eq!(
        reloaded,
        ConfigReload {
            applied: vec!["plex.path_mapping".into()],
            after_restart: vec![],
        }
    );
    let after = root.plex_check_paths(None).await.unwrap().reports;
    assert_eq!((after[0].ok, after[0].no_mapping.count), (1, 0));
}

/// A restart setting changed in the file is pending, the value the server
/// runs with beside it.
#[tokio::test]
async fn a_restart_setting_changed_in_the_file_is_pending_with_the_old_value_running() {
    let (_addr, root, dir) = served("reload-restart", "network { mdns = false; }").await;
    std::fs::write(dir.as_ref().join("server.ucl"), "network { mdns = true; }").unwrap();
    let reloaded = root.config_reload().await.unwrap();
    assert_eq!(reloaded.after_restart, ["network.mdns"]);
    assert!(reloaded.applied.is_empty());
    let settings = root.settings().await.unwrap();
    let mdns = entry(&settings, "network.mdns");
    assert!(mdns.restart_pending);
    assert_eq!(mdns.value, Some(json!(true)));
    assert_eq!(mdns.running, Some(json!(false)));
}

/// A file the server would not start with changes nothing, and the
/// answer says what is wrong and where.
#[tokio::test]
async fn an_invalid_file_changes_nothing() {
    let (_addr, root, dir) = served("reload-invalid", r#"name = "Nimble";"#).await;
    let file = dir.as_ref().join("server.ucl");
    let before = root.settings().await.unwrap();
    std::fs::write(&file, r#"name = "Den"; transcode { max_jobz = 3; }"#).unwrap();
    let refused = root.config_reload().await.unwrap_err();
    let jsonrpsee::core::client::Error::Call(refused) = refused else {
        panic!("{refused:?}");
    };
    assert_eq!(refused.code(), error_code::INVALID_INPUT);
    assert!(
        refused.message().contains(&file.display().to_string())
            && refused.message().contains("max_jobz"),
        "{}",
        refused.message()
    );
    assert_eq!(root.settings().await.unwrap(), before);
}

#[tokio::test]
async fn a_reload_with_nothing_changed_says_so() {
    let (_addr, root, _dir) = served("reload-same", r#"name = "Nimble";"#).await;
    assert_eq!(root.config_reload().await.unwrap(), ConfigReload::default());
}

/// The runtime layer still wins over the file: a value set in the web UI
/// stays, and the file's new value is what it overrides.
#[tokio::test]
async fn a_runtime_value_still_wins_over_a_change_in_the_file() {
    let (_addr, root, dir) = served("reload-runtime-wins", r#"name = "Nimble";"#).await;
    root.setting_set("name".into(), json!("Den")).await.unwrap();
    std::fs::write(dir.as_ref().join("server.ucl"), r#"name = "Attic";"#).unwrap();
    assert_eq!(root.config_reload().await.unwrap(), ConfigReload::default());
    let settings = root.settings().await.unwrap();
    let name = entry(&settings, "name");
    assert_eq!(name.value, Some(json!("Den")));
    assert_eq!(name.overrides, Some(json!("Attic")));
}
