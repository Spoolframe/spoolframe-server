//! Redeeming a play token (P8-04): the server whose item it is (SA) opens
//! a session for one of a paired server's users, for that one item.
//!
//! The security review's checks, each at redemption: the token is used up
//! in the statement that finds it, so it works once; only from the
//! endpoint it was minted for; only while its pair and its library's grant
//! are live; and only for what plays. The mode is who redeemed it: the
//! device itself (direct), or the pair's own endpoint, for which SB asked
//! the token when it plays on to the device (proxy). The play served is
//! counted then, by mode, not at mint: a token never redeemed played
//! nothing.
//!
//! A revoked grant or an unpairing ends the sessions it opened, and their
//! playbacks with them ([`Federation::end_remote_sessions`]), and a pair's
//! new key confirmed ends those its old key opened. The session is
//! registered before its token is used up
//! ([`crate::remote_sessions::RemoteSessions::register`]), so one that
//! commits just after finds it to end too.
//!
//! A download token (P8-07) is redeemed the same way, and opens a session
//! for its item's download alone; it is not a play, and is not counted as
//! one.

use iroh::EndpointId;
use spoolframe_protocol::federation::TokenScope;
use spoolframe_types::{MediaId, MediaKind};

use super::Federation;
use crate::auth::SessionToken;
use crate::principal::Principal;
use crate::remote_sessions::{MAX_PLAY_SESSIONS, PlayRoom, RemoteSessionUser};
use crate::store::StoreError;

/// Why a play token opened nothing.
#[derive(Debug, thiserror::Error)]
pub enum PlayTokenError {
    /// Unknown, used, expired, minted for another endpoint, its grant or
    /// pair gone, or not a play: told alike.
    #[error("that play token opens nothing")]
    Invalid,
    /// The catalog cannot say where the token's item is now: the token
    /// is not used up, and works when it can.
    #[error("the token's item cannot be looked up now")]
    Unavailable,
    /// The pair, or this server, holds as many play sessions as it may
    /// (F8-06): the token is not used up, and works once one ends.
    #[error("as many paired servers' plays are open as may be")]
    Busy,
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl Federation {
    /// Redeem play token `token` from `endpoint` (hex): the session it
    /// opens, and whose it is.
    pub async fn redeem_play_token(
        &self,
        token: &str,
        endpoint: &str,
    ) -> Result<(SessionToken, RemoteSessionUser), PlayTokenError> {
        let store = self.store();
        let hash = SessionToken(token.to_owned()).hash();
        // Read before it is used up, for the session registered first; the
        // statement that uses it up checks all of it again.
        let kept = store
            .federation_token(&hash)
            .await?
            .filter(|t| t.device == endpoint && t.used_at.is_none())
            .ok_or(PlayTokenError::Invalid)?;
        let Ok(Principal::RemoteUser { server, user }) = kept.principal.parse() else {
            return Err(PlayTokenError::Invalid);
        };
        let item: MediaId = kept.item.parse().map_err(|_| PlayTokenError::Invalid)?;
        // Mint keeps only plays and downloads of what plays; read back, it
        // is held to that again.
        let scope = [TokenScope::Play, TokenScope::Download]
            .into_iter()
            .find(|s| s.name() == kept.scope)
            .ok_or(PlayTokenError::Invalid)?;
        if !matches!(
            item.kind(),
            MediaKind::Movie | MediaKind::Episode | MediaKind::Track
        ) {
            return Err(PlayTokenError::Invalid);
        }
        let mut session_user = RemoteSessionUser {
            server,
            user,
            display_name: String::new(),
            peer: kept.peer,
            library: kept.library,
            item: item.clone(),
            scope,
            proxied: false,
            preferences: None,
            download: None,
        };
        // Registered before the token is used up, and opened only if
        // nothing ended it since (F8-02, F8-03, F8-05): a grant revoked, an
        // unpairing or a key confirmed that commits after the token is
        // used up ends it with the pair's open sessions, and a download
        // token's mint counts it from before the token stops counting.
        let room = PlayRoom {
            per_peer: self.limits().play_sessions,
            total: MAX_PLAY_SESSIONS,
        };
        let pending = self
            .auth
            .remote_sessions()
            .register(session_user.clone(), endpoint, room)
            .ok_or(PlayTokenError::Busy)?;
        // Where the item is now, not where it was at mint (F8-04): moved
        // out of its library, it opens nothing. Asked once registered, so
        // a move after is the catalog change that ends the session.
        match self
            .shared_in(&session_user.peer, &item, &session_user.library)
            .await
        {
            Some(true) => {}
            Some(false) => return Err(PlayTokenError::Invalid),
            None => return Err(PlayTokenError::Unavailable),
        }
        let redeemed = store
            .redeem_federation_token(&hash, endpoint)
            .await?
            .ok_or(PlayTokenError::Invalid)?;
        #[cfg(test)]
        self.hold_redeeming.here().await;
        // The redemption checked its pair is live, and read what it is.
        let proxied = redeemed.peer_endpoint == endpoint;
        // SA's admins manage servers, not people (P8-03): the Playback
        // page names the paired server, never its user.
        session_user.display_name = redeemed.peer_name;
        session_user.proxied = proxied;
        session_user.preferences = redeemed
            .preferences
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok());
        session_user.download = redeemed.download;
        let session = pending
            .open(session_user.clone())
            .ok_or(PlayTokenError::Invalid)?;
        // Counted off the way of the session's opening: a count missed is
        // logged, and the play goes on. A download is not a play.
        if scope == TokenScope::Play {
            let counting = store.clone();
            let peer = session_user.peer.clone();
            tokio::spawn(async move {
                if let Err(e) = counting.count_federation_served(&peer, proxied).await {
                    tracing::warn!(error = %e, "cannot count a paired server's play");
                }
            });
        }
        tracing::info!(
            peer = %redeemed.peer_endpoint,
            principal = %session_user.principal(),
            %item,
            proxied,
            scope = scope.name(),
            "a paired server's token is redeemed"
        );
        Ok((session, session_user))
    }

