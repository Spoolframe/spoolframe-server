//! Users and devices › Federation (P8-01): the servers paired with this
//! one, pairing with another by its invite link, unpairing, and the new
//! keys held for a paired server until the admin confirms one; the
//! libraries shared with each (P8-03), and syncing what each shares with
//! this one (P8-02). Experimental: every method is
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
    /// until it expires, a day after. At most four held, the rejected
    /// apart: a key past them is not held.
    pub held_keys: Vec<PeerKey>,
    /// Whether it has a federation connection open to this server now.
    pub connected: bool,
    /// When this server last synced what that one shares with it (P8-02),
    /// Unix seconds: its latest whole sync of a library. `None` before the
    /// first, or while it shares nothing with this server.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub last_sync: Option<i64>,
    /// Plays of this server's items by that server's users (P8-03),
    /// counted as this server minted their play tokens: one number for the
    /// server. Admins manage servers, not people: no user of it is named.
    pub plays_here: u64,
    /// Plays of that server's items by this server's users, counted as
    /// this server vouched for them.
    pub plays_there: u64,
    /// That server's users' plays this server refused to mint a token
    /// for: an item not shared, a user or device that does not read.
    pub refused_here: u64,
    /// This server's users' plays that server refused.
    pub refused_there: u64,
    /// Of `plays_here`, those the device played from this server itself
    /// (P8-04).
    #[serde(default)]
    pub direct_here: u64,
    /// Of `plays_here`, those that server played from this one in its own
    /// name and on to its device, the device not reaching this server
    /// (P8-04): a pair whose plays all go this way is seen, not silent.
    #[serde(default)]
    pub proxied_here: u64,
    /// This server's users' plays of that server's items that this server
    /// played on to their devices (P8-04): its own uplink carried them.
    #[serde(default)]
    pub proxied_there: u64,
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

/// A library this server shares with a paired server (P8-03): every user
/// of that server may browse and watch it. Grants are server-wide; there
/// is none per remote user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrantEntry {
    /// The paired server's `id`, as `peerList` gives it.
    pub peer: String,
    /// The library's id, as `libraryList` gives it.
    pub library: String,
    /// Who shared it: an admin's username, or `server CLI`.
    pub granted_by: String,
    /// Unix seconds.
    pub granted_at: i64,
}

/// What `lookUp` found of a `federation` invite before it is redeemed
/// (P8-01): the server that made it, and the fingerprint of the key the
/// link names, for the two admins to compare.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairLookup {
    /// What the server calls itself.
    pub name: String,
    pub server_id: String,
    /// Of the key this server dials and would pin.
    pub fingerprint: String,
    /// The display name of the admin who made the invite; `None` when
    /// they have none, or the server's CLI made it.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub inviter: Option<String>,
    /// When the invite expires, Unix seconds.
    pub expires_at: i64,
}

/// A paired server's own page (`peerDetail`): its row, and what the list
/// does not show (P8-01–P8-04).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerDetail {
    pub peer: PeerEntry,
    /// When it was last seen connected, Unix seconds, kept to the
    /// minute: while it is offline, offline since then. `None` if never.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub last_seen: Option<i64>,
    /// When its key last said hello under another server id, and was
    /// refused (as it is every time); `None` if never. Unpairing is the
    /// only way on.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub identity_refused_at: Option<i64>,
    /// What it shares with this server, as kept here (P8-02).
    pub received: Vec<ReceivedLibrary>,
    /// Its artwork kept here, in bytes.
    pub cached_bytes: u64,
    /// The libraries it stopped sharing in the last week, the latest
    /// first: dropped from users' apps, their progress kept.
    pub withdrawn: Vec<WithdrawnLibrary>,
    /// The last seven days' plays across the pair, the oldest first,
    /// today last.
    pub week: Vec<PlayDay>,
}

/// A library a paired server shares with this one, as kept here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceivedLibrary {
    /// Its id on that server.
    pub id: String,
    pub title: String,
    /// `movies`, `shows` or `music`.
    pub kind: String,
    /// Its films, shows or artists kept here.
    pub items: u64,
    /// How many whole syncs brought a change.
    pub revision: u64,
    /// The last whole sync, Unix seconds; `None` before the first.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub synced_at: Option<i64>,
}

/// A library a paired server stopped sharing with this one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WithdrawnLibrary {
    pub id: String,
    pub title: String,
    /// Unix seconds.
    pub withdrawn_at: i64,
}

