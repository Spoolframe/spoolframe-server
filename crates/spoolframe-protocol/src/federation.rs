//! Server-to-server federation on `spoolframe/federation/1` (P8-01,
//! ARCHITECTURE §26–§28): pairing two servers.
//!
//! Experimental ([`crate::experimental::FEDERATION`]): a server serves the
//! ALPN only while its `experimental { federation = true; }` is on, and
//! closes it at once otherwise. These messages may change without
//! [`PROTOCOL_VERSION`] or the client protocol's version moving.
//!
//! A server's identity is its endpoint key, which the QUIC handshake
//! proves. Its [`crate::ServerId`] is a label with no key, which outlives
//! a rotated key: the server that is dialled pins the pair (endpoint id,
//! server id). Nothing here is signed; arriving on the connection is the
//! proof.
//!
//! One request per bidirectional stream, as [`crate::codec`] frames it: a
//! [`PeerRequest`], answered by a [`PeerReply`]. The first is always
//! [`PeerRequest::Hello`]. An endpoint the server does not know reaches
//! only `Hello` and [`PeerRequest::RedeemPairing`]; anything else resets
//! the stream with [`reset::NOT_ALLOWED`] and is counted against it as a
//! wrong guess at an invite code. A known server id from a new key is
//! held until an admin confirms the key ([`Standing::KeyHeld`]), and
//! reaches only `Hello`.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The version of `spoolframe/federation/1`'s messages this build speaks:
/// the ALPN versions itself (P8-01, 2026-10-22), apart from the client
/// protocol's [`crate::PROTOCOL_VERSION`].
pub const PROTOCOL_VERSION: u32 = 1;

/// The longest request frame a server reads on this ALPN. A pairing is a
/// code, a name and some address hints.
pub const MAX_REQUEST: u32 = 16 << 10;

/// The most address hints, and the most relays, a redemption carries; the
/// rest are left out, as an invite link's are ([`crate::invite::MAX_HINTS`]).
pub const MAX_HINTS: usize = crate::invite::MAX_HINTS;

/// Stream reset codes on this ALPN.
pub mod reset {
    /// The request is not one this peer may make now: a stranger's
    /// anything but `Hello` and a redemption, a held key's anything but
    /// `Hello`, or any request before `Hello`.
    pub const NOT_ALLOWED: u32 = 1;
    /// The request did not arrive in time, or did not decode.
    pub const BAD_REQUEST: u32 = 2;
}

/// The first request on every connection: who the dialling server says it
/// is. Its endpoint is proved by the connection; its server id is checked
/// against the pin for that endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerHello {
    /// [`PROTOCOL_VERSION`] of the dialling server.
    pub protocol_version: u32,
    /// Its [`crate::ServerId`], as written.
    pub server_id: String,
    /// What it calls itself (`name`): shown to the other's admins. Cut to
    /// [`crate::MAX_HELLO_TEXT`] characters.
    pub name: String,
}

impl PeerHello {
    /// This `PeerHello` with its name cut to [`crate::MAX_HELLO_TEXT`]
    /// characters: what a server keeps of it.
    pub fn within_bounds(mut self) -> Self {
        self.name = server_name(self.name);
        self
    }
}

/// A server's `name` cut to [`crate::MAX_HELLO_TEXT`] characters, as a
/// [`PeerHello`]'s is: what a server keeps of another's name, from
/// wherever it came.
pub fn server_name(mut name: String) -> String {
    crate::messages::cut(&mut name);
    name
}

/// The answer to [`PeerHello`]: the server dialled, and what the dialling
/// one is to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerInfo {
    /// [`PROTOCOL_VERSION`] of the server dialled.
    pub protocol_version: u32,
    pub server_id: String,
    pub name: String,
    pub standing: Standing,
}

/// What the dialling server is to the one it dialled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Standing {
    /// No pair: it may redeem a pairing invite, and nothing else.
    Stranger,
    /// Paired: this endpoint and server id are pinned.
    Paired,
    /// Its server id is paired with another key. The new key waits for
    /// the other's admin to confirm it, and reaches only `Hello` until
    /// then.
    KeyHeld,
    /// A standing this build does not know.
    #[serde(other)]
    Unknown,
}

