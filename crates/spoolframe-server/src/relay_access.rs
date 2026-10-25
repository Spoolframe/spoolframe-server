//! The main server's side of its relay's question (P6-04): which tier an
//! endpoint is in, for a relay of its own to let through or not. Only the
//! relays `network.own_relays` names are answered; anyone else is closed
//! on at once. A relay is told at once when a paired server's answer no
//! longer holds, unpaired or moved to a new key, rather than left to keep
//! it until it expires.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use iroh::EndpointId;
use iroh::endpoint::{Connection, VarInt};
use iroh::protocol::{AcceptError, ProtocolHandler};
use spoolframe_protocol::close;
use spoolframe_protocol::codec::{read_frame_within, write_frame};
use spoolframe_protocol::relay_access::{
    AccessAnswer, AccessQuery, AccessRevoked, MAX_FRAME, Tier, parse_play_token,
};
use tokio::sync::broadcast::error::RecvError;

use crate::auth::Auth;
use crate::store::StoreError;

/// How long a relay may take to send its question once it opens a stream.
const QUERY_WAIT: Duration = Duration::from_secs(10);
/// The connections one named relay may hold on this ALPN at once (P6-07,
/// both reviews' first finding). A relay asks over one kept connection;
/// the second is room for a reconnect while the old one closes. The
/// transport holds each to 4 streams, so a relay whose key is stolen
/// holds at most 8 asks on the store, not as many as it can open.
pub const MAX_CONNECTIONS: usize = 2;

#[derive(Debug, Clone)]
pub struct RelayAccess {
    /// The relays of its own the server answers (`network.own_relays`).
    relays: Arc<Vec<EndpointId>>,
    /// The server's own key: its relay must carry it too.
    server: EndpointId,
    auth: Auth,
    /// Federation, while `experimental.federation` is on (P8-01): a live
    /// pair's endpoint is a friend.
    federation: Option<Arc<crate::federation::Federation>>,
    /// How many connections each relay holds now.
    open: Arc<parking_lot::Mutex<HashMap<EndpointId, usize>>>,
}

/// One connection counted against its relay's [`MAX_CONNECTIONS`]; it
/// is uncounted when dropped.
struct Held {
    open: Arc<parking_lot::Mutex<HashMap<EndpointId, usize>>>,
    relay: EndpointId,
}

impl Drop for Held {
    fn drop(&mut self) {
        let mut open = self.open.lock();
        if let Some(n) = open.get_mut(&self.relay) {
            *n -= 1;
            if *n == 0 {
                open.remove(&self.relay);
            }
        }
    }
}

impl RelayAccess {
    /// Answering `relays` for `server`, over `auth`'s store; servers
    /// `federation` pairs with are friends, while there is one.
    pub fn new(
        relays: Vec<EndpointId>,
        server: EndpointId,
        auth: Auth,
        federation: Option<Arc<crate::federation::Federation>>,
    ) -> Self {
        Self {
            relays: Arc::new(relays),
            server,
            auth,
            federation,
            open: Arc::default(),
        }
    }

    /// Count a connection from `relay`, if it holds fewer than
    /// [`MAX_CONNECTIONS`].
    fn hold(&self, relay: EndpointId) -> Option<Held> {
        let mut open = self.open.lock();
        let n = open.entry(relay).or_default();
        if *n >= MAX_CONNECTIONS {
            return None;
        }
        *n += 1;
        Some(Held {
            open: self.open.clone(),
            relay,
        })
    }

    /// Which tier `endpoint` is in: the server itself and a device with a
    /// live session, or trusted for a user, are its owner's. A device that
    /// is no one's, with a live invite's token for it, is the tier of the
    /// invite's kind (P6-08, [`crate::invites::holder_tier`]); one with a
    /// portable user's session (P7-06) a friend's, as is a server paired
    /// with this one while federation is on (P8-01), and a paired server's
    /// user's device with a play token, or the session it opened (P8-04);
    /// anyone else a stranger.
    pub async fn tier(&self, endpoint: &str, invite: Option<&str>) -> Tier {
        self.tier_of(endpoint, invite)
            .await
            .unwrap_or_else(|stranger| stranger)
    }

