//! Playlist rows (PX-40): playlists, their shares and entries, and each
//! user's pins. Who may do what is `spoolframe_policy::playlists`, and
//! what an entry's item is the catalog's: `crate::playlists` brings them
//! together. This is storage.

use std::collections::{HashMap, HashSet};

use spoolframe_policy::playlists::{self as policy, Since, Step};
use spoolframe_protocol::playlists::{EntryId, Kinds, PlaySettings, Repeat};
use sqlx::{QueryBuilder, Sqlite};

use super::{Store, StoreError, decode_error};

/// How many revisions of entry changes are kept for redoing an editor's
/// edit made on an older one: an editor further behind reloads.
const RECORDED_REVISIONS: i64 = 500;

#[derive(Debug, thiserror::Error)]
pub enum PlaylistError {
    #[error("no such playlist")]
    NotFound,
    /// An edit made on an older revision touches an entry changed since.
    #[error("the playlist changed since that revision")]
    Conflict,
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<policy::Conflict> for PlaylistError {
    fn from(_: policy::Conflict) -> Self {
        Self::Conflict
    }
}

/// One playlist, with its shares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistRow {
    pub id: String,
    /// A `Principal`, as text.
    pub owner: String,
    pub name: String,
    pub kinds: Kinds,
    pub server_wide: bool,
    pub revision: u64,
    pub created_at: i64,
    pub updated_at: i64,
    pub shares: Vec<ShareRow>,
}

impl PlaylistRow {
    /// `principal`'s share of it, if it has one: `Some(true)` to edit.
    pub fn share_of(&self, principal: &str) -> Option<bool> {
        self.shares
            .iter()
            .find(|s| s.principal == principal)
            .map(|s| s.can_edit)
    }
}

/// Who a playlist is shared with.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ShareRow {
    pub principal: String,
    pub can_edit: bool,
}

/// One entry of a playlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryRow {
    pub entry_id: EntryId,
    pub media_id: String,
    /// `movie`, `episode` or `track`.
    pub kind: String,
    pub show_id: Option<String>,
    pub album_id: Option<String>,
    pub artist_id: Option<String>,
}

/// An entry to add: an item, with what it is found by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEntry {
    pub media_id: String,
    pub kind: String,
    pub show_id: Option<String>,
    pub album_id: Option<String>,
    pub artist_id: Option<String>,
    pub external_ids: Vec<String>,
}

/// A playlist to make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPlaylist {
    pub owner: String,
    pub name: String,
    pub kinds: Kinds,
    pub server_wide: bool,
    pub shares: Vec<ShareRow>,
}

/// A change to a playlist's name, kinds or sharing. `None` leaves it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlaylistChange {
    pub name: Option<String>,
    pub kinds: Option<Kinds>,
    pub server_wide: Option<bool>,
    pub shares: Option<Vec<ShareRow>>,
}

/// One edit, its items resolved to entries to add.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryEdit {
    Add {
        entries: Vec<NewEntry>,
        before: Option<EntryId>,
    },
    Remove(EntryId),
    Move {
        entry: EntryId,
        before: Option<EntryId>,
    },
}

impl EntryEdit {
    fn touches(&self) -> Option<EntryId> {
        match self {
            EntryEdit::Remove(entry) | EntryEdit::Move { entry, .. } => Some(*entry),
            EntryEdit::Add { .. } => None,
        }
    }
}

/// A user's place in a playlist and how they play it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlaceRow {
    /// The entry playing or last played; `None`: from the top.
    pub entry: Option<EntryId>,
    pub settings: PlaySettings,
    /// The shuffled round being played, in its order.
    pub round: Option<Vec<EntryId>>,
}

fn repeat_text(repeat: Repeat) -> &'static str {
    match repeat {
        Repeat::One => "one",
        Repeat::All => "all",
        Repeat::Off | Repeat::Unknown => "off",
    }
}