/// Pair with the server dialled by redeeming its `federation` invite.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedeemPairing {
    /// The invite's code, as its link carries it; any case, dashes or
    /// spaces are taken.
    pub code: String,
    /// Addresses the dialling server may be reached at, as `ip:port`, so
    /// the other can dial it back (to say it unpaired). Hints: at most
    /// [`MAX_HINTS`] are kept, and one that does not read is left out.
    #[serde(default)]
    pub addrs: Vec<String>,
    /// URLs of relays it may be reached through; hints, as `addrs`.
    #[serde(default)]
    pub relays: Vec<String>,
}

impl fmt::Debug for RedeemPairing {
    // The code pairs a server until it is used up: keep it out of logs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RedeemPairing")
            .field("code", &"<redacted>")
            .field("addrs", &self.addrs)
            .field("relays", &self.relays)
            .finish()
    }
}

/// The pair is made: the dialled server pinned the dialling one, and
/// says who it is so the other can write its own row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paired {
    pub server_id: String,
    pub name: String,
}

/// Why no pair was made, or a peer was turned away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PairingRefused {
    /// No invite has this code, or it was revoked: counted as a wrong
    /// guess.
    InviteInvalid,
    InviteExpired,
    /// Every use it had is used.
    InviteExhausted,
    /// The code is a live invite's of another kind (an account's, a
    /// portable user's).
    WrongInviteKind,
    /// Too many wrong codes, or too many strangers asking: try again in
    /// `retry_after_secs`.
    RateLimited {
        retry_after_secs: u64,
    },
    /// This endpoint, or this server id, is paired already.
    AlreadyPaired,
    /// This endpoint is paired under another server id: refused every
    /// time, and the connection is closed with
    /// [`crate::close::SERVER_ID_CHANGED`].
    ServerIdChanged,
    /// The hello's server id is the dialled server's own: a server never
    /// pairs with itself, or with one that copied its id.
    SameServerId,
    /// A new key for a paired server id that is not held: its admin
    /// rejected it within the day, or as many new keys as are held for
    /// that server wait already.
    KeyRefused,
    /// The other server holds this one's new key until its admin
    /// confirms it: what a server tells its own admin (`PairServer`),
    /// never sent on the ALPN, where the standing says it.
    KeyHeld,
    /// A reason this build does not know.
    #[serde(other)]
    Unknown,
}

impl PairingRefused {
    /// Its name on the wire, the `type` tag: what the admin API's
    /// `PAIRING_REFUSED` carries as `data.reason`, and the web UI matches.
    pub fn name(self) -> &'static str {
        match self {
            PairingRefused::InviteInvalid => "invite_invalid",
            PairingRefused::InviteExpired => "invite_expired",
            PairingRefused::InviteExhausted => "invite_exhausted",
            PairingRefused::WrongInviteKind => "wrong_invite_kind",
            PairingRefused::RateLimited { .. } => "rate_limited",
            PairingRefused::AlreadyPaired => "already_paired",
            PairingRefused::ServerIdChanged => "server_id_changed",
            PairingRefused::SameServerId => "same_server_id",
            PairingRefused::KeyRefused => "key_refused",
            PairingRefused::KeyHeld => "key_held",
            PairingRefused::Unknown => "unknown",
        }
    }
}

impl fmt::Display for PairingRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PairingRefused::InviteInvalid => f.write_str("that invite code is not valid"),
            PairingRefused::InviteExpired => f.write_str("the invite has expired"),
            PairingRefused::InviteExhausted => f.write_str("the invite has been used up"),
            PairingRefused::WrongInviteKind => {
                f.write_str("that invite is not one to pair a server with")
            }
            PairingRefused::RateLimited { retry_after_secs } => {
                write!(f, "too many attempts; try again in {retry_after_secs} s")
            }
            PairingRefused::AlreadyPaired => f.write_str("the servers are paired already"),
            PairingRefused::ServerIdChanged => {
                f.write_str("this server is paired under another server id")
            }
            PairingRefused::SameServerId => f.write_str("that server id is this server's own"),
            PairingRefused::KeyRefused => {
                f.write_str("that new key for a paired server is not held for its admin")
            }
            PairingRefused::KeyHeld => f.write_str(
                "the other server holds this server's new key until its admin confirms it",
            ),
            PairingRefused::Unknown => f.write_str("refused for a reason this build does not know"),
        }
    }
}

