//! Delegated authentication (P8-03): this server vouching for one of its
//! users to a paired server whose item they want ([`Federation::assert_play`]),
//! and minting a token when a paired server vouches for one of its own
//! ([`Federation::mint`]).
//!
//! An assertion is a message on the pair's connection, which proves who
//! sent it; there is no signature. Only local accounts are vouched for, by
//! their stable id: a portable guest of this server's is not passed on.
//! The server whose item it is checks that the item is in a library
//! shared with the pair, and mints a random bearer, kept hashed, bound to
//! the device and to that one item, for minutes. P8-04 redeems it.
//!
//! Plays are counted one number per server, never per user: the server
//! whose item is played counts as it mints, the other as it vouches.
//! Refused assertions are counted on both sides.

use std::time::Duration;

use iroh::EndpointId;
use spoolframe_catalog::CatalogError;
use spoolframe_federation::DialError;
use spoolframe_protocol::ServerId;
use spoolframe_protocol::federation::{AssertUser, AssertionRefused, TokenGranted, TokenScope};
use spoolframe_types::{MediaId, MediaKind};

use super::{Federation, ShareError};
use crate::auth::SessionToken;
use crate::principal::{Principal, RemoteServerId, Who};
use crate::store::StoreError;
use crate::store::federation_tokens::{NewToken, PlaySide, TokenKept, TokenLimits};
use crate::store::users::LocalUserId;

/// How long a play token works once minted.
pub const PLAY_TOKEN_LIFETIME: Duration = Duration::from_secs(5 * 60);

/// The most play tokens that work at once (H3): a user starts a few plays
/// in five minutes, and a pair's assertions are paced at one a second.
pub const TOKEN_LIMITS: TokenLimits = TokenLimits {
    per_user: 10,
    per_peer: 500,
};

