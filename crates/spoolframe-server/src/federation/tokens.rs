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
//! A download token (P8-07) is asserted and minted the same way, with
//! limits of its own: the session it opens lives while its download is
//! made and read.
//!
//! Plays are counted one number per server, never per user: the server
//! whose item is played counts as the token is redeemed (P8-04), the
//! other as it vouches.
//! Refused assertions are counted on both sides.

use std::time::Duration;

use iroh::EndpointId;
use spoolframe_federation::DialError;
use spoolframe_protocol::ServerId;
use spoolframe_protocol::federation::{AssertUser, AssertionRefused, TokenGranted, TokenScope};
use spoolframe_types::MediaId;

use super::Federation;
use crate::auth::SessionToken;
use crate::principal::{Principal, RemoteServerId, Who};
use crate::store::StoreError;
use crate::store::federation::PeerRecord;
use crate::store::federation_tokens::{AlsoHeld, NewToken, PlaySide, TokenKept};
use crate::store::users::LocalUserId;

/// How long a play token works once minted.
pub const PLAY_TOKEN_LIFETIME: Duration = Duration::from_secs(5 * 60);

/// The tokens this server had granted to devices, by user, item (its
/// `fed:` id) and device, and when ([`Federation::granted_to_device`]).
pub(super) type DeviceGrants = parking_lot::Mutex<
    std::collections::HashMap<(LocalUserId, String, EndpointId), std::time::Instant>,
