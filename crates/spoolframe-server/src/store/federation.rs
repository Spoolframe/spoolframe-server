//! Servers paired with this one (P8-01, ARCHITECTURE §26–§28): one row
//! per pair, on both sides, pinning the peer's endpoint key and server id
//! together.
//!
//! Redeeming a `federation` invite is the pairing: the use is taken and
//! the row made in one transaction, as a `create-user` invite makes its
//! user. Unpairing revokes the row and keeps it; a pair made again is a
//! new row. A known server id from a new key is held for the pair, with
//! at most [`MAX_HELD_KEYS`] others, until an admin confirms that key
//! (which moves the pin) or rejects it.

use sqlx::Row;
use uuid::Uuid;

use super::invites::{InviteKind, InviteState, take_use};
use super::{Store, StoreError};

/// A paired server, as this one keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerRecord {
    pub id: String,
    /// Its `ServerId`, as written.
    pub server_id: String,
    /// Its `EndpointId`, hex: the key its connections prove.
    pub endpoint_id: String,
    pub display_name: String,
    /// Where it may be dialled: `ip:port` hints.
    pub addrs: Vec<String>,
    /// Relay URLs it may be reached through: hints.
    pub relays: Vec<String>,
    /// Who paired it here: the invite's maker, or the admin who redeemed
    /// the other's invite.
    pub paired_by: String,
    /// The invite of this server's that made the pair; `None` where this
    /// server redeemed the other's.
    pub invite_id: Option<String>,
    /// Unix seconds.
    pub paired_at: i64,
    pub revoked_at: Option<i64>,
}

impl PeerRecord {
    /// Not unpaired.
    pub fn live(&self) -> bool {
        self.revoked_at.is_none()
    }

    /// Whether `key` is the key this pair is pinned to (C8-01): what a
    /// connection to or from it is kept on.
    pub fn pinned_to(&self, key: &iroh::EndpointId) -> bool {
        self.endpoint_id == key.to_string()
    }
}

/// A new key that said it is a paired server (P8-01's key rotation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldKey {
    /// Its `EndpointId`, hex.
    pub endpoint_id: String,
    /// When it first and last said so, Unix seconds. The last is written
    /// at most once per [`SEEN_EVERY`].
    pub first_seen: i64,
    pub last_seen: i64,
    /// When an admin rejected it: refused, not held, until
    /// [`REJECTED_FOR`] after.
    pub rejected_at: Option<i64>,
}

/// The most new keys kept for one pair, held and rejected together. A
/// key past them is dropped, never swapped in: what the admin is looking
/// at cannot change under them.
pub const MAX_HELD_KEYS: i64 = 4;

/// A held key not seen for this long is forgotten, seconds.
pub const HELD_FOR: i64 = 7 * 86_400;

/// A rejected key is refused for this long, seconds; then it is
/// forgotten, and may be held again.
pub const REJECTED_FOR: i64 = 86_400;

/// A held key's `last_seen` is written at most this often, seconds: a key
/// that reconnects in a loop does not write each time.
pub const SEEN_EVERY: i64 = 3_600;

/// What saying hello did for a new key of a live pair's server id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyHold {
    /// Held now, for the first time.
    New,
    /// Held already: seen again.
    Held,
    /// Rejected by an admin within [`REJECTED_FOR`]: refused.
    Rejected,
    /// [`MAX_HELD_KEYS`] are kept for the pair already: dropped.
    Full,
    /// The pair is not live.
    NoPeer,
}

/// A pair to keep: everything but its id, who made it and when.
#[derive(Debug, Clone, Copy)]
pub struct NewPeer<'a> {
    pub server_id: &'a str,
    pub endpoint_id: &'a str,
    pub display_name: &'a str,
    pub addrs: &'a [String],
    pub relays: &'a [String],
}

#[derive(Debug, thiserror::Error)]
pub enum PeerError {
    /// The endpoint, or the server id, has a live pair already.
    #[error("that server is paired already")]
    AlreadyPaired,
    #[error("no paired server {0:?}")]
    NoSuchPeer(String),
    /// Confirming or rejecting a key that is not held for the pair: the
    /// held keys changed since the admin looked.
    #[error("that key is not held for that server: the held keys changed, look again")]
    KeyNotHeld,
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<sqlx::Error> for PeerError {
    fn from(e: sqlx::Error) -> Self {
        Self::Store(e.into())
    }
}

/// What redeeming a `federation` invite did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairRedeemed {
    /// The pair was made, and a use taken.
    Paired(Box<PeerRecord>),
    /// Nothing was made: the invite is not usable (or of another kind),
    /// as this says.
    Refused(InviteState),
    /// Nothing was made, and the invite keeps its use: the endpoint or
    /// the server id has a live pair.
    AlreadyPaired,
}

/// A pair row's columns, as [`record`] reads them: a macro, so each query
/// is one `concat!`ed literal.
macro_rules! columns {
    () => {
        "id, server_id, endpoint_id, display_name, addrs, relays, paired_by, invite_id, \
         paired_at, revoked_at"
    };
}

fn hints(row: &sqlx::sqlite::SqliteRow, column: &str) -> Result<Vec<String>, sqlx::Error> {
    let text: String = row.try_get(column)?;
    serde_json::from_str(&text).map_err(|e| sqlx::Error::ColumnDecode {
        index: column.into(),
        source: e.into(),
    })
}

