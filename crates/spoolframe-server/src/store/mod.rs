//! The server database: SQLite through sqlx, one file in the state
//! directory (ARCHITECTURE §21).
//!
//! Only the running server opens it. Admin tools go through the admin
//! API, so users, sessions and rate limits have a single owner.

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

    /// A fresh state directory for one test.
    pub(crate) fn state_dir(test: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("spoolframe-store-{}", std::process::id()))
            .join(test);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn a_new_database_gets_every_migration() {
        let store = Store::open(&state_dir("new")).await.unwrap();
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
        let dir = state_dir("reopen");
        Store::open(&dir).await.unwrap().pool.close().await;
        // A migration applied twice would fail on CREATE TABLE.
        Store::open(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn foreign_keys_are_enforced() {
        let store = Store::open(&state_dir("fk")).await.unwrap();
        let orphan = sqlx::query(
            "INSERT INTO password_credentials (user_id, phc, updated_at) VALUES (42, 'x', 0)",
        )
        .execute(&store.pool)
        .await;
        assert!(orphan.is_err());
    }
}
