//! Users and devices › Federation (P8-01): the `federation_*` methods,
//! over [`crate::federation::Federation`]. Experimental: without it
//! (`experimental.federation` off) each is `NOT_CONFIGURED`.

use jsonrpsee::Extensions;
use jsonrpsee::core::{RpcResult, async_trait};
use jsonrpsee::types::ErrorObjectOwned;
use serde_json::json;
use spoolframe_admin_api::{
    FederationApiServer, GrantEntry, PairLookup, PeerDetail, PeerEntry, PeerKey, PlayDay,
    ReceivedLibrary, WithdrawnLibrary, error_code,
};
use spoolframe_federation::DialError;
use spoolframe_protocol::federation::PairingRefused;
use spoolframe_protocol::fingerprint;

use super::{AdminApi, error, internal};
use crate::federation::{Federation, PairError, ReplicaError, ShareError};
use crate::store::federation::{PeerError, PeerRecord};
use crate::store::federation_grants::GrantRecord;

impl AdminApi {
    /// Federation, or why there is none. Gated on the server's switch
    /// alone: the admin API has no `Hello` to name experimental features
    /// in, and its callers are this server's own web UI and CLI, built
    /// with it. The client protocol also asks that the app named the
    /// feature (`federation_for`), as apps of other builds connect there.
    fn federation(&self) -> RpcResult<&Federation> {
        self.federation.as_deref().ok_or_else(|| {
            error(
                error_code::NOT_CONFIGURED,
                "federation is experimental and off: set experimental.federation and restart",
            )
        })
    }

    /// What changes a pair asks first, in this order: an admin signed in
    /// recently, federation on, and who the change is by, as a pair keeps
    /// it ([`AdminApi::maker`]).
    async fn pair_change(&self, ext: &Extensions) -> RpcResult<(&Federation, String)> {
        let caller = self.recent_admin(ext).await?;
        let federation = self.federation()?;
        let (by, _) = self.maker(ext, caller).await?;
        Ok((federation, by))
    }

    async fn peer_entry(&self, federation: &Federation, peer: PeerRecord) -> RpcResult<PeerEntry> {
        let print = |endpoint: &str| fingerprint::of_hex(endpoint).unwrap_or_default();
        let live = peer.live();
        let held = if live {
            federation.held_keys(&peer.id).await.map_err(internal)?
        } else {
            Vec::new()
        };
        let (here, there) = federation.play_counts(&peer.id).await.map_err(internal)?;
        Ok(PeerEntry {
            fingerprint: print(&peer.endpoint_id),
            connected: live && federation.connected(&peer.endpoint_id),
            last_sync: federation
                .last_sync(&peer.server_id)
                .await
                .map_err(internal)?,
            plays_here: here.plays,
            plays_there: there.plays,
            refused_here: here.refused,
            refused_there: there.refused,
            direct_here: here.direct(),
            proxied_here: here.proxied,
            proxied_there: there.proxied,
            held_keys: held
                .into_iter()
                .map(|key| PeerKey {
                    fingerprint: print(&key.endpoint_id),
                    endpoint_id: key.endpoint_id,
                    first_seen: key.first_seen,
                    last_seen: key.last_seen,
                    rejected_at: key.rejected_at,
                })
                .collect(),
            id: peer.id,
            name: peer.display_name,
            server_id: peer.server_id,
            endpoint_id: peer.endpoint_id,
            paired_by: peer.paired_by,
            invite_id: peer.invite_id,
            paired_at: peer.paired_at,
            revoked_at: peer.revoked_at,
        })
    }
}

/// A pairing's refusal, as `PAIRING_REFUSED`'s `data.reason`.
fn refused(reason: &str, message: impl Into<String>) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(
        error_code::PAIRING_REFUSED,
        message.into(),
        Some(json!({ "reason": reason })),
    )
}

fn pair_error(e: PairError) -> ErrorObjectOwned {
    let message = e.to_string();
    match e {
        PairError::NotALink | PairError::NoCode | PairError::WrongKind | PairError::Itself => {
            error(error_code::INVALID_INPUT, message)
        }
        PairError::Starting => error(error_code::NOT_CONFIGURED, message),
        PairError::AlreadyPaired => refused(PairingRefused::AlreadyPaired.name(), message),
        PairError::KeyHeld => refused(PairingRefused::KeyHeld.name(), message),
        PairError::Dial(DialError::Refused(PairingRefused::RateLimited { retry_after_secs })) => {
            ErrorObjectOwned::owned(
                error_code::RATE_LIMITED,
                message,
                Some(json!({ "retryAfterMs": retry_after_secs.saturating_mul(1000) })),
            )
        }
        PairError::Dial(DialError::Refused(reason)) => refused(reason.name(), message),
        PairError::Dial(_) | PairError::Unexpected(_) => error(error_code::UNAVAILABLE, message),
        PairError::Store(e) => internal(e),
    }
}