/// A day's plays across a pair: one number per server, never per user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayDay {
    /// Its first second, Unix seconds (UTC).
    pub day: i64,
    /// Plays of this server's items by that server's users, as
    /// `playsHere` counts them; of them, those played through that server.
    pub plays_here: u64,
    pub proxied_here: u64,
    /// Plays of that server's items by this server's users; of them,
    /// those this server played on to their devices.
    pub plays_there: u64,
    pub proxied_there: u64,
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
    /// `already_paired`, `key_held`, `key_refused`, `server_id_changed`,
    /// `same_server_id`); `RATE_LIMITED`, with `data.retryAfterMs`, when
    /// it refuses as rate limited.
    #[method(name = "redeem", with_extensions)]
    async fn redeem(&self, link: String) -> jsonrpsee::core::RpcResult<PeerEntry>;

    /// Look `link`, another server's `federation` invite, up without
    /// redeeming it: dial that server, hear its name, and ask it whether
    /// the code still works, as an app looks an invite up. What the admin
    /// checks before `redeem`. Errors as for `redeem`; an invite used,
    /// expired or revoked is `PAIRING_REFUSED`, as redeeming it would be.
    #[method(name = "lookUp", with_extensions)]
    async fn look_up(&self, link: String) -> jsonrpsee::core::RpcResult<PairLookup>;

    /// Server `id`'s own page: its row, when it was last online, what it
    /// shares with this server, what it stopped sharing, and the week's
    /// plays. Live or unpaired. `NO_SUCH_PEER` for an id never paired.
    #[method(name = "peerDetail", with_extensions)]
    async fn peer_detail(&self, id: String) -> jsonrpsee::core::RpcResult<PeerDetail>;

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

    /// The libraries shared with paired server `peer` now (P8-03), the
    /// first shared first. `NO_SUCH_PEER` for one that is not paired.
    #[method(name = "grantList", with_extensions)]
    async fn peer_grant_list(&self, peer: String) -> jsonrpsee::core::RpcResult<Vec<GrantEntry>>;

    /// Share `library`, one of this server's, with paired server `peer`:
    /// its users may browse and watch it from now on. Shared already, the
    /// grant is kept as it was. The grant. `NO_SUCH_PEER` for a server
    /// that is not paired; `INVALID_INPUT` for a library this server does
    /// not serve.
    #[method(name = "grantAdd", with_extensions)]
    async fn peer_grant_add(
        &self,
        peer: String,
        library: String,
    ) -> jsonrpsee::core::RpcResult<GrantEntry>;

    /// Stop sharing `library` with paired server `peer`. Its cached copy
    /// on that server goes at that server's next sync. `NO_SUCH_GRANT`
    /// for a library not shared with it.
    #[method(name = "grantRevoke", with_extensions)]
    async fn peer_grant_revoke(
        &self,
        peer: String,
        library: String,
    ) -> jsonrpsee::core::RpcResult<()>;

    /// Sync now what paired server `id` shares with this one (P8-02), as
    /// the replica does on connecting and every
    /// `federation.sync_interval_secs`: dial it if it is offline, drop
    /// the libraries it no longer shares, and sync the others. Admins
    /// only, signed in recently, as for any change of a pair's
    /// (`RECENT_SIGN_IN_REQUIRED`). `NO_SUCH_PEER` for one that is not paired; `UNAVAILABLE` when
    /// it cannot be reached, or does not pair with this server now.
    #[method(name = "refresh", with_extensions)]
    async fn peer_refresh(&self, id: String) -> jsonrpsee::core::RpcResult<()>;
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
            last_sync: None,
            plays_here: 0,
            plays_there: 0,
            refused_here: 0,
            refused_there: 0,
            direct_here: 0,
            proxied_here: 0,
            proxied_there: 0,
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

    #[test]
    fn a_grant_goes_both_ways_in_camel_case() {
        let grant = GrantEntry {
            peer: "id".into(),
            library: "plex:1".into(),
            granted_by: "alice".into(),
            granted_at: 7,
        };
        let json = serde_json::to_value(&grant).unwrap();
        assert_eq!(json["grantedBy"], "alice");
        assert_eq!(serde_json::from_value::<GrantEntry>(json).unwrap(), grant);
    }

    #[test]
    fn a_peer_s_page_and_a_lookup_go_both_ways_in_camel_case() {
        let detail = PeerDetail {
            peer: entry(),
            last_seen: Some(5),
            identity_refused_at: None,
            received: vec![ReceivedLibrary {
                id: "plex:1".into(),
                title: "Movies".into(),
                kind: "movies".into(),
                items: 412,
                revision: 3,
                synced_at: None,
            }],
            cached_bytes: 1 << 30,
            withdrawn: vec![WithdrawnLibrary {
                id: "plex:2".into(),
                title: "TV Shows".into(),
                withdrawn_at: 4,
            }],
            week: vec![PlayDay {
                day: 86_400,
                plays_here: 44,
                proxied_here: 3,
                plays_there: 17,
                proxied_there: 0,
            }],
        };
        let json = serde_json::to_value(&detail).unwrap();
        assert_eq!(json["received"][0]["syncedAt"], serde_json::Value::Null);
        assert_eq!(json["week"][0]["proxiedHere"], 3);
        assert_eq!(json["withdrawn"][0]["withdrawnAt"], 4);
        assert!(json["identityRefusedAt"].is_null());
        assert_eq!(serde_json::from_value::<PeerDetail>(json).unwrap(), detail);
        let found = PairLookup {
            name: "Bob's Media".into(),
            server_id: "mzxw6ytbojtg633c".into(),
            fingerprint: "M4QX-8KDT-2WNF-7RKD".into(),
            inviter: None,
            expires_at: 9,
        };
        let json = serde_json::to_value(&found).unwrap();
        assert_eq!(json["expiresAt"], 9);
        assert_eq!(serde_json::from_value::<PairLookup>(json).unwrap(), found);
    }

    /// Every field is sent, `None`s too, as everywhere on the admin API.
    #[test]
    fn an_optional_field_left_out_is_refused() {
        let mut json = serde_json::to_value(entry()).unwrap();
        json.as_object_mut().unwrap().remove("revokedAt");
        assert!(serde_json::from_value::<PeerEntry>(json).is_err());
    }
}
