//! Users and devices › Federation (P8-01): the servers paired with this
//! one, pairing with another by its invite link, unpairing, and the new
//! keys held for a paired server until the admin confirms one. Experimental: every method is
//! `NOT_CONFIGURED` while `experimental.federation` is off.
//!
//! Admins only (`FORBIDDEN`). What changes a pair needs a recent sign-in
//! (`RECENT_SIGN_IN_REQUIRED`), as `inviteCreate` does; listing does not.

use serde::{Deserialize, Serialize};

/// A server paired with this one, or once paired.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerEntry {
    /// What `peerRevoke`, `keyConfirm` and `keyReject` take.
    pub id: String,
    /// What it called itself when the pair was made.
    pub name: String,
    /// Its `ServerId`.
    pub server_id: String,
    /// Its `EndpointId`, hex: the key pinned.
    pub endpoint_id: String,
    /// The key's fingerprint, to compare with what the other server's
    /// admin sees as this server's (`serverStatus`).
    pub fingerprint: String,
    /// The admin who paired it here: who made the invite on the server
    /// whose invite it was, who redeemed it on the other.
    pub paired_by: String,
    /// The invite of this server's it redeemed; `None` where this server
    /// redeemed the other's.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub invite_id: Option<String>,
    /// Unix seconds.
    pub paired_at: i64,
    /// When it was unpaired, from either side; `None` while paired.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub revoked_at: Option<i64>,
    /// New keys that say they are this server, first seen first: those
    /// held wait for `keyConfirm` or `keyReject`; a rejected one is shown
    /// until it expires, a day after. At most four, the rejected among
    /// them: a key past them is not held.
    pub held_keys: Vec<PeerKey>,
    /// Whether it has a federation connection open to this server now.
    pub connected: bool,
}

/// A key held for a paired server (P8-01's key rotation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerKey {
    /// Its `EndpointId`, hex: what `keyConfirm` and `keyReject` name.
    pub endpoint_id: String,
    pub fingerprint: String,
    /// When it first and last said it is the paired server, Unix
    /// seconds. The last is kept to the hour; a key not seen for a week
    /// is forgotten.
    pub first_seen: i64,
    pub last_seen: i64,
    /// When an admin rejected it; `None` while it is held.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub rejected_at: Option<i64>,
}

/// Federation's admin methods: `federation_peerList` and the rest.
/// Only with `client` or `server`: the macro makes one or both.
#[cfg(any(feature = "client", feature = "server"))]
#[cfg_attr(
    all(feature = "client", feature = "server"),
    jsonrpsee::proc_macros::rpc(client, server, namespace = "federation")
)]
#[cfg_attr(
    all(feature = "client", not(feature = "server")),
    jsonrpsee::proc_macros::rpc(client, namespace = "federation")
)]
#[cfg_attr(
    all(feature = "server", not(feature = "client")),
    jsonrpsee::proc_macros::rpc(server, namespace = "federation")
)]
pub trait FederationApi {
    /// Every server paired with this one, the paired first, then those
    /// unpaired, each newest first.
    #[method(name = "peerList", with_extensions)]
    async fn peer_list(&self) -> jsonrpsee::core::RpcResult<Vec<PeerEntry>>;

    /// Pair with the server whose `federation` invite `link` is: dial it,
    /// redeem the code, and keep the pair. The pair as kept.
    /// `INVALID_INPUT` for a link that is not one, has no code, or is this
    /// server's own; `UNAVAILABLE` when the other server cannot be
    /// reached; `PAIRING_REFUSED` when it refuses or the two are paired
    /// already, `data.reason` saying why (`invite_invalid`,
    /// `invite_expired`, `invite_exhausted`, `wrong_invite_kind`,
    /// `rate_limited`, `already_paired`, `key_held`).
    #[method(name = "redeem", with_extensions)]
    async fn redeem(&self, link: String) -> jsonrpsee::core::RpcResult<PeerEntry>;

    /// Unpair server `id`: its row is kept, unpaired, its connections are
    /// closed, and it is told (best effort: it revokes its own side when
    /// it hears). `NO_SUCH_PEER` for one that is not paired.
    #[method(name = "peerRevoke", with_extensions)]
    async fn peer_revoke(&self, id: String) -> jsonrpsee::core::RpcResult<()>;

    /// Accept `endpointId`, a key held for paired server `id`: the pair
    /// moves to it, with all that hangs on it, and its other held keys are
    /// forgotten. `NO_SUCH_PEER` for one that is not paired;
    /// `KEY_NOT_HELD`, changing nothing, for a key not held for it: the
    /// admin confirms the key they saw, never one that came after.
    #[method(name = "keyConfirm", with_extensions)]
    async fn key_confirm(
        &self,
        id: String,
        endpoint_id: String,
    ) -> jsonrpsee::core::RpcResult<PeerEntry>;

    /// Refuse `endpointId`, a key held for paired server `id`: it reaches
    /// nothing, and is refused, not held again, for a day. Errors as for
    /// `keyConfirm`.
    #[method(name = "keyReject", with_extensions)]
    async fn key_reject(
        &self,
        id: String,
        endpoint_id: String,
    ) -> jsonrpsee::core::RpcResult<PeerEntry>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> PeerEntry {
        PeerEntry {
            id: "id".into(),
            name: "Alice's server".into(),
            server_id: "mzxw6ytbojtg633c".into(),
            endpoint_id: "ab".repeat(32),
            fingerprint: "PGJA-BHZ1-F68M-DFSE".into(),
            paired_by: "alice".into(),
            invite_id: None,
            paired_at: 1,
            revoked_at: None,
            held_keys: Vec::new(),
            connected: false,
        }
    }

    #[test]
    fn an_entry_goes_both_ways_in_camel_case() {
        let held = PeerEntry {
            held_keys: vec![PeerKey {
                endpoint_id: "cd".repeat(32),
                fingerprint: "QAJ7-XQG9-2MDW-AN9H".into(),
                first_seen: 2,
                last_seen: 3,
                rejected_at: None,
            }],
            ..entry()
        };
        let json = serde_json::to_value(&held).unwrap();
        assert!(json["heldKeys"][0].get("lastSeen").is_some(), "{json}");
        assert!(json["heldKeys"][0]["rejectedAt"].is_null(), "{json}");
        assert!(json.get("revokedAt").unwrap().is_null(), "{json}");
        assert_eq!(serde_json::from_value::<PeerEntry>(json).unwrap(), held);
    }

    /// Every field is sent, `None`s too, as everywhere on the admin API.
    #[test]
    fn an_optional_field_left_out_is_refused() {
        let mut json = serde_json::to_value(entry()).unwrap();
        json.as_object_mut().unwrap().remove("revokedAt");
        assert!(serde_json::from_value::<PeerEntry>(json).is_err());
    }
}
