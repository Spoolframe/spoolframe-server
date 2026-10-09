//! The main server's side of its relay's question (P6-04): which tier an
//! endpoint is in, for a relay of its own to let through or not. Only the
//! relays `network.own_relays` names are answered; anyone else is closed
//! on at once.

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

#[derive(Debug, Clone)]
pub struct RelayAccess {
    /// The relays of its own the server answers (`network.own_relays`).
    relays: Arc<Vec<EndpointId>>,
    /// The server's own key: its relay must carry it too.
    server: EndpointId,
    auth: Auth,
}

impl RelayAccess {
    pub fn new(relays: Vec<EndpointId>, server: EndpointId, auth: Auth) -> Self {
        Self {
            relays: Arc::new(relays),
            server,
            auth,
        }
    }

    /// Which tier `endpoint` is in: the server itself and a device with a
    /// live session, or trusted for a user, are its owner's. Friends come
    /// with P7 and P8; until then there are none.
    pub async fn tier(&self, endpoint: &str) -> Tier {
        if endpoint == self.server.to_string() {
            return Tier::Owner;
        }
        let store = self.auth.store();
        let signed_in = store.endpoint_signed_in(endpoint).await;
        let trusted = self.auth.trusted_profiles(endpoint).await;
        match (signed_in, trusted) {
            (Ok(true), _) => Tier::Owner,
            (_, Ok(profiles)) if !profiles.is_empty() => Tier::Owner,
            (Err(e), _) | (_, Err(e)) => {
                // Not known to be the owner's: refused, as a stranger.
                tracing::warn!(error = %e, endpoint, "cannot tell the relay who an endpoint is");
                Tier::Stranger
            }
            _ => Tier::Stranger,
        }
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
                let tier = this.tier(&query.endpoint).await;
                if write_frame(&mut send, &AccessAnswer { tier }).await.is_ok() {
                    let _ = send.finish();
                }
            });
        }
        Ok(())
    }
}
