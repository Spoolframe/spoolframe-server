//! The server database: SQLite through sqlx, one file in the state
//! directory (ARCHITECTURE §21).
//!
//! Only the running server opens it. Admin tools go through the admin
//! API, so users, sessions and rate limits have a single owner.

pub mod identities;
pub mod passkeys;
pub mod pins;
pub mod sessions;
pub mod tracks;
pub mod trust;
pub mod user_ids;
pub mod users;
pub mod watch;

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};

/// The database file's name inside the state directory.
pub const DB_FILE: &str = "spoolframe.db";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("cannot open the database {path}: {source}")]
    Open { path: PathBuf, source: sqlx::Error },
    #[error("cannot migrate the database {path}: {source}")]
    Migrate {
        path: PathBuf,
        source: sqlx::migrate::MigrateError,
    },
    /// The copy made before migration 0012 (P3-20) could not be written;
    /// the migration has not run.
    #[error("cannot back the database up to {path}: {source}")]
    Backup { path: PathBuf, source: sqlx::Error },
    /// A secret named by a user's old id could not take the new one; the
    /// next start tries again.
    #[error("cannot rename the secret {from} to its user's new id: {source}")]
    RenameSecret {
        from: PathBuf,
        source: std::io::Error,
    },
    /// The database or its backup could not be made private.
    #[error("cannot make {path} readable by the server's user only: {source}")]
    Private {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
}

/// The open database.
#[derive(Debug, Clone)]
pub struct Store {
    pool: SqlitePool,
    /// Bumped after anything that may end sessions (PX-10), so the
    /// connections of a device that is no longer signed in are closed.
    sessions_ended: std::sync::Arc<tokio::sync::watch::Sender<u64>>,
    /// What "now" is for the times it writes and compares (PX-09).
    clock: crate::clock::Clock,
}

impl Store {
    /// Open (creating if needed) the database in `state_dir` and apply the
    /// migrations it has not seen. Before migration 0012 (UUID user ids,
    /// P3-20) the database is copied to `spoolframe.db.pre-0012`; after
    /// it, the secrets named by user id take the new ids.
    pub async fn open(state_dir: &Path) -> Result<Self, StoreError> {
        let path = state_dir.join(DB_FILE);
        // Password, PIN and session hashes (PX-11 finding 35): the
        // server's user's only, whatever the umask. SQLite makes the WAL
        // and its index with the database's mode; ones an older server
        // left are closed here.
        crate::files::private_file(&path).map_err(|source| StoreError::Private {
            path: path.clone(),
            source,
        })?;
        for suffix in ["-wal", "-shm"] {
            let side = state_dir.join(format!("{DB_FILE}{suffix}"));
            let private =
                <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600);
            match std::fs::set_permissions(&side, private) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    return Err(StoreError::Private {
                        path: side,
                        source: e,
                    });
                }
                _ => {}
            }
        }
        let options = SqliteConnectOptions::from_str("sqlite:")
            .map_err(|source| StoreError::Open {
                path: path.clone(),
                source,
            })?
            .filename(&path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .map_err(|source| StoreError::Open {
                path: path.clone(),
                source,
            })?;
        user_ids::back_up_if_pending(&pool, state_dir).await?;
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(|source| StoreError::Migrate { path, source })?;
        user_ids::rename_secrets(&pool, state_dir).await?;
        Ok(Self {
            pool,
            sessions_ended: std::sync::Arc::new(tokio::sync::watch::Sender::new(0)),
            clock: crate::clock::Clock::default(),
        })
    }

    /// Judge times by `clock` (tests move it instead of sleeping).
    pub fn with_clock(mut self, clock: crate::clock::Clock) -> Self {
        self.clock = clock;
        self
    }

    /// The clock its times are by.
    pub fn clock(&self) -> &crate::clock::Clock {
        &self.clock
    }

    /// Seconds since the Unix epoch by its clock: how the database
    /// stores times.
    pub(crate) fn now(&self) -> i64 {
        self.clock.unix_secs()
    }

    /// Changes each time sessions may have ended: revoked, or their user
    /// disabled. What changed is not said; the connections of every
    /// signed-in device are checked again.
    pub fn sessions_ended(&self) -> tokio::sync::watch::Receiver<u64> {
        self.sessions_ended.subscribe()
    }

    /// Say that sessions may have ended. A change that ends sessions
    /// outside this store's own methods calls it too.
    pub fn note_sessions_ended(&self) {
        self.sessions_ended.send_modify(|n| *n = n.wrapping_add(1));
    }
}

