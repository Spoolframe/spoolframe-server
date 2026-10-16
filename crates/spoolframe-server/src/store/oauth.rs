//! OAuth clients, grants, codes and tokens (PX-42). The rules (scopes,
//! PKCE, lifetimes) live in `crate::oauth`; this is storage. Codes and
//! tokens arrive here already hashed.

use spoolframe_types::Role;

use super::users::{LocalUserId, role};
use super::{Store, StoreError};

/// The seconds in a day: what "calls today" is counted by, in UTC.
const DAY: i64 = 86_400;

/// A client as it registered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientRecord {
    pub id: String,
    pub name: String,
    pub redirect_uris: Vec<String>,
    pub created_at: i64,
}

/// A consent to keep: the grant, and the code that goes back to the client.
#[derive(Debug, Clone, Copy)]
pub struct NewGrant<'a> {
    pub client_id: &'a str,
    pub user: LocalUserId,
    /// Space-separated.
    pub scopes: &'a str,
    pub redirect_uri: &'a str,
    pub code_hash: &'a str,
    pub code_challenge: &'a str,
    pub code_expires_at: i64,
}

/// A pair of tokens to keep for a grant, by their hashes.
#[derive(Debug, Clone, Copy)]
pub struct NewTokens<'a> {
    pub access_hash: &'a str,
    pub access_expires_at: i64,
    pub refresh_hash: &'a str,
    pub refresh_expires_at: i64,
}

/// What a code given back to the token endpoint turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Redeemed {
    /// Unused and in time: spent now.
    Fresh {
        grant_id: String,
        client_id: String,
        redirect_uri: String,
        code_challenge: String,
        scopes: String,
    },
    /// Used before: its grant, and every token of it, is gone now.
    Reused,
    /// Unknown, or expired.
    Unknown,
}

/// What a refresh token given back turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refreshed {
    /// Live and the client's: spent now, and the new pair kept.
    Rotated { scopes: String },
    /// Spent before: its grant is gone now.
    Reused,
    /// Unknown, expired, another client's, or its user disabled.
    Unknown,
}

/// Whose an access token is, read at each use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OauthAccess {
    pub grant_id: String,
    pub client_name: String,
    pub user: LocalUserId,
    /// The user's role now, not at consent.
    pub role: Role,
    /// Space-separated.
    pub scopes: String,
}

/// A grant as My profile › Connected apps lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantRecord {
    pub id: String,
    pub user: LocalUserId,
    pub username: String,
    pub client_name: String,
    pub redirect_uri: String,
    /// Space-separated.
    pub scopes: String,
    pub created_at: i64,
    pub last_call_at: Option<i64>,
    /// Today's (UTC) calls.
    pub calls_today: u32,
}

/// The grants' columns, joined with their client and user, as
/// [`GrantRecord`] reads them; `?1` is today's UTC day.
macro_rules! grant_columns {
    () => {
        "SELECT g.id, g.user_id, u.username, c.name, g.redirect_uri, g.scopes, \
           g.created_at, g.last_call_at, \
           CASE WHEN g.calls_day = ?1 THEN g.calls_today ELSE 0 END \
         FROM oauth_grants g JOIN oauth_clients c ON c.id = g.client_id \
         JOIN users u ON u.id = g.user_id "
    };
}

type GrantRow = (
    String,
    LocalUserId,
    String,
    String,
    String,
    String,
    i64,
    Option<i64>,
    i64,
);

fn grant(row: GrantRow) -> GrantRecord {
    let (id, user, username, client_name, redirect_uri, scopes, created_at, last_call_at, calls) =
        row;
    GrantRecord {
        id,
        user,
        username,
        client_name,
        redirect_uri,
        scopes,
        created_at,
        last_call_at,
        calls_today: u32::try_from(calls).unwrap_or(u32::MAX),
    }
}

