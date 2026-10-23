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
//! playbacks with them ([`Federation::end_remote_sessions`]).

use iroh::EndpointId;
use spoolframe_protocol::federation::TokenScope;
use spoolframe_types::{MediaId, MediaKind};

use super::Federation;
use crate::auth::SessionToken;
use crate::principal::Principal;
use crate::remote_sessions::RemoteSessionUser;
use crate::store::StoreError;

/// Why a play token opened nothing.
#[derive(Debug, thiserror::Error)]
pub enum PlayTokenError {
    /// Unknown, used, expired, minted for another endpoint, its grant or
    /// pair gone, or not a play: told alike.
    #[error("that play token opens nothing")]
    Invalid,
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
        let redeemed = store
            .redeem_federation_token(&hash, endpoint)
            .await?
            .ok_or(PlayTokenError::Invalid)?;
        let Ok(Principal::RemoteUser { server, user }) = redeemed.principal.parse() else {
            return Err(PlayTokenError::Invalid);
        };
        let item: MediaId = redeemed.item.parse().map_err(|_| PlayTokenError::Invalid)?;
        // Mint keeps only plays of what plays; read back, it is held to
        // that again.
        if redeemed.scope != TokenScope::Play.name()
            || !matches!(
                item.kind(),
                MediaKind::Movie | MediaKind::Episode | MediaKind::Track
            )
        {
            return Err(PlayTokenError::Invalid);
        }
        // The redemption checked its pair is live, and read what it is.
        let proxied = redeemed.peer_endpoint == endpoint;
        // Counted off the way of the session's opening: a count missed is
        // logged, and the play goes on.
        let counting = store.clone();
        let peer = redeemed.peer.clone();
        tokio::spawn(async move {
            if let Err(e) = counting.count_federation_served(&peer, proxied).await {
                tracing::warn!(error = %e, "cannot count a paired server's play");
            }
        });
        // SA's admins manage servers, not people (P8-03): the Playback
        // page names the paired server, never its user.
        let session_user = RemoteSessionUser {
            server,
            user,
            display_name: redeemed.peer_name,
            peer: redeemed.peer,
            library: redeemed.library,
            item: item.clone(),
            scope: TokenScope::Play,
            proxied,
            preferences: redeemed
                .preferences
                .as_deref()
                .and_then(|json| serde_json::from_str(json).ok()),
        };
        let session = self
            .auth
            .remote_sessions()
            .open(session_user.clone(), endpoint);
        tracing::info!(
            peer = %redeemed.peer_endpoint,
            principal = %session_user.principal(),
            %item,
            proxied,
            "a play token is redeemed"
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

    /// End the play token sessions of pair `peer`, those of `library`
    /// alone if given, with their playbacks: at once, as signed-out
    /// sessions' are.
    pub(crate) fn end_remote_sessions(&self, peer: &str, library: Option<&str>) {
        let ended = self
            .auth
            .remote_sessions()
            .end_where(|u| u.peer == peer && library.is_none_or(|library| u.library == library));
        if ended > 0 {
            tracing::info!(
                peer,
                library,
                ended,
                "a paired server's play sessions ended"
            );
            self.store().note_sessions_ended();
        }
    }
}
