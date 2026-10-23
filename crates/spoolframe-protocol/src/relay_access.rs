//! Who a relay of the server's own lets through (P6-04).
//!
//! The relay runs as a process of its own (`spoolframe-server relay`),
//! with no users of its own: it asks the main server, over an Iroh
//! connection on [`RELAY_ACCESS_ALPN`], which tier an endpoint that
//! connects to it is in. The main server answers only the relays its
//! config names. One bidirectional stream per question: an
//! [`AccessQuery`] frame, answered by an [`AccessAnswer`] frame.
//!
//! A device redeeming an invite is no one's yet: it proves it holds the
//! code with an [`invite_token`] (P6-08), which the relay reads from the
//! `Authorization` header of its connection and passes in the question.
//! A device about to play a paired server's item (P8-04) proves it holds
//! that server's play token the same way, with a [`play_relay_token`].

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};

/// The ALPN a relay asks the main server on.
pub const RELAY_ACCESS_ALPN: &[u8] = b"spoolframe/relay-access/1";

/// What an invite holder's token starts with: its scheme and version.
pub const INVITE_TOKEN_PREFIX: &str = "sf-invite.1.";

/// What a play token holder's relay token starts with (P8-04).
pub const PLAY_TOKEN_PREFIX: &str = "sf-play.1.";

/// Which tier `endpoint` is in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessQuery {
    /// The endpoint's key, hex.
    pub endpoint: String,
    /// The invite holder's token it connected to the relay with (P6-08),
    /// or a play token holder's (P8-04), if it is one. Left out without
    /// one; a server from before it ignores it, and answers as without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invite: Option<String>,
}

/// The tier of the endpoint asked about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessAnswer {
    pub tier: Tier,
}

/// Who an endpoint is to the main server, for its relay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// The server itself, a device of its users (one with a live session
    /// or trusted for a user, P4-01), or the holder of an invite that
    /// makes an account here (`create-user`, any role; P6-08).
    Owner,
    /// The holder of an invite for a portable user (P7) or a server (P8),
    /// and later those users and servers themselves; a paired server's
    /// user's device holding a play token, and while the session it opened
    /// lives (P8-04).
    Friend,
    /// Anyone else.
    Stranger,
    /// A tier this build does not know: a stranger.
    #[serde(other)]
    Unknown,
}

/// The token whoever holds invite `code` connects to the server's relay
/// with, as `endpoint` (its key's bytes): `sf-invite.1.` and the base64url
/// of BLAKE3 keyed with the code's hash, over the key. It proves the code
/// without carrying it, and admits no other endpoint: the relay's
/// handshake proved the key it was made for. `None` for a code that
/// cannot be one.
pub fn invite_token(code: &str, endpoint: &[u8; 32]) -> Option<String> {
    let code = crate::invite::normalize_code(code)?;
    let mac = invite_mac(blake3::hash(code.as_bytes()).as_bytes(), endpoint);
    Some(format!(
        "{INVITE_TOKEN_PREFIX}{}",
        URL_SAFE_NO_PAD.encode(mac)
    ))
}

/// What a token for the invite whose code hashes to `code_hash` carries,
/// as `endpoint`: the server keeps only the hash, and checks with this.
pub fn invite_mac(code_hash: &[u8; 32], endpoint: &[u8; 32]) -> [u8; 32] {
    *blake3::keyed_hash(code_hash, endpoint).as_bytes()
}

/// The MAC `token` carries, if it is an invite holder's token at all.
/// The relay passes on only these, and play token holders' ([`is_relay_token`]):
/// anything else is no token.
pub fn parse_invite_token(token: &str) -> Option<[u8; 32]> {
    mac_after(token, INVITE_TOKEN_PREFIX)
}

/// The token whoever holds paired-server play token `token` connects to
/// that server's relay with, as `endpoint` (P8-04): `sf-play.1.` and the
/// base64url of BLAKE3 keyed with the play token's hash, over the key, as
/// an invite holder's is made with the code's. The server keeps only the
/// play token's hash, and checks with it.
pub fn play_relay_token(token: &str, endpoint: &[u8; 32]) -> String {
    let mac = invite_mac(blake3::hash(token.as_bytes()).as_bytes(), endpoint);
    format!("{PLAY_TOKEN_PREFIX}{}", URL_SAFE_NO_PAD.encode(mac))
}

/// The MAC `token` carries, if it is a play token holder's relay token.
pub fn parse_play_token(token: &str) -> Option<[u8; 32]> {
    mac_after(token, PLAY_TOKEN_PREFIX)
}

/// Whether `token` is one a relay passes on: an invite holder's or a play
/// token holder's.
pub fn is_relay_token(token: &str) -> bool {
    parse_invite_token(token).is_some() || parse_play_token(token).is_some()
}

