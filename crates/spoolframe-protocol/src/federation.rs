//! Server-to-server federation on `spoolframe/federation/1`: pairing two
//! servers, and replicating what one shares with the other
//! ([`SyncLibrary`] and the rest).
//!
//! An app does not speak this ALPN. It sees a paired server's libraries
//! and items through its own server, as `fed:` ids. The client protocol's
//! types for playing them directly are here too: [`RemoteHints`],
//! [`RemoteGranted`], [`RedeemPlayToken`] and [`PlayTokenRedeemed`].
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
//! reaches only `Hello`. Only a live pair reaches replication's requests.

mod assertion;
mod media;
mod replication;
mod subtitles;

pub use assertion::*;
pub use media::*;
pub use replication::*;
pub use subtitles::*;

use std::fmt;

use serde::{Deserialize, Serialize};

/// The version of `spoolframe/federation/1`'s messages this build speaks:
/// the ALPN versions itself, apart from the client protocol's
/// [`crate::PROTOCOL_VERSION`].
pub const PROTOCOL_VERSION: u32 = 1;

/// The least time between two syncs a pair's [`PeerRequest::SharesChanged`]
/// sets off: a pair that sends notices without end makes the receiver
/// sync it at most this often.
pub const SHARES_CHANGED_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

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
    /// The connection has as many streams open as it may: wait for one
    /// to end, and ask again.
    pub const BUSY: u32 = 3;
    /// The asker took none of the reply within its own wait for it: the
    /// reply is given up on, and what was not read is lost.
    pub const STALLED: u32 = 4;
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

/// Whether `request` names a paired server's item, library or image
/// anywhere, in any field (a `fed:` id): what a server refuses from
/// a client that did not name the experimental `federation` feature, at
/// one boundary rather than in each request's handler. A
/// [`crate::Request`], or a resource's request, as an image's is. Text
/// that only starts like one, a search's or a name, is not an id.
pub fn names_federated<T: Serialize + ?Sized>(request: &T) -> bool {
    fn names(value: &ciborium::Value) -> bool {
        match value {
            ciborium::Value::Text(text) => federated_id(text),
            ciborium::Value::Array(values) => values.iter().any(names),
            ciborium::Value::Map(entries) => entries.iter().any(|(k, v)| names(k) || names(v)),
            ciborium::Value::Tag(_, value) => names(value),
            _ => false,
        }
    }
    ciborium::Value::serialized(request).is_ok_and(|value| names(&value))
}

/// Whether `text` is a `fed:` id of an item, a library or an image.
pub fn federated_id(text: &str) -> bool {
    use spoolframe_types::{ArtworkId, LibraryId, MediaId};
    text.starts_with("fed:")
        && (text
            .parse::<MediaId>()
            .is_ok_and(|id| id.remote().is_some())
            || text
                .parse::<LibraryId>()
                .is_ok_and(|id| id.remote().is_some())
            || text
                .parse::<ArtworkId>()
                .is_ok_and(|id| id.remote().is_some()))
}