fn record(row: &sqlx::sqlite::SqliteRow) -> Result<PeerRecord, sqlx::Error> {
    Ok(PeerRecord {
        id: row.try_get("id")?,
        server_id: row.try_get("server_id")?,
        endpoint_id: row.try_get("endpoint_id")?,
        display_name: row.try_get("display_name")?,
        addrs: hints(row, "addrs")?,
        relays: hints(row, "relays")?,
        paired_by: row.try_get("paired_by")?,
        invite_id: row.try_get("invite_id")?,
        paired_at: row.try_get("paired_at")?,
        revoked_at: row.try_get("revoked_at")?,
    })
}

/// Keep `peer`, paired by `paired_by` with `invite_id`, on `conn`, unless
/// its endpoint or server id has a live pair: then `None`, and nothing is
/// written.
async fn insert_peer(
    conn: &mut sqlx::SqliteConnection,
    peer: NewPeer<'_>,
    paired_by: &str,
    invite_id: Option<&str>,
    now: i64,
) -> Result<Option<PeerRecord>, sqlx::Error> {
    let taken: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM federation_peers \
                        WHERE revoked_at IS NULL AND (endpoint_id = ?1 OR server_id = ?2))",
    )
    .bind(peer.endpoint_id)
    .bind(peer.server_id)
    .fetch_one(&mut *conn)
    .await?;
    if taken {
        return Ok(None);
    }
    let id = Uuid::now_v7().hyphenated().to_string();
    let row = sqlx::query(concat!(
        "INSERT INTO federation_peers (id, server_id, endpoint_id, display_name, addrs, relays, \
                                       paired_by, invite_id, paired_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) RETURNING ",
        columns!()
    ))
    .bind(&id)
    .bind(peer.server_id)
    .bind(peer.endpoint_id)
    .bind(peer.display_name)
    .bind(serde_json::to_string(peer.addrs).expect("strings serialize"))
    .bind(serde_json::to_string(peer.relays).expect("strings serialize"))
    .bind(paired_by)
    .bind(invite_id)
    .bind(now)
    .fetch_one(&mut *conn)
    .await?;
    Ok(Some(record(&row)?))
}

/// Whether pair `id` is live, on `conn`.
async fn peer_is_live(conn: &mut sqlx::SqliteConnection, id: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM federation_peers WHERE id = ?1 AND revoked_at IS NULL)",
    )
    .bind(id)
    .fetch_one(conn)
    .await
}

/// Pair `id`, live or not, on `conn`.
async fn fetch_peer(
    conn: &mut sqlx::SqliteConnection,
    id: &str,
) -> Result<Option<PeerRecord>, sqlx::Error> {
    let row = sqlx::query(concat!(
        "SELECT ",
        columns!(),
        " FROM federation_peers WHERE id = ?1"
    ))
    .bind(id)
    .fetch_optional(conn)
    .await?;
    row.as_ref().map(record).transpose()
}

/// What saying hello again does for `endpoint_id`, a new key of pair
/// `id`, as read on `conn` at `now`, when that is all it does: nothing to
/// write. `None` when holding it writes: a key to add or a `last_seen`
/// to move, or keys out of their time to forget.
async fn hold_without_writing(
    conn: &mut sqlx::SqliteConnection,
    id: &str,
    endpoint_id: &str,
    now: i64,
) -> Result<Option<KeyHold>, sqlx::Error> {
    if !peer_is_live(&mut *conn, id).await? {
        return Ok(Some(KeyHold::NoPeer));
    }
    let expired: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM federation_peer_held_keys \
                        WHERE peer = ?1 \
                          AND (rejected_at <= ?2 OR (rejected_at IS NULL AND last_seen <= ?3)))",
    )
    .bind(id)
    .bind(now - REJECTED_FOR)
    .bind(now - HELD_FOR)
    .fetch_one(&mut *conn)
    .await?;
    if expired {
        return Ok(None);
    }
    let held: Option<(i64, Option<i64>)> = sqlx::query_as(
        "SELECT last_seen, rejected_at FROM federation_peer_held_keys \
         WHERE peer = ?1 AND endpoint_id = ?2",
    )
    .bind(id)
    .bind(endpoint_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(match held {
        Some((_, Some(_))) => Some(KeyHold::Rejected),
        Some((last_seen, None)) if now - last_seen < SEEN_EVERY => Some(KeyHold::Held),
        Some(_) => None,
        None => {
            let kept: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM federation_peer_held_keys WHERE peer = ?1",
            )
            .bind(id)
            .fetch_one(&mut *conn)
            .await?;
            (kept >= MAX_HELD_KEYS).then_some(KeyHold::Full)
        }
    })
}

