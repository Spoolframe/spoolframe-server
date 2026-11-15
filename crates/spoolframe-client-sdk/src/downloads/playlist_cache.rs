//! Playlists and hearts kept for offline: every playlist a profile sees,
//! with all its tracks and its cover, and each track's heart, so that with
//! no server an app lists every playlist ("12 of 22 on this iPhone", or
//! greyed with none here) and every heart.
//!
//! Kept in the profile's offline database ([`super::local_db`]): the
//! playlists in the server's order with their summaries, every entry of
//! each (no cap: a playlist re-read replaces only its own rows, in one
//! transaction, read a page at a time so it is never all in memory), and
//! the hearts, one row per track. Covers are files beside it, in
//! `playlist-art/`.
//!
//! Filled by the SDK itself, with no app call:
//! - every read of the playlists list ([`crate::Session::playlists`]),
//!   the app's or the SDK's own, keeps their summaries and order, drops
//!   the ones no longer listed, and queues a read of each playlist whose
//!   tracks are not kept or were kept at another revision, and of each
//!   cover that changed;
//! - a session made or found queues a read of the list, at most once per
//!   [`LIST_EVERY`] per profile (each connect);
//! - a `PlaylistChanged` event queues a read of that playlist (of the list
//!   for one not kept); `PlaylistGone` one too, which drops it when the
//!   server no longer shows it to the user; `LikedChanged` one of Liked;
//! - a read of a playlist's first page that holds all of it
//!   ([`crate::Session::playlist`]) is kept as it is.
//!
//! The queue is read one playlist at a time, in the background, over the
//! connections that are up (it never dials), only while the client is
//! open and its downloads may use the network (no metered link unless
//! cellular is allowed, as for downloads). Metadata only: no audio, and
//! no artwork but each playlist's cover.
//!
//! Hearts are learned from each playlist entry read, each item's details
//! read (`Session::item`, which downloads read too), each like or unlike
//! sent through the session, and Liked read whole: a track kept as liked
//! that Liked no longer holds is unliked.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Weak};
use std::time::Duration;

use iroh::EndpointId;
use parking_lot::Mutex;
use rusqlite::{OptionalExtension, params};
use spoolframe_protocol::ArtworkSize;
use spoolframe_protocol::playlists::{PlaylistPage, PlaylistSummary};
use spoolframe_types::{ArtworkId, Item, MediaId};
use tokio::sync::{Notify, watch};
use tokio::time::Instant;

use super::Shared;
use super::local_db::{self, Local, PLAYLIST_KIND};
use super::store::Store;
use crate::client::{Client, Inner};
use crate::{PlaylistList, SdkError, Session};

/// How long after the SDK read a profile's playlists list it reads it
/// again on its own when a session is made or found.
pub const LIST_EVERY: Duration = Duration::from_secs(5 * 60);
/// How many of a playlist's entries [`super::OfflinePlaylist::entries`]
/// holds; [`super::Downloads::offline_playlist_entries`] pages them all.
pub const INLINE_ENTRIES: u32 = 1_000;
/// A page of a playlist read for the cache: the protocol's most.
const PAGE: u32 = 200;

/// What the background reader is to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Job {
    List,
    Playlist(String),
}

type ServerUser = (EndpointId, String);

/// The background reader's queue, and the profiles' generations.
#[derive(Debug, Default)]
pub(super) struct Reader {
    jobs: Mutex<VecDeque<(ServerUser, Job)>>,
    wake: Notify,
    listed_at: Mutex<HashMap<ServerUser, Instant>>,
    generations: Mutex<Generations>,
}

/// How many times each server, and each profile on it (by username, as
/// it is forgotten), had what is kept of it forgotten. A write asked for
/// before a forget carries the generation of then, and is refused once it
/// differs: a read in flight when its profile was forgotten never brings
/// its database back.
#[derive(Debug, Default)]
struct Generations {
    servers: HashMap<String, u64>,
    users: HashMap<(String, String), u64>,
}

