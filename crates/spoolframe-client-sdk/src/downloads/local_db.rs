//! The offline database: one SQLite file per server and user,
//! `<dir>/<server>/<user>/offline.sqlite3`, holding what offline needs and
//! the downloads do not: every playlist the profile sees with all its
//! entries, each track's heart, and the search index of what the profile
//! holds here (`Client::search_offline`).
//!
//! Every connection lives on one thread of its own ([`Dbs`]): a task sent
//! there runs in order with the others, so a manifest's write and its
//! index's are never reordered. An async caller awaits the answer; a sync
//! one (the offline library and search, as before) blocks for it; a
//! change nobody waits for is posted.
//!
//! A database is opened when first needed, in WAL mode, and migrated by
//! its `user_version`. A new one removes the JSON files of the build
//! before (`playlists.json`, `hearts.json` and the covers), whose
//! playlists the background fill reads again, and builds its search index
//! from the manifests; so does one whose index is of another
//! [`INDEX_VERSION`]. After that the index follows each manifest written
//! or removed ([`Local::reindex`]).
//!
//! The search index is an FTS5 table (`unicode61 remove_diacritics 2`)
//! over text already folded by [`super::offline_search`]'s normaliser,
//! which folds what the tokenizer does not ("ß" as "ss", "æ" as "ae"),
//! beside a table of what each row is: its kind, id, item and year.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use spoolframe_protocol::playlists::{Builtin, PlaylistEntry, PlaylistSummary};
use spoolframe_types::{Album, Artist, Item, MediaId, SearchKind, Show};

use super::DownloadPhase;
use super::store::{EntryKind, Manifest, Store};

/// The database's file, in a user's directory.
pub(super) const FILE: &str = "offline.sqlite3";
/// The schema's version: `user_version` once migrated.
const SCHEMA: i64 = 1;
/// The search index's version: a database whose index is of another is
/// indexed again from the manifests.
pub(super) const INDEX_VERSION: &str = "1";
/// The files of the build before, removed when a database is made.
const OLD_FILES: [&str; 2] = ["playlists.json", "hearts.json"];
/// Where the playlists' covers are kept, beside the database.
pub(super) const ART: &str = "playlist-art";

type Task = Box<dyn FnOnce(&mut Local) + Send>;

/// The thread holding every offline database's connection, and the way to
/// it. Its thread ends when this is dropped.
#[derive(Debug)]
pub(crate) struct Dbs {
    tx: mpsc::Sender<Task>,
}

impl Dbs {
    pub(super) fn start(store: Store) -> Arc<Self> {
        let (tx, rx) = mpsc::channel::<Task>();
        let spawned = std::thread::Builder::new()
            .name("spoolframe-offline-db".into())
            .spawn(move || {
                let mut local = Local {
                    store,
                    open: HashMap::new(),
                    named: HashSet::new(),
                    with_db: HashMap::new(),
                };
                while let Ok(task) = rx.recv() {
                    task(&mut local);
                }
            });
        if let Err(e) = spawned {
            tracing::error!(error = %e, "the offline database's thread did not start");
        }
        Arc::new(Self { tx })
    }

    /// Run `task` there, waited for by nobody.
    pub(super) fn post(&self, task: impl FnOnce(&mut Local) + Send + 'static) {
        let _ = self.tx.send(Box::new(task));
    }

    /// Run `task` there and wait for its answer, blocking: `None` with the
    /// thread gone.
    pub(super) fn call<R: Send + 'static>(
        &self,
        task: impl FnOnce(&mut Local) -> R + Send + 'static,
    ) -> Option<R> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.post(move |local| {
            let _ = tx.send(task(local));
        });
        rx.recv().ok()
    }

    /// Run `task` there and await its answer.
    pub(super) async fn call_async<R: Send + 'static>(
        &self,
        task: impl FnOnce(&mut Local) -> R + Send + 'static,
    ) -> Option<R> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.post(move |local| {
            let _ = tx.send(task(local));
        });
        rx.await.ok()
    }
}

/// The connections, on their thread.
pub(super) struct Local {
    pub store: Store,
    open: HashMap<(String, String), Connection>,
    /// The databases whose username is written ([`Local::named`]): not
    /// written again by each task.
    named: HashSet<(String, String)>,
    /// Each server's users with a database here, once looked for on the
    /// disk; kept up as one is made or forgotten.
    with_db: HashMap<String, Vec<String>>,
}

/// What is wrong with an offline database, logged where it happens.
fn logged<T>(what: &str, result: rusqlite::Result<T>) -> Option<T> {
    result
        .map_err(|e| tracing::warn!(error = %e, "offline database: {what}"))
        .ok()
}

impl Local {
    fn path(&self, server: &str, user_id: &str) -> PathBuf {
        self.store.user_dir(server, user_id).join(FILE)
    }

    /// `server`'s `user_id`'s database, opened (and made) if it is not.
    pub fn db(&mut self, server: &str, user_id: &str) -> Option<&mut Connection> {
        let key = (server.to_owned(), user_id.to_owned());
        if !self.open.contains_key(&key) {
            let conn = self.open_db(server, user_id)?;
            if let Some(users) = self.with_db.get_mut(server)
                && !users.iter().any(|u| u == user_id)
            {
                users.push(user_id.to_owned());
            }
            self.open.insert(key.clone(), conn);
        }
        self.open.get_mut(&key)
    }

