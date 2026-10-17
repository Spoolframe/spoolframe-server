//! The server database: SQLite through sqlx, one file in the state
//! directory (ARCHITECTURE §21).
//!
//! Only the running server opens it. Admin tools go through the admin
//! API, so users, sessions and rate limits have a single owner.

pub mod audit;
pub mod grants;
pub mod identities;
pub mod invites;
pub mod oauth;
pub mod passkeys;
pub mod pins;
pub mod playlists;
pub mod plays;
pub mod portable;
pub mod sessions;
pub mod subtitle_timings;
pub mod ties;
pub mod tracks;
pub mod trust;
pub mod user_ids;
pub mod users;
pub mod watch;

#[cfg(test)]
mod disk_full_tests;

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

/// The database file's name inside the state directory.
pub const DB_FILE: &str = "spoolframe.db";

/// Push `values` to `query` as a parenthesised list of binds, for an `IN`.
/// The list must not be empty: SQLite reads `IN ()` as false, but a
/// caller that has nothing to ask for should not ask.
pub(crate) fn push_in_list<'t, T>(
    query: &mut sqlx::QueryBuilder<sqlx::Sqlite>,
    values: impl IntoIterator<Item = T>,
) where
    T: sqlx::Encode<'t, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite>,
{
    query.push("(");
    let mut list = query.separated(", ");
    for value in values {
        list.push_bind(value);
    }
    query.push(")");
}

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
    Db(#[source] sqlx::Error),
}

impl From<sqlx::Error> for StoreError {
    fn from(e: sqlx::Error) -> Self {
        if is_disk_full(&e) && DISK_FULL_REPORT.due(now()) {
            tracing::error!("the disk is full: database writes fail until space is freed ({e})");
        }
        Self::Db(e)
    }
}

/// SQLite's "database or disk is full" (SQLITE_FULL).
fn is_disk_full(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(d) if d.code().as_deref() == Some("13"))
}

/// The disk-full error is said once a minute at most: while the disk is
/// full every request that writes fails, and each says so itself.
static DISK_FULL_REPORT: Throttle = Throttle::new(60);

/// Says whether something is due again, at most once each `every_secs`.
struct Throttle {
    every_secs: i64,
    /// When it was last due, in Unix seconds; `i64::MIN` for never.
    last: std::sync::atomic::AtomicI64,
}

impl Throttle {
    const fn new(every_secs: i64) -> Self {
        Self {
            every_secs,
            last: std::sync::atomic::AtomicI64::new(i64::MIN),
        }
    }

    /// Whether it is due at `now` (Unix seconds); if it is, the next is
    /// `every_secs` later.
    fn due(&self, now: i64) -> bool {
        use std::sync::atomic::Ordering;
        let last = self.last.load(Ordering::Relaxed);
        last.checked_add(self.every_secs)
            .is_none_or(|next| now >= next)
            && self
                .last
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
    }
}

/// A store error type's `From<sqlx::Error>`: into its `Store` variant
/// through [`StoreError::from`], which reports a full disk.
macro_rules! from_sqlx_through_store_error {
    ($($error:ty),+ $(,)?) => {$(
        impl From<sqlx::Error> for $error {
            fn from(e: sqlx::Error) -> Self {
                Self::Store(e.into())
            }
        }
    )+};
}

from_sqlx_through_store_error!(
    identities::IdentityError,
    invites::InviteError,
    passkeys::PasskeyError,
    pins::PinError,
    playlists::PlaylistError,
    portable::IdentityStateError,
    ties::TieError,
    trust::TrustError,
    users::UserError,
);

/// A stored value that does not read back, as the store's error.
pub(super) fn decode_error(e: impl Into<sqlx::error::BoxDynError>) -> StoreError {
    StoreError::Db(sqlx::Error::Decode(e.into()))
}

/// A stored portable user id, read back.
pub(super) fn parse_id(
    text: String,
) -> Result<spoolframe_protocol::portable::PortableUserId, StoreError> {
    text.parse()
        .map_err(|e: spoolframe_protocol::portable::InvalidPortableUserId| decode_error(e))
}