/// A generation ([`Generations`]) of one profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Generation(u64, u64);

impl Generations {
    fn of(&self, server: &str, username: &str) -> Generation {
        Generation(
            self.servers.get(server).copied().unwrap_or_default(),
            self.users
                .get(&(server.to_owned(), username.to_owned()))
                .copied()
                .unwrap_or_default(),
        )
    }

    fn forget(&mut self, server: &str, username: Option<&str>) {
        match username {
            Some(username) => {
                *self
                    .users
                    .entry((server.to_owned(), username.to_owned()))
                    .or_default() += 1;
            }
            None => *self.servers.entry(server.to_owned()).or_default() += 1,
        }
    }
}

/// Whose database a write is for, and the generation it was asked in:
/// taken before the server is asked, so its answer is dropped if the
/// profile was forgotten meanwhile.
#[derive(Debug, Clone)]
pub(crate) struct Writer {
    pub server: EndpointId,
    pub user_id: String,
    pub username: String,
    generation: Generation,
}

impl Reader {
    /// `writer`'s profile was not forgotten since it was taken.
    fn current(&self, writer: &Writer) -> bool {
        self.generations
            .lock()
            .of(&writer.server.to_string(), &writer.username)
            == writer.generation
    }

    /// Queue `job` unless it is queued already: at the head, or last.
    fn enqueue(&self, key: ServerUser, job: Job, front: bool) {
        let mut jobs = self.jobs.lock();
        if !jobs.iter().any(|(k, j)| *k == key && *j == job) {
            if front {
                jobs.push_front((key, job));
            } else {
                jobs.push_back((key, job));
            }
        }
    }

    fn push(&self, key: ServerUser, job: Job) {
        self.enqueue(key, job, false);
        self.wake.notify_one();
    }

    /// Put `job` back at the queue's head, without waking the reader: it
    /// waits on the network instead.
    fn put_back(&self, key: ServerUser, job: Job) {
        self.enqueue(key, job, true);
    }

    #[cfg(test)]
    pub(super) fn queued(&self) -> Vec<Job> {
        self.jobs.lock().iter().map(|(_, j)| j.clone()).collect()
    }

    #[cfg(test)]
    pub(super) fn clear(&self) {
        self.jobs.lock().clear();
    }
}

/// The playlist's cover is to be kept again.
fn cover_stale(kept: Option<&str>, summary: &PlaylistSummary) -> bool {
    kept != summary.artwork.as_ref().map(ToString::to_string).as_deref()
}

fn json(summary: &PlaylistSummary) -> String {
    serde_json::to_string(summary).expect("a summary serializes")
}

fn revision(summary: &PlaylistSummary) -> i64 {
    local_db::revision_of(summary.revision)
}

/// The hearts of a page's tracks.
fn page_hearts(page: &PlaylistPage) -> Vec<(MediaId, bool)> {
    page.entries
        .iter()
        .filter(|e| matches!(e.item, Item::Track(_)))
        .map(|e| (e.item.id().clone(), e.liked))
        .collect()
}

impl Shared {
    /// The writer for `server`'s `user_id` (`username`) now: take it
    /// before asking the server what is to be written.
    pub(crate) fn writer(&self, server: EndpointId, user_id: &str, username: &str) -> Writer {
        let generation = self
            .playlists
            .generations
            .lock()
            .of(&server.to_string(), username);
        Writer {
            server,
            user_id: user_id.to_owned(),
            username: username.to_owned(),
            generation,
        }
    }

    /// Run `task` on `writer`'s database, waited for by nobody: not at
    /// all once its profile was forgotten since it was taken. Nothing
    /// without a downloads directory.
    fn post_db(
        &self,
        writer: &Writer,
        task: impl FnOnce(&mut rusqlite::Connection, &Store) -> rusqlite::Result<()> + Send + 'static,
    ) {
        let Some(dbs) = &self.dbs else {
            return;
        };
        let (reader, writer) = (self.playlists.clone(), writer.clone());
        dbs.post(move |local: &mut Local| {
            if let Some((conn, store)) = writer_db(local, &reader, &writer)
                && let Err(e) = task(conn, &store)
            {
                tracing::warn!(error = %e, "offline database: playlists not kept");
            }
        });
    }