    /// [`RelayAccess::tier`]: asked in turn, most answers ending at the
    /// first; an answer that cannot be had ends it as a stranger's (`Err`).
    async fn tier_of(&self, endpoint: &str, invite: Option<&str>) -> Result<Tier, Tier> {
        if endpoint == self.server.to_string() {
            return Ok(Tier::Owner);
        }
        let known = |asked: Result<bool, StoreError>| {
            asked.map_err(|e| {
                // Not known to be anyone's: refused, as a stranger.
                tracing::warn!(error = %e, endpoint, "cannot tell the relay who an endpoint is");
                Tier::Stranger
            })
        };
        let store = self.auth.store();
        // A device signed in here is the owner's; one trusted for a user
        // is too.
        if known(store.endpoint_signed_in(endpoint).await)? {
            return Ok(Tier::Owner);
        }
        let trusted = self.auth.trusted_profiles(endpoint).await;
        if known(trusted.map(|profiles| !profiles.is_empty()))? {
            return Ok(Tier::Owner);
        }
        if known(store.endpoint_has_portable_session(endpoint).await)? {
            return Ok(Tier::Friend);
        }
        let id = endpoint.parse::<EndpointId>().ok();
        if let Some(federation) = &self.federation {
            if known(federation.is_paired(endpoint).await)? {
                return Ok(Tier::Friend);
            }
            // P8-04: a paired server's user's device, while the session its
            // play token opened lives, or holding the token to redeem.
            if self.auth.remote_sessions().endpoint_has(endpoint) {
                return Ok(Tier::Friend);
            }
            // A play token's relay token is checked as one alone: one that
            // does not hold stops here, as a stranger's, and is never tried
            // as an invite's.
            if let (Some(token), Some(id)) = (invite, &id)
                && parse_play_token(token).is_some()
            {
                return Ok(if federation.holds_play_token(id, token).await {
                    Tier::Friend
                } else {
                    Tier::Stranger
                });
            }
        }
        let holder = match (invite, &id) {
            (Some(token), Some(id)) => crate::invites::holder_tier(&self.auth, id, token).await,
            _ => None,
        };
        Ok(holder.unwrap_or(Tier::Stranger))
    }
}

impl ProtocolHandler for RelayAccess {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let relay = connection.remote_id();
        if !self.relays.contains(&relay) {
            tracing::info!(peer = %relay.fmt_short(), "refusing a relay this server does not name");
            connection.close(
                VarInt::from_u32(close::UNSUPPORTED_PROTOCOL),
                b"not a relay of this server's",
            );
            connection.closed().await;
            return Ok(());
        }
        let Some(_held) = self.hold(relay) else {
            tracing::warn!(
                relay = %relay.fmt_short(),
                max = MAX_CONNECTIONS,
                "refusing a connection: the relay holds as many as it may"
            );
            connection.close(VarInt::from_u32(close::BUSY), b"too many connections");
            connection.closed().await;
            return Ok(());
        };
        tracing::debug!(relay = %relay.fmt_short(), "the server's relay connected");
        let mut revoked = self.federation.as_ref().map(|f| f.revocations());
        loop {
            let revocation = async {
                match revoked.as_mut() {
                    Some(revoked) => revoked.recv().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                accepted = connection.accept_bi() => {
                    let Ok((send, recv)) = accepted else {
                        break;
                    };
                    tokio::spawn(self.clone().answer(send, recv));
                }
                revocation = revocation => match revocation {
                    Ok(endpoint) => {
                        tokio::spawn(tell_revoked(connection.clone(), endpoint));
                    }
                    // Behind by more than are kept: closed, the relay
                    // forgets every answer as it connects again.
                    Err(RecvError::Lagged(_)) => {
                        connection.close(VarInt::from_u32(close::BUSY), b"answers revoked");
                        break;
                    }
                    Err(RecvError::Closed) => revoked = None,
                },
            }
        }
        Ok(())
    }
}

impl RelayAccess {
    /// The question on `recv`, answered on `send`.
    async fn answer(
        self,
        mut send: iroh::endpoint::SendStream,
        mut recv: iroh::endpoint::RecvStream,
    ) {
        let query: AccessQuery =
            match tokio::time::timeout(QUERY_WAIT, read_frame_within(&mut recv, MAX_FRAME)).await {
                Ok(Ok(query)) => query,
                _ => return,
            };
        let tier = self.tier(&query.endpoint, query.invite.as_deref()).await;
        if write_frame(&mut send, &AccessAnswer { tier }).await.is_ok() {
            let _ = send.finish();
        }
    }
}

