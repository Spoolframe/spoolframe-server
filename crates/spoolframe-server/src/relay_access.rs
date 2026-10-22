//! The main server's side of its relay's question (P6-04): which tier an
//! endpoint is in, for a relay of its own to let through or not. Only the
//! relays `network.own_relays` names are answered; anyone else is closed
//! on at once.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use iroh::EndpointId;
use iroh::endpoint::{Connection, VarInt};
use iroh::protocol::{AcceptError, ProtocolHandler};
use spoolframe_protocol::close;
use spoolframe_protocol::codec::{read_frame_within, write_frame};
use spoolframe_protocol::relay_access::{AccessAnswer, AccessQuery, Tier};

use crate::auth::Auth;

/// A query is a key and some framing: anything larger is not one.
const MAX_QUERY: u32 = 1024;
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
    /// with this one while federation is on (P8-01); anyone else a
    /// stranger.
    pub async fn tier(&self, endpoint: &str, invite: Option<&str>) -> Tier {
        if endpoint == self.server.to_string() {
            return Tier::Owner;
        }
        let store = self.auth.store();
        // A device signed in here is the owner's; one trusted for a user
        // is too. Asked in turn: most answers end at the first.
        match store.endpoint_signed_in(endpoint).await {
            Ok(true) => return Tier::Owner,
            Ok(false) => {}
            Err(e) => {
                // Not known to be the owner's: refused, as a stranger.
                tracing::warn!(error = %e, endpoint, "cannot tell the relay who an endpoint is");
                return Tier::Stranger;
            }
        }
        match self.auth.trusted_profiles(endpoint).await {
            Ok(profiles) if !profiles.is_empty() => return Tier::Owner,
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, endpoint, "cannot tell the relay who an endpoint is");
                return Tier::Stranger;
            }
        }
        match store.endpoint_has_portable_session(endpoint).await {
            Ok(true) => return Tier::Friend,
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(error = %e, endpoint, "cannot tell the relay who an endpoint is");
                return Tier::Stranger;
            }
        }
        if let Some(federation) = &self.federation {
            match federation.is_paired(endpoint).await {
                Ok(true) => return Tier::Friend,
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(error = %e, endpoint, "cannot tell the relay who an endpoint is");
                    return Tier::Stranger;
                }
            }
        }
        let holder = match (invite, endpoint.parse::<EndpointId>()) {
            (Some(token), Ok(id)) => crate::invites::holder_tier(&self.auth, &id, token).await,
            _ => None,
        };
        holder.unwrap_or(Tier::Stranger)
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
        while let Ok((mut send, mut recv)) = connection.accept_bi().await {
            let this = self.clone();
            tokio::spawn(async move {
                let query: AccessQuery =
                    match tokio::time::timeout(QUERY_WAIT, read_frame_within(&mut recv, MAX_QUERY))
                        .await
                    {
                        Ok(Ok(query)) => query,
                        _ => return,
                    };
                let tier = this.tier(&query.endpoint, query.invite.as_deref()).await;
                if write_frame(&mut send, &AccessAnswer { tier }).await.is_ok() {
                    let _ = send.finish();
                }
            });
        }
        Ok(())
    }
}