impl Store {
    /// Every pair, live or not: the live first, then the unpaired, each
    /// newest first.
    pub async fn federation_peers(&self) -> Result<Vec<PeerRecord>, StoreError> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM federation_peers ORDER BY revoked_at IS NOT NULL, paired_at DESC, id DESC"
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(record).collect::<Result<_, _>>()?)
    }

    /// Pair `id`, live or not.
    pub async fn federation_peer(&self, id: &str) -> Result<Option<PeerRecord>, StoreError> {
        let mut conn = self.pool.acquire().await?;
        Ok(fetch_peer(&mut conn, id).await?)
    }

    /// The live pair whose key is `endpoint_id` (hex).
    pub async fn live_peer_by_endpoint(
        &self,
        endpoint_id: &str,
    ) -> Result<Option<PeerRecord>, StoreError> {
        self.one_peer(
            concat!(
                "SELECT ",
                columns!(),
                " FROM federation_peers WHERE endpoint_id = ?1 AND revoked_at IS NULL"
            ),
            endpoint_id,
        )
        .await
    }

    /// The live pair whose server id is `server_id`.
    pub async fn live_peer_by_server_id(
        &self,
        server_id: &str,
    ) -> Result<Option<PeerRecord>, StoreError> {
        self.one_peer(
            concat!(
                "SELECT ",
                columns!(),
                " FROM federation_peers WHERE server_id = ?1 AND revoked_at IS NULL"
            ),
            server_id,
        )
        .await
    }

    /// The row `query` finds by `value`, its one parameter.
    async fn one_peer(
        &self,
        query: &'static str,
        value: &str,
    ) -> Result<Option<PeerRecord>, StoreError> {
        let row = sqlx::query(query)
            .bind(value)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(record).transpose()?)
    }

    /// Pair `peer` with the `federation` invite whose code hashes to
    /// `capability_hash`, and take one of its uses, in one transaction:
    /// the pair is paired by whoever made the invite. A peer whose
    /// endpoint or server id has a live pair already makes nothing, and
    /// the invite keeps its use.
    ///
    /// The use is taken first: a write, so the transaction holds the
    /// database's write lock from its first statement, and a redemption
    /// racing it waits, then sees what it left.
    pub async fn pair_by_invite(
        &self,
        capability_hash: &str,
        peer: NewPeer<'_>,
    ) -> Result<PairRedeemed, StoreError> {
        let now = self.now();
        let mut tx = self.begin_write().await?;
        let taken = match take_use(&mut tx, capability_hash, InviteKind::Federation, now).await? {
            Ok(taken) => taken,
            Err(refused) => {
                tx.rollback().await?;
                return Ok(PairRedeemed::Refused(refused));
            }
        };
        match insert_peer(&mut tx, peer, &taken.created_by, Some(&taken.id), now).await? {
            Some(record) => {
                tx.commit().await?;
                Ok(PairRedeemed::Paired(Box::new(record)))
            }
            None => {
                tx.rollback().await?;
                Ok(PairRedeemed::AlreadyPaired)
            }
        }
    }

    /// Keep `peer`, whose invite this server redeemed, paired by admin
    /// `paired_by`. [`PeerError::AlreadyPaired`] if its endpoint or
    /// server id has a live pair.
    pub async fn add_peer(
        &self,
        peer: NewPeer<'_>,
        paired_by: &str,
    ) -> Result<PeerRecord, PeerError> {
        let mut tx = self.begin_write().await?;
        let Some(record) = insert_peer(&mut tx, peer, paired_by, None, self.now()).await? else {
            tx.rollback().await?;
            return Err(PeerError::AlreadyPaired);
        };
        tx.commit().await?;
        Ok(record)
    }

    /// Unpair `id`: its row is revoked and kept. The row as it was, if it
    /// was live; `None` for one that was not, or no row.
    pub async fn revoke_peer(&self, id: &str) -> Result<Option<PeerRecord>, StoreError> {
        self.revoke_peer_by(
            concat!(
                "UPDATE federation_peers SET revoked_at = ?2 \
                 WHERE id = ?1 AND revoked_at IS NULL RETURNING ",
                columns!()
            ),
            id,
        )
        .await
    }

    /// Unpair the live pair whose key is `endpoint_id`, as [`Self::revoke_peer`]:
    /// for a peer that said it unpaired.
    pub async fn revoke_peer_by_endpoint(
        &self,
        endpoint_id: &str,
    ) -> Result<Option<PeerRecord>, StoreError> {
        self.revoke_peer_by(
            concat!(
                "UPDATE federation_peers SET revoked_at = ?2 \
                 WHERE endpoint_id = ?1 AND revoked_at IS NULL RETURNING ",
                columns!()
            ),
            endpoint_id,
        )
        .await
    }

    /// Revoke the live row `query` finds by `value`, at now (`?2`), with
    /// its grants and its users' tokens (P8-03), and forget the keys held
    /// for it and what it was last given of each library (P8-02).
    async fn revoke_peer_by(
        &self,
        query: &'static str,
        value: &str,
    ) -> Result<Option<PeerRecord>, StoreError> {
        let now = self.now();
        let mut tx = self.begin_write().await?;
        let row = sqlx::query(query)
            .bind(value)
            .bind(now)
            .fetch_optional(&mut *tx)
            .await?;
        let peer = row.as_ref().map(record).transpose()?;
        if let Some(peer) = &peer {
            sqlx::query("DELETE FROM federation_peer_held_keys WHERE peer = ?1")
                .bind(&peer.id)
                .execute(&mut *tx)
                .await?;
            super::federation_grants::revoke_all(&mut tx, &peer.id, now).await?;
            // What it was last given (P8-02): nothing is shared any more.
            sqlx::query("DELETE FROM federation_sync_states WHERE peer = ?1")
                .bind(&peer.id)
                .execute(&mut *tx)
                .await?;
            // Its users' tokens (P8-03): nothing of this server's plays
            // for them any more.
            sqlx::query("DELETE FROM federation_tokens WHERE peer = ?1")
                .bind(&peer.id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(peer)
    }

    /// Hold `endpoint_id` (hex), which said it is live pair `id`'s server,
    /// for an admin. A key held already is only seen again, written at
    /// most once per [`SEEN_EVERY`]; a rejected one is refused; a new one
    /// past [`MAX_HELD_KEYS`] is dropped. Keys out of their time are
    /// forgotten first. Nothing is written unless something changes, and
    /// the write lock is taken only then: a key that keeps saying hello
    /// is answered from a read.
    pub async fn hold_peer_key(&self, id: &str, endpoint_id: &str) -> Result<KeyHold, StoreError> {
        let now = self.now();
        {
            let mut conn = self.pool.acquire().await?;
            if let Some(hold) = hold_without_writing(&mut conn, id, endpoint_id, now).await? {
                return Ok(hold);
            }
        }
        // As read again under the lock: another hello may have written
        // since.
        let mut tx = self.begin_write().await?;
        if !peer_is_live(&mut tx, id).await? {
            tx.rollback().await?;
            return Ok(KeyHold::NoPeer);
        }
        sqlx::query(
            "DELETE FROM federation_peer_held_keys \
             WHERE peer = ?1 AND (rejected_at <= ?2 OR (rejected_at IS NULL AND last_seen <= ?3))",
        )
        .bind(id)
        .bind(now - REJECTED_FOR)
        .bind(now - HELD_FOR)
        .execute(&mut *tx)
        .await?;
        let held: Option<(i64, Option<i64>)> = sqlx::query_as(
            "SELECT last_seen, rejected_at FROM federation_peer_held_keys \
             WHERE peer = ?1 AND endpoint_id = ?2",
        )
        .bind(id)
        .bind(endpoint_id)
        .fetch_optional(&mut *tx)
        .await?;
        let hold = match held {
            Some((_, Some(_))) => KeyHold::Rejected,
            Some((last_seen, None)) => {
                if now - last_seen >= SEEN_EVERY {
                    sqlx::query(
                        "UPDATE federation_peer_held_keys SET last_seen = ?3 \
                         WHERE peer = ?1 AND endpoint_id = ?2",
                    )
                    .bind(id)
                    .bind(endpoint_id)
                    .bind(now)
                    .execute(&mut *tx)
                    .await?;
                }
                KeyHold::Held
            }
            None => {
                let kept: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM federation_peer_held_keys WHERE peer = ?1",
                )
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
                if kept >= MAX_HELD_KEYS {
                    KeyHold::Full
                } else {
                    sqlx::query(
                        "INSERT INTO federation_peer_held_keys \
                             (peer, endpoint_id, first_seen, last_seen) \
                         VALUES (?1, ?2, ?3, ?3)",
                    )
                    .bind(id)
                    .bind(endpoint_id)
                    .bind(now)
                    .execute(&mut *tx)
                    .await?;
                    KeyHold::New
                }
            }
        };
        tx.commit().await?;
        Ok(hold)
    }

    /// The new keys kept for pair `id`, held and rejected, in their time:
    /// the first seen first.
    pub async fn held_peer_keys(&self, id: &str) -> Result<Vec<HeldKey>, StoreError> {
        let now = self.now();
        let rows: Vec<(String, i64, i64, Option<i64>)> = sqlx::query_as(
            "SELECT endpoint_id, first_seen, last_seen, rejected_at \
             FROM federation_peer_held_keys \
             WHERE peer = ?1 AND (rejected_at > ?2 OR (rejected_at IS NULL AND last_seen > ?3)) \
             ORDER BY first_seen, rowid",
        )
        .bind(id)
        .bind(now - REJECTED_FOR)
        .bind(now - HELD_FOR)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(endpoint_id, first_seen, last_seen, rejected_at)| HeldKey {
                    endpoint_id,
                    first_seen,
                    last_seen,
                    rejected_at,
                },
            )
            .collect())
    }

    /// Whether `endpoint_id` is held, not rejected and in its time, for
    /// live pair `id`, in `tx`: [`PeerError::NoSuchPeer`] or
    /// [`PeerError::KeyNotHeld`] if not.
    async fn check_held(
        &self,
        conn: &mut sqlx::SqliteConnection,
        id: &str,
        endpoint_id: &str,
    ) -> Result<(), PeerError> {
        if !peer_is_live(&mut *conn, id).await? {
            return Err(PeerError::NoSuchPeer(id.to_owned()));
        }
        let held: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM federation_peer_held_keys \
                            WHERE peer = ?1 AND endpoint_id = ?2 \
                              AND rejected_at IS NULL AND last_seen > ?3)",
        )
        .bind(id)
        .bind(endpoint_id)
        .bind(self.now() - HELD_FOR)
        .fetch_one(&mut *conn)
        .await?;
        if held {
            Ok(())
        } else {
            Err(PeerError::KeyNotHeld)
        }
    }

    /// Confirm `endpoint_id`, a key held for live pair `id`: the pin moves
    /// to it, the pair's held keys are forgotten, and the pair, its id and
    /// what hangs on it stay, but for the tokens minted for the old key.
    /// The row as it is now.
    /// [`PeerError::KeyNotHeld`] for a key that is not held for it (the
    /// admin confirms the key they saw, or nothing), and
    /// [`PeerError::AlreadyPaired`] for one that is another live pair's.
    pub async fn confirm_peer_key(
        &self,
        id: &str,
        endpoint_id: &str,
    ) -> Result<PeerRecord, PeerError> {
        let mut tx = self.begin_write().await?;
        self.check_held(&mut tx, id, endpoint_id).await?;
        let taken: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM federation_peers \
                            WHERE revoked_at IS NULL AND endpoint_id = ?1)",
        )
        .bind(endpoint_id)
        .fetch_one(&mut *tx)
        .await?;
        if taken {
            return Err(PeerError::AlreadyPaired);
        }
        // The tokens minted for the old key go with its pin (F8-03): they
        // were its proxied plays', and it is no one's now.
        sqlx::query(
            "DELETE FROM federation_tokens WHERE peer = ?1 \
               AND device = (SELECT endpoint_id FROM federation_peers WHERE id = ?1)",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        let row = sqlx::query(concat!(
            "UPDATE federation_peers SET endpoint_id = ?2 WHERE id = ?1 RETURNING ",
            columns!()
        ))
        .bind(id)
        .bind(endpoint_id)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM federation_peer_held_keys WHERE peer = ?1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(record(&row)?)
    }

    /// Reject `endpoint_id`, a key held for live pair `id`: it reaches
    /// nothing, and is refused, not held again, for [`REJECTED_FOR`]. The
    /// pair's row. [`PeerError::KeyNotHeld`] as for
    /// [`Self::confirm_peer_key`].
    pub async fn reject_peer_key(
        &self,
        id: &str,
        endpoint_id: &str,
    ) -> Result<PeerRecord, PeerError> {
        let mut tx = self.begin_write().await?;
        self.check_held(&mut tx, id, endpoint_id).await?;
        sqlx::query(
            "UPDATE federation_peer_held_keys SET rejected_at = ?3 \
             WHERE peer = ?1 AND endpoint_id = ?2",
        )
        .bind(id)
        .bind(endpoint_id)
        .bind(self.now())
        .execute(&mut *tx)
        .await?;
        let peer = fetch_peer(&mut tx, id)
            .await?
            .ok_or_else(|| PeerError::NoSuchPeer(id.to_owned()))?;
        tx.commit().await?;
        Ok(peer)
    }

    /// Pair `id` is connected now: its last seen moves to now, written
    /// at most once a minute.
    pub async fn peer_seen(&self, id: &str) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE federation_peers SET last_seen = ?2 \
             WHERE id = ?1 AND (last_seen IS NULL OR last_seen <= ?2 - 60)",
        )
        .bind(id)
        .bind(self.now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Pair `id` is reached at `addrs` from now: for a test whose pair's
    /// new key answers elsewhere.
    #[cfg(test)]
    pub(crate) async fn set_peer_addrs(
        &self,
        id: &str,
        addrs: &[String],
    ) -> Result<(), StoreError> {
        sqlx::query("UPDATE federation_peers SET addrs = ?2 WHERE id = ?1")
            .bind(id)
            .bind(serde_json::to_string(addrs).expect("strings serialize"))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The live pair whose key is `endpoint_id` said hello under another
    /// server id now, and was refused.
    pub async fn peer_identity_refused(&self, endpoint_id: &str) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE federation_peers SET identity_refused_at = ?2 \
             WHERE endpoint_id = ?1 AND revoked_at IS NULL",
        )
        .bind(endpoint_id)
        .bind(self.now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Pair `id`'s last seen connected and its last hello under another
    /// server id, Unix seconds: `None` for one never so.
    pub async fn peer_presence(&self, id: &str) -> Result<(Option<i64>, Option<i64>), StoreError> {
        let row: Option<(Option<i64>, Option<i64>)> = sqlx::query_as(
            "SELECT last_seen, identity_refused_at FROM federation_peers WHERE id = ?1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::super::invites::{InviteKind, InviteOrigin, NewInvite};
    use super::super::tests::{TestStore, test_store};
    use super::*;
    use spoolframe_types::Role;

    const SB_KEY: &str = "0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b";
    const SB_NEW_KEY: &str = "0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c";
    const SB_ID: &str = "mzxw6ytbojtg633c";

    fn addrs() -> Vec<String> {
        vec!["203.0.113.7:7420".to_owned()]
    }

    fn sb<'a>(addrs: &'a [String]) -> NewPeer<'a> {
        NewPeer {
            server_id: SB_ID,
            endpoint_id: SB_KEY,
            display_name: "Bob's server",
            addrs,
            relays: &[],
        }
    }

    async fn with_invite(test: &str, kind: InviteKind, uses: u32) -> TestStore {
        let store = test_store(test).await;
        store
            .create_invite(NewInvite {
                kind,
                capability_hash: "h1",
                role: Role::Consumer,
                note: None,
                uses,
                expires_at: store.now() + 3600,
                created_by: "alice",
                origin: &InviteOrigin::Web,
            })
            .await
            .unwrap();
        store
    }

    #[tokio::test]
    async fn redeeming_a_federation_invite_pairs_and_takes_a_use() {
        let store = with_invite("fed-pair", InviteKind::Federation, 2).await;
        let addrs = addrs();
        let PairRedeemed::Paired(peer) = store.pair_by_invite("h1", sb(&addrs)).await.unwrap()
        else {
            panic!("not paired");
        };
        assert_eq!(
            (
                peer.server_id.as_str(),
                peer.endpoint_id.as_str(),
                peer.paired_by.as_str()
            ),
            (SB_ID, SB_KEY, "alice")
        );
        assert_eq!(peer.addrs, addrs);
        assert!(peer.live());
        let invite = &store.live_invites().await.unwrap()[0];
        assert_eq!(peer.invite_id.as_deref(), Some(invite.id.as_str()));
        assert_eq!(invite.uses_left, 1);
        assert_eq!(
            store.live_peer_by_endpoint(SB_KEY).await.unwrap(),
            Some((*peer).clone())
        );
        assert_eq!(
            store.live_peer_by_server_id(SB_ID).await.unwrap(),
            Some(*peer)
        );
    }

    /// A server paired already makes nothing, by key or by server id, and
    /// the invite keeps its use.
    #[tokio::test]
    async fn a_paired_server_cannot_pair_again_and_the_use_is_kept() {
        let store = with_invite("fed-pair-again", InviteKind::Federation, 3).await;
        store.pair_by_invite("h1", sb(&[])).await.unwrap();
        let same_key = NewPeer {
            server_id: "aaaaaaaaaaaaaaaa",
            ..sb(&[])
        };
        let same_id = NewPeer {
            endpoint_id: SB_NEW_KEY,
            ..sb(&[])
        };
        for peer in [same_key, same_id] {
            assert_eq!(
                store.pair_by_invite("h1", peer).await.unwrap(),
                PairRedeemed::AlreadyPaired
            );
        }
        assert_eq!(store.live_invites().await.unwrap()[0].uses_left, 2);
        assert_eq!(store.federation_peers().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn another_kind_s_code_pairs_nothing_and_keeps_its_use() {
        let store = with_invite("fed-wrong-kind", InviteKind::CreateUser, 1).await;
        assert_eq!(
            store.pair_by_invite("h1", sb(&[])).await.unwrap(),
            PairRedeemed::Refused(InviteState::Usable {
                kind: InviteKind::CreateUser,
                role: Role::Consumer,
            })
        );
        assert_eq!(
            store.pair_by_invite("no-such", sb(&[])).await.unwrap(),
            PairRedeemed::Refused(InviteState::Invalid)
        );
        assert_eq!(store.live_invites().await.unwrap()[0].uses_left, 1);
        assert!(store.federation_peers().await.unwrap().is_empty());
    }

    /// Unpairing keeps the row, revoked; pairing again makes a new one.
    /// A pair seen connected is kept as seen at most once a minute; a
    /// hello under another server id is kept for its live row alone.
    #[tokio::test]
    async fn a_pair_s_last_seen_and_a_changed_id_are_kept() {
        let store = test_store("fed-presence").await;
        let peer = store.add_peer(sb(&addrs()), "alice").await.unwrap();
        assert_eq!(store.peer_presence(&peer.id).await.unwrap(), (None, None));
        let first = store.now();
        store.peer_seen(&peer.id).await.unwrap();
        store.clock().advance(std::time::Duration::from_secs(30));
        store.peer_seen(&peer.id).await.unwrap();
        assert_eq!(store.peer_presence(&peer.id).await.unwrap().0, Some(first));
        store.clock().advance(std::time::Duration::from_secs(30));
        store.peer_seen(&peer.id).await.unwrap();
        assert_eq!(
            store.peer_presence(&peer.id).await.unwrap().0,
            Some(first + 60)
        );
        store.peer_identity_refused(SB_NEW_KEY).await.unwrap();
        assert_eq!(store.peer_presence(&peer.id).await.unwrap().1, None);
        store.peer_identity_refused(SB_KEY).await.unwrap();
        assert_eq!(
            store.peer_presence(&peer.id).await.unwrap().1,
            Some(first + 60)
        );
    }

    #[tokio::test]
    async fn an_unpaired_row_is_kept_and_a_new_pair_is_a_new_row() {
        let store = with_invite("fed-unpair", InviteKind::Federation, 2).await;
        let PairRedeemed::Paired(first) = store.pair_by_invite("h1", sb(&[])).await.unwrap() else {
            panic!("not paired");
        };
        store.clock().advance(std::time::Duration::from_secs(5));
        let revoked = store.revoke_peer(&first.id).await.unwrap().unwrap();
        assert_eq!(revoked.revoked_at, Some(store.now()));
        assert_eq!(store.revoke_peer(&first.id).await.unwrap(), None);
        assert_eq!(store.live_peer_by_endpoint(SB_KEY).await.unwrap(), None);
        let PairRedeemed::Paired(second) = store.pair_by_invite("h1", sb(&[])).await.unwrap()
        else {
            panic!("not paired again");
        };
        assert_ne!(second.id, first.id);
        let all = store.federation_peers().await.unwrap();
        assert_eq!(
            all.iter()
                .map(|p| (p.id.as_str(), p.live()))
                .collect::<Vec<_>>(),
            [(second.id.as_str(), true), (first.id.as_str(), false)]
        );
    }

    /// The side that redeemed another's invite keeps its row with no
    /// invite of its own.
    #[tokio::test]
    async fn the_redeeming_side_keeps_its_row_paired_by_its_admin() {
        let store = test_store("fed-add").await;
        let peer = store.add_peer(sb(&addrs()), "bob").await.unwrap();
        assert_eq!(peer.paired_by, "bob");
        assert_eq!(peer.invite_id, None);
        assert!(matches!(
            store.add_peer(sb(&[]), "bob").await,
            Err(PeerError::AlreadyPaired)
        ));
        assert_eq!(
            store
                .revoke_peer_by_endpoint(SB_KEY)
                .await
                .unwrap()
                .map(|p| p.id),
            Some(peer.id)
        );
        assert!(store.live_peer_by_endpoint(SB_KEY).await.unwrap().is_none());
    }

    /// A key of its own for each `n`, hex.
    fn key(n: u8) -> String {
        format!("{n:02x}").repeat(32)
    }

    fn held(keys: &[HeldKey]) -> Vec<&str> {
        keys.iter().map(|k| k.endpoint_id.as_str()).collect()
    }

    /// P8-01's key rotation: the admin confirms the key they saw, which
    /// moves the pin and forgets the pair's other held keys. A key that
    /// said hello after it is never the one pinned.
    #[tokio::test]
    async fn the_key_confirmed_is_the_one_named_never_a_later_one() {
        let store = test_store("fed-key").await;
        let peer = store.add_peer(sb(&[]), "bob").await.unwrap();
        let (a, b) = (key(0xa1), key(0xb2));
        assert_eq!(
            store.hold_peer_key(&peer.id, &a).await.unwrap(),
            KeyHold::New
        );
        store.clock().advance(std::time::Duration::from_secs(5));
        assert_eq!(
            store.hold_peer_key(&peer.id, &b).await.unwrap(),
            KeyHold::New
        );
        assert_eq!(
            held(&store.held_peer_keys(&peer.id).await.unwrap()),
            [a.as_str(), b.as_str()]
        );
        let confirmed = store.confirm_peer_key(&peer.id, &a).await.unwrap();
        assert_eq!(
            (confirmed.id.as_str(), confirmed.endpoint_id.as_str()),
            (peer.id.as_str(), a.as_str())
        );
        assert!(store.held_peer_keys(&peer.id).await.unwrap().is_empty());
        assert_eq!(store.live_peer_by_endpoint(SB_KEY).await.unwrap(), None);
        assert_eq!(store.live_peer_by_endpoint(&b).await.unwrap(), None);
        assert_eq!(
            store.live_peer_by_endpoint(&a).await.unwrap().map(|p| p.id),
            Some(peer.id)
        );
    }

    /// Confirming or rejecting a key that is not held changes nothing:
    /// the pin and the held keys stay as they were.
    #[tokio::test]
    async fn a_key_that_is_not_held_is_neither_confirmed_nor_rejected() {
        let store = test_store("fed-key-not-held").await;
        let peer = store.add_peer(sb(&[]), "bob").await.unwrap();
        let (a, b) = (key(0xa1), key(0xb2));
        store.hold_peer_key(&peer.id, &a).await.unwrap();
        let before = store.held_peer_keys(&peer.id).await.unwrap();
        assert!(matches!(
            store.confirm_peer_key(&peer.id, &b).await,
            Err(PeerError::KeyNotHeld)
        ));
        assert!(matches!(
            store.reject_peer_key(&peer.id, &b).await,
            Err(PeerError::KeyNotHeld)
        ));
        assert_eq!(store.held_peer_keys(&peer.id).await.unwrap(), before);
        assert_eq!(
            store.federation_peer(&peer.id).await.unwrap().unwrap(),
            peer
        );
    }

    /// A rejected key is refused, not held again, for a day; then it is
    /// forgotten.
    #[tokio::test]
    async fn a_rejected_key_is_refused_for_a_day() {
        let store = test_store("fed-key-rejected").await;
        let peer = store.add_peer(sb(&[]), "bob").await.unwrap();
        let a = key(0xa1);
        store.hold_peer_key(&peer.id, &a).await.unwrap();
        let row = store.reject_peer_key(&peer.id, &a).await.unwrap();
        assert_eq!(row.endpoint_id, SB_KEY);
        assert_eq!(
            store.hold_peer_key(&peer.id, &a).await.unwrap(),
            KeyHold::Rejected
        );
        let kept = store.held_peer_keys(&peer.id).await.unwrap();
        assert_eq!(kept[0].rejected_at, Some(store.now()));
        assert!(matches!(
            store.confirm_peer_key(&peer.id, &a).await,
            Err(PeerError::KeyNotHeld)
        ));
        store
            .clock()
            .advance(std::time::Duration::from_secs(REJECTED_FOR as u64));
        assert!(store.held_peer_keys(&peer.id).await.unwrap().is_empty());
        assert_eq!(
            store.hold_peer_key(&peer.id, &a).await.unwrap(),
            KeyHold::New
        );
    }

    /// With [`MAX_HELD_KEYS`] kept, a new key is dropped, and the keys the
    /// admin sees stay; a rejected one counts until it expires.
    #[tokio::test]
    async fn with_the_held_keys_full_a_new_key_is_not_held() {
        let store = test_store("fed-key-full").await;
        let peer = store.add_peer(sb(&[]), "bob").await.unwrap();
        let keys: Vec<String> = (1..=MAX_HELD_KEYS as u8).map(key).collect();
        for k in &keys {
            assert_eq!(
                store.hold_peer_key(&peer.id, k).await.unwrap(),
                KeyHold::New
            );
        }
        store.reject_peer_key(&peer.id, &keys[0]).await.unwrap();
        let late = key(0xee);
        assert_eq!(
            store.hold_peer_key(&peer.id, &late).await.unwrap(),
            KeyHold::Full
        );
        assert_eq!(
            held(&store.held_peer_keys(&peer.id).await.unwrap()),
            keys.iter().map(String::as_str).collect::<Vec<_>>()
        );
        // A key held already is still seen.
        assert_eq!(
            store.hold_peer_key(&peer.id, &keys[1]).await.unwrap(),
            KeyHold::Held
        );
    }

    /// A key held already is seen again without a write, but once per
    /// [`SEEN_EVERY`]; one not seen for [`HELD_FOR`] is forgotten.
    #[tokio::test]
    async fn a_held_key_is_seen_at_most_hourly_and_forgotten_after_a_week() {
        let store = test_store("fed-key-seen").await;
        let peer = store.add_peer(sb(&[]), "bob").await.unwrap();
        let a = key(0xa1);
        store.hold_peer_key(&peer.id, &a).await.unwrap();
        let first = store.now();
        store.clock().advance(std::time::Duration::from_secs(600));
        store.hold_peer_key(&peer.id, &a).await.unwrap();
        let kept = &store.held_peer_keys(&peer.id).await.unwrap()[0];
        assert_eq!((kept.first_seen, kept.last_seen), (first, first));
        store
            .clock()
            .advance(std::time::Duration::from_secs(SEEN_EVERY as u64));
        store.hold_peer_key(&peer.id, &a).await.unwrap();
        let kept = &store.held_peer_keys(&peer.id).await.unwrap()[0];
        assert_eq!((kept.first_seen, kept.last_seen), (first, store.now()));

        store
            .clock()
            .advance(std::time::Duration::from_secs(HELD_FOR as u64));
        assert!(store.held_peer_keys(&peer.id).await.unwrap().is_empty());
        assert!(matches!(
            store.confirm_peer_key(&peer.id, &a).await,
            Err(PeerError::KeyNotHeld)
        ));
    }

    #[tokio::test]
    async fn an_unpaired_server_s_key_is_neither_held_nor_confirmed() {
        let store = test_store("fed-key-unpaired").await;
        let peer = store.add_peer(sb(&[]), "bob").await.unwrap();
        store.hold_peer_key(&peer.id, SB_NEW_KEY).await.unwrap();
        store.revoke_peer(&peer.id).await.unwrap();
        assert!(store.held_peer_keys(&peer.id).await.unwrap().is_empty());
        assert!(matches!(
            store.confirm_peer_key(&peer.id, SB_NEW_KEY).await,
            Err(PeerError::NoSuchPeer(_))
        ));
        assert!(matches!(
            store.reject_peer_key(&peer.id, SB_NEW_KEY).await,
            Err(PeerError::NoSuchPeer(_))
        ));
        assert_eq!(
            store.hold_peer_key(&peer.id, SB_NEW_KEY).await.unwrap(),
            KeyHold::NoPeer
        );
        assert!(store.held_peer_keys(&peer.id).await.unwrap().is_empty());
    }

    /// A key that is already another live pair's is not moved onto this
    /// one.
    #[tokio::test]
    async fn a_held_key_that_is_another_pair_s_is_not_confirmed() {
        let store = test_store("fed-key-taken").await;
        let peer = store.add_peer(sb(&[]), "bob").await.unwrap();
        store
            .add_peer(
                NewPeer {
                    server_id: "aaaaaaaaaaaaaaaa",
                    endpoint_id: SB_NEW_KEY,
                    ..sb(&[])
                },
                "bob",
            )
            .await
            .unwrap();
        store.hold_peer_key(&peer.id, SB_NEW_KEY).await.unwrap();
        assert!(matches!(
            store.confirm_peer_key(&peer.id, SB_NEW_KEY).await,
            Err(PeerError::AlreadyPaired)
        ));
    }

    /// Two servers redeem a one-use invite at once: exactly one pairs, and
    /// the other is told the invite is used.
    #[tokio::test]
    async fn two_redemptions_of_a_one_use_invite_at_once_pair_one() {
        let store = with_invite("fed-race-two", InviteKind::Federation, 1).await;
        let (k1, k2) = (key(1), key(2));
        let first = NewPeer {
            server_id: "aaaaaaaaaaaaaaaa",
            endpoint_id: &k1,
            ..sb(&[])
        };
        let second = NewPeer {
            server_id: "bbbbbbbbbbbbbbbb",
            endpoint_id: &k2,
            ..sb(&[])
        };
        let (one, two) = tokio::join!(
            store.pair_by_invite("h1", first),
            store.pair_by_invite("h1", second)
        );
        let outcomes = [one.unwrap(), two.unwrap()];
        let paired = outcomes
            .iter()
            .filter(|o| matches!(o, PairRedeemed::Paired(_)))
            .count();
        assert_eq!(paired, 1, "{outcomes:?}");
        assert!(
            outcomes.contains(&PairRedeemed::Refused(InviteState::Exhausted)),
            "{outcomes:?}"
        );
        assert_eq!(store.federation_peers().await.unwrap().len(), 1);
    }

    /// A redemption racing the invite's revocation pairs, or makes
    /// nothing: never a pair without its use, nor a use without its pair.
    #[tokio::test]
    async fn a_redemption_racing_a_revocation_pairs_whole_or_not_at_all() {
        let store = with_invite("fed-race-revoke", InviteKind::Federation, 2).await;
        let invite = store.live_invites().await.unwrap()[0].id.clone();
        let (redeemed, revoked) = tokio::join!(
            store.pair_by_invite("h1", sb(&[])),
            store.revoke_invite(&invite)
        );
        revoked.unwrap();
        let peers = store.federation_peers().await.unwrap();
        let uses_left: i64 = sqlx::query_scalar("SELECT uses_left FROM invites WHERE id = ?1")
            .bind(&invite)
            .fetch_one(&store.pool)
            .await
            .unwrap();
        match redeemed.unwrap() {
            PairRedeemed::Paired(peer) => {
                assert_eq!(peers, [*peer]);
                assert_eq!(uses_left, 1);
            }
            other => {
                assert_eq!(other, PairRedeemed::Refused(InviteState::Invalid));
                assert!(peers.is_empty());
                assert_eq!(uses_left, 2);
            }
        }
    }
}