    /// Whether `token`, the relay token `endpoint` connected to this
    /// server's relay with, proves it holds a play token minted for it that
    /// works now (P8-04): its relay lets it through as a friend, to reach
    /// this server and redeem it. Guarded as an invite holder's is
    /// (`crate::invites::mac_matches`). The relay token of one of its own
    /// tokens, used up or expired but still kept, lets it through no more,
    /// and is not counted as a guess: a device that reconnects with it
    /// after its play is not guessing, and must not use up the guesses its
    /// next redemption needs.
    pub async fn holds_play_token(&self, endpoint: &EndpointId, token: &str) -> bool {
        let Some(mac) = spoolframe_protocol::relay_access::parse_play_token(token) else {
            return false;
        };
        let store = self.store();
        let device = endpoint.to_string();
        let hashes = store.federation_tokens_of(&device);
        crate::invites::mac_matches(&self.auth, endpoint, "play-token", mac, hashes)
            .await
            .unwrap_or(false)
    }

    /// End the play token sessions whose item is no longer in their
    /// library, or whose library is shared no more, with their playbacks:
    /// as the catalog is replaced (F8-04), a library or a Plex server
    /// added, removed or changed. Those still being redeemed end too. The
    /// change does not say which items it moved, so each session's item
    /// is placed again, once per place. An item the catalog cannot place
    /// now ends its sessions, as it opens none at redemption: this runs
    /// only as an admin changes the catalog, so a miss counted over
    /// several runs would wait days. A move within a Plex server, which
    /// replaces no catalog, is caught as the session next starts a
    /// playback or a download (`client.rs`).
    pub async fn end_moved_sessions(&self) {
        let sessions = self.auth.remote_sessions();
        let mut ended = 0;
        for (peer, item, library) in sessions.places() {
            if self.shared_in(&peer, &item, &library).await != Some(true) {
                ended += sessions
                    .end_where(|u| u.peer == peer && u.item == item && u.library == library);
            }
        }
        self.auth
            .remote_sessions_ended(ended, "their items left their libraries");
    }

    /// End the play token sessions of pair `peer`, those of `library`
    /// alone if given, with their playbacks: at once, as signed-out
    /// sessions' are. Those still being redeemed end too, and open no
    /// more.
    pub(crate) fn end_remote_sessions(&self, peer: &str, library: Option<&str>) {
        let ended = self
            .auth
            .remote_sessions()
            .end_where(|u| u.peer == peer && library.is_none_or(|library| u.library == library));
        match library {
            Some(library) => self.auth.remote_sessions_ended(
                ended,
                format_args!("pair {peer}'s grant of {library} revoked"),
            ),
            None => self
                .auth
                .remote_sessions_ended(ended, format_args!("pair {peer} unpaired")),
        }
    }
}

