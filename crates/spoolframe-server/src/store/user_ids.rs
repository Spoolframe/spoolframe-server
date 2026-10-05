//! What runs around migration 0012, which gives users UUIDs (P3-20): a
//! backup of the database before it, and the secrets named by a user's id
//! renamed after it. The tables are the migration's own.

use std::path::{Path, PathBuf};

use sqlx::SqlitePool;

use super::{DB_FILE, StoreError};
use crate::config::secrets::Secrets;
use crate::plex_accounts::secret_name;

/// The migration that changes user ids.
const VERSION: i64 = 12;

/// The backup's name beside the database.
pub const BACKUP_FILE: &str = "spoolframe.db.pre-0012";

/// Copy the database to [`BACKUP_FILE`] in `state_dir` if migration 0012
/// is still to run on it; `None` when it has run, or the database is new
/// (no `users` table yet: nothing to keep). A backup already there (from
/// a start where the migration failed) is kept, and this one goes beside
/// it under the first free `.1`, `.2`, ...
pub(super) async fn back_up_if_pending(
    pool: &SqlitePool,
    state_dir: &Path,
) -> Result<Option<PathBuf>, StoreError> {
    let (has_users, applied): (bool, bool) = sqlx::query_as(
        "SELECT \
           EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'users'), \
           EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations')",
    )
    .fetch_one(pool)
    .await?;
    if !has_users {
        return Ok(None);
    }
    if applied {
        let done: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM _sqlx_migrations WHERE version = ?1)")
                .bind(VERSION)
                .fetch_one(pool)
                .await?;
        if done {
            return Ok(None);
        }
    }
    let path = free_backup_path(state_dir);
    // A consistent copy of what is committed, the WAL included, made
    // while the pool is open.
    sqlx::query("VACUUM INTO ?1")
        .bind(path.to_string_lossy().into_owned())
        .execute(pool)
        .await
        .map_err(|source| StoreError::Backup {
            path: path.clone(),
            source,
        })?;
    tracing::info!(
        backup = %path.display(),
        "database backed up before giving users UUIDs (migration 0012); \
         to undo the migration, stop the server and put the backup in place of {DB_FILE}"
    );
    Ok(Some(path))
}

fn free_backup_path(state_dir: &Path) -> PathBuf {
    let first = state_dir.join(BACKUP_FILE);
    if !first.exists() {
        return first;
    }
    (1..)
        .map(|n| state_dir.join(format!("{BACKUP_FILE}.{n}")))
        .find(|p| !p.exists())
        .expect("some suffix is free")
}