/// The server sending it no longer pairs with the one it is sent to:
/// that one revokes its own row too. Best effort: a server that is not
/// told keeps its row until its admin revokes it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unpaired {}

/// An admin's `PairServer` (client protocol, P8-01): the other server's
/// `federation` invite link, with its code.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairServer {
    pub link: String,
}

impl fmt::Debug for PairServer {
    // The link carries the code: keep it out of logs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PairServer { link: <redacted> }")
    }
}

/// A server paired by `PairServer` (client protocol, P8-01), as the app
/// shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerServer {
    /// What it calls itself.
    pub name: String,
    pub server_id: String,
    /// Its `EndpointId`, hex: the key pinned.
    pub endpoint_id: String,
    /// The key's fingerprint, as the other server's admin sees it.
    pub fingerprint: String,
}

/// A request on `spoolframe/federation/1`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PeerRequest {
    /// The first request on every connection: `Info`.
    Hello(PeerHello),
    /// A stranger's only other request: `Paired`, or `Refused`.
    RedeemPairing(RedeemPairing),
    /// A paired server unpaired: `Done`, then the connection is closed
    /// with [`crate::close::UNPAIRED`].
    Unpaired(Unpaired),
    /// A request this build does not know: not allowed.
    #[serde(other)]
    Unknown,
}