/// Why this server got no token for its user.
#[derive(Debug, thiserror::Error)]
pub enum AssertError {
    /// The item is not a paired server's.
    #[error("{0} is not a paired server's item")]
    NotFederated(MediaId),
    /// No live pair has the item's origin.
    #[error("no paired server {0:?}")]
    NotPaired(String),
    /// Paired, but not connected now.
    #[error("the paired server is offline")]
    Offline,
    /// Only local accounts are vouched for: a portable guest is not
    /// passed on.
    #[error("only an account of this server's is vouched for")]
    NotALocalUser,
    /// The paired server refused, or did not answer.
    #[error(transparent)]
    Dial(#[from] DialError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl Federation {
    /// Vouch for `user`, who wants `item` (a paired server's) on the
    /// device `device`, to the paired server it is from: its token, which
    /// the device presents there (P8-04). Counted as a play of this
    /// server's users on that pair, or as a refusal.
    pub async fn assert_play(
        &self,
        user: LocalUserId,
        display_name: &str,
        item: &MediaId,
        device: EndpointId,
    ) -> Result<TokenGranted, AssertError> {
        let (origin, remote) = item
            .remote()
            .ok_or_else(|| AssertError::NotFederated(item.clone()))?;
        let peer = self
            .live_peer_by_server_id(origin)
            .await?
            .ok_or_else(|| AssertError::NotPaired(origin.to_owned()))?;
        let dialed = self.links.get(origin).ok_or(AssertError::Offline)?;
        let granted = dialed
            .assert_user(AssertUser {
                user: user.to_string(),
                display_name: display_name.to_owned(),
                client_endpoint: device.to_string(),
                item: remote.to_string(),
                scope: TokenScope::Play,
            })
            .await;
        match granted {
            Ok(granted) => {
                self.store()
                    .count_federation_play(&peer.id, PlaySide::Asserted)
                    .await?;
                tracing::info!(
                    peer = %peer.endpoint_id,
                    %item,
                    user = %user,
                    "a paired server minted a play token for a user here"
                );
                Ok(granted)
            }
            Err(e) => {
                if matches!(e, DialError::NotGranted(_)) {
                    self.store()
                        .count_federation_refusal(&peer.id, PlaySide::Asserted)
                        .await?;
                }
                tracing::info!(
                    peer = %peer.endpoint_id,
                    %item,
                    error = %e,
                    "a paired server minted no play token"
                );
                Err(e.into())
            }
        }
    }

    /// [`Federation::assert_play`] for `who`: a local account only. A
    /// portable guest is not passed on, and is counted as refused.
    pub async fn assert_play_for(
        &self,
        who: &Who,
        item: &MediaId,
        device: EndpointId,
    ) -> Result<TokenGranted, AssertError> {
        let Some(local) = who.local() else {
            if let Some((origin, _)) = item.remote()
                && let Some(peer) = self.live_peer_by_server_id(origin).await?
            {
                self.store()
                    .count_federation_refusal(&peer.id, PlaySide::Asserted)
                    .await?;
            }
            return Err(AssertError::NotALocalUser);
        };
        self.assert_play(local.user, &local.display_name, item, device)
            .await
    }

    /// Pair `server_id` vouches for `assertion`'s user (P8-03): a token
    /// for its item, minted and kept hashed, or why not. Counted as a play
    /// of the pair's users here, or as a refusal.
    pub(crate) async fn mint(
        &self,
        server_id: &ServerId,
        assertion: &AssertUser,
    ) -> Result<TokenGranted, AssertionRefused> {
        let store = self.store();
        let unavailable = |e: &dyn std::fmt::Display| {
            tracing::warn!(error = %e, "cannot answer a paired server's assertion");
            AssertionRefused::Unavailable
        };
        let peer = store
            .live_peer_by_server_id(server_id.as_str())
            .await
            .map_err(|e| unavailable(&e))?
            .ok_or(AssertionRefused::NotShared)?;
        let checked = self.checked(&peer.id, assertion).await;
        let (user, device, item, library) = match checked {
            Ok(checked) => checked,
            Err(reason) => {
                if reason != AssertionRefused::Unavailable
                    && let Err(e) = store
                        .count_federation_refusal(&peer.id, PlaySide::Served)
                        .await
                {
                    tracing::warn!(error = %e, "cannot count a refused assertion");
                }
                tracing::info!(
                    peer = %peer.endpoint_id,
                    server_id = %server_id,
                    item = assertion.item,
                    %reason,
                    "a paired server's assertion is refused"
                );
                return Err(reason);
            }
        };
        let principal = Principal::RemoteUser {
            server: RemoteServerId::from(server_id),
            user,
        };
        // A bearer as a session's is, kept by its hash alike.
        let token = SessionToken::generate();
        let expires_at =
            store.now() + i64::try_from(PLAY_TOKEN_LIFETIME.as_secs()).unwrap_or(i64::MAX);
        let kept = store
            .keep_federation_token(
                NewToken {
                    token_hash: &token.hash(),
                    peer: &peer.id,
                    principal: &principal.to_string(),
                    item: &item.to_string(),
                    library: &library,
                    // As the redeeming connection names its endpoint: one
                    // key, whichever encoding the assertion used.
                    device: &device.to_string(),
                    scope: assertion.scope.name(),
                    expires_at,
                },
                TOKEN_LIMITS,
            )
            .await
            .map_err(|e| unavailable(&e))?;
        match kept {
            TokenKept::Kept => {}
            // Revoked since the item was placed.
            TokenKept::NotShared => return Err(AssertionRefused::NotShared),
            TokenKept::TooMany => {
                tracing::info!(
                    peer = %peer.endpoint_id,
                    %principal,
                    "a paired server holds as many play tokens as it may"
                );
                return Err(AssertionRefused::Unavailable);
            }
        }
        if let Err(e) = store
            .count_federation_play(&peer.id, PlaySide::Served)
            .await
        {
            tracing::warn!(error = %e, "cannot count a paired server's play");
        }
        tracing::info!(
            peer = %peer.endpoint_id,
            %principal,
            %item,
            "a play token is minted for a paired server's user"
        );
        Ok(TokenGranted {
            token: token.0,
            scope: assertion.scope,
            expires_at,
        })
    }

    /// `assertion` read and checked for pair `peer`: its user, its
    /// device, its item, and the shared library the item is in.
    async fn checked(
        &self,
        peer: &str,
        assertion: &AssertUser,
    ) -> Result<(uuid::Uuid, EndpointId, MediaId, String), AssertionRefused> {
        // A stable user id, as a local account's: a portable user's id, or
        // anything else, is not one.
        let user =
            uuid::Uuid::try_parse(&assertion.user).map_err(|_| AssertionRefused::InvalidUser)?;
        let device = assertion
            .client_endpoint
            .parse::<EndpointId>()
            .map_err(|_| AssertionRefused::InvalidDevice)?;
        if assertion.scope != TokenScope::Play {
            return Err(AssertionRefused::InvalidScope);
        }
        let item: MediaId = assertion
            .item
            .parse()
            .map_err(|_| AssertionRefused::NotShared)?;
        // Only what plays: never a show, a season or an album (M6).
        if !matches!(
            item.kind(),
            MediaKind::Movie | MediaKind::Episode | MediaKind::Track
        ) {
            return Err(AssertionRefused::InvalidScope);
        }
        let library = match self.shared_library_of(peer, &item).await {
            Ok(Some(library)) => library,
            // Not there, or in no library served: not shared.
            Ok(None)
            | Err(ShareError::Catalog(
                CatalogError::NotFound(_) | CatalogError::NotServed(_) | CatalogError::ForeignId(_),
            )) => return Err(AssertionRefused::NotShared),
            Err(e) => {
                tracing::warn!(%item, error = %e, "cannot place an item a paired server asked for");
                return Err(AssertionRefused::Unavailable);
            }
        };
        Ok((user, device, item, library.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::auth::Auth;
    use crate::network::ServerName;
    use crate::store::federation::NewPeer;
    use crate::store::tests::test_store;
    use crate::test_support::{FakeCatalog, movie};

    /// RFC 4648 base32, no padding: what the key parser takes besides hex.
    fn base32(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
        let mut out = String::new();
        let (mut buffer, mut bits) = (0u32, 0);
        for &byte in bytes {
            buffer = (buffer << 8) | u32::from(byte);
            bits += 8;
            while bits >= 5 {
                bits -= 5;
                out.push(ALPHABET[((buffer >> bits) & 31) as usize] as char);
            }
        }
        if bits > 0 {
            out.push(ALPHABET[((buffer << (5 - bits)) & 31) as usize] as char);
        }
        out
    }

    /// A device named in another encoding the key parser takes (base32)
    /// is kept as its hex, which is what the redeeming connection says:
    /// the token works from that device.
    #[tokio::test]
    async fn a_token_s_device_is_kept_as_its_canonical_hex() {
        let store = test_store("fed-token-device").await;
        let federation = Federation::new(
            Auth::new(store.store.clone()),
            Arc::new(FakeCatalog::new("/nowhere/movie.mkv".into())),
            "aaaaaaaaaaaaaaaa".parse().unwrap(),
            ServerName::new("Alice's server"),
            super::super::ReplicaSettings::in_dir("/nowhere/federation".into()),
        );
        let bob_s: ServerId = "mzxw6ytbojtg633c".parse().unwrap();
        let peer = store
            .add_peer(
                NewPeer {
                    server_id: bob_s.as_str(),
                    endpoint_id: &"0b".repeat(32),
                    display_name: "Bob's server",
                    addrs: &[],
                    relays: &[],
                },
                "alice",
            )
            .await
            .unwrap();
        federation
            .share(&peer.id, &["plex:1".parse().unwrap()], "alice")
            .await
            .unwrap();
        let device = iroh::SecretKey::from_bytes(&[7; 32]).public();
        let base32 = base32(device.as_bytes());
        let granted = federation
            .mint(
                &bob_s,
                &AssertUser {
                    user: uuid::Uuid::from_u128(7).to_string(),
                    display_name: "Bob".into(),
                    client_endpoint: base32,
                    item: movie().id().to_string(),
                    scope: TokenScope::Play,
                },
            )
            .await
            .unwrap();
        let kept = store
            .federation_token(&SessionToken(granted.token).hash())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(kept.device, device.to_string());
    }
}