/// A server's `name` cut to [`crate::MAX_HELLO_TEXT`] characters, as a
/// [`PeerHello`]'s is: what a server keeps of another's name, from
/// wherever it came. Control characters go first: the name is logged,
/// and a newline in it would forge a line of the log.
pub fn server_name(mut name: String) -> String {
    name.retain(|c| !c.is_control());
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
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

impl PairingRefused {
    /// What to show: the same text as its `Display`.
    pub fn message(&self) -> String {
        self.to_string()
    }
}

/// [`PairingRefused::message`], for Swift and Kotlin. A free function, not
/// a method: UniFFI 0.32's Kotlin lowers a method's receiver by the type's
/// Rust name, which the Kotlin rename (`PairingRefusal`) leaves undefined.
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn pairing_refused_message(refused: PairingRefused) -> String {
    refused.message()
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

/// An admin's `PairServer` (client protocol): the other server's
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

/// The most libraries one `ShareLibraries` names; the rest are left out.
pub const MAX_SHARED: usize = 64;

/// An admin's `ShareLibraries` (client protocol): share libraries
/// of this server with a paired one, as the QR flow offers once the pair
/// is made. Every user of the paired server may browse and watch them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareLibraries {
    /// The paired server's [`crate::ServerId`], as `ServerPaired` gave it.
    pub server_id: String,
    /// At most [`MAX_SHARED`]; the rest are left out.
    pub libraries: Vec<spoolframe_types::LibraryId>,
}

impl ShareLibraries {
    /// This request with at most [`MAX_SHARED`] libraries: what a server
    /// reads of it.
    pub fn within_bounds(mut self) -> Self {
        self.libraries.truncate(MAX_SHARED);
        self
    }
}

/// A server paired by `PairServer` (client protocol), as the app
/// shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
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
    /// A pair's: the libraries shared with it now: `Libraries`.
    Libraries,
    /// A pair's: what the sender shares with the server it is sent to
    /// changed (a grant added or revoked), so that one syncs the sender
    /// soon rather than at its next interval. It carries nothing: the
    /// receiver asks `Libraries` and `Sync` as ever, at most once per
    /// [`SHARES_CHANGED_EVERY`] per pair, a notice within it waiting
    /// for its end and coalesced with any others. `Done`. Sent best
    /// effort, on the connection the sender holds to the pair: a pair
    /// not connected is not told, and syncs on connecting. A server that
    /// does not know it resets the stream `NOT_ALLOWED`, which changes
    /// nothing.
    SharesChanged,
    /// A pair's: one shared library's changes: `Page`s on the
    /// same stream, the last with `more: false`; `NotShared` for a
    /// library that is not shared with it, `Unavailable` when it cannot
    /// be walked now.
    Sync(SyncLibrary),
    /// A pair's: one image of an item in a library shared with it:
    /// `Artwork`, `NotShared` or `NotFound`, written as a reply
    /// ([`crate::codec::write_reply`]), not a frame.
    Artwork(ArtworkFetch),
    /// A pair's: it vouches for one of its users, who wants an item of
    /// this server's: `Token`, or `AssertionRefused`.
    AssertUser(AssertUser),
    /// A pair's: one of its users searches subtitles for an item of this
    /// server's: `SubtitleResults`, or `Failed`.
    SearchSubtitles(SubtitleSearchAsk),
    /// A pair's: one of its users downloads a result, counted
    /// under the pair: `SubtitleDownloaded`, or `Failed`.
    DownloadSubtitle(SubtitleDownloadAsk),
    /// A pair's: one of its users removes a downloaded subtitle:
    /// `Done`, or `Failed`.
    RemoveSubtitleDownload(SubtitleRemovalAsk),
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
    /// The libraries shared with the pair, at most
    /// [`MAX_SHARED_LIBRARIES`].
    Libraries {
        libraries: Vec<SharedLibrary>,
        /// What the answering server calls itself now, as its
        /// [`PeerInfo`]'s `name`: so a pair learns of a rename at its next
        /// sync, without dialling again. Absent from a server before
        /// 0.16.3.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// A page of a [`SyncLibrary`]'s answer.
    Page(SyncPage),
    /// An image: at most [`MAX_ARTWORK_BYTES`].
    Artwork {
        /// Its media type, as `image/jpeg`.
        content_type: String,
        #[serde(with = "serde_bytes")]
        bytes: Vec<u8>,
    },
    /// The library, or the item the image is of, is not shared with the
    /// pair (now, or any more): a cache of it goes.
    NotShared,
    /// Shared, but the item or image is not there now.
    NotFound,
    /// The sharing server cannot answer now (its catalog is down): the
    /// asker keeps what it has and asks again later.
    Unavailable,
    /// The token for an [`AssertUser`].
    Token(TokenGranted),
    /// No token for an [`AssertUser`], and why.
    AssertionRefused {
        reason: AssertionRefused,
    },
    /// A subtitle search's results.
    SubtitleResults {
        search: spoolframe_types::SubtitleSearch,
    },
    /// A subtitle downloaded.
    SubtitleDownloaded {
        downloaded: spoolframe_types::SubtitleDownloaded,
    },
    /// A subtitle ask refused, as the client protocol would refuse
    /// it: the home server passes it on to its device as it is.
    Failed {
        error: crate::Error,
    },
    /// A reply this build does not know.
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{self, round_trip};

    /// A request naming a paired server's item anywhere names one; a
    /// search for text that only starts like an id does not.
    #[test]
    fn a_fed_id_is_found_in_any_field_of_a_request() {
        let alien: spoolframe_types::MediaId = "fed:movie:mzxw6ytbojtg633c:plex:1".parse().unwrap();
        assert!(names_federated(&crate::Request::Item { id: alien.clone() }));
        assert!(names_federated(&crate::Request::UpdateWatchProgress {
            id: alien,
            position_ms: 0,
        }));
        assert!(!names_federated(&crate::Request::Item {
            id: "plex:movie:1".parse().unwrap(),
        }));
        assert!(!names_federated(&crate::Request::SetServerRow {
            server_id: "fed:nothing".into(),
            shown: true,
        }));
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

    fn item() -> RemoteItem {
        RemoteItem {
            id: "plex:track:33".into(),
            kind: RemoteKind::Track,
            title: "Digital Love".into(),
            parent: Some("plex:album:31".into()),
            grandparent: Some("plex:artist:30".into()),
            parent_title: Some("Discovery".into()),
            grandparent_title: Some("Daft Punk".into()),
            index: Some(3),
            parent_index: Some(1),
            year: Some(2001),
            date: Some("2001-03-12".into()),
            summary: None,
            content_rating: None,
            duration_ms: Some(301_000),
            count: Some(14),
            genres: vec!["Electronic".into()],
            artwork: vec![RemoteArtwork {
                id: "plex:/library/metadata/31/thumb/9".into(),
                kind: RemoteArtworkKind::Poster,
            }],
            external_ids: vec!["mbid://5e4b7b2c".into()],
            sort_title: None,
            released: None,
            added_at: None,
            rating_tenths: None,
            audience_rating_tenths: None,
            size: None,
            resolution: None,
            dynamic_range: None,
            hash: String::new(),
        }
        .hashed()
    }

    /// P8-02's requests and replies, through CBOR and back.
    #[test]
    fn replication_goes_both_ways() {
        round_trip(PeerRequest::Libraries);
        round_trip(PeerRequest::SharesChanged);
        round_trip(PeerRequest::Sync(SyncLibrary {
            library: "plex:2".into(),
            since: None,
        }));
        round_trip(PeerRequest::Sync(SyncLibrary {
            library: "plex:2".into(),
            since: Some("0192f1c4".into()),
        }));
        round_trip(PeerRequest::Artwork(ArtworkFetch {
            id: "plex:/library/metadata/31/thumb/9".into(),
        }));
        round_trip(PeerReply::Libraries {
            libraries: vec![SharedLibrary {
                id: "plex:2".into(),
                title: "Music".into(),
                kind: RemoteLibraryKind::Music,
            }],
            name: Some("Bob's server".into()),
        });
        for mode in [SyncMode::Changes, SyncMode::Snapshot] {
            round_trip(PeerReply::Page(SyncPage {
                mode,
                cursor: "0192f1c4".into(),
                items: vec![item()],
                removed: vec!["plex:track:32".into()],
                more: true,
            }));
        }
        round_trip(PeerReply::Artwork {
            content_type: "image/png".into(),
            bytes: vec![0x89, b'P', b'N', b'G'],
        });
        round_trip(PeerReply::NotShared);
        round_trip(PeerReply::NotFound);
        round_trip(PeerReply::Unavailable);
    }

    /// The hash is of the content: it moves when the item does, not when
    /// the hash field itself is set.
    #[test]
    fn an_item_s_hash_follows_its_content() {
        let a = item();
        assert_eq!(a.digest(), a.hash);
        assert_eq!(a.clone().hashed(), a);
        let renamed = RemoteItem {
            title: "Something About Us".into(),
            ..a.clone()
        };
        assert_ne!(renamed.digest(), a.hash);
    }

    /// What a library is sorted and filtered by is sent only when there
    /// is some: an item without it encodes with the fields it had before
    /// them, so its hash is as an older server makes it and nothing is
    /// sent again for them alone; one with them crosses whole.
    #[test]
    fn the_browse_facts_are_sent_only_when_known() {
        let bytes = crate::codec::encode(&item()).unwrap();
        let value: ciborium::Value = ciborium::from_reader(bytes.as_slice()).unwrap();
        let keys: Vec<String> = value
            .as_map()
            .unwrap()
            .iter()
            .filter_map(|(k, _)| k.as_text().map(str::to_owned))
            .collect();
        assert_eq!(
            keys,
            [
                "id",
                "kind",
                "title",
                "parent",
                "grandparent",
                "parent_title",
                "grandparent_title",
                "index",
                "parent_index",
                "year",
                "date",
                "summary",
                "content_rating",
                "duration_ms",
                "count",
                "genres",
                "artwork",
                "external_ids",
                "hash",
            ]
        );
        let known = RemoteItem {
            sort_title: Some("Something".into()),
            released: Some("2001-03-12".into()),
            added_at: Some(1_700_000_000),
            rating_tenths: Some(87),
            audience_rating_tenths: Some(91),
            size: Some(30 << 30),
            resolution: Some(spoolframe_types::Resolution::FullHd),
            dynamic_range: Some(spoolframe_types::DynamicRange::DolbyVision),
            ..item()
        }
        .hashed();
        assert_ne!(known.hash, item().hash);
        round_trip(PeerReply::Page(SyncPage {
            mode: SyncMode::Snapshot,
            cursor: "c".into(),
            items: vec![known],
            removed: Vec::new(),
            more: false,
        }));
    }

    /// A page past the bounds is cut to them as it is read: items,
    /// removed ids, and each item's lists and texts.
    #[test]
    fn a_page_is_cut_to_its_bounds() {
        let long = RemoteItem {
            title: "x".repeat(MAX_ITEM_TEXT + 10),
            summary: Some("y".repeat(MAX_ITEM_SUMMARY + 10)),
            genres: vec!["g".into(); MAX_ITEM_GENRES + 1],
            artwork: vec![item().artwork[0].clone(); MAX_ITEM_ARTWORK + 1],
            external_ids: vec!["tmdb://1".into(); MAX_ITEM_EXTERNAL_IDS + 1],
            ..item()
        };
        let page = SyncPage {
            mode: SyncMode::Snapshot,
            cursor: "c".into(),
            items: vec![long; MAX_PAGE_ITEMS + 1],
            removed: vec!["plex:movie:1".into(); MAX_PAGE_REMOVED + 1],
            more: false,
        }
        .within_bounds();
        assert_eq!(page.items.len(), MAX_PAGE_ITEMS);
        assert_eq!(page.removed.len(), MAX_PAGE_REMOVED);
        let item = &page.items[0];
        assert_eq!(item.title.chars().count(), MAX_ITEM_TEXT);
        assert_eq!(
            item.summary.as_ref().unwrap().chars().count(),
            MAX_ITEM_SUMMARY
        );
        assert_eq!(item.genres.len(), MAX_ITEM_GENRES);
        assert_eq!(item.artwork.len(), MAX_ITEM_ARTWORK);
        assert_eq!(item.external_ids.len(), MAX_ITEM_EXTERNAL_IDS);
        let listed = libraries_within_bounds(vec![
            SharedLibrary {
                id: "plex:1".into(),
                title: "Movies".into(),
                kind: RemoteLibraryKind::Movies,
            };
            MAX_SHARED_LIBRARIES + 1
        ]);
        assert_eq!(listed.len(), MAX_SHARED_LIBRARIES);
    }

    /// What a newer sharing server adds reads as `Unknown`, item by item,
    /// never failing the page: an item kind, an artwork role, a library
    /// kind, a sync mode.
    #[test]
    fn what_a_newer_server_replicates_reads_as_unknown() {
        let decode = |value: serde_json::Value| codec::encode(&value).unwrap();
        let page = serde_json::json!({
            "type": "page",
            "mode": "patch",
            "cursor": "c",
            "items": [{
                "id": "plex:clip:1",
                "kind": "clip",
                "title": "Trailer",
                "artwork": [{ "id": "a", "kind": "banner" }],
                "hash": "h",
                "rating": 7,
            }],
            "more": false,
        });
        let PeerReply::Page(page) = codec::decode::<PeerReply>(&decode(page)).unwrap() else {
            panic!("not a page");
        };
        assert_eq!(page.mode, SyncMode::Unknown);
        assert_eq!(page.items[0].kind, RemoteKind::Unknown);
        assert_eq!(page.items[0].artwork[0].kind, RemoteArtworkKind::Unknown);
        let libraries = serde_json::json!({
            "type": "libraries",
            "libraries": [{ "id": "plex:4", "title": "Photos", "kind": "photos" }],
        });
        let PeerReply::Libraries { libraries, name } =
            codec::decode::<PeerReply>(&decode(libraries)).unwrap()
        else {
            panic!("not libraries");
        };
        assert_eq!(libraries[0].kind, RemoteLibraryKind::Unknown);
        assert_eq!(name, None);
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

    /// A grant's notice is a tag and nothing else: what an older server
    /// reads as a request it does not know, and refuses.
    #[test]
    fn a_notice_of_shares_changed_is_its_tag_alone() {
        let wire = codec::encode(&serde_json::json!({ "type": "shares_changed" })).unwrap();
        assert_eq!(
            codec::decode::<PeerRequest>(&wire).unwrap(),
            PeerRequest::SharesChanged
        );
        assert_eq!(codec::encode(&PeerRequest::SharesChanged).unwrap(), wire);
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

    /// The client protocol's share request and reply (P8-03), and its
    /// bound.
    #[test]
    fn sharing_from_an_app_goes_both_ways_and_is_bounded() {
        use crate::{Reply, Request, Response};
        let library: spoolframe_types::LibraryId = "plex:1".parse().unwrap();
        let share = ShareLibraries {
            server_id: "mzxw6ytbojtg633c".into(),
            libraries: vec![library.clone(); MAX_SHARED + 3],
        };
        round_trip(Request::ShareLibraries(share.clone()));
        round_trip(Response::Ok(Reply::LibrariesShared {
            libraries: vec![library],
        }));
        assert_eq!(share.within_bounds().libraries.len(), MAX_SHARED);
    }

    /// P8-02's client exposure: a library's server, Home's server rows
    /// and items no longer available, the row setting and the event. What
    /// is empty is left out, so a reply to an app that does not name the
    /// feature reads as before.
    #[test]
    fn client_exposure_goes_both_ways_and_is_left_out_when_empty() {
        use crate::{Event, HomeScreen, Reply, Request, Response, ServerRow};
        use spoolframe_types::{Library, LibraryKind, LibraryServer};
        let server = LibraryServer {
            server_id: "mzxw6ytbojtg633c".into(),
            name: "Alice's server".into(),
            online: false,
        };
        let shared = Library {
            id: "fed:mzxw6ytbojtg633c:plex:1".parse().unwrap(),
            title: "Movies".into(),
            kind: LibraryKind::Movies,
            server: Some(server.clone()),
        };
        round_trip(Response::Ok(Reply::Libraries {
            libraries: vec![shared],
        }));
        round_trip(Response::Ok(Reply::Home(HomeScreen {
            unavailable: vec!["fed:movie:mzxw6ytbojtg633c:plex:1".parse().unwrap()],
            servers: vec![ServerRow {
                server,
                shown: false,
                items: Vec::new(),
            }],
            ..HomeScreen::default()
        })));
        round_trip(Request::SetServerRow {
            server_id: "mzxw6ytbojtg633c".into(),
            shown: true,
        });
        round_trip(Event::LibrariesChanged);
        // P8-03: progress by item, and history's items no longer available.
        let alien: spoolframe_types::MediaId = "fed:movie:mzxw6ytbojtg633c:plex:1".parse().unwrap();
        round_trip(Request::UpdateWatchProgress {
            id: alien.clone(),
            position_ms: 2_400_000,
        });
        round_trip(Response::Ok(Reply::Items {
            items: Vec::new(),
            total: 1,
            unavailable: vec![alien],
        }));
        let items = serde_json::to_value(Reply::Items {
            items: Vec::new(),
            total: 0,
            unavailable: Vec::new(),
        })
        .unwrap();
        assert!(items.get("unavailable").is_none(), "{items}");
        let own = Library {
            id: "plex:1".parse().unwrap(),
            title: "Movies".into(),
            kind: LibraryKind::Movies,
            server: None,
        };
        let json = serde_json::to_value(&own).unwrap();
        assert!(json.get("server").is_none(), "{json}");
        let home = serde_json::to_value(HomeScreen::default()).unwrap();
        assert!(home.get("servers").is_none() && home.get("unavailable").is_none());
        assert_eq!(serde_json::from_value::<Library>(json).unwrap(), own);
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

    /// P8-03's assertion and its answers, through CBOR and back; a scope
    /// left out is a play, a newer one reads as unknown, and the token
    /// stays out of debug output.
    #[test]
    fn an_assertion_goes_both_ways() {
        let assertion = AssertUser {
            user: "0192f1c4-5b6e-7a3d-9c41-2e8f7b6a5d4c".into(),
            display_name: "Bob".into(),
            client_endpoint: "ab".repeat(32),
            item: "plex:movie:1".into(),
            scope: TokenScope::Play,
            preferences: None,
            download: None,
        };
        round_trip(PeerRequest::AssertUser(assertion.clone()));
        // P8-04: the user's track preferences go with it when SB has them.
        let mut preferences = spoolframe_types::TrackPreferences::default();
        preferences.audio.languages = vec!["fra".into()];
        round_trip(PeerRequest::AssertUser(AssertUser {
            preferences: Some(preferences),
            ..assertion.clone()
        }));
        let granted = TokenGranted {
            token: "f00d".repeat(16),
            scope: TokenScope::Play,
            expires_at: 1_791_000_300,
        };
        assert!(!format!("{granted:?}").contains("f00d"));
        round_trip(PeerReply::Token(granted));
        for reason in [
            AssertionRefused::NotShared,
            AssertionRefused::InvalidUser,
            AssertionRefused::InvalidDevice,
            AssertionRefused::InvalidScope,
            AssertionRefused::Unavailable,
        ] {
            round_trip(PeerReply::AssertionRefused { reason });
        }
        let decode = |value: serde_json::Value| codec::encode(&value).unwrap();
        let sent = serde_json::json!({
            "type": "assert_user",
            "user": assertion.user,
            "display_name": "Bob",
            "client_endpoint": assertion.client_endpoint,
            "item": "plex:movie:1",
        });
        assert_eq!(
            codec::decode::<PeerRequest>(&decode(sent)).unwrap(),
            PeerRequest::AssertUser(assertion.clone())
        );
        let later = serde_json::json!({
            "type": "assert_user",
            "user": assertion.user,
            "display_name": "Bob",
            "client_endpoint": assertion.client_endpoint,
            "item": "plex:movie:1",
            "scope": "subtitles",
        });
        let PeerRequest::AssertUser(later) = codec::decode(&decode(later)).unwrap() else {
            panic!("not an assertion");
        };
        assert_eq!(later.scope, TokenScope::Unknown);
        let long = AssertUser {
            display_name: "x".repeat(500),
            ..assertion
        };
        assert_eq!(
            long.within_bounds().display_name.chars().count(),
            crate::MAX_HELLO_TEXT
        );
    }

    /// P8-07: a download token's assertion, its grant, and the device a
    /// download is planned for, through CBOR and back; what an older
    /// server sends reads as before.
    #[test]
    fn a_download_token_and_its_device_go_both_ways() {
        use crate::{Reply, Request, Response};
        round_trip(PeerRequest::AssertUser(AssertUser {
            user: "0192f1c4-5b6e-7a3d-9c41-2e8f7b6a5d4c".into(),
            display_name: "Bob".into(),
            client_endpoint: "ab".repeat(32),
            item: "plex:movie:1".into(),
            scope: TokenScope::Download,
            preferences: None,
            download: Some("ab".repeat(16)),
        }));
        let alien: spoolframe_types::MediaId = "fed:movie:mzxw6ytbojtg633c:plex:1".parse().unwrap();
        round_trip(Request::RemoteGrant {
            id: alien.clone(),
            download: Some("cd".repeat(16)),
        });
        round_trip(Response::Ok(Reply::RemoteGranted(RemoteGranted {
            token: "f00d".repeat(16),
            item: "plex:movie:1".parse().unwrap(),
            endpoint_id: "ab".repeat(32),
            expires_at: 1_791_000_300,
            download: Some("ab".repeat(16)),
        })));
        round_trip(Request::PrepareDownload {
            id: alien.clone(),
            source: crate::downloads::DownloadSource::Original,
            tracks: crate::downloads::DownloadTracks::All,
            device: Some(crate::downloads::DownloadDevice {
                platform: spoolframe_types::Platform::Ios,
                capabilities: None,
            }),
        });
        round_trip(crate::downloads::DownloadState::Failed {
            reason: "gone".into(),
            cause: crate::downloads::DownloadFailure::NoLongerAvailable,
        });
        let older = serde_json::json!({ "type": "remote_grant", "id": alien.to_string() });
        assert_eq!(
            codec::decode::<Request>(&codec::encode(&older).unwrap()).unwrap(),
            Request::RemoteGrant {
                id: alien,
                download: None
            }
        );
    }

    /// M8-03: a name with a newline in it would forge a log line.
    #[test]
    fn a_name_keeps_no_control_character() {
        let forged = PeerHello {
            name: "Bob\n2026-10-23T00:00:00Z ERROR forged\u{1b}[0m".into(),
            ..hello()
        };
        assert_eq!(
            forged.within_bounds().name,
            "Bob2026-10-23T00:00:00Z ERROR forged[0m"
        );
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

    /// P8-07's subtitle asks and their replies, through CBOR and back; an
    /// ask without the user's choices reads as the defaults.
    #[test]
    fn subtitle_asks_go_both_ways() {
        let subtitles = spoolframe_types::SubtitlePreferences {
            sdh: spoolframe_types::SdhPreference::Prefer,
            ..Default::default()
        };
        round_trip(PeerRequest::SearchSubtitles(SubtitleSearchAsk {
            user: "0192f1c4-0000-7000-8000-000000000001".into(),
            subtitles,
            item: "plex:1".into(),
            language: Some("pt-BR".into()),
        }));
        round_trip(PeerRequest::DownloadSubtitle(SubtitleDownloadAsk {
            user: "0192f1c4-0000-7000-8000-000000000001".into(),
            item: "plex:1".into(),
            result: "opensubtitles:101".into(),
        }));
        round_trip(PeerRequest::RemoveSubtitleDownload(SubtitleRemovalAsk {
            user: "0192f1c4-0000-7000-8000-000000000001".into(),
            item: "plex:1".into(),
            download: "d1".into(),
        }));
        round_trip(PeerReply::SubtitleResults {
            search: spoolframe_types::SubtitleSearch {
                languages: vec!["en".into()],
                results: Vec::new(),
                sources: Vec::new(),
            },
        });
        round_trip(PeerReply::Failed {
            error: crate::Error::QuotaExhausted {
                whose: spoolframe_types::QuotaOf::User,
                resets_at: Some(1_893_456_000),
                site: None,
            },
        });

        #[derive(Serialize)]
        struct Bare<'a> {
            user: &'a str,
            item: &'a str,
        }
        let bytes = codec::encode(&Bare {
            user: "u",
            item: "plex:1",
        })
        .unwrap();
        let read: SubtitleSearchAsk = codec::decode(&bytes).unwrap();
        assert_eq!(read.subtitles, Default::default());
        assert_eq!(read.language, None);
    }
}
