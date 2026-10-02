//! The server database: SQLite through sqlx, one file in the state
//! directory (ARCHITECTURE §21).
//!
//! Only the running server opens it. Admin tools go through the admin
//! API, so users, sessions and rate limits have a single owner.

pub mod pins;
pub mod sessions;
pub mod tracks;
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
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
}

/// The open database.
#[derive(Debug, Clone)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    /// Open (creating if needed) the database in `state_dir` and apply the
    /// migrations it has not seen.
    pub async fn open(state_dir: &Path) -> Result<Self, StoreError> {
        let path = state_dir.join(DB_FILE);
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
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(|source| StoreError::Migrate { path, source })?;
        Ok(Self { pool })
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