/// Seconds since the Unix epoch: how the database stores times.
pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A fresh state directory for one test, under a random name, so
    /// tests running at once (in this run or another) never share one.
    /// Removed when dropped.
    pub(crate) async fn state_dir(test: &str) -> spoolframe_test_dir::TestDir {
        spoolframe_test_dir::TestDir::new(&format!("spoolframe-store-{test}")).await
    }

    /// A store in a state directory of its own, which goes with it.
    pub(crate) struct TestStore {
        // Declared first, so it closes before its directory goes.
        pub(crate) store: Store,
        _dir: spoolframe_test_dir::TestDir,
    }

    impl std::ops::Deref for TestStore {
        type Target = Store;

        fn deref(&self) -> &Store {
            &self.store
        }
    }

    pub(crate) async fn test_store(test: &str) -> TestStore {
        let dir = state_dir(test).await;
        let store = Store::open(dir.as_ref()).await.unwrap();
        TestStore { store, _dir: dir }
    }

    #[tokio::test]
    async fn a_new_database_gets_every_migration() {
        let store = test_store("new").await;
        let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(
            applied as usize,
            sqlx::migrate!("./migrations").iter().count()
        );
    }

    #[tokio::test]
    async fn opening_again_applies_nothing_twice() {
        let dir = state_dir("reopen").await;
        Store::open(dir.as_ref()).await.unwrap().pool.close().await;
        // A migration applied twice would fail on CREATE TABLE.
        Store::open(dir.as_ref()).await.unwrap();
    }

    /// `path`'s permission bits.
    pub(crate) fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
            .permissions()
            .mode()
            & 0o777
    }

    /// The database holds password hashes and session hashes (PX-11
    /// finding 35): only the server's user reads it, or its WAL, whatever
    /// the umask.
    #[tokio::test]
    async fn the_database_and_its_wal_are_readable_by_the_server_s_user_only() {
        let dir = state_dir("modes").await;
        let store = Store::open(dir.as_ref()).await.unwrap();
        for file in [DB_FILE, "spoolframe.db-wal", "spoolframe.db-shm"] {
            assert_eq!(mode(&dir.as_ref().join(file)), 0o600, "{file}");
        }
        store.pool.close().await;
    }

    /// One made by an older server, readable by everyone, is made private
    /// at the next start.
    #[tokio::test]
    async fn a_database_left_readable_by_others_is_made_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = state_dir("modes-old").await;
        let store = Store::open(dir.as_ref()).await.unwrap();
        store.pool.close().await;
        for file in [DB_FILE, "spoolframe.db-wal", "spoolframe.db-shm"] {
            let path = dir.as_ref().join(file);
            if path.exists() {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            }
        }
        let store = Store::open(dir.as_ref()).await.unwrap();
        for file in [DB_FILE, "spoolframe.db-wal", "spoolframe.db-shm"] {
            assert_eq!(mode(&dir.as_ref().join(file)), 0o600, "{file}");
        }
        store.pool.close().await;
    }

    #[tokio::test]
    async fn foreign_keys_are_enforced() {
        let store = test_store("fk").await;
        let orphan = sqlx::query(
            "INSERT INTO password_credentials (user_id, phc, updated_at) VALUES (42, 'x', 0)",
        )
        .execute(&store.pool)
        .await;
        assert!(orphan.is_err());
    }
}