    /// Learn `hearts` of `writer`'s user's tracks, one row each.
    pub(crate) fn learn_hearts(
        &self,
        writer: &Writer,
        hearts: impl IntoIterator<Item = (MediaId, bool)>,
    ) {
        let hearts: Vec<(MediaId, bool)> = hearts.into_iter().collect();
        if hearts.is_empty() {
            return;
        }
        self.post_db(writer, move |conn, _| {
            let tx = conn.transaction()?;
            local_db::learn_hearts(&tx, hearts)?;
            tx.commit()
        });
    }

    /// `writer`'s user read their playlists list: keep it, and queue
    /// what of it is not kept as it is now.
    pub(crate) fn playlists_listed(&self, writer: &Writer, list: &PlaylistList) {
        let key = (writer.server, writer.user_id.clone());
        self.playlists
            .listed_at
            .lock()
            .insert(key.clone(), Instant::now());
        let reader = self.playlists.clone();
        let username = writer.username.clone();
        let summaries = list.playlists.clone();
        self.post_db(writer, move |conn, store| {
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES ('username', ?1)",
                [&username],
            )?;
            let mut before: HashMap<String, (Option<i64>, Option<String>, i64)> = HashMap::new();
            {
                let mut stmt =
                    tx.prepare("SELECT id, read_at, cover_artwork, revision FROM playlists")?;
                for row in
                    stmt.query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?, r.get(3)?))))?
                {
                    let (id, kept) = row?;
                    before.insert(id, kept);
                }
            }
            let listed: Vec<&str> = summaries.iter().map(|s| s.id.as_str()).collect();
            for gone in before.keys().filter(|id| !listed.contains(&id.as_str())) {
                tx.execute("DELETE FROM playlists WHERE id = ?1", [gone])?;
                local_db::drop_row(&tx, PLAYLIST_KIND, gone)?;
            }
            let mut queued = Vec::new();
            for (position, summary) in summaries.iter().enumerate() {
                let old = before.get(&summary.id);
                let read = old.and_then(|(read_at, _, _)| *read_at);
                let cover = old.and_then(|(_, art, _)| art.as_deref());
                // A list answered after a later read of one of its
                // playlists leaves that one's summary as it is: a
                // playlist's revision never moves back.
                let older = old.is_some_and(|(_, _, kept)| *kept > revision(summary));
                if !older && (read != Some(revision(summary)) || cover_stale(cover, summary)) {
                    queued.push(summary.id.clone());
                }
                let position = i64::try_from(position).unwrap_or(i64::MAX);
                match old {
                    None => {
                        tx.execute(
                            "INSERT INTO playlists (id, position, summary, revision)
                             VALUES (?1, ?2, ?3, ?4)",
                            params![summary.id, position, json(summary), revision(summary)],
                        )?;
                        local_db::put_summary_row(&tx, summary)?;
                    }
                    Some((_, _, kept_revision)) => {
                        tx.execute(
                            "UPDATE playlists SET position = ?2 WHERE id = ?1 AND position != ?2",
                            params![summary.id, position],
                        )?;
                        local_db::put_summary_if_newer(&tx, summary, *kept_revision)?;
                    }
                }
            }
            tx.commit()?;
            local_db::sweep_art(conn, &local_db::art_dir(store, &key.0.to_string(), &key.1))?;
            for id in queued {
                reader.push(key.clone(), Job::Playlist(id));
            }
            Ok(())
        });
    }

    /// `writer`'s user read a page of a playlist: keep its summary,
    /// its hearts, and its entries when the page holds all of them; else
    /// queue a read of it, if what is kept is of another revision.
    pub(crate) fn playlist_read(&self, writer: &Writer, page: &PlaylistPage) {
        let reader = self.playlists.clone();
        let key = (writer.server, writer.user_id.clone());
        let summary = page.playlist.clone();
        let hearts = page_hearts(page);
        // Its entries only when the page holds them all: else they are
        // not kept.
        let whole = (page.offset == 0 && page.entries.len() as u64 == u64::from(page.total))
            .then(|| page.entries.clone());
        self.post_db(writer, move |conn, _| {
            let tx = conn.transaction()?;
            local_db::learn_hearts(&tx, hearts)?;
            let kept = match &whole {
                Some(entries) => local_db::commit_snapshot(
                    &tx,
                    &summary,
                    local_db::Entries::Given(entries),
                    local_db::CoverUpdate::Leave,
                )?,
                None => local_db::put_summary(&tx, &summary)?,
            };
            // What is kept now, which a later read may have kept already,
            // says whether it is to be read again: not this page.
            let now: Option<(Option<i64>, i64, Option<String>, String)> = if kept {
                Some(tx.query_row(
                    "SELECT read_at, revision, cover_artwork, summary FROM playlists WHERE id = ?1",
                    [&summary.id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )?)
            } else {
                None
            };
            tx.commit()?;
            let Some((read_at, kept_revision, cover, kept_summary)) = now else {
                // Not listed yet: the list is read again, for its place.
                reader.push(key, Job::List);
                return Ok(());
            };
            let stale_cover = serde_json::from_str::<PlaylistSummary>(&kept_summary)
                .map_or(true, |kept| cover_stale(cover.as_deref(), &kept));
            if stale_cover || read_at != Some(kept_revision) {
                reader.push(key, Job::Playlist(summary.id.clone()));
            }
            Ok(())
        });
    }

    /// Server `server` told that playlist `id` changed (`Some` revision)
    /// or is gone (`None`): each user with a database there has it read
    /// again, or the list when it is not kept.
    pub(crate) fn playlist_changed(&self, server: EndpointId, id: &str, revision: Option<u64>) {
        let id = id.to_owned();
        self.each_user(server, move |conn| {
            let read_at: Option<Option<i64>> = conn
                .query_row("SELECT read_at FROM playlists WHERE id = ?1", [&id], |r| {
                    r.get(0)
                })
                .optional()?;
            Ok(match (read_at, revision) {
                (None, _) => Some(Job::List),
                (Some(read), Some(revision)) if read == Some(local_db::revision_of(revision)) => {
                    None
                }
                _ => Some(Job::Playlist(id.clone())),
            })
        });
    }

    /// Server `server` told that a user's Liked changed: read it again.
    pub(crate) fn liked_changed(&self, server: EndpointId) {
        self.each_user(server, |conn| {
            let liked: Option<String> = conn
                .query_row(
                    "SELECT id FROM playlists WHERE json_extract(summary, '$.builtin') = 'liked'",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(Some(liked.map_or(Job::List, Job::Playlist)))
        });
    }

    /// For each user of `server` with a database here, the job `decide`
    /// says to queue.
    fn each_user(
        &self,
        server: EndpointId,
        decide: impl Fn(&rusqlite::Connection) -> rusqlite::Result<Option<Job>> + Send + 'static,
    ) {
        let Some(dbs) = &self.dbs else {
            return;
        };
        let reader = self.playlists.clone();
        dbs.post(move |local: &mut Local| {
            for user in local.users_with_db(&server.to_string()) {
                let Some(conn) = local.db(&server.to_string(), &user) else {
                    continue;
                };
                match decide(conn) {
                    Ok(Some(job)) => reader.push((server, user), job),
                    Ok(None) => {}
                    Err(e) => tracing::warn!(error = %e, "offline database: event not followed"),
                }
            }
        });
    }

    /// A session of `server`'s `user_id` was made or found: read their
    /// list, unless the SDK did within [`LIST_EVERY`].
    pub(super) fn playlists_due(&self, server: EndpointId, user_id: &str) {
        let key = (server, user_id.to_owned());
        let recent = self
            .playlists
            .listed_at
            .lock()
            .get(&key)
            .is_some_and(|at| at.elapsed() < LIST_EVERY);
        if !recent {
            self.playlists.push(key, Job::List);
        }
    }

    /// Forget what is kept of `server`'s playlists, hearts and index:
    /// every profile's databases, or with `username` only theirs.
    ///
    /// The profile's generation moves first, so a read in flight (the
    /// background reader's, or a screen's) has its answer dropped and
    /// never makes the database again; its queued reads go, and the
    /// reader's session of it with them.
    pub(crate) async fn forget_playlists(&self, server: &str, username: Option<&str>) {
        self.playlists.generations.lock().forget(server, username);
        // Whose: the sessions' users of that name (or every one of the
        // server's), and the databases kept under it.
        let mut gone: Vec<String> = self
            .sessions
            .lock()
            .iter()
            .filter(|((s, _), parts)| {
                s.to_string() == server && username.is_none_or(|u| parts.user.username == u)
            })
            .map(|((_, user), _)| user.clone())
            .collect();
        if let Some(dbs) = &self.dbs {
            let (s, u) = (server.to_owned(), username.map(str::to_owned));
            if let Some(users) = dbs
                .call_async(move |local: &mut Local| local.forget(&s, u.as_deref()))
                .await
            {
                gone.extend(users);
            }
        }
        let theirs = |s: &EndpointId, user: &String| {
            s.to_string() == server && (username.is_none() || gone.contains(user))
        };
        self.playlists
            .jobs
            .lock()
            .retain(|((s, user), _)| !theirs(s, user));
        self.playlists
            .listed_at
            .lock()
            .retain(|(s, user), _| !theirs(s, user));
        self.sessions.lock().retain(|(s, user), _| !theirs(s, user));
    }

    /// Wait until every change posted to the databases so far is made.
    #[cfg(test)]
    pub(super) fn flush_db(&self) {
        if let Some(dbs) = &self.dbs {
            dbs.call(|_: &mut Local| ());
        }
    }
}

/// `writer`'s database, on the database thread, with the store: `None`
/// once its profile was forgotten since `writer` was taken, or when it
/// cannot be opened. Noted as its username's ([`Local::named`]).
fn writer_db<'a>(
    local: &'a mut Local,
    reader: &Reader,
    writer: &Writer,
) -> Option<(&'a mut rusqlite::Connection, Store)> {
    if !reader.current(writer) {
        return None;
    }
    let server = writer.server.to_string();
    local.named(&server, &writer.user_id, &writer.username);
    let store = local.store.clone();
    let conn = local.db(&server, &writer.user_id)?;
    Some((conn, store))
}