>;

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
        item: &MediaId,
        device: EndpointId,
    ) -> Result<TokenGranted, AssertError> {
        let (peer, _) = self.remote_peer(item).await?;
        let wanted = Wanted::play(true);
        self.assert_to(&peer, user, item, device, wanted).await
    }

    /// [`Federation::assert_play`] to `peer`, the live pair `item` is from,
    /// as the caller found it, for the token `wanted` says.
    async fn assert_to(
        &self,
        peer: &PeerRecord,
        user: LocalUserId,
        item: &MediaId,
        device: EndpointId,
        wanted: Wanted,
    ) -> Result<TokenGranted, AssertError> {
        let Wanted {
            scope,
            download,
            counted,
        } = wanted;
        let (_, remote) = item
            .remote()
            .ok_or_else(|| AssertError::NotFederated(item.clone()))?;
        let dialed = self.links.pinned(peer).ok_or(AssertError::Offline)?;
        // SA chooses the tracks by them, as this server would: it has none
        // of the user's own.
        let preferences = self
            .store()
            .track_preferences(Principal::LocalUser(user))
            .await?;
        let granted = dialed
            .assert_user(AssertUser {
                user: user.to_string(),
                // Never the user's name: SA hears who they are by their
                // stable id alone (F8-08). Empty, as the wire still has
                // the field, until the protocol's next revision drops it.
                display_name: String::new(),
                client_endpoint: device.to_string(),
                item: remote.to_string(),
                scope,
                preferences: Some(preferences),
                download,
            })
            .await;
        match granted {
            Ok(granted) => {
                if counted && scope == TokenScope::Play {
                    self.store()
                        .count_federation_play(&peer.id, PlaySide::Asserted)
                        .await?;
                }
                tracing::info!(
                    peer = %peer.endpoint_id,
                    %item,
                    user = %user,
                    scope = scope.name(),
                    "a paired server minted a token for a user here"
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
                    scope = scope.name(),
                    "a paired server minted no token"
                );
                Err(e.into())
            }
        }
    }

    /// [`Federation::assert_play`] for `who` to `peer`, the live pair
    /// `item` is from: a local account only. A portable guest is not passed
    /// on, and is counted as refused. Counted as a play unless `counted`
    /// is false.
    pub async fn assert_play_for(
        &self,
        who: &Who,
        peer: &PeerRecord,
        item: &MediaId,
        device: EndpointId,
        counted: bool,
    ) -> Result<TokenGranted, AssertError> {
        self.assert_for(who, peer, item, device, Wanted::play(counted))
            .await
    }

    /// A download token (P8-07) for `who` to `peer`, the live pair `item` is
    /// from, bound to `device`: of `download` alone, SA's id of it, for a
    /// device's own reads, or with `None` for this server, which prepares
    /// and keeps the download. Not a play, and not counted as one.
    pub async fn assert_download_for(
        &self,
        who: &Who,
        peer: &PeerRecord,
        item: &MediaId,
        device: EndpointId,
        download: Option<String>,
    ) -> Result<TokenGranted, AssertError> {
        let wanted = Wanted {
            scope: TokenScope::Download,
            download,
            counted: false,
        };
        self.assert_for(who, peer, item, device, wanted).await
    }

    /// [`Federation::assert_to`] for `who`: a local account only. A
    /// portable guest is not passed on, and is counted as refused.
    async fn assert_for(
        &self,
        who: &Who,
        peer: &PeerRecord,
        item: &MediaId,
        device: EndpointId,
        wanted: Wanted,
    ) -> Result<TokenGranted, AssertError> {
        let Some(local) = who.local() else {
            self.store()
                .count_federation_refusal(&peer.id, PlaySide::Asserted)
                .await?;
            return Err(AssertError::NotALocalUser);
        };
        self.assert_to(peer, local.user, item, device, wanted).await
    }

    /// `who` was granted a token for `item` on `device` (P8-04): a proxied
    /// start of it from that device within the token's lifetime is the
    /// same play, its dial failed, and is not counted again.
    pub fn granted_to_device(&self, who: &Who, item: &MediaId, device: EndpointId) {
        let Some(local) = who.local() else {
            return;
        };
        let now = std::time::Instant::now();
        let mut granted = self.device_grants.lock();
        granted.retain(|_, at| now.duration_since(*at) < PLAY_TOKEN_LIFETIME);
        granted.insert((local.user, item.to_string(), device), now);
    }

    /// Whether `who` was granted a token for `item` on `device` within its
    /// lifetime: taken, so it stands for one play only.
    pub(crate) fn took_device_grant(&self, who: &Who, item: &MediaId, device: EndpointId) -> bool {
        let Some(local) = who.local() else {
            return false;
        };
        self.device_grants
            .lock()
            .remove(&(local.user, item.to_string(), device))
            .is_some_and(|at| at.elapsed() < PLAY_TOKEN_LIFETIME)
    }

    /// Pair `server_id` vouches for `assertion`'s user (P8-03): a token
    /// for its item, minted and kept hashed, or why not. A refusal is
    /// counted here; the play, once the token is redeemed (P8-04).
    pub(crate) async fn mint(
        &self,
        server_id: &ServerId,
        endpoint: EndpointId,
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
        let (user, device, item, library, scope) = match checked {
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
        let preferences = assertion
            .preferences
            .as_ref()
            .and_then(|p| serde_json::to_string(p).ok());
        // Downloads are held to limits of their own, counting the
        // download sessions open: they live for hours, and must never take
        // a play's place (P8-07). Counted as the token is kept, those
        // being redeemed among them (F8-05).
        let sessions = self.auth.remote_sessions();
        let downloads = |u: &crate::remote_sessions::RemoteSessionUser| {
            u.scope == TokenScope::Download && u.peer == peer.id
        };
        let also = || match scope {
            TokenScope::Download => {
                let (of_user, of_peer) = sessions.count_both(|u| {
                    let of_peer = downloads(u);
                    (of_peer && u.principal() == principal, of_peer)
                });
                AlsoHeld { of_user, of_peer }
            }
            _ => AlsoHeld::default(),
        };
        let limits = match scope {
            TokenScope::Download => self.limits().download_tokens,
            _ => self.limits().tokens,
        };
        #[cfg(test)]
        self.holds.minting.here().await;
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
                    scope: scope.name(),
                    expires_at,
                    // Kept for the session it opens (P8-04).
                    preferences: preferences.as_deref(),
                    download: assertion.download.as_deref(),
                    // Kept only while it is the pair's key: an assertion
                    // on the old key's connection as a new one is
                    // confirmed mints nothing.
                    asserted_by: &endpoint.to_string(),
                },
                // The most that work at once (H3), as the settings say.
                limits,
                also,
            )
            .await
            .map_err(|e| unavailable(&e))?;
        match kept {
            TokenKept::Kept => {}
            // Revoked since the item was placed, or the pair moved from
            // the key that asserted.
            TokenKept::NotShared => return Err(AssertionRefused::NotShared),
            TokenKept::TooMany => {
                tracing::info!(
                    peer = %peer.endpoint_id,
                    %principal,
                    scope = scope.name(),
                    "a paired server holds as many tokens as it may"
                );
                return Err(AssertionRefused::Unavailable);
            }
        }
        // The play is counted as the token is redeemed, by who redeems it
        // (P8-04): one never redeemed played nothing.
        tracing::info!(
            peer = %peer.endpoint_id,
            %principal,
            %item,
            scope = scope.name(),
            "a token is minted for a paired server's user"
        );
        Ok(TokenGranted {
            token: token.0,
            scope,
            expires_at,
        })
    }

    /// `assertion` read and checked for pair `peer`: its user, its
    /// device, its item, the shared library the item is in, and its scope.
    async fn checked(
        &self,
        peer: &str,
        assertion: &AssertUser,
    ) -> Result<(uuid::Uuid, EndpointId, MediaId, String, TokenScope), AssertionRefused> {
        // A stable user id, as a local account's: a portable user's id, or
        // anything else, is not one.
        let user =
            uuid::Uuid::try_parse(&assertion.user).map_err(|_| AssertionRefused::InvalidUser)?;
        let device = assertion
            .client_endpoint
            .parse::<EndpointId>()
            .map_err(|_| AssertionRefused::InvalidDevice)?;
        // Sharing a library includes downloading from it (P8-07): no
        // switch of its own.
        if !matches!(assertion.scope, TokenScope::Play | TokenScope::Download)
            || (assertion.scope == TokenScope::Play && assertion.download.is_some())
        {
            return Err(AssertionRefused::InvalidScope);
        }
        let item: MediaId = assertion
            .item
            .parse()
            .map_err(|_| AssertionRefused::NotShared)?;
        // Only what plays: never a show, a season or an album (M6).
        if !item.kind().plays() {
            return Err(AssertionRefused::InvalidScope);
        }
        let library = match self.placed_shared(peer, &item).await {
            Ok(Some(library)) => library,
            Ok(None) => return Err(AssertionRefused::NotShared),
            Err(e) => {
                tracing::warn!(%item, error = %e, "cannot place an item a paired server asked for");
                return Err(AssertionRefused::Unavailable);
            }
        };
        Ok((user, device, item, library.to_string(), assertion.scope))
    }
}