/// Tell the relay on `connection` that what it was answered for
/// `endpoint` no longer holds, within [`QUERY_WAIT`]. A relay from before
/// this never reads it, and keeps its answer until it expires.
async fn tell_revoked(connection: Connection, endpoint: EndpointId) {
    let told = tokio::time::timeout(QUERY_WAIT, async {
        let mut send = connection.open_uni().await.map_err(|e| e.to_string())?;
        let revoked = AccessRevoked {
            endpoint: endpoint.to_string(),
        };
        write_frame(&mut send, &revoked)
            .await
            .map_err(|e| e.to_string())?;
        send.finish().map_err(|e| e.to_string())
    })
    .await;
    match told {
        Ok(Ok(())) => {
            tracing::debug!(endpoint = %endpoint.fmt_short(), "the relay is told an answer no longer holds")
        }
        Ok(Err(e)) => {
            tracing::warn!(endpoint = %endpoint.fmt_short(), error = %e, "the relay cannot be told an answer no longer holds")
        }
        Err(_) => {
            tracing::warn!(endpoint = %endpoint.fmt_short(), "the relay was not told in time that an answer no longer holds")
        }
    }
}

#[cfg(test)]
mod tests {
    use iroh::endpoint::presets;
    use iroh::{Endpoint, SecretKey};
    use spoolframe_protocol::relay_access::RELAY_ACCESS_ALPN;

    use super::*;
    use crate::federation::test_pairs::{bob_s_key, sharing};
    use crate::store::federation::KeyHold;

    fn endpoint() -> iroh::endpoint::Builder {
        Endpoint::builder(presets::Minimal).relay_mode(iroh::RelayMode::Disabled)
    }

    /// What the server tells `relay` next of an answer that no longer
    /// holds.
    async fn told(relay: &Connection) -> AccessRevoked {
        let wait = std::time::Duration::from_secs(10);
        let mut recv = tokio::time::timeout(wait, relay.accept_uni())
            .await
            .expect("told in time")
            .unwrap();
        read_frame_within(&mut recv, MAX_FRAME).await.unwrap()
    }

    /// The audit's C8-01: a relay connected is told at once that a pair's
    /// old key, its new one confirmed, is no friend now, and then that an
    /// unpaired server's key is not.
    #[tokio::test]
    async fn a_relay_is_told_of_a_pair_s_old_key_and_of_an_unpairing_at_once() {
        let (store, federation, bob_s) = sharing("relay-access-revoked").await;
        let federation = Arc::new(federation);
        let peer = store
            .live_peer_by_server_id(bob_s.as_str())
            .await
            .unwrap()
            .unwrap();
        let server = endpoint()
            .alpns(vec![RELAY_ACCESS_ALPN.to_vec()])
            .bind()
            .await
            .unwrap();
        let relay_key = SecretKey::from_bytes(&[4; 32]);
        let access = RelayAccess::new(
            vec![relay_key.public()],
            server.id(),
            federation.auth().clone(),
            Some(federation.clone()),
        );
        let _router = iroh::protocol::Router::builder(server.clone())
            .accept(RELAY_ACCESS_ALPN, access)
            .spawn();
        let relay = endpoint().secret_key(relay_key).bind().await.unwrap();
        let connection = relay
            .connect(server.addr(), RELAY_ACCESS_ALPN)
            .await
            .unwrap();
        // Answered once: the server is listening for what it revokes.
        let (mut send, mut recv) = connection.open_bi().await.unwrap();
        let query = AccessQuery {
            endpoint: bob_s_key().to_string(),
            invite: None,
        };
        write_frame(&mut send, &query).await.unwrap();
        send.finish().unwrap();
        let answer: AccessAnswer = read_frame_within(&mut recv, MAX_FRAME).await.unwrap();
        assert_eq!(answer.tier, Tier::Friend);

        let new = SecretKey::from_bytes(&[12; 32]).public().to_string();
        assert_eq!(
            store.hold_peer_key(&peer.id, &new).await.unwrap(),
            KeyHold::New
        );
        federation
            .confirm_key(&peer.id, &new, "alice")
            .await
            .unwrap();
        assert_eq!(told(&connection).await.endpoint, bob_s_key().to_string());
        federation.unpair(&peer.id, "alice").await.unwrap().unwrap();
        assert_eq!(told(&connection).await.endpoint, new);
    }
}
