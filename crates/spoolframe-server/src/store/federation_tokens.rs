//! Delegated authentication between paired servers (P8-03): the tokens
//! this server mints for a paired server's users, by their hash, and the
//! plays counted across each pair, one number per server and side. P8-04
//! redeems a token once ([`Store::redeem_federation_token`]) and counts
//! the play served then, by who redeemed it.

use super::{Store, StoreError};

/// A day in seconds: plays are also counted a UTC day at a time, for the
/// Federation page's week.
pub const DAY: i64 = 86_400;

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
    /// `play` or `download` (P8-07).
    pub scope: &'a str,
    pub expires_at: i64,
    /// The user's track preferences on their own server, as JSON (P8-04).
    pub preferences: Option<&'a str>,
    /// A device's download token's one download, by this server's id
    /// (P8-07).
    pub download: Option<&'a str>,
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

/// A token redeemed (P8-04): what it opens.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct RedeemedToken {
    pub peer: String,
    pub principal: String,
    pub item: String,
    pub library: String,
    pub scope: String,
    /// The user's track preferences on their own server, as JSON (P8-04).
    pub preferences: Option<String>,
    /// The pair's endpoint, hex: a token it redeemed itself is a proxied
    /// play's.
    pub peer_endpoint: String,
    /// What the pair calls itself.
    pub peer_name: String,
    /// A device's download token's one download (P8-07).
    pub download: Option<String>,
}

/// The most tokens that work at once (H3): per pair and user, and per
/// pair. A play takes one token, for minutes; past these the pair is told
/// to try later. Download tokens (P8-07) are held to limits of their own,
/// so downloads that live for hours never take a play's place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenLimits {
    pub per_user: u64,
    pub per_peer: u64,
}

/// What else holds a token's place beside the unused tokens the store
/// keeps (P8-07): the download sessions open now, of the pair's user and
/// of the pair. None for a play token.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AlsoHeld {
    pub of_user: u64,
    pub of_peer: u64,
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
    /// Of the plays served, those the pair's server redeemed and played
    /// on to the device (P8-04). Of the plays asserted, those this server
    /// played on to its own device.
    pub proxied: u64,
}

impl PlayCount {
    /// Of the plays served, those the device redeemed itself (P8-04).
    pub fn direct(&self) -> u64 {
        self.plays.saturating_sub(self.proxied)
    }
}

impl Store {
    /// Keep `token`, minted now, if its library is shared with its pair
    /// and the pair is within `limits`, counting its scope's unused tokens
    /// and what is `also` held: checked and kept in one transaction, so a
    /// grant revoked meanwhile is never missed (M6).
    pub async fn keep_federation_token(
        &self,
        token: NewToken<'_>,
        limits: TokenLimits,
        also: AlsoHeld,
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
             WHERE peer = ?1 AND expires_at > ?3 AND used_at IS NULL AND scope = ?4",
        )
        .bind(token.peer)
        .bind(token.principal)
        .bind(self.now())
        .bind(token.scope)
        .fetch_one(&mut *tx)
        .await?;
        let past = |held: i64, also: u64, limit: u64| {
            u64::try_from(held).unwrap_or_default().saturating_add(also) >= limit
        };
        if past(of_user, also.of_user, limits.per_user)
            || past(of_peer, also.of_peer, limits.per_peer)
        {
            return Ok(TokenKept::TooMany);
        }
        sqlx::query(
            "INSERT INTO federation_tokens \
                 (token_hash, peer, principal, item, library, device, scope, created_at, \
                  expires_at, preferences, download) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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
        .bind(token.preferences)
        .bind(token.download)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(TokenKept::Kept)
    }

    /// Redeem the token whose hash is `token_hash` from `device` (hex), now
    /// (P8-04): what it opens, if it was minted for that endpoint, is
    /// unused and unexpired, and its pair and its library's grant are
    /// live. Used up by this, in one statement, so two redemptions at once
    /// open one session. A token tried from another endpoint stays unused.
    pub async fn redeem_federation_token(
        &self,
        token_hash: &str,
        device: &str,
    ) -> Result<Option<RedeemedToken>, StoreError> {
        Ok(sqlx::query_as(
            "UPDATE federation_tokens SET used_at = ?3 \
             WHERE token_hash = ?1 AND device = ?2 AND used_at IS NULL AND expires_at > ?3 \
               AND EXISTS (SELECT 1 FROM federation_grants g \
                           JOIN federation_peers p ON p.id = g.peer \
                           WHERE g.peer = federation_tokens.peer \
                             AND g.library = federation_tokens.library \
                             AND g.revoked_at IS NULL AND p.revoked_at IS NULL) \
             RETURNING peer, principal, item, library, scope, preferences, download, \
                 (SELECT endpoint_id FROM federation_peers WHERE id = federation_tokens.peer) \
                     AS peer_endpoint, \
                 (SELECT display_name FROM federation_peers WHERE id = federation_tokens.peer) \
                     AS peer_name",
        )
        .bind(token_hash)
        .bind(device)
        .bind(self.now())
        .fetch_optional(&self.pool)
        .await?)
    }