    fn open_db(&self, server: &str, user_id: &str) -> Option<Connection> {
        let dir = self.store.user_dir(server, user_id);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!(error = %e, "offline database: no directory");
            return None;
        }
        let path = self.path(server, user_id);
        let made = !path.exists();
        let mut conn = logged("open", Connection::open(&path))?;
        logged("set up", set_up(&mut conn))?;
        if made {
            // The build before kept these as JSON: read again, not moved.
            for old in OLD_FILES {
                let _ = std::fs::remove_file(dir.join(old));
            }
            let _ = std::fs::remove_dir_all(art_dir(&self.store, server, user_id));
        }
        let indexed: Option<String> = logged(
            "index version",
            conn.query_row("SELECT value FROM meta WHERE key = 'index'", [], |r| {
                r.get(0)
            })
            .optional(),
        )?;
        if indexed.as_deref() != Some(INDEX_VERSION) {
            logged(
                "index",
                rebuild_index(&mut conn, &self.store, server, user_id),
            )?;
        }
        Some(conn)
    }

    /// The users of `server` with a database here: looked for on the
    /// disk the first time, then kept ([`Local::with_db`]).
    pub(super) fn users_with_db(&mut self, server: &str) -> Vec<String> {
        let store = &self.store;
        self.with_db
            .entry(server.to_owned())
            .or_insert_with(|| {
                store
                    .users_with(FILE)
                    .into_iter()
                    .filter(|(s, _)| s == server)
                    .map(|(_, u)| u)
                    .collect()
            })
            .clone()
    }

    /// Item `id` of `server` (a `kind` entry) was written or removed:
    /// bring each database of that server up to date with what is on the
    /// disk of it now.
    pub fn reindex(&mut self, server: &str, id: &MediaId, kind: EntryKind) {
        let manifests = match kind {
            EntryKind::Track => super::music::track_entries(&self.store, server, id),
            EntryKind::Film | EntryKind::Episode => self
                .store
                .of_item(id)
                .into_iter()
                .filter(|m| m.server == server && m.item.id() == id)
                .collect(),
            EntryKind::Unknown => return,
        };
        let store = self.store.clone();
        for user in self.users_with_db(server) {
            let Some(conn) = self.db(server, &user) else {
                continue;
            };
            let best = best_of(&store, &user, manifests.iter());
            let done = logged(
                "reindex",
                conn.transaction().and_then(|tx| {
                    match best {
                        Some(m) => index_manifest(&tx, m)?,
                        None => unindex(&tx, id, kind)?,
                    }
                    tx.commit()
                }),
            );
            if done.is_none() {
                // Read again from the manifests next time it opens.
                let _ = conn.execute("DELETE FROM meta WHERE key = 'index'", []);
            }
        }
    }

    /// Close and delete `server`'s databases: every user's, or with
    /// `username` only theirs; with their covers. The users whose were.
    pub fn forget(&mut self, server: &str, username: Option<&str>) -> Vec<String> {
        let mut gone = Vec::new();
        for user in self.users_with_db(server) {
            if let Some(username) = username {
                let theirs = self.db(server, &user).and_then(|conn| {
                    conn.query_row("SELECT value FROM meta WHERE key = 'username'", [], |r| {
                        r.get::<_, String>(0)
                    })
                    .optional()
                    .ok()
                    .flatten()
                });
                if theirs.as_deref() != Some(username) {
                    continue;
                }
            }
            let key = (server.to_owned(), user.clone());
            self.open.remove(&key);
            self.named.remove(&key);
            let path = self.path(server, &user);
            for suffix in ["", "-wal", "-shm"] {
                let mut file = path.clone().into_os_string();
                file.push(suffix);
                let _ = std::fs::remove_file(PathBuf::from(file));
            }
            let _ = std::fs::remove_dir_all(art_dir(&self.store, server, &user));
            gone.push(user);
        }
        if let Some(users) = self.with_db.get_mut(server) {
            users.retain(|u| !gone.contains(u));
        }
        gone
    }

    /// Note whose `server`'s `user_id`'s database is: what forgetting a
    /// profile by name finds it by, whatever wrote to it first.
    pub fn named(&mut self, server: &str, user_id: &str, username: &str) {
        let key = (server.to_owned(), user_id.to_owned());
        if self.named.contains(&key) {
            return;
        }
        if let Some(conn) = self.db(server, user_id)
            && conn
                .execute(
                    "INSERT OR IGNORE INTO meta (key, value) VALUES ('username', ?1)",
                    [username],
                )
                .is_ok()
        {
            self.named.insert(key);
        }
    }
}

