//! Play rows (PX-52). What counts and how time played is measured live in
//! `crate::watch::plays`; this is storage.

use std::collections::HashMap;

use spoolframe_types::MediaKind;
use sqlx::{QueryBuilder, Sqlite};

use super::{Store, StoreError, push_in_list};
use crate::principal::Principal;

/// What a play is of: its row's `kind`. Films, episodes and tracks are
/// played; nothing else is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayedKind {
    Movie,
    Episode,
    Track,
}

impl PlayedKind {
    /// The kind a play of `kind` is kept as, if it has plays.
    pub fn of(kind: MediaKind) -> Option<Self> {
        match kind {
            MediaKind::Movie => Some(Self::Movie),
            MediaKind::Episode => Some(Self::Episode),
            MediaKind::Track => Some(Self::Track),
            _ => None,
        }
    }

    /// As the `kind` column holds it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Movie => "movie",
            Self::Episode => "episode",
            Self::Track => "track",
        }
    }
}

/// One play, as stored.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct PlayRow {
    pub id: i64,
    pub media_id: String,
    pub kind: String,
    pub show_id: Option<String>,
    pub album_id: Option<String>,
    pub artist_id: Option<String>,
    pub started_at: i64,
    pub ended_at: i64,
    pub played_ms: i64,
    pub duration_ms: Option<i64>,
    pub counted: bool,
    pub closed: bool,
    pub endpoint_id: Option<String>,
    pub device_name: Option<String>,
    pub path: String,
    pub playback_id: Option<String>,
}

/// A play to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPlay<'a> {
    pub media_id: &'a str,
    pub kind: PlayedKind,
    pub show_id: Option<&'a str>,
    pub album_id: Option<&'a str>,
    pub artist_id: Option<&'a str>,
    pub external_ids: &'a [String],
    pub duration_ms: Option<i64>,
    pub endpoint_id: Option<&'a str>,
    pub device_name: Option<&'a str>,
    pub playback_id: Option<&'a str>,
}

/// An open play as far as it has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayUpdate {
    pub played_ms: i64,
    pub counted: bool,
    pub over_http: bool,
    /// Close it: nothing changes it after this.
    pub close: bool,
}

/// An item's counted plays, for one principal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaySummary {
    pub count: u32,
    /// When the latest ended, Unix seconds.
    pub last: i64,
}