fn repeat_of(text: &str) -> Repeat {
    match text {
        "one" => Repeat::One,
        "all" => Repeat::All,
        _ => Repeat::Off,
    }
}

/// What [`Store::playlists_holding`] finds entries by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeldBy {
    /// The item itself.
    Item,
    /// An episode's show.
    Show,
    /// A track's album.
    Album,
    /// A track's artist.
    Artist,
}

impl HeldBy {
    /// Its column: a constant, never input.
    fn column(self) -> &'static str {
        match self {
            HeldBy::Item => "media_id",
            HeldBy::Show => "show_id",
            HeldBy::Album => "album_id",
            HeldBy::Artist => "artist_id",
        }
    }
}

/// Why a pinned playlist left someone's Pinned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinNoteReason {
    Unshared,
    Deleted,
}

impl PinNoteReason {
    fn as_str(self) -> &'static str {
        match self {
            PinNoteReason::Unshared => "unshared",
            PinNoteReason::Deleted => "deleted",
        }
    }
}

/// A pinned playlist that left Pinned, until dismissed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinNoteRow {
    pub playlist_id: String,
    pub name: String,
    pub reason: PinNoteReason,
    pub noted_at: i64,
}

fn kinds_bits(kinds: Kinds) -> i64 {
    i64::from(kinds.movies) | i64::from(kinds.episodes) << 1 | i64::from(kinds.tracks) << 2
}

fn kinds_of(bits: i64) -> Kinds {
    Kinds {
        movies: bits & 1 != 0,
        episodes: bits & 2 != 0,
        tracks: bits & 4 != 0,
    }
}

type PlaylistTuple = (String, String, String, i64, bool, i64, i64, i64);

type PlaceTuple = (Option<i64>, bool, String, bool, Option<String>);