fn grant_entry(grant: GrantRecord) -> GrantEntry {
    GrantEntry {
        peer: grant.peer,
        library: grant.library,
        granted_by: grant.granted_by,
        granted_at: grant.granted_at,
    }
}

fn share_error(e: ShareError) -> ErrorObjectOwned {
    match e {
        ShareError::NoSuchPeer(_) => error(error_code::NO_SUCH_PEER, e.to_string()),
        ShareError::NoSuchLibrary(_) => error(error_code::INVALID_INPUT, e.to_string()),
        ShareError::Catalog(e) => error(error_code::UNAVAILABLE, e.to_string()),
        ShareError::Store(e) => internal(e),
    }
}

/// The live pair `id`, or `NO_SUCH_PEER`.
async fn live_peer(federation: &Federation, id: &str) -> RpcResult<PeerRecord> {
    federation
        .peer(id)
        .await
        .map_err(internal)?
        .filter(PeerRecord::live)
        .ok_or_else(|| error(error_code::NO_SUCH_PEER, "no server paired by that id"))
}

fn replica_error(e: ReplicaError) -> ErrorObjectOwned {
    match e {
        ReplicaError::NoSuchPeer(_) => error(error_code::NO_SUCH_PEER, e.to_string()),
        ReplicaError::Dial(_)
        | ReplicaError::NotPaired(_)
        | ReplicaError::TooLarge { .. }
        | ReplicaError::TooSlow(_) => error(error_code::UNAVAILABLE, e.to_string()),
        ReplicaError::Store(e) => internal(e),
    }
}

fn peer_error(e: PeerError) -> ErrorObjectOwned {
    match e {
        PeerError::NoSuchPeer(_) => error(error_code::NO_SUCH_PEER, e.to_string()),
        PeerError::KeyNotHeld => error(error_code::KEY_NOT_HELD, e.to_string()),
        PeerError::AlreadyPaired => refused(
            PairingRefused::AlreadyPaired.name(),
            "that key is another pair's",
        ),
        PeerError::Store(e) => internal(e),
    }
}

#[async_trait]
impl FederationApiServer for AdminApi {
    async fn peer_list(&self, ext: &Extensions) -> RpcResult<Vec<PeerEntry>> {
        self.admin(ext).await?;
        let federation = self.federation()?;
        let peers = federation.peers().await.map_err(internal)?;
        let mut entries = Vec::with_capacity(peers.len());
        for peer in peers {
            entries.push(self.peer_entry(federation, peer).await?);
        }
        Ok(entries)
    }

    async fn redeem(&self, ext: &Extensions, link: String) -> RpcResult<PeerEntry> {
        let (federation, by) = self.pair_change(ext).await?;
        let peer = federation
            .redeem_link(&link, &by)
            .await
            .map_err(pair_error)?;
        self.peer_entry(federation, peer).await
    }

    async fn look_up(&self, ext: &Extensions, link: String) -> RpcResult<PairLookup> {
        // As `redeem`: it dials out and spends the other server's
        // strangers' budget.
        self.recent_admin(ext).await?;
        let found = self
            .federation()?
            .look_up_link(&link)
            .await
            .map_err(pair_error)?;
        Ok(PairLookup {
            name: found.name,
            server_id: found.server_id,
            fingerprint: found.fingerprint,
            inviter: found.inviter,
            expires_at: found.expires_at,
        })
    }

    async fn peer_detail(&self, ext: &Extensions, id: String) -> RpcResult<PeerDetail> {
        self.admin(ext).await?;
        let federation = self.federation()?;
        let peer = federation
            .peer(&id)
            .await
            .map_err(internal)?
            .ok_or_else(|| error(error_code::NO_SUCH_PEER, "no server paired by that id"))?;
        let page = federation.peer_page(&peer).await.map_err(internal)?;
        Ok(PeerDetail {
            peer: self.peer_entry(federation, peer).await?,
            last_seen: page.last_seen,
            identity_refused_at: page.identity_refused_at,
            received: page
                .received
                .into_iter()
                .map(|l| ReceivedLibrary {
                    id: l.library,
                    title: l.title,
                    kind: l.kind,
                    items: l.items,
                    revision: u64::try_from(l.revision).unwrap_or_default(),
                    synced_at: l.synced_at,
                })
                .collect(),
            cached_bytes: page.cached_bytes,
            withdrawn: page
                .withdrawn
                .into_iter()
                .map(|(id, title, withdrawn_at)| WithdrawnLibrary {
                    id,
                    title,
                    withdrawn_at,
                })
                .collect(),
            week: page
                .week
                .into_iter()
                .map(|d| PlayDay {
                    day: d.day,
                    plays_here: d.here,
                    proxied_here: d.proxied_here,
                    plays_there: d.there,
                    proxied_there: d.proxied_there,
                })
                .collect(),
        })
    }