/// Run `task` on `writer`'s database and await it: refused once its
/// profile was forgotten since it was taken.
async fn on_db<R: Send + 'static>(
    shared: &Shared,
    writer: &Writer,
    task: impl FnOnce(&mut rusqlite::Connection, &Store) -> rusqlite::Result<R> + Send + 'static,
) -> Result<R, SdkError> {
    let dbs = shared
        .dbs
        .as_ref()
        .ok_or_else(|| SdkError::Downloads("no ClientOptions::downloads_dir".into()))?;
    let (reader, writer) = (shared.playlists.clone(), writer.clone());
    let answer = dbs
        .call_async(move |local: &mut Local| {
            if !reader.current(&writer) {
                return Err("the profile was forgotten".to_owned());
            }
            let (conn, store) =
                writer_db(local, &reader, &writer).ok_or_else(|| "cannot be opened".to_owned())?;
            task(conn, &store).map_err(|e| e.to_string())
        })
        .await;
    match answer {
        Some(Ok(r)) => Ok(r),
        Some(Err(e)) => Err(SdkError::Downloads(format!("offline database: {e}"))),
        None => Err(SdkError::Downloads("offline database: gone".into())),
    }
}

/// How many times a playlist edited while it is read is read again from
/// its start before it is left for its next change or list read.
pub(super) const READ_ATTEMPTS: usize = 3;