/// A test's hold on the next redemption, once its token is used up and
/// before its session opens: it says it is there, and waits to go on.
#[cfg(test)]
#[derive(Debug, Default)]
pub(super) struct Hold(
    parking_lot::Mutex<
        Option<(
            tokio::sync::oneshot::Sender<()>,
            tokio::sync::oneshot::Receiver<()>,
        )>,
    >,
);

#[cfg(test)]
impl Hold {
    /// Hold the next redemption: what says it is held there, and what
    /// lets it go on.
    pub(super) fn next(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (reached, held) = tokio::sync::oneshot::channel();
        let (go, going) = tokio::sync::oneshot::channel();
        *self.0.lock() = Some((reached, going));
        (held, go)
    }

    async fn here(&self) {
        let held = self.0.lock().take();
        if let Some((reached, go)) = held {
            let _ = reached.send(());
            let _ = go.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::sync::Arc;

    use spoolframe_protocol::federation::AssertionRefused;
    use spoolframe_protocol::relay_access::{Tier, play_relay_token};

    use super::super::tokens::tests::{bob_s_key, bob_wants, minted, sharing, sharing_from};
    use super::*;
    use crate::store::federation::KeyHold;
    use crate::test_support::{FakeCatalog, movie, music_library};

    /// A device of Bob's.
    fn device() -> EndpointId {
        iroh::SecretKey::from_bytes(&[9; 32]).public()
    }

    /// Redeem `token` from `endpoint`, held once its token is used up
    /// while `meanwhile` runs to its end: what the redemption opened.
    async fn redeemed_while<F: Future<Output = ()>>(
        federation: &Federation,
        token: &str,
        endpoint: &str,
        meanwhile: F,
    ) -> Result<(SessionToken, RemoteSessionUser), PlayTokenError> {
        let (held, go) = federation.hold_redeeming.next();
        let racing = async {
            held.await.unwrap();
            meanwhile.await;
            go.send(()).unwrap();
        };
        tokio::join!(federation.redeem_play_token(token, endpoint), racing).0
    }

    /// The security review's F8-02: a token used up just before its pair
    /// is unpaired, or its grant revoked, opens no session once the
    /// revocation has ended the pair's: its device is no one's, and the
    /// relay lets it through no more.
    #[tokio::test]
    async fn a_revocation_while_a_token_is_redeemed_leaves_no_session() {
        for unpaired in [true, false] {
            let (store, federation, bob_s) = sharing("fed-redeem-race-revoke").await;
            let federation = Arc::new(federation);
            let peer = store
                .live_peer_by_server_id(bob_s.as_str())
                .await
                .unwrap()
                .unwrap();
            let device = device().to_string();
            let open = minted(&federation, &bob_s, device.clone()).await;
            let (before, _) = federation.redeem_play_token(&open, &device).await.unwrap();
            let token = minted(&federation, &bob_s, device.clone()).await;
            let revoke = async {
                if unpaired {
                    federation.unpair(&peer.id, "alice").await.unwrap().unwrap();
                } else {
                    federation
                        .unshare(&peer.id, "plex:1", "alice")
                        .await
                        .unwrap()
                        .unwrap();
                }
            };
            let redeemed = redeemed_while(&federation, &token, &device, revoke).await;
            assert!(
                matches!(redeemed, Err(PlayTokenError::Invalid)),
                "unpaired {unpaired}"
            );
            let sessions = federation.auth.remote_sessions();
            assert_eq!(sessions.count(|_| true), 0, "unpaired {unpaired}");
            assert!(sessions.owner(&before.0).is_none());
            let relay = crate::relay_access::RelayAccess::new(
                Vec::new(),
                iroh::SecretKey::from_bytes(&[1; 32]).public(),
                federation.auth.clone(),
                Some(federation.clone()),
            );
            assert_eq!(relay.tier(&device, None).await, Tier::Stranger);
        }
    }

    /// F8-02's defence in depth: a session whose pair or grant is gone
    /// without its cleanup reaching it authenticates no more, and ends
    /// as it is tried.
    #[tokio::test]
    async fn a_session_outliving_its_pair_or_grant_authenticates_no_more() {
        for unpaired in [true, false] {
            let (store, federation, bob_s) = sharing("fed-redeem-stale").await;
            let peer = store
                .live_peer_by_server_id(bob_s.as_str())
                .await
                .unwrap()
                .unwrap();
            let device = device().to_string();
            let token = minted(&federation, &bob_s, device.clone()).await;
            let (session, _) = federation.redeem_play_token(&token, &device).await.unwrap();
            assert!(
                federation
                    .auth
                    .endpoint_who(&session.0, &device)
                    .await
                    .unwrap()
                    .is_some()
            );
            if unpaired {
                store.revoke_peer(&peer.id).await.unwrap().unwrap();
            } else {
                store
                    .revoke_library_grant(&peer.id, "plex:1")
                    .await
                    .unwrap()
                    .unwrap();
            }
            let owner = federation.auth.session_owner(&session.0).await.unwrap();
            assert!(owner.is_none_or(|o| !o.live), "unpaired {unpaired}");
            assert!(
                federation
                    .auth
                    .endpoint_who(&session.0, &device)
                    .await
                    .unwrap()
                    .is_none(),
                "unpaired {unpaired}"
            );
            assert!(!federation.auth.remote_sessions().endpoint_has(&device));
        }
    }

    /// F8-03: confirming Bob's server's new key ends what its old key
    /// held, the proxied sessions it opened and the tokens minted for it,
    /// even one being redeemed then; and its pooled connection. The relay
    /// lets the old key through no more. A device's own tokens and
    /// sessions are not the pair's key's, and keep working.
    #[tokio::test]
    async fn a_confirmed_key_ends_what_the_old_key_held() {
        let (store, federation, bob_s) = sharing("fed-redeem-rotated").await;
        let federation = Arc::new(federation);
        let peer = store
            .live_peer_by_server_id(bob_s.as_str())
            .await
            .unwrap()
            .unwrap();
        let old = bob_s_key().to_string();
        let device = device().to_string();
        let proxied = minted(&federation, &bob_s, old.clone()).await;
        let (proxied, user) = federation.redeem_play_token(&proxied, &old).await.unwrap();
        assert!(user.proxied);
        let unused = minted(&federation, &bob_s, old.clone()).await;
        let racing = minted(&federation, &bob_s, old.clone()).await;
        let direct = minted(&federation, &bob_s, device.clone()).await;
        let (direct, _) = federation
            .redeem_play_token(&direct, &device)
            .await
            .unwrap();
        let direct_unused = minted(&federation, &bob_s, device.clone()).await;

        let new = iroh::SecretKey::from_bytes(&[12; 32]).public().to_string();
        assert_eq!(
            store.hold_peer_key(&peer.id, &new).await.unwrap(),
            KeyHold::New
        );
        let confirm = async {
            federation
                .confirm_key(&peer.id, &new, "alice")
                .await
                .unwrap();
        };
        let redeemed = redeemed_while(&federation, &racing, &old, confirm).await;
        assert!(matches!(redeemed, Err(PlayTokenError::Invalid)));

        let auth = &federation.auth;
        assert!(auth.endpoint_who(&proxied.0, &old).await.unwrap().is_none());
        assert!(matches!(
            federation.redeem_play_token(&unused, &old).await,
            Err(PlayTokenError::Invalid)
        ));
        let relay = crate::relay_access::RelayAccess::new(
            Vec::new(),
            iroh::SecretKey::from_bytes(&[1; 32]).public(),
            auth.clone(),
            Some(federation.clone()),
        );
        let relay_token = play_relay_token(&unused, bob_s_key().as_bytes());
        assert_eq!(relay.tier(&old, None).await, Tier::Stranger);
        assert_eq!(relay.tier(&old, Some(&relay_token)).await, Tier::Stranger);
        assert!(
            auth.endpoint_who(&direct.0, &device)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            federation
                .redeem_play_token(&direct_unused, &device)
                .await
                .is_ok()
        );
    }

    /// F8-05: a download token being redeemed, used up but its session
    /// not open yet, still holds its place under the download caps: a
    /// mint meanwhile is refused at the cap, per user and per pair.
    #[tokio::test]
    async fn a_download_token_being_redeemed_holds_its_place_under_the_caps() {
        use crate::store::federation_tokens::TokenLimits;
        for (per_user, per_peer, next_user) in [(1, 100, 7), (100, 1, 8)] {
            let (_store, federation, bob_s) = sharing("fed-redeem-race-caps").await;
            federation.set_limits(super::super::Limits {
                download_tokens: TokenLimits { per_user, per_peer },
                ..federation.limits()
            });
            let device = device().to_string();
            let token = federation
                .mint(&bob_s, &bob_wants(device.clone(), TokenScope::Download, 7))
                .await
                .unwrap()
                .token;
            let mut minted_meanwhile = None;
            let mint = async {
                minted_meanwhile = Some(
                    federation
                        .mint(
                            &bob_s,
                            &bob_wants(device.clone(), TokenScope::Download, next_user),
                        )
                        .await,
                );
            };
            redeemed_while(&federation, &token, &device, mint)
                .await
                .unwrap();
            assert_eq!(
                minted_meanwhile.unwrap().map(|g| g.scope),
                Err(AssertionRefused::Unavailable),
                "per user {per_user}, per pair {per_peer}"
            );
        }
    }

    /// F8-04: a token, and a session, are held to the library their item
    /// is in now. Moved to a library not shared with the pair, while its
    /// grant stays, the item opens nothing: not a session open already,
    /// nor one being redeemed then, nor a token kept, nor a new one.
    /// Shared where it is now, it plays from there.
    #[tokio::test]
    async fn an_item_moved_out_of_its_shared_library_opens_nothing() {
        let catalog = Arc::new(FakeCatalog::new("/nowhere/movie.mkv".into()));
        catalog.serve_music();
        let (store, federation, bob_s) = sharing_from("fed-redeem-moved", catalog.clone()).await;
        let peer = store
            .live_peer_by_server_id(bob_s.as_str())
            .await
            .unwrap()
            .unwrap();
        let device = device().to_string();
        let open = minted(&federation, &bob_s, device.clone()).await;
        let (session, _) = federation.redeem_play_token(&open, &device).await.unwrap();
        let racing = minted(&federation, &bob_s, device.clone()).await;
        let unused = minted(&federation, &bob_s, device.clone()).await;

        let moved = async {
            catalog.move_to(movie().id(), music_library().id);
            federation.end_moved_sessions().await;
        };
        let redeemed = redeemed_while(&federation, &racing, &device, moved).await;
        assert!(matches!(redeemed, Err(PlayTokenError::Invalid)));
        assert!(
            federation
                .auth
                .endpoint_who(&session.0, &device)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(federation.auth.remote_sessions().count(|_| true), 0);
        assert!(matches!(
            federation.redeem_play_token(&unused, &device).await,
            Err(PlayTokenError::Invalid)
        ));
        assert_eq!(
            federation
                .mint(&bob_s, &bob_wants(device.clone(), TokenScope::Play, 7))
                .await
                .map(|g| g.scope),
            Err(AssertionRefused::NotShared)
        );

        federation
            .share(&peer.id, &[music_library().id], "alice")
            .await
            .unwrap();
        let shared = minted(&federation, &bob_s, device.clone()).await;
        let (_, user) = federation
            .redeem_play_token(&shared, &device)
            .await
            .unwrap();
        assert_eq!(user.library, music_library().id.to_string());
    }

    /// F8-06: a pair holds at most its setting's play sessions; past it a
    /// token is refused and not used up, and works once one ends. A
    /// download's session is held to the download token limits instead.
    #[tokio::test]
    async fn a_pair_holds_at_most_its_play_sessions() {
        let (_store, federation, bob_s) = sharing("fed-redeem-play-room").await;
        federation.set_limits(super::super::Limits {
            play_sessions: 2,
            ..federation.limits()
        });
        let device = device().to_string();
        let mut open = Vec::new();
        for _ in 0..2 {
            let token = minted(&federation, &bob_s, device.clone()).await;
            open.push(
                federation
                    .redeem_play_token(&token, &device)
                    .await
                    .unwrap()
                    .0,
            );
        }
        let third = minted(&federation, &bob_s, device.clone()).await;
        assert!(matches!(
            federation.redeem_play_token(&third, &device).await,
            Err(PlayTokenError::Busy)
        ));
        let download = federation
            .mint(&bob_s, &bob_wants(device.clone(), TokenScope::Download, 7))
            .await
            .unwrap()
            .token;
        assert!(
            federation
                .redeem_play_token(&download, &device)
                .await
                .is_ok()
        );
        assert!(federation.auth.remote_sessions().end(&open[0].0));
        assert!(federation.redeem_play_token(&third, &device).await.is_ok());
    }
}