    async fn peer_revoke(&self, ext: &Extensions, id: String) -> RpcResult<()> {
        let (federation, by) = self.pair_change(ext).await?;
        match federation.unpair(&id, &by).await.map_err(internal)? {
            Some(_) => Ok(()),
            None => Err(error(
                error_code::NO_SUCH_PEER,
                "no server paired by that id",
            )),
        }
    }

    async fn key_confirm(
        &self,
        ext: &Extensions,
        id: String,
        endpoint_id: String,
    ) -> RpcResult<PeerEntry> {
        let (federation, by) = self.pair_change(ext).await?;
        let peer = federation
            .confirm_key(&id, &endpoint_id, &by)
            .await
            .map_err(peer_error)?;
        self.peer_entry(federation, peer).await
    }

    async fn key_reject(
        &self,
        ext: &Extensions,
        id: String,
        endpoint_id: String,
    ) -> RpcResult<PeerEntry> {
        let (federation, by) = self.pair_change(ext).await?;
        let peer = federation
            .reject_key(&id, &endpoint_id, &by)
            .await
            .map_err(peer_error)?;
        self.peer_entry(federation, peer).await
    }

    async fn peer_grant_list(&self, ext: &Extensions, peer: String) -> RpcResult<Vec<GrantEntry>> {
        self.admin(ext).await?;
        let federation = self.federation()?;
        let peer = live_peer(federation, &peer).await?;
        let grants = federation.grants(&peer.id).await.map_err(internal)?;
        Ok(grants.into_iter().map(grant_entry).collect())
    }

    async fn peer_grant_add(
        &self,
        ext: &Extensions,
        peer: String,
        library: String,
    ) -> RpcResult<GrantEntry> {
        let (federation, by) = self.pair_change(ext).await?;
        let library = library
            .parse()
            .map_err(|_| error(error_code::INVALID_INPUT, format!("no library {library}")))?;
        let mut grants = federation
            .share(&peer, &[library], &by)
            .await
            .map_err(share_error)?;
        Ok(grant_entry(grants.remove(0)))
    }

    async fn peer_grant_revoke(
        &self,
        ext: &Extensions,
        peer: String,
        library: String,
    ) -> RpcResult<()> {
        let (federation, by) = self.pair_change(ext).await?;
        match federation
            .unshare(&peer, &library, &by)
            .await
            .map_err(internal)?
        {
            Some(_) => Ok(()),
            None => Err(error(
                error_code::NO_SUCH_GRANT,
                "that library is not shared with that server",
            )),
        }
    }

    async fn peer_refresh(&self, ext: &Extensions, id: String) -> RpcResult<()> {
        // As `look_up`: it dials the pair, and writes the cache (F8-09).
        self.recent_admin(ext).await?;
        self.federation()?.refresh(&id).await.map_err(replica_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(e: &ErrorObjectOwned) -> serde_json::Value {
        serde_json::from_str(e.data().unwrap().get()).unwrap()
    }

    /// The protocol's refusal is named as the protocol names it.
    #[test]
    fn a_refusal_is_named_by_its_reason() {
        let e = pair_error(PairError::Dial(DialError::Refused(
            PairingRefused::InviteExpired,
        )));
        assert_eq!(e.code(), error_code::PAIRING_REFUSED);
        assert_eq!(data(&e)["reason"], "invite_expired");
        let e = pair_error(PairError::AlreadyPaired);
        assert_eq!(data(&e)["reason"], "already_paired");
        let e = pair_error(PairError::Dial(DialError::Refused(
            PairingRefused::RateLimited {
                retry_after_secs: 3,
            },
        )));
        assert_eq!(e.code(), error_code::RATE_LIMITED);
        assert_eq!(data(&e)["retryAfterMs"], 3000);
        let e = pair_error(PairError::Dial(DialError::Unreachable("x".into())));
        assert_eq!(e.code(), error_code::UNAVAILABLE);
    }
}