/// Where a playlist's pages and its cover are read: the session, or a
/// test's stand-in.
pub(super) trait Pages {
    async fn page(&self, id: &str, offset: u32, limit: u32) -> Result<PlaylistPage, SdkError>;
    async fn cover(&self, art: &ArtworkId) -> Result<Vec<u8>, SdkError>;
}

impl Pages for Session {
    async fn page(&self, id: &str, offset: u32, limit: u32) -> Result<PlaylistPage, SdkError> {
        self.playlist_raw(id, offset, limit).await
    }

    async fn cover(&self, art: &ArtworkId) -> Result<Vec<u8>, SdkError> {
        self.artwork(art, ArtworkSize::Large).await
    }
}

/// What one read of a playlist's pages came to.
enum Snapshot {
    /// Every page, all of one revision: staged.
    Whole {
        summary: PlaylistSummary,
        total: u32,
    },
    /// The server no longer shows it to the user.
    Gone,
    /// A page of another revision or total than the first: it was edited
    /// while it was read.
    Mixed,
    /// The client closed, or the pages ran out before the total.
    Cut,
    /// The device is on a mobile network that downloads may not use
    /// ([`Shared::network_held`]): left for one that allows it.
    Held,
}

/// `request`'s answer, unless the network is held
/// ([`Shared::network_held`]) before it is asked or while it is waited
/// on: then `None`, and it is let go of unanswered.
async fn unless_held<T>(shared: &Shared, request: impl Future<Output = T>) -> Option<T> {
    let mut changed = shared.interrupt.subscribe();
    if shared.network_held() {
        return None;
    }
    tokio::select! {
        biased;
        () = until_held(shared, &mut changed, true) => None,
        answer = request => Some(answer),
    }
}