fn mac_after(token: &str, prefix: &str) -> Option<[u8; 32]> {
    let encoded = token.strip_prefix(prefix)?;
    // 32 bytes are 43 characters unpadded: nothing longer is decoded.
    if encoded.len() != 43 {
        return None;
    }
    URL_SAFE_NO_PAD.decode(encoded).ok()?.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec;

    #[test]
    fn a_query_and_its_answer_go_both_ways() {
        for invite in [None, Some("sf-invite.1.x".to_owned())] {
            let query = AccessQuery {
                endpoint: "3f9a".into(),
                invite,
            };
            let back: AccessQuery = codec::decode(&codec::encode(&query).unwrap()).unwrap();
            assert_eq!(back, query);
        }
        for tier in [Tier::Owner, Tier::Friend, Tier::Stranger] {
            let answer = AccessAnswer { tier };
            let back: AccessAnswer = codec::decode(&codec::encode(&answer).unwrap()).unwrap();
            assert_eq!(back, answer);
        }
    }

    #[test]
    fn a_tier_this_build_does_not_know_is_unknown() {
        let answer: AccessAnswer =
            serde_json::from_value(serde_json::json!({ "tier": "neighbour" })).unwrap();
        assert_eq!(answer.tier, Tier::Unknown);
    }

    /// P6-08: a relay from before the token asks without it, and one with
    /// it asks a server from before it in a shape that server reads.
    #[test]
    fn a_query_without_a_token_is_the_one_before_it() {
        #[derive(Debug, Serialize, Deserialize)]
        struct Before {
            endpoint: String,
        }
        let before = codec::encode(&Before {
            endpoint: "3f9a".into(),
        })
        .unwrap();
        let read: AccessQuery = codec::decode(&before).unwrap();
        assert_eq!(read.invite, None);
        let without = AccessQuery {
            endpoint: "3f9a".into(),
            invite: None,
        };
        assert_eq!(codec::encode(&without).unwrap(), before);
        let with = AccessQuery {
            invite: Some("sf-invite.1.x".into()),
            ..without
        };
        let old: Before = codec::decode(&codec::encode(&with).unwrap()).unwrap();
        assert_eq!(old.endpoint, "3f9a");
    }

    #[test]
    fn a_token_is_the_code_s_mac_over_the_endpoint_and_reads_back() {
        let me = [7u8; 32];
        let token = invite_token("qw7k-2mfp-xr4t", &me).unwrap();
        assert!(token.starts_with(INVITE_TOKEN_PREFIX), "{token}");
        assert!(!token.contains("QW7K"), "{token}");
        // However the code is typed.
        assert_eq!(invite_token("QW7K2MFPXR4T", &me), Some(token.clone()));
        let hash = blake3::hash(b"QW7K2MFPXR4T");
        assert_eq!(
            parse_invite_token(&token),
            Some(invite_mac(hash.as_bytes(), &me))
        );
        // Another endpoint, another code: another token.
        assert_ne!(
            invite_token("QW7K2MFPXR4T", &[8u8; 32]),
            Some(token.clone())
        );
        assert_ne!(invite_token("QW7K2MFPXR4A", &me), Some(token));
        assert_eq!(invite_token("not a code", &me), None);
    }

    /// P8-04: a play token holder's relay token is the token hash's MAC
    /// over the endpoint, and is never an invite holder's.
    #[test]
    fn a_play_relay_token_is_its_token_s_mac_over_the_endpoint() {
        let me = [7u8; 32];
        let token = play_relay_token("bearer", &me);
        assert!(token.starts_with(PLAY_TOKEN_PREFIX), "{token}");
        assert!(!token.contains("bearer"), "{token}");
        let hash = blake3::hash(b"bearer");
        assert_eq!(
            parse_play_token(&token),
            Some(invite_mac(hash.as_bytes(), &me))
        );
        assert_eq!(parse_invite_token(&token), None);
        assert!(is_relay_token(&token));
        assert_ne!(play_relay_token("bearer", &[8u8; 32]), token);
        assert!(!is_relay_token("sf-play.1.x"));
    }

    #[test]
    fn only_a_well_formed_token_is_one() {
        let token = invite_token("QW7K2MFPXR4T", &[7u8; 32]).unwrap();
        for not in [
            "".to_owned(),
            "Bearer x".to_owned(),
            token.replace("sf-invite.1.", "sf-invite.2."),
            format!("{token}A"),
            token[..token.len() - 1].to_owned(),
            format!("{INVITE_TOKEN_PREFIX}{}", "!".repeat(43)),
        ] {
            assert_eq!(parse_invite_token(&not), None, "{not:?}");
        }
    }
}