/// Note `play`'s external ids as its item's, for `principal`. An id names
/// one item: if it was another key's, it is this one's now, but a paired
/// server's item never takes one from this server's own (M8-04), as with
/// `watch_state_ids`. One already this item's is left unwritten.
async fn note_play_ids(
    tx: &mut sqlx::SqliteConnection,
    principal: &str,
    play: &NewPlay<'_>,
) -> Result<(), StoreError> {
    for external in play.external_ids {
        sqlx::query(
            "INSERT INTO play_ids (principal, media_id, external_id) VALUES (?1, ?2, ?3) \
             ON CONFLICT (principal, external_id) DO UPDATE SET media_id = excluded.media_id \
             WHERE play_ids.media_id != excluded.media_id \
               AND (excluded.media_id NOT LIKE 'fed:%' OR play_ids.media_id LIKE 'fed:%')",
        )
        .bind(principal)
        .bind(play.media_id)
        .bind(external)
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

impl Store {
    /// Open a play for `principal`, now; its id.
    pub async fn open_play(
        &self,
        principal: Principal,
        play: NewPlay<'_>,
    ) -> Result<i64, StoreError> {
        let principal = principal.to_string();
        let now = self.now();
        let mut tx = self.begin_write().await?;
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO plays \
               (principal, media_id, kind, show_id, album_id, artist_id, started_at, ended_at, \
                duration_ms, endpoint_id, device_name, playback_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8, ?9, ?10, ?11) RETURNING id",
        )
        .bind(&principal)
        .bind(play.media_id)
        .bind(play.kind.as_str())
        .bind(play.show_id)
        .bind(play.album_id)
        .bind(play.artist_id)
        .bind(now)
        .bind(play.duration_ms)
        .bind(play.endpoint_id)
        .bind(play.device_name)
        .bind(play.playback_id)
        .fetch_one(&mut *tx)
        .await?;
        note_play_ids(&mut tx, &principal, &play).await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Move open play `id` on to `update`, as of now. A closed play is
    /// left as it is.
    pub async fn update_play(&self, id: i64, update: PlayUpdate) -> Result<(), StoreError> {
        // One statement: it takes the writer when it starts.
        sqlx::query(
            "UPDATE plays SET ended_at = ?2, played_ms = ?3, counted = ?4, \
               path = CASE WHEN ?5 THEN 'http' ELSE path END, closed = ?6 \
             WHERE id = ?1 AND closed = 0",
        )
        .bind(id)
        .bind(self.now())
        .bind(update.played_ms)
        .bind(update.counted)
        .bind(update.over_http)
        .bind(update.close)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record a play made on a device with no server (PX-51), closed as
    /// it is, from `started_at` to `ended_at` (Unix seconds): its path is
    /// `offline`. Recorded once: a play of the same item from the same
    /// device with the same `play.playback_id` (the device's own time of
    /// it) is there already, and this is `false`.
    pub async fn record_offline_play(
        &self,
        principal: Principal,
        play: NewPlay<'_>,
        (started_at, ended_at): (i64, i64),
        played_ms: i64,
        counted: bool,
    ) -> Result<bool, StoreError> {
        let principal = principal.to_string();
        let mut tx = self.begin_write().await?;
        let there: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM plays WHERE principal = ?1 AND media_id = ?2 AND playback_id = ?3 \
               AND endpoint_id IS ?4",
        )
        .bind(&principal)
        .bind(play.media_id)
        .bind(play.playback_id)
        .bind(play.endpoint_id)
        .fetch_optional(&mut *tx)
        .await?;
        if there.is_some() {
            return Ok(false);
        }
        sqlx::query(
            "INSERT INTO plays \
               (principal, media_id, kind, show_id, album_id, artist_id, started_at, ended_at, \
                played_ms, duration_ms, counted, closed, endpoint_id, device_name, path, \
                playback_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 1, ?12, ?13, 'offline', ?14)",
        )
        .bind(&principal)
        .bind(play.media_id)
        .bind(play.kind.as_str())
        .bind(play.show_id)
        .bind(play.album_id)
        .bind(play.artist_id)
        .bind(started_at)
        .bind(ended_at)
        .bind(played_ms)
        .bind(play.duration_ms)
        .bind(counted)
        .bind(play.endpoint_id)
        .bind(play.device_name)
        .bind(play.playback_id)
        .execute(&mut *tx)
        .await?;
        note_play_ids(&mut tx, &principal, &play).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Close every play still open: at start, those a server that stopped
    /// left open, as far as they had got. How many.
    pub async fn close_open_plays(&self) -> Result<u64, StoreError> {
        Ok(sqlx::query("UPDATE plays SET closed = 1 WHERE closed = 0")
            .execute(&self.pool)
            .await?
            .rows_affected())
    }

    /// Every play of `principal`'s, the first first.
    pub async fn all_plays(&self, principal: Principal) -> Result<Vec<PlayRow>, StoreError> {
        Ok(sqlx::query_as(
            "SELECT id, media_id, kind, show_id, album_id, artist_id, started_at, ended_at, \
               played_ms, duration_ms, counted, closed, endpoint_id, device_name, path, \
               playback_id \
             FROM plays WHERE principal = ?1 ORDER BY id",
        )
        .bind(principal.to_string())
        .fetch_all(&self.pool)
        .await?)
    }

    /// The counted plays of `media_ids`, in one query; an item with none
    /// is not in the map.
    pub async fn play_summaries(
        &self,
        principal: Principal,
        media_ids: &[String],
    ) -> Result<HashMap<String, PlaySummary>, StoreError> {
        if media_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let mut query: QueryBuilder<Sqlite> = QueryBuilder::new(
            "SELECT media_id, count(*), max(ended_at) FROM plays \
             WHERE counted = 1 AND principal = ",
        );
        query
            .push_bind(principal.to_string())
            .push(" AND media_id IN ");
        push_in_list(&mut query, media_ids);
        query.push(" GROUP BY media_id");
        let rows: Vec<(String, i64, i64)> = query.build_query_as().fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(|(id, count, last)| {
                let count = u32::try_from(count).unwrap_or(u32::MAX);
                (id, PlaySummary { count, last })
            })
            .collect())
    }

    /// `principal`'s played items of `kinds` (all kinds when empty), each
    /// once, by its latest counted play, newest first: a page of `limit`
    /// from `offset`, and how many there are.
    pub async fn recently_played(
        &self,
        principal: Principal,
        kinds: &[PlayedKind],
        offset: u32,
        limit: u32,
        federated: bool,
    ) -> Result<(Vec<String>, u32), StoreError> {
        let principal = principal.to_string();
        let filter = |query: &mut QueryBuilder<Sqlite>| {
            query
                .push(" FROM plays WHERE counted = 1 AND principal = ")
                .push_bind(principal.clone());
            if !kinds.is_empty() {
                query.push(" AND kind IN ");
                push_in_list(query, kinds.iter().map(|k| k.as_str()));
            }
            // A paired server's items only for a client that names the
            // experimental `federation` feature (P8-02).
            if !federated {
                query.push(concat!(" AND ", own_item!("media_id")));
            }
        };
        let mut page: QueryBuilder<Sqlite> = QueryBuilder::new("SELECT media_id");
        filter(&mut page);
        page.push(" GROUP BY media_id ORDER BY max(ended_at) DESC, max(id) DESC LIMIT ")
            .push_bind(limit)
            .push(" OFFSET ")
            .push_bind(offset);
        let mut count: QueryBuilder<Sqlite> = QueryBuilder::new("SELECT count(DISTINCT media_id)");
        filter(&mut count);
        // Two reads of the pool, side by side.
        let (ids, total): (Vec<String>, i64) = tokio::try_join!(
            page.build_query_scalar().fetch_all(&self.pool),
            count.build_query_scalar().fetch_one(&self.pool),
        )?;
        Ok((ids, u32::try_from(total).unwrap_or(u32::MAX)))
    }

    /// The key that an item with one of `external_ids` has plays under,
    /// other than `media_id`. That key is the item's earlier one only if
    /// the item has left it, which the caller checks before
    /// [`Store::move_plays`]: a copy of the item keeps its own.
    pub async fn plays_under_another_key(
        &self,
        principal: Principal,
        media_id: &str,
        external_ids: &[String],
    ) -> Result<Option<String>, StoreError> {
        if external_ids.is_empty() {
            return Ok(None);
        }
        let mut query: QueryBuilder<Sqlite> =
            QueryBuilder::new("SELECT media_id FROM play_ids WHERE principal = ");
        query
            .push_bind(principal.to_string())
            .push(" AND media_id != ")
            .push_bind(media_id)
            .push(" AND external_id IN ");
        push_in_list(&mut query, external_ids);
        query.push(" LIMIT 1");
        Ok(query
            .build_query_scalar()
            .fetch_optional(&self.pool)
            .await?)
    }

    /// Move `principal`'s plays under `from` to `to` (the item got a new
    /// key), with their ids. Only the key changes: what each play was
    /// stays as it was closed.
    pub async fn move_plays(
        &self,
        principal: Principal,
        from: &str,
        to: &str,
    ) -> Result<(), StoreError> {
        let principal = principal.to_string();
        let mut tx = self.begin_write().await?;
        let moved =
            sqlx::query("UPDATE plays SET media_id = ?3 WHERE principal = ?1 AND media_id = ?2")
                .bind(&principal)
                .bind(from)
                .bind(to)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        sqlx::query("UPDATE play_ids SET media_id = ?3 WHERE principal = ?1 AND media_id = ?2")
            .bind(&principal)
            .bind(from)
            .bind(to)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        if moved > 0 {
            tracing::info!(%principal, %from, %to, plays = moved, "plays re-linked");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::test_store;
    use super::*;
    use crate::store::users::LocalUserId;

    const ALICE: Principal = Principal::LocalUser(LocalUserId::from_uuid(uuid::Uuid::from_u128(1)));
    const BOB: Principal = Principal::LocalUser(LocalUserId::from_uuid(uuid::Uuid::from_u128(2)));

    fn play<'a>(media_id: &'a str, kind: PlayedKind, ids: &'a [String]) -> NewPlay<'a> {
        NewPlay {
            media_id,
            kind,
            show_id: None,
            album_id: None,
            artist_id: None,
            external_ids: ids,
            duration_ms: Some(300_000),
            endpoint_id: Some("e"),
            device_name: None,
            playback_id: Some("p"),
        }
    }

    const COUNTED: PlayUpdate = PlayUpdate {
        played_ms: 200_000,
        counted: true,
        over_http: false,
        close: true,
    };

    #[tokio::test]
    async fn a_closed_play_is_never_rewritten() {
        let store = test_store("plays-closed").await;
        let id = store
            .open_play(ALICE, play("plex:track:1", PlayedKind::Track, &[]))
            .await
            .unwrap();
        store.update_play(id, COUNTED).await.unwrap();
        let later = PlayUpdate {
            played_ms: 1,
            counted: false,
            over_http: true,
            close: false,
        };
        store.update_play(id, later).await.unwrap();
        let rows = store.all_plays(ALICE).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].counted && rows[0].closed);
        assert_eq!(
            (rows[0].played_ms, rows[0].path.as_str()),
            (200_000, "iroh")
        );
    }

    #[tokio::test]
    async fn recently_played_is_counted_newest_first_by_kind_and_one_s_own() {
        let store = test_store("plays-recent").await;
        for (media_id, kind) in [
            ("plex:track:1", PlayedKind::Track),
            ("plex:movie:2", PlayedKind::Movie),
            ("plex:track:1", PlayedKind::Track),
            ("plex:track:3", PlayedKind::Track),
        ] {
            let id = store
                .open_play(ALICE, play(media_id, kind, &[]))
                .await
                .unwrap();
            store.update_play(id, COUNTED).await.unwrap();
        }
        // Started and stopped: history, not a play.
        store
            .open_play(ALICE, play("plex:track:4", PlayedKind::Track, &[]))
            .await
            .unwrap();
        let id = store
            .open_play(BOB, play("plex:track:5", PlayedKind::Track, &[]))
            .await
            .unwrap();
        store.update_play(id, COUNTED).await.unwrap();

        let (all, total) = store
            .recently_played(ALICE, &[], 0, 10, false)
            .await
            .unwrap();
        assert_eq!(all, ["plex:track:3", "plex:track:1", "plex:movie:2"]);
        assert_eq!(total, 3);
        let (tracks, total) = store
            .recently_played(ALICE, &[PlayedKind::Track], 1, 10, false)
            .await
            .unwrap();
        assert_eq!(
            (tracks.as_slice(), total),
            (&["plex:track:1".to_owned()][..], 2)
        );
        let summaries = store
            .play_summaries(ALICE, &["plex:track:1".into(), "plex:track:4".into()])
            .await
            .unwrap();
        assert_eq!(summaries["plex:track:1"].count, 2);
        assert!(!summaries.contains_key("plex:track:4"));
    }

    #[tokio::test]
    async fn plays_are_found_and_moved_through_their_external_ids() {
        let store = test_store("plays-relink").await;
        let ids = vec!["mbid://1".to_owned()];
        for _ in 0..2 {
            let id = store
                .open_play(ALICE, play("plex:track:1", PlayedKind::Track, &ids))
                .await
                .unwrap();
            store.update_play(id, COUNTED).await.unwrap();
        }
        let old = store
            .plays_under_another_key(ALICE, "plex:track:99", &ids)
            .await
            .unwrap();
        assert_eq!(old.as_deref(), Some("plex:track:1"));
        assert_eq!(
            store
                .plays_under_another_key(BOB, "plex:track:99", &ids)
                .await
                .unwrap(),
            None
        );
        // M8-04: a paired server's track played under the same id does
        // not take it from the local one.
        store
            .open_play(
                ALICE,
                play(
                    "fed:track:aaaaaaaaaaaaaaaa:plex:track:1",
                    PlayedKind::Track,
                    &ids,
                ),
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .plays_under_another_key(ALICE, "plex:track:99", &ids)
                .await
                .unwrap()
                .as_deref(),
            Some("plex:track:1")
        );
        store
            .move_plays(ALICE, "plex:track:1", "plex:track:99")
            .await
            .unwrap();
        let summaries = store
            .play_summaries(ALICE, &["plex:track:99".into()])
            .await
            .unwrap();
        assert_eq!(summaries["plex:track:99"].count, 2);
        assert_eq!(
            store
                .plays_under_another_key(ALICE, "plex:track:99", &ids)
                .await
                .unwrap(),
            None
        );
    }
}