fn set_up(conn: &mut Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version < 1 {
        let tx = conn.transaction()?;
        tx.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;
             CREATE TABLE playlists (
                 id TEXT PRIMARY KEY,
                 position INTEGER NOT NULL,
                 summary TEXT NOT NULL,
                 revision INTEGER NOT NULL,
                 read_at INTEGER,
                 total INTEGER NOT NULL DEFAULT 0,
                 cover_artwork TEXT,
                 cover_file TEXT
             );
             CREATE TABLE playlist_entries (
                 playlist TEXT NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
                 position INTEGER NOT NULL,
                 item_id TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 title TEXT NOT NULL,
                 artist TEXT,
                 album TEXT,
                 duration_ms INTEGER,
                 item TEXT NOT NULL,
                 PRIMARY KEY (playlist, position)
             ) WITHOUT ROWID;
             CREATE INDEX playlist_entries_item ON playlist_entries(item_id);
             CREATE TABLE playlist_staging (
                 playlist TEXT NOT NULL,
                 position INTEGER NOT NULL,
                 item_id TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 title TEXT NOT NULL,
                 artist TEXT,
                 album TEXT,
                 duration_ms INTEGER,
                 item TEXT NOT NULL,
                 PRIMARY KEY (playlist, position)
             ) WITHOUT ROWID;
             CREATE TABLE hearts (track TEXT PRIMARY KEY, liked INTEGER NOT NULL) WITHOUT ROWID;
             CREATE TABLE search_items (
                 rowid INTEGER PRIMARY KEY,
                 kind TEXT NOT NULL,
                 ref TEXT NOT NULL,
                 item TEXT,
                 year INTEGER,
                 sort TEXT NOT NULL,
                 parent TEXT,
                 grandparent TEXT,
                 UNIQUE (kind, ref)
             );
             CREATE INDEX search_items_parent ON search_items(parent);
             CREATE INDEX search_items_grandparent ON search_items(grandparent);
             CREATE VIRTUAL TABLE search USING fts5(
                 text,
                 tokenize = 'unicode61 remove_diacritics 2'
             );
             PRAGMA user_version = 1;",
        )?;
        tx.commit()?;
    }
    // A newer schema adds its step above, from the one before.
    const { assert!(SCHEMA == 1) };
    Ok(())
}

/// The kind a search row is of, as kept.
pub(super) fn kind_name(kind: SearchKind) -> &'static str {
    match kind {
        SearchKind::Shows => "shows",
        SearchKind::Movies => "movies",
        SearchKind::People => "people",
        SearchKind::Artists => "artists",
        SearchKind::Albums => "albums",
        SearchKind::Tracks => "tracks",
        SearchKind::Episodes => "episodes",
    }
}

/// The search rows' kind of a kept playlist.
pub(super) const PLAYLIST_KIND: &str = "playlists";

/// `m` is one `user_id` sees here whole: theirs (or kept from listening),
/// finished, and a track's file on the disk.
fn shown_to(store: &Store, user_id: &str, m: &Manifest) -> bool {
    m.is_for(user_id)
        && m.phase == DownloadPhase::Done
        && match m.kind {
            EntryKind::Film | EntryKind::Episode => true,
            EntryKind::Track => store.media(m).is_some_and(|p| p.exists()),
            EntryKind::Unknown => false,
        }
}

/// Of `manifests` (one item's entries), the one `user_id` sees, a
/// download before a track kept from listening.
fn best_of<'a>(
    store: &Store,
    user_id: &str,
    manifests: impl Iterator<Item = &'a Manifest>,
) -> Option<&'a Manifest> {
    manifests
        .filter(|m| shown_to(store, user_id, m))
        .min_by_key(|m| m.cached_only())
}