type EntryTuple = (
    i64,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

const PLAYLIST_COLUMNS: &str =
    "p.id, p.owner, p.name, p.kinds, p.server_wide, p.revision, p.created_at, p.updated_at";

fn playlist_of(row: PlaylistTuple, shares: Vec<ShareRow>) -> PlaylistRow {
    let (id, owner, name, kinds, server_wide, revision, created_at, updated_at) = row;
    PlaylistRow {
        id,
        owner,
        name,
        kinds: kinds_of(kinds),
        server_wide,
        revision: revision.max(0) as u64,
        created_at,
        updated_at,
        shares,
    }
}

/// `entry_id` as SQLite keeps it.
fn db_id(entry: EntryId) -> i64 {
    i64::try_from(entry).unwrap_or(i64::MAX)
}

impl Store {
    /// Make a playlist: its id, a new UUIDv7.
    pub async fn create_playlist(&self, new: &NewPlaylist) -> Result<PlaylistRow, StoreError> {
        let id = uuid::Uuid::now_v7().to_string();
        let now = self.now();
        let mut tx = self.begin_write().await?;
        sqlx::query(
            "INSERT INTO playlists (id, owner, name, kinds, server_wide, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        )
        .bind(&id)
        .bind(&new.owner)
        .bind(&new.name)
        .bind(kinds_bits(new.kinds))
        .bind(new.server_wide)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        write_shares(&mut tx, &id, &new.shares).await?;
        tx.commit().await?;
        Ok(PlaylistRow {
            id,
            owner: new.owner.clone(),
            name: new.name.clone(),
            kinds: new.kinds,
            server_wide: new.server_wide,
            revision: 0,
            created_at: now,
            updated_at: now,
            shares: new.shares.clone(),
        })
    }

    /// One playlist, whoever may see it.
    pub async fn playlist(&self, id: &str) -> Result<Option<PlaylistRow>, StoreError> {
        // Its columns are this module's constant, never input.
        let row: Option<PlaylistTuple> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {PLAYLIST_COLUMNS} FROM playlists p WHERE p.id = ?1"
        )))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let mut shares = self.shares_of(std::slice::from_ref(&row.0)).await?;
        Ok(Some(playlist_of(
            row,
            shares.remove(id).unwrap_or_default(),
        )))
    }

    /// The playlists `principal` sees: their own, those shared with them,
    /// and the server-wide ones. Oldest first.
    pub async fn playlists_seen_by(&self, principal: &str) -> Result<Vec<PlaylistRow>, StoreError> {
        let rows: Vec<PlaylistTuple> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {PLAYLIST_COLUMNS} FROM playlists p \
             WHERE p.owner = ?1 OR p.server_wide = 1 \
                OR EXISTS (SELECT 1 FROM playlist_shares s \
                           WHERE s.playlist_id = p.id AND s.principal = ?1) \
             ORDER BY p.created_at, p.id"
        )))
        .bind(principal)
        .fetch_all(&self.pool)
        .await?;
        let ids: Vec<String> = rows.iter().map(|r| r.0.clone()).collect();
        let mut shares = self.shares_of(&ids).await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                let shares = shares.remove(&row.0).unwrap_or_default();
                playlist_of(row, shares)
            })
            .collect())
    }

    /// The playlists `principal` sees that hold an entry whose `by` is one
    /// of `values`: an item's screen's "in these playlists". Oldest first.
    pub async fn playlists_holding(
        &self,
        principal: &str,
        by: HeldBy,
        values: &[String],
    ) -> Result<Vec<PlaylistRow>, StoreError> {
        if values.is_empty() {
            return Ok(Vec::new());
        }
        let mut query: QueryBuilder<Sqlite> = QueryBuilder::new(format!(
            "SELECT {PLAYLIST_COLUMNS} FROM playlists p WHERE (p.owner = "
        ));
        query
            .push_bind(principal)
            .push(
                " OR p.server_wide = 1 OR EXISTS (SELECT 1 FROM playlist_shares s \
                 WHERE s.playlist_id = p.id AND s.principal = ",
            )
            .push_bind(principal)
            .push(
                ")) AND EXISTS (SELECT 1 FROM playlist_entries e WHERE e.playlist_id = p.id AND e.",
            )
            .push(by.column())
            .push(" IN (");
        let mut list = query.separated(", ");
        for value in values {
            list.push_bind(value);
        }
        query.push(")) ORDER BY p.created_at, p.id");
        let rows: Vec<PlaylistTuple> = query.build_query_as().fetch_all(&self.pool).await?;
        let ids: Vec<String> = rows.iter().map(|r| r.0.clone()).collect();
        let mut shares = self.shares_of(&ids).await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                let shares = shares.remove(&row.0).unwrap_or_default();
                playlist_of(row, shares)
            })
            .collect())
    }

    /// The shares of `ids`, by playlist.
    async fn shares_of(
        &self,
        ids: &[String],
    ) -> Result<HashMap<String, Vec<ShareRow>>, StoreError> {
        let mut shares: HashMap<String, Vec<ShareRow>> = HashMap::new();
        if ids.is_empty() {
            return Ok(shares);
        }
        let mut query: QueryBuilder<Sqlite> = QueryBuilder::new(
            "SELECT playlist_id, principal, can_edit FROM playlist_shares WHERE playlist_id IN (",
        );
        let mut list = query.separated(", ");
        for id in ids {
            list.push_bind(id);
        }
        query.push(") ORDER BY principal");
        let rows: Vec<(String, String, bool)> =
            query.build_query_as().fetch_all(&self.pool).await?;
        for (playlist, principal, can_edit) in rows {
            shares.entry(playlist).or_default().push(ShareRow {
                principal,
                can_edit,
            });
        }
        Ok(shares)
    }

    /// Change a playlist's name, kinds or sharing; its new revision. Who
    /// can no longer see it loses its pin, with a note.
    pub async fn update_playlist(
        &self,
        id: &str,
        change: &PlaylistChange,
    ) -> Result<u64, PlaylistError> {
        let now = self.now();
        let mut tx = self.begin_write().await?;
        let revision = bump(&mut tx, id, now).await?;
        if let Some(name) = &change.name {
            sqlx::query("UPDATE playlists SET name = ?2 WHERE id = ?1")
                .bind(id)
                .bind(name)
                .execute(&mut *tx)
                .await?;
        }
        if let Some(kinds) = change.kinds {
            sqlx::query("UPDATE playlists SET kinds = ?2 WHERE id = ?1")
                .bind(id)
                .bind(kinds_bits(kinds))
                .execute(&mut *tx)
                .await?;
        }
        if let Some(server_wide) = change.server_wide {
            sqlx::query("UPDATE playlists SET server_wide = ?2 WHERE id = ?1")
                .bind(id)
                .bind(server_wide)
                .execute(&mut *tx)
                .await?;
        }
        if let Some(shares) = &change.shares {
            sqlx::query("DELETE FROM playlist_shares WHERE playlist_id = ?1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            write_shares(&mut tx, id, shares).await?;
        }
        // Whoever pinned it and sees it no more: not its owner, not shared
        // with, and it is not server-wide.
        let unseen = "playlist_pins.playlist_id = ?1 \
             AND NOT EXISTS (SELECT 1 FROM playlists p WHERE p.id = ?1 \
                             AND (p.server_wide = 1 OR p.owner = playlist_pins.principal)) \
             AND NOT EXISTS (SELECT 1 FROM playlist_shares s WHERE s.playlist_id = ?1 \
                             AND s.principal = playlist_pins.principal)";
        note_pins(&mut tx, id, unseen, None, PinNoteReason::Unshared, now).await?;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM playlist_pins WHERE {unseen}"
        )))
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(revision)
    }

    /// Delete a playlist, and with it its entries, shares, pins and
    /// places. Whoever pinned it, but `by`, gets a note.
    pub async fn delete_playlist(&self, id: &str, by: &str) -> Result<(), PlaylistError> {
        let now = self.now();
        let mut tx = self.begin_write().await?;
        note_pins(
            &mut tx,
            id,
            "playlist_pins.playlist_id = ?1",
            Some(by),
            PinNoteReason::Deleted,
            now,
        )
        .await?;
        let deleted = sqlx::query("DELETE FROM playlists WHERE id = ?1")
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if deleted == 0 {
            return Err(PlaylistError::NotFound);
        }
        tx.commit().await?;
        Ok(())
    }

    /// A playlist's entries, in order.
    pub async fn playlist_entries(&self, id: &str) -> Result<Vec<EntryRow>, StoreError> {
        let rows: Vec<EntryTuple> = sqlx::query_as(
            "SELECT entry_id, media_id, kind, show_id, album_id, artist_id \
                 FROM playlist_entries WHERE playlist_id = ?1 ORDER BY position, entry_id",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(entry_id, media_id, kind, show_id, album_id, artist_id)| EntryRow {
                    entry_id: entry_id.max(0) as EntryId,
                    media_id,
                    kind,
                    show_id,
                    album_id,
                    artist_id,
                },
            )
            .collect())
    }

    /// Apply `edits`, made on revision `base`, to playlist `id`'s entries:
    /// its new revision. Redone on a newer revision as
    /// [`policy::rebase`] allows, else [`PlaylistError::Conflict`] and
    /// nothing changed.
    pub async fn edit_playlist(
        &self,
        id: &str,
        base: u64,
        edits: Vec<EntryEdit>,
    ) -> Result<u64, PlaylistError> {
        let now = self.now();
        let mut tx = self.begin_write().await?;
        let (current, recorded_from): (i64, i64) =
            sqlx::query_as("SELECT revision, recorded_from FROM playlists WHERE id = ?1")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(PlaylistError::NotFound)?;
        let touched: Vec<i64> = sqlx::query_scalar(
            "SELECT entry_id FROM playlist_edits WHERE playlist_id = ?1 AND revision > ?2",
        )
        .bind(id)
        .bind(i64::try_from(base).unwrap_or(i64::MAX))
        .fetch_all(&mut *tx)
        .await?;
        let since = Since {
            base,
            current: current.max(0) as u64,
            recorded_from: recorded_from.max(0) as u64,
            touched: touched.into_iter().map(|e| e.max(0) as EntryId).collect(),
        };
        policy::rebase(&since, edits.iter().filter_map(EntryEdit::touches))?;
        let mut order = entry_order(&mut tx, id).await?;
        let mut touched_now = Vec::new();
        let mut removed = Vec::new();
        for edit in edits {
            let step = match edit {
                EntryEdit::Add { entries, before } => {
                    let mut added = Vec::with_capacity(entries.len());
                    for entry in &entries {
                        added.push(insert_entry(&mut tx, id, entry, now).await?);
                    }
                    Step::Insert {
                        entries: added,
                        before,
                    }
                }
                EntryEdit::Remove(entry) => {
                    touched_now.push(entry);
                    removed.push(entry);
                    Step::Remove(entry)
                }
                EntryEdit::Move { entry, before } => {
                    touched_now.push(entry);
                    Step::Move { entry, before }
                }
            };
            policy::apply(&mut order, step)?;
        }
        for entry in removed {
            sqlx::query("DELETE FROM playlist_entries WHERE entry_id = ?1 AND playlist_id = ?2")
                .bind(db_id(entry))
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        write_order(&mut tx, &order).await?;
        let revision = bump(&mut tx, id, now).await?;
        record(&mut tx, id, revision, &touched_now).await?;
        tx.commit().await?;
        Ok(revision)
    }

    /// Take `entries` out of playlist `id`: their items left the library.
    /// Not anyone's edit, but an edit made before it that touches one of
    /// them is refused as if it were. The new revision; `None` when none
    /// of them was there.
    pub async fn drop_playlist_entries(
        &self,
        id: &str,
        entries: &[EntryId],
    ) -> Result<Option<u64>, PlaylistError> {
        let now = self.now();
        let mut tx = self.begin_write().await?;
        let mut dropped = Vec::new();
        for entry in entries {
            let gone = sqlx::query(
                "DELETE FROM playlist_entries WHERE entry_id = ?1 AND playlist_id = ?2",
            )
            .bind(db_id(*entry))
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if gone > 0 {
                dropped.push(*entry);
            }
        }
        if dropped.is_empty() {
            return Ok(None);
        }
        let order = entry_order(&mut tx, id).await?;
        write_order(&mut tx, &order).await?;
        let revision = bump(&mut tx, id, now).await?;
        record(&mut tx, id, revision, &dropped).await?;
        tx.commit().await?;
        Ok(Some(revision))
    }

    /// Entry `entry`'s item has a new key (a Plex rebuild): it follows,
    /// keeping its place and id. Playlist `id`'s new revision.
    pub async fn relink_playlist_entry(
        &self,
        id: &str,
        entry: EntryId,
        media_id: &str,
    ) -> Result<u64, PlaylistError> {
        let now = self.now();
        let mut tx = self.begin_write().await?;
        let moved = sqlx::query(
            "UPDATE playlist_entries SET media_id = ?3 WHERE entry_id = ?1 AND playlist_id = ?2",
        )
        .bind(db_id(entry))
        .bind(id)
        .bind(media_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if moved == 0 {
            return Err(PlaylistError::NotFound);
        }
        let revision = bump(&mut tx, id, now).await?;
        tx.commit().await?;
        tracing::info!(
            playlist = id,
            entry,
            to = media_id,
            "playlist entry re-linked"
        );
        Ok(revision)
    }

    /// The stable ids an entry's item had when it was added.
    pub async fn playlist_entry_ids(&self, entry: EntryId) -> Result<Vec<String>, StoreError> {
        Ok(sqlx::query_scalar(
            "SELECT external_id FROM playlist_entry_ids WHERE entry_id = ?1 ORDER BY external_id",
        )
        .bind(db_id(entry))
        .fetch_all(&self.pool)
        .await?)
    }

    /// `principal`'s pinned playlists, in their order.
    pub async fn pins(&self, principal: &str) -> Result<Vec<String>, StoreError> {
        Ok(sqlx::query_scalar(
            "SELECT playlist_id FROM playlist_pins WHERE principal = ?1 ORDER BY position",
        )
        .bind(principal)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Pin playlist `id` for `principal` at `position` in their order (0
    /// first; past the end, last). A playlist pinned already moves there.
    pub async fn pin_playlist(
        &self,
        principal: &str,
        id: &str,
        position: usize,
    ) -> Result<(), StoreError> {
        let mut tx = self.begin_write().await?;
        let mut pins: Vec<String> = sqlx::query_scalar(
            "SELECT playlist_id FROM playlist_pins WHERE principal = ?1 ORDER BY position",
        )
        .bind(principal)
        .fetch_all(&mut *tx)
        .await?;
        pins.retain(|p| p != id);
        pins.insert(position.min(pins.len()), id.to_owned());
        write_pins(&mut tx, principal, &pins).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Take playlist `id` out of `principal`'s Pinned: whether it was there.
    pub async fn unpin_playlist(&self, principal: &str, id: &str) -> Result<bool, StoreError> {
        let mut tx = self.begin_write().await?;
        let mut pins: Vec<String> = sqlx::query_scalar(
            "SELECT playlist_id FROM playlist_pins WHERE principal = ?1 ORDER BY position",
        )
        .bind(principal)
        .fetch_all(&mut *tx)
        .await?;
        let before = pins.len();
        pins.retain(|p| p != id);
        if pins.len() == before {
            return Ok(false);
        }
        write_pins(&mut tx, principal, &pins).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// The pinned playlists that left `principal`'s Pinned, oldest first.
    pub async fn pin_notes(&self, principal: &str) -> Result<Vec<PinNoteRow>, StoreError> {
        let rows: Vec<(String, String, String, i64)> = sqlx::query_as(
            "SELECT playlist_id, name, reason, noted_at FROM playlist_pin_notes \
             WHERE principal = ?1 ORDER BY noted_at, playlist_id",
        )
        .bind(principal)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(playlist_id, name, reason, noted_at)| PinNoteRow {
                playlist_id,
                name,
                reason: if reason == "deleted" {
                    PinNoteReason::Deleted
                } else {
                    PinNoteReason::Unshared
                },
                noted_at,
            })
            .collect())
    }

    /// Forget `principal`'s pin notes: a client of theirs showed them.
    /// Whether there were any.
    pub async fn dismiss_pin_notes(&self, principal: &str) -> Result<bool, StoreError> {
        Ok(
            sqlx::query("DELETE FROM playlist_pin_notes WHERE principal = ?1")
                .bind(principal)
                .execute(&self.pool)
                .await?
                .rows_affected()
                > 0,
        )
    }

    /// `principal`'s place in playlist `id`; none before they first play
    /// it.
    pub async fn playlist_place(
        &self,
        principal: &str,
        id: &str,
    ) -> Result<Option<PlaceRow>, StoreError> {
        let row: Option<PlaceTuple> = sqlx::query_as(
            "SELECT entry_id, shuffle, repeat, skip_watched, round FROM playlist_places \
             WHERE principal = ?1 AND playlist_id = ?2",
        )
        .bind(principal)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        let Some((entry, shuffle, repeat, skip_watched, round)) = row else {
            return Ok(None);
        };
        let round = round
            .map(|text| serde_json::from_str::<Vec<EntryId>>(&text).map_err(decode_error))
            .transpose()?;
        Ok(Some(PlaceRow {
            entry: entry.map(|e| e.max(0) as EntryId),
            settings: PlaySettings {
                shuffle,
                repeat: repeat_of(&repeat),
                skip_watched,
            },
            round,
        }))
    }

    /// Keep `place` as `principal`'s in playlist `id`.
    pub async fn set_playlist_place(
        &self,
        principal: &str,
        id: &str,
        place: &PlaceRow,
    ) -> Result<(), StoreError> {
        let round = place
            .round
            .as_ref()
            .map(|round| serde_json::to_string(round).map_err(decode_error))
            .transpose()?;
        sqlx::query(
            "INSERT INTO playlist_places \
               (principal, playlist_id, entry_id, shuffle, repeat, skip_watched, round, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
             ON CONFLICT (principal, playlist_id) DO UPDATE SET \
               entry_id = excluded.entry_id, shuffle = excluded.shuffle, \
               repeat = excluded.repeat, skip_watched = excluded.skip_watched, \
               round = excluded.round, updated_at = excluded.updated_at",
        )
        .bind(principal)
        .bind(id)
        .bind(place.entry.map(db_id))
        .bind(place.settings.shuffle)
        .bind(repeat_text(place.settings.repeat))
        .bind(place.settings.skip_watched)
        .bind(round)
        .bind(self.now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Who has pinned playlist `id`.
    pub async fn pinned_by(&self, id: &str) -> Result<HashSet<String>, StoreError> {
        let who: Vec<String> =
            sqlx::query_scalar("SELECT principal FROM playlist_pins WHERE playlist_id = ?1")
                .bind(id)
                .fetch_all(&self.pool)
                .await?;
        Ok(who.into_iter().collect())
    }
}

/// Raise playlist `id`'s revision: the new one. Not found, if it is gone.
async fn bump(tx: &mut sqlx::SqliteConnection, id: &str, now: i64) -> Result<u64, PlaylistError> {
    let revision: Option<i64> = sqlx::query_scalar(
        "UPDATE playlists SET revision = revision + 1, updated_at = ?2 WHERE id = ?1 \
         RETURNING revision",
    )
    .bind(id)
    .bind(now)
    .fetch_optional(&mut *tx)
    .await?;
    let revision = revision.ok_or(PlaylistError::NotFound)?;
    // The oldest revisions' records go: an editor that far behind reloads.
    let keep_from = revision - RECORDED_REVISIONS;
    if keep_from > 0 {
        sqlx::query("DELETE FROM playlist_edits WHERE playlist_id = ?1 AND revision <= ?2")
            .bind(id)
            .bind(keep_from)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE playlists SET recorded_from = ?2 WHERE id = ?1")
            .bind(id)
            .bind(keep_from)
            .execute(&mut *tx)
            .await?;
    }
    Ok(revision.max(0) as u64)
}

/// Record that `revision` removed or moved `entries`.
async fn record(
    tx: &mut sqlx::SqliteConnection,
    id: &str,
    revision: u64,
    entries: &[EntryId],
) -> Result<(), sqlx::Error> {
    for entry in entries {
        sqlx::query(
            "INSERT OR IGNORE INTO playlist_edits (playlist_id, revision, entry_id) \
             VALUES (?1, ?2, ?3)",
        )
        .bind(id)
        .bind(i64::try_from(revision).unwrap_or(i64::MAX))
        .bind(db_id(*entry))
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

async fn entry_order(
    tx: &mut sqlx::SqliteConnection,
    id: &str,
) -> Result<Vec<EntryId>, sqlx::Error> {
    let order: Vec<i64> = sqlx::query_scalar(
        "SELECT entry_id FROM playlist_entries WHERE playlist_id = ?1 \
         ORDER BY position, entry_id",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    Ok(order.into_iter().map(|e| e.max(0) as EntryId).collect())
}

/// Number the entries 0, 1, 2… in `order`.
async fn write_order(
    tx: &mut sqlx::SqliteConnection,
    order: &[EntryId],
) -> Result<(), sqlx::Error> {
    for (position, entry) in order.iter().enumerate() {
        sqlx::query(
            "UPDATE playlist_entries SET position = ?2 WHERE entry_id = ?1 AND position != ?2",
        )
        .bind(db_id(*entry))
        .bind(position as i64)
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

/// A new entry, with no place yet: its id.
async fn insert_entry(
    tx: &mut sqlx::SqliteConnection,
    id: &str,
    entry: &NewEntry,
    now: i64,
) -> Result<EntryId, sqlx::Error> {
    let entry_id: i64 = sqlx::query_scalar(
        "INSERT INTO playlist_entries \
           (playlist_id, position, media_id, kind, show_id, album_id, artist_id, added_at) \
         VALUES (?1, -1, ?2, ?3, ?4, ?5, ?6, ?7) RETURNING entry_id",
    )
    .bind(id)
    .bind(&entry.media_id)
    .bind(&entry.kind)
    .bind(&entry.show_id)
    .bind(&entry.album_id)
    .bind(&entry.artist_id)
    .bind(now)
    .fetch_one(&mut *tx)
    .await?;
    for external in &entry.external_ids {
        sqlx::query(
            "INSERT OR IGNORE INTO playlist_entry_ids (entry_id, external_id) VALUES (?1, ?2)",
        )
        .bind(entry_id)
        .bind(external)
        .execute(&mut *tx)
        .await?;
    }
    Ok(entry_id.max(0) as EntryId)
}

async fn write_shares(
    tx: &mut sqlx::SqliteConnection,
    id: &str,
    shares: &[ShareRow],
) -> Result<(), sqlx::Error> {
    for share in shares {
        sqlx::query(
            "INSERT INTO playlist_shares (playlist_id, principal, can_edit) VALUES (?1, ?2, ?3) \
             ON CONFLICT (playlist_id, principal) DO UPDATE SET can_edit = excluded.can_edit",
        )
        .bind(id)
        .bind(&share.principal)
        .bind(share.can_edit)
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

async fn write_pins(
    tx: &mut sqlx::SqliteConnection,
    principal: &str,
    pins: &[String],
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM playlist_pins WHERE principal = ?1")
        .bind(principal)
        .execute(&mut *tx)
        .await?;
    for (position, id) in pins.iter().enumerate() {
        sqlx::query(
            "INSERT INTO playlist_pins (principal, playlist_id, position) VALUES (?1, ?2, ?3)",
        )
        .bind(principal)
        .bind(id)
        .bind(position as i64)
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

/// Note, for each pin of playlist `id` that `which` (a condition on
/// `playlist_pins`, `?1` the playlist) picks, but `by`'s, that it left
/// Pinned for `reason`. A newer note for the same playlist replaces an
/// older one.
async fn note_pins(
    tx: &mut sqlx::SqliteConnection,
    id: &str,
    which: &str,
    by: Option<&str>,
    reason: PinNoteReason,
    now: i64,
) -> Result<(), sqlx::Error> {
    // `which` is this module's constant, never input.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO playlist_pin_notes (principal, playlist_id, name, reason, noted_at) \
         SELECT playlist_pins.principal, playlist_pins.playlist_id, p.name, ?3, ?4 \
         FROM playlist_pins JOIN playlists p ON p.id = playlist_pins.playlist_id \
         WHERE {which} AND playlist_pins.principal IS NOT ?2 \
         ON CONFLICT (principal, playlist_id) DO UPDATE SET \
           name = excluded.name, reason = excluded.reason, noted_at = excluded.noted_at"
    )))
    .bind(id)
    .bind(by)
    .bind(reason.as_str())
    .bind(now)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests;
