//! A disk that fills and empties again (2026-10-08): writes fail while
//! it is full, and work again once space is back.
//!
//! The disk is a cap on the database's size (`PRAGMA max_page_count`),
//! set on every connection in the pool: SQLite fails a write that would
//! grow the database past it with SQLITE_FULL, as it does on ENOSPC.

use std::time::Duration;

use spoolframe_types::Role;

use super::Store;
use super::sessions::{RENEW_EVERY_SECS, SessionBinding};
use super::tests::state_dir;
use super::watch::WatchUpdate;
use crate::principal::Principal;
use crate::store::users::LocalUserId;

const ALICE: Principal = Principal::LocalUser(LocalUserId::from_uuid(uuid::Uuid::from_u128(1)));

/// Hold every connection the pool may open, and run `pragma` on each.
async fn on_every_connection(store: &Store, pragma: &str) {
    let mut held = Vec::new();
    for _ in 0..store.pool.options().get_max_connections() {
        let mut conn = store.pool.acquire().await.unwrap();
        sqlx::query(sqlx::AssertSqlSafe(pragma.to_owned()))
            .execute(&mut *conn)
            .await
            .unwrap();
        held.push(conn);
    }
}

/// Fill the disk: the database may not grow past its present size.
/// Returns the pragma that says so, for a connection opened later.
async fn fill(store: &Store) -> String {
    let pages: i64 = sqlx::query_scalar("PRAGMA page_count")
        .fetch_one(&store.pool)
        .await
        .unwrap();
    let full = format!("PRAGMA max_page_count = {pages}");
    on_every_connection(store, &full).await;
    full
}

/// Free the disk again.
async fn free(store: &Store) {
    on_every_connection(store, "PRAGMA max_page_count = 1073741823").await;
}

fn assert_disk_full(e: &str) {
    assert!(e.contains("(code: 13)"), "not SQLITE_FULL: {e}");
}

/// Ids enough to grow the database by many pages.
fn big_ids() -> Vec<String> {
    (0..200)
        .map(|i| format!("big://{i}/{}", "x".repeat(1000)))
        .collect()
}

fn progress<'a>(media_id: &'a str, ids: &'a [String]) -> WatchUpdate<'a> {
    WatchUpdate {
        media_id,
        show_id: None,
        external_ids: ids,
        position_ms: Some(1_000),
        duration_ms: Some(7_000_000),
        watched: false,
        unhide: true,
    }
}

/// What the dev server did on 2026-10-08. A COMMIT that cannot append to
/// the WAL fails with SQLITE_FULL, and SQLite rolls the whole transaction
/// back by itself; sqlx, not told, sends its own ROLLBACK, which fails
/// with "no transaction is active", and keeps counting the connection as
/// one transaction deep. Back in the pool, the connection's next
/// "transaction" is a SAVEPOINT: SQLite opens a real transaction for it,
/// and once a write in it fails, sqlx's `ROLLBACK TO` undoes the write but
/// leaves that transaction, and its write lock, open for good.
///
/// A size cap fails a statement, not a COMMIT, so the test fails an
/// insert that SQLite answers the same way: one row, no index, no
/// statement journal, so SQLITE_FULL rolls back the whole transaction.
#[tokio::test]
async fn writes_work_again_once_a_full_disk_has_space() {
    let dir = state_dir("disk-full").await;
    let store = Store::open(dir.as_ref()).await.unwrap();
    let alice = store
        .add_user("alice", None, "phc", Role::Consumer)
        .await
        .unwrap();
    store
        .create_session("h", alice, &SessionBinding::Api, store.now() + 3_600)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE spill (b BLOB)")
        .execute(&store.pool)
        .await
        .unwrap();

    let full = fill(&store).await;
    // Leave the store one connection: the failures below meet on it, as
    // they may on a busy server.
    let mut others = Vec::new();
    for _ in 1..store.pool.options().get_max_connections() {
        others.push(store.pool.acquire().await.unwrap());
    }
    {
        let mut tx = store.pool.begin().await.unwrap();
        let e = sqlx::query("INSERT INTO spill VALUES (zeroblob(1000000))")
            .execute(&mut *tx)
            .await
            .unwrap_err();
        assert_disk_full(&e.to_string());
    }
    // The disk is still full for whichever connection the store has now
    // (a new one, if that one was closed).
    {
        let mut conn = store.pool.acquire().await.unwrap();
        sqlx::query(sqlx::AssertSqlSafe(full))
            .execute(&mut *conn)
            .await
            .unwrap();
    }
    // A watch progress report (and a Plex sync, which writes the same
    // way) while the disk is still full.
    let e = store
        .save_watch(ALICE, progress("plex:movie:1", &big_ids()))
        .await
        .unwrap_err();
    assert_disk_full(&e.to_string());
    drop(others);

    free(&store).await;
    store
        .clock()
        .advance(Duration::from_secs(RENEW_EVERY_SECS as u64));
    let touched_at = store.now();
    for i in 0..store.pool.options().get_max_connections() {
        // The session touch that waited out the busy timeout.
        tokio::time::timeout(
            Duration::from_secs(10),
            store.use_api_session("h", 3_600, 86_400),
        )
        .await
        .expect("the session touch hung")
        .unwrap_or_else(|e| panic!("session touch {i} once the disk had space: {e}"));
        tokio::time::timeout(
            Duration::from_secs(10),
            store.save_watch(ALICE, progress(&format!("plex:movie:after-{i}"), &[])),
        )
        .await
        .expect("the progress write hung")
        .unwrap_or_else(|e| panic!("progress write {i} once the disk had space: {e}"));
        store
            .clock()
            .advance(Duration::from_secs(RENEW_EVERY_SECS as u64));
    }
    store.close().await;

    // What was written then is in the database, and the database (with
    // its WAL and index) is whole.
    let store = Store::open(dir.as_ref()).await.unwrap();
    let saved: Vec<String> = sqlx::query_scalar(
        "SELECT media_id FROM watch_state WHERE media_id LIKE 'plex:movie:after-%' \
         ORDER BY media_id",
    )
    .fetch_all(&store.pool)
    .await
    .unwrap();
    assert_eq!(
        saved,
        (0..store.pool.options().get_max_connections())
            .map(|i| format!("plex:movie:after-{i}"))
            .collect::<Vec<_>>()
    );
    let session = &store.list_sessions(alice).await.unwrap()[0];
    assert!(
        session.last_used_at >= touched_at,
        "the session touch was lost"
    );
    let check: String = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_one(&store.pool)
        .await
        .unwrap();
    assert_eq!(check, "ok");
    store.close().await;
}