/// Put one search row: `text` is folded already.
#[allow(clippy::too_many_arguments)]
fn put_row(
    tx: &Transaction,
    kind: &str,
    id: &str,
    item: Option<&Item>,
    year: Option<u16>,
    sort: &str,
    parents: (Option<&str>, Option<&str>),
    text: &str,
) -> rusqlite::Result<()> {
    let json = item.map(|i| serde_json::to_string(i).expect("an item serializes"));
    let existing: Option<(i64, Option<String>, String)> = tx
        .prepare_cached("SELECT rowid, item, sort FROM search_items WHERE kind = ?1 AND ref = ?2")?
        .query_row(params![kind, id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?;
    match existing {
        // Unchanged: its item, and its sort, which holds its name (all a
        // playlist's row is).
        Some((_, old, old_sort)) if old == json && old_sort == sort => {}
        Some((rowid, _, _)) => {
            tx.execute(
                "UPDATE search_items SET item = ?2, year = ?3, sort = ?4, parent = ?5,
                 grandparent = ?6 WHERE rowid = ?1",
                params![rowid, json, year, sort, parents.0, parents.1],
            )?;
            tx.execute(
                "UPDATE search SET text = ?2 WHERE rowid = ?1",
                params![rowid, text],
            )?;
        }
        None => {
            tx.execute(
                "INSERT INTO search_items (kind, ref, item, year, sort, parent, grandparent)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![kind, id, json, year, sort, parents.0, parents.1],
            )?;
            let rowid = tx.last_insert_rowid();
            tx.execute(
                "INSERT INTO search (rowid, text) VALUES (?1, ?2)",
                params![rowid, text],
            )?;
        }
    }
    Ok(())
}

/// Take the search row of `kind` `id` out.
pub(super) fn drop_row(tx: &Transaction, kind: &str, id: &str) -> rusqlite::Result<()> {
    let rowid: Option<i64> = tx
        .query_row(
            "SELECT rowid FROM search_items WHERE kind = ?1 AND ref = ?2",
            params![kind, id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(rowid) = rowid {
        tx.execute("DELETE FROM search WHERE rowid = ?1", [rowid])?;
        tx.execute("DELETE FROM search_items WHERE rowid = ?1", [rowid])?;
    }
    Ok(())
}

/// Take a parent row (show, album, artist) out once no row names it.
fn drop_if_orphan(tx: &Transaction, kind: &str, id: &str) -> rusqlite::Result<()> {
    let named: bool = tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM search_items
         WHERE kind != ?1 AND (parent = ?2 OR grandparent = ?2))",
        params![kind, id],
        |r| r.get(0),
    )?;
    if !named {
        drop_row(tx, kind, id)?;
    }
    Ok(())
}

/// The search text of `item`, with `also` (its show, album, artist),
/// folded.
fn text_of(item: &Item, also: &[Option<&str>]) -> String {
    let mut text = super::offline_search::fold(item.title());
    for more in also.iter().flatten() {
        text.push(' ');
        text.push_str(&super::offline_search::fold(more));
    }
    text
}

/// The order a kind's matches come in: by title, then id.
fn sort_of(item: &Item) -> String {
    format!(
        "{}\u{0}{}",
        super::offline_search::folded(item.title()),
        item.id()
    )
}

/// Index `m`, a finished download one user sees: its item and its show,
/// or a track's album and artist.
fn index_manifest(tx: &Transaction, m: &Manifest) -> rusqlite::Result<()> {
    let id = m.item.id().to_string();
    match &m.item {
        Item::Movie(v) => put_row(
            tx,
            kind_name(SearchKind::Movies),
            &id,
            Some(&m.item),
            v.year,
            &sort_of(&m.item),
            (None, None),
            &text_of(&m.item, &[]),
        ),
        Item::Episode(e) => {
            let show = m.show.clone().unwrap_or_else(|| {
                Item::Show(Show {
                    merge: Default::default(),
                    id: e.show_id.clone(),
                    title: e.show_title.clone().unwrap_or_default(),
                    year: None,
                    summary: None,
                    content_rating: None,
                    season_count: None,
                    genres: Vec::new(),
                    last_aired: None,
                    cast: Vec::new(),
                    artwork: Vec::new(),
                })
            });
            let show_id = show.id().to_string();
            let show_title = show.title().to_owned();
            let show_year = spoolframe_protocol::search_rank::year_of(&show);
            put_row(
                tx,
                kind_name(SearchKind::Shows),
                &show_id,
                Some(&show),
                show_year,
                &sort_of(&show),
                (None, None),
                &text_of(&show, &[]),
            )?;
            put_row(
                tx,
                kind_name(SearchKind::Episodes),
                &id,
                Some(&m.item),
                e.year,
                &sort_of(&m.item),
                (Some(&show_id), None),
                &text_of(&m.item, &[Some(&show_title)]),
            )
        }
        Item::Track(t) => {
            let artist = m.artist.clone().unwrap_or_else(|| {
                Item::Artist(Artist {
                    merge: Default::default(),
                    id: t.artist_id.clone(),
                    name: t.artist_name.clone().unwrap_or_default(),
                    summary: None,
                    artwork: Vec::new(),
                })
            });
            let artist_name = artist.title().to_owned();
            let album = m.album.clone().unwrap_or_else(|| {
                Item::Album(Album {
                    merge: Default::default(),
                    id: t.album_id.clone(),
                    artist_id: t.artist_id.clone(),
                    artist_name: Some(artist_name.clone()),
                    title: t.album_title.clone().unwrap_or_default(),
                    year: None,
                    track_count: t.album_track_count,
                    artwork: Vec::new(),
                })
            });
            let album_title = album.title().to_owned();
            let (album_id, artist_id) = (t.album_id.to_string(), t.artist_id.to_string());
            put_row(
                tx,
                kind_name(SearchKind::Artists),
                &artist_id,
                Some(&artist),
                None,
                &sort_of(&artist),
                (None, None),
                &text_of(&artist, &[]),
            )?;
            put_row(
                tx,
                kind_name(SearchKind::Albums),
                &album_id,
                Some(&album),
                spoolframe_protocol::search_rank::year_of(&album),
                &sort_of(&album),
                (Some(&artist_id), None),
                &text_of(&album, &[Some(&artist_name)]),
            )?;
            put_row(
                tx,
                kind_name(SearchKind::Tracks),
                &id,
                Some(&m.item),
                None,
                &sort_of(&m.item),
                (Some(&album_id), Some(&artist_id)),
                &text_of(
                    &m.item,
                    &[
                        Some(&album_title),
                        Some(&artist_name),
                        t.artist_name.as_deref(),
                    ],
                ),
            )
        }
        _ => Ok(()),
    }
}

/// Take item `id` (an entry of `kind`) out of the index, and its show,
/// album or artist once nothing else here names them.
fn unindex(tx: &Transaction, id: &MediaId, kind: EntryKind) -> rusqlite::Result<()> {
    let key = id.to_string();
    let item_kind = match kind {
        EntryKind::Film => SearchKind::Movies,
        EntryKind::Episode => SearchKind::Episodes,
        EntryKind::Track => SearchKind::Tracks,
        EntryKind::Unknown => return Ok(()),
    };
    let parents: Option<(Option<String>, Option<String>)> = tx
        .query_row(
            "SELECT parent, grandparent FROM search_items WHERE kind = ?1 AND ref = ?2",
            params![kind_name(item_kind), key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    drop_row(tx, kind_name(item_kind), &key)?;
    let Some((parent, grandparent)) = parents else {
        return Ok(());
    };
    match item_kind {
        SearchKind::Episodes => {
            if let Some(show) = parent {
                drop_if_orphan(tx, kind_name(SearchKind::Shows), &show)?;
            }
        }
        SearchKind::Tracks => {
            if let Some(album) = parent {
                drop_if_orphan(tx, kind_name(SearchKind::Albums), &album)?;
            }
            if let Some(artist) = grandparent {
                drop_if_orphan(tx, kind_name(SearchKind::Artists), &artist)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Index again, from the manifests, everything `server`'s `user_id`
/// holds here; the playlists' rows stay.
fn rebuild_index(
    conn: &mut Connection,
    store: &Store,
    server: &str,
    user_id: &str,
) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    tx.execute(
        "DELETE FROM search WHERE rowid IN
         (SELECT rowid FROM search_items WHERE kind != ?1)",
        [PLAYLIST_KIND],
    )?;
    tx.execute("DELETE FROM search_items WHERE kind != ?1", [PLAYLIST_KIND])?;
    let mut by_item: HashMap<MediaId, Vec<Manifest>> = HashMap::new();
    for m in store.scan() {
        if m.server == server {
            by_item.entry(m.item.id().clone()).or_default().push(m);
        }
    }
    for manifests in by_item.values() {
        if let Some(m) = best_of(store, user_id, manifests.iter()) {
            index_manifest(&tx, m)?;
        }
    }
    tx.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('index', ?1)",
        [INDEX_VERSION],
    )?;
    tx.commit()
}

/// How many of each kind `matched` (an FTS5 query) finds, of `year`
/// when one is given, in one query; a kind with none is left out.
pub(super) fn counts(
    conn: &Connection,
    matched: &str,
    year: Option<u16>,
) -> rusqlite::Result<HashMap<SearchKind, u32>> {
    let mut stmt = conn.prepare_cached(
        "SELECT i.kind, count(*) FROM search JOIN search_items i ON i.rowid = search.rowid
         WHERE search MATCH ?1 AND (?2 IS NULL OR i.year = ?2) GROUP BY i.kind",
    )?;
    let by_name: HashMap<String, u32> = stmt
        .query_map(params![matched, year], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(SEARCHED
        .into_iter()
        .filter_map(|kind| Some((kind, *by_name.get(kind_name(kind))?)))
        .collect())
}

/// Every kind a search row is of, but a playlist.
const SEARCHED: [SearchKind; 7] = [
    SearchKind::Shows,
    SearchKind::Movies,
    SearchKind::People,
    SearchKind::Artists,
    SearchKind::Albums,
    SearchKind::Tracks,
    SearchKind::Episodes,
];

/// The candidates of `kind` that `matched` (an FTS5 query) finds, of
/// `year` when one is given: at most `limit` ([`counts`] says how many
/// there are). Best first by the ranker's tier against `text` (the query,
/// folded as titles are ranked): the exact title, then those that start
/// with it, then the rest; within a tier by title. So the limit never
/// cuts a better match the ranker would have put first.
pub(super) fn find(
    conn: &Connection,
    kind: SearchKind,
    matched: &str,
    text: &str,
    year: Option<u16>,
    limit: usize,
) -> rusqlite::Result<Vec<Item>> {
    let kind = kind_name(kind);
    let mut stmt = conn.prepare_cached(
        "SELECT i.item FROM search JOIN search_items i ON i.rowid = search.rowid
         WHERE search MATCH ?1 AND i.kind = ?2 AND (?3 IS NULL OR i.year = ?3)
         ORDER BY CASE
             WHEN substr(i.sort, 1, length(?5) + 1) = ?5 || char(0) THEN 0
             WHEN substr(i.sort, 1, length(?5)) = ?5 THEN 1
             ELSE 2
         END, i.sort
         LIMIT ?4",
    )?;
    let items = stmt
        .query_map(
            params![
                matched,
                kind,
                year,
                i64::try_from(limit).unwrap_or(i64::MAX),
                text
            ],
            |r| r.get::<_, String>(0),
        )?
        .filter_map(|json| json.ok().and_then(|j| serde_json::from_str(&j).ok()))
        .collect();
    Ok(items)
}

/// The ids of the kept playlists that `matched` finds, in the server's
/// order.
pub(super) fn find_playlists(conn: &Connection, matched: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(
        "SELECT i.ref FROM search JOIN search_items i ON i.rowid = search.rowid
         JOIN playlists p ON p.id = i.ref
         WHERE search MATCH ?1 AND i.kind = ?2 ORDER BY p.position",
    )?;
    stmt.query_map(params![matched, PLAYLIST_KIND], |r| r.get(0))?
        .collect()
}

/// A playlist kept, as the library reads it.
#[derive(Debug, Clone)]
pub(super) struct KeptPlaylist {
    pub summary: PlaylistSummary,
    /// The revision its entries were read at; `None` until they are.
    pub read_at: Option<u64>,
    pub total: u32,
    pub cover: Option<(String, String)>,
}

/// Every playlist kept, in the server's order; `only` these ids when
/// given.
pub(super) fn playlists(
    conn: &Connection,
    only: Option<&[String]>,
) -> rusqlite::Result<Vec<KeptPlaylist>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, summary, read_at, total, cover_artwork, cover_file FROM playlists
         ORDER BY position",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<i64>>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, Option<String>>(5)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, summary, read_at, total, art, file) = row?;
        // Not asked for: not decoded.
        if only.is_some_and(|only| !only.contains(&id)) {
            continue;
        }
        let Ok(summary) = serde_json::from_str::<PlaylistSummary>(&summary) else {
            continue;
        };
        out.push(KeptPlaylist {
            summary,
            read_at: read_at.and_then(|r| u64::try_from(r).ok()),
            total: u32::try_from(total).unwrap_or(u32::MAX),
            cover: art.zip(file),
        });
    }
    Ok(out)
}

/// Every kept playlist's id, and whether it is Liked: without reading
/// their summaries.
pub(super) fn kept_targets(conn: &Connection) -> rusqlite::Result<Vec<(String, bool)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, coalesce(json_extract(summary, '$.builtin') = 'liked', 0) FROM playlists",
    )?;
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect()
}

/// The entries of playlist `id` from `offset`, at most `limit`, in its
/// order, each with the heart kept of it.
pub(super) fn entries(
    conn: &Connection,
    id: &str,
    offset: u32,
    limit: u32,
) -> rusqlite::Result<Vec<(Item, Option<bool>)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT e.item, h.liked FROM playlist_entries e
         LEFT JOIN hearts h ON h.track = e.item_id
         WHERE e.playlist = ?1 ORDER BY e.position LIMIT ?2 OFFSET ?3",
    )?;
    let items = stmt
        .query_map(params![id, limit, offset], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<bool>>(1)?))
        })?
        .filter_map(|row| {
            let (json, liked) = row.ok()?;
            Some((serde_json::from_str(&json).ok()?, liked))
        })
        .collect();
    Ok(items)
}