/// The answer to a [`PeerRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PeerReply {
    Info(PeerInfo),
    Paired(Paired),
    Refused {
        reason: PairingRefused,
    },
    Done,
    /// A reply this build does not know.
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec;

    fn round_trip<T>(value: T)
    where
        T: Serialize + serde::de::DeserializeOwned + PartialEq + fmt::Debug,
    {
        let bytes = codec::encode(&value).unwrap();
        assert_eq!(codec::decode::<T>(&bytes).unwrap(), value);
    }

    fn hello() -> PeerHello {
        PeerHello {
            protocol_version: PROTOCOL_VERSION,
            server_id: "mzxw6ytbojtg633c".into(),
            name: "Bob's server".into(),
        }
    }

    #[test]
    fn every_request_goes_both_ways() {
        round_trip(PeerRequest::Hello(hello()));
        round_trip(PeerRequest::RedeemPairing(RedeemPairing {
            code: "QW7K-2MFP-XR4T".into(),
            addrs: vec!["203.0.113.7:7420".into()],
            relays: vec!["https://relay.example.org".into()],
        }));
        round_trip(PeerRequest::Unpaired(Unpaired {}));
    }

    #[test]
    fn every_reply_goes_both_ways() {
        for standing in [Standing::Stranger, Standing::Paired, Standing::KeyHeld] {
            round_trip(PeerReply::Info(PeerInfo {
                protocol_version: PROTOCOL_VERSION,
                server_id: "mzxw6ytbojtg633c".into(),
                name: "Alice's server".into(),
                standing,
            }));
        }
        round_trip(PeerReply::Paired(Paired {
            server_id: "mzxw6ytbojtg633c".into(),
            name: "Alice's server".into(),
        }));
        for reason in [
            PairingRefused::InviteInvalid,
            PairingRefused::InviteExpired,
            PairingRefused::InviteExhausted,
            PairingRefused::WrongInviteKind,
            PairingRefused::RateLimited {
                retry_after_secs: 30,
            },
            PairingRefused::AlreadyPaired,
            PairingRefused::ServerIdChanged,
            PairingRefused::SameServerId,
            PairingRefused::KeyRefused,
            PairingRefused::KeyHeld,
        ] {
            round_trip(PeerReply::Refused { reason });
        }
        round_trip(PeerReply::Done);
    }

    /// `name()` is the serde tag, for every reason.
    #[test]
    fn a_refusal_s_name_is_its_tag() {
        for reason in [
            PairingRefused::InviteInvalid,
            PairingRefused::InviteExpired,
            PairingRefused::InviteExhausted,
            PairingRefused::WrongInviteKind,
            PairingRefused::RateLimited {
                retry_after_secs: 30,
            },
            PairingRefused::AlreadyPaired,
            PairingRefused::ServerIdChanged,
            PairingRefused::SameServerId,
            PairingRefused::KeyRefused,
            PairingRefused::KeyHeld,
            PairingRefused::Unknown,
        ] {
            let json = serde_json::to_value(reason).unwrap();
            assert_eq!(json["type"], reason.name(), "{reason:?}");
        }
    }

    /// What a newer server adds reads as `Unknown`, never as a decode
    /// failure: a request (which the server then does not allow), a
    /// reply, a standing and a refusal.
    #[test]
    fn what_a_newer_server_sends_reads_as_unknown() {
        let decode = |value: serde_json::Value| codec::encode(&value).unwrap();
        assert_eq!(
            codec::decode::<PeerRequest>(&decode(serde_json::json!({ "type": "share_library" })))
                .unwrap(),
            PeerRequest::Unknown
        );
        assert_eq!(
            codec::decode::<PeerReply>(&decode(serde_json::json!({ "type": "catalog" }))).unwrap(),
            PeerReply::Unknown
        );
        let info = serde_json::json!({
            "type": "info",
            "protocol_version": 2,
            "server_id": "mzxw6ytbojtg633c",
            "name": "Alice's server",
            "standing": "neighbour",
            "libraries": 3,
        });
        let PeerReply::Info(info) = codec::decode::<PeerReply>(&decode(info)).unwrap() else {
            panic!("not info");
        };
        assert_eq!(info.standing, Standing::Unknown);
        let refused = serde_json::json!({ "type": "refused", "reason": { "type": "busy" } });
        assert_eq!(
            codec::decode::<PeerReply>(&decode(refused)).unwrap(),
            PeerReply::Refused {
                reason: PairingRefused::Unknown
            }
        );
    }

    /// A redemption from a server that sends no hints reads as one with
    /// none.
    #[test]
    fn a_redemption_without_hints_has_none() {
        let sent = serde_json::json!({ "type": "redeem_pairing", "code": "QW7K2MFPXR4T" });
        let PeerRequest::RedeemPairing(redeem) =
            codec::decode(&codec::encode(&sent).unwrap()).unwrap()
        else {
            panic!("not a redemption");
        };
        assert!(redeem.addrs.is_empty() && redeem.relays.is_empty());
    }

    /// The client protocol's request and reply (P8-01).
    #[test]
    fn pairing_from_an_app_goes_both_ways_and_keeps_the_link_out_of_debug() {
        use crate::{Error, Reply, Request, Response};
        let request = Request::PairServer(PairServer {
            link: "spoolframe://invite?server=x&kind=federation&cap=QW7K-2MFP-XR4T".into(),
        });
        round_trip(request.clone());
        assert!(!format!("{request:?}").contains("QW7K"));
        round_trip(Response::Ok(Reply::ServerPaired {
            server: PeerServer {
                name: "Alice's server".into(),
                server_id: "mzxw6ytbojtg633c".into(),
                endpoint_id: "ab".repeat(32),
                fingerprint: "PGJA-BHZ1-F68M-DFSE".into(),
            },
        }));
        round_trip(Response::Err(Error::PairingRefused {
            reason: PairingRefused::AlreadyPaired,
        }));
    }

    #[test]
    fn the_code_stays_out_of_debug_output() {
        let redeem = RedeemPairing {
            code: "QW7K-2MFP-XR4T".into(),
            addrs: Vec::new(),
            relays: Vec::new(),
        };
        assert!(!format!("{redeem:?}").contains("QW7K"));
    }

    #[test]
    fn a_long_name_is_cut_as_a_device_s_is() {
        let long = PeerHello {
            name: "x".repeat(500),
            ..hello()
        };
        assert_eq!(
            long.within_bounds().name.chars().count(),
            crate::MAX_HELLO_TEXT
        );
    }
}
