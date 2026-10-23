//! Delegated authentication between paired servers (P8-03): the tokens
//! this server mints for a paired server's users, by their hash, and the
//! plays counted across each pair, one number per server and side.

use super::{Store, StoreError};

/// A token to keep: its hash and what it opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewToken<'a> {
    /// blake3 of the token, hex.
    pub token_hash: &'a str,
    pub peer: &'a str,
    /// `remote:<serverid>:<uuid>`.
    pub principal: &'a str,
    pub item: &'a str,
    pub library: &'a str,
    /// The device's endpoint id, hex.
    pub device: &'a str,
    /// `play`; P8-07 adds `download`.
    pub scope: &'a str,
    pub expires_at: i64,
}

/// A token kept, as P8-04 reads it to redeem it.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct TokenRecord {
    pub peer: String,
    pub principal: String,
    pub item: String,
    pub library: String,
    pub device: String,
    pub scope: String,
    pub created_at: i64,
    pub expires_at: i64,
    pub used_at: Option<i64>,
}

/// The most tokens that work at once (H3): per pair and user, and per
/// pair. A play takes one token, for minutes; past these the pair is told
/// to try later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenLimits {
    pub per_user: u64,
    pub per_peer: u64,
}

/// Whether a token was kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKept {
    Kept,
    /// Its library is not shared with the pair now: revoked, or the pair
    /// unpaired, since the item was placed.
    NotShared,
    /// The pair, or its user, holds as many tokens as it may.
    TooMany,
}

/// Which side of a pair a count is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaySide {
    /// This server's items played by the pair's users: counted as a token
    /// is minted.
    Served,
    /// This server's users playing the pair's items: counted as this
    /// server vouches for them.
    Asserted,
}

impl PlaySide {
    fn name(self) -> &'static str {
        match self {
            PlaySide::Served => "served",
            PlaySide::Asserted => "asserted",
        }
    }
}

/// The counts of one pair and side.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlayCount {
    pub plays: u64,
    pub refused: u64,
}