/// Of playlist `id`'s entries, the ids among `here` (a temp table filled
/// by [`fill_here`]), in its order.
pub(super) fn entries_here(conn: &Connection, id: &str) -> rusqlite::Result<Vec<MediaId>> {
    let mut stmt = conn.prepare_cached(
        "SELECT e.item_id FROM playlist_entries e JOIN temp.here h ON h.id = e.item_id
         WHERE e.playlist = ?1 ORDER BY e.position",
    )?;
    let ids = stmt
        .query_map([id], |r| r.get::<_, String>(0))?
        .filter_map(|id| id.ok().and_then(|id| id.parse().ok()))
        .collect();
    Ok(ids)
}

/// Fill the temp table `here` with the ids of the tracks on this
/// device, in one transaction.
pub(super) fn fill_here(conn: &mut Connection, here: &HashSet<MediaId>) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS here (id TEXT PRIMARY KEY) WITHOUT ROWID;",
    )?;
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM temp.here", [])?;
    {
        let mut stmt = tx.prepare_cached("INSERT OR IGNORE INTO temp.here (id) VALUES (?1)")?;
        for id in here {
            stmt.execute([id.to_string()])?;
        }
    }
    tx.commit()
}

/// The hearts kept of the tracks in `here` ([`fill_here`]).
pub(super) fn hearts_here(conn: &Connection) -> rusqlite::Result<HashMap<MediaId, bool>> {
    let mut stmt = conn.prepare_cached(
        "SELECT h.track, h.liked FROM hearts h JOIN temp.here t ON t.id = h.track",
    )?;
    let hearts = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?)))?
        .filter_map(|row| {
            let (id, liked) = row.ok()?;
            Some((id.parse().ok()?, liked))
        })
        .collect();
    Ok(hearts)
}