/// Until the network is held ([`Shared::network_held`]) when `held`, or
/// allows downloads when not, as `changed` (the downloads' interrupt,
/// subscribed before it was last looked at) tells it moved. Never, once
/// the interrupt is gone.
async fn until_held(shared: &Shared, changed: &mut watch::Receiver<u64>, held: bool) {
    while shared.network_held() != held {
        if changed.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// Read playlist `id` of `session`'s user whole, a page at a time, and its
/// cover, and keep them; drop it when the server no longer shows it to
/// them.
async fn read_one(shared: &Shared, session: &Session, id: &str) -> Result<(), SdkError> {
    let writer = session.offline_writer();
    read_with(shared, session, &writer, id).await
}

/// [`read_one`] from `pages`. Every page must be of the first one's
/// revision: one edited while it is read is read again from its start, up
/// to [`READ_ATTEMPTS`] times, and is never kept mixed, so no track is
/// doubled or lost and Liked never unlikes a track it was only reordered
/// past.
pub(super) async fn read_with(
    shared: &Shared,
    pages: &impl Pages,
    writer: &Writer,
    id: &str,
) -> Result<(), SdkError> {
    let (server, user_id) = (&writer.server.to_string(), &writer.user_id);
    let store = shared.store()?.clone();
    let mut held = false;
    for _ in 0..READ_ATTEMPTS {
        let staged = match stage_all(shared, pages, writer, id).await {
            Ok(staged) => staged,
            Err(e) => {
                // What was staged goes with the read; refused, as the
                // read was, once the profile is forgotten.
                let _ = drop_staging(shared, writer, id).await;
                return Err(e);
            }
        };
        match staged {
            Snapshot::Whole { summary, total } => {
                match commit(shared, &store, pages, writer, summary, total).await? {
                    Committed::Kept => return Ok(()),
                    Committed::Held => {
                        held = true;
                        break;
                    }
                }
            }
            Snapshot::Gone => {
                let (id, art) = (id.to_owned(), local_db::art_dir(&store, server, user_id));
                return on_db(shared, writer, move |conn, _| {
                    let tx = conn.transaction()?;
                    tx.execute("DELETE FROM playlists WHERE id = ?1", [&id])?;
                    tx.execute("DELETE FROM playlist_staging WHERE playlist = ?1", [&id])?;
                    local_db::drop_row(&tx, PLAYLIST_KIND, &id)?;
                    tx.commit()?;
                    local_db::sweep_art(conn, &art)
                })
                .await;
            }
            Snapshot::Cut => break,
            Snapshot::Held => {
                held = true;
                break;
            }
            Snapshot::Mixed => {
                tracing::debug!(
                    playlist = id,
                    "a playlist changed while it was read; read again"
                );
            }
        }
    }
    // Left for its next change or list read, or, held, for a network
    // that allows it: what was staged goes.
    drop_staging(shared, writer, id).await?;
    if held {
        shared.playlists.put_back(
            (writer.server, writer.user_id.clone()),
            Job::Playlist(id.to_owned()),
        );
    }
    Ok(())
}

/// Drop what is staged of playlist `id`.
async fn drop_staging(shared: &Shared, writer: &Writer, id: &str) -> Result<(), SdkError> {
    let id = id.to_owned();
    on_db(shared, writer, move |conn, _| {
        conn.execute("DELETE FROM playlist_staging WHERE playlist = ?1", [&id])
            .map(drop)
    })
    .await
}

/// Read and stage every page of playlist `id`.
async fn stage_all(
    shared: &Shared,
    pages: &impl Pages,
    writer: &Writer,
    id: &str,
) -> Result<Snapshot, SdkError> {
    let first = match unless_held(shared, pages.page(id, 0, PAGE)).await {
        None => return Ok(Snapshot::Held),
        Some(Ok(page)) => page,
        Some(Err(SdkError::NotFound(_))) => return Ok(Snapshot::Gone),
        Some(Err(e)) => return Err(e),
    };
    let total = first.total;
    let summary = first.playlist.clone();
    let mut page = first;
    let mut read: u32 = 0;
    loop {
        if page.playlist.revision != summary.revision || page.total != total {
            return Ok(Snapshot::Mixed);
        }
        let got = u32::try_from(page.entries.len()).unwrap_or(u32::MAX);
        let (pid, offset, hearts) = (id.to_owned(), read, page_hearts(&page));
        let entries = std::mem::take(&mut page.entries);
        on_db(shared, writer, move |conn, _| {
            let tx = conn.transaction()?;
            local_db::stage(&tx, &pid, offset, &entries)?;
            local_db::learn_hearts(&tx, hearts)?;
            tx.commit()
        })
        .await?;
        read = read.saturating_add(got);
        if got == 0 || read >= total || *shared.closed.borrow() {
            break;
        }
        // Looked at before each page, and while it is waited on: a move
        // onto a mobile network stops the read at once.
        page = match unless_held(shared, pages.page(id, read, PAGE)).await {
            Some(page) => page?,
            None => return Ok(Snapshot::Held),
        };
    }
    Ok(match read >= total {
        true => Snapshot::Whole { summary, total },
        false => Snapshot::Cut,
    })
}

/// What [`commit`] came to.
enum Committed {
    /// Kept, or left for a later read already kept.
    Kept,
    /// The network was held before its cover came: nothing kept.
    Held,
}

/// Keep what [`stage_all`] staged of `summary`, with its cover.
async fn commit(
    shared: &Shared,
    store: &Store,
    pages: &impl Pages,
    writer: &Writer,
    summary: PlaylistSummary,
    total: u32,
) -> Result<Committed, SdkError> {
    let (server, user_id) = (&writer.server.to_string(), &writer.user_id);
    let old_cover: Option<(String, String)> = {
        let id = summary.id.clone();
        on_db(shared, writer, move |conn, _| {
            conn.query_row(
                "SELECT cover_artwork, cover_file FROM playlists WHERE id = ?1",
                [&id],
                |r| Ok(r.get::<_, Option<String>>(0)?.zip(r.get(1)?)),
            )
            .optional()
            .map(Option::flatten)
        })
        .await?
    };
    // A cover is fetched here and written on the database thread, with
    // the commit: there the profile's generation is checked first, so one
    // forgotten while the cover was fetched gets no file and no folder.
    let fetched = match &summary.artwork {
        Some(art)
            if old_cover
                .as_ref()
                .is_some_and(|(a, _)| *a == art.to_string()) =>
        {
            Cover::Kept(old_cover)
        }
        Some(art) => match unless_held(shared, pages.cover(art)).await {
            None => return Ok(Committed::Held),
            Some(Ok(bytes)) => Cover::Fetched(art.to_string(), bytes),
            Some(Err(e)) => {
                tracing::debug!(id = %art, error = %e, "a playlist's cover not kept");
                Cover::Kept(None)
            }
        },
        None => Cover::Kept(None),
    };
    let art = local_db::art_dir(store, server, user_id);
    on_db(shared, writer, move |conn, _| {
        let cover = match fetched {
            Cover::Kept(cover) => cover,
            Cover::Fetched(art_id, bytes) => keep_cover(&art, art_id, &bytes),
        };
        local_db::commit_staged(conn, &summary, total, cover)?;
        local_db::sweep_art(conn, &art)
    })
    .await?;
    Ok(Committed::Kept)
}

/// A playlist's cover for [`commit`]: as kept already (or none), or
/// fetched now and still to be written.
enum Cover {
    Kept(Option<(String, String)>),
    Fetched(String, Vec<u8>),
}

/// Write cover `art_id`'s `bytes` in `dir`: its artwork id and file;
/// `None` when it cannot be. Only on the database thread, once the
/// profile's generation was found current.
fn keep_cover(dir: &Path, art_id: String, bytes: &[u8]) -> Option<(String, String)> {
    let file = super::manager::art_file_name(&art_id);
    match crate::storage::files::write(&dir.join(&file), bytes) {
        Ok(()) => Some((art_id, file)),
        Err(e) => {
            tracing::warn!(error = %e, "a playlist's cover not kept");
            None
        }
    }
}

/// Until the client is closed or gone: read what is queued, one at a
/// time, over the connections that are up; it never dials. Holds the
/// client only while it reads.
pub(super) async fn read_while_connected(client: Weak<Inner>, shared: Arc<Shared>) {
    let mut closed = shared.closed.subscribe();
    loop {
        let next = shared.playlists.jobs.lock().pop_front();
        let Some(((server, user), job)) = next else {
            tokio::select! {
                () = shared.playlists.wake.notified() => continue,
                _ = closed.wait_for(|closed| *closed) => return,
            }
        };
        if *shared.closed.borrow() {
            return;
        }
        // Metered with cellular not allowed: back in the queue, read
        // once the network or the settings allow it.
        if shared.network_held() {
            shared.playlists.put_back((server, user), job);
            let mut changed = shared.interrupt.subscribe();
            tokio::select! {
                () = until_held(&shared, &mut changed, false) => continue,
                _ = closed.wait_for(|closed| *closed) => return,
            }
        }
        let Some(inner) = client.upgrade() else {
            return;
        };
        let client = Client { inner };
        if client.link(server).open().is_none() {
            continue;
        }
        let parts = shared.sessions.lock().get(&(server, user.clone())).cloned();
        let Some(parts) = parts else {
            continue;
        };
        let session = parts.attach(client);
        let read = match &job {
            Job::List => session.playlists_fresh().await.map(drop),
            Job::Playlist(id) => read_one(&shared, &session, id).await,
        };
        if let Err(e) = read {
            tracing::info!(%server, ?job, error = %e, "a playlist not kept for offline; read again later");
        }
    }
}

#[cfg(test)]
#[path = "playlist_cache_tests.rs"]
mod tests;