/// Whether `endpoint_id` is one of `user`'s devices: one they have had
/// a session on.
async fn user_has_device(
    conn: &mut sqlx::SqliteConnection,
    user: users::LocalUserId,
    endpoint_id: &str,
) -> Result<bool, sqlx::Error> {
    let theirs: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM sessions WHERE user_id = ?1 AND endpoint_id = ?2 LIMIT 1",
    )
    .bind(user)
    .bind(endpoint_id)
    .fetch_optional(conn)
    .await?;
    Ok(theirs.is_some())
}

/// End `endpoint_id` for `user`: drop its trust for them, so it cannot
/// resume them (P4-01), and revoke their live sessions on it. How many
/// sessions were ended; the caller commits, then notes them ended.
async fn end_device(
    conn: &mut sqlx::SqliteConnection,
    user: users::LocalUserId,
    endpoint_id: &str,
    now: i64,
) -> Result<u64, sqlx::Error> {
    sqlx::query("DELETE FROM trusted_endpoints WHERE user_id = ?1 AND endpoint_id = ?2")
        .bind(user)
        .bind(endpoint_id)
        .execute(&mut *conn)
        .await?;
    Ok(sqlx::query(
        "UPDATE sessions SET revoked_at = ?3 \
         WHERE user_id = ?1 AND endpoint_id = ?2 AND revoked_at IS NULL",
    )
    .bind(user)
    .bind(endpoint_id)
    .bind(now)
    .execute(&mut *conn)
    .await?
    .rows_affected())
}