/// Keep `hearts`, one row each.
pub(super) fn learn_hearts(
    tx: &Transaction,
    hearts: impl IntoIterator<Item = (MediaId, bool)>,
) -> rusqlite::Result<()> {
    let mut stmt = tx.prepare_cached(
        "INSERT INTO hearts (track, liked) VALUES (?1, ?2)
         ON CONFLICT (track) DO UPDATE SET liked = excluded.liked
         WHERE liked != excluded.liked",
    )?;
    for (id, liked) in hearts {
        stmt.execute(params![id.to_string(), liked])?;
    }
    Ok(())
}

/// Liked, playlist `liked`, was read whole: what it holds is liked, and
/// any other track kept as liked is not.
pub(super) fn liked_whole(tx: &Transaction, liked: &str) -> rusqlite::Result<()> {
    tx.execute(
        "UPDATE hearts SET liked = 0 WHERE liked = 1 AND track NOT IN
         (SELECT item_id FROM playlist_entries WHERE playlist = ?1)",
        [liked],
    )?;
    tx.execute(
        "INSERT INTO hearts (track, liked)
         SELECT DISTINCT item_id, 1 FROM playlist_entries WHERE playlist = ?1 AND kind = 'track'
         ON CONFLICT (track) DO UPDATE SET liked = 1 WHERE liked != 1",
        [liked],
    )?;
    Ok(())
}