impl Store {
    /// The grants of `user`, or of everyone: the most recent first.
    pub async fn oauth_grants(
        &self,
        user: Option<LocalUserId>,
    ) -> Result<Vec<GrantRecord>, StoreError> {
        let rows: Vec<GrantRow> = sqlx::query_as(concat!(
            grant_columns!(),
            "WHERE (?2 IS NULL OR g.user_id = ?2) ORDER BY g.created_at DESC, g.id DESC"
        ))
        .bind(self.now().div_euclid(DAY))
        .bind(user)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(grant).collect())
    }

    /// The grant `id`, if there is one.
    pub async fn oauth_grant(&self, id: &str) -> Result<Option<GrantRecord>, StoreError> {
        let row: Option<GrantRow> = sqlx::query_as(concat!(grant_columns!(), "WHERE g.id = ?2"))
            .bind(self.now().div_euclid(DAY))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(grant))
    }

    /// Keep a client that registered itself.
    pub async fn add_oauth_client(
        &self,
        id: &str,
        name: &str,
        redirect_uris: &[String],
    ) -> Result<(), StoreError> {
        let uris = serde_json::to_string(redirect_uris).map_err(super::decode_error)?;
        sqlx::query(
            "INSERT INTO oauth_clients (id, name, redirect_uris, created_at) \
             VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(id)
        .bind(name)
        .bind(uris)
        .bind(self.now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The client `id`, if it is registered.
    pub async fn oauth_client(&self, id: &str) -> Result<Option<ClientRecord>, StoreError> {
        let row: Option<(String, String, String, i64)> = sqlx::query_as(
            "SELECT id, name, redirect_uris, created_at FROM oauth_clients WHERE id = ?1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(id, name, uris, created_at)| {
            Ok(ClientRecord {
                id,
                name,
                redirect_uris: serde_json::from_str(&uris).map_err(super::decode_error)?,
                created_at,
            })
        })
        .transpose()
    }

    /// Keep a consent and its code. A grant the user gave the client
    /// before is replaced, and its tokens stop working: consent is asked
    /// each time, and the last one is what holds. The new grant's id.
    pub async fn grant_oauth(&self, grant: NewGrant<'_>) -> Result<String, StoreError> {
        let id = uuid::Uuid::now_v7().to_string();
        let now = self.now();
        let mut tx = self.begin_write().await?;
        sqlx::query("DELETE FROM oauth_grants WHERE client_id = ?1 AND user_id = ?2")
            .bind(grant.client_id)
            .bind(grant.user)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO oauth_grants (id, client_id, user_id, scopes, redirect_uri, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(&id)
        .bind(grant.client_id)
        .bind(grant.user)
        .bind(grant.scopes)
        .bind(grant.redirect_uri)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO oauth_codes (code_hash, grant_id, redirect_uri, code_challenge, expires_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(grant.code_hash)
        .bind(&id)
        .bind(grant.redirect_uri)
        .bind(grant.code_challenge)
        .bind(grant.code_expires_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Spend the code hashing to `code_hash`, if it is unused and in time.
    /// A code used before ends its grant: whoever holds it now may not be
    /// the client it was meant for (OAuth 2.1 §4.1.3).
    pub async fn redeem_oauth_code(&self, code_hash: &str) -> Result<Redeemed, StoreError> {
        let now = self.now();
        let mut tx = self.begin_write().await?;
        type Row = (String, Option<i64>, i64, String, String, String, String);
        let row: Option<Row> = sqlx::query_as(
            "SELECT c.grant_id, c.used_at, c.expires_at, g.client_id, c.redirect_uri, \
                    c.code_challenge, g.scopes \
             FROM oauth_codes c JOIN oauth_grants g ON g.id = c.grant_id \
             WHERE c.code_hash = ?1",
        )
        .bind(code_hash)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((grant_id, used_at, expires_at, client_id, redirect_uri, code_challenge, scopes)) =
            row
        else {
            return Ok(Redeemed::Unknown);
        };
        if used_at.is_some() {
            sqlx::query("DELETE FROM oauth_grants WHERE id = ?1")
                .bind(&grant_id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(Redeemed::Reused);
        }
        if expires_at <= now {
            return Ok(Redeemed::Unknown);
        }
        sqlx::query("UPDATE oauth_codes SET used_at = ?2 WHERE code_hash = ?1")
            .bind(code_hash)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(Redeemed::Fresh {
            grant_id,
            client_id,
            redirect_uri,
            code_challenge,
            scopes,
        })
    }

    /// Keep a new pair of tokens for `grant_id`.
    pub async fn add_oauth_tokens(
        &self,
        grant_id: &str,
        tokens: NewTokens<'_>,
    ) -> Result<(), StoreError> {
        let mut tx = self.begin_write().await?;
        insert_tokens(&mut tx, grant_id, tokens).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Spend the refresh token hashing to `refresh_hash`, if it is live
    /// and `client_id`'s, and keep `tokens` in its place, in one
    /// transaction. One spent before ends its grant: two parties hold it.
    pub async fn refresh_oauth(
        &self,
        refresh_hash: &str,
        client_id: &str,
        tokens: NewTokens<'_>,
    ) -> Result<Refreshed, StoreError> {
        let now = self.now();
        let mut tx = self.begin_write().await?;
        type Row = (String, Option<i64>, i64, String, String, Option<i64>);
        let row: Option<Row> = sqlx::query_as(
            "SELECT t.grant_id, t.used_at, t.expires_at, g.client_id, g.scopes, u.disabled_at \
             FROM oauth_tokens t JOIN oauth_grants g ON g.id = t.grant_id \
             JOIN users u ON u.id = g.user_id \
             WHERE t.token_hash = ?1 AND t.kind = 'refresh'",
        )
        .bind(refresh_hash)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((grant_id, used_at, expires_at, owner, scopes, disabled_at)) = row else {
            return Ok(Refreshed::Unknown);
        };
        if owner != client_id {
            return Ok(Refreshed::Unknown);
        }
        if used_at.is_some() {
            sqlx::query("DELETE FROM oauth_grants WHERE id = ?1")
                .bind(&grant_id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(Refreshed::Reused);
        }
        if expires_at <= now || disabled_at.is_some() {
            return Ok(Refreshed::Unknown);
        }
        sqlx::query("UPDATE oauth_tokens SET used_at = ?2 WHERE token_hash = ?1")
            .bind(refresh_hash)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        insert_tokens(&mut tx, &grant_id, tokens).await?;
        tx.commit().await?;
        Ok(Refreshed::Rotated { scopes })
    }

    /// Whose the access token hashing to `token_hash` is, if it is live:
    /// in time, its grant kept, its user not disabled. Read on every call,
    /// so a revoke or a disable stops the next one.
    pub async fn oauth_access(&self, token_hash: &str) -> Result<Option<OauthAccess>, StoreError> {
        let row: Option<(String, String, LocalUserId, String, String)> = sqlx::query_as(
            "SELECT g.id, c.name, g.user_id, u.role, g.scopes \
             FROM oauth_tokens t JOIN oauth_grants g ON g.id = t.grant_id \
             JOIN oauth_clients c ON c.id = g.client_id JOIN users u ON u.id = g.user_id \
             WHERE t.token_hash = ?1 AND t.kind = 'access' AND t.expires_at > ?2 \
               AND u.disabled_at IS NULL",
        )
        .bind(token_hash)
        .bind(self.now())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(
            |(grant_id, client_name, user, stored, scopes)| OauthAccess {
                grant_id,
                client_name,
                user,
                role: role(stored),
                scopes,
            },
        ))
    }

    /// Count a call made with `grant_id`'s tokens: its last call is now,
    /// and today's count, which starts over each UTC day, goes up by one.
    pub async fn note_oauth_call(&self, grant_id: &str) -> Result<(), StoreError> {
        let now = self.now();
        sqlx::query(
            "UPDATE oauth_grants SET last_call_at = ?2, \
               calls_today = CASE WHEN calls_day = ?3 THEN calls_today + 1 ELSE 1 END, \
               calls_day = ?3 \
             WHERE id = ?1",
        )
        .bind(grant_id)
        .bind(now)
        .bind(now.div_euclid(DAY))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// How many calls `grant_id`'s tokens made today (UTC), and when the
    /// last was; `None` for no such grant.
    pub async fn oauth_calls(
        &self,
        grant_id: &str,
    ) -> Result<Option<(u32, Option<i64>)>, StoreError> {
        let row: Option<(i64, Option<i64>)> = sqlx::query_as(
            "SELECT CASE WHEN calls_day = ?2 THEN calls_today ELSE 0 END, last_call_at \
             FROM oauth_grants WHERE id = ?1",
        )
        .bind(grant_id)
        .bind(self.now().div_euclid(DAY))
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(calls, last)| (u32::try_from(calls).unwrap_or(u32::MAX), last)))
    }

    /// End the grant `grant_id`: its codes and tokens go with it. Whether
    /// there was one.
    pub async fn revoke_oauth_grant(&self, grant_id: &str) -> Result<bool, StoreError> {
        Ok(sqlx::query("DELETE FROM oauth_grants WHERE id = ?1")
            .bind(grant_id)
            .execute(&self.pool)
            .await?
            .rows_affected()
            > 0)
    }

    /// Drop what can no longer be used: codes and tokens past their time,
    /// and clients registered more than `unused_client_secs` ago that no
    /// user granted anything. How many rows went.
    pub async fn purge_oauth(&self, unused_client_secs: i64) -> Result<u64, StoreError> {
        let now = self.now();
        let mut removed = 0;
        removed += sqlx::query("DELETE FROM oauth_codes WHERE expires_at <= ?1")
            .bind(now)
            .execute(&self.pool)
            .await?
            .rows_affected();
        removed += sqlx::query("DELETE FROM oauth_tokens WHERE expires_at <= ?1")
            .bind(now)
            .execute(&self.pool)
            .await?
            .rows_affected();
        removed += sqlx::query(
            "DELETE FROM oauth_clients WHERE created_at <= ?1 \
             AND NOT EXISTS (SELECT 1 FROM oauth_grants g WHERE g.client_id = oauth_clients.id)",
        )
        .bind(now - unused_client_secs)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(removed)
    }
}

async fn insert_tokens(
    tx: &mut sqlx::SqliteConnection,
    grant_id: &str,
    tokens: NewTokens<'_>,
) -> Result<(), sqlx::Error> {
    for (hash, kind, expires_at) in [
        (tokens.access_hash, "access", tokens.access_expires_at),
        (tokens.refresh_hash, "refresh", tokens.refresh_expires_at),
    ] {
        sqlx::query(
            "INSERT INTO oauth_tokens (token_hash, grant_id, kind, expires_at) \
             VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(hash)
        .bind(grant_id)
        .bind(kind)
        .bind(expires_at)
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}