/// The token an assertion asks for.
struct Wanted {
    scope: TokenScope,
    /// A device's download token's one download, by SA's id.
    download: Option<String>,
    /// Counted as a play here (a play not counted already, P8-04).
    counted: bool,
}

impl Wanted {
    fn play(counted: bool) -> Self {
        Self {
            scope: TokenScope::Play,
            download: None,
            counted,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
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

    /// Alice's server, sharing its movies with Bob's: the store, the
    /// federation, and Bob's server's id.
    pub(crate) async fn sharing(
        test: &str,
    ) -> (crate::store::tests::TestStore, Federation, ServerId) {
        sharing_from(
            test,
            Arc::new(FakeCatalog::new("/nowhere/movie.mkv".into())),
        )
        .await
    }

    /// [`sharing`], from `catalog`.
    pub(crate) async fn sharing_from(
        test: &str,
        catalog: Arc<FakeCatalog>,
    ) -> (crate::store::tests::TestStore, Federation, ServerId) {
        let store = test_store(test).await;
        let federation = Federation::new(
            Auth::new(store.store.clone()),
            catalog,
            "aaaaaaaaaaaaaaaa".parse().unwrap(),
            ServerName::new("Alice's server"),
            super::super::ReplicaSettings::in_dir("/nowhere/federation".into()),
        );
        let bob_s: ServerId = "mzxw6ytbojtg633c".parse().unwrap();
        let peer = store
            .add_peer(
                NewPeer {
                    server_id: bob_s.as_str(),
                    endpoint_id: &bob_s_key().to_string(),
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
        (store, federation, bob_s)
    }

    /// Bob's server's key: the pair's endpoint, which redeems a proxied
    /// play's token.
    pub(crate) fn bob_s_key() -> EndpointId {
        iroh::SecretKey::from_bytes(&[11; 32]).public()
    }

    /// A token for Bob's play of the movie on `client_endpoint`.
    pub(crate) async fn minted(
        federation: &Federation,
        bob_s: &ServerId,
        client_endpoint: String,
    ) -> String {
        federation
            .mint(
                bob_s,
                bob_s_key(),
                &bob_wants(client_endpoint, TokenScope::Play, 7),
            )
            .await
            .unwrap()
            .token
    }

    /// User `user` of Bob's server wants the movie on `client_endpoint`,
    /// for `scope`.
    pub(crate) fn bob_wants(client_endpoint: String, scope: TokenScope, user: u128) -> AssertUser {
        AssertUser {
            user: uuid::Uuid::from_u128(user).to_string(),
            display_name: "Bob".into(),
            client_endpoint,
            item: movie().id().to_string(),
            scope,
            preferences: None,
            download: None,
        }
    }

    /// A device named in another encoding the key parser takes (base32)
    /// is kept as its hex, which is what the redeeming connection says:
    /// the token works from that device.
    #[tokio::test]
    async fn a_token_s_device_is_kept_as_its_canonical_hex() {
        let (store, federation, bob_s) = sharing("fed-token-device").await;
        let device = iroh::SecretKey::from_bytes(&[7; 32]).public();
        let token = minted(&federation, &bob_s, base32(device.as_bytes())).await;
        let kept = store
            .federation_token(&SessionToken(token).hash())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(kept.device, device.to_string());
    }

    /// A device that played comes back to the relay with its used token's
    /// relay token (P8-04): not let through by it, and not counted as a
    /// guess either, past as many times as a guesser is allowed; its next
    /// token still lets it through.
    #[tokio::test]
    async fn a_device_s_own_used_relay_token_is_not_a_guess() {
        use spoolframe_protocol::relay_access::play_relay_token;
        let (_store, federation, bob_s) = sharing("fed-token-stale").await;
        let device = iroh::SecretKey::from_bytes(&[9; 32]).public();
        let token = minted(&federation, &bob_s, device.to_string()).await;
        federation
            .redeem_play_token(&token, &device.to_string())
            .await
            .unwrap();
        // Its play over, its session gone.
        federation.auth.remote_sessions().end_where(|_| true);
        let stale = play_relay_token(&token, device.as_bytes());
        for _ in 0..=crate::auth::MAX_FAILURES {
            assert!(!federation.holds_play_token(&device, &stale).await);
        }
        let next = minted(&federation, &bob_s, device.to_string()).await;
        assert!(
            federation
                .holds_play_token(&device, &play_relay_token(&next, device.as_bytes()))
                .await
        );
    }

    /// F8-08: SB tells SA who its user is by their stable id, never their
    /// name: SA needs none (its pages name the pair), and a paired server
    /// is not trusted with SB's users. The assertion SA hears says the
    /// user's UUID and an empty `display_name`, however they are called.
    #[tokio::test]
    async fn an_assertion_says_the_user_s_id_and_not_their_name() {
        use iroh::endpoint::presets;
        use spoolframe_federation::Dialed;
        use spoolframe_protocol::FEDERATION_ALPN;
        use spoolframe_protocol::codec::{read_frame_within, write_frame};
        use spoolframe_protocol::federation::{
            MAX_REQUEST, PROTOCOL_VERSION, PeerHello, PeerInfo, PeerReply, PeerRequest, Standing,
        };
        let endpoint = |alpns| {
            iroh::Endpoint::builder(presets::Minimal)
                .relay_mode(iroh::RelayMode::Disabled)
                .alpns(alpns)
                .bind()
        };
        // SA, as far as hearing an assertion: it refuses every one.
        let sa = endpoint(vec![FEDERATION_ALPN.to_vec()]).await.unwrap();
        let (heard, mut assertions) = tokio::sync::mpsc::unbounded_channel();
        let answering = sa.clone();
        tokio::spawn(async move {
            let connection = answering.accept().await.unwrap().await.unwrap();
            while let Ok((mut send, mut recv)) = connection.accept_bi().await {
                let reply = match read_frame_within(&mut recv, MAX_REQUEST).await.unwrap() {
                    PeerRequest::Hello(_) => PeerReply::Info(PeerInfo {
                        protocol_version: PROTOCOL_VERSION,
                        server_id: "aaaaaaaaaaaaaaaa".into(),
                        name: "Alice's server".into(),
                        standing: Standing::Paired,
                    }),
                    PeerRequest::AssertUser(assertion) => {
                        let _ = heard.send(assertion);
                        PeerReply::AssertionRefused {
                            reason: AssertionRefused::NotShared,
                        }
                    }
                    other => panic!("not asked of SA here: {other:?}"),
                };
                write_frame(&mut send, &reply).await.unwrap();
                send.finish().unwrap();
            }
        });
        // SB, paired with SA and connected to it.
        let store = test_store("fed-token-no-name").await;
        let federation = Federation::new(
            Auth::new(store.store.clone()),
            Arc::new(FakeCatalog::new("/nowhere/movie.mkv".into())),
            "mzxw6ytbojtg633c".parse().unwrap(),
            ServerName::new("Bob's server"),
            super::super::ReplicaSettings::in_dir("/nowhere/federation".into()),
        );
        store
            .add_peer(
                NewPeer {
                    server_id: "aaaaaaaaaaaaaaaa",
                    endpoint_id: &sa.id().to_string(),
                    display_name: "Alice's server",
                    addrs: &[],
                    relays: &[],
                },
                "bob",
            )
            .await
            .unwrap();
        let sb = endpoint(Vec::new()).await.unwrap();
        let hello = PeerHello {
            protocol_version: PROTOCOL_VERSION,
            server_id: "mzxw6ytbojtg633c".into(),
            name: "Bob's server".into(),
        };
        let (dialed, _) = Dialed::connect(&sb, sa.addr(), hello).await.unwrap();
        federation.links.set("aaaaaaaaaaaaaaaa", Arc::new(dialed));
        let bob = store
            .add_user(
                "bob",
                Some("Robert Distinctive"),
                "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA",
                spoolframe_types::Role::Consumer,
            )
            .await
            .unwrap();
        let alien = MediaId::federated("aaaaaaaaaaaaaaaa", movie().id()).unwrap();
        let device = iroh::SecretKey::from_bytes(&[5; 32]).public();
        let refused = federation.assert_play(bob, &alien, device).await;
        assert!(refused.is_err(), "SA refuses every assertion");
        let heard = assertions.recv().await.unwrap();
        assert_eq!(heard.user, bob.to_string());
        assert_eq!(heard.display_name, "");
    }
}