/// An entry's kind, as kept.
fn entry_kind(item: &Item) -> &'static str {
    match item {
        Item::Movie(_) => "movie",
        Item::Show(_) => "show",
        Item::Season(_) => "season",
        Item::Episode(_) => "episode",
        Item::Artist(_) => "artist",
        Item::Album(_) => "album",
        Item::Track(_) => "track",
    }
}

/// Put the search row of kept playlist `id`, named `name` (folded).
pub(super) fn put_playlist_row(tx: &Transaction, id: &str, name: &str) -> rusqlite::Result<()> {
    put_row(tx, PLAYLIST_KIND, id, None, None, name, (None, None), name)
}

/// Put `entries` of playlist `playlist` in `table` from `offset`.
fn put_entries<'a>(
    tx: &Transaction,
    table: &str,
    playlist: &str,
    offset: u64,
    entries: impl Iterator<Item = &'a PlaylistEntry>,
) -> rusqlite::Result<()> {
    let mut stmt = tx.prepare_cached(&format!(
        "INSERT OR REPLACE INTO {table}
         (playlist, position, item_id, kind, title, artist, album, duration_ms, item)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"
    ))?;
    for (at, e) in (offset..).zip(entries) {
        let (artist, album, duration) = match &e.item {
            Item::Track(t) => (t.artist_name.clone(), t.album_title.clone(), t.duration),
            Item::Movie(m) => (None, None, m.duration),
            Item::Episode(ep) => (ep.show_title.clone(), None, ep.duration),
            _ => (None, None, None),
        };
        stmt.execute(params![
            playlist,
            i64::try_from(at).unwrap_or(i64::MAX),
            e.item.id().to_string(),
            entry_kind(&e.item),
            e.item.title(),
            artist,
            album,
            duration.map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX)),
            serde_json::to_string(&e.item).expect("an item serializes"),
        ])?;
    }
    Ok(())
}

/// Keep a page of playlist `playlist`'s entries read from `offset`,
/// aside until [`commit_staged`]: the first page starts it anew.
pub(super) fn stage(
    tx: &Transaction,
    playlist: &str,
    offset: u32,
    entries: &[PlaylistEntry],
) -> rusqlite::Result<()> {
    if offset == 0 {
        tx.execute(
            "DELETE FROM playlist_staging WHERE playlist = ?1",
            [playlist],
        )?;
    }
    put_entries(
        tx,
        "playlist_staging",
        playlist,
        u64::from(offset),
        entries.iter(),
    )
}