/// Rename `secrets/plex-account-<old id>` to `plex-account-<uuid>` for
/// each user migration 0012 gave a new id. Run at every start: after a
/// crash between the migration's commit and this, the next start finishes
/// it. A secret whose new name is taken already is left as it is, and
/// said so.
pub(super) async fn rename_secrets(pool: &SqlitePool, state_dir: &Path) -> Result<(), StoreError> {
    let has_map: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master \
                        WHERE type = 'table' AND name = 'legacy_user_ids')",
    )
    .fetch_one(pool)
    .await?;
    if !has_map {
        return Ok(());
    }
    let ids: Vec<(i64, String)> = sqlx::query_as("SELECT old_id, new_id FROM legacy_user_ids")
        .fetch_all(pool)
        .await?;
    let secrets = Secrets::new(state_dir);
    for (old, new) in ids {
        let from = secrets.path(&secret_name(old));
        if !from.exists() {
            continue;
        }
        let to = secrets.path(&secret_name(&new));
        if to.exists() {
            tracing::warn!(
                old = %from.display(),
                new = %to.display(),
                "a secret named by an old user id is left in place: its new name is taken"
            );
            continue;
        }
        std::fs::rename(&from, &to).map_err(|source| StoreError::RenameSecret {
            from: from.clone(),
            source,
        })?;
        tracing::info!(
            from = %from.display(),
            to = %to.display(),
            "secret renamed to its user's new id"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use sqlx::sqlite::SqliteConnectOptions;
    use uuid::{Uuid, Variant};

    use super::super::tests::state_dir;
    use super::super::{DB_FILE, Store};
    use super::*;
    use crate::principal::Principal;
    use crate::store::users::LocalUserId;

    /// A database at the schema before 0012, as a server of the time left
    /// it, with `foreign_keys` as given (off to plant an orphan).
    async fn before_0012(dir: &Path, foreign_keys: bool) -> SqlitePool {
        let options = SqliteConnectOptions::new()
            .filename(dir.join(DB_FILE))
            .create_if_missing(true)
            .foreign_keys(foreign_keys);
        let pool = sqlx::pool::PoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        let mut before = sqlx::migrate!("./migrations");
        before.migrations = before
            .iter()
            .filter(|m| m.version < VERSION)
            .cloned()
            .collect::<Vec<_>>()
            .into();
        before.run(&pool).await.unwrap();
        pool
    }

    /// A household: root (1) and alice (2) made in the same second, bob
    /// (3) earlier; sessions, a device, a PIN asked for on it, a passkey,
    /// an SSO link, watch state, a hide, track choices, and alice's and
    /// root's Plex links as secrets. Plus watch state of a user 9 who does
    /// not exist.
    const HOUSEHOLD: &str = "
        INSERT INTO users (id, username, created_at, role, display_name)
            VALUES (1, 'root', 100, 'admin', NULL),
                   (2, 'alice', 100, 'consumer', 'Alice A.'),
                   (3, 'bob', 50, 'consumer', NULL);
        INSERT INTO password_credentials (user_id, phc, updated_at)
            VALUES (1, 'phc-root', 1), (2, 'phc-alice', 2);
        INSERT INTO devices (endpoint_id, first_seen_at, last_seen_at, platform)
            VALUES ('ep-tv', 1, 2, 'tvos');
        INSERT INTO sessions (token_hash, user_id, kind, endpoint_id, created_at,
                              expires_at, last_used_at, authenticated_at)
            VALUES ('h-alice-tv', 2, 'endpoint', 'ep-tv', 10, 99999999999, 11, 10),
                   ('h-root-api', 1, 'api', NULL, 20, 99999999999, 21, 20),
                   ('h-bob-tv', 3, 'endpoint', 'ep-tv', 30, 31, 30, 30);
        INSERT INTO pin_credentials (user_id, phc, updated_at, digits)
            VALUES (2, 'pin-alice', 3, 4);
        INSERT INTO pin_required (user_id, endpoint_id, set_at) VALUES (2, 'ep-tv', 4);
        INSERT INTO passkey_users (user_id, handle)
            VALUES (2, '3f0d7c0e-4a8e-4d0b-9c7e-0d6a8b1e2f3a');
        INSERT INTO passkeys (credential_id, user_id, name, credential, rp_id, created_at)
            VALUES ('cred-alice', 2, 'MacBook', '{}', 'tower.lan', 5);
        INSERT INTO external_identities (user_id, issuer, subject, display, linked_at)
            VALUES (3, 'https://id.example', 'sub-bob', 'bob@example', 6);
        INSERT INTO watch_state (principal, media_id, show_id, position_ms,
                                 duration_ms, watched, updated_at)
            VALUES ('local:2', 'plex:movie:1', NULL, 60000, 3600000, 0, 7),
                   ('local:3', 'plex:episode:5', 'plex:show:4', NULL, NULL, 1, 8),
                   ('local:9', 'plex:movie:1', NULL, 1, 2, 0, 9);
        INSERT INTO watch_state_ids (principal, media_id, external_id)
            VALUES ('local:2', 'plex:movie:1', 'imdb://tt1'),
                   ('local:9', 'plex:movie:1', 'imdb://tt1');
        INSERT INTO continue_hidden (principal, media_id, hidden_at)
            VALUES ('local:3', 'plex:show:4', 10);
        INSERT INTO continue_hidden_ids (principal, media_id, external_id)
            VALUES ('local:3', 'plex:show:4', 'tvdb://4');
        INSERT INTO track_preferences (principal, preferences, updated_at)
            VALUES ('local:1', '{}', 11);
        INSERT INTO track_overrides (principal, scope, override, updated_at)
            VALUES ('local:2', 'plex:show:4', '{}', 12);
    ";

    fn plant_secrets(dir: &Path) {
        let secrets = Secrets::new(dir);
        secrets.set("plex-account-2", "alice's link").unwrap();
        secrets.set("plex-account-1", "root's link").unwrap();
        secrets.set("plex-token", "the server's").unwrap();
    }

    async fn household(test: &str) -> spoolframe_test_dir::TestDir {
        let dir = state_dir(test).await;
        let pool = before_0012(dir.as_ref(), true).await;
        sqlx::raw_sql(HOUSEHOLD).execute(&pool).await.unwrap();
        pool.close().await;
        plant_secrets(dir.as_ref());
        dir
    }

    async fn ids_by_name(store: &Store) -> BTreeMap<String, LocalUserId> {
        store
            .list_users()
            .await
            .unwrap()
            .into_iter()
            .map(|u| (u.username, u.id))
            .collect()
    }

    async fn count(store: &Store, sql: &str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
            .fetch_one(&store.pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn each_user_gets_a_uuidv7_in_creation_order() {
        let dir = household("uuid-v7").await;
        let store = Store::open(dir.as_ref()).await.unwrap();
        let ids = ids_by_name(&store).await;
        for id in ids.values() {
            assert_eq!(id.uuid().get_version_num(), 7, "{id}");
            assert_eq!(id.uuid().get_variant(), Variant::RFC4122, "{id}");
        }
        // bob was made first; root and alice in the same second, root
        // before alice.
        assert!(ids["bob"] < ids["root"] && ids["root"] < ids["alice"]);
        let (secs, _) = ids["alice"].uuid().get_timestamp().unwrap().to_unix();
        assert_eq!(secs, 100);
    }

    #[tokio::test]
    async fn every_row_follows_its_user_to_the_new_id() {
        let dir = household("rows").await;
        let store = Store::open(dir.as_ref()).await.unwrap();
        let ids = ids_by_name(&store).await;
        let (root, alice, bob) = (ids["root"], ids["alice"], ids["bob"]);

        let users = store.list_users().await.unwrap();
        let alice_record = users.iter().find(|u| u.id == alice).unwrap();
        assert_eq!(alice_record.display_name.as_deref(), Some("Alice A."));
        assert!(alice_record.has_password && alice_record.has_pin && alice_record.has_passkey);
        assert!(users.iter().find(|u| u.id == bob).unwrap().has_sso);
        assert!(users.iter().find(|u| u.id == root).unwrap().role.is_admin());

        assert_eq!(
            store
                .password_login("alice")
                .await
                .unwrap()
                .unwrap()
                .phc
                .as_deref(),
            Some("phc-alice")
        );
        let session = store
            .use_endpoint_session("h-alice-tv", "ep-tv", 3600)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(session.user, alice);
        assert!(session.pin_required, "the PIN is still asked for on the TV");
        assert_eq!(
            store.api_session("h-root-api").await.unwrap().unwrap().user,
            root
        );
        // bob's session expired; it is kept all the same.
        assert_eq!(
            count(
                &store,
                &format!("SELECT count(*) FROM sessions WHERE user_id = '{bob}'")
            )
            .await,
            1
        );
        assert_eq!(
            store.pin_info(alice).await.unwrap().unwrap().digits,
            Some(4)
        );
        // The passkey's user handle is what it was.
        assert_eq!(
            store
                .passkey_handle_user("3f0d7c0e-4a8e-4d0b-9c7e-0d6a8b1e2f3a")
                .await
                .unwrap(),
            Some(alice)
        );
        assert_eq!(store.list_passkeys(alice).await.unwrap().len(), 1);
        assert_eq!(
            store
                .identity_owner("https://id.example", "sub-bob")
                .await
                .unwrap(),
            Some(bob)
        );

        let alice_watch = store.all_watch(Principal::LocalUser(alice)).await.unwrap();
        assert_eq!(alice_watch.len(), 1);
        assert_eq!(alice_watch[0].position_ms, Some(60000));
        assert_eq!(
            count(
                &store,
                &format!("SELECT count(*) FROM watch_state_ids WHERE principal = 'local:{alice}'")
            )
            .await,
            1
        );
        assert!(
            store
                .hidden(Principal::LocalUser(bob))
                .await
                .unwrap()
                .contains("plex:show:4")
        );
        assert_eq!(
            count(
                &store,
                &format!(
                    "SELECT count(*) FROM continue_hidden_ids WHERE principal = 'local:{bob}'"
                )
            )
            .await,
            1
        );
        assert_eq!(
            count(
                &store,
                &format!("SELECT count(*) FROM track_preferences WHERE principal = 'local:{root}'")
            )
            .await,
            1
        );
        assert!(
            store
                .track_override(Principal::LocalUser(alice), "plex:show:4")
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn nothing_is_left_on_an_old_id() {
        let dir = household("no-old-ids").await;
        let store = Store::open(dir.as_ref()).await.unwrap();
        let orphans: Vec<(String, i64, String, i64)> = sqlx::query_as("PRAGMA foreign_key_check")
            .fetch_all(&store.pool)
            .await
            .unwrap();
        assert_eq!(orphans, []);
        for table in [
            "watch_state",
            "watch_state_ids",
            "continue_hidden",
            "continue_hidden_ids",
            "track_preferences",
            "track_overrides",
        ] {
            let old = count(
                &store,
                &format!(
                    "SELECT count(*) FROM {table} \
                     WHERE principal NOT IN (SELECT 'local:' || id FROM users)"
                ),
            )
            .await;
            assert_eq!(old, 0, "{table}");
        }
        for table in [
            "password_credentials",
            "sessions",
            "pin_credentials",
            "pin_required",
            "passkey_users",
            "passkeys",
            "external_identities",
        ] {
            let typed: i64 = count(
                &store,
                &format!("SELECT count(*) FROM {table} WHERE typeof(user_id) != 'text'"),
            )
            .await;
            assert_eq!(typed, 0, "{table}");
        }
        // Every row came along: the household's counts, less user 9's.
        assert_eq!(count(&store, "SELECT count(*) FROM sessions").await, 3);
        assert_eq!(count(&store, "SELECT count(*) FROM watch_state").await, 2);
    }

    #[tokio::test]
    async fn plex_link_secrets_take_the_new_ids() {
        let dir = household("secrets").await;
        let store = Store::open(dir.as_ref()).await.unwrap();
        let ids = ids_by_name(&store).await;
        let secrets = Secrets::new(dir.as_ref());
        assert_eq!(
            secrets.get(&secret_name(ids["alice"])).unwrap().as_deref(),
            Some("alice's link")
        );
        assert_eq!(
            secrets.get(&secret_name(ids["root"])).unwrap().as_deref(),
            Some("root's link")
        );
        assert_eq!(secrets.get("plex-account-2").unwrap(), None);
        assert_eq!(secrets.get("plex-account-1").unwrap(), None);
        assert_eq!(
            secrets.get("plex-token").unwrap().as_deref(),
            Some("the server's")
        );
    }

    /// A crash after the migration committed, before the secrets moved:
    /// the next start moves them.
    #[tokio::test]
    async fn a_secret_left_behind_is_renamed_at_the_next_start() {
        let dir = household("secrets-later").await;
        let store = Store::open(dir.as_ref()).await.unwrap();
        let alice = ids_by_name(&store).await["alice"];
        store.pool.close().await;
        let secrets = Secrets::new(dir.as_ref());
        std::fs::rename(
            secrets.path(&secret_name(alice)),
            secrets.path("plex-account-2"),
        )
        .unwrap();

        Store::open(dir.as_ref()).await.unwrap();
        assert_eq!(
            secrets.get(&secret_name(alice)).unwrap().as_deref(),
            Some("alice's link")
        );
        assert_eq!(secrets.get("plex-account-2").unwrap(), None);
    }

    #[tokio::test]
    async fn the_database_is_backed_up_first_and_once() {
        let dir = household("backup").await;
        let store = Store::open(dir.as_ref()).await.unwrap();
        store.pool.close().await;
        let backup = dir.as_ref().join(BACKUP_FILE);
        let pool = SqlitePool::connect_with(
            SqliteConnectOptions::new()
                .filename(&backup)
                .read_only(true),
        )
        .await
        .unwrap();
        let old: Vec<(i64, String)> = sqlx::query_as("SELECT id, username FROM users ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(
            old,
            [
                (1, "root".to_owned()),
                (2, "alice".to_owned()),
                (3, "bob".to_owned())
            ]
        );
        pool.close().await;

        // Opened again, the migration does not run, nor is a backup made.
        let ids_before = ids_by_name(&Store::open(dir.as_ref()).await.unwrap()).await;
        let again = Store::open(dir.as_ref()).await.unwrap();
        assert_eq!(ids_by_name(&again).await, ids_before);
        assert!(!dir.as_ref().join(format!("{BACKUP_FILE}.1")).exists());
    }

    #[tokio::test]
    async fn a_new_database_is_not_backed_up() {
        let dir = state_dir("backup-new").await;
        Store::open(dir.as_ref()).await.unwrap();
        assert!(!dir.as_ref().join(BACKUP_FILE).exists());
    }

    /// Every row of every table, and the schema, as text.
    async fn dump(path: &Path) -> Vec<String> {
        let pool = SqlitePool::connect_with(SqliteConnectOptions::new().filename(path))
            .await
            .unwrap();
        let mut out: Vec<String> = sqlx::query_scalar(
            "SELECT type || ' ' || name || ': ' || coalesce(sql, '') FROM sqlite_master \
             ORDER BY name",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let tables: Vec<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
                .fetch_all(&pool)
                .await
                .unwrap();
        for table in tables {
            let columns: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info(?1)")
                .bind(&table)
                .fetch_all(&pool)
                .await
                .unwrap();
            let row = columns
                .iter()
                .map(|c| format!("quote(\"{c}\")"))
                .collect::<Vec<_>>()
                .join(" || ',' || ");
            let rows: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT {row} FROM \"{table}\" ORDER BY 1"
            )))
            .fetch_all(&pool)
            .await
            .unwrap();
            out.extend(rows.into_iter().map(|r| format!("{table}: {r}")));
        }
        pool.close().await;
        out
    }

    /// A session whose user is gone (planted with foreign keys off) cannot
    /// be moved to a new id: the migration fails part-way, after `users`
    /// was rebuilt, and nothing it did stays.
    #[tokio::test]
    async fn a_failed_migration_leaves_the_database_as_it_was() {
        let dir = state_dir("rollback").await;
        let pool = before_0012(dir.as_ref(), false).await;
        sqlx::raw_sql(HOUSEHOLD).execute(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO sessions (token_hash, user_id, kind, created_at, expires_at) \
             VALUES ('h-ghost', 42, 'api', 0, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;
        plant_secrets(dir.as_ref());
        let db = dir.as_ref().join(DB_FILE);
        let before = dump(&db).await;

        let failed = Store::open(dir.as_ref()).await;
        assert!(
            matches!(failed, Err(StoreError::Migrate { .. })),
            "{failed:?}"
        );
        assert_eq!(dump(&db).await, before);
        let secrets = Secrets::new(dir.as_ref());
        assert_eq!(
            secrets.get("plex-account-2").unwrap().as_deref(),
            Some("alice's link")
        );
        // The backup was made, and is the database as it was.
        assert_eq!(dump(&dir.as_ref().join(BACKUP_FILE)).await, before);
    }

    #[tokio::test]
    async fn a_new_user_s_id_is_a_uuidv7_shown_as_a_principal() {
        let dir = state_dir("fresh").await;
        let store = Store::open(dir.as_ref()).await.unwrap();
        let id = store.add_first_admin("root", "phc").await.unwrap();
        assert_eq!(id.uuid().get_version_num(), 7);
        let shown = Principal::LocalUser(id).to_string();
        let uuid = shown.strip_prefix("local:").unwrap();
        assert_eq!(
            uuid,
            Uuid::parse_str(uuid).unwrap().hyphenated().to_string()
        );
    }

    #[tokio::test]
    async fn two_servers_first_users_have_different_ids() {
        let (a, b) = (state_dir("server-a").await, state_dir("server-b").await);
        let first = |dir: spoolframe_test_dir::TestDir| async move {
            let store = Store::open(dir.as_ref()).await.unwrap();
            store.add_first_admin("root", "phc").await.unwrap()
        };
        assert_ne!(first(a).await, first(b).await);
    }
}