/// Whether a connection given back to the pool may be handed out again:
/// not when sqlx still counts it inside a transaction.
///
/// A COMMIT that fails with SQLITE_FULL or an I/O error (a full disk:
/// the WAL cannot grow) makes SQLite roll the whole transaction back by
/// itself. sqlx is not told: its own ROLLBACK then fails with "no
/// transaction is active", and it goes on counting the connection one
/// transaction deep. Its next transaction on that connection is then a
/// SAVEPOINT, which SQLite runs as a real transaction, and the
/// `ROLLBACK TO` that sqlx sends when one fails leaves it, and the write
/// lock it took, open: every other connection's write then fails with
/// "database is locked" until the server restarts (2026-10-08). Closing
/// the connection ends whatever SQLite still has open on it; the pool
/// opens a new one when it needs one. `closed` counts the ones closed.
async fn reusable(
    conn: &mut sqlx::SqliteConnection,
    closed: &std::sync::atomic::AtomicU64,
) -> Result<bool, sqlx::Error> {
    use sqlx::Connection;
    // Most connections come back outside any transaction, and sqlx's
    // count of them only goes down: it falls when the connection's
    // worker runs a queued ROLLBACK, never rises on its own. So a
    // connection counted outside one already is, with no round trip.
    if !conn.is_in_transaction() {
        return Ok(true);
    }
    // A dropped transaction's ROLLBACK is queued, not yet run: a round
    // trip to the connection's worker waits for it.
    conn.ping().await?;
    let reusable = !conn.is_in_transaction();
    if !reusable {
        closed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tracing::warn!(
            "a database connection came back to the pool still counted inside a \
             transaction; closing it"
        );
    }
    Ok(reusable)
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
    /// Connections closed coming back to the pool inside a transaction
    /// ([`reusable`]).
    #[cfg_attr(not(test), allow(dead_code))]
    closed_in_transaction: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl Store {
    /// Close every connection, once each is given back; SQLite
    /// checkpoints the WAL as the last one closes. Every clone of the
    /// store is closed with it: a call after this fails.
    pub async fn close(&self) {
        self.pool.close().await;
    }

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
            // NORMAL, not sqlx's FULL: under WAL a commit then writes the
            // log without an fsync, which only a checkpoint does. A crash
            // of the server loses nothing; a power loss or a crash of the
            // host can lose the last commits before it, never the
            // database's consistency. What is written often (progress,
            // session renewals) a device sends again anyway, and FULL
            // paid an fsync for each of those writes.
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));
        let closed_in_transaction = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .after_release({
                let closed = closed_in_transaction.clone();
                move |conn, _| {
                    let closed = closed.clone();
                    Box::pin(async move { reusable(conn, &closed).await })
                }
            })
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
            closed_in_transaction,
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

    /// A transaction that writes, begun IMMEDIATE: it takes SQLite's
    /// writer at once, waiting for it as any write does (busy_timeout).
    /// A deferred one that reads first holds a WAL snapshot, and if
    /// another connection commits before it writes, its first write
    /// fails with SQLITE_BUSY_SNAPSHOT ("database is locked"), which the
    /// busy handler never retries: an admin's password change racing a
    /// progress write or a session renewal failed that way.
    pub(crate) async fn begin_write(
        &self,
    ) -> Result<sqlx::Transaction<'static, sqlx::Sqlite>, sqlx::Error> {
        self.pool.begin_with("BEGIN IMMEDIATE").await
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

    /// WAL with synchronous NORMAL (review 2026-10-11 M21): no fsync per
    /// commit, only per checkpoint.
    #[tokio::test]
    async fn commits_are_not_synced_one_by_one() {
        let store = test_store("synchronous").await;
        let mut conn = store.pool.acquire().await.unwrap();
        let synchronous: i64 = sqlx::query_scalar("PRAGMA synchronous")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        // 1 is NORMAL; sqlx's default, FULL, is 2.
        assert_eq!(synchronous, 1);
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

    /// A write transaction that reads first, while another connection
    /// commits a write before it writes: what an admin's password change
    /// beside a progress report does. `race` begins it.
    async fn read_then_write_beside_a_commit(
        store: &Store,
        race: impl std::future::Future<
            Output = Result<sqlx::Transaction<'static, sqlx::Sqlite>, sqlx::Error>,
        >,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("CREATE TABLE IF NOT EXISTS race (n INTEGER)")
            .execute(&store.pool)
            .await
            .unwrap();
        let mut tx = race.await?;
        let _: i64 = sqlx::query_scalar("SELECT count(*) FROM race")
            .fetch_one(&mut *tx)
            .await?;
        let pool = store.pool.clone();
        let other = tokio::spawn(async move {
            sqlx::query("INSERT INTO race (n) VALUES (1)")
                .execute(&pool)
                .await
        });
        // The other write commits now, unless this one holds the writer.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let written = sqlx::query("INSERT INTO race (n) VALUES (2)")
            .execute(&mut *tx)
            .await;
        let committed = match written {
            Ok(_) => tx.commit().await,
            Err(e) => Err(e),
        };
        other.await.unwrap().unwrap();
        committed
    }

    /// The race itself: a deferred transaction's snapshot goes stale and
    /// its write is refused, not waited for (SQLITE_BUSY_SNAPSHOT).
    #[tokio::test]
    async fn a_deferred_transaction_that_read_first_loses_the_race() {
        let store = test_store("race-deferred").await;
        let err = read_then_write_beside_a_commit(&store, store.pool.begin())
            .await
            .unwrap_err();
        let code = err.as_database_error().and_then(|e| e.code());
        assert_eq!(code.as_deref(), Some("517"), "{err}");
    }

    /// Review 2026-10-11 H4: one begun IMMEDIATE holds the writer from its
    /// read, so the other write waits for it and both land.
    #[tokio::test]
    async fn a_write_transaction_that_read_first_still_writes() {
        let store = test_store("race-immediate").await;
        read_then_write_beside_a_commit(&store, store.begin_write())
            .await
            .unwrap();
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM race")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(rows, 2);
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

    #[test]
    fn a_throttle_is_due_once_each_period() {
        let throttle = Throttle::new(60);
        assert!(throttle.due(1_000));
        assert!(!throttle.due(1_059));
        assert!(throttle.due(1_060));
        assert!(!throttle.due(1_061));
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