/// Kept playlist `id`'s revision (of its summary) and the revision its
/// entries were read whole at; `None` for one not kept.
pub(super) fn revisions(
    tx: &Transaction,
    id: &str,
) -> rusqlite::Result<Option<(i64, Option<i64>)>> {
    tx.query_row(
        "SELECT revision, read_at FROM playlists WHERE id = ?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
}

/// A playlist's `revision` as kept.
pub(super) fn revision_of(revision: u64) -> i64 {
    i64::try_from(revision).unwrap_or(i64::MAX)
}

/// `summary` as kept playlist `summary.id`'s summary, with its search
/// row: unless what is kept is of a later revision. A playlist's revision
/// never moves back, whichever read is answered last.
pub(super) fn put_summary_if_newer(
    tx: &Transaction,
    summary: &PlaylistSummary,
    kept_revision: i64,
) -> rusqlite::Result<()> {
    let revision = revision_of(summary.revision);
    if revision < kept_revision {
        return Ok(());
    }
    let changed = tx.execute(
        "UPDATE playlists SET summary = ?2, revision = ?3
         WHERE id = ?1 AND (summary != ?2 OR revision != ?3)",
        params![
            summary.id,
            serde_json::to_string(summary).expect("a summary serializes"),
            revision,
        ],
    )?;
    if changed == 0 {
        return Ok(());
    }
    put_summary_row(tx, summary)
}

/// Where a whole read's entries are: staged by [`stage`] (`total` of
/// them), or given.
pub(super) enum Entries<'a> {
    Staged { total: u32 },
    Given(&'a [PlaylistEntry]),
}

/// What a whole read does to the cover kept.
pub(super) enum CoverUpdate {
    Leave,
    Set(Option<(String, String)>),
}

/// Playlist `summary` was read whole: its `entries` replace the kept
/// ones, with its summary, total and `cover`, and Liked's hearts follow
/// what is kept ([`liked_whole`]). Whether it is kept: nothing for a
/// playlist no longer kept, whose staging goes all the same.
///
/// The background read stages its pages and then waits on the cover, so
/// a screen's read of a later revision may be kept first: entries kept
/// whole at that revision, or at this one, are left as they are, and the
/// staging goes. The cover is kept unless the entries kept are of a
/// later revision; the summary unless it is older.
pub(super) fn commit_snapshot(
    tx: &Transaction,
    summary: &PlaylistSummary,
    entries: Entries<'_>,
    cover: CoverUpdate,
) -> rusqlite::Result<bool> {
    let kept = revisions(tx, &summary.id)?;
    if let Some((kept_revision, read_at)) = kept {
        let revision = revision_of(summary.revision);
        if read_at.is_none_or(|read| read < revision) {
            tx.execute(
                "DELETE FROM playlist_entries WHERE playlist = ?1",
                [&summary.id],
            )?;
            let total = match &entries {
                Entries::Staged { total } => {
                    tx.execute(
                        "INSERT INTO playlist_entries SELECT * FROM playlist_staging
                         WHERE playlist = ?1",
                        [&summary.id],
                    )?;
                    *total
                }
                Entries::Given(given) => {
                    put_entries(tx, "playlist_entries", &summary.id, 0, given.iter())?;
                    u32::try_from(given.len()).unwrap_or(u32::MAX)
                }
            };
            tx.execute(
                "UPDATE playlists SET read_at = ?2, total = ?3 WHERE id = ?1",
                params![summary.id, revision, total],
            )?;
        }
        if let CoverUpdate::Set(cover) = cover
            && read_at.is_none_or(|read| read <= revision)
        {
            let (art, file) = cover.unzip();
            tx.execute(
                "UPDATE playlists SET cover_artwork = ?2, cover_file = ?3 WHERE id = ?1",
                params![summary.id, art, file],
            )?;
        }
        put_summary_if_newer(tx, summary, kept_revision)?;
        if summary.builtin == Some(Builtin::Liked) {
            // Of the entries kept, which may be a later read's.
            liked_whole(tx, &summary.id)?;
        }
    }
    if let Entries::Staged { .. } = entries {
        tx.execute(
            "DELETE FROM playlist_staging WHERE playlist = ?1",
            [&summary.id],
        )?;
    }
    Ok(kept.is_some())
}

/// [`commit_snapshot`] of what [`stage`] staged, in one transaction.
pub(super) fn commit_staged(
    conn: &mut Connection,
    summary: &PlaylistSummary,
    total: u32,
    cover: Option<(String, String)>,
) -> rusqlite::Result<bool> {
    let tx = conn.transaction()?;
    let kept = commit_snapshot(
        &tx,
        summary,
        Entries::Staged { total },
        CoverUpdate::Set(cover),
    )?;
    tx.commit()?;
    Ok(kept)
}

/// [`commit_snapshot`] of a page holding all of playlist `summary`'s
/// `entries`, in one transaction, its cover left as it is.
#[cfg(test)]
pub(super) fn put_whole(
    conn: &mut Connection,
    summary: &PlaylistSummary,
    entries: &[PlaylistEntry],
) -> rusqlite::Result<bool> {
    let tx = conn.transaction()?;
    let kept = commit_snapshot(&tx, summary, Entries::Given(entries), CoverUpdate::Leave)?;
    tx.commit()?;
    Ok(kept)
}

/// Kept playlist `summary` changed: its summary and search row, so it is
/// found by its name now (a rename comes as a playlist's read, not only
/// as the list's). Nothing for one not kept, or one kept at a later
/// revision. Whether it is kept.
pub(super) fn put_summary(tx: &Transaction, summary: &PlaylistSummary) -> rusqlite::Result<bool> {
    let kept = revisions(tx, &summary.id)?;
    if let Some((kept_revision, _)) = kept {
        put_summary_if_newer(tx, summary, kept_revision)?;
    }
    Ok(kept.is_some())
}

/// The search row of kept playlist `summary`, by its name now.
pub(super) fn put_summary_row(tx: &Transaction, summary: &PlaylistSummary) -> rusqlite::Result<()> {
    let name = super::offline_search::folded(&summary.name);
    put_playlist_row(tx, &summary.id, &name)
}

/// The covers' directory of the database at `db`.
pub(super) fn art_dir(store: &Store, server: &str, user_id: &str) -> PathBuf {
    store.user_dir(server, user_id).join(ART)
}

/// Remove the covers in `dir` no kept playlist names.
pub(super) fn sweep_art(conn: &Connection, dir: &Path) -> rusqlite::Result<()> {
    let mut stmt =
        conn.prepare_cached("SELECT cover_file FROM playlists WHERE cover_file IS NOT NULL")?;
    let named: HashSet<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    if let Ok(files) = std::fs::read_dir(dir) {
        for file in files.filter_map(Result::ok) {
            let name = file.file_name().to_string_lossy().into_owned();
            if !named.contains(&name) {
                let _ = std::fs::remove_file(file.path());
            }
        }
    }
    Ok(())
}