impl Store {
    /// Keep `token`, minted now, if its library is shared with its pair
    /// and the pair is within `limits`: checked and kept in one
    /// transaction, so a grant revoked meanwhile is never missed (M6).
    pub async fn keep_federation_token(
        &self,
        token: NewToken<'_>,
        limits: TokenLimits,
    ) -> Result<TokenKept, StoreError> {
        let mut tx = self.begin_write().await?;
        let shared: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM federation_grants g \
                            JOIN federation_peers p ON p.id = g.peer \
                            WHERE g.peer = ?1 AND g.library = ?2 \
                              AND g.revoked_at IS NULL AND p.revoked_at IS NULL)",
        )
        .bind(token.peer)
        .bind(token.library)
        .fetch_one(&mut *tx)
        .await?;
        if !shared {
            return Ok(TokenKept::NotShared);
        }
        let (of_user, of_peer): (i64, i64) = sqlx::query_as(
            "SELECT COUNT(*) FILTER (WHERE principal = ?2), COUNT(*) FROM federation_tokens \
             WHERE peer = ?1 AND expires_at > ?3 AND used_at IS NULL",
        )
        .bind(token.peer)
        .bind(token.principal)
        .bind(self.now())
        .fetch_one(&mut *tx)
        .await?;
        let past = |held: i64, limit: u64| u64::try_from(held).unwrap_or_default() >= limit;
        if past(of_user, limits.per_user) || past(of_peer, limits.per_peer) {
            return Ok(TokenKept::TooMany);
        }
        sqlx::query(
            "INSERT INTO federation_tokens \
                 (token_hash, peer, principal, item, library, device, scope, created_at, \
                  expires_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )
        .bind(token.token_hash)
        .bind(token.peer)
        .bind(token.principal)
        .bind(token.item)
        .bind(token.library)
        .bind(token.device)
        .bind(token.scope)
        .bind(self.now())
        .bind(token.expires_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(TokenKept::Kept)
    }

    /// The token whose hash is `token_hash`, expired or not.
    pub async fn federation_token(
        &self,
        token_hash: &str,
    ) -> Result<Option<TokenRecord>, StoreError> {
        Ok(sqlx::query_as(
            "SELECT peer, principal, item, library, device, scope, created_at, expires_at, \
                    used_at \
             FROM federation_tokens WHERE token_hash = ?1",
        )
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Delete pair `peer`'s tokens for items of `library`, or all of its
    /// tokens with `None`: how many went.
    pub async fn delete_federation_tokens(
        &self,
        peer: &str,
        library: Option<&str>,
    ) -> Result<u64, StoreError> {
        Ok(sqlx::query(
            "DELETE FROM federation_tokens WHERE peer = ?1 AND (?2 IS NULL OR library = ?2)",
        )
        .bind(peer)
        .bind(library)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// Delete the tokens past their expiry: how many went.
    pub async fn prune_federation_tokens(&self) -> Result<u64, StoreError> {
        Ok(
            sqlx::query("DELETE FROM federation_tokens WHERE expires_at <= ?1")
                .bind(self.now())
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }

    /// Count a play across pair `peer`, on `side`, now.
    pub async fn count_federation_play(
        &self,
        peer: &str,
        side: PlaySide,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO federation_plays (peer, side, plays) VALUES (?1, ?2, 1) \
             ON CONFLICT (peer, side) DO UPDATE SET plays = plays + 1",
        )
        .bind(peer)
        .bind(side.name())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Count an assertion refused across pair `peer`, on `side`.
    pub async fn count_federation_refusal(
        &self,
        peer: &str,
        side: PlaySide,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO federation_plays (peer, side, refused) VALUES (?1, ?2, 1) \
             ON CONFLICT (peer, side) DO UPDATE SET refused = refused + 1",
        )
        .bind(peer)
        .bind(side.name())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The counts of pair `peer` on `side`; all zero before the first.
    pub async fn federation_plays(
        &self,
        peer: &str,
        side: PlaySide,
    ) -> Result<PlayCount, StoreError> {
        let row: Option<(i64, i64)> = sqlx::query_as(
            "SELECT plays, refused FROM federation_plays \
             WHERE peer = ?1 AND side = ?2",
        )
        .bind(peer)
        .bind(side.name())
        .fetch_optional(&self.pool)
        .await?;
        Ok(
            row.map_or_else(PlayCount::default, |(plays, refused)| PlayCount {
                plays: u64::try_from(plays).unwrap_or_default(),
                refused: u64::try_from(refused).unwrap_or_default(),
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::federation::NewPeer;
    use super::super::tests::{TestStore, test_store};
    use super::*;

    async fn paired(test: &str) -> (TestStore, String) {
        let store = test_store(test).await;
        let peer = store
            .add_peer(
                NewPeer {
                    server_id: "mzxw6ytbojtg633c",
                    endpoint_id: &"0b".repeat(32),
                    display_name: "Bob's server",
                    addrs: &[],
                    relays: &[],
                },
                "alice",
            )
            .await
            .unwrap();
        for library in ["plex:1", "plex:2"] {
            store
                .grant_library(&peer.id, library, "alice")
                .await
                .unwrap();
        }
        (store, peer.id)
    }

    const ROOMY: TokenLimits = TokenLimits {
        per_user: 100,
        per_peer: 100,
    };

    async fn keep(store: &Store, token: NewToken<'_>) {
        assert_eq!(
            store.keep_federation_token(token, ROOMY).await.unwrap(),
            TokenKept::Kept
        );
    }

    const BOB: &str = "remote:mzxw6ytbojtg633c:00000000-0000-0000-0000-000000000007";

    fn token<'a>(hash: &'a str, peer: &'a str, library: &'a str, expires_at: i64) -> NewToken<'a> {
        NewToken {
            token_hash: hash,
            peer,
            principal: BOB,
            item: "plex:movie:1",
            library,
            device: "ab",
            scope: "play",
            expires_at,
        }
    }

    /// A token is found by its hash; past its expiry it is pruned, and a
    /// revoked grant deletes the tokens of its library alone.
    #[tokio::test]
    async fn tokens_expire_and_go_with_their_grant() {
        let (store, peer) = paired("fed-tokens").await;
        let now = store.now();
        keep(&store, token("h1", &peer, "plex:1", now + 300)).await;
        keep(&store, token("h2", &peer, "plex:2", now + 600)).await;
        let kept = store.federation_token("h1").await.unwrap().unwrap();
        assert_eq!((kept.item.as_str(), kept.used_at), ("plex:movie:1", None));
        assert_eq!(store.prune_federation_tokens().await.unwrap(), 0);
        store.clock().advance(std::time::Duration::from_secs(300));
        assert_eq!(store.prune_federation_tokens().await.unwrap(), 1);
        assert_eq!(store.federation_token("h1").await.unwrap(), None);
        assert_eq!(
            store
                .delete_federation_tokens(&peer, Some("plex:1"))
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            store
                .delete_federation_tokens(&peer, Some("plex:2"))
                .await
                .unwrap(),
            1
        );
    }

    /// Unpairing deletes every token of the pair.
    #[tokio::test]
    async fn unpairing_deletes_the_pair_s_tokens() {
        let (store, peer) = paired("fed-tokens-unpair").await;
        let later = store.now() + 300;
        keep(&store, token("h1", &peer, "plex:1", later)).await;
        store.revoke_peer(&peer).await.unwrap().unwrap();
        assert_eq!(store.federation_token("h1").await.unwrap(), None);
    }

    /// A token is kept only for a library shared with the pair now, and
    /// only while the pair's user, and the pair, hold fewer than their
    /// limits of working tokens; an expired or used one does not count
    /// (H3, M6).
    #[tokio::test]
    async fn a_token_is_kept_only_while_shared_and_within_the_limits() {
        let (store, peer) = paired("fed-tokens-limits").await;
        let now = store.now();
        let limits = TokenLimits {
            per_user: 2,
            per_peer: 3,
        };
        let kept = |token| store.keep_federation_token(token, limits);
        assert_eq!(
            kept(token("h0", &peer, "plex:9", now + 300)).await.unwrap(),
            TokenKept::NotShared
        );
        assert_eq!(
            kept(token("h1", &peer, "plex:1", now + 1)).await.unwrap(),
            TokenKept::Kept
        );
        assert_eq!(
            kept(token("h2", &peer, "plex:1", now + 300)).await.unwrap(),
            TokenKept::Kept
        );
        assert_eq!(
            kept(token("h3", &peer, "plex:1", now + 300)).await.unwrap(),
            TokenKept::TooMany,
            "Bob holds two"
        );
        store.clock().advance(std::time::Duration::from_secs(1));
        assert_eq!(
            kept(token("h4", &peer, "plex:1", now + 300)).await.unwrap(),
            TokenKept::Kept,
            "h1 has expired"
        );
        let carol = NewToken {
            principal: "remote:mzxw6ytbojtg633c:00000000-0000-0000-0000-000000000008",
            ..token("h5", &peer, "plex:1", now + 300)
        };
        assert_eq!(kept(carol).await.unwrap(), TokenKept::Kept);
        let dave = NewToken {
            principal: "remote:mzxw6ytbojtg633c:00000000-0000-0000-0000-000000000009",
            ..token("h6", &peer, "plex:1", now + 300)
        };
        assert_eq!(
            kept(dave).await.unwrap(),
            TokenKept::TooMany,
            "the pair holds three"
        );
        store.revoke_library_grant(&peer, "plex:2").await.unwrap();
        let revoked = NewToken {
            principal: "remote:mzxw6ytbojtg633c:00000000-0000-0000-0000-00000000000a",
            ..token("h7", &peer, "plex:2", now + 300)
        };
        let fresh = store.keep_federation_token(revoked, ROOMY).await.unwrap();
        assert_eq!(fresh, TokenKept::NotShared);
    }

    /// A token used is a play begun, no longer one held: a user who plays
    /// eleven things in five minutes is not stopped at ten.
    #[tokio::test]
    async fn a_used_token_does_not_count_against_the_limits() {
        let (store, peer) = paired("fed-tokens-used").await;
        let now = store.now();
        let limits = TokenLimits {
            per_user: 10,
            per_peer: 500,
        };
        for n in 0..11 {
            let hash = format!("h{n}");
            assert_eq!(
                store
                    .keep_federation_token(token(&hash, &peer, "plex:1", now + 300), limits)
                    .await
                    .unwrap(),
                TokenKept::Kept,
                "play {n}"
            );
            sqlx::query("UPDATE federation_tokens SET used_at = ?2 WHERE token_hash = ?1")
                .bind(&hash)
                .bind(now)
                .execute(&store.pool)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn plays_and_refusals_are_counted_per_pair_and_side() {
        let (store, peer) = paired("fed-plays").await;
        assert_eq!(
            store
                .federation_plays(&peer, PlaySide::Served)
                .await
                .unwrap(),
            PlayCount::default()
        );
        for _ in 0..2 {
            store
                .count_federation_play(&peer, PlaySide::Served)
                .await
                .unwrap();
        }
        store
            .count_federation_refusal(&peer, PlaySide::Served)
            .await
            .unwrap();
        let served = store
            .federation_plays(&peer, PlaySide::Served)
            .await
            .unwrap();
        assert_eq!((served.plays, served.refused), (2, 1));
        assert_eq!(
            store
                .federation_plays(&peer, PlaySide::Asserted)
                .await
                .unwrap(),
            PlayCount::default()
        );
    }
}