    /// The hashes of the tokens minted for `device` (hex) and kept, each
    /// with whether it works now: unused and unexpired (P8-04). What its
    /// relay token is checked against: one that matches a token used up
    /// or expired is the device's own, stale, not a guess.
    pub async fn federation_tokens_of(
        &self,
        device: &str,
    ) -> Result<Vec<(String, bool)>, StoreError> {
        Ok(sqlx::query_as(
            "SELECT token_hash, used_at IS NULL AND expires_at > ?2 FROM federation_tokens \
             WHERE device = ?1",
        )
        .bind(device)
        .bind(self.now())
        .fetch_all(&self.pool)
        .await?)
    }

    /// Count a play of this server's served across pair `peer`, `proxied`
    /// or played by the device itself (P8-04): at its token's redemption.
    pub async fn count_federation_served(
        &self,
        peer: &str,
        proxied: bool,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO federation_plays (peer, side, plays, proxied) \
             VALUES (?1, 'served', 1, ?2) \
             ON CONFLICT (peer, side) DO UPDATE SET \
                 plays = plays + 1, proxied = proxied + excluded.proxied",
        )
        .bind(peer)
        .bind(i64::from(proxied))
        .execute(&self.pool)
        .await?;
        self.count_federation_day(peer, PlaySide::Served, 1, i64::from(proxied))
            .await
    }

    /// Count a play of pair `peer`'s item that this server plays on to
    /// its own user's device (P8-04): one of the plays it asserted.
    pub async fn count_federation_proxied(&self, peer: &str) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO federation_plays (peer, side, proxied) VALUES (?1, 'asserted', 1) \
             ON CONFLICT (peer, side) DO UPDATE SET proxied = proxied + 1",
        )
        .bind(peer)
        .execute(&self.pool)
        .await?;
        self.count_federation_day(peer, PlaySide::Asserted, 0, 1)
            .await
    }

    /// Add `plays` and `proxied` to pair `peer`'s count on `side` today.
    async fn count_federation_day(
        &self,
        peer: &str,
        side: PlaySide,
        plays: i64,
        proxied: i64,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO federation_play_days (peer, side, day, plays, proxied) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT (peer, side, day) DO UPDATE SET \
                 plays = plays + excluded.plays, proxied = proxied + excluded.proxied",
        )
        .bind(peer)
        .bind(side.name())
        .bind(self.now().div_euclid(DAY))
        .bind(plays)
        .bind(proxied)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Pair `peer`'s counts on `side` for each day from `since` (a Unix
    /// day) on, as (day, plays, proxied): days with none are left out.
    pub async fn federation_play_days(
        &self,
        peer: &str,
        side: PlaySide,
        since: i64,
    ) -> Result<Vec<(i64, u64, u64)>, StoreError> {
        let rows: Vec<(i64, i64, i64)> = sqlx::query_as(
            "SELECT day, plays, proxied FROM federation_play_days \
             WHERE peer = ?1 AND side = ?2 AND day >= ?3 ORDER BY day",
        )
        .bind(peer)
        .bind(side.name())
        .bind(since)
        .fetch_all(&self.pool)
        .await?;
        let count = |n: i64| u64::try_from(n).unwrap_or_default();
        Ok(rows
            .into_iter()
            .map(|(day, plays, proxied)| (day, count(plays), count(proxied)))
            .collect())
    }

    /// Delete the days' counts before `before`, a Unix day.
    pub async fn prune_federation_play_days(&self, before: i64) -> Result<u64, StoreError> {
        Ok(
            sqlx::query("DELETE FROM federation_play_days WHERE day < ?1")
                .bind(before)
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
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
        self.count_federation_day(peer, side, 1, 0).await
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
        let row: Option<(i64, i64, i64)> = sqlx::query_as(
            "SELECT plays, refused, proxied FROM federation_plays \
             WHERE peer = ?1 AND side = ?2",
        )
        .bind(peer)
        .bind(side.name())
        .fetch_optional(&self.pool)
        .await?;
        let count = |n: i64| u64::try_from(n).unwrap_or_default();
        Ok(
            row.map_or_else(PlayCount::default, |(plays, refused, proxied)| PlayCount {
                plays: count(plays),
                refused: count(refused),
                proxied: count(proxied),
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
            store
                .keep_federation_token(token, ROOMY, AlsoHeld::default())
                .await
                .unwrap(),
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
            preferences: None,
            download: None,
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
        let kept = |token| store.keep_federation_token(token, limits, AlsoHeld::default());
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
        let fresh = store
            .keep_federation_token(revoked, ROOMY, AlsoHeld::default())
            .await
            .unwrap();
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
                    .keep_federation_token(
                        token(&hash, &peer, "plex:1", now + 300),
                        limits,
                        AlsoHeld::default(),
                    )
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

    /// Download tokens (P8-07) are counted apart from play tokens, with
    /// the download sessions held beside them: a user's downloads never
    /// take their plays' place, nor the other way.
    #[tokio::test]
    async fn download_tokens_are_counted_apart_with_their_sessions() {
        let (store, peer) = paired("fed-tokens-download").await;
        let now = store.now();
        let limits = TokenLimits {
            per_user: 2,
            per_peer: 100,
        };
        let download = |hash| NewToken {
            scope: "download",
            ..token(hash, &peer, "plex:1", now + 300)
        };
        for hash in ["p1", "p2"] {
            assert_eq!(
                store
                    .keep_federation_token(
                        token(hash, &peer, "plex:1", now + 300),
                        limits,
                        AlsoHeld::default()
                    )
                    .await
                    .unwrap(),
                TokenKept::Kept
            );
        }
        let one_session = AlsoHeld {
            of_user: 1,
            of_peer: 1,
        };
        assert_eq!(
            store
                .keep_federation_token(download("d1"), limits, one_session)
                .await
                .unwrap(),
            TokenKept::Kept,
            "two play tokens leave a download its place"
        );
        assert_eq!(
            store
                .keep_federation_token(download("d2"), limits, one_session)
                .await
                .unwrap(),
            TokenKept::TooMany,
            "one unused download token and one session"
        );
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

    /// A redeemed play is counted served, by who redeemed it (P8-04).
    /// Each count is also kept by the day, for the Federation page's
    /// week; days past it are pruned.
    #[tokio::test]
    async fn plays_are_counted_a_day_at_a_time_too() {
        let (store, peer) = paired("fed-play-days").await;
        let today = store.now().div_euclid(DAY);
        store.count_federation_served(&peer, true).await.unwrap();
        store.count_federation_served(&peer, false).await.unwrap();
        store
            .count_federation_play(&peer, PlaySide::Asserted)
            .await
            .unwrap();
        store.count_federation_proxied(&peer).await.unwrap();
        store
            .clock()
            .advance(std::time::Duration::from_secs(DAY as u64));
        store.count_federation_served(&peer, false).await.unwrap();
        assert_eq!(
            store
                .federation_play_days(&peer, PlaySide::Served, today)
                .await
                .unwrap(),
            [(today, 2, 1), (today + 1, 1, 0)]
        );
        assert_eq!(
            store
                .federation_play_days(&peer, PlaySide::Asserted, today)
                .await
                .unwrap(),
            [(today, 1, 1)]
        );
        assert_eq!(
            store.prune_federation_play_days(today + 1).await.unwrap(),
            2
        );
        assert_eq!(
            store
                .federation_play_days(&peer, PlaySide::Served, today)
                .await
                .unwrap(),
            [(today + 1, 1, 0)]
        );
    }

    #[tokio::test]
    async fn a_served_play_is_counted_by_its_mode() {
        let (store, peer) = paired("fed-plays-mode").await;
        store.count_federation_served(&peer, false).await.unwrap();
        for _ in 0..2 {
            store.count_federation_served(&peer, true).await.unwrap();
        }
        let served = store
            .federation_plays(&peer, PlaySide::Served)
            .await
            .unwrap();
        assert_eq!((served.plays, served.direct(), served.proxied), (3, 1, 2));
    }

    /// A token is redeemed once, from its device alone, before it expires
    /// (P8-04); one tried from another endpoint stays good for its own.
    #[tokio::test]
    async fn a_token_is_redeemed_once_from_its_device() {
        let (store, peer) = paired("fed-tokens-redeem").await;
        let now = store.now();
        keep(&store, token("h1", &peer, "plex:1", now + 300)).await;
        assert_eq!(
            store.redeem_federation_token("h1", "cd").await.unwrap(),
            None,
            "another device"
        );
        let redeemed = store
            .redeem_federation_token("h1", "ab")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (redeemed.item.as_str(), redeemed.library.as_str()),
            ("plex:movie:1", "plex:1")
        );
        assert_eq!(
            store.redeem_federation_token("h1", "ab").await.unwrap(),
            None,
            "used"
        );
        keep(&store, token("h2", &peer, "plex:1", now + 300)).await;
        store.clock().advance(std::time::Duration::from_secs(300));
        assert_eq!(
            store.redeem_federation_token("h2", "ab").await.unwrap(),
            None,
            "expired"
        );
    }

    /// A token whose grant was revoked, or whose pair unpaired, since it
    /// was minted opens nothing, even if it was kept (P8-04).
    #[tokio::test]
    async fn a_token_opens_nothing_once_its_grant_or_pair_is_gone() {
        let (store, peer) = paired("fed-tokens-redeem-revoked").await;
        let later = store.now() + 300;
        keep(&store, token("h1", &peer, "plex:1", later)).await;
        keep(&store, token("h2", &peer, "plex:2", later)).await;
        store.revoke_library_grant(&peer, "plex:1").await.unwrap();
        assert_eq!(
            store.redeem_federation_token("h1", "ab").await.unwrap(),
            None
        );
        assert!(
            store
                .redeem_federation_token("h2", "ab")
                .await
                .unwrap()
                .is_some()
        );
    }
}
